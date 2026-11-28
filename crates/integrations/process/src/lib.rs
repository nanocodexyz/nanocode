#![cfg(not(target_family = "wasm"))]

pub use nanocode_home::SessionEnvironment;

#[cfg(unix)]
mod bounded;
#[cfg(unix)]
pub use bounded::{Bounded, Input, bounded};
mod foreground;
pub use foreground::{ForegroundOutput, OutputSink, run_bash};
#[cfg(unix)]
mod handover;
#[cfg(unix)]
pub use handover::{TERMINATED, hand_over, leave_reader, read_outputs_if_requested};
#[cfg(unix)]
mod marked;
#[cfg(unix)]
pub use marked::terminate_marked;
#[cfg(unix)]
mod embedded;
mod output;
mod process;
#[cfg(unix)]
pub use embedded::{Contained, Embedded, Running, Sink, embed};
#[cfg(unix)]
mod login;
#[cfg(unix)]
mod warm;
use nanocode_home::shell as selection;

pub use process::ProcessGroupGuard;

pub use process::ambient_sensitive_environment;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use nanocode_executor::JoinHandle;
use nanocode_platform::timeout;
use nanocode_sync::Notify;
use nanocode_sync::{Actor, Atom};

pub use nanocode_process_contracts::{
    DEFAULT_EXEC_YIELD_MS, DEFAULT_POLL_YIELD_MS, DEFAULT_WRITE_YIELD_MS, MAX_EXEC_YIELD_MS,
    MAX_POLL_YIELD_MS, MIN_POLL_YIELD_MS,
};

static COMMAND_ENVIRONMENT: std::sync::LazyLock<Atom<Vec<(OsString, OsString)>>> =
    std::sync::LazyLock::new(|| Atom::new(Vec::new()));

pub fn set_command_environment(environment: Vec<(OsString, OsString)>) {
    COMMAND_ENVIRONMENT.set(environment);
}

static MIN_WAIT_MS: AtomicU64 = AtomicU64::new(0);

pub fn set_min_wait_ms(value: u64) {
    MIN_WAIT_MS.store(value, Ordering::Relaxed);
}

static DEADLINE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn set_time_limit(limit: Duration) {
    let _ = DEADLINE.set(Instant::now() + limit);
}

fn time_left() -> Option<u64> {
    DEADLINE
        .get()
        .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_secs())
}

fn waited(yield_time: Duration, min_wait_ms: Option<u64>) -> Duration {
    yield_time.max(Duration::from_millis(
        min_wait_ms.unwrap_or_else(|| MIN_WAIT_MS.load(Ordering::Relaxed)),
    ))
}

type Prepare = std::sync::LazyLock<(), Box<dyn FnOnce() + Send>>;

static PREPARE: std::sync::OnceLock<Prepare> = std::sync::OnceLock::new();

pub fn prepare_commands(prepare: impl FnOnce() + Send + 'static) {
    drop(PREPARE.set(Prepare::new(Box::new(prepare))));
}

pub(crate) fn command_environment() -> Arc<Vec<(OsString, OsString)>> {
    if let Some(prepare) = PREPARE.get() {
        Prepare::force(prepare);
    }
    COMMAND_ENVIRONMENT.get()
}

pub(crate) fn redact(bytes: &[u8], redactor: &nanocode_process_contracts::Redactor) -> Vec<u8> {
    let mut output = bytes.to_vec();
    for (secret, placeholder) in redactor.forms() {
        let secret = secret.as_bytes();
        let mut redacted = Vec::with_capacity(output.len());
        let mut rest = output.as_slice();
        while !rest.is_empty() {
            if rest.starts_with(secret) {
                redacted.extend_from_slice(placeholder.as_bytes());
                rest = &rest[secret.len()..];
            } else {
                redacted.push(rest[0]);
                rest = &rest[1..];
            }
        }
        output = redacted;
    }
    output
}

const DRAIN_GRACE: Duration = Duration::from_secs(2);
const MAX_SESSIONS: usize = 64;
pub const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const PROTECTED_SESSIONS: usize = 8;

pub use nanocode_process_contracts::{
    ExecCommand, ExecCommandResult, ForegroundCommand, ForegroundExecutor, LastActivity,
    OutputUnit, ProcessExecutors, RunningProcess, ShellExecutor, WriteStdin,
};

#[derive(Clone, Default)]
pub struct NativeProcessExecutors {
    environment: Arc<Vec<(OsString, OsString)>>,
}

impl NativeProcessExecutors {
    #[must_use]
    pub fn with_environment(environment: Vec<(OsString, OsString)>) -> Self {
        Self {
            environment: Arc::new(environment),
        }
    }
}

impl ProcessExecutors for NativeProcessExecutors {
    fn shell(&self, environment: Vec<(OsString, OsString)>) -> Arc<dyn ShellExecutor> {
        let environment = self
            .environment
            .iter()
            .cloned()
            .chain(environment)
            .collect();
        Arc::new(ShellSessions::with_environment(Arc::new(environment)))
    }

    fn foreground(&self) -> Arc<dyn ForegroundExecutor> {
        match self.environment.is_empty() {
            true => Arc::new(NativeForeground),
            false => Arc::new(EnvironmentForeground(Arc::clone(&self.environment))),
        }
    }
}

struct EnvironmentForeground(Arc<Vec<(OsString, OsString)>>);

impl ForegroundExecutor for EnvironmentForeground {
    fn execute<'a>(
        &'a self,
        request: ForegroundCommand<'a>,
    ) -> nanocode_process_contracts::ProcessFuture<'a, Result<ForegroundOutput, String>> {
        foreground::run_bash_with_environment(
            request.workspace,
            request.command,
            request.timeout,
            request.max_stdout,
            request.max_stderr,
            request.session,
            &self.0,
            request.output,
        )
    }
}

struct Sessions {
    shards: [Atom<HashMap<i64, Arc<Session>>>; SHARDS],
    count: AtomicUsize,
    exits: Arc<Notify>,
}

const SHARDS: usize = MAX_SESSIONS;

impl Sessions {
    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Atom::default()),
            count: AtomicUsize::new(0),
            exits: Arc::default(),
        }
    }

    fn shard(&self, id: i64) -> &Atom<HashMap<i64, Arc<Session>>> {
        let index = id.unsigned_abs() % SHARDS as u64;
        &self.shards[usize::try_from(index).unwrap_or_default()]
    }

    fn get(&self, id: i64) -> Option<Arc<Session>> {
        self.shard(id).read(|sessions| sessions.get(&id).cloned())
    }

    fn insert(&self, session: &Arc<Session>) -> Option<Arc<Session>> {
        let pruned = self.prune();
        self.shard(session.id).update(|sessions| {
            sessions.insert(session.id, Arc::clone(session));
        });
        self.count.fetch_add(1, Ordering::SeqCst);
        pruned
    }

    fn remove(&self, id: i64) -> Option<Arc<Session>> {
        let removed = self.shard(id).try_update(|sessions| sessions.remove(&id));
        if removed.is_some() {
            self.count.fetch_sub(1, Ordering::SeqCst);
            self.exits.notify_waiters();
        }
        removed
    }

    fn prune(&self) -> Option<Arc<Session>> {
        while self.count.load(Ordering::SeqCst) >= MAX_SESSIONS {
            let shards: Vec<_> = self.shards.iter().map(Atom::get).collect();
            let sessions: Vec<_> = shards.iter().flat_map(|shard| shard.values()).collect();
            let victim = prune(&sessions)?;
            if let Some(pruned) = self.remove(victim) {
                return Some(pruned);
            }
        }
        None
    }

    fn each(&self, mut visit: impl FnMut(&Arc<Session>)) {
        for shard in &self.shards {
            shard.read(|sessions| sessions.values().for_each(&mut visit));
        }
    }

    fn take(&self) -> Vec<Arc<Session>> {
        let mut taken = Vec::new();
        for shard in &self.shards {
            let sessions = shard.set(HashMap::new());
            self.count.fetch_sub(sessions.len(), Ordering::SeqCst);
            taken.extend(sessions.values().cloned());
        }
        self.exits.notify_waiters();
        taken
    }
}

pub type SecretEnvironment = Arc<dyn Fn() -> Vec<(OsString, OsString)> + Send + Sync>;

const LOWERCASE_PROXY_VARIABLES: [&str; 4] = ["http_proxy", "https_proxy", "no_proxy", "all_proxy"];

#[must_use]
pub fn file_secrets(file: PathBuf) -> SecretEnvironment {
    Arc::new(move || {
        let secrets: BTreeMap<String, String> = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        secrets
            .into_iter()
            .filter(|(name, _)| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
                    && !name.as_bytes()[0].is_ascii_digit()
                    || LOWERCASE_PROXY_VARIABLES.contains(&name.as_str())
            })
            .map(|(name, value)| (name.into(), value.into()))
            .collect()
    })
}

pub struct ShellSessions {
    terminations: AtomicU64,
    uses: AtomicU64,
    sessions: Arc<Sessions>,
    next_session_id: AtomicI64,
    #[cfg(unix)]
    login_shell: std::sync::OnceLock<Option<PathBuf>>,
    environment: Arc<Vec<(OsString, OsString)>>,
    secrets: Option<SecretEnvironment>,
    per_caller: bool,
    origin: Instant,
    busy_at: AtomicU64,
    output_limit: usize,
}

struct CallEnds<'a>(&'a ShellSessions);

impl Drop for CallEnds<'_> {
    fn drop(&mut self) {
        self.0.touch();
    }
}

impl ShellSessions {
    pub fn with_environment(environment: Arc<Vec<(OsString, OsString)>>) -> Self {
        Self {
            terminations: AtomicU64::new(0),
            uses: AtomicU64::new(0),
            sessions: Arc::new(Sessions::new()),
            next_session_id: AtomicI64::new(1),
            #[cfg(unix)]
            login_shell: std::sync::OnceLock::new(),
            environment,
            secrets: None,
            per_caller: false,
            origin: Instant::now(),
            busy_at: AtomicU64::new(0),
            output_limit: DEFAULT_OUTPUT_LIMIT_BYTES,
        }
    }

    #[must_use]
    pub const fn per_caller(mut self) -> Self {
        self.per_caller = true;
        self
    }

    #[must_use]
    pub const fn output_limit(mut self, bytes: usize) -> Self {
        self.output_limit = bytes;
        self
    }

    pub fn with_secrets(
        environment: Arc<Vec<(OsString, OsString)>>,
        secrets: SecretEnvironment,
    ) -> Self {
        Self {
            secrets: Some(secrets),
            ..Self::with_environment(environment)
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.busy_at.store(now, Ordering::Relaxed);
    }

    fn active(&self) -> bool {
        let mut active = false;
        self.sessions
            .each(|session| active |= !session.exited.get());
        active
    }

    pub fn idle_for(&self) -> Duration {
        if self.active() {
            self.touch();
            return Duration::ZERO;
        }
        self.origin
            .elapsed()
            .saturating_sub(Duration::from_millis(self.busy_at.load(Ordering::Relaxed)))
    }

    pub async fn quiet(&self) {
        loop {
            let exited = self.sessions.exits.notified();
            let mut exited = std::pin::pin!(exited);
            exited.as_mut().enable();
            if !self.active() {
                return;
            }
            exited.await;
        }
    }

    pub const fn default_shell_name(&self) -> &'static str {
        selection::default_shell_name()
    }

    pub fn execute<'call>(
        &'call self,
        command: ExecCommand,
        workspace: &'call Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecCommandResult> + Send + 'call>>
    {
        Box::pin(async move {
            let _ends = CallEnds(self);
            let started_at = Instant::now();
            let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
            let workdir = resolve_workdir(workspace, command.workdir.as_deref());
            let Some(shell) = selection::chosen() else {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    format!(
                        "exec_command failed for `{}`: this host has not made psh its shell",
                        command.script
                    ),
                );
            };
            if !command.tty {
                let directory = workdir.join(".");
                let entered =
                    nanocode_platform::spawn_blocking(move || std::fs::metadata(directory))
                        .await
                        .unwrap_or_else(|error| Err(std::io::Error::other(error)));
                if let Err(error) = entered {
                    return create_process_failed(started_at, &error);
                }
            }
            let host_secrets = self
                .secrets
                .as_ref()
                .map_or_else(Vec::new, |secrets| secrets());
            let (mut environment, secrets) =
                process::sanitized_environment(&self.environment, &host_secrets);
            let login = command.login.unwrap_or(true);
            let yield_time = duration_ms(
                command.yield_time_ms,
                DEFAULT_EXEC_YIELD_MS,
                250,
                MAX_EXEC_YIELD_MS,
            );
            #[cfg(unix)]
            if login && let Some(login_shell) = self.login_shell.get_or_init(selection::login_shell)
            {
                let reported = timeout(
                    yield_time,
                    login::environment(login_shell, &workdir, &environment, &host_secrets),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the login shell {} did not report its environment within {yield_time:?}",
                        login_shell.display()
                    ))
                });
                match reported {
                    Ok(reported) => environment = reported,
                    Err(error) => {
                        return ExecCommandResult::failed(
                            started_at.elapsed(),
                            format!("exec_command failed for `{}`: {error}", command.script),
                        );
                    }
                }
            }
            let search = environment
                .iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.clone());
            let requested = command
                .shell
                .as_deref()
                .and_then(|requested| selection::requested(requested, search.as_deref()));
            let stands_in = requested.is_none();
            if stands_in {
                let bash = OsString::from(selection::default_shell_path());
                if !environment.iter().any(|(name, _)| name == "SHELL") {
                    environment.push((OsString::from("SHELL"), bash.clone()));
                }
                environment.push((OsString::from("PSH_NAME"), bash));
            }
            let shell = requested.unwrap_or(shell);
            let terminations = self.terminations.load(Ordering::SeqCst);
            let script = command.script.clone();
            #[cfg(unix)]
            let warm = match command.tty || !stands_in {
                true => None,
                false => match embedded::spawn(
                    &script,
                    &workdir,
                    &environment,
                    login,
                    embedded::Streams::InOrder,
                ) {
                    Some(spawned) => Some(spawned),
                    None => warm::spawn(&script, &workdir, &shell, login, &environment).await,
                },
            };
            #[cfg(not(unix))]
            let warm = None;
            let spawned = match warm {
                Some(spawned) => Ok(spawned),
                None => {
                    let (directory, tty) = (workdir.clone(), command.tty);
                    nanocode_platform::spawn_blocking(move || {
                        process::spawn(&script, &directory, &shell, login, tty, &environment)
                    })
                    .await
                    .unwrap_or_else(|error| Err(std::io::Error::other(error)))
                }
            };
            let spawned = match spawned {
                Ok(spawned) => spawned,
                Err(error) => return create_process_failed(started_at, &error),
            };
            let launch = Launch {
                command: command.script.clone(),
                workdir,
                started_at: SystemTime::now(),
                caller: command.caller,
            };
            let session = match Session::new(
                session_id,
                spawned,
                secrets,
                launch,
                &self.sessions.exits,
                self.output_limit,
                command.output_token_budget,
            ) {
                Ok(session) => session,
                Err(error) => {
                    return ExecCommandResult::failed(
                        started_at.elapsed(),
                        format!("exec_command failed for `{}`: {error}", command.script),
                    );
                }
            };
            let mut cancellation = CancelExecution {
                session: Some(Arc::clone(&session)),
                sessions: Arc::clone(&self.sessions),
            };
            let (max_output_tokens, output_unit) = (command.max_output_tokens, command.output_unit);
            let min_wait_ms = command.min_wait_ms;
            session
                .last_used
                .store(self.uses.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            let busy = Busy::new(&session);
            let initial = session.owner.run(move |state| {
                Box::pin(state.wait_for_output(
                    waited(yield_time, min_wait_ms),
                    max_output_tokens,
                    output_unit,
                    started_at,
                ))
            });
            let pruned = self.sessions.insert(&session);
            if let Some(pruned) = pruned {
                nanocode_platform::spawn_task(async move {
                    pruned.terminate().await;
                });
            }
            if self.terminations.load(Ordering::SeqCst) != terminations {
                self.forget(session_id);
                session.process_group.terminate_and_disarm();
            }

            let result = initial
                .await
                .unwrap_or_else(|_| session.stopped(started_at));
            drop(busy);
            cancellation.session = None;
            if result.exit_code.is_some() {
                self.forget(session_id);
            }
            result
        })
    }

    pub fn write_stdin<'call>(
        &'call self,
        request: WriteStdin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecCommandResult> + Send + 'call>>
    {
        Box::pin(async move {
            let _ends = CallEnds(self);
            let started_at = Instant::now();
            let session_id = i64::from(request.session_id);
            let session = self.sessions.get(session_id);
            let Some(session) = session else {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    format!(
                        "write_stdin failed: Unknown process id {}",
                        request.session_id
                    ),
                );
            };
            if self.per_caller && session.launch.caller != request.caller {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    format!(
                        "write_stdin failed: process id {} belongs to another session",
                        request.session_id
                    ),
                );
            }

            if !request.chars.is_empty() && !session.tty && request.chars != "\u{3}" {
                return ExecCommandResult::failed(
                    started_at.elapsed(),
                    "write_stdin failed: stdin is closed for this session; rerun exec_command with tty=true to keep stdin open"
                        .to_owned(),
                );
            }
            let (default, minimum, maximum) = if request.chars.is_empty() {
                (DEFAULT_POLL_YIELD_MS, MIN_POLL_YIELD_MS, MAX_POLL_YIELD_MS)
            } else {
                (DEFAULT_WRITE_YIELD_MS, 250, MAX_EXEC_YIELD_MS)
            };
            let yield_time = duration_ms(request.yield_time_ms, default, minimum, maximum);
            let yield_time = if request.chars.is_empty() {
                waited(yield_time, request.min_wait_ms)
            } else {
                yield_time
            };
            let (max_output_tokens, output_unit) = (request.max_output_tokens, request.output_unit);
            let chars = request.chars;
            if !chars.is_empty() {
                session.activity.touch();
            }
            session
                .last_used
                .store(self.uses.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            let busy = Busy::new(&session);
            let result = session
                .owner
                .run(move |state| {
                    Box::pin(async move {
                        if !chars.is_empty() {
                            let written = if state.tty {
                                state.write(&chars).await
                            } else {
                                state.process_group.interrupt().await
                            };
                            if let Err(error) = written {
                                return ExecCommandResult::failed(
                                    started_at.elapsed(),
                                    format!("write_stdin failed: {error}"),
                                );
                            }
                        }
                        state
                            .wait_for_output(yield_time, max_output_tokens, output_unit, started_at)
                            .await
                    })
                })
                .await
                .unwrap_or_else(|_| session.stopped(started_at));
            drop(busy);
            if result.exit_code.is_some() {
                self.forget(session_id);
            }
            result
        })
    }

    pub fn running(&self) -> Vec<RunningProcess> {
        let mut running = Vec::new();
        self.sessions.each(|session| {
            if !session.exited.get() {
                running.push(RunningProcess {
                    session_id: session.id,
                    caller: session.launch.caller.clone(),
                    command: session.launch.command.clone(),
                    workdir: session.launch.workdir.clone(),
                    started_at: session.launch.started_at,
                    in_call: session.busy.load(Ordering::SeqCst) > 0,
                    silent_for: session.activity.silent_for(),
                });
            }
        });
        running.sort_unstable_by_key(|process| process.session_id);
        running
    }

    pub fn terminate_all<'call>(
        &'call self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'call>> {
        Box::pin(async move {
            self.terminations.fetch_add(1, Ordering::SeqCst);
            for session in self.sessions.take() {
                session.terminate().await;
            }
        })
    }

    pub fn terminate<'call>(
        &'call self,
        session_id: i64,
        caller: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'call>> {
        Box::pin(async move {
            let Some(session) = self.sessions.get(session_id) else {
                return;
            };
            if self.per_caller && session.launch.caller != caller {
                return;
            }
            if self.sessions.remove(session_id).is_some() {
                session.terminate().await;
            }
        })
    }

    fn forget(&self, session_id: i64) {
        self.sessions.remove(session_id);
    }
}

struct Launch {
    command: String,
    workdir: PathBuf,
    started_at: SystemTime,
    caller: Option<String>,
}

struct Session {
    id: i64,
    tty: bool,
    process_group: process::SharedProcessGroup,
    owner: Actor<SessionState>,
    launch: Launch,
    last_used: AtomicU64,
    busy: AtomicUsize,
    exited: Arc<process::Exit>,
    activity: Arc<LastActivity>,
}

struct Busy<'a>(&'a Session);

impl<'a> Busy<'a> {
    fn new(session: &'a Session) -> Self {
        session.busy.fetch_add(1, Ordering::SeqCst);
        Self(session)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

fn prune(sessions: &[&Arc<Session>]) -> Option<i64> {
    if sessions.len() < MAX_SESSIONS
        || sessions
            .iter()
            .all(|session| session.busy.load(Ordering::SeqCst) > 0)
    {
        return None;
    }
    let by_id: HashMap<i64, &Arc<Session>> = sessions
        .iter()
        .map(|session| (session.id, *session))
        .collect();
    let sessions = &by_id;
    let mut recent = sessions
        .values()
        .map(|session| (session.last_used.load(Ordering::Relaxed), session.id))
        .collect::<Vec<_>>();
    recent.sort_unstable_by(|left, right| right.cmp(left));
    let protected = recent
        .iter()
        .take(PROTECTED_SESSIONS)
        .map(|(_, id)| *id)
        .collect::<HashSet<_>>();
    let candidates = || {
        recent.iter().rev().filter(|(_, id)| {
            !protected.contains(id) && sessions[id].busy.load(Ordering::SeqCst) == 0
        })
    };
    candidates()
        .find(|(_, id)| sessions[id].exited.get())
        .or_else(|| candidates().next())
        .map(|(_, id)| *id)
}

struct SessionState {
    id: i64,
    tty: bool,
    child: process::ProcessChild,
    stdin: Option<process::ProcessStdin>,
    process_group: process::SharedProcessGroup,
    drains: Option<Vec<JoinHandle<()>>>,
    output: output::Capture,
    output_bound: usize,
    secrets: nanocode_process_contracts::Redactor,
}

impl Session {
    fn new(
        id: i64,
        spawned: process::SpawnedProcess,
        secrets: nanocode_process_contracts::Redactor,
        launch: Launch,
        exits: &Arc<Notify>,
        output_limit: usize,
        output_token_budget: Option<usize>,
    ) -> std::io::Result<Arc<Self>> {
        let tty = matches!(spawned.stdin.as_ref(), Some(process::ProcessStdin::Pty(_)));
        let output_bound = match secrets.forms().next() {
            Some(_) => usize::MAX,
            None => output::effective_token_limit(output_token_budget),
        };
        let output = output::Capture::new(output_limit, output_bound)?;
        let chunks = output::Sender::Captured(output.clone());
        let activity = Arc::new(LastActivity::new());
        let exited = spawned.child.exited();
        let drains = match spawned.output {
            process::ProcessOutput::Pipes { stdout, stderr } => vec![
                nanocode_platform::spawn_task(output::drain(
                    stdout,
                    chunks.clone(),
                    Arc::clone(&activity),
                    Arc::clone(&exited),
                )),
                nanocode_platform::spawn_task(output::drain(
                    stderr,
                    chunks,
                    Arc::clone(&activity),
                    Arc::clone(&exited),
                )),
            ],
            process::ProcessOutput::Pty(reader) => vec![output::drain_blocking(
                reader,
                chunks,
                Arc::clone(&activity),
            )],
            #[cfg(unix)]
            process::ProcessOutput::Streams { stdout, stderr } => vec![
                nanocode_platform::spawn_task(output::drain(
                    Some(stdout),
                    chunks.clone(),
                    Arc::clone(&activity),
                    Arc::clone(&exited),
                )),
                nanocode_platform::spawn_task(output::drain(
                    Some(stderr),
                    chunks,
                    Arc::clone(&activity),
                    Arc::clone(&exited),
                )),
            ],
            #[cfg(unix)]
            process::ProcessOutput::Stream(both) => {
                vec![nanocode_platform::spawn_task(output::drain(
                    Some(both),
                    chunks,
                    Arc::clone(&activity),
                    Arc::clone(&exited),
                ))]
            }
        };
        let process_group = spawned.process_group;
        exited.notify(exits);
        let owner = Actor::spawn(SessionState {
            id,
            tty,
            child: spawned.child,
            stdin: spawned.stdin,
            process_group: process_group.clone(),
            drains: Some(drains),
            output,
            output_bound,
            secrets,
        })?;
        Ok(Arc::new(Self {
            id,
            tty,
            process_group,
            owner,
            launch,
            last_used: AtomicU64::new(0),
            busy: AtomicUsize::new(0),
            exited,
            activity,
        }))
    }

    fn stopped(&self, started_at: Instant) -> ExecCommandResult {
        ExecCommandResult::failed(
            started_at.elapsed(),
            format!("shell session {} stopped", self.id),
        )
    }

    async fn terminate(&self) {
        if let Err(error) = self.process_group.terminate().await {
            nanocode_trace::warn!(
                shell.session.id = self.id,
                %error,
                "failed to terminate shell process group"
            );
        }
        let _ = self.owner.run(|state| Box::pin(state.close())).await;
    }
}

impl SessionState {
    async fn close(&mut self) {
        self.stdin.take();
        if let Err(error) = self.child.wait().await {
            nanocode_trace::warn!(
                shell.session.id = self.id,
                %error,
                "failed to reap terminated shell process"
            );
        }
        self.finish_drains().await;
    }

    async fn write(&mut self, chars: &str) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin is closed")
        })?;
        stdin.write(chars.as_bytes()).await
    }

    async fn wait_for_output(
        &mut self,
        yield_time: Duration,
        max_output_tokens: Option<usize>,
        output_unit: OutputUnit,
        started_at: Instant,
    ) -> ExecCommandResult {
        let status = timeout(yield_time, self.child.wait()).await;
        let exit_code = match status {
            Ok(Ok(exit_code)) => {
                self.finish_drains().await;
                Some(exit_code)
            }
            Ok(Err(error)) => {
                self.process_group.terminate_and_disarm();
                let message = format!("failed to wait for shell command: {error}");
                let _ = self.output.push(message.into_bytes()).await;
                Some(1)
            }
            Err(_) => None,
        };
        let (output, original_token_count) =
            match self.take_output(max_output_tokens, output_unit).await {
                Ok(output) => output,
                Err(error) => {
                    return ExecCommandResult::failed(started_at.elapsed(), error.to_string());
                }
            };
        ExecCommandResult {
            error: None,
            chunk_id: Some(generate_chunk_id()),
            wall_time_seconds: started_at.elapsed().as_secs_f64(),
            exit_code,
            session_id: exit_code.is_none().then_some(self.id),
            original_token_count,
            output,
            time_left_seconds: time_left(),
        }
    }

    async fn finish_drains(&mut self) {
        let Some(handles) = self.drains.take() else {
            return;
        };
        let deadline = nanocode_platform::now() + DRAIN_GRACE;
        for mut handle in handles {
            if nanocode_platform::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
            }
        }
    }

    async fn take_output(
        &mut self,
        max_output_tokens: Option<usize>,
        output_unit: OutputUnit,
    ) -> Result<(String, Option<usize>), nanocode_sync::Stopped> {
        let mut captured = self.output.take().await?;
        let limit = output::effective_token_limit(max_output_tokens).min(self.output_bound);
        let original_token_count = captured.total_bytes.saturating_add(3) / 4;
        let output = captured.limit(&self.secrets, limit, output_unit, original_token_count);
        Ok((output, Some(original_token_count)))
    }
}

pub struct CapturedOutput {
    head: Vec<u8>,
    head_len: usize,
    head_recent: VecDeque<u8>,
    head_rest: VecDeque<Piece>,
    keep: usize,
    omitted_bytes: usize,
    settled: VecDeque<Piece>,
    settled_len: usize,
    tail: VecDeque<u8>,
}

enum Piece {
    Ascii { len: usize, newlines: usize },
    Raw(Vec<u8>),
}

impl Piece {
    const fn len(&self) -> usize {
        match self {
            Self::Ascii { len, .. } => *len,
            Self::Raw(bytes) => bytes.len(),
        }
    }

    fn drop_front(&mut self, count: usize) {
        match self {
            Self::Ascii { len, .. } => *len -= count,
            Self::Raw(bytes) => drop(bytes.drain(..count)),
        }
    }
}

fn push_piece(pieces: &mut VecDeque<Piece>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if bytes.is_ascii() {
        let count = bytes.iter().filter(|&&byte| byte == b'\n').count();
        if let Some(Piece::Ascii { len, newlines }) = pieces.back_mut() {
            *len += bytes.len();
            *newlines += count;
        } else {
            pieces.push_back(Piece::Ascii {
                len: bytes.len(),
                newlines: count,
            });
        }
    } else if let Some(Piece::Raw(raw)) = pieces.back_mut() {
        raw.extend_from_slice(bytes);
    } else {
        pieces.push_back(Piece::Raw(bytes.to_vec()));
    }
}

impl Default for CapturedOutput {
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

fn settle(pieces: &mut VecDeque<Piece>, recent: &mut VecDeque<u8>, count: usize) {
    let (front, back) = recent.as_slices();
    let from_front = count.min(front.len());
    push_piece(pieces, &front[..from_front]);
    push_piece(pieces, &back[..count - from_front]);
    drop(recent.drain(..count));
}

impl CapturedOutput {
    pub const fn new(take_limit: usize) -> Self {
        Self {
            head: Vec::new(),
            head_len: 0,
            head_recent: VecDeque::new(),
            head_rest: VecDeque::new(),
            keep: (take_limit / 2).saturating_add(char::MAX_LEN_UTF8),
            omitted_bytes: 0,
            settled: VecDeque::new(),
            settled_len: 0,
            tail: VecDeque::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8], limit: usize) {
        if bytes.is_empty() {
            return;
        }
        if limit == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(bytes.len());
            return;
        }

        let head_budget = limit / 2;
        let tail_budget = limit.saturating_sub(head_budget);
        let head_len = head_budget.saturating_sub(self.head_len).min(bytes.len());
        let (head, tail_bytes) = bytes.split_at(head_len);
        let raw = self.keep.saturating_sub(self.head.len()).min(head.len());
        self.head.extend_from_slice(&head[..raw]);
        self.head_recent.extend(&head[raw..]);
        self.head_len += head_len;
        if tail_bytes.len() >= tail_budget {
            let kept_from = tail_bytes.len().saturating_sub(tail_budget);
            self.omitted_bytes = self
                .omitted_bytes
                .saturating_add(self.settled_len)
                .saturating_add(self.tail.len())
                .saturating_add(kept_from);
            self.settled.clear();
            self.settled_len = 0;
            self.tail.clear();
            self.tail.extend(&tail_bytes[kept_from..]);
        } else {
            let mut excess = self
                .settled_len
                .saturating_add(self.tail.len())
                .saturating_add(tail_bytes.len())
                .saturating_sub(tail_budget);
            self.omitted_bytes = self.omitted_bytes.saturating_add(excess);
            while excess > 0 {
                let Some(piece) = self.settled.front_mut() else {
                    drop(self.tail.drain(..excess));
                    break;
                };
                let evicted = piece.len().min(excess);
                piece.drop_front(evicted);
                if piece.len() == 0 {
                    self.settled.pop_front();
                }
                self.settled_len -= evicted;
                excess -= evicted;
            }
            self.tail.extend(tail_bytes);
        }
        let after = self.settled_len.saturating_add(self.tail.len());
        let head_settle = self
            .head_recent
            .len()
            .saturating_add(after)
            .saturating_sub(self.keep)
            .min(self.head_recent.len());
        settle(&mut self.head_rest, &mut self.head_recent, head_settle);
        let tail_settle = self.tail.len().saturating_sub(self.keep);
        settle(&mut self.settled, &mut self.tail, tail_settle);
        self.settled_len += tail_settle;
    }

    pub fn take(&mut self) -> CapturedChunk {
        let head = std::mem::take(&mut self.head);
        let head_recent = std::mem::take(&mut self.head_recent);
        let head_rest = std::mem::take(&mut self.head_rest);
        let head_len = std::mem::take(&mut self.head_len);
        let settled = std::mem::take(&mut self.settled);
        let settled_len = std::mem::take(&mut self.settled_len);
        let tail = std::mem::take(&mut self.tail);
        let omitted_bytes = std::mem::take(&mut self.omitted_bytes);
        CapturedChunk {
            total_bytes: head_len
                .saturating_add(settled_len)
                .saturating_add(tail.len())
                .saturating_add(omitted_bytes),
            head,
            head_recent,
            head_rest,
            settled,
            tail,
            omitted_bytes,
        }
    }
}

pub struct CapturedChunk {
    head: Vec<u8>,
    head_recent: VecDeque<u8>,
    head_rest: VecDeque<Piece>,
    settled: VecDeque<Piece>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
    total_bytes: usize,
}

impl CapturedChunk {
    pub fn limit(
        &mut self,
        secrets: &nanocode_process_contracts::Redactor,
        limit: usize,
        unit: OutputUnit,
        original_token_count: usize,
    ) -> String {
        self.trim_truncated_secrets(secrets);
        let marker = self.omission_marker();
        let mut redaction = Redaction::new(secrets, limit);
        redaction.push(&self.head);
        self.head_rest
            .iter()
            .for_each(|piece| redaction.piece(piece));
        let (front, back) = self.head_recent.as_slices();
        redaction.push(front);
        redaction.push(back);
        if let Some(marker) = &marker {
            redaction.push(b"\n");
            redaction.push(marker.as_bytes());
            redaction.push(b"\n");
        }
        self.settled.iter().for_each(|piece| redaction.piece(piece));
        let (front, back) = self.tail.as_slices();
        redaction.push(front);
        redaction.push(back);
        let cut = redaction.finish();
        let (output, _) = match marker {
            Some(marker) => cut.limit_omitted_in(unit, &marker, original_token_count),
            None => cut.limit_in(unit),
        };
        output
    }

    pub fn render(&mut self, secrets: &nanocode_process_contracts::Redactor) -> String {
        self.trim_truncated_secrets(secrets);
        let mut raw = String::from_utf8_lossy(&self.with_omission_marker()).into_owned();
        secrets.text(&mut raw);
        raw
    }

    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn trim_truncated_secrets(&mut self, secrets: &nanocode_process_contracts::Redactor) {
        if self.omitted_bytes == 0 || secrets.forms().next().is_none() {
            return;
        }
        let mut head_keep = self.head.len();
        while let Some(cut) = secrets
            .forms()
            .filter_map(|(secret, _)| {
                let head = &self.head[..head_keep];
                (1..=head.len().min(secret.len().saturating_sub(1)))
                    .rev()
                    .find(|&length| head.ends_with(&secret.as_bytes()[..length]))
            })
            .max()
        {
            head_keep -= cut;
        }
        let tail = self.tail.make_contiguous();
        let mut tail_cut = 0;
        while let Some(cut) = secrets
            .forms()
            .filter_map(|(secret, _)| {
                let tail = &tail[tail_cut..];
                (1..=tail.len().min(secret.len().saturating_sub(1)))
                    .rev()
                    .find(|&length| tail.starts_with(&secret.as_bytes()[secret.len() - length..]))
            })
            .max()
        {
            tail_cut += cut;
        }
        let head_cut = self.head.len() - head_keep;
        self.head.truncate(head_keep);
        drop(self.tail.drain(..tail_cut));
        self.omitted_bytes = self
            .omitted_bytes
            .saturating_add(head_cut)
            .saturating_add(tail_cut);
    }

    pub fn omission_marker(&self) -> Option<String> {
        (self.omitted_bytes > 0).then(|| format!("... {} bytes omitted ...", self.omitted_bytes))
    }

    pub fn with_omission_marker(&self) -> Vec<u8> {
        let marker = self.omission_marker();
        let marker_length = marker.as_ref().map_or(0, |marker| marker.len() + 2);
        let mut output = Vec::with_capacity(
            self.head
                .len()
                .saturating_add(self.tail.len())
                .saturating_add(marker_length),
        );
        output.extend_from_slice(&self.head);
        if let Some(marker) = marker {
            output.push(b'\n');
            output.extend_from_slice(marker.as_bytes());
            output.push(b'\n');
        }
        output.extend(self.tail.iter().copied());
        output
    }
}

struct Redaction<'a> {
    buffer: Vec<u8>,
    carry: Vec<u8>,
    cut: nanocode_process_contracts::OutputCut,
    stages: Vec<Replacement<'a>>,
}

struct Replacement<'a> {
    form: &'a str,
    pending: String,
    placeholder: &'a str,
}

impl<'a> Redaction<'a> {
    fn new(secrets: &'a nanocode_process_contracts::Redactor, limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            carry: Vec::new(),
            cut: nanocode_process_contracts::OutputCut::new(limit),
            stages: secrets
                .forms()
                .map(|(form, placeholder)| Replacement {
                    form,
                    pending: String::new(),
                    placeholder,
                })
                .collect(),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for piece in bytes.chunks(output::READ_BUFFER_LENGTH) {
            self.buffer.clear();
            self.buffer.append(&mut self.carry);
            self.buffer.extend_from_slice(piece);
            let mut chunks = self.buffer.utf8_chunks().peekable();
            while let Some(chunk) = chunks.next() {
                replace_forms(&mut self.stages, &mut self.cut, chunk.valid());
                let invalid = chunk.invalid();
                if invalid.is_empty() {
                    continue;
                }
                if chunks.peek().is_none()
                    && std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none())
                {
                    self.carry.extend_from_slice(invalid);
                } else {
                    replace_forms(&mut self.stages, &mut self.cut, "\u{FFFD}");
                }
            }
        }
    }

    fn piece(&mut self, piece: &Piece) {
        match piece {
            Piece::Ascii { len, newlines } => {
                if !self.carry.is_empty() {
                    self.carry.clear();
                    replace_forms(&mut self.stages, &mut self.cut, "\u{FFFD}");
                }
                self.cut.skip_ascii(*len, *newlines);
            }
            Piece::Raw(bytes) => self.push(bytes),
        }
    }

    fn finish(mut self) -> nanocode_process_contracts::OutputCut {
        if !self.carry.is_empty() {
            replace_forms(&mut self.stages, &mut self.cut, "\u{FFFD}");
        }
        let mut stages = &mut self.stages[..];
        while let Some((stage, rest)) = stages.split_first_mut() {
            let pending = std::mem::take(&mut stage.pending);
            replace_forms(&mut *rest, &mut self.cut, &pending);
            stages = rest;
        }
        self.cut
    }
}

fn replace_forms(
    stages: &mut [Replacement<'_>],
    cut: &mut nanocode_process_contracts::OutputCut,
    text: &str,
) {
    let Some((stage, rest)) = stages.split_first_mut() else {
        cut.write(text);
        return;
    };
    stage.pending.push_str(text);
    let mut start = 0;
    while let Some(found) = stage.pending[start..].find(stage.form) {
        replace_forms(rest, cut, &stage.pending[start..start + found]);
        replace_forms(rest, cut, stage.placeholder);
        start += found + stage.form.len();
    }
    let held = stage.pending.floor_char_boundary(
        stage
            .pending
            .len()
            .saturating_sub(stage.form.len() - 1)
            .max(start),
    );
    replace_forms(rest, cut, &stage.pending[start..held]);
    drop(stage.pending.drain(..held));
}

fn create_process_failed(started_at: Instant, error: &std::io::Error) -> ExecCommandResult {
    let rejected = format!("Failed to create unified exec process: {error}");
    ExecCommandResult::failed(
        started_at.elapsed(),
        format!(
            "exec_command failed: CreateProcess {{ message: {:?} }}",
            format!("Rejected({rejected:?})")
        ),
    )
}

fn resolve_workdir(workspace: &Path, requested: Option<&str>) -> PathBuf {
    let requested = requested.filter(|workdir| !workdir.is_empty());
    match requested.map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace.join(path),
        None => workspace.to_owned(),
    }
}

fn duration_ms(requested: Option<u64>, default: u64, minimum: u64, maximum: u64) -> Duration {
    let requested = requested.unwrap_or(default);
    Duration::from_millis(requested.clamp(minimum, maximum))
}

fn generate_chunk_id() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    random().to_le_bytes()[..3]
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 0x0f)]),
            ]
        })
        .collect()
}

fn random() -> u64 {
    const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
    static STATE: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    let state = STATE.get_or_init(|| {
        let mut seed = [0_u8; 8];
        let _ = nanocode_platform::fill_random(&mut seed);
        AtomicU64::new(u64::from_ne_bytes(seed))
    });
    let mut bits = state
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    bits ^ (bits >> 31)
}

struct CancelExecution {
    session: Option<Arc<Session>>,
    sessions: Arc<Sessions>,
}
impl Drop for CancelExecution {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.sessions.remove(session.id);
            nanocode_platform::spawn_task(async move {
                session.terminate().await;
            });
        }
    }
}

pub use nanocode_home::shell::default_shell_name;

#[cfg(unix)]
#[must_use]
pub fn raise_open_file_limit() -> Option<u64> {
    use nanocode_sys::unix::{Limit, open_file_limit, set_open_file_limit};
    let limit = open_file_limit();
    let current = limit.current.unwrap_or(u64::MAX);
    let mut target = limit.maximum.unwrap_or(u64::MAX);
    while target > current {
        let raised = Limit {
            current: Some(target),
            maximum: limit.maximum,
        };
        if set_open_file_limit(raised).is_ok() {
            return Some(target);
        }
        target = target.min(1 << 21) / 2;
    }
    Some(current)
}

#[cfg(not(unix))]
#[must_use]
pub const fn raise_open_file_limit() -> Option<u64> {
    None
}

impl ShellExecutor for ShellSessions {
    fn execute<'a>(
        &'a self,
        command: ExecCommand,
        workspace: &'a Path,
    ) -> nanocode_process_contracts::ProcessFuture<'a, ExecCommandResult> {
        Self::execute(self, command, workspace)
    }
    fn write_stdin(
        &self,
        request: WriteStdin,
    ) -> nanocode_process_contracts::ProcessFuture<'_, ExecCommandResult> {
        Self::write_stdin(self, request)
    }
    fn terminate_all(&self) -> nanocode_process_contracts::ProcessFuture<'_, ()> {
        Self::terminate_all(self)
    }
    fn terminate(
        &self,
        session_id: i64,
        caller: Option<String>,
    ) -> nanocode_process_contracts::ProcessFuture<'_, ()> {
        Self::terminate(self, session_id, caller)
    }
    fn running(&self) -> Vec<RunningProcess> {
        Self::running(self)
    }
}

pub struct NativeForeground;
impl ForegroundExecutor for NativeForeground {
    fn execute<'a>(
        &'a self,
        request: ForegroundCommand<'a>,
    ) -> nanocode_process_contracts::ProcessFuture<'a, Result<ForegroundOutput, String>> {
        run_bash(
            request.workspace,
            request.command,
            request.timeout,
            request.max_stdout,
            request.max_stderr,
            request.session,
            request.output,
        )
    }
}

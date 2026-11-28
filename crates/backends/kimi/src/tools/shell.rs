//! Kimi Code's Bash tool and the background-task tools over it (TaskList, TaskOutput, TaskStop, WaitFor),
//! with the completion notices Kimi Code injects when a background task ends. Defaults follow `kimi -p`:
//! a background task without `timeout` has no time limit (`timeout_ms: 0`).

// Without processes (wasm), only the spawning path is missing; what it alone reaches is unused there.
#![cfg_attr(not(unix), allow(dead_code))]

use super::io::{self, Host, Kind};
use super::{Builtins, Outcome, input, normalize};
use nanocode_platform::Instant;
use nanocode_sync::future::{AbortHandle, abortable};
use nanocode_sync::watch;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const FOREGROUND_DEFAULT_S: u64 = 60;
const FOREGROUND_MAX_S: u64 = 300;
const BACKGROUND_MAX_S: u64 = 86_400;
const OUTPUT_LIMIT_BYTES: usize = 16 * 1024 * 1024;
const PREVIEW_BYTES: usize = 32_768;
const SPILL_CHARS: usize = 50_000;
const KILL_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_STOP_REASON: &str = "Stopped by TaskStop";
const OUTPUT_LIMIT_REASON: &str = "Output limit exceeded: the command produced more than 16 MiB and was terminated. Redirect large output to a file (e.g. `command > out.txt`) and inspect it in slices instead.";
const NEXT_STEP: &str =
    "next_step: Use Read with output_path to page through the saved output, or Grep to search it.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
}

impl Status {
    const fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Killed => "killed",
        }
    }
}

/// What changes while a task runs.
struct Run {
    output: String,
    file: Option<PathBuf>,
    ended: Option<Instant>,
    exit: Option<i32>,
    status: Status,
    stop_reason: Option<String>,
    timed_out: bool,
    stopped: bool,
    detached: bool,
}

/// A process Bash started: Kimi Code's task record (`agentTaskInfo`) plus its output.
struct Task {
    id: String,
    description: String,
    command: String,
    pid: u32,
    /// Stops a task run through a host, which has no process to signal.
    abort: Mutex<Option<AbortHandle>>,
    parent: String,
    timeout_ms: Option<u64>,
    started: Instant,
    run: Mutex<Run>,
}

impl Task {
    fn run(&self) -> MutexGuard<'_, Run> {
        self.run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wall(&self) -> String {
        let end = self.run().ended.unwrap_or_else(Instant::now);
        format!(
            "{:.3} seconds",
            end.duration_since(self.started).as_secs_f64()
        )
    }

    fn running(&self) -> bool {
        self.run().ended.is_none()
    }

    /// Signals the task's process group, or aborts its host command (any signal ends it at once).
    fn signal(&self, signal: i32) {
        match &*self
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some(handle) => handle.abort(),
            None => self::signal(self.pid, signal),
        }
    }

    /// `formatPlainObject` of the task info, in its field order.
    fn fields(&self) -> Vec<(&'static str, String)> {
        let run = self.run();
        let mut fields = vec![
            ("task_id", self.id.clone()),
            ("description", self.description.clone()),
            ("status", run.status.name().to_owned()),
            ("detached", "true".to_owned()),
        ];
        fields.extend(
            run.stop_reason
                .clone()
                .map(|reason| ("stop_reason", reason)),
        );
        fields.extend(self.timeout_ms.map(|ms| ("timeout_ms", ms.to_string())));
        fields.push(("kind", "process".to_owned()));
        fields.push(("command", self.command.clone()));
        fields.push(("pid", self.pid.to_string()));
        fields.extend(run.exit.map(|code| ("exit_code", code.to_string())));
        fields.push(("parent_tool_call_id", self.parent.clone()));
        fields
    }

    fn terminal_reason(&self) -> Option<(&'static str, String)> {
        let run = self.run();
        let reason = match run.status {
            Status::TimedOut => "timed_out",
            Status::Killed if run.stop_reason.is_some() => "stopped",
            Status::Failed if run.stop_reason.is_some() => "failed",
            _ => return None,
        };
        Some(("terminal_reason", reason.to_owned()))
    }

    /// `formatTaskRecord`: the wall time, then the fields.
    fn record(&self, mut fields: Vec<(&'static str, String)>) -> String {
        if fields.is_empty() {
            fields = self.fields();
        }
        format!("Wall time: {}\n{}", self.wall(), plain(&fields))
    }

    /// The `<notification>` Kimi Code queues as a user message when a background task ends.
    fn notification(&self) -> String {
        let run = self.run();
        let status = run.status.name();
        let description = &self.description;
        let line = match (&run.stop_reason, run.status) {
            (_, Status::TimedOut) => format!("{description} timed out."),
            (Some(reason), Status::Killed) => {
                format!("{description} was stopped. Reason: {reason}")
            }
            (Some(reason), _) => format!("{description} {status}. Reason: {reason}"),
            (None, _) => format!("{description} {status}."),
        };
        let end = run.ended.unwrap_or_else(Instant::now);
        let wall = format!(
            "{:.3} seconds",
            end.duration_since(self.started).as_secs_f64()
        );
        let mut lines = vec![
            format!(
                "<notification id=\"task:{id}:{status}\" category=\"task\" type=\"task.{status}\" source_kind=\"background_task\" source_id=\"{id}\">",
                id = self.id
            ),
            format!("Title: Background process {status}"),
            format!(
                "Severity: {}",
                if run.status == Status::Completed {
                    "info"
                } else {
                    "warning"
                }
            ),
            format!("Wall time: {wall}\n{line}"),
        ];
        if let Some(file) = &run.file {
            let path = file.display();
            lines.push(format!(
                "<output-file path=\"{path}\" bytes=\"{}\">\nRead the output file to retrieve the result: {path}\n</output-file>",
                run.output.len()
            ));
        }
        lines.push("</notification>".to_owned());
        lines.join("\n")
    }
}

/// State the process threads share with the tools.
struct Shared {
    tasks: Mutex<Vec<Arc<Task>>>,
    pending: Mutex<Vec<String>>,
    changed: watch::Sender<()>,
    /// Where output logs go: locally, or on the host, where `persist` writes them.
    dir: Option<PathBuf>,
    host: Option<Host>,
}

impl Shared {
    fn tasks(&self) -> Vec<Arc<Task>> {
        self.tasks
            .lock()
            .map(|tasks| tasks.clone())
            .unwrap_or_default()
    }

    fn pending(&self) -> MutexGuard<'_, Vec<String>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Background tasks, running ones first: Kimi Code lists live handlers, then finished history.
    fn background(&self, active_only: bool) -> Vec<Arc<Task>> {
        let tasks = self.tasks().into_iter().filter(|task| task.run().detached);
        let (running, finished): (Vec<_>, Vec<_>) = tasks.partition(|task| task.running());
        match active_only {
            true => running,
            false => running.into_iter().chain(finished).collect(),
        }
    }

    fn find(&self, id: &str) -> Option<Arc<Task>> {
        self.background(false)
            .into_iter()
            .find(|task| task.id == id)
    }

    /// Appends output the way Kimi Code's task output log does: decoded text, file created on first write.
    fn append(&self, task: &Task, text: &str) {
        let mut run = task.run();
        if text.is_empty() || run.ended.is_some() {
            return;
        }
        if let (Some(dir), Some(_)) = (&self.dir, &self.host) {
            run.file = Some(dir.join(&task.id).join("output.log"));
        } else if let Some(dir) = &self.dir {
            let path = dir.join(&task.id).join("output.log");
            let written = std::fs::create_dir_all(dir.join(&task.id)).and_then(|()| {
                use std::io::Write as _;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?
                    .write_all(text.as_bytes())
            });
            if written.is_ok() {
                run.file = Some(path);
            }
        }
        run.output.push_str(text);
        if run.output.len() > OUTPUT_LIMIT_BYTES && run.stop_reason.is_none() {
            run.stop_reason = Some(OUTPUT_LIMIT_REASON.to_owned());
            task.signal(SIGTERM);
        }
    }

    /// Writes a host task's log as it stands; the log reaches the host when the task ends and whenever
    /// TaskOutput reports it. ponytail: the whole log is rewritten each time, as `FileHost` cannot append.
    async fn persist(&self, task: &Task) {
        let Some(host) = &self.host else {
            return;
        };
        let (file, output) = {
            let run = task.run();
            (run.file.clone(), run.output.clone())
        };
        if let Some(file) = file {
            let _ = io::store(Some(host), &file, output.as_bytes()).await;
        }
    }

    /// The process exited and its output drained: settle the status and notify if it ran in the background.
    fn finish(&self, task: &Task, code: i32) {
        let notice = {
            let mut run = task.run();
            run.ended = Some(Instant::now());
            run.exit = Some(code);
            run.status = match () {
                () if run.timed_out => Status::TimedOut,
                () if run.stopped => Status::Killed,
                () if code == 0 => Status::Completed,
                () => Status::Failed,
            };
            run.detached
        };
        if notice {
            let text = task.notification();
            self.pending().push(text);
        }
        let _ = self.changed.send(());
    }

    /// Waits until `done` holds or `deadline` passes.
    async fn until(&self, deadline: Option<Instant>, done: impl Fn() -> bool) {
        loop {
            let mut changed = self.changed.subscribe();
            if done() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return;
            }
            match deadline {
                Some(deadline) => {
                    let _ = nanocode_platform::timeout_at(deadline, changed.changed()).await;
                }
                None => {
                    let _ = changed.changed().await;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    cwd: Option<String>,
    timeout: Option<u64>,
    description: Option<String>,
    run_in_background: Option<bool>,
    disable_timeout: Option<bool>,
}

#[derive(Deserialize)]
struct ListInput {
    active_only: Option<bool>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct TaskInput {
    task_id: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct WaitInput {
    timeout: u64,
    task_id: Option<String>,
}

/// Bash and the background tasks it starts.
pub struct Shell {
    cwd: PathBuf,
    host: Option<Host>,
    shell: &'static str,
    shared: Arc<Shared>,
    /// WaitFor calls and milliseconds waited in the current turn.
    tally: Mutex<(u32, u64)>,
}

impl Shell {
    pub fn new(cwd: &Path, host: Option<Host>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            shell: shell_path(),
            shared: Arc::new(Shared {
                tasks: Mutex::default(),
                pending: Mutex::default(),
                changed: watch::Sender::new(()),
                dir: match &host {
                    Some(_) => Some(tasks_dir(Path::new("/tmp"))),
                    None => cfg!(unix).then(|| tasks_dir(&std::env::temp_dir())),
                },
                host: host.clone(),
            }),
            host,
            tally: Mutex::default(),
        }
    }

    /// Kimi Code's `BashTool.execution`.
    pub async fn bash(&self, _builtins: &Builtins, call: &str, args: &Value) -> Outcome {
        let args: BashInput = match input("Bash", args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let background = args.run_in_background == Some(true);
        let explicit = args
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if background && explicit.is_empty() {
            return Outcome::error("description is required when run_in_background is true.");
        }
        let foreground_ms = args
            .timeout
            .unwrap_or(FOREGROUND_DEFAULT_S)
            .min(FOREGROUND_MAX_S)
            * 1000;
        let timeout_ms = match (background, args.disable_timeout == Some(true), args.timeout) {
            (false, ..) => Some(foreground_ms),
            (true, true, _) => None,
            (true, false, None) => Some(0),
            (true, false, Some(seconds)) => Some(seconds.min(BACKGROUND_MAX_S) * 1000),
        };
        let description = match explicit.is_empty() {
            false => explicit.to_owned(),
            true => format!("Bash: {}", ellipsis(&args.command, 60)),
        };
        let cwd = normalize(&self.cwd.join(args.cwd.as_deref().unwrap_or_default()));
        if !matches!(io::kind(self.host.as_ref(), &cwd).await, Ok(Kind::Dir)) {
            return Outcome::error(format!(
                "Failed to spawn \"{}\": cwd {} does not exist or is not a directory",
                self.shell,
                cwd.display()
            ));
        }
        let make = |pid| Task {
            id: task_id(),
            description,
            command: args.command.clone(),
            pid,
            abort: Mutex::default(),
            parent: call.to_owned(),
            timeout_ms,
            started: Instant::now(),
            run: Mutex::new(Run {
                output: String::new(),
                file: None,
                ended: None,
                exit: None,
                status: Status::Running,
                stop_reason: None,
                timed_out: false,
                stopped: false,
                detached: background,
            }),
        };
        let launched = match &self.host {
            Some(host) => Ok(launch_host(&self.shared, host, &cwd, &args.command, make)),
            None => launch(&self.shared, self.shell, &cwd, &args.command, make),
        };
        let task = match launched {
            Ok(task) => task,
            Err(error) => {
                return Outcome::error(format!("Failed to spawn \"{}\": {error}", self.shell));
            }
        };
        if background {
            if let Some(ms) = timeout_ms.filter(|ms| *ms > 0) {
                match self.host {
                    Some(_) => arm_host(&task, Duration::from_millis(ms)),
                    None => arm(&task, Duration::from_millis(ms)),
                }
            }
            return Outcome::ok(started(&task, "", false));
        }
        // A foreground call that is dropped (the turn was cancelled) must not leave its process behind.
        let guard = Guard(&self.shared, &task);
        let deadline = task.started + Duration::from_millis(foreground_ms);
        self.shared.until(Some(deadline), || !task.running()).await;
        std::mem::forget(guard);
        {
            let mut run = task.run();
            if run.ended.is_none() {
                run.detached = true;
                let output = run.output.clone();
                drop(run);
                return Outcome::ok(started(&task, &output, true));
            }
        }
        self.shared.forget(&task);
        completion(&task, call)
    }

    /// Kimi Code's `TaskListTool`.
    pub fn list(&self, args: &Value) -> Outcome {
        let args: ListInput = match input("TaskList", args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let active_only = args.active_only.unwrap_or(true);
        let mut tasks = self.shared.background(active_only);
        tasks.truncate(args.limit.unwrap_or(20));
        Outcome::ok(task_list(&tasks, active_only))
    }

    /// Kimi Code's `TaskOutputTool`.
    pub async fn output(&self, args: &Value) -> Outcome {
        let args: TaskInput = match input("TaskOutput", args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let Some(task) = self.shared.find(&args.task_id) else {
            return Outcome::error(format!("Task not found: {}", args.task_id));
        };
        self.shared.persist(&task).await;
        let (file, total, running, preview) = {
            let run = task.run();
            let bytes = run.output.as_bytes();
            let tail = &bytes[bytes.len().saturating_sub(PREVIEW_BYTES)..];
            let preview = String::from_utf8_lossy(tail).into_owned();
            (run.file.clone(), bytes.len(), run.ended.is_none(), preview)
        };
        let truncated = total > PREVIEW_BYTES;
        // A live task reports the bytes of its decoded preview, a finished one the bytes it read.
        let preview_bytes = match running {
            true => preview.len(),
            false => total.min(PREVIEW_BYTES),
        };
        let mut fields = vec![(
            "retrieval_status",
            if running { "not_ready" } else { "success" }.to_owned(),
        )];
        fields.extend(task.fields());
        let path = file.as_ref().map(|file| file.display().to_string());
        fields.extend(path.clone().map(|path| ("output_path", path)));
        fields.extend(task.terminal_reason());
        fields.push(("output_size_bytes", total.to_string()));
        fields.push(("output_preview_bytes", preview_bytes.to_string()));
        fields.push(("output_truncated", truncated.to_string()));
        fields.push(("full_output_available", path.is_some().to_string()));
        if path.is_some() {
            fields.push(("full_output_tool", "Read".to_owned()));
            fields.push(("full_output_hint", match truncated {
                true => format!("Only the last {PREVIEW_BYTES} bytes are shown above. Use the Read tool with the output_path to page through the full log (parameters: path, line_offset, n_lines; read about 300 lines per page)."),
                false => "The preview above is the complete output. Use the Read tool with the output_path if you need to re-read the full log later (parameters: path, line_offset, n_lines; read about 300 lines per page).".to_owned(),
            }));
        }
        let mut lines = vec![task.record(fields), String::new()];
        if truncated {
            lines.push(match &path {
                Some(path) => format!("[Truncated. Full output: {path}]"),
                None => "[Truncated. No persisted full log is available for this task.]".to_owned(),
            });
        }
        lines.push("[output]".to_owned());
        lines.push(match preview.is_empty() {
            true => "[no output available]".to_owned(),
            false => preview,
        });
        Outcome::ok(lines.join("\n"))
    }

    /// Kimi Code's `TaskStopTool`.
    pub async fn stop(&self, args: &Value) -> Outcome {
        let args: TaskInput = match input("TaskStop", args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let Some(task) = self.shared.find(&args.task_id) else {
            return Outcome::error(format!("Task not found: {}", args.task_id));
        };
        let reason = match args.reason.as_deref().map(str::trim) {
            Some(reason) if !reason.is_empty() => reason.to_owned(),
            _ => DEFAULT_STOP_REASON.to_owned(),
        };
        if !task.running() {
            let run = task.run();
            let previous = run
                .stop_reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty());
            let line = format!(
                "task_id: {}\nstatus: {}\nreason: {}",
                task.id,
                run.status.name(),
                previous.unwrap_or("Task already in terminal state")
            );
            drop(run);
            return Outcome::ok(format!("Wall time: {}\n{line}", task.wall()));
        }
        self.shared.halt(&task, Some(reason.clone())).await;
        let run = task.run();
        let line = format!(
            "task_id: {}\nstatus: {}\nreason: {}",
            task.id,
            run.status.name(),
            run.stop_reason.as_deref().unwrap_or(&reason)
        );
        drop(run);
        Outcome::ok(format!("Wall time: {}\n{line}", task.wall()))
    }

    /// Kimi Code's `WaitForTool`, main agent. A completion notice queued and not yet delivered is new
    /// input, which ends the wait the way Kimi Code's steer signal does.
    pub async fn wait(&self, args: &Value) -> Outcome {
        let args: WaitInput = match input("WaitFor", args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let calls = {
            let mut tally = self
                .tally
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tally.0 += 1;
            tally.0
        };
        let start = Instant::now();
        let timeout_ms = args.timeout * 1000;
        let at_start = self.shared.background(true);
        let target = match &args.task_id {
            None if at_start.is_empty() => {
                return Outcome::ok(self.warned(
                    format!("{}\n\nNo background tasks are running, so there is nothing to wait for. Finished tasks report back via automatic notification.", plain(&[("wait_status", "no_tasks".to_owned()), ("waited_ms", "0".to_owned()), ("timeout_ms", timeout_ms.to_string())])),
                    calls,
                ));
            }
            None => None,
            Some(id) => match self.shared.find(id) {
                Some(task) => Some(task),
                None => return Outcome::error(self.warned(format!("Task not found: {id}"), calls)),
            },
        };
        let finished = || match &target {
            Some(task) => (!task.running()).then(|| task.clone()),
            None => at_start
                .iter()
                .filter(|task| !task.running())
                .min_by_key(|task| task.run().ended)
                .cloned(),
        };
        let steered = || !self.shared.pending().is_empty();
        let deadline = start + Duration::from_millis(timeout_ms);
        self.shared
            .until(Some(deadline), || finished().is_some() || steered())
            .await;
        let waited = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Ok(mut tally) = self.tally.lock() {
            tally.1 += waited;
        }
        let id = args.task_id.clone().map(|id| ("task_id", id));
        let still_running = || {
            let running = self.shared.background(true);
            match running.is_empty() {
                true => String::new(),
                false => format!("\n\n[still_running]\n{}", task_list(&running, true)),
            }
        };
        let output = match finished() {
            Some(task) => {
                let mut fields = task.fields();
                fields.extend(task.terminal_reason());
                let mut text = format!(
                    "{}\n\n[finished]\n{}",
                    plain(&[
                        ("wait_status", "completed".to_owned()),
                        ("task_id", task.id.clone()),
                        ("waited_ms", waited.to_string()),
                        ("timeout_ms", timeout_ms.to_string()),
                    ]),
                    task.record(fields)
                );
                let extras = at_start
                    .iter()
                    .filter(|other| other.id != task.id && !other.running())
                    .map(|other| other.record(Vec::new()))
                    .collect::<Vec<_>>();
                if !extras.is_empty() {
                    text.push_str(&format!(
                        "\n\n[completed_during_wait]\n{}\nUse TaskOutput with one of the task_id values above to read the full output.",
                        extras.join("\n---\n")
                    ));
                }
                text + &still_running()
            }
            None => {
                let (status, extra, message) = match steered() {
                    true => (
                        "interrupted",
                        Some(("reason", "steer".to_owned())),
                        "New input ended this wait early. Read the new input before deciding what to do next. Background tasks have not been stopped; completion still arrives via automatic notification.",
                    ),
                    false => (
                        "timed_out",
                        None,
                        "The wait ended before the task finished — a timeout is not an error. Prefer continuing with other work over waiting again; completion arrives via automatic notification.",
                    ),
                };
                let mut fields = vec![("wait_status", status.to_owned())];
                fields.extend(extra);
                fields.extend(id);
                fields.push(("waited_ms", waited.to_string()));
                fields.push(("timeout_ms", timeout_ms.to_string()));
                format!("{}\n{message}", plain(&fields)) + &still_running()
            }
        };
        Outcome::ok(self.warned(output, calls))
    }

    /// `withRepeatWarning`: from the second WaitFor call of a turn on.
    fn warned(&self, output: String, calls: u32) -> String {
        if calls < 2 {
            return output;
        }
        let waited_ms = self.tally.lock().map(|tally| tally.1).unwrap_or_default();
        format!(
            "{output}\n\n[wait_warning]\nThis is WaitFor call {calls} in this turn, and you have already spent {} waiting. Stop calling WaitFor. Repeated waiting wastes the user's time and is almost never the right move. Do not call it again in this turn unless the user explicitly asked you to wait. Do something useful now — another part of the task, or verifying earlier work — or end your turn with a progress update. Finished background tasks notify you automatically, so you will not miss the result.",
            wait_seconds((waited_ms + 500) / 1000)
        )
    }

    /// A new turn: Kimi Code counts repeated WaitFor calls per turn.
    pub fn turn(&self) {
        if let Ok(mut tally) = self.tally.lock() {
            *tally = (0, 0);
        }
    }

    /// Completion notices queued since the last call. Kimi Code delivers them as the input of a new turn.
    pub fn notifications(&self) -> Vec<String> {
        let notices = std::mem::take(&mut *self.shared.pending());
        if !notices.is_empty() {
            self.turn();
        }
        notices
    }

    pub async fn shutdown(&self) {
        for task in self.shared.tasks() {
            if task.running() {
                self.shared.halt(&task, None).await;
            }
        }
        if let Some(dir) = &self.shared.dir {
            let root = dir.parent().unwrap_or(dir);
            match &self.host {
                Some(host) => {
                    let command = format!("rm -rf {}", io::quote(&root.display().to_string()));
                    let _ = io::run(host, Path::new("/"), &command, KILL_GRACE, 4096, None).await;
                }
                None => {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
}

impl Shared {
    /// Stops a running task: SIGTERM to its process group, SIGKILL after the grace period.
    async fn halt(&self, task: &Task, reason: Option<String>) {
        {
            let mut run = task.run();
            run.stopped = true;
            if reason.is_some() {
                run.stop_reason = reason;
            }
        }
        task.signal(SIGTERM);
        self.until(Some(Instant::now() + KILL_GRACE), || !task.running())
            .await;
        if task.running() {
            task.signal(SIGKILL);
            self.until(None, || !task.running()).await;
        }
    }

    fn forget(&self, task: &Arc<Task>) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|other| !Arc::ptr_eq(other, task));
        }
    }
}

struct Guard<'a>(&'a Arc<Shared>, &'a Arc<Task>);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.1.signal(SIGKILL);
        self.0.forget(self.1);
    }
}

/// The result of a foreground command that ran to its end (`completionResult` and the spill Kimi Code
/// gives Bash output beyond 50,000 characters, which points at the task's output log).
fn completion(task: &Task, call: &str) -> Outcome {
    let run = task.run();
    let out = &run.output;
    let join = |message: &str| match out.is_empty() || out.ends_with('\n') {
        true => format!("{out}{message}"),
        false => format!("{out}\n{message}"),
    };
    let (text, failed, suffix) = match (&run.stop_reason, run.exit) {
        (Some(reason), _) if run.status != Status::Completed => (join(reason), true, None),
        (_, Some(0)) => match out.is_empty() {
            true => ("Command executed successfully.".to_owned(), false, None),
            false => (out.clone(), false, Some("Command executed successfully.")),
        },
        (_, code) => {
            let code = code.unwrap_or(-1);
            let body = match out.is_empty() {
                true => format!("Process exited with code {code}\n"),
                false => out.clone(),
            };
            let message = format!("Command failed with exit code: {code}.");
            let text = match body.ends_with('\n') {
                true => format!("{body}{message}"),
                false => format!("{body}\n{message}"),
            };
            (text, true, None)
        }
    };
    let total = units(out);
    let output = match (&run.file, total > SPILL_CHARS && units(&text) > SPILL_CHARS) {
        (Some(file), true) => {
            let info = format!(
                "task_id: {id}\noutput_size_bytes: {}\nnext_step: Use TaskOutput(task_id=\"{id}\") to query the task output.",
                out.len(),
                id = task.id
            );
            let suffix = suffix.map_or(info.clone(), |suffix| format!("{suffix}\n{info}"));
            spill(&text, call, &file.display().to_string(), total, &suffix)
        }
        _ => text,
    };
    Outcome {
        output,
        failed,
        ..Outcome::default()
    }
}

/// `ToolResultTruncationService.truncateForModel` with the output already persisted at `path`.
fn spill(text: &str, call: &str, path: &str, total: usize, suffix: &str) -> String {
    let shaped = shape_lines(text);
    if units(&shaped) <= SPILL_CHARS {
        let lines = suffix.split('\n').collect::<Vec<_>>();
        let inline = match lines
            .iter()
            .any(|line| !line.is_empty() && shaped.contains(line))
        {
            false => suffix.to_owned(),
            true => lines
                .into_iter()
                .filter(|line| !line.is_empty() && !shaped.contains(line))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let mut note = format!(
            "[Per-line truncation occurred; the complete output was saved to a file.\noutput_path: {path}\n{NEXT_STEP}]"
        );
        if !inline.is_empty() {
            note.push_str(&format!("\n\n{inline}"));
        }
        return match shaped.is_empty() || shaped.ends_with('\n') {
            true => format!("{shaped}{note}"),
            false => format!("{shaped}\n{note}"),
        };
    }
    let length = units(text);
    let mut lines = vec![
        format!(
            "Tool output exceeded {SPILL_CHARS} characters; the full output was saved to a file."
        ),
        "tool_name: Bash".to_owned(),
        format!("tool_call_id: {call}"),
        format!("output_size_chars: {total}"),
    ];
    if total == length {
        lines.push(format!("output_size_bytes: {}", text.len()));
    }
    lines.push(format!("output_path: {path}"));
    lines.push(NEXT_STEP.to_owned());
    let head = boundary(text, 4096, false);
    let tail_start = boundary(
        text,
        units(&text[..head]).max(length.saturating_sub(1024)),
        true,
    );
    let head_units = units(&text[..head]);
    let tail_units = units(&text[..tail_start]);
    lines.push(String::new());
    lines.push(format!("[preview: chars [0, {head_units})]"));
    lines.push(text[..head].to_owned());
    if tail_start < text.len() {
        if tail_units > head_units {
            lines.push(String::new());
            lines.push(format!("[elided: chars [{head_units}, {tail_units})]"));
        }
        lines.push(String::new());
        lines.push(format!("[preview: chars [{tail_units}, {length})]"));
        lines.push(text[tail_start..].to_owned());
    }
    if !suffix.is_empty() {
        lines.push(String::new());
        lines.push(suffix.to_owned());
    }
    lines.join("\n")
}

/// `shapeStringPerLine` with Kimi Code's 2,000-character line cap.
fn shape_lines(text: &str) -> String {
    const MARKER: &str = "[...truncated]";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let end = match rest.find(['\r', '\n']) {
            Some(at) if rest[at..].starts_with("\r\n") => at + 2,
            Some(at) => at + 1,
            None => rest.len(),
        };
        let line = &rest[..end];
        rest = &rest[end..];
        if units(line) <= 2000 {
            out.push_str(line);
            continue;
        }
        let body = line.trim_end_matches(['\r', '\n']);
        let suffix = format!("{MARKER}{}", &line[body.len()..]);
        let keep = 2000usize.max(units(&suffix)) - units(&suffix);
        out.push_str(&line[..boundary(line, keep, false)]);
        out.push_str(&suffix);
    }
    out
}

/// JavaScript string length: UTF-16 code units.
fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The byte index of the code point boundary at `at` UTF-16 units, moving forward or back out of a pair.
fn boundary(text: &str, at: usize, forward: bool) -> usize {
    let mut seen = 0;
    for (index, character) in text.char_indices() {
        if seen >= at {
            return index;
        }
        seen += character.len_utf16();
        if seen > at {
            return if forward {
                index + character.len_utf8()
            } else {
                index
            };
        }
    }
    text.len()
}

/// The `backgroundStartedResult` Bash returns when a task starts in, or moves to, the background.
fn started(task: &Task, output: &str, timed_out: bool) -> String {
    let next = match timed_out {
        true => {
            "next_step: The task now runs in the background. You will be automatically notified when it completes — do NOT wait, poll, or call TaskOutput on it; continue with your current work.\n"
        }
        false => {
            "next_step: The completion arrives automatically in a later turn — do NOT wait, poll, or call TaskOutput on it; continue with your current work.\nnext_step: Use TaskStop only if the task must be cancelled.\n"
        }
    };
    let metadata = format!(
        "task_id: {}\npid: {}\ndescription: {}\nstatus: running\nautomatic_notification: true\n{next}human_shell_hint: The task is visible in the background-task panel.",
        task.id, task.pid, task.description
    );
    match output.is_empty() {
        true => metadata,
        false => format!("{metadata}\n\nforeground_output:\n{output}"),
    }
}

/// `formatTaskList`.
fn task_list(tasks: &[Arc<Task>], active_only: bool) -> String {
    let header = format!(
        "{}: {}",
        if active_only {
            "active_background_tasks"
        } else {
            "background_tasks"
        },
        tasks.len()
    );
    match tasks.is_empty() {
        true => format!("{header}\nNo background tasks found."),
        false => format!(
            "{header}\n{}",
            tasks
                .iter()
                .map(|task| task.record(Vec::new()))
                .collect::<Vec<_>>()
                .join("\n---\n")
        ),
    }
}

/// `formatPlainObject`.
fn plain(fields: &[(&str, String)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `formatWaitSeconds`.
fn wait_seconds(total: u64) -> String {
    let (minutes, seconds) = (total / 60, total % 60);
    let (hours, rest) = (minutes / 60, minutes % 60);
    match () {
        () if total < 60 => format!("{total}s"),
        () if minutes < 60 && seconds == 0 => format!("{minutes}m"),
        () if minutes < 60 => format!("{minutes}m {seconds:02}s"),
        () if rest == 0 => format!("{hours}h"),
        () => format!("{hours}h {rest:02}m"),
    }
}

/// The first `limit` UTF-16 units of `text`, with `…` when cut.
fn ellipsis(text: &str, limit: usize) -> String {
    match units(text) > limit {
        true => format!("{}…", &text[..boundary(text, limit, false)]),
        false => text.to_owned(),
    }
}

/// Kimi Code's `generateTaskId("Bash")`.
fn task_id() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 8];
    let _ = nanocode_platform::fill_random(&mut bytes);
    let suffix = bytes
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % 36] as char)
        .collect::<String>();
    format!("bash-{suffix}")
}

/// Node's incremental UTF-8 decoding: complete characters out, an incomplete tail kept for the next chunk,
/// each invalid sequence replaced by U+FFFD.
fn decode(pending: &mut Vec<u8>, end: bool) -> String {
    let mut out = String::new();
    let mut rest = pending.as_slice();
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                rest = &[];
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match error.error_len() {
                    Some(length) => {
                        out.push('\u{FFFD}');
                        rest = &after[length..];
                    }
                    None if end => {
                        out.push('\u{FFFD}');
                        rest = &[];
                        break;
                    }
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    *pending = rest.to_vec();
    out
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

#[cfg(unix)]
fn signal(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nanocode_platform::unix::signal_group(pid, signal);
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _signal: i32) {}

/// The host's shell, found the way Kimi Code probes it.
fn shell_path() -> &'static str {
    ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .unwrap_or("/bin/sh")
}

/// A fresh `nanocode-kimi-<tag>/tasks` under `root`.
fn tasks_dir(root: &Path) -> PathBuf {
    let tag = nanocode_platform::uuid_v4().simple().to_string();
    root.join(format!("nanocode-kimi-{}", &tag[..16]))
        .join("tasks")
}

/// Kimi Code's background timeout: SIGTERM at the deadline, SIGKILL after the grace period.
#[cfg(unix)]
fn arm(task: &Arc<Task>, after: Duration) {
    let task = task.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        {
            let mut run = task.run();
            if run.ended.is_some() {
                return;
            }
            run.timed_out = true;
            run.stop_reason = Some("Timed out".to_owned());
        }
        task.signal(SIGTERM);
        std::thread::sleep(KILL_GRACE);
        if task.running() {
            task.signal(SIGKILL);
        }
    });
}

#[cfg(not(unix))]
fn arm(_task: &Arc<Task>, _after: Duration) {}

/// Spawns `shell -c "cd <cwd> && <command>"` in its own process group, stdout and stderr on one pipe,
/// stdin closed, with the variables Kimi Code sets; threads collect the output and the exit.
#[cfg(unix)]
fn launch(
    shared: &Arc<Shared>,
    shell: &str,
    cwd: &Path,
    command: &str,
    task: impl FnOnce(u32) -> Task,
) -> std::io::Result<Arc<Task>> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    let quoted = format!("'{}'", cwd.display().to_string().replace('\'', "'\\''"));
    let (mut reader, writer) = std::io::pipe()?;
    let mut child = std::process::Command::new(shell)
        .arg("-c")
        .arg(format!("cd {quoted} && {command}"))
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env(
            "GIT_TERMINAL_PROMPT",
            std::env::var_os("GIT_TERMINAL_PROMPT").unwrap_or_else(|| "0".into()),
        )
        .env("SHELL", shell)
        .stdin(std::process::Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0)
        .spawn()?;
    let task = Arc::new(task(child.id()));
    if let Ok(mut tasks) = shared.tasks.lock() {
        tasks.push(task.clone());
    }
    let (drained, wait_drained) = std::sync::mpsc::channel();
    let (from, into) = (shared.clone(), task.clone());
    std::thread::spawn(move || {
        let (mut buffer, mut pending) = (vec![0u8; 65_536], Vec::new());
        loop {
            let read = reader.read(&mut buffer).unwrap_or(0);
            pending.extend_from_slice(&buffer[..read]);
            from.append(&into, &decode(&mut pending, read == 0));
            if read == 0 {
                break;
            }
        }
        let _ = drained.send(());
    });
    let (from, into) = (shared.clone(), task.clone());
    std::thread::spawn(move || {
        let code = child
            .wait()
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(-1);
        let _ = wait_drained.recv_timeout(Duration::from_millis(250));
        from.finish(&into, code);
    });
    Ok(task)
}

/// Runs `cd <cwd> && <command>` through the host's shell with the variables Kimi Code sets, streaming
/// its output into the task when the host streams it, and taking it from the result otherwise.
fn launch_host(
    shared: &Arc<Shared>,
    host: &Host,
    cwd: &Path,
    command: &str,
    task: impl FnOnce(u32) -> Task,
) -> Arc<Task> {
    let task = Arc::new(task(0));
    if let Ok(mut tasks) = shared.tasks.lock() {
        tasks.push(task.clone());
    }
    let sink = Arc::new(Sink {
        shared: shared.clone(),
        task: task.clone(),
        pending: Mutex::default(),
        streamed: std::sync::atomic::AtomicBool::new(false),
    });
    let script = format!(
        "export NO_COLOR=1 TERM=dumb GIT_TERMINAL_PROMPT=\"${{GIT_TERMINAL_PROMPT:-0}}\" SHELL=\"${{BASH:-$SHELL}}\"; cd {} && {command}",
        io::quote(&cwd.display().to_string())
    );
    let (host, cwd, output) = (host.clone(), cwd.to_path_buf(), sink.clone());
    let (run, handle) = abortable(async move {
        let limit = Duration::from_secs(BACKGROUND_MAX_S);
        io::run(
            &host,
            &cwd,
            &script,
            limit,
            OUTPUT_LIMIT_BYTES + 1,
            Some(&*output),
        )
        .await
    });
    *task
        .abort
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    spawn(async move {
        let code = match run.await {
            Ok(Ok(out)) => {
                let rest = match sink.streamed.load(std::sync::atomic::Ordering::Relaxed) {
                    true => sink.flush(),
                    false => out.stdout + &out.stderr,
                };
                sink.shared.append(&sink.task, &rest);
                out.exit_code
            }
            Ok(Err(error)) => {
                sink.shared.append(&sink.task, &error.to_string());
                -1
            }
            Err(_) => -1,
        };
        sink.shared.persist(&sink.task).await;
        sink.shared.finish(&sink.task, code);
    });
    task
}

struct Sink {
    shared: Arc<Shared>,
    task: Arc<Task>,
    pending: Mutex<Vec<u8>>,
    streamed: std::sync::atomic::AtomicBool,
}

impl Sink {
    fn flush(&self) -> String {
        decode(
            &mut self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            true,
        )
    }
}

impl nanocode_process_contracts::OutputSink for Sink {
    fn write(&self, bytes: &[u8]) {
        self.streamed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let text = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.extend_from_slice(bytes);
            decode(&mut pending, false)
        };
        self.shared.append(&self.task, &text);
    }
}

/// Kimi Code's background timeout for a host task: at the deadline it is marked timed out and aborted.
fn arm_host(task: &Arc<Task>, after: Duration) {
    let task = task.clone();
    spawn(async move {
        nanocode_platform::sleep(after).await;
        {
            let mut run = task.run();
            if run.ended.is_some() {
                return;
            }
            run.timed_out = true;
            run.stop_reason = Some("Timed out".to_owned());
        }
        task.signal(SIGTERM);
    });
}

#[cfg(not(target_family = "wasm"))]
fn spawn(future: impl std::future::Future<Output = ()> + Send + 'static) {
    drop(nanocode_platform::spawn_task(future));
}

#[cfg(target_family = "wasm")]
fn spawn(future: impl std::future::Future<Output = ()> + 'static) {
    nanocode_platform::spawn(future);
}

#[cfg(not(unix))]
fn launch(
    _shared: &Arc<Shared>,
    _shell: &str,
    _cwd: &Path,
    _command: &str,
    _task: impl FnOnce(u32) -> Task,
) -> std::io::Result<Arc<Task>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "processes are not available on this platform",
    ))
}

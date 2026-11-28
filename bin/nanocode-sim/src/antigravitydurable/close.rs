use super::*;
use nanocode::process::{ExecCommand, ExecCommandResult, ProcessFuture, ShellExecutor, WriteStdin};
use nanocode_backend_antigravity_tools::{Engine, Outcome};

const CONVERSATION: &str = "agent-reopened";
const CLOSING: &str = "closing";
const SIBLING: &str = "sibling";
const LOST: &str = "closing lost";
const REUSED: &str = "closing reused";
const FOLLOWED: &str = "closing followed";
const HELD: &str = "closing held";
const SLICE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
enum Start {
    BeforeClose,
    AfterClose,
}

#[derive(Clone)]
struct Launch {
    owner: Option<String>,
    script: String,
    live: bool,
    lost: bool,
    asked: u64,
    answered: u64,
}

#[derive(Default)]
struct SharedShell {
    launches: Atom<Vec<Launch>>,
    reuse: Atom<Option<usize>>,
    blanket: AtomicU64,
    foreign: Atom<Vec<String>>,
    holding: Signal,
    release: Signal,
}

impl SharedShell {
    fn live(&self, tag: &str) -> Vec<i64> {
        self.launches.read(|launches| {
            (1_i64..)
                .zip(launches)
                .filter(|(_, launch)| launch.live && launch.script.starts_with(tag))
                .map(|(id, _)| id)
                .collect()
        })
    }

    fn id(&self, script: &str) -> Option<i64> {
        self.launches.read(|launches| {
            (1_i64..)
                .zip(launches)
                .find(|(_, launch)| launch.script == script)
                .map(|(id, _)| id)
        })
    }

    fn with<R>(&self, id: i64, change: impl FnOnce(&mut Launch) -> R) -> Option<R> {
        let index = usize::try_from(id).ok()?.checked_sub(1)?;
        let mut change = Some(change);
        self.launches.update(|launches| {
            let change = change.take()?;
            launches.get_mut(index).map(change)
        })
    }

    fn read(&self, id: i64) -> Option<Launch> {
        let index = usize::try_from(id).ok()?.checked_sub(1)?;
        self.launches.read(|launches| launches.get(index).cloned())
    }

    async fn answered_after(&self, id: i64, change: impl FnOnce(&mut Launch)) {
        let Some(before) = self.with(id, |launch| {
            change(launch);
            launch.asked
        }) else {
            return;
        };
        while self
            .read(id)
            .is_some_and(|launch| launch.answered <= before)
        {
            nanocode_platform::sleep(POLL).await;
        }
    }

    fn end_all(&self) {
        self.launches.update(|launches| {
            for launch in launches.iter_mut() {
                launch.live = false;
            }
        });
    }
}

const fn running(session: i64) -> ExecCommandResult {
    ExecCommandResult {
        error: None,
        chunk_id: None,
        wall_time_seconds: 0.0,
        exit_code: None,
        session_id: Some(session),
        original_token_count: None,
        output: String::new(),
        time_left_seconds: None,
    }
}

impl ShellExecutor for SharedShell {
    fn execute<'a>(
        &'a self,
        command: ExecCommand,
        _workspace: &'a std::path::Path,
    ) -> ProcessFuture<'a, ExecCommandResult> {
        Box::pin(async move {
            if command.script == HELD {
                self.holding.fire();
                self.release.wait().await;
            }
            let launch = Launch {
                owner: command.caller,
                script: command.script,
                live: true,
                lost: false,
                asked: 0,
                answered: 0,
            };
            let reuse = self.reuse.update(Option::take);
            let index = self.launches.update(|launches| match reuse {
                Some(index) => {
                    launches[index] = launch.clone();
                    index
                }
                None => {
                    launches.push(launch.clone());
                    launches.len() - 1
                }
            });
            running(i64::try_from(index + 1).expect("a session id fits i64"))
        })
    }

    fn write_stdin(&self, request: WriteStdin) -> ProcessFuture<'_, ExecCommandResult> {
        Box::pin(async move {
            let id = i64::from(request.session_id);
            let Some(launch) = self.with(id, |launch| {
                launch.asked += 1;
                launch.clone()
            }) else {
                return ExecCommandResult::failed(Duration::ZERO, format!("unknown session {id}"));
            };
            let wait = request
                .yield_time_ms
                .map_or(SLICE, Duration::from_millis)
                .min(SLICE);
            nanocode_platform::sleep(wait).await;
            self.with(id, |launch| launch.answered += 1);
            if launch.lost {
                return ExecCommandResult::failed(
                    Duration::ZERO,
                    "the poll's turn ended".to_owned(),
                );
            }
            if launch.owner != request.caller {
                self.foreign.update(|foreign| {
                    foreign.push(format!(
                        "{:?} polled {id}, owned by {:?}",
                        request.caller, launch.owner
                    ));
                });
                return ExecCommandResult::failed(
                    Duration::ZERO,
                    format!("session {id} is another's"),
                );
            }
            if !launch.live {
                return ExecCommandResult::failed(Duration::ZERO, format!("unknown session {id}"));
            }
            running(id)
        })
    }

    fn terminate_all(&self) -> ProcessFuture<'_, ()> {
        Box::pin(async move {
            self.blanket.fetch_add(1, Ordering::AcqRel);
            self.end_all();
        })
    }

    fn terminate(&self, session_id: i64, caller: Option<String>) -> ProcessFuture<'_, ()> {
        Box::pin(async move {
            self.with(session_id, |launch| {
                if launch.owner == caller {
                    launch.live = false;
                }
            });
        })
    }
}

fn engine(shell: &Arc<SharedShell>) -> Engine {
    let shell: Arc<dyn ShellExecutor> = Arc::clone(shell) as Arc<dyn ShellExecutor>;
    Engine::new(
        shell,
        Arc::new(NoFiles),
        std::path::PathBuf::from("/sim/workspace"),
        std::path::PathBuf::from(format!("/sim/brain/{CONVERSATION}")),
        CONVERSATION.to_owned(),
        None,
    )
}

fn command(line: &str) -> Value {
    json!({"CommandLine": line, "toolSummary": "Simulated command", "WaitMsBeforeAsync": 500})
}

async fn background(engine: &Engine, line: &str, step: u64) -> Result<(), String> {
    match engine.call("run_command", &command(line), step).await {
        Outcome::Background(_) => Ok(()),
        other => Err(format!("{line}: ended as {other:?}, not a background task")),
    }
}

async fn run(seed: u64, start: Start, shell: Arc<SharedShell>) -> Result<(), String> {
    let closing = engine(&shell);
    let sibling = engine(&shell);
    if Rng::derive(seed, "antigravity close order").chance(500_000) {
        background(&closing, LOST, 1).await?;
        background(&sibling, "sibling first", 1).await?;
    } else {
        background(&sibling, "sibling first", 1).await?;
        background(&closing, LOST, 1).await?;
    }
    background(&closing, REUSED, 2).await?;
    background(&closing, FOLLOWED, 3).await?;
    let (Some(lost), Some(reused), Some(followed)) =
        (shell.id(LOST), shell.id(REUSED), shell.id(FOLLOWED))
    else {
        return Err(format!("{start:?}: a background command never launched"));
    };
    shell
        .answered_after(lost, |launch| launch.lost = true)
        .await;
    shell
        .answered_after(reused, |launch| launch.live = false)
        .await;
    shell.reuse.set(usize::try_from(reused - 1).ok());
    background(&sibling, "sibling reuser", 2).await?;
    if shell.id("sibling reuser") != Some(reused) {
        return Err(format!(
            "{start:?}: the sibling did not reuse session {reused}"
        ));
    }
    let held = Arc::new(Atom::new(None));
    let returned = Arc::new(Signal::new());
    {
        let closing = closing.clone();
        let held = Arc::clone(&held);
        let returned = Arc::clone(&returned);
        nanocode_platform::spawn(async move {
            let outcome = closing.call("run_command", &command(HELD), 4).await;
            held.set(Some(outcome));
            returned.fire();
        });
    }
    shell.holding.wait().await;
    if let Start::BeforeClose = start {
        shell.release.fire();
        while shell.id(HELD).is_none() {
            nanocode_platform::sleep(POLL).await;
        }
    }
    closing.shutdown().await;
    let asked = shell.read(followed).map(|launch| launch.asked);
    let left = shell.live(CLOSING);
    if !left.is_empty() {
        return Err(format!(
            "{start:?}: sessions {left:?} outlived the close of their agent"
        ));
    }
    shell.release.fire();
    returned.wait().await;
    let left = shell.live(CLOSING);
    if !left.is_empty() {
        return Err(format!(
            "{start:?}: sessions {left:?} that started as their agent closed kept running"
        ));
    }
    match held.get().as_ref() {
        Some(Outcome::Error(_) | Outcome::Output(_)) if matches!(start, Start::BeforeClose) => {}
        Some(Outcome::Error(_)) => {}
        outcome => {
            return Err(format!(
                "{start:?}: the command that raced the close returned {outcome:?}"
            ));
        }
    }
    nanocode_platform::sleep(SLICE * 4).await;
    let polled = shell.read(followed).map(|launch| launch.asked) != asked;
    let blanket = shell.blanket.load(Ordering::Acquire);
    let sibling_live = shell.live(SIBLING).len();
    let foreign = shell.foreign.get();
    sibling.shutdown().await;
    let sibling_left = shell.live(SIBLING);
    shell.end_all();
    if blanket > 0 {
        return Err(format!(
            "{start:?}: closing one agent ended the whole shared shell"
        ));
    }
    if polled {
        return Err(format!(
            "{start:?}: the closed agent kept polling session {followed}"
        ));
    }
    if sibling_live != 2 {
        return Err(format!(
            "{start:?}: the sibling had {sibling_live} live sessions after the other agent closed, not 2"
        ));
    }
    if !foreign.is_empty() {
        return Err(format!("{start:?}: {}", foreign.join("; ")));
    }
    if !sibling_left.is_empty() {
        return Err(format!(
            "{start:?}: the sibling's sessions {sibling_left:?} outlived its own close"
        ));
    }
    Ok(())
}

pub(super) fn simulate(seed: u64) -> Vec<String> {
    let mut wrong = Vec::new();
    for start in [Start::BeforeClose, Start::AfterClose] {
        let clock = Clock::new();
        crate::host::enter(seed, &clock);
        let shell = Arc::new(SharedShell::default());
        let (ended, result) = epoch(
            seed,
            &clock,
            &format!("antigravity close {start:?}"),
            run(seed, start, Arc::clone(&shell)),
        );
        shell.end_all();
        match (ended, result) {
            (_, Some(Ok(()))) => {}
            (_, Some(Err(error))) => wrong.push(error),
            (ended, None) => wrong.push(format!(
                "antigravity close {start:?}: ended {ended:?} without a result"
            )),
        }
    }
    wrong
}

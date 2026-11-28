use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicUsize,
};

use nanocode::process::{
    ExecCommand, ExecCommandResult, FileHost, ProcessFuture, ShellExecutor, WriteStdin,
};
use nanocode_backend_antigravity_tools::{Engine, Outcome, TRUNCATE_LIMIT};

use super::*;

const POLLS: usize = 40;
const YIELD: Duration = Duration::from_millis(250);
const SHELL_SESSION: i64 = 7;
const CONVERSATION: &str = "sim-conversation";
const TASK: &str = "sim-conversation/task-1";
const BRAIN: &str = "/sim/brain";
const PREVIEW_NOTICE: &str = "middle of the output truncated:\n";
const MARKER_OPEN: &str = "\n<truncated ";
const MARKER_CLOSE: &str = " bytes from the middle of the output>\n";
const SPLIT_SLACK: usize = 6;

fn chunks(seed: u64) -> Vec<String> {
    let mut rng = Rng::derive(seed, "antigravity output chunks");
    let alphabet = ["a", "b", "\n", "é", "€", "😀"];
    (0..POLLS)
        .map(|_| {
            let wanted = match rng.chance(300_000) {
                true => rng.range(1, 16),
                false => rng.range(1, 3 * TRUNCATE_LIMIT as u64),
            };
            let mut chunk = String::new();
            while (chunk.len() as u64) < wanted {
                chunk.push_str(alphabet[rng.pick(alphabet.len())]);
            }
            chunk
        })
        .collect()
}

const fn reply(output: String, exit_code: Option<i32>, session_id: Option<i64>) -> ExecCommandResult {
    ExecCommandResult {
        error: None,
        chunk_id: None,
        wall_time_seconds: 0.0,
        exit_code,
        session_id,
        original_token_count: None,
        output,
        time_left_seconds: None,
    }
}

struct Flood {
    chunks: Vec<String>,
    next: AtomicUsize,
}

impl ShellExecutor for Flood {
    fn execute<'a>(
        &'a self,
        _command: ExecCommand,
        _workspace: &'a Path,
    ) -> ProcessFuture<'a, ExecCommandResult> {
        Box::pin(async { reply(String::new(), None, Some(SHELL_SESSION)) })
    }

    fn write_stdin(&self, _request: WriteStdin) -> ProcessFuture<'_, ExecCommandResult> {
        Box::pin(async move {
            nanocode_platform::sleep(YIELD).await;
            let at = self.next.fetch_add(1, Ordering::AcqRel);
            let last = at + 1 >= self.chunks.len();
            reply(
                self.chunks[at].clone(),
                last.then_some(0),
                (!last).then_some(SHELL_SESSION),
            )
        })
    }

    fn terminate_all(&self) -> ProcessFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[derive(Clone)]
struct Call {
    append: bool,
    path: PathBuf,
    contents: Arc<str>,
}

#[derive(Default)]
struct Ledger {
    calls: Atom<Vec<Call>>,
}

impl Ledger {
    fn record(&self, append: bool, path: &Path, contents: &str) {
        let call = Call {
            append,
            path: path.to_path_buf(),
            contents: contents.into(),
        };
        self.calls.update(|calls| calls.push(call.clone()));
    }

    fn content(&self, path: &Path) -> Option<String> {
        self.calls.read(|calls| {
            calls.iter().filter(|call| call.path == path).fold(
                None,
                |held: Option<String>, call| match call.append {
                    true => Some(held.unwrap_or_default() + &call.contents),
                    false => Some(call.contents.to_string()),
                },
            )
        })
    }
}

impl FileHost for Ledger {
    fn read<'a>(&'a self, path: &'a Path) -> ProcessFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move { Ok(self.content(path)) })
    }

    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
    ) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.record(false, path, contents);
            Ok(())
        })
    }

    fn append<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
    ) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.record(true, path, contents);
            Ok(())
        })
    }

    fn remove<'a>(&'a self, _path: &'a Path) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn read_bytes<'a>(
        &'a self,
        _path: &'a Path,
    ) -> ProcessFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async { Ok(None) })
    }

    fn write_bytes<'a>(
        &'a self,
        _path: &'a Path,
        _bytes: &'a [u8],
    ) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

struct Replies {
    start: String,
    status: String,
    finish: Vec<String>,
}

async fn run(shell: Arc<Flood>, ledger: Arc<Ledger>) -> Result<Replies, String> {
    let engine = Engine::new(
        shell,
        ledger,
        PathBuf::from("/sim/work"),
        PathBuf::from(BRAIN),
        CONVERSATION.to_owned(),
        None,
    );
    let command = json!({"CommandLine": "flood", "toolSummary": "flood", "WaitMsBeforeAsync": 500});
    let Outcome::Background(start) = engine.call("run_command", &command, 1).await else {
        return Err("the command did not become a background task".to_owned());
    };
    engine.wait().await;
    let status = json!({"Action": "status", "TaskId": TASK});
    let Outcome::Output(status) = engine.call("manage_task", &status, 2).await else {
        return Err("manage_task status did not answer".to_owned());
    };
    Ok(Replies {
        start,
        status,
        finish: engine
            .take_messages()
            .into_iter()
            .map(|message| message.content)
            .collect(),
    })
}

fn check_preview(label: &str, shown: &str, full: &str, wrong: &mut Vec<String>) {
    if full.len() <= TRUNCATE_LIMIT {
        if shown != full {
            wrong.push(format!(
                "{label} changed {} bytes of short output",
                full.len()
            ));
        }
        return;
    }
    let cut = shown
        .split_once(PREVIEW_NOTICE)
        .and_then(|(_, body)| body.split_once(MARKER_OPEN))
        .and_then(|(head, rest)| {
            let (numbers, tail) = rest.split_once(MARKER_CLOSE)?;
            let (cut, total) = numbers.split_once(" of ")?;
            Some((
                head,
                tail,
                cut.parse::<usize>().ok()?,
                total.parse::<usize>().ok()?,
            ))
        });
    let Some((head, tail, cut, total)) = cut else {
        wrong.push(format!(
            "{label} shows {} bytes of {} without a truncation preview",
            shown.len(),
            full.len()
        ));
        return;
    };
    let kept = head.len() + tail.len();
    if shown.len() > TRUNCATE_LIMIT {
        wrong.push(format!(
            "{label} is {} bytes, over {TRUNCATE_LIMIT}",
            shown.len()
        ));
    }
    if total != full.len() {
        wrong.push(format!("{label} counts {total} bytes of {}", full.len()));
    }
    if !full.starts_with(head) || !full.ends_with(tail) {
        wrong.push(format!(
            "{label} previews bytes that are not the output's ends"
        ));
    }
    if cut + kept > total || cut + kept + SPLIT_SLACK < total {
        wrong.push(format!(
            "{label} cuts {cut} and keeps {kept} of {total} bytes"
        ));
    }
}

fn check_log(calls: &[Call], chunks: &[String], full: &str, wrong: &mut Vec<String>) {
    let writes = calls.iter().filter(|call| !call.append).count();
    if writes != 1 || calls.first().is_none_or(|call| call.append) {
        wrong.push(format!(
            "the task log was written {writes} times, not once before its appends"
        ));
        return;
    }
    let path = &calls[0].path;
    if !path.starts_with(BRAIN) || calls.iter().any(|call| call.path != *path) {
        wrong.push(format!(
            "the task log moved or left {BRAIN}: {}",
            path.display()
        ));
    }
    let initial = &calls[0].contents;
    let polled = chunks
        .iter()
        .scan(0, |end, chunk| {
            *end += chunk.len();
            Some(*end)
        })
        .position(|end| end == initial.len());
    let Some(polled) = polled else {
        wrong.push("the initial log write is not a whole number of polls".to_owned());
        return;
    };
    let appended = &chunks[polled + 1..];
    let same = calls[1..].len() == appended.len()
        && calls[1..]
            .iter()
            .zip(appended)
            .all(|(call, chunk)| call.append && *call.contents == **chunk);
    if !same {
        wrong.push(format!(
            "{} appends followed {} polls after the initial write, not one per poll",
            calls.len() - 1,
            appended.len()
        ));
    }
    let logged: String = calls
        .iter()
        .fold(String::new(), |held, call| match call.append {
            true => held + &call.contents,
            false => call.contents.to_string(),
        });
    if logged != full || !full.starts_with(&**initial) {
        wrong.push("the task log is not the whole output".to_owned());
    }
}

pub(super) fn simulate(seed: u64, digest: &mut u64) -> (u64, Vec<String>) {
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let chunks = chunks(seed);
    let full: String = chunks.concat();
    let shell = Arc::new(Flood {
        chunks: chunks.clone(),
        next: AtomicUsize::new(0),
    });
    let ledger = Arc::new(Ledger::default());
    let (ended, result) = epoch(
        seed,
        &clock,
        "antigravity background output",
        run(shell, Arc::clone(&ledger)),
    );
    let replies = match (ended, result) {
        (Ended::Finished, Some(Ok(replies))) => replies,
        (Ended::Finished, Some(Err(error))) => return (0, vec![error]),
        (ended, _) => return (0, vec![format!("the output run ended {ended:?}")]),
    };
    let calls = ledger.calls.get();
    let mut wrong = Vec::new();
    check_log(&calls, &chunks, &full, &mut wrong);
    let initial = calls.first().map_or("", |call| &call.contents);
    let started = replies
        .start
        .split_once("Log output:\n")
        .map(|(_, shown)| shown);
    let status = replies
        .status
        .split_once("Log output:\n")
        .and_then(|(_, rest)| rest.rsplit_once("\n\nLast progress: "))
        .map(|(shown, _)| shown);
    let finish = match replies.finish.as_slice() {
        [message] => message
            .split_once("Command output:\n")
            .and_then(|(_, rest)| rest.rsplit_once("\nLog: file://"))
            .map(|(shown, _)| shown),
        _ => None,
    };
    for (label, shown, whole) in [
        ("the start reply", started, initial),
        ("the task status", status, full.as_str()),
        ("the finish message", finish, full.as_str()),
    ] {
        match shown {
            Some(shown) => check_preview(label, shown, whole, &mut wrong),
            None => wrong.push(format!("{label} has no output section")),
        }
    }
    *digest = fnv(
        *digest,
        format!(
            "output {} {} {} {}",
            calls.len(),
            full.len(),
            replies.status.len(),
            replies.finish.iter().map(String::len).sum::<usize>()
        )
        .as_bytes(),
    );
    (u64::try_from(chunks.len()).expect("polls fit u64"), wrong)
}

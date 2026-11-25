use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    num::NonZeroU32,
    os::unix::process::ExitStatusExt as _,
    path::PathBuf,
    process::{Command, ExitCode},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocode_board::{
    Applied, Board, BoardError, ClaimId, Entry, EventId, NewTask, Op, Outcome, Phase, Rejection,
    Task, TaskId,
    access::{Can, Grant, Identity, Verifier, Who},
    schedule::Retry,
    tasks::{OnFail, Planned, TaskOp, Terms},
    watch::{Filter, Kind, Step},
};
use nanocode_durability_core::{
    Migration, Sql as _, SqlRow, SqlStatement, StoreError, migrate_format,
};
use nanocode_executor::signal::unix::{SignalKind, signal};
use nanocode_store_sql::{Host, PgConnection, PgError, PgOptions, PgPool, PgSql, log_wake};
use nanocode_sync::Signal;
use nanocode_turnlog::{
    LogHost,
    parted::{PartedLog, SqlParts},
};
use serde_json::{Value, json};

use crate::dir::DirLog;

const SCOPE: &str = "board";

const SPREAD: usize = 8;

static SUBMITTED: AtomicU64 = AtomicU64::new(0);

const HELP: &str = "nc-board: a nanocode task board kept in a directory or on Postgres
usage: nc-board [--dir DIR | --sql URL] [--as ACTOR] COMMAND
  DIR defaults to $NC_BOARD_DIR, URL (postgres://user[:password]@host[:port]/database) to $NC_BOARD_SQL, ACTOR to
  $NC_BOARD_AS; ACTOR is a principal such as agent:b1, optionally with +profile. A Postgres board is shared by every
  machine that reaches the database: its tables are created on first use, and wait, claim-next --wait and run next
  --wait wake on its LISTEN/NOTIFY, re-reading every --poll-ms as well
  whoami                                   ACTOR, the board, what ACTOR may do on it and the claims ACTOR holds
  init                                     charter the board as ACTOR and let everyone read, post, claim and manage
  post ID [--parent ID] [--label K=V]... [--needs ID]... [--assignee ACTOR] [--on-fail cascade|wait]
          [--attempts N [--backoff SECS]] SPEC
                                           --attempts: a failure before the Nth claim opens the task again after SECS (default
                                           0), doubling each time, instead of failing it; --on-fail wait: a failure stalls the
                                           task until reopen instead of cancelling what needs it
  list [--label K=V]... [--status open|claimed|done|failed|cancelled|blocked|ready]...
  show ID
  claim ID --lease SECS
  claim-next [--label K=V]... [--spread K] --lease SECS [--wait SECS --poll-ms MS]
                                           claim a ready task matching every label: best rank, then ACTOR's own,
                                           then one of the K (default 8) oldest; after a lost race retries the oldest until a claim lands
                                           or none is ready; exits 2 when none is ready, or with --wait when none became ready in SECS
  plan NAME FILE                           post a JSON array of {key, spec, labels?, needs?: [{key}|{task}], parent?, terms?: {on_fail?:
                                           wait, retry?: {max_attempts, backoff_ms}}} as tasks NAME/key
                                           in one write; FILE - reads stdin, and the same NAME and FILE again posts nothing
  run (ID | next [--label K=V]... [--spread K] [--wait SECS --poll-ms MS]) --lease SECS [--result TEXT]
      [--on-exit finish|release] -- COMMAND...
                                           claim, then run COMMAND with NC_BOARD_TASK and NC_BOARD_CLAIM set, renewing every third of
                                           the lease; when COMMAND exits still holding the claim, finish on exit 0 or fail (finish), or
                                           release (release); on SIGTERM, SIGINT or SIGHUP stop COMMAND and release; exits with
                                           COMMAND's code, 1 when the claim is lost, or 2 with --wait when no task became ready in SECS
  renew ID --lease SECS [--claim CLAIM]
  finish ID [--claim CLAIM] DETAIL
  fail ID [--claim CLAIM] DETAIL
  release ID [--claim CLAIM] NOTE
  block ID KIND REASON
  unblock ID
  reopen ID
  cancel ID REASON
  reply ID BODY
  label ID [--label K=V]... [--unset K]...  set and remove labels on a task, such as pr=owner/repo#12 or status=review
  assign ID [--to ACTOR]                   keep a task for one actor; without --to, return it to anyone who may claim it
  schedule ID --in SECS [--every SECS] [--times N] BODY  reply BODY on the task when due, and every SECS after with --every
  events [--after SEQ] [--label K=V]... [--kind KIND]... [--under ID]  the log's changes after SEQ, each with who wrote it
  wait --timeout SECS --poll-ms MS [--after SEQ] [--label K=V]... [--kind KIND]... [--under ID]
                                           fire due schedules and print the changes after SEQ (default: the head when it
                                           starts) once one matches; exits 2 when none comes before the timeout
  grant KEY --who WHO... --can PATTERN=CAN[,CAN]...  WHO is everyone, kind:KIND or a principal; PATTERN a task id, a
                                           subtree such as security/ or *; an empty CAN list closes PATTERN to WHO; the grant's id is ACTOR#KEY
  revoke GRANT                             remove the grant with that id
  grants                                   every grant on the board, with its id
  may ID                                   what ACTOR may do on the task or pattern ID
every command prints JSON: a task, or a list of tasks; CLAIM defaults to ACTOR's current claim on the task";

type Failure = Box<dyn std::error::Error>;

struct Parsed {
    flags: BTreeMap<String, Vec<String>>,
    words: Vec<String>,
}

impl Parsed {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, Failure> {
        let mut flags = BTreeMap::<String, Vec<String>>::new();
        let mut words = Vec::new();
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            if argument == "--" {
                words.extend(arguments.by_ref());
            } else if matches!(argument.as_str(), "-h" | "--help") {
                words.insert(0, "help".to_owned());
            } else if let Some(name) = argument.strip_prefix("--") {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                flags.entry(name.to_owned()).or_default().push(value);
            } else {
                words.push(argument);
            }
        }
        Ok(Self { flags, words })
    }

    fn allow(&self, names: &[&str]) -> Result<(), Failure> {
        match self.flags.keys().find(|name| {
            !names.contains(&name.as_str()) && !matches!(name.as_str(), "dir" | "sql" | "as")
        }) {
            Some(name) => Err(format!("--{name} does not apply here").into()),
            None => Ok(()),
        }
    }

    fn one(&self, name: &str) -> Result<Option<String>, Failure> {
        match self.flags.get(name).map(Vec::as_slice) {
            None => Ok(None),
            Some([value]) => Ok(Some(value.clone())),
            Some(_) => Err(format!("--{name} is given more than once").into()),
        }
    }

    fn many(&self, name: &str) -> Vec<String> {
        self.flags.get(name).cloned().unwrap_or_default()
    }

    fn words<const N: usize>(&self) -> Result<[String; N], Failure> {
        <[String; N]>::try_from(self.words[1..].to_vec()).map_err(|words| {
            format!(
                "{} takes {N} arguments, not {}; see nc-board --help",
                self.words[0],
                words.len()
            )
            .into()
        })
    }

    fn labels(&self) -> Result<BTreeMap<String, String>, Failure> {
        self.many("label")
            .iter()
            .map(|pair| {
                pair.split_once('=')
                    .filter(|(key, _)| !key.is_empty())
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .ok_or_else(|| format!("label {pair} is not KEY=VALUE").into())
            })
            .collect()
    }

    fn lease_ms(&self) -> Result<u64, Failure> {
        self.secs_ms("lease")?
            .ok_or_else(|| "--lease SECS is required".into())
    }

    fn secs_ms(&self, name: &str) -> Result<Option<u64>, Failure> {
        let Some(secs) = self.one(name)? else {
            return Ok(None);
        };
        let secs = secs
            .parse::<u64>()
            .map_err(|error| format!("--{name} {secs}: {error}"))?;
        Ok(Some(secs.checked_mul(1000).ok_or_else(|| {
            format!("--{name} overflows milliseconds")
        })?))
    }

    fn count(&self, name: &str) -> Result<Option<u64>, Failure> {
        self.one(name)?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("--{name} {value}: {error}").into())
            })
            .transpose()
    }

    fn filter(&self) -> Result<Filter, Failure> {
        let kinds = self
            .many("kind")
            .iter()
            .map(|kind| {
                serde_json::from_value::<Kind>(Value::String(kind.clone())).map_err(|_| {
                    format!("--kind {kind} is not a change kind such as replied").into()
                })
            })
            .collect::<Result<BTreeSet<_>, Failure>>()?;
        Ok(Filter {
            under: self.one("under")?.map(TaskId),
            labels: self.labels()?,
            kinds,
        })
    }

    fn setting(&self, flag: &str, variable: &str) -> Result<String, Failure> {
        match self.one(flag)? {
            Some(value) => Ok(value),
            None => std::env::var(variable)
                .map_err(|_| format!("give --{flag} or set {variable}").into()),
        }
    }
}

struct Trusted;

impl Verifier for Trusted {
    fn verify(&self, credential: &str) -> Option<String> {
        Some(credential.to_owned())
    }
}

fn identity(actor: &str) -> Result<Identity, Failure> {
    let (principal, profile) = match actor.split_once('+') {
        Some((principal, profile)) => (principal, Some(profile)),
        None => (actor, None),
    };
    let identity =
        Identity::verify(&Trusted, principal).map_err(|denied| format!("{actor}: {denied:?}"))?;
    match profile {
        Some(profile) => identity
            .profile(profile)
            .map_err(|denied| format!("{actor}: {denied:?}").into()),
        None => Ok(identity),
    }
}

fn now_ms() -> Result<u64, Failure> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

const fn status(task: &Task) -> &'static str {
    match task.phase {
        Phase::Open => "open",
        Phase::Claimed { .. } => "claimed",
        Phase::Done { .. } => "done",
        Phase::Failed { .. } => "failed",
        Phase::Cancelled { .. } => "cancelled",
        Phase::Blocked { .. } => "blocked",
    }
}

fn render(board: &Board, id: &TaskId, task: &Task, at_ms: u64) -> Value {
    let mut out = json!({
        "id": id,
        "parent": task.spec.parent,
        "spec": task.spec.spec,
        "labels": task.spec.labels,
        "needs": task.spec.needs,
        "assignee": task.assignee,
        "status": status(task),
        "ready": board.fold().claimable(id, at_ms).is_ok(),
        "attempts": task.attempts,
        "replies": task.replies.iter().map(|(by, body)| json!({"by": by, "body": body})).collect::<Vec<_>>(),
    });
    let phase = match &task.phase {
        Phase::Open => json!({}),
        Phase::Claimed {
            claim,
            worker,
            until_ms,
        } => {
            json!({"claim": claim, "worker": worker, "until_ms": until_ms, "expired": *until_ms <= at_ms})
        }
        Phase::Done { result, .. } => json!({"result": result}),
        Phase::Failed { error, .. } => json!({"error": error}),
        Phase::Cancelled { reason } => json!({"reason": reason}),
        Phase::Blocked { kind, reason } => json!({"kind": kind, "reason": reason}),
    };
    if let (Value::Object(out), Value::Object(phase)) = (&mut out, phase) {
        out.extend(phase);
    }
    out
}

fn author(event: &str) -> &str {
    event
        .split_once('#')
        .or_else(|| event.rsplit_once('.'))
        .map_or(event, |(by, _)| by)
}

fn changes(board: &Board, after: u64, filter: &Filter) -> Vec<Value> {
    let tasks = board.fold().tasks();
    board
        .entries()
        .filter(|entry| entry.seq > after && filter.matches(tasks, entry))
        .map(|entry: &Entry| {
            json!({
                "seq": entry.seq,
                "event": entry.event,
                "by": author(&entry.event.0),
                "at_ms": entry.at_ms,
                "task": entry.task,
                "change": entry.change,
            })
        })
        .collect()
}

fn shown(board: &Board, id: &TaskId, at_ms: u64) -> Result<Value, Failure> {
    let task = board
        .task(id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    Ok(render(board, id, task, at_ms))
}

fn held(board: &Board, id: &TaskId, given: Option<String>) -> Result<ClaimId, Failure> {
    if let Some(claim) = given {
        return Ok(ClaimId(claim));
    }
    match board.task(id).map(|task| &task.phase) {
        Some(Phase::Claimed { claim, worker, .. }) if *worker == board.writer() => {
            Ok(claim.clone())
        }
        Some(Phase::Claimed { worker, .. }) => Err(format!(
            "{id} is claimed by {worker}, not {}; pass --claim",
            board.writer()
        )
        .into()),
        Some(_) => Err(format!("{id} is not claimed").into()),
        None => Err(format!("no task {id} on this board").into()),
    }
}

async fn claim_next(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<Option<TaskId>, Failure> {
    let lease_ms = parsed.lease_ms()?;
    let labels = parsed.labels()?;
    let spread = match parsed.one("spread")? {
        Some(spread) => spread
            .parse::<usize>()
            .ok()
            .filter(|spread| *spread > 0)
            .ok_or(format!("--spread {spread} is not a positive count"))?,
        None => SPREAD,
    };
    let Some(mut task) = board
        .fold()
        .spread(board.writer(), &labels, at_ms, spread)
        .cloned()
    else {
        return Ok(None);
    };
    loop {
        let claim = Op::Claim {
            task: task.clone(),
            lease_ms,
            fence: None,
            cap: None,
        };
        match ids.submit(board, at_ms, claim).await {
            Err(BoardError::Rejected(
                rejection @ (Rejection::Held { .. } | Rejection::Terminal),
            )) => {
                let Some(head) = board.fold().pick(board.writer(), &labels, at_ms).cloned() else {
                    return Ok(None);
                };
                if head == task {
                    return Err(BoardError::Rejected(rejection).to_string().into());
                }
                task = head;
            }
            landed => {
                landed.map_err(|error| error.to_string())?;
                return Ok(Some(task));
            }
        }
    }
}

async fn submit(board: &mut Board, ids: Ids, at_ms: u64, op: Op) -> Result<(), Failure> {
    ids.submit(board, at_ms, op)
        .await
        .map(drop)
        .map_err(|error| match error {
            BoardError::Unsettled { .. } => {
                format!("{error}; run show to see whether it landed").into()
            }
            error => error.to_string().into(),
        })
}

fn id(word: &str) -> TaskId {
    TaskId(word.to_owned())
}

async fn run(parsed: &Parsed, board: &mut Board, ids: Ids) -> Result<Option<Value>, Failure> {
    let at_ms = now_ms()?;
    let command = parsed.words[0].as_str();
    let written = match command {
        "init" => return chartered(parsed, board, at_ms).await.map(Some),
        "list" => return listed(parsed, board, at_ms).map(Some),
        "show" => {
            parsed.allow(&[])?;
            let [task] = parsed.words()?;
            return shown(board, &id(&task), at_ms).map(Some);
        }
        "claim-next" => {
            parsed.allow(&["label", "lease", "spread", "wait", "poll-ms"])?;
            parsed.words::<0>()?;
            let Some(task) = claim_next(parsed, board, ids, at_ms).await? else {
                return Ok(None);
            };
            task
        }
        "plan" => return planned(parsed, board, at_ms).await.map(Some),
        "post" => posted(parsed, board, ids, at_ms).await?,
        "claim" => claimed(parsed, board, ids, at_ms).await?,
        "renew" => renewed(parsed, board, ids, at_ms).await?,
        "finish" | "fail" => finished(parsed, board, ids, at_ms, command).await?,
        "release" => released(parsed, board, ids, at_ms).await?,
        "block" => blocked(parsed, board, ids, at_ms).await?,
        "unblock" | "reopen" => {
            parsed.allow(&[])?;
            let [task] = parsed.words()?;
            let op = if command == "unblock" {
                TaskOp::Unblock { task: id(&task) }
            } else {
                TaskOp::Reopen { task: id(&task) }
            };
            submit(board, ids, at_ms, Op::Tasks(op)).await?;
            id(&task)
        }
        "cancel" => cancelled(parsed, board, ids, at_ms).await?,
        "reply" => replied(parsed, board, ids, at_ms).await?,
        "label" => labelled(parsed, board, ids, at_ms).await?,
        "assign" => assigned(parsed, board, ids, at_ms).await?,
        "schedule" => return scheduled(parsed, board, ids, at_ms).await.map(Some),
        "grant" => return granted(parsed, board, at_ms).await.map(Some),
        "revoke" => return revoked(parsed, board, at_ms).await.map(Some),
        "grants" => return grants(parsed, board).map(Some),
        "may" => return permitted(parsed, board).map(Some),
        "events" => {
            parsed.allow(&["after", "label", "kind", "under"])?;
            parsed.words::<0>()?;
            let after = parsed.count("after")?.unwrap_or(0);
            return Ok(Some(Value::Array(changes(board, after, &parsed.filter()?))));
        }
        other => return Err(format!("unknown command {other}; see nc-board --help").into()),
    };
    shown(board, &written, now_ms()?).map(Some)
}

async fn chartered(parsed: &Parsed, board: &mut Board, at_ms: u64) -> Result<Value, Failure> {
    parsed.allow(&[])?;
    parsed.words::<0>()?;
    board
        .charter(at_ms)
        .await
        .map_err(|error| error.to_string())?;
    let grant = Grant {
        who: vec![Who::Everyone],
        can: BTreeMap::from([("*".to_owned(), Can::ALL.to_vec())]),
    };
    board
        .grant("everyone", at_ms, grant)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"owner": board.writer(), "granted": "everyone may do everything"}))
}

fn listed(parsed: &Parsed, board: &Board, at_ms: u64) -> Result<Value, Failure> {
    parsed.allow(&["label", "status"])?;
    parsed.words::<0>()?;
    let labels = parsed.labels()?;
    let statuses = parsed.many("status");
    let listed = board
        .tasks()
        .filter(|(_, task)| {
            labels
                .iter()
                .all(|(key, value)| task.spec.labels.get(key) == Some(value))
        })
        .map(|(id, task)| render(board, id, task, at_ms))
        .filter(|task| {
            statuses.is_empty()
                || statuses.iter().any(|wanted| {
                    task["status"] == wanted.as_str()
                        || (wanted == "ready" && task["ready"] == true)
                })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(listed))
}

async fn planned(parsed: &Parsed, board: &mut Board, at_ms: u64) -> Result<Value, Failure> {
    parsed.allow(&[])?;
    let [name, file] = parsed.words()?;
    let mut text = String::new();
    if file == "-" {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text = std::fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
    }
    let mut plan =
        serde_json::from_str::<Value>(&text).map_err(|error| format!("{file}: {error}"))?;
    for planned in plan.as_array_mut().into_iter().flatten() {
        if let Value::Object(planned) = planned {
            planned.entry("labels").or_insert_with(|| json!({}));
            planned.entry("needs").or_insert_with(|| json!([]));
        }
    }
    let tasks =
        serde_json::from_value::<Vec<Planned>>(plan).map_err(|error| format!("{file}: {error}"))?;
    let placed = board
        .batch(at_ms, id(&name), tasks)
        .await
        .map_err(|error| error.to_string())?;
    placed
        .iter()
        .map(|task| shown(board, task, at_ms))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

async fn posted(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&[
        "parent", "label", "needs", "assignee", "on-fail", "attempts", "backoff",
    ])?;
    let [task, spec] = parsed.words()?;
    let on_fail = match parsed.one("on-fail")?.as_deref() {
        None | Some("cascade") => OnFail::Cascade,
        Some("wait") => OnFail::Wait,
        Some(other) => {
            return Err(format!("--on-fail {other} is not cascade or wait").into());
        }
    };
    let retry = match (parsed.count("attempts")?, parsed.secs_ms("backoff")?) {
        (None, None) => None,
        (None, Some(_)) => return Err("--backoff needs --attempts".into()),
        (Some(attempts), backoff_ms) => Some(Retry {
            max_attempts: attempts,
            backoff_ms: backoff_ms.unwrap_or(0),
        }),
    };
    let new = NewTask {
        id: id(&task),
        parent: parsed.one("parent")?.map(|parent| id(&parent)),
        spec,
        labels: parsed.labels()?,
        needs: parsed.many("needs").iter().map(|need| id(need)).collect(),
        terms: Terms {
            assignee: parsed.one("assignee")?,
            on_fail,
            retry,
            ..Terms::default()
        },
    };
    submit(board, ids, at_ms, Op::Post { task: new }).await?;
    Ok(id(&task))
}

async fn claimed(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&["lease"])?;
    let [task] = parsed.words()?;
    let lease_ms = parsed.lease_ms()?;
    let op = Op::Claim {
        task: id(&task),
        lease_ms,
        fence: None,
        cap: None,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn renewed(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&["lease", "claim"])?;
    let [task] = parsed.words()?;
    let lease_ms = parsed.lease_ms()?;
    let claim = held(board, &id(&task), parsed.one("claim")?)?;
    let op = Op::Renew {
        task: id(&task),
        claim,
        lease_ms,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn finished(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
    command: &str,
) -> Result<TaskId, Failure> {
    parsed.allow(&["claim"])?;
    let [task, detail] = parsed.words()?;
    let claim = held(board, &id(&task), parsed.one("claim")?)?;
    let outcome = if command == "finish" {
        Outcome::Done(detail)
    } else {
        Outcome::Failed(detail)
    };
    let op = Op::Finish {
        task: id(&task),
        claim,
        outcome,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn released(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&["claim"])?;
    let [task, note] = parsed.words()?;
    let claim = held(board, &id(&task), parsed.one("claim")?)?;
    let op = Op::Release {
        task: id(&task),
        claim,
        note,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn blocked(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&[])?;
    let [task, kind, reason] = parsed.words()?;
    let claim = match board.task(&id(&task)).map(|task| &task.phase) {
        Some(Phase::Claimed { claim, worker, .. }) if *worker == board.writer() => {
            Some(claim.clone())
        }
        _ => None,
    };
    let op = Op::Tasks(TaskOp::Block {
        task: id(&task),
        claim,
        kind,
        reason,
    });
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn cancelled(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&[])?;
    let [task, reason] = parsed.words()?;
    let op = Op::Cancel {
        task: id(&task),
        reason,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn replied(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&[])?;
    let [task, body] = parsed.words()?;
    let op = Op::Reply {
        task: id(&task),
        body,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn labelled(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&["label", "unset"])?;
    let [task] = parsed.words()?;
    let set = parsed.labels()?;
    let unset = parsed.many("unset").into_iter().collect::<BTreeSet<_>>();
    if set.is_empty() && unset.is_empty() {
        return Err("label needs --label K=V or --unset K".into());
    }
    let op = Op::Relabel {
        task: id(&task),
        set,
        unset,
    };
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn assigned(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<TaskId, Failure> {
    parsed.allow(&["to"])?;
    let [task] = parsed.words()?;
    let op = Op::Tasks(TaskOp::Assign {
        task: id(&task),
        to: parsed.one("to")?,
    });
    submit(board, ids, at_ms, op).await?;
    Ok(id(&task))
}

async fn scheduled(
    parsed: &Parsed,
    board: &mut Board,
    ids: Ids,
    at_ms: u64,
) -> Result<Value, Failure> {
    parsed.allow(&["in", "every", "times"])?;
    let [task, body] = parsed.words()?;
    let wait_ms = parsed.secs_ms("in")?.ok_or("--in SECS is required")?;
    let first_ms = at_ms
        .checked_add(wait_ms)
        .ok_or("--in overflows the clock")?;
    let schedule = format!("{}/{task}/{at_ms}", board.writer());
    let step = Step::Schedule {
        id: schedule.clone(),
        by: board.writer().to_owned(),
        send: Box::new(Op::Reply {
            task: id(&task),
            body,
        }),
        first_ms,
        every_ms: parsed.secs_ms("every")?,
        until_ms: None,
        times: parsed.count("times")?,
    };
    ids.submit(board, at_ms, Op::Watch(step))
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"schedule": schedule, "task": task, "due_ms": first_ms}))
}

fn grantees(parsed: &Parsed) -> Vec<Who> {
    parsed
        .many("who")
        .iter()
        .map(|who| match who.as_str() {
            "everyone" => Who::Everyone,
            other => other.strip_prefix("kind:").map_or_else(
                || Who::Principal(other.to_owned()),
                |kind| Who::Kind(kind.to_owned()),
            ),
        })
        .collect::<Vec<_>>()
}

fn permissions(parsed: &Parsed) -> Result<BTreeMap<String, Vec<Can>>, Failure> {
    parsed
        .many("can")
        .iter()
        .map(|entry| {
            let (pattern, cans) = entry
                .split_once('=')
                .ok_or_else(|| format!("--can {entry} is not PATTERN=CAN[,CAN]..."))?;
            let cans = cans
                .split(',')
                .filter(|can| !can.is_empty())
                .map(|can| {
                    serde_json::from_value::<Can>(Value::String(can.to_owned()))
                        .map_err(|_| format!("--can {entry}: {can} is not a permission"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((pattern.to_owned(), cans))
        })
        .collect::<Result<BTreeMap<_, _>, Failure>>()
}

async fn granted(parsed: &Parsed, board: &mut Board, at_ms: u64) -> Result<Value, Failure> {
    parsed.allow(&["who", "can"])?;
    let [key] = parsed.words()?;
    let who = grantees(parsed);
    if who.is_empty() {
        return Err("grant needs --who WHO".into());
    }
    let can = permissions(parsed)?;
    if can.is_empty() {
        return Err("grant needs --can PATTERN=CAN[,CAN]...".into());
    }
    let grant = Grant { who, can };
    board
        .grant(&key, at_ms, grant.clone())
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"grant": format!("{}#{key}", board.writer()), "who": grant.who, "can": grant.can}))
}

async fn revoked(parsed: &Parsed, board: &mut Board, at_ms: u64) -> Result<Value, Failure> {
    parsed.allow(&[])?;
    let [grant] = parsed.words()?;
    board
        .revoke(&format!("revoke:{grant}"), at_ms, EventId(grant.clone()))
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"revoked": grant}))
}

fn grants(parsed: &Parsed, board: &Board) -> Result<Value, Failure> {
    parsed.allow(&[])?;
    parsed.words::<0>()?;
    let grants = board
        .fold()
        .access()
        .grants()
        .iter()
        .map(|(id, grant)| json!({"grant": id, "who": grant.who, "can": grant.can}))
        .collect();
    Ok(Value::Array(grants))
}

fn permitted(parsed: &Parsed, board: &Board) -> Result<Value, Failure> {
    parsed.allow(&[])?;
    let [channel] = parsed.words()?;
    let can = Can::ALL
        .into_iter()
        .filter(|can| board.may(&channel, *can))
        .collect::<Vec<_>>();
    Ok(json!({"actor": board.writer(), "channel": channel, "can": can}))
}

enum Store {
    Dir(PathBuf),
    Postgres(Postgres),
}

struct Postgres {
    url: String,
    options: PgOptions,
    sql: Arc<PgSql>,
    log: Arc<PartedLog<SqlParts<PgSql>>>,
}

#[derive(Clone, Copy)]
enum Ids {
    Minted,
    Unique,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
}

enum Wake {
    Poll,
    Listen(PgConnection),
}

const PREPARED: &str = "SELECT CAST(to_regclass('nanocode_plog_p0') IS NOT NULL AS TEXT)";

const HOLDS: Migration = Migration {
    statements: [
        &[
            "CREATE TABLE IF NOT EXISTS nanocode_board_holds (key TEXT PRIMARY KEY, holder TEXT NOT NULL, expires_at TEXT NOT NULL)",
        ],
        &[
            "CREATE TABLE IF NOT EXISTS nanocode_board_holds (key TEXT PRIMARY KEY, holder TEXT NOT NULL, expires_at TIMESTAMPTZ NOT NULL)",
        ],
    ],
    adopt: None,
};

const HOLDS_CHANNEL: &str = "nanocode_board_holds";

const HOLD_LEASE: Duration = Duration::from_secs(15);

const HOLD: &str = "INSERT INTO nanocode_board_holds AS hold (key, holder, expires_at) VALUES ($1::text, CAST(gen_random_uuid() AS TEXT), clock_timestamp() + CAST($2::text AS BIGINT) * interval '1 millisecond') ON CONFLICT (key) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at WHERE hold.expires_at <= clock_timestamp() RETURNING holder";

const HELD_MS: &str = "SELECT CAST(CAST(GREATEST(CEIL(EXTRACT(EPOCH FROM expires_at - clock_timestamp()) * 1000), 0) AS BIGINT) AS TEXT) FROM nanocode_board_holds WHERE key = $1::text";

const RENEW: &str = "UPDATE nanocode_board_holds SET expires_at = clock_timestamp() + CAST($3::text AS BIGINT) * interval '1 millisecond' WHERE key = $1::text AND holder = $2::text RETURNING holder";

const UNHOLD: &str =
    "DELETE FROM nanocode_board_holds WHERE key = $1::text AND holder = $2::text RETURNING holder";

const WOKEN: &str = "SELECT CAST(count(*) AS TEXT) FROM (SELECT pg_notify('nanocode_board_holds', $1::text)) AS woken";

enum Claimed {
    Holding(String),
    Busy(Duration),
}

impl Ids {
    const fn of(store: &Store) -> Self {
        match store {
            Store::Dir(_) => Self::Unique,
            Store::Postgres(_) => Self::Minted,
        }
    }

    async fn submit(self, board: &mut Board, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        match self {
            Self::Minted => board.submit(at_ms, op).await,
            Self::Unique => {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| {
                        BoardError::Store(StoreError::Backend(
                            "the clock is before 1970".to_owned(),
                        ))
                    })?
                    .as_nanos();
                let key = format!(
                    "{:x}-{nanos:x}-{:x}",
                    std::process::id(),
                    SUBMITTED.fetch_add(1, Ordering::Relaxed)
                );
                board.submit_keyed(&key, at_ms, op).await
            }
        }
    }
}

impl Store {
    fn open(parsed: &Parsed, runtime: &nanocode_executor::Runtime) -> Result<Self, Failure> {
        let given = (parsed.one("dir")?, parsed.one("sql")?);
        let (dir, url) = match given {
            (Some(_), Some(_)) => return Err("give --dir or --sql, not both".into()),
            (None, None) => match (
                std::env::var("NC_BOARD_DIR").ok(),
                std::env::var("NC_BOARD_SQL").ok(),
            ) {
                (Some(_), Some(_)) => {
                    return Err(
                        "NC_BOARD_DIR and NC_BOARD_SQL are both set; unset one or pass --dir or --sql"
                            .into(),
                    );
                }
                (None, None) => {
                    return Err(
                        "give --dir DIR or --sql URL, or set NC_BOARD_DIR or NC_BOARD_SQL".into(),
                    );
                }
                given => given,
            },
            given => given,
        };
        match (dir, url) {
            (Some(dir), _) => Ok(Self::Dir(PathBuf::from(dir))),
            (None, Some(url)) => Postgres::connect(url, runtime).map(Self::Postgres),
            (None, None) => Err("give --dir DIR or --sql URL".into()),
        }
    }

    fn shown(&self) -> Value {
        match self {
            Self::Dir(dir) => json!({"dir": dir}),
            Self::Postgres(postgres) => json!({"postgres": postgres.shown()}),
        }
    }

    fn wake(&self, runtime: &nanocode_executor::Runtime) -> Wake {
        let Self::Postgres(postgres) = self else {
            return Wake::Poll;
        };
        let listening = runtime.block_on(async {
            let mut connection = PgConnection::connect(&postgres.options).await?;
            connection.listen(log_wake::CHANNEL).await?;
            Ok::<_, PgError>(connection)
        });
        match listening {
            Ok(connection) => Wake::Listen(connection),
            Err(error) => {
                eprintln!("nc-board: no Postgres wake ({error}); polling");
                Wake::Poll
            }
        }
    }
}

impl Postgres {
    fn connect(url: String, runtime: &nanocode_executor::Runtime) -> Result<Self, Failure> {
        let options = PgOptions::parse(&url).map_err(|error| format!("--sql: {error}"))?;
        let pool = PgPool::new(options.clone(), 1);
        let sql = Arc::new(PgSql::reading(pool.clone(), pool));
        let log = Arc::new(PartedLog::new(
            Arc::new(SqlParts::new(Arc::clone(&sql))),
            NonZeroU32::MIN,
        ));
        runtime
            .block_on(async {
                let statements = [SqlStatement {
                    sql: PREPARED,
                    params: Vec::new(),
                }];
                let answer = sql.transaction("nc_board_prepared", &statements).await?;
                let prepared = answer
                    .first()
                    .and_then(|rows| rows.first())
                    .and_then(|cells| cells.first())
                    .is_some_and(|cell| cell.as_deref() == Some("true"));
                if !prepared {
                    log.prepare().await?;
                }
                migrate_format(&*sql, "board_holds", &[HOLDS]).await
            })
            .map_err(|error| format!("{}: {error}", address(&options)))?;
        Ok(Self {
            url,
            options,
            sql,
            log,
        })
    }

    fn shown(&self) -> String {
        address(&self.options)
    }

    async fn claim(&self, key: &str) -> Result<Claimed, Failure> {
        let lease_ms = HOLD_LEASE.as_millis().to_string();
        let statements = [
            SqlStatement {
                sql: HOLD,
                params: vec![key, &lease_ms],
            },
            SqlStatement {
                sql: HELD_MS,
                params: vec![key],
            },
        ];
        let answer = transact(&self.sql, &self.shown(), "nc_board_hold", &statements).await?;
        if let Some(holder) = cell(&answer, 0) {
            return Ok(Claimed::Holding(holder.to_owned()));
        }
        let held_ms = cell(&answer, 1)
            .map_or(Ok(0), str::parse::<u64>)
            .map_err(|error| format!("{}: hold expiry: {error}", self.shown()))?;
        Ok(Claimed::Busy(Duration::from_millis(held_ms)))
    }

    async fn hold(&self, key: &str) -> Result<String, Failure> {
        let mut listening: Option<PgConnection> = None;
        loop {
            let held = match self.claim(key).await? {
                Claimed::Holding(holder) => return Ok(holder),
                Claimed::Busy(held) => held,
            };
            let Some(connection) = listening.as_mut() else {
                let mut connection = PgConnection::connect(&self.options)
                    .await
                    .map_err(|error| format!("{}: {error}", self.shown()))?;
                connection
                    .listen(HOLDS_CHANNEL)
                    .await
                    .map_err(|error| format!("{}: {error}", self.shown()))?;
                listening = Some(connection);
                continue;
            };
            let woken = nanocode_executor::timeout(held, async {
                loop {
                    if connection.notification().await?.payload == key {
                        return Ok::<_, PgError>(());
                    }
                }
            })
            .await;
            if let Ok(Err(error)) = woken {
                return Err(format!("{}: {error}", self.shown()).into());
            }
        }
    }

    async fn release(&self, key: &str, holder: &str) -> Result<bool, Failure> {
        let statements = [
            SqlStatement {
                sql: UNHOLD,
                params: vec![key, holder],
            },
            SqlStatement {
                sql: WOKEN,
                params: vec![key],
            },
        ];
        let answer = transact(&self.sql, &self.shown(), "nc_board_unhold", &statements).await?;
        Ok(cell(&answer, 0).is_some())
    }

    async fn held<T>(
        &self,
        key: &str,
        work: impl Future<Output = Result<T, Failure>>,
    ) -> Result<T, Failure> {
        let holder = self.hold(key).await?;
        let stop = Arc::new(Signal::new());
        let renewing = nanocode_executor::spawn(renewing(
            Arc::clone(&self.sql),
            self.shown(),
            (key.to_owned(), holder.clone()),
            Arc::clone(&stop),
        ));
        let result = work.await;
        stop.fire();
        let joined = renewing.await;
        let released = self.release(key, &holder).await;
        joined.map_err(|error| format!("renewing the write hold on {key}: {error}"))?;
        if !released? {
            return Err(format!(
                "{}: the write hold on {key} lapsed while writing; a write by the same actor may have been lost",
                self.shown()
            )
            .into());
        }
        result
    }
}

async fn renewing(sql: Arc<PgSql>, at: String, (key, holder): (String, String), stop: Arc<Signal>) {
    let lease_ms = HOLD_LEASE.as_millis().to_string();
    loop {
        if nanocode_executor::timeout(HOLD_LEASE / 3, stop.wait())
            .await
            .is_ok()
        {
            return;
        }
        let statements = [SqlStatement {
            sql: RENEW,
            params: vec![&key, &holder, &lease_ms],
        }];
        if let Ok(answer) = transact(&sql, &at, "nc_board_renew_hold", &statements).await
            && cell(&answer, 0).is_none()
        {
            return;
        }
    }
}

async fn transact(
    sql: &PgSql,
    at: &str,
    operation: &'static str,
    statements: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, String> {
    sql.transaction(operation, statements)
        .await
        .map_err(|error| format!("{at}: {error}"))
}

fn cell(answer: &[Vec<SqlRow>], statement: usize) -> Option<&str> {
    answer
        .get(statement)
        .and_then(|rows| rows.first())
        .and_then(|row| row.first())
        .and_then(|cell| cell.as_deref())
}

fn address(options: &PgOptions) -> String {
    let PgOptions {
        user,
        port,
        database,
        ..
    } = options;
    match &options.host {
        Host::Tcp(host) => format!("postgres://{user}@{host}:{port}/{database}"),
        Host::Unix(path) => format!(
            "postgres://{user}@/{database}?host={}&port={port}",
            path.display()
        ),
    }
}

impl Wake {
    fn nap(&mut self, runtime: &nanocode_executor::Runtime, wait: Duration) {
        let Self::Listen(connection) = self else {
            std::thread::sleep(wait);
            return;
        };
        let heard = runtime.block_on(nanocode_executor::timeout(wait, async {
            loop {
                let notification = connection.notification().await?;
                if log_wake::parse(&notification.payload).is_some_and(|(scope, _)| scope == SCOPE) {
                    return Ok::<_, PgError>(());
                }
            }
        }));
        if let Ok(Err(error)) = heard {
            eprintln!("nc-board: lost the Postgres wake ({error}); polling");
            *self = Self::Poll;
        }
    }
}

async fn opened(store: &Store, identity: Identity) -> Result<Board, Failure> {
    let host: Arc<dyn LogHost> = match store {
        Store::Dir(dir) => Arc::new(DirLog::open(dir, SCOPE)?),
        Store::Postgres(postgres) => Arc::clone(&postgres.log) as Arc<dyn LogHost>,
    };
    Board::open(host, SCOPE, identity)
        .await
        .map_err(|error| error.to_string().into())
}

fn locked<T>(
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    access: Access,
    work: impl AsyncFnOnce(&mut Board) -> Result<T, Failure>,
) -> Result<T, Failure> {
    match store {
        Store::Dir(_) => runtime.block_on(async {
            let mut board = opened(store, identity.clone()).await?;
            work(&mut board).await
        }),
        Store::Postgres(postgres) => runtime.block_on(async {
            let work = async {
                let mut board = opened(store, identity.clone()).await?;
                work(&mut board).await
            };
            match access {
                Access::Read => work.await,
                Access::Write => {
                    let key = format!("nc-board:{SCOPE}:{}", identity.actor());
                    postgres.held(&key, work).await
                }
            }
        }),
    }
}

fn pulling<T>(
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    (access, wait_ms, poll_ms): (Access, u64, u64),
    mut pass: impl AsyncFnMut(&mut Board) -> Result<Option<T>, Failure>,
) -> Result<Option<T>, Failure> {
    assert!(poll_ms > 0);
    let mut wake = store.wake(runtime);
    let deadline_ms = now_ms()?.saturating_add(wait_ms);
    loop {
        if let Some(found) = locked(store, identity, runtime, access, async |board| {
            pass(board).await
        })? {
            return Ok(Some(found));
        }
        let now = now_ms()?;
        if now >= deadline_ms {
            return Ok(None);
        }
        wake.nap(
            runtime,
            Duration::from_millis(poll_ms.min(deadline_ms - now)),
        );
    }
}

fn wait_poll(parsed: &Parsed) -> Result<(u64, u64), Failure> {
    let wait_ms = parsed.secs_ms("wait")?.ok_or("--wait SECS is required")?;
    let poll_ms = parsed
        .count("poll-ms")?
        .filter(|poll| *poll > 0)
        .ok_or("--wait needs --poll-ms MS, positive")?;
    Ok((wait_ms, poll_ms))
}

fn running(
    parsed: &Parsed,
    store: &Store,
    actor: &str,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    parsed.allow(&[
        "lease", "label", "spread", "result", "wait", "poll-ms", "on-exit",
    ])?;
    let lease_ms = parsed.lease_ms()?;
    let renew_every = Duration::from_millis(lease_ms / 3);
    if renew_every.is_zero() {
        return Err("run needs a lease of at least 3 ms to renew every third of it".into());
    }
    assert!(renew_every < Duration::from_millis(lease_ms));
    let (target, command) = parsed.words[1..]
        .split_first()
        .ok_or("run takes ID or next, then -- COMMAND")?;
    let (program, arguments) = command.split_first().ok_or("run needs -- COMMAND")?;
    let Some((task, claim)) = run_claimed(parsed, store, identity, runtime, target, lease_ms)?
    else {
        return Ok(None);
    };
    let ending = match parsed.one("on-exit")?.as_deref() {
        None | Some("finish") => Ending::Finish,
        Some("release") => Ending::Release,
        Some(other) => return Err(format!("--on-exit {other} is not finish or release").into()),
    };
    let (events, heard) = mpsc::channel();
    listen_for_stops(events.clone())?;
    let mut child = Command::new(program);
    child
        .args(arguments)
        .env("NC_BOARD_TASK", &task.0)
        .env("NC_BOARD_CLAIM", &claim.0)
        .env("NC_BOARD_AS", actor);
    match store {
        Store::Dir(dir) => child.env("NC_BOARD_DIR", dir).env_remove("NC_BOARD_SQL"),
        Store::Postgres(postgres) => child
            .env("NC_BOARD_SQL", &postgres.url)
            .env_remove("NC_BOARD_DIR"),
    };
    let mut child = child
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let pid = child.id();
    std::thread::spawn(move || events.send(Ended::Exited(child.wait())));
    let held = (task, claim);
    let ended = supervise(
        store,
        identity,
        runtime,
        &held,
        (lease_ms, renew_every),
        pid,
        &heard,
    )?;
    settle(parsed, store, identity, runtime, held, ended, ending)
}

fn run_claimed(
    parsed: &Parsed,
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    target: &str,
    lease_ms: u64,
) -> Result<Option<(TaskId, ClaimId)>, Failure> {
    let ids = Ids::of(store);
    if parsed.flags.contains_key("wait") {
        if target != "next" {
            return Err("--wait applies to run next".into());
        }
        let (wait_ms, poll_ms) = wait_poll(parsed)?;
        return pulling(
            store,
            identity,
            runtime,
            (Access::Write, wait_ms, poll_ms),
            async |board| {
                let Some(task) = claim_next(parsed, board, ids, now_ms()?).await? else {
                    return Ok(None);
                };
                let claim = held(board, &task, None)?;
                Ok(Some((task, claim)))
            },
        );
    }
    locked(store, identity, runtime, Access::Write, async |board| {
        let at_ms = now_ms()?;
        let task = if target == "next" {
            claim_next(parsed, board, ids, at_ms)
                .await?
                .ok_or("no ready task matches")?
        } else {
            let task = TaskId(target.to_owned());
            let op = Op::Claim {
                task: task.clone(),
                lease_ms,
                fence: None,
                cap: None,
            };
            submit(board, ids, at_ms, op).await?;
            task
        };
        let claim = held(board, &task, None)?;
        Ok((task, claim))
    })
    .map(Some)
}

fn listen_for_stops(stopping: mpsc::Sender<Ended>) -> Result<(), Failure> {
    let stops = [
        SignalKind::terminate(),
        SignalKind::interrupt(),
        SignalKind::hangup(),
    ]
    .map(|kind| signal(kind).map(|listener| (kind, listener)))
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| format!("listening for signals: {error}"))?;
    std::thread::spawn(move || {
        let mut stops = stops;
        let Ok(runtime) = nanocode_executor::Runtime::new() else {
            return;
        };
        let stopped = runtime.block_on(std::future::poll_fn(|context| {
            stops
                .iter_mut()
                .find_map(|(kind, listener)| {
                    listener
                        .poll_recv(context)
                        .is_ready()
                        .then_some(kind.as_raw_value())
                })
                .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
        }));
        let _ = stopping.send(Ended::Signalled(stopped));
    });
    Ok(())
}

fn supervise(
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    (task, claim): &(TaskId, ClaimId),
    (lease_ms, renew_every): (u64, Duration),
    pid: u32,
    heard: &mpsc::Receiver<Ended>,
) -> Result<Exit, Failure> {
    let ids = Ids::of(store);
    let stop = |exits: &mpsc::Receiver<Ended>| {
        let _ = Command::new("kill").arg(pid.to_string()).status();
        loop {
            match exits.recv() {
                Ok(Ended::Exited(_)) | Err(_) => return,
                Ok(Ended::Signalled(_)) => {}
            }
        }
    };
    loop {
        match heard.recv_timeout(renew_every) {
            Ok(Ended::Exited(status)) => return Ok(Exit::Status(status?)),
            Ok(Ended::Signalled(signal)) => {
                stop(heard);
                return Ok(Exit::Signalled(signal));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("lost the command's exit status".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let renewed = locked(store, identity, runtime, Access::Write, async |board| {
            let op = Op::Renew {
                task: task.clone(),
                claim: claim.clone(),
                lease_ms,
            };
            ids.submit(board, now_ms()?, op)
                .await
                .map(drop)
                .map_err(|error| error.to_string().into())
        });
        if let Err(error) = renewed {
            stop(heard);
            return Err(format!("claim of {task} lost, command stopped: {error}").into());
        }
    }
}

fn settle(
    parsed: &Parsed,
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    (task, claim): (TaskId, ClaimId),
    ended: Exit,
    ending: Ending,
) -> Result<Option<Value>, Failure> {
    let ids = Ids::of(store);
    let code = match &ended {
        Exit::Status(status) => status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal)),
        Exit::Signalled(signal) => 128 + signal,
    };
    let result = parsed.one("result")?;
    locked(store, identity, runtime, Access::Write, async |board| {
        let holding = matches!(
            board.task(&task).map(|task| &task.phase),
            Some(Phase::Claimed { claim: held, .. }) if *held == claim
        );
        if holding {
            let (task, claim) = (task.clone(), claim.clone());
            let op = match (&ended, ending) {
                (Exit::Signalled(signal), _) => Op::Release {
                    task,
                    claim,
                    note: format!("nc-board run stopped by signal {signal}"),
                },
                (Exit::Status(_), Ending::Release) => Op::Release {
                    task,
                    claim,
                    note: format!("exit {code} without finishing"),
                },
                (Exit::Status(status), Ending::Finish) if status.success() => Op::Finish {
                    task,
                    claim,
                    outcome: Outcome::Done(result.unwrap_or_else(|| "exit 0".to_owned())),
                },
                (Exit::Status(_), Ending::Finish) => Op::Finish {
                    task,
                    claim,
                    outcome: Outcome::Failed(format!("exit {code}")),
                },
            };
            submit(board, ids, now_ms()?, op).await?;
        }
        Ok(Some(
            json!({"task": shown(board, &task, now_ms()?)?, "exit_code": code}),
        ))
    })
}

#[derive(Clone, Copy)]
enum Ending {
    Finish,
    Release,
}

enum Ended {
    Exited(std::io::Result<std::process::ExitStatus>),
    Signalled(i32),
}

enum Exit {
    Status(std::process::ExitStatus),
    Signalled(i32),
}

fn waiting(
    parsed: &Parsed,
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    parsed.allow(&["after", "label", "kind", "under", "timeout", "poll-ms"])?;
    parsed.words::<0>()?;
    let filter = parsed.filter()?;
    let timeout_ms = parsed
        .secs_ms("timeout")?
        .ok_or("--timeout SECS is required")?;
    let poll_ms = parsed
        .count("poll-ms")?
        .filter(|poll| *poll > 0)
        .ok_or("--poll-ms MS is required and positive")?;
    let mut after = parsed.count("after")?;
    pulling(
        store,
        identity,
        runtime,
        (Access::Read, timeout_ms, poll_ms),
        async |board| {
            let from = after.unwrap_or_else(|| board.fold().next().saturating_sub(1));
            board
                .fire_due(now_ms()?)
                .await
                .map_err(|error| error.to_string())?;
            after = Some(from);
            let found = changes(board, from, &filter);
            Ok((!found.is_empty()).then_some(Value::Array(found)))
        },
    )
}

fn whoami(
    parsed: &Parsed,
    store: &Store,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    parsed.allow(&[])?;
    parsed.words::<0>()?;
    locked(store, identity, runtime, Access::Read, async |board| {
        let at_ms = now_ms()?;
        let actor = identity.actor();
        let claims = board
            .tasks()
            .filter_map(|(id, task)| match &task.phase {
                Phase::Claimed {
                    claim,
                    worker,
                    until_ms,
                } if *worker == board.writer() => Some(json!({
                    "task": id,
                    "claim": claim,
                    "until_ms": until_ms,
                    "expired": *until_ms <= at_ms,
                })),
                _ => None,
            })
            .collect::<Vec<_>>();
        let can = Can::ALL
            .into_iter()
            .filter(|can| board.may("*", *can))
            .collect::<Vec<_>>();
        Ok(Some(json!({
            "actor": board.writer(),
            "kind": actor.kind(),
            "profile": actor.profile(),
            "board": store.shown(),
            "owner": board.fold().access().owner(),
            "can": can,
            "claims": claims,
        })))
    })
}

fn session(parsed: &Parsed) -> Result<Option<Value>, Failure> {
    assert!(!parsed.words.is_empty());
    let actor = match parsed.words[0].as_str() {
        "list" | "show" | "events" | "may" => parsed
            .setting("as", "NC_BOARD_AS")
            .unwrap_or_else(|_| "agent:reader".to_owned()),
        _ => parsed.setting("as", "NC_BOARD_AS")?,
    };
    let identity = identity(&actor)?;
    let runtime = nanocode_executor::Runtime::new()?;
    let store = Store::open(parsed, &runtime)?;
    let ids = Ids::of(&store);
    match parsed.words[0].as_str() {
        "whoami" => return whoami(parsed, &store, &identity, &runtime),
        "wait" => return waiting(parsed, &store, &identity, &runtime),
        "run" => return running(parsed, &store, &actor, &identity, &runtime),
        "claim-next" if parsed.flags.contains_key("wait") => {
            let (wait_ms, poll_ms) = wait_poll(parsed)?;
            return pulling(
                &store,
                &identity,
                &runtime,
                (Access::Write, wait_ms, poll_ms),
                async |board| run(parsed, board, ids).await,
            );
        }
        _ => {}
    }
    let reader = matches!(parsed.words[0].as_str(), "list" | "show" | "events");
    let access = if reader || matches!(parsed.words[0].as_str(), "grants" | "may") {
        Access::Read
    } else {
        Access::Write
    };
    locked(&store, &identity, &runtime, access, async |board| {
        if reader && board.fold().access().owner().is_some() && !board.may("*", Can::Read) {
            return Err(format!("{actor} may not read this board").into());
        }
        run(parsed, board, ids).await
    })
}

pub fn main() -> ExitCode {
    let parsed = match Parsed::parse(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("nc-board: {error}");
            return ExitCode::FAILURE;
        }
    };
    if parsed.words.first().is_none_or(|word| word == "help") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match session(&parsed) {
        Ok(Some(value)) => {
            println!("{value}");
            match value.get("exit_code").and_then(Value::as_i64) {
                Some(0) | None => ExitCode::SUCCESS,
                Some(code) => u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from),
            }
        }
        Ok(None) if parsed.words[0] == "wait" => {
            eprintln!("nc-board: no change matched before the timeout");
            ExitCode::from(2)
        }
        Ok(None) => {
            eprintln!("nc-board: no ready task matches");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("nc-board: {error}");
            ExitCode::FAILURE
        }
    }
}

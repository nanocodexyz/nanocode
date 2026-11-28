use std::{
    cell::RefCell,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::{
    Nanocode,
    backend::Prompt,
    session_host::{Submission, Submitted, TurnState},
    turnlog::{
        Compaction, Extension, LogHost, MemoryLog, OneLog, OneLogStore, Policy, Rider, ScopeRows,
        StoredCheckpoint, Write,
    },
};
use nanocode_code_mode::code_mode::{CellJournalSink, CellJournalUpdate};
use nanocode_durability_core::{
    OwnerId, OwnerToken, StateStore, StoreError, StoreFuture, StoreRecord,
};
use nanocode_turnlog::Fold;
use serde_json::{Map, Value, json};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    memory::{self, Model},
    rng::Rng,
    threadscope::{World, driven, flag, raw},
};

const THREAD: &str = "warm";
const STATE: &str = "agent";
const SESSION: &str = "cells";
const TURNS: Range<u64> = 24..49;
const LONGER: u64 = 4;
const SNAPSHOT_ROWS: Range<u64> = 4..10;
const PAGE_MESSAGES: Range<u64> = 4..10;
const MESSAGE_BYTES: Range<u64> = 128..1_025;
const JOURNAL_BYTES: Range<u64> = 128..1_025;
const RETAIN_FINISHED: usize = 16;
const REPORT_SNAPSHOT_ROWS: u64 = 128;
const REPORT_PAGE_MESSAGES: u64 = 32;
const AGENT_ROUNDS: u64 = 2;
const AGENT_BLOB_BYTES: Range<usize> = 200..16_384;
const WORDS: [&str; 8] = [
    "the", "agent", "reads", "a", "file", "and", "writes", "output",
];

#[derive(Default)]
struct Meter {
    calls: AtomicU64,
    rows: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub calls: u64,
    pub rows: u64,
    pub bytes: u64,
    pub resident: u64,
}

impl Meter {
    fn cost(&self) -> Cost {
        Cost {
            calls: self.calls.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            resident: 0,
        }
    }

    fn read<'a>(&self, rows: impl Iterator<Item = &'a (u64, String)>) {
        let (mut count, mut bytes) = (0, 0);
        for (_, body) in rows {
            count += 1;
            bytes += body.len() as u64;
        }
        assert!(count > 0 || bytes == 0, "a read of no rows reads no bytes");
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(count, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Cost {
    fn since(self, before: Self, resident: usize) -> Self {
        assert!(self.rows >= before.rows, "rows read only grow");
        assert!(self.bytes >= before.bytes, "bytes read only grow");
        Self {
            calls: self.calls - before.calls,
            rows: self.rows - before.rows,
            bytes: self.bytes - before.bytes,
            resident: resident as u64,
        }
    }
}

struct Metered {
    inner: Arc<dyn LogHost>,
    meter: Arc<Meter>,
}

impl LogHost for Metered {
    fn call_bound(&self) -> Option<Duration> {
        self.inner.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.inner.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.inner.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let rows = self.inner.scan(scope, from, to).await?;
            self.meter.read(rows.iter());
            Ok(rows)
        })
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let found = self.inner.scans(scans).await?;
            self.meter.read(found.iter().flatten());
            Ok(found)
        })
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let found = self.inner.scan_ranges(scope, ranges).await?;
            self.meter.read(found.iter().flatten());
            Ok(found)
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let rows = self.inner.load(scope).await?;
            self.meter.read(rows.iter());
            Ok(rows)
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.inner.append(scope, rows)
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.inner.append_compacting(scope, rows, compaction)
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.inner.extend(scope, extension)
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.inner.carry(scope, write, rider)
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        self.inner.checkpoint(scope)
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.inner.save_checkpoint(scope, checkpoint)
    }
}

#[derive(Clone, Copy)]
struct Shape {
    turns: u64,
    snapshot_rows: u64,
    page: u64,
}

struct Host {
    log: Arc<dyn LogHost>,
    meter: Arc<Meter>,
}

impl Host {
    fn new() -> Self {
        let meter = Arc::new(Meter::default());
        let log = Arc::new(Metered {
            inner: Arc::new(MemoryLog::new()),
            meter: Arc::clone(&meter),
        });
        Self { log, meter }
    }
}

fn text(rng: &mut Rng, bytes: &Range<u64>) -> String {
    let length = rng.range(bytes.start, bytes.end);
    let mut text = String::with_capacity(usize::try_from(length).unwrap_or(0) + 16);
    while (text.len() as u64) < length {
        let at = usize::try_from(rng.below(WORDS.len() as u64)).expect("a word index fits");
        text.push_str(WORDS[at]);
        text.push_str(&format!(" {} ", rng.below(1 << 20)));
    }
    text
}

fn key(kind: u64, at: u64) -> String {
    format!("b3:{:064x}", (kind << 48) + at)
}

fn page_of(shape: Shape, at: u64) -> Range<u64> {
    let first = at - at % shape.page;
    first..at + 1
}

fn records(rng: &mut Rng, shape: Shape, at: u64) -> Vec<StoreRecord> {
    let messages: Vec<String> = page_of(shape, at).map(|message| key(1, message)).collect();
    let pages: Vec<String> = (0..at / shape.page)
        .map(|sealed| key(2, sealed * shape.page + shape.page - 1))
        .chain(std::iter::once(key(2, at)))
        .collect();
    let message = json!({"role": "assistant", "text": text(rng, &MESSAGE_BYTES)});
    vec![
        StoreRecord {
            key: key(1, at),
            value: Arc::from(format!("={message}")),
        },
        StoreRecord {
            key: key(2, at),
            value: Arc::from(format!("={}", json!({"messages": messages}))),
        },
        StoreRecord {
            key: key(3, at),
            value: Arc::from(format!("={}", json!({"pages": pages}))),
        },
    ]
}

fn journal(rng: &mut Rng, cell: u64) -> Result<CellJournalUpdate, String> {
    serde_json::from_value(json!({
        "session_id": SESSION,
        "cell_id": cell,
        "seq": 1,
        "header": {
            "version": 1,
            "origin_call_id": format!("exec-{cell}"),
            "script": text(rng, &JOURNAL_BYTES),
            "tools": [],
            "yield_time_ms": 10_000
        },
        "entries_from": 0,
        "entries": [],
        "calls": [{
            "id": 1,
            "call_id": format!("nested-{cell}"),
            "name": "Bash",
            "input": {"command": "cargo test"},
            "state": {"state": "completed", "encoded": text(rng, &JOURNAL_BYTES), "success": true}
        }],
        "observer_call_id": null,
        "content_returned": 0,
        "finished": true
    }))
    .map_err(|error| format!("a cell journal update does not decode: {error}"))
}

fn policy(shape: Shape) -> Policy {
    Policy {
        retain_finished: Some(RETAIN_FINISHED),
        snapshot_rows: Some(usize::try_from(shape.snapshot_rows).expect("a row count fits")),
        ..Policy::default()
    }
}

async fn opened(host: &Host, shape: Shape) -> Result<Arc<OneLog>, String> {
    nanocode::turnlog::thread_scope_sim::open(Arc::clone(&host.log), THREAD, policy(shape))
        .await
        .map_err(|error| format!("the log does not open: {error}"))
}

async fn submitted(log: &OneLog, rng: &mut Rng, at: u64) -> Result<String, String> {
    let request = format!("turn-{at}");
    let submission = Submission {
        request_id: request.clone(),
        prompt: Prompt::new(text(rng, &MESSAGE_BYTES)),
        hand: None,
        metadata: raw(&json!({ "seq": at })),
    };
    match log.submit(&submission).await {
        Ok(Submitted::Accepted { .. }) => {}
        answer => return Err(format!("turn {at} submits as {answer:?}")),
    }
    match log.start(&request).await {
        Ok(Some(_)) => Ok(request),
        answer => Err(format!("turn {at} starts as {answer:?}")),
    }
}

async fn finished(
    log: &OneLog,
    sink: &dyn CellJournalSink,
    rng: &mut Rng,
    request: &str,
    at: u64,
) -> Result<(), String> {
    sink.save(journal(rng, at + 1)?)
        .await
        .map_err(|error| format!("turn {at} journal: {error}"))?;
    sink.forget(SESSION, at + 1).await;
    let result = Map::from_iter([("text".to_owned(), Value::String(format!("answer {at}")))]);
    match log.finish(request, TurnState::Completed, &result).await {
        Ok(Some(_)) => Ok(()),
        answer => Err(format!("turn {at} finishes as {answer:?}")),
    }
}

struct Stored {
    log: Arc<OneLog>,
    store: OneLogStore,
    owner: OwnerToken,
    sink: Arc<dyn CellJournalSink>,
}

async fn stored_turn(
    stored: &mut Stored,
    rng: &mut Rng,
    shape: Shape,
    at: u64,
) -> Result<(), String> {
    let request = submitted(&stored.log, rng, at).await?;
    let payload = json!({"turn": at, "history": key(3, at)}).to_string();
    let records = records(rng, shape, at);
    stored
        .store
        .replace(STATE, &stored.owner, at, &payload, &records)
        .await
        .map_err(|error| format!("turn {at} replaces: {error}"))?;
    finished(&stored.log, &*stored.sink, rng, &request, at).await
}

async fn written(host: &Host, rng: &mut Rng, shape: Shape) -> Result<(), String> {
    let log = opened(host, shape).await?;
    let mut store = log.store();
    let owner = store
        .acquire(STATE, OwnerId::new())
        .await
        .map_err(|error| format!("the state does not acquire: {error}"))?
        .owner;
    let mut stored = Stored {
        sink: log.sink(),
        log,
        store,
        owner,
    };
    for at in 0..shape.turns {
        stored_turn(&mut stored, rng, shape, at).await?;
    }
    stored
        .store
        .flush()
        .await
        .map_err(|error| format!("the log does not flush: {error}"))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Reopen {
    pub open: Cost,
    pub restore: Cost,
}

async fn reopened(host: &Host, shape: Shape, whole: bool) -> Result<Reopen, String> {
    let before = host.meter.cost();
    let log = opened(host, shape).await?;
    let open = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    let before = host.meter.cost();
    let mut store = log.store();
    store
        .acquire(STATE, OwnerId::new())
        .await
        .map_err(|error| format!("the reopened state does not acquire: {error}"))?;
    let last = shape.turns - 1;
    let wanted: Vec<String> = if whole {
        (0..shape.turns).map(|at| key(1, at)).collect()
    } else {
        page_of(shape, last).map(|at| key(1, at)).collect()
    };
    let read = store
        .read_records(STATE, &wanted)
        .await
        .map_err(|error| format!("the reopened state reads its messages: {error}"))?;
    if let Some(at) = read.iter().position(Option::is_none) {
        return Err(format!("the reopened state lost message {}", wanted[at]));
    }
    let restore = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    Ok(Reopen { open, restore })
}

async fn row_bytes_max(host: &Host) -> Result<u64, String> {
    let rows = host
        .log
        .scan(THREAD, 0, nanocode_turnlog::SCAN_END)
        .await
        .map_err(|error| format!("the log scans: {error}"))?;
    Ok(rows
        .iter()
        .map(|(_, body)| body.len() as u64)
        .max()
        .unwrap_or(0))
}

async fn measured(rng: &mut Rng, shape: Shape) -> Result<(Reopen, u64), String> {
    let host = Host::new();
    written(&host, rng, shape).await?;
    let reopen = reopened(&host, shape, false).await?;
    Ok((reopen, row_bytes_max(&host).await?))
}

fn bounded(world: &World, shape: Shape, short: Reopen, long: (Reopen, u64)) {
    let (long, row_bytes_max) = long;
    let slack_rows = shape.snapshot_rows + shape.page + 2;
    let reads = |reopen: Reopen| {
        (
            reopen.open.rows + reopen.restore.rows,
            reopen.open.bytes + reopen.restore.bytes,
        )
    };
    let (short_rows, short_bytes) = reads(short);
    let (long_rows, long_bytes) = reads(long);
    let resident = |reopen: Reopen| reopen.open.resident.max(reopen.restore.resident);
    let wrong = if long_rows > short_rows + slack_rows {
        Some(format!(
            "reads {long_rows} rows, past the {short_rows} rows after {} turns plus {slack_rows}",
            shape.turns
        ))
    } else if long_bytes > short_bytes + slack_rows * row_bytes_max {
        Some(format!(
            "reads {long_bytes} bytes, past the {short_bytes} bytes after {} turns plus {slack_rows} rows of {row_bytes_max} bytes",
            shape.turns
        ))
    } else if resident(long) > resident(short) + slack_rows * row_bytes_max {
        Some(format!(
            "holds {} resident bytes, past the {} after {} turns plus {slack_rows} rows of {row_bytes_max} bytes",
            resident(long),
            resident(short),
            shape.turns
        ))
    } else {
        None
    };
    if let Some(wrong) = wrong {
        flag(
            &world.wrong,
            format!(
                "warm: a reopen after {} turns (snapshot every {} rows, pages of {}) {wrong}",
                shape.turns * LONGER,
                shape.snapshot_rows,
                shape.page
            ),
        );
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World) {
    let shape = Shape {
        turns: rng.range(TURNS.start, TURNS.end),
        snapshot_rows: rng.range(SNAPSHOT_ROWS.start, SNAPSHOT_ROWS.end),
        page: rng.range(PAGE_MESSAGES.start, PAGE_MESSAGES.end),
    };
    let longer = Shape {
        turns: shape.turns * LONGER,
        ..shape
    };
    let mut writes = Rng::derive(rng.below(u64::MAX), "warm-short");
    let short = driven(world, "a short log", measured(&mut writes, shape));
    let mut writes = Rng::derive(rng.below(u64::MAX), "warm-long");
    let long = driven(world, "a long log", measured(&mut writes, longer));
    match (short, long) {
        (Some(Ok((short, _))), Some(Ok(long))) => bounded(world, shape, short, long),
        (Some(Err(error)), _) | (_, Some(Err(error))) => {
            flag(&world.wrong, format!("warm: {error}"));
        }
        _ => {}
    }
}

struct Agent {
    log: Arc<OneLog>,
    runtime: Nanocode,
    agent: Option<nanocode::Agent>,
    sink: Arc<dyn CellJournalSink>,
}

fn claude(log: &Arc<OneLog>, model: &Arc<Model>) -> Result<Nanocode, String> {
    let store = nanocode::session_store(log.store()).map_err(|error| error.to_string())?;
    memory::claude_runtime(model, Some(store), memory::tools())
}

async fn agent_turn(agent: &Agent, model: &Model, rng: &mut Rng, at: u64) -> Result<(), String> {
    let request = submitted(&agent.log, rng, at).await?;
    model.begin(at);
    memory::prompt(agent.agent.as_ref().ok_or("no agent")?, at).await?;
    finished(&agent.log, &*agent.sink, rng, &request, at).await
}

async fn agent_reopen(seed: u64, shape: Shape) -> Result<Reopen, String> {
    let host = Host::new();
    let mut rng = Rng::derive(seed, "warm-agent");
    let model = Arc::new(Model::sized(
        &memory::BACKENDS[0],
        AGENT_ROUNDS,
        AGENT_BLOB_BYTES.start,
        AGENT_BLOB_BYTES.end,
        seed,
    ));
    let log = opened(&host, shape).await?;
    let runtime = claude(&log, &model)?;
    let spawned = memory::claude_spawn(&runtime, true).await?;
    let agent = Agent {
        sink: log.sink(),
        log,
        runtime,
        agent: Some(spawned),
    };
    for at in 0..shape.turns {
        agent_turn(&agent, &model, &mut rng, at).await?;
    }
    memory::close(agent.agent).await;
    drop(agent.runtime.shutdown().await);
    drop(agent.sink);
    let mut store = agent.log.store();
    store
        .flush()
        .await
        .map_err(|error| format!("the log does not flush: {error}"))?;
    drop(store);
    drop(agent.log);
    let before = host.meter.cost();
    let log = opened(&host, shape).await?;
    let open = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    let before = host.meter.cost();
    let runtime = claude(&log, &model)?;
    let reopened = memory::reopen(&runtime, None, memory::Reopen::Open).await?;
    let restore = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    memory::close(Some(reopened)).await;
    drop(runtime.shutdown().await);
    Ok(Reopen { open, restore })
}

async fn stored_reopen(seed: u64, shape: Shape) -> Result<Reopen, String> {
    let host = Host::new();
    let mut rng = Rng::derive(seed, "warm-stored");
    written(&host, &mut rng, shape).await?;
    reopened(&host, shape, true).await
}

fn report(seed: u64, agent: bool, turns: u64) -> Result<Reopen, String> {
    let shape = Shape {
        turns,
        snapshot_rows: REPORT_SNAPSHOT_ROWS,
        page: REPORT_PAGE_MESSAGES,
    };
    let clock = Clock::new();
    crate::host::entered(seed, &clock, || {
        let done = RefCell::new(None);
        let mut executor = Executor::new(Arc::clone(&clock), Rng::derive(seed, "warm-report"));
        let task: Task<'_> = Box::pin(async {
            let reopen = if agent {
                agent_reopen(seed, shape).await
            } else {
                stored_reopen(seed, shape).await
            };
            done.replace(Some(reopen));
        });
        match executor.run(vec![task], None) {
            Ended::Finished => done
                .into_inner()
                .unwrap_or_else(|| Err("the reopen answered nothing".to_owned())),
            ended => Err(format!("the reopen ended {ended:?}")),
        }
    })
}

pub fn main(args: &[String]) -> std::process::ExitCode {
    let agent = args.iter().any(|arg| arg == "--agent");
    for turns in args.iter().filter_map(|arg| arg.parse::<u64>().ok()) {
        match report(1, agent, turns) {
            Ok(Reopen { open, restore }) => println!(
                "WARM turns={turns} agent={agent} open_calls={} open_rows={} open_bytes={} open_resident={} restore_calls={} restore_rows={} restore_bytes={} restore_resident={}",
                open.calls,
                open.rows,
                open.bytes,
                open.resident,
                restore.calls,
                restore.rows,
                restore.bytes,
                restore.resident
            ),
            Err(error) => {
                println!("WARM turns={turns} agent={agent} failed: {error}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

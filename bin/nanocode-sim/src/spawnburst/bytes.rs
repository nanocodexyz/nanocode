use std::{
    cell::RefCell,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::durability::{Admission, DurableOwner, EncodedPayload, SessionRecord, SessionStore};
use nanocode::turnlog::{
    ChildScope, Compaction, Extension, LogHost, MemoryLog, OneLog, Rider, ScopeRows,
    StoredCheckpoint, Write, child_scope,
};
use nanocode_agent_core::{Lineage, Origin};
use nanocode_backend_codex::durability::CodexFacts;
use nanocode_code_mode::code_mode::{CellJournalSink, CellJournalUpdate};
use nanocode_durability_core::{ChildScopes, FactLog, StateStore, StoreError, StoreFuture};
use serde_json::{Value, json};

use super::{ROOT, THREAD, facts::notice, serving_policy};
use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
    scripted::simulated_model,
    threadscope::{self, World},
};

pub(super) const LEGACY_SCOPE_BYTES: u64 = 1_024;
pub(super) const RESIDENT_SCOPE_BYTES: u64 = 2_560;
const TURNS: Range<u64> = 1..3;
const FACTS_PER_TURN: Range<u64> = 1..4;
const FACT_TEXT_BYTES: Range<u64> = 32..2_048;
const JOURNAL_TEXT_BYTES: Range<u64> = 32..1_024;
const TOKEN_PPM: u64 = 350_000;
const TOKEN_BYTES: Range<u64> = 6..17;
const SEQ_BYTES: u64 = 8;
const TOKEN_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_-./";
const WORDS: [&str; 16] = [
    "the", "child", "reads", "file", "and", "writes", "result", "to", "log", "with", "tool",
    "output", "of", "cargo", "test", "build",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub writes: u64,
    pub rows: u64,
    pub bytes: u64,
}

impl Written {
    fn since(self, before: Self) -> Self {
        assert!(self.writes >= before.writes, "writes only grow");
        assert!(self.bytes >= before.bytes, "bytes only grow");
        Self {
            writes: self.writes - before.writes,
            rows: self.rows - before.rows,
            bytes: self.bytes - before.bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layouts {
    pub own: Written,
    pub parent: Written,
}

#[derive(Default)]
struct Meter {
    writes: AtomicU64,
    rows: AtomicU64,
    bytes: AtomicU64,
}

impl Meter {
    fn written(&self) -> Written {
        Written {
            writes: self.writes.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn landed<'a>(&self, scope: &str, bodies: impl Iterator<Item = &'a str>) {
        let key = scope.len() as u64 + SEQ_BYTES;
        let (mut rows, mut bytes) = (0, 0);
        for body in bodies {
            rows += 1;
            bytes += key + body.len() as u64;
        }
        assert!(
            rows > 0 || bytes == 0,
            "a write without rows lands no bytes"
        );
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(rows, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

struct Metered {
    inner: Arc<dyn LogHost>,
    meter: Arc<Meter>,
}

impl Metered {
    fn appended(&self, scope: &str, rows: &[(u64, String)], compaction: &Compaction<'_>) {
        let bodies = rows
            .iter()
            .chain(compaction.archived)
            .map(|(_, body)| body.as_str());
        self.meter.landed(scope, bodies);
    }

    async fn extended(&self, scope: &str, extension: Extension<'_>) -> Result<(), StoreError> {
        let landed = self
            .inner
            .scan(scope, extension.seq, extension.seq + 1)
            .await?;
        assert!(landed.len() <= 1, "one seq holds one row");
        let Some((seq, body)) = landed.first() else {
            return Err(StoreError::Backend(format!(
                "an extended row of {scope} is missing at seq {}",
                extension.seq
            )));
        };
        assert_eq!(*seq, extension.seq, "a scan of one seq answers that seq");
        self.rewrote(scope, body.len() as u64);
        Ok(())
    }

    fn rewrote(&self, scope: &str, bytes: u64) {
        self.meter.writes.fetch_add(1, Ordering::Relaxed);
        self.meter.rows.fetch_add(1, Ordering::Relaxed);
        self.meter
            .bytes
            .fetch_add(scope.len() as u64 + SEQ_BYTES + bytes, Ordering::Relaxed);
    }
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
        self.inner.scan(scope, from, to)
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        self.inner.scans(scans)
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        self.inner.scan_ranges(scope, ranges)
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.inner.load(scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.inner.append(scope, rows).await?;
            self.meter
                .landed(scope, rows.iter().map(|(_, body)| body.as_str()));
            Ok(())
        })
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.inner
                .append_compacting(scope, rows, compaction.clone())
                .await?;
            self.appended(scope, rows, &compaction);
            Ok(())
        })
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let extending = self.inner.extend(scope, extension)?;
        Some(Box::pin(async move {
            extending.await?;
            self.extended(scope, extension).await
        }))
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let counted = match &write {
            Write::Append { rows, compaction } => Ok((*rows, compaction.clone())),
            Write::Extend(extension) => Err(*extension),
        };
        let carrying = self.inner.carry(scope, write, rider)?;
        Some(Box::pin(async move {
            carrying.await?;
            match counted {
                Ok((rows, compaction)) => {
                    self.appended(scope, rows, &compaction);
                    Ok(())
                }
                Err(extension) => self.extended(scope, extension).await,
            }
        }))
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
        let saving = self.inner.save_checkpoint(scope, checkpoint)?;
        Some(Box::pin(async move {
            saving.await?;
            let body = checkpoint.version.len() + checkpoint.body.len();
            self.rewrote(scope, body as u64);
            Ok(())
        }))
    }
}

fn text(rng: &mut Rng, bytes: u64) -> String {
    let mut text = String::with_capacity(usize::try_from(bytes).unwrap_or(0) + 16);
    while (text.len() as u64) < bytes {
        if rng.chance(TOKEN_PPM) {
            for _ in 0..rng.range(TOKEN_BYTES.start, TOKEN_BYTES.end) {
                let at = usize::try_from(rng.below(TOKEN_ALPHABET.len() as u64))
                    .expect("an alphabet index fits");
                text.push(char::from(TOKEN_ALPHABET[at]));
            }
        } else {
            let at = usize::try_from(rng.below(WORDS.len() as u64)).expect("a word index fits");
            text.push_str(WORDS[at]);
        }
        text.push(' ');
    }
    assert!(text.len() as u64 >= bytes, "the text reaches its length");
    text
}

fn sized(rng: &mut Rng, bytes: &Range<u64>) -> String {
    let length = rng.range(bytes.start, bytes.end);
    text(rng, length)
}

fn payload(value: &Value) -> Result<EncodedPayload, String> {
    EncodedPayload::encode(value).map_err(|error| format!("a checkpoint does not encode: {error}"))
}

fn child_id(index: u64) -> String {
    format!("child-{index}")
}

fn journal(rng: &mut Rng, id: &str, cell: u64) -> Result<CellJournalUpdate, String> {
    let script = sized(rng, &JOURNAL_TEXT_BYTES);
    let output = sized(rng, &JOURNAL_TEXT_BYTES);
    serde_json::from_value(json!({
        "session_id": id,
        "cell_id": cell,
        "seq": 1,
        "header": {
            "version": 1,
            "origin_call_id": format!("exec-{id}-{cell}"),
            "script": script,
            "tools": [],
            "yield_time_ms": 10_000
        },
        "entries_from": 0,
        "entries": [],
        "calls": [{
            "id": 1,
            "call_id": format!("nested-{id}-{cell}"),
            "name": "Bash",
            "input": {"command": "cargo test"},
            "state": {"state": "completed", "encoded": output, "success": true}
        }],
        "observer_call_id": null,
        "content_returned": 0,
        "finished": true
    }))
    .map_err(|error| format!("a cell journal update does not decode: {error}"))
}

struct Turning<'a> {
    facts: &'a CodexFacts,
    sink: &'a dyn CellJournalSink,
    owner: Option<&'a DurableOwner>,
    id: &'a str,
}

async fn admitted(owner: Option<&DurableOwner>, turn: &str, prompt: &str) -> Result<(), String> {
    let Some(owner) = owner else {
        return Ok(());
    };
    match owner
        .admit_typed::<_, Value, Value>(turn.to_owned(), &json!({"prompt": prompt}))
        .await
    {
        Ok(Admission::Accepted | Admission::Pending) => {}
        answer => return Err(format!("a child turn {turn} was admitted as {answer:?}")),
    }
    owner
        .begin_attempt(turn.to_owned())
        .await
        .map_err(|error| format!("a child turn {turn} did not begin: {error}"))
}

async fn turns(rng: &mut Rng, turning: &Turning<'_>) -> Result<(), String> {
    for turn in 0..rng.range(TURNS.start, TURNS.end) {
        let name = format!("turn-{turn}");
        let prompt = sized(rng, &FACT_TEXT_BYTES);
        admitted(turning.owner, &name, &prompt).await?;
        for _ in 0..rng.range(FACTS_PER_TURN.start, FACTS_PER_TURN.end) {
            let said = sized(rng, &FACT_TEXT_BYTES);
            turning
                .facts
                .commit(notice(&said)?)
                .await
                .map_err(|error| format!("child {} fact: {error}", turning.id))?;
        }
        turning
            .sink
            .save(journal(rng, turning.id, turn + 1)?)
            .await
            .map_err(|error| format!("child {} journal: {error}", turning.id))?;
        if let Some(owner) = turning.owner {
            owner
                .complete(
                    name,
                    payload(&json!({"turn": turn}))?,
                    &json!({"answer": prompt}),
                )
                .await
                .map_err(|error| format!("child {} turn: {error}", turning.id))?;
        }
    }
    Ok(())
}

struct Legacy {
    log: Arc<OneLog>,
    store: SessionStore,
    spawns: Arc<dyn ChildScopes>,
    sink: Arc<dyn CellJournalSink>,
    owner: DurableOwner,
}

async fn legacy_parent(host: Arc<dyn LogHost>, layout: ChildScope) -> Result<Legacy, String> {
    let log = nanocode::turnlog::thread_scope_sim::open(host, THREAD, serving_policy())
        .await
        .map_err(|error| format!("the parent's log did not open: {error}"))?;
    log.set_child_scope(layout).map_err(|error| {
        format!(
            "the parent refused child scope {}: {error}",
            layout.setting()
        )
    })?;
    let store = nanocode::session_store(log.store()).map_err(|error| error.to_string())?;
    let spawns = log
        .store()
        .child_scopes()
        .ok_or_else(|| "the parent's store places no children".to_owned())?;
    let session = store
        .session(SessionRecord::root(ROOT, simulated_model(), None))
        .await
        .map_err(|error| error.to_string())?;
    let (owner, _) = session
        .acquire_agent()
        .await
        .map_err(|error| error.to_string())?;
    admitted(Some(&owner), super::OPERATION, "spawn the children").await?;
    let sink = log.sink();
    Ok(Legacy {
        log,
        store,
        spawns,
        sink,
        owner,
    })
}

async fn legacy_child(parent: &Legacy, seed: u64, index: u64) -> Result<(), String> {
    let id = child_id(index);
    let mut rng = Rng::derive(seed ^ index, "child-bytes-child");
    parent
        .spawns
        .spawn(&id)
        .await
        .map_err(|error| format!("child {id} spawn: {error}"))?;
    let lineage = Lineage::child_of(&Lineage::root(ROOT), ROOT, Origin::Subagent);
    let record = SessionRecord::root(id.clone(), simulated_model(), None).with_lineage(lineage);
    let session = parent
        .store
        .session(record)
        .await
        .map_err(|error| format!("child {id} session: {error}"))?;
    let (owner, _) = session
        .acquire_agent()
        .await
        .map_err(|error| format!("child {id} owner: {error}"))?;
    let log = session
        .facts()
        .ok_or_else(|| format!("child {id} keeps no facts"))?;
    let facts = CodexFacts::open(log).map_err(|error| format!("child {id} facts: {error}"))?;
    let turning = Turning {
        facts: &facts,
        sink: &*parent.sink,
        owner: Some(&owner),
        id: &id,
    };
    turns(&mut rng, &turning).await
}

async fn legacy_burst(
    host: Arc<dyn LogHost>,
    meter: &Meter,
    seed: u64,
    children: u64,
    layout: ChildScope,
) -> Result<Written, String> {
    let parent = legacy_parent(host, layout).await?;
    let before = meter.written();
    let burst = (0..children).map(|index| legacy_child(&parent, seed, index));
    let failed = nanocode_sync::future::join_all(burst)
        .await
        .into_iter()
        .find_map(Result::err);
    if let Some(error) = failed {
        return Err(error);
    }
    parent
        .owner
        .complete(
            super::OPERATION.to_owned(),
            payload(&json!({"spawned": children}))?,
            &json!(null),
        )
        .await
        .map_err(|error| format!("the parent turn did not complete: {error}"))?;
    parent
        .log
        .store()
        .flush()
        .await
        .map_err(|error| format!("the parent's log did not flush: {error}"))?;
    let spawned = parent.log.spawned_children().len() as u64;
    assert!(
        spawned == children || layout == ChildScope::Parent,
        "every child in its own scope is recorded on the parent"
    );
    Ok(meter.written().since(before))
}

pub(super) fn legacy(seed: u64, children: u64, layout: ChildScope) -> Result<Written, String> {
    let clock = Clock::new();
    crate::host::entered(seed, &clock, || {
        let meter = Arc::new(Meter::default());
        let host: Arc<dyn LogHost> = Arc::new(Metered {
            inner: Arc::new(MemoryLog::new()),
            meter: Arc::clone(&meter),
        });
        let done = RefCell::new(None);
        let mut executor =
            Executor::new(Arc::clone(&clock), Rng::derive(seed, "child-bytes-legacy"));
        let task: Task<'_> = Box::pin(async {
            let burst = legacy_burst(host, &meter, seed, children, layout).await;
            done.replace(Some(burst));
        });
        match executor.run(vec![task], None) {
            Ended::Finished => done
                .into_inner()
                .unwrap_or_else(|| Err("the legacy burst answered nothing".to_owned())),
            ended => Err(format!("the legacy burst ended {ended:?}")),
        }
    })
}

struct Resident {
    states: Box<dyn StateStore>,
    spawns: Arc<dyn ChildScopes>,
    sink: Arc<dyn CellJournalSink>,
}

async fn resident_child(parent: &Resident, seed: u64, index: u64) -> Result<(), String> {
    let id = child_id(index);
    let mut rng = Rng::derive(seed ^ index, "child-bytes-child");
    parent
        .spawns
        .spawn(&id)
        .await
        .map_err(|error| format!("child {id} spawn: {error}"))?;
    let logs = parent
        .states
        .facts()
        .ok_or_else(|| "the facade keeps no facts".to_owned())?;
    let log: Arc<dyn FactLog> = logs.scoped(&id, false);
    let facts = CodexFacts::open(log).map_err(|error| format!("child {id} facts: {error}"))?;
    let turning = Turning {
        facts: &facts,
        sink: &*parent.sink,
        owner: None,
        id: &id,
    };
    turns(&mut rng, &turning).await
}

async fn resident_open(
    logs: Arc<nanocode::turnlog::ThreadLogs>,
    layout: ChildScope,
) -> Result<Resident, String> {
    let over = OneLog::over(logs, THREAD, &serving_policy())
        .await
        .map_err(|error| format!("the parent's facade did not open: {error}"))?;
    over.set_child_scope(layout).map_err(|error| {
        format!(
            "the facade refused child scope {}: {error}",
            layout.setting()
        )
    })?;
    let states = over.states().await.map_err(|error| error.to_string())?;
    let spawns = states
        .child_scopes()
        .ok_or_else(|| "the facade places no children".to_owned())?;
    let sink = over.sink().await.map_err(|error| error.to_string())?;
    Ok(Resident {
        states,
        spawns,
        sink,
    })
}

async fn resident_burst(parent: &mut Resident, seed: u64, children: u64) -> Result<(), String> {
    for index in 0..children {
        resident_child(parent, seed, index).await?;
    }
    parent
        .states
        .flush()
        .await
        .map_err(|error| format!("the facade did not flush: {error}"))
}

fn checkpoint_written(world: &World) -> Written {
    let stats = world.checkpoint_stats();
    Written {
        writes: stats.manifests,
        rows: stats.manifests + stats.parts_written,
        bytes: stats.part_bytes_written + stats.listing_bytes_written,
    }
}

fn stored(world: &World, scopes: &[String]) -> Written {
    let mut written = checkpoint_written(world);
    for scope in scopes {
        let rows = world.rows(scope);
        written.writes += u64::from(!rows.is_empty());
        written.rows += rows.len() as u64;
        written.bytes += rows
            .iter()
            .map(|(_, body)| scope.len() as u64 + SEQ_BYTES + body.len() as u64)
            .sum::<u64>();
    }
    written
}

async fn resident_flow(
    world: &World,
    node: &threadscope::Node,
    seed: u64,
    children: u64,
    layout: ChildScope,
) -> Result<Written, String> {
    let mut parent = resident_open(Arc::new(node.logs.clone()), layout).await?;
    let mut scopes = vec![THREAD.to_owned()];
    scopes.extend((0..children).map(|index| child_scope(THREAD, &child_id(index))));
    let before = stored(world, &scopes);
    resident_burst(&mut parent, seed, children).await?;
    for part in 0..threadscope::PARTS {
        node.logs
            .scopes()
            .release(nanocode_partitions::Partition(part))
            .await
            .map_err(|error| format!("the release of partition {part} failed: {error}"))?;
    }
    Ok(stored(world, &scopes).since(before))
}

fn resident_world(
    seed: u64,
    time: &Arc<Clock>,
    children: u64,
    layout: ChildScope,
) -> Result<Written, String> {
    let mut rng = Rng::derive(seed, "child-bytes-world");
    let world = threadscope::world(&mut rng, time);
    let node = world.node("child-bytes");
    let done = RefCell::new(None);
    let mut executor = Executor::new(Arc::clone(time), Rng::derive(seed, "child-bytes-resident"));
    let task: Task<'_> = Box::pin(async {
        let flow = resident_flow(&world, &node, seed, children, layout).await;
        done.replace(Some(flow));
    });
    match executor.run(vec![task], None) {
        Ended::Finished => done
            .into_inner()
            .unwrap_or_else(|| Err("the resident burst answered nothing".to_owned())),
        ended => Err(format!("the resident burst ended {ended:?}")),
    }
}

pub(super) fn resident(seed: u64, children: u64, layout: ChildScope) -> Result<Written, String> {
    let time = Clock::new();
    crate::host::entered(seed, &time, || {
        resident_world(seed, &time, children, layout)
    })
}

pub(super) fn measured(seed: u64, children: u64) -> Result<(Layouts, Layouts), String> {
    nanocode_turnlog::sim::release_layouts(true);
    let legacy = Layouts {
        own: legacy(seed, children, ChildScope::Own)?,
        parent: legacy(seed, children, ChildScope::Parent)?,
    };
    let resident = Layouts {
        own: resident(seed, children, ChildScope::Own)?,
        parent: resident(seed, children, ChildScope::Parent)?,
    };
    Ok((legacy, resident))
}

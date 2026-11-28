use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::turnlog::{
    OneLog, OneLogOver, Policy, ServedStore, ThreadLogs, ThreadPath, ThreadPaths, thread_scope_sim,
};
use nanocode_durability_core::{FactLogs, StateStore, StoreError, StoreFuture, fold_facts};
use nanocode_turnlog::{
    Compaction, Extension, LogHost, Rider, ScopeRows, StoredCheckpoint, Write, sim as log_sim,
};

use crate::{
    rng::Rng,
    threadscope::{World, driven, flag},
    threadsessions::{self, Transcript},
};

const THREAD: &str = "facts-reads";
const NODE: &str = "facts-reads";

struct Counted {
    log: Arc<dyn LogHost>,
    rows: AtomicU64,
}

impl Counted {
    fn wrapping(log: Arc<dyn LogHost>) -> Arc<Self> {
        Arc::new(Self {
            log,
            rows: AtomicU64::new(0),
        })
    }

    fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    async fn read(
        &self,
        call: StoreFuture<'_, Result<ScopeRows, StoreError>>,
    ) -> Result<ScopeRows, StoreError> {
        let rows = call.await?;
        self.rows.fetch_add(rows.len() as u64, Ordering::Relaxed);
        Ok(rows)
    }

    async fn reads(
        &self,
        call: StoreFuture<'_, Result<Vec<ScopeRows>, StoreError>>,
    ) -> Result<Vec<ScopeRows>, StoreError> {
        let scopes = call.await?;
        let rows: usize = scopes.iter().map(Vec::len).sum();
        self.rows.fetch_add(rows as u64, Ordering::Relaxed);
        Ok(scopes)
    }
}

impl LogHost for Counted {
    fn call_bound(&self) -> Option<Duration> {
        self.log.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.log.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.log.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        Box::pin(self.read(self.log.scan(scope, from, to)))
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.reads(self.log.scans(scans)))
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.reads(self.log.scan_ranges(scope, ranges)))
    }

    fn load<'a>(&'a self, scope: &'a str) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        Box::pin(self.read(self.log.load(scope)))
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.log.append(scope, rows)
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.log.append_compacting(scope, rows, compaction)
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.log.extend(scope, extension)
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.log.carry(scope, write, rider)
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        self.log.checkpoint(scope)
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.log.save_checkpoint(scope, checkpoint)
    }
}

struct Legacy;

impl ThreadPaths for Legacy {
    fn path(&self, _thread: &str) -> ThreadPath {
        ThreadPath::Legacy
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Loads {
    first: Vec<u64>,
    second: Vec<u64>,
}

fn facts(store: &dyn StateStore, path: &str) -> Result<Arc<dyn FactLogs>, String> {
    store
        .facts()
        .ok_or_else(|| format!("the {path} old writer offers no fact log"))
}

async fn folded(
    host: &Counted,
    facts: &dyn FactLogs,
    state: &str,
    from: u64,
) -> Result<u64, String> {
    let scoped = facts.scoped(state, false);
    let fold = fold_facts::<Transcript>(&*scoped)
        .await
        .map_err(|error| format!("a fold of {state} answers {error}"))?;
    let snapshot = fold
        .snapshot()
        .map_err(|error| format!("a snapshot of {state} answers {error}"))?;
    scoped.remember(snapshot);
    let rows = host.rows();
    assert!(rows >= from, "a host's row count only grows");
    Ok(rows - from)
}

async fn direct(host: Arc<Counted>, states: &[String]) -> Result<Loads, String> {
    let follower = log_sim::follow(Arc::clone(&host) as Arc<dyn LogHost>, THREAD)
        .await
        .map_err(|error| format!("a direct follow answers {error}"))?;
    let log = thread_scope_sim::claim(follower, Policy::default())
        .await
        .map_err(|error| format!("a direct claim answers {error}"))?;
    let mut loads = Loads::default();
    for state in states {
        let from = host.rows();
        let facts = facts(&log.store(), "direct")?;
        loads.first.push(folded(&host, &*facts, state, from).await?);
    }
    for state in states {
        let from = host.rows();
        let facts = facts(&log.store(), "direct")?;
        loads
            .second
            .push(folded(&host, &*facts, state, from).await?);
    }
    assert_eq!(
        loads.first.len(),
        states.len(),
        "one first load per session"
    );
    Ok(loads)
}

async fn served(over: &OneLogOver) -> Result<Arc<dyn FactLogs>, String> {
    match over.served_store().await {
        Ok(ServedStore::Legacy(store)) => facts(&store, "served"),
        Ok(ServedStore::Resident(_)) => Err("a legacy thread serves its resident store".to_owned()),
        Err(error) => Err(format!("a served store answers {error}")),
    }
}

async fn facade(
    logs: Arc<ThreadLogs>,
    host: Arc<Counted>,
    states: &[String],
) -> Result<Loads, String> {
    let over = OneLog::over(logs, THREAD, &Policy::default())
        .await
        .map_err(|error| format!("a facade open answers {error}"))?;
    over.claim()
        .await
        .map_err(|error| format!("a facade claim answers {error}"))?;
    let mut loads = Loads::default();
    for state in states {
        let facts = served(&over).await?;
        let from = host.rows();
        loads.first.push(folded(&host, &*facts, state, from).await?);
    }
    for state in states {
        let from = host.rows();
        let facts = served(&over).await?;
        loads
            .second
            .push(folded(&host, &*facts, state, from).await?);
    }
    assert_eq!(
        loads.first.len(),
        states.len(),
        "one first load per session"
    );
    Ok(loads)
}

fn agree(direct: &Loads, facade: &Loads) -> Result<(), String> {
    assert_eq!(
        direct.first.len(),
        facade.first.len(),
        "both paths load each session"
    );
    assert_eq!(
        direct.second.len(),
        facade.second.len(),
        "both paths reload each session"
    );
    if direct.second != facade.second {
        return Err(format!(
            "a second load on the facade's handle reads {:?} rows, a warm old writer {:?}",
            facade.second, direct.second
        ));
    }
    if direct.first != facade.first {
        return Err(format!(
            "a first load through the facade reads {:?} rows, the old writer opened directly {:?}",
            facade.first, direct.first
        ));
    }
    Ok(())
}

async fn compared(rng: &mut Rng, world: &World) -> Result<(), String> {
    let written = Counted::wrapping(world.host());
    let sessions =
        threadsessions::write(rng, Arc::clone(&written) as Arc<dyn LogHost>, THREAD).await?;
    let states = sessions.states();
    let host = Counted::wrapping(world.host());
    let shared = Arc::clone(&host) as Arc<dyn LogHost>;
    let node = world.node_wrapped(NODE, |_| (shared, None));
    let logs = Arc::new(node.logs.clone().with_paths(Arc::new(Legacy)));
    let facade = facade(logs, host, &states).await?;
    let direct = direct(Arc::clone(&written), &states).await?;
    agree(&direct, &facade)
}

pub(crate) fn checked(seed: u64, world: &World) {
    if world.wrong.read(Option::is_some) {
        return;
    }
    let mut rng = Rng::derive(seed, "facts_reads");
    if let Some(Err(error)) = driven(world, "a facts reads check", compared(&mut rng, world)) {
        flag(&world.wrong, format!("flips: facts reads: {error}"));
    }
}

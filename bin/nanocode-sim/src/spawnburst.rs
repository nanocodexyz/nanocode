mod bytes;
mod facts;
mod host;
mod judge;

use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc, time::Duration};

use nanocode::durability::{Admission, BeginStep, DurableOwner, EncodedPayload, SessionRecord};
use nanocode::turnlog::{
    ChildScope, OneLog, OneLogOver, ServedStore, ThreadPath, ThreadPaths, child_scope,
};
use nanocode_agent_core::{AgentHandle, Lineage, Origin};
use nanocode_subagents::{AgentTask, DEFAULT_MAX_SUBAGENTS, Registry};
use nanocode_sync::Atom;
use serde_json::{Value, json};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
    scripted::{Ledger, Script, ScriptedFactory, simulated_model},
    threadscope::World,
    wiring::Wiring,
};

use host::{FaultHost, Faults};
use judge::{Answer, Verdict};

const BURSTS: [u64; 3] = [6, 50, 200];
const THREAD: &str = "spawn-burst";
const ROOT: &str = "parent";
const OPERATION: &str = "parent-turn";
const STEP_KIND: &str = "spawn_agent";
const CHILD_STEPS_MAX: u64 = 4;
const CHILD_STEP_LATENCY_MS_MIN: u64 = 50;
const CHILD_STEP_LATENCY_MS_MAX: u64 = 2_000;
const RESUMES_MAX: u64 = 3;
const POISON_PPM: u64 = 300_000;
const SETTLE: Duration = Duration::from_secs(600);
const NS_PER_MS: u64 = 1_000_000;
const HISTORY_FACTS_MAX: u64 = 600;
const HISTORY_TEXT_BYTES_MAX: u64 = 4_096;
const HISTORY_SNAPSHOT_ROWS_MIN: u64 = 2;
const HISTORY_SNAPSHOT_ROWS_MAX: u64 = 64;
const PARENT_LAYOUT_PPM: u64 = 250_000;
const MEASURED_PPM: u64 = 250_000;
const PLAIN_PPM: u64 = 250_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpawnBurstMutation {
    #[default]
    None,
    ChildNeverAnswers,
    ReplayRespawns,
    SpawnedTwice,
    InterruptedReruns,
    RestoredChildTakesTurns,
    QueuedUntyped,
    FactsScanOnWriter,
    ChildOnParentLog,
    SpawnBatchSplit,
    ChildSnapshotsEveryRow,
    FacadeOnlyChildScope,
}

impl SpawnBurstMutation {
    const ALL: [Self; 12] = [
        Self::None,
        Self::ChildNeverAnswers,
        Self::ReplayRespawns,
        Self::SpawnedTwice,
        Self::InterruptedReruns,
        Self::RestoredChildTakesTurns,
        Self::QueuedUntyped,
        Self::FactsScanOnWriter,
        Self::ChildOnParentLog,
        Self::SpawnBatchSplit,
        Self::ChildSnapshotsEveryRow,
        Self::FacadeOnlyChildScope,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ChildNeverAnswers => "child_never_answers",
            Self::ReplayRespawns => "replay_respawns",
            Self::SpawnedTwice => "spawned_twice",
            Self::InterruptedReruns => "interrupted_reruns",
            Self::RestoredChildTakesTurns => "restored_child_takes_turns",
            Self::QueuedUntyped => "queued_untyped",
            Self::FactsScanOnWriter => "facts_scan_on_writer",
            Self::ChildOnParentLog => "child_on_parent_log",
            Self::SpawnBatchSplit => "spawn_batch_split",
            Self::ChildSnapshotsEveryRow => "child_snapshots_every_row",
            Self::FacadeOnlyChildScope => "facade_only_child_scope",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpawnBurstStats {
    pub seeds: u64,
    pub spawns: u64,
    pub started: u64,
    pub queued: u64,
    pub refused: u64,
    pub held: u64,
    pub failed: u64,
    pub interrupted: u64,
    pub folded: u64,
    pub replayed: u64,
    pub crashes: u64,
    pub appends: u64,
    pub stalls: u64,
    pub scans: u64,
    pub parent_appends: u64,
    pub parent_scans: u64,
    pub parent_jobs: u64,
    pub child_appends: u64,
    pub replays: u64,
    pub replay_scans: u64,
    pub replay_held_ms_max: u64,
    pub measured_children: u64,
    pub legacy_own_bytes: u64,
    pub legacy_parent_bytes: u64,
    pub legacy_own_rows: u64,
    pub legacy_parent_rows: u64,
    pub resident_own_bytes: u64,
    pub resident_parent_bytes: u64,
    pub resident_own_rows: u64,
    pub resident_parent_rows: u64,
}

impl SpawnBurstStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.spawns += other.spawns;
        self.started += other.started;
        self.queued += other.queued;
        self.refused += other.refused;
        self.held += other.held;
        self.failed += other.failed;
        self.interrupted += other.interrupted;
        self.folded += other.folded;
        self.replayed += other.replayed;
        self.crashes += other.crashes;
        self.appends += other.appends;
        self.stalls += other.stalls;
        self.scans += other.scans;
        self.parent_appends += other.parent_appends;
        self.parent_scans += other.parent_scans;
        self.parent_jobs += other.parent_jobs;
        self.child_appends += other.child_appends;
        self.replays += other.replays;
        self.replay_scans += other.replay_scans;
        self.replay_held_ms_max = self.replay_held_ms_max.max(other.replay_held_ms_max);
        self.measured_children += other.measured_children;
        self.legacy_own_bytes += other.legacy_own_bytes;
        self.legacy_parent_bytes += other.legacy_parent_bytes;
        self.legacy_own_rows += other.legacy_own_rows;
        self.legacy_parent_rows += other.legacy_parent_rows;
        self.resident_own_rows += other.resident_own_rows;
        self.resident_parent_rows += other.resident_parent_rows;
        self.resident_own_bytes += other.resident_own_bytes;
        self.resident_parent_bytes += other.resident_parent_bytes;
    }
}

#[derive(Clone, Copy, Debug)]
struct Shape {
    spawns: u64,
    history_facts: u64,
    history_text_bytes: usize,
    history_snapshot_rows: usize,
    faults: Faults,
    crash_at_ms: Option<u64>,
    poison: Option<u64>,
    script: Script,
    layout: ChildScope,
    measured: bool,
    wiring: Wiring,
}

impl Shape {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "spawn-burst-shape");
        let spawns = BURSTS[rng.pick(BURSTS.len())];
        let history_facts = rng.range(0, HISTORY_FACTS_MAX);
        let history_text_bytes = usize::try_from(rng.range(1, HISTORY_TEXT_BYTES_MAX))
            .expect("a history text length fits in usize");
        let history_snapshot_rows =
            usize::try_from(rng.range(HISTORY_SNAPSHOT_ROWS_MIN, HISTORY_SNAPSHOT_ROWS_MAX))
                .expect("a snapshot row count fits in usize");
        let faults = Faults::of(&mut rng);
        let crash_at_ms = rng
            .chance(host::CRASH_PPM)
            .then(|| rng.range(1, faults.burst_ms_max(spawns)));
        let poison = rng.chance(POISON_PPM).then(|| rng.below(spawns));
        let script = Script {
            steps_max: rng.range(1, CHILD_STEPS_MAX),
            step_latency_ms_min: CHILD_STEP_LATENCY_MS_MIN,
            step_latency_ms_max: rng.range(CHILD_STEP_LATENCY_MS_MIN, CHILD_STEP_LATENCY_MS_MAX),
            restore_latency_ms_max: 0,
            shutdown_latency_ms_max: 0,
            mutation: crate::agents::AgentsMutation::None,
        };
        let layout = if rng.chance(PARENT_LAYOUT_PPM) {
            ChildScope::Parent
        } else {
            ChildScope::Own
        };
        let measured = rng.chance(MEASURED_PPM);
        let wiring = Wiring::drawn(&mut rng, Wiring::Plain, PLAIN_PPM);
        Self {
            spawns,
            history_facts,
            history_text_bytes,
            history_snapshot_rows,
            faults,
            crash_at_ms,
            poison,
            script,
            layout,
            measured,
            wiring,
        }
    }

    const fn placed(&self) -> ChildScope {
        match self.wiring {
            Wiring::Served => ChildScope::Parent,
            Wiring::Plain | Wiring::Routed => self.layout,
        }
    }

    fn describe(&self) -> String {
        format!(
            "scenario=spawn_burst spawns={} history_facts={} history_bytes={} snapshot_rows={} {} crash_at_ms={:?} poison={:?} children={} wiring={}",
            self.spawns,
            self.history_facts,
            self.history_text_bytes,
            self.history_snapshot_rows,
            self.faults.describe(),
            self.crash_at_ms,
            self.poison,
            self.layout.setting(),
            self.wiring.name()
        )
    }
}

fn task(index: u64) -> AgentTask {
    AgentTask {
        role: "worker".to_owned(),
        task: format!("burst task {index}"),
        output_schema: Value::Object(serde_json::Map::new()),
    }
}

fn step_id(index: u64) -> String {
    format!("spawn-{index}")
}

fn child_state(index: u64) -> String {
    format!("child-{index}")
}

fn serving_policy() -> nanocode::turnlog::Policy {
    nanocode::turnlog::Policy {
        host_call_bound: Some(host::call_bound()),
        ..nanocode::turnlog::Policy::default()
    }
}

struct Legacy;

impl ThreadPaths for Legacy {
    fn path(&self, _thread: &str) -> ThreadPath {
        ThreadPath::Legacy
    }
}

type Placements = Arc<Atom<Vec<String>>>;

struct Parent {
    over: Option<Arc<OneLogOver>>,
    placements: Placements,
    owner: DurableOwner,
    session: nanocode::durability::DurableSession,
    store: nanocode::durability::SessionStore,
    host: Arc<FaultHost>,
    replays: facts::Replays,
    root: AgentHandle,
    registry: Arc<Registry>,
    log: Arc<nanocode::turnlog::OneLog>,
}

struct Opened {
    over: Option<Arc<OneLogOver>>,
    log: Arc<OneLog>,
    store: nanocode::durability::SessionStore,
}

async fn plain(
    host: &Arc<FaultHost>,
    policy: nanocode::turnlog::Policy,
    layout: ChildScope,
) -> Result<Opened, String> {
    let shared: Arc<dyn nanocode::turnlog::LogHost> = Arc::clone(host) as _;
    let log = nanocode::turnlog::thread_scope_sim::open(shared, THREAD, policy)
        .await
        .map_err(|error| format!("the parent's thread log did not open: {error}"))?;
    log.set_child_scope(layout).map_err(|error| {
        format!(
            "the parent refused child scope {}: {error}",
            layout.setting()
        )
    })?;
    let store = nanocode::session_store(log.store())
        .map_err(|error| format!("the parent's session store did not open: {error}"))?;
    Ok(Opened {
        over: None,
        log,
        store,
    })
}

async fn served(
    world: &World,
    host: &Arc<FaultHost>,
    policy: &nanocode::turnlog::Policy,
    layout: ChildScope,
) -> Result<Opened, String> {
    let shared: Arc<dyn nanocode::turnlog::LogHost> = Arc::clone(host) as _;
    let node = world.node_wrapped(THREAD, |_| (shared, None));
    let logs = Arc::new(node.logs.clone().with_paths(Arc::new(Legacy)));
    let over = OneLog::over(logs, THREAD, policy)
        .await
        .map_err(|error| format!("the parent's facade did not open: {error}"))?;
    over.claim()
        .await
        .map_err(|error| format!("the parent's facade did not claim: {error}"))?;
    over.set_child_scope(layout).map_err(|error| {
        format!(
            "the parent's facade refused child scope {}: {error}",
            layout.setting()
        )
    })?;
    let ServedStore::Legacy(served) = over
        .served_store()
        .await
        .map_err(|error| format!("the parent's served store did not open: {error}"))?
    else {
        return Err("a thread on the legacy path serves its resident store".to_owned());
    };
    let log = over
        .legacy()
        .ok_or_else(|| "a thread on the legacy path holds no old writer".to_owned())?;
    let store = nanocode::session_store(served)
        .map_err(|error| format!("the parent's session store did not open: {error}"))?;
    Ok(Opened {
        over: Some(over),
        log,
        store,
    })
}

async fn open(
    world: &World,
    host: &Arc<FaultHost>,
    ledger: &Arc<Ledger>,
    policy: nanocode::turnlog::Policy,
    shape: &Shape,
) -> Result<Parent, String> {
    let Opened { over, log, store } = match shape.wiring {
        Wiring::Served => served(world, host, &policy, shape.layout).await?,
        Wiring::Plain => plain(host, policy, shape.layout).await?,
        Wiring::Routed => return Err("spawn_burst opens no routed facade store".to_owned()),
    };
    let session = store
        .session(SessionRecord::root(ROOT, simulated_model(), None))
        .await
        .map_err(|error| format!("the parent's session did not open: {error}"))?;
    let (owner, _) = session
        .acquire_agent()
        .await
        .map_err(|error| format!("the parent did not acquire its session: {error}"))?;
    let root = AgentHandle::new(
        ROOT,
        simulated_model(),
        Arc::new(ScriptedFactory::new(Arc::clone(ledger))),
    )
    .with_child_journal(Some(session.child_journal()));
    let (registry, _control, _updates) = nanocode_subagents::channel(DEFAULT_MAX_SUBAGENTS);
    Ok(Parent {
        over,
        placements: Placements::default(),
        owner,
        session,
        store,
        host: Arc::clone(host),
        replays: facts::Replays::default(),
        root,
        registry,
        log,
    })
}

async fn admitted(parent: &Parent) -> Result<bool, String> {
    match parent
        .owner
        .admit_typed::<_, Value, Value>(OPERATION.to_owned(), &json!({"prompt": "spawn the burst"}))
        .await
    {
        Ok(Admission::Accepted | Admission::Pending) => {}
        Ok(Admission::Completed { .. }) => return Ok(false),
        Ok(other) => return Err(format!("the parent turn was admitted as {other:?}")),
        Err(error) => return Err(format!("the parent turn was not admitted: {error}")),
    }
    parent
        .owner
        .begin_attempt(OPERATION.to_owned())
        .await
        .map_err(|error| format!("the parent turn did not begin an attempt: {error}"))?;
    Ok(true)
}

async fn spawn_one(
    parent: &Parent,
    shape: &Shape,
    index: u64,
    mutation: SpawnBurstMutation,
) -> Answer {
    let step = step_id(index);
    let begun = parent
        .owner
        .begin_step(
            OPERATION.to_owned(),
            step.clone(),
            STEP_KIND.to_owned(),
            &json!({"task": index}),
        )
        .await;
    match begun {
        Ok(BeginStep::Execute) => {}
        Ok(BeginStep::Replay(output)) if mutation != SpawnBurstMutation::ReplayRespawns => {
            return Answer::replayed(&output);
        }
        Ok(BeginStep::Replay(_)) => {}
        Ok(BeginStep::Interrupted) if mutation == SpawnBurstMutation::InterruptedReruns => {}
        Ok(BeginStep::Interrupted) => {
            let answer = if parent.log.spawned(&child_state(index)) {
                Answer::Folded(child_state(index))
            } else {
                Answer::Interrupted
            };
            let completed = parent
                .owner
                .complete_step(OPERATION.to_owned(), step, &answer.output())
                .await;
            return Answer::completed(completed, answer);
        }
        Err(error) => return Answer::parent(&error),
    }
    if shape.poison == Some(index) {
        parent.host.poison();
        std::future::pending::<()>().await;
    }
    if mutation == SpawnBurstMutation::ChildNeverAnswers && index == 0 {
        std::future::pending::<()>().await;
    }
    if mutation == SpawnBurstMutation::SpawnedTwice && index == 0 {
        drop(
            nanocode_subagents::start_agent(&parent.root, &parent.registry, ROOT, task(index))
                .await,
        );
    }
    let started =
        nanocode_subagents::start_agent(&parent.root, &parent.registry, ROOT, task(index)).await;
    let answer = match Answer::of(started.map_err(|error| error.to_string())) {
        Answer::Started(_) => match child_facts(parent, index).await {
            Ok(()) => {
                placement(parent, index).await;
                Answer::Started(child_state(index))
            }
            Err(error) => Answer::failed(error),
        },
        other => other,
    };
    let completed = parent
        .owner
        .complete_step(OPERATION.to_owned(), step, &answer.output())
        .await;
    Answer::completed(completed, answer)
}

async fn child_facts(parent: &Parent, index: u64) -> Result<(), String> {
    let lineage = Lineage::child_of(&Lineage::root(ROOT), ROOT, Origin::Subagent);
    let record =
        SessionRecord::root(child_state(index), simulated_model(), None).with_lineage(lineage);
    let child = parent
        .store
        .session(record)
        .await
        .map_err(|error| error.to_string())?;
    let log = child
        .facts()
        .ok_or_else(|| format!("child {index}'s session keeps no facts"))?;
    let state = child_state(index);
    let scope = if parent.log.spawned(&state) {
        nanocode::turnlog::child_scope(THREAD, &state)
    } else {
        THREAD.to_owned()
    };
    let owner = facts::opened(log, scope, &parent.host, &parent.replays).await?;
    owner
        .commit(facts::notice(&format!("child {index} began"))?)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn misplaced(parent: &Parent, index: u64) -> Result<Option<String>, String> {
    let state = child_state(index);
    let scope = child_scope(THREAD, &state);
    let own = parent.host.raw_rows(&scope).await?;
    let named = scope.clone();
    let found = parent
        .log
        .read(move |fold| crate::wiring::recorded(fold.hosts(), &named))
        .await;
    if let Some(over) = &parent.over {
        assert_eq!(
            over.path(),
            ThreadPath::Legacy,
            "a served parent stays on the legacy path"
        );
    }
    Ok(crate::wiring::misplaced(&state, &scope, found, own.len()))
}

async fn placement(parent: &Parent, index: u64) {
    let wrong = match misplaced(parent, index).await {
        Ok(wrong) => wrong,
        Err(error) => Some(format!("child {index}'s placement did not read: {error}")),
    };
    if let Some(wrong) = wrong {
        parent
            .placements
            .update(|placements| placements.push(wrong.clone()));
    }
}

async fn history(
    world: &World,
    host: &Arc<FaultHost>,
    ledger: &Arc<Ledger>,
    shape: &Shape,
) -> Result<(), String> {
    let policy = nanocode::turnlog::Policy {
        snapshot_rows: Some(shape.history_snapshot_rows),
        ..serving_policy()
    };
    let parent = open(world, host, ledger, policy, shape).await?;
    facts::history(
        &parent.session,
        host,
        shape.history_facts,
        shape.history_text_bytes,
    )
    .await
}

async fn burst(
    parent: &Parent,
    shape: &Shape,
    mutation: SpawnBurstMutation,
) -> (BTreeMap<u64, Answer>, Result<(), String>) {
    let calls = (0..shape.spawns)
        .map(|index| async move { (index, spawn_one(parent, shape, index, mutation).await) });
    let answers: BTreeMap<u64, Answer> = nanocode_sync::future::join_all(calls)
        .await
        .into_iter()
        .collect();
    let completed = match EncodedPayload::encode(&json!({"spawned": answers.len()})) {
        Ok(checkpoint) => parent
            .owner
            .complete(
                OPERATION.to_owned(),
                checkpoint,
                &json!({"answers": answers.len()}),
            )
            .await
            .map_err(|error| format!("the parent turn could not save its history: {error}")),
        Err(error) => Err(format!("the parent's checkpoint did not encode: {error}")),
    };
    (answers, completed)
}

async fn attempt(
    world: &World,
    host: &Arc<FaultHost>,
    ledger: &Arc<Ledger>,
    shape: &Shape,
    mutation: SpawnBurstMutation,
    verdict: &std::cell::RefCell<Verdict>,
    logs: &std::cell::RefCell<Vec<Arc<nanocode::turnlog::OneLog>>>,
) {
    let parent = match open(world, host, ledger, serving_policy(), shape).await {
        Ok(parent) => parent,
        Err(error) => return verdict.borrow_mut().parent_failed(error),
    };
    logs.borrow_mut().push(Arc::clone(&parent.log));
    match admitted(&parent).await {
        Ok(true) => {}
        Ok(false) => return verdict.borrow_mut().finished(),
        Err(error) => return verdict.borrow_mut().parent_failed(error),
    }
    let (answers, completed) = burst(&parent, shape, mutation).await;
    verdict.borrow_mut().answered(answers);
    verdict
        .borrow_mut()
        .misplaced(parent.placements.read(Clone::clone));
    if let Err(error) = completed {
        verdict.borrow_mut().parent_failed(error);
    }
    nanocode_platform::sleep(SETTLE).await;
    verdict
        .borrow_mut()
        .replayed(parent.replays.read(Clone::clone));
    let listed = parent.registry.directory(ROOT, true, false).await;
    verdict.borrow_mut().listed(
        listed
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| (entry.agent_id.to_string(), entry.task))
                    .collect()
            })
            .map_err(|error| error.to_string()),
    );
}

struct Run {
    verdict: Verdict,
    stats: SpawnBurstStats,
    digest: u64,
}

fn simulate(seed: u64, mutation: SpawnBurstMutation, only: Option<&str>) -> Run {
    let shape = Shape::of(seed);
    nanocode_durability::sim::mutate(match mutation {
        SpawnBurstMutation::RestoredChildTakesTurns => {
            nanocode_durability::sim::Mutation::RestoredChildTakesTurns
        }
        _ => nanocode_durability::sim::Mutation::None,
    });
    nanocode_turnlog::sim::mutate(match mutation {
        SpawnBurstMutation::QueuedUntyped => nanocode_turnlog::sim::Mutation::QueuedUntyped,
        SpawnBurstMutation::FactsScanOnWriter => nanocode_turnlog::sim::Mutation::FactsScanOnWriter,
        _ => nanocode_turnlog::sim::Mutation::None,
    });
    nanocode::turnlog::thread_scope_sim::mutate(match mutation {
        SpawnBurstMutation::ChildOnParentLog => {
            nanocode::turnlog::thread_scope_sim::Mutation::ChildOnParentLog
        }
        SpawnBurstMutation::SpawnBatchSplit => {
            nanocode::turnlog::thread_scope_sim::Mutation::SpawnBatchSplit
        }
        SpawnBurstMutation::ChildSnapshotsEveryRow => {
            nanocode::turnlog::thread_scope_sim::Mutation::ChildSnapshotsEveryRow
        }
        SpawnBurstMutation::FacadeOnlyChildScope => {
            nanocode::turnlog::thread_scope_sim::Mutation::FacadeOnlyChildScope
        }
        _ => nanocode::turnlog::thread_scope_sim::Mutation::None,
    });
    nanocode_turnlog::sim::release_layouts(shape.layout == ChildScope::Own);
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let ledger = Ledger::new(seed, shape.script, Arc::clone(&clock));
    let host = Arc::new(FaultHost::new(seed, shape.faults, THREAD));
    let world = crate::threadscope::world(&mut Rng::derive(seed, "spawn-burst-world"), &clock);
    let logs = std::cell::RefCell::new(Vec::new());
    let verdict = std::cell::RefCell::new(Verdict::default());
    let built = std::cell::RefCell::new(None);
    let mut setup = Executor::new(Arc::clone(&clock), Rng::derive(seed, "spawn-burst-history"));
    let task: Task<'_> = Box::pin(async {
        built.replace(Some(history(&world, &host, &ledger, &shape).await));
    });
    let ended = setup.run(vec![task], None);
    if let Some(Err(error)) = built.into_inner() {
        verdict
            .borrow_mut()
            .parent_failed(format!("the parent's history did not build: {error}"));
    } else if ended != Ended::Finished {
        verdict
            .borrow_mut()
            .parent_failed(format!("the parent's history build ended {ended:?}"));
    }
    host.storm();
    let base_ns = clock.now_ns();
    let mut crash_at_ns = shape
        .crash_at_ms
        .map(|ms| base_ns.saturating_add(ms.saturating_mul(NS_PER_MS)));
    let mut attempts = 0;
    loop {
        attempts += 1;
        let scheduler = Rng::derive(seed ^ attempts, "spawn-burst-scheduler");
        let mut executor = Executor::new(Arc::clone(&clock), scheduler);
        let task: Task<'_> = Box::pin(attempt(
            &world, &host, &ledger, &shape, mutation, &verdict, &logs,
        ));
        match executor.run(vec![task], crash_at_ns.take()) {
            Ended::Finished => break,
            Ended::Stuck if host.poisoned() => verdict.borrow_mut().crashed(),
            Ended::Stuck => {
                verdict.borrow_mut().stuck();
                break;
            }
            Ended::Crashed => verdict.borrow_mut().crashed(),
        }
        if attempts > RESUMES_MAX {
            verdict.borrow_mut().resumes_exhausted(attempts);
            break;
        }
    }
    let restored = crate::factscopes::Restored {
        thread: THREAD,
        policy: serving_policy(),
        root: ROOT,
        children: (0..shape.spawns).map(child_state).collect(),
    };
    verdict.borrow_mut().restored(crate::factscopes::restored(
        &clock,
        seed,
        Arc::clone(&host) as _,
        &restored,
    ));
    verdict.borrow_mut().layout(shape.placed());
    verdict
        .borrow_mut()
        .spawned(logs.borrow().last().map(|log| log.spawned_children()));
    verdict
        .borrow_mut()
        .appended(host.parent_appends(), shape.spawns, attempts);
    if shape.measured && only.is_none_or(|only| only == "child_bytes") {
        verdict
            .borrow_mut()
            .bytes(shape.spawns, bytes::measured(seed, shape.spawns));
    }
    let verdict = verdict.into_inner();
    let mut stats = verdict.stats(shape.spawns);
    host.count(&mut stats);
    stats.parent_jobs = logs.borrow().iter().map(|log| log.jobs()).sum();
    nanocode_turnlog::sim::release_layouts(false);
    Run {
        digest: host.digest() ^ ledger.digest(),
        verdict,
        stats,
    }
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: SpawnBurstStats,
    shape: String,
}

fn panicked(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default()
}

fn run_seed(seed: u64, mutation: SpawnBurstMutation, only: Option<&str>) -> Outcome {
    let shape = Shape::of(seed).describe();
    match std::panic::catch_unwind(|| simulate(seed, mutation, only)) {
        Ok(run) => Outcome {
            digest: run.digest,
            wrong: run.verdict.wrong(only),
            stats: run.stats,
            shape,
        },
        Err(panic) => Outcome {
            digest: 0,
            wrong: Some(format!("panic: {}", panicked(&*panic))),
            stats: SpawnBurstStats::default(),
            shape,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpawnBurstArgs {
    pub mutation: SpawnBurstMutation,
    pub invariant: Option<&'static str>,
    pub known_red: bool,
}

pub fn names() -> String {
    SpawnBurstMutation::names()
}

pub fn invariants() -> String {
    judge::INVARIANTS.join("|")
}

pub fn invariant(name: &str) -> Option<&'static str> {
    judge::INVARIANTS.into_iter().find(|known| *known == name)
}

pub fn parse(name: &str) -> Option<SpawnBurstMutation> {
    SpawnBurstMutation::parse(name)
}

pub fn mutated(args: SpawnBurstArgs) -> bool {
    args.mutation != SpawnBurstMutation::None
}

pub fn judge(
    seed: u64,
    args: SpawnBurstArgs,
    twice: bool,
    verbose: bool,
    totals: &mut SpawnBurstStats,
) -> Option<String> {
    let first = run_seed(seed, args.mutation, args.invariant);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, args.mutation, args.invariant);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, args: SpawnBurstArgs) -> String {
    let mut command = format!("nanocode-sim --scenario spawn_burst --seed {seed}");
    if args.mutation != SpawnBurstMutation::None {
        command.push_str(" --mutation ");
        command.push_str(args.mutation.name());
    }
    if let Some(only) = args.invariant {
        command.push_str(" --invariant ");
        command.push_str(only);
    }
    if args.known_red {
        command.push_str(" --known-red");
    }
    command.push_str(&Wiring::flag());
    command
}

fn known_red(seeds: &Range<u64>, passed: u64, failed: u64, args: SpawnBurstArgs) -> ExitCode {
    let invariant = args.invariant.unwrap_or("all");
    println!(
        "RESULT seeds={}..{} scenario=spawn_burst known_red={invariant} passed={passed} failed={failed}",
        seeds.start, seeds.end
    );
    if failed > 0 {
        println!("MUTATION known_red_{invariant} caught by {failed} seeds");
        return ExitCode::SUCCESS;
    }
    println!(
        "MUTATION known_red_{invariant} NOT caught: the invariant holds on these seeds, so move it to a pass job"
    );
    ExitCode::FAILURE
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    args: SpawnBurstArgs,
    totals: &SpawnBurstStats,
) -> ExitCode {
    let mutation = args.mutation;
    if args.known_red {
        return known_red(seeds, passed, failed, args);
    }
    println!(
        "RESULT seeds={}..{} scenario=spawn_burst mutation={} invariant={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name(),
        args.invariant.unwrap_or("all")
    );
    match mutation {
        SpawnBurstMutation::None if failed == 0 => ExitCode::SUCCESS,
        SpawnBurstMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}

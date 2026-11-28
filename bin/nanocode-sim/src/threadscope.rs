use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
};

use nanocode::{
    backend::Prompt,
    claude::sim::Mutation as ClaudeMutation,
    durability::facts::{Decision, EffectResult, Fact, Intent, IntentTarget, Marker, TurnStatus},
    session_host::{LogEntry, Submission, Submitted, TurnState},
    turnlog::{
        FinishedFilter, Found, Probes, ThreadCommand, ThreadFacts, ThreadFold, ThreadLog,
        ThreadLogs, ThreadPolicy, ThreadReply, ThreadState, thread_history,
        thread_scope_sim::{self, Mutation},
    },
};
use nanocode_durability_core::{
    BlobRef, Digest, DurableFold, FactLogs, FoldError, Input, InputKind, OwnedFact, Seen,
    StoreError, StoreFuture, VALUE_INLINE_BYTES, commit_fact, fold_facts,
};
use nanocode_scope::{
    Checkpoints, ClaimHook, Committed, Manifest, MemoryCheckpoints, Part, PartedLeases, Policy,
    Resident, Route, ScopeError, Wiring,
    sim::{self as scope_sim, Mutation as ScopeMutation},
};
use nanocode_sync::{Atom, block_on};
use nanocode_turnlog::{
    CachedAt, FOLD_SNAPSHOT_KEY, Fold, Hold, LogHost, Phase, Record, SCAN_END, Status, Wake,
    parted::{ModelParts, PartStore, PartedLog},
    sim::Mutation as TurnLogMutation,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, json, value::RawValue};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
    threadlegacy::{self, Legacy, View},
    threadsessions::{self, Sessions},
};

pub(crate) const PARTS: u32 = 2;
const THREADS: Range<u64> = 2..5;
const NODES: Range<u64> = 2..4;
const STEPS: Range<u64> = 120..320;
const TTL_MS: u64 = 1_000;
const GRACE_MS: u64 = 300;
const MARGIN_MS: u64 = 100;
const TICK_MS_MAX: u64 = 250;
const START_MS: u64 = 1_000_000;
const CHECKPOINT_ROWS: Range<u64> = 2..8;
const CHECKPOINT_MS: u64 = 1_500;
const RESIDENT_BYTES: Range<u64> = 1_500..12_000;
const PART_ROWS: Range<u64> = 1..5;
const FORWARD_MS: u64 = 50;
const ADMIT_MS: u64 = TTL_MS;
const SESSIONS: u64 = 2;
const CELLS: u64 = 3;
const RELEASE_PPM: u64 = 40_000;
const CRASH_PPM: u64 = 12_000;
const TICK_PPM: u64 = 120_000;
const LEGACY_PPM: u64 = 500_000;
const SESSIONS_PPM: u64 = 500_000;
const SESSION_CHECK_PPM: u64 = 300_000;
const JOB_PPM: u64 = 150_000;
const FACT_LOG_PPM: u64 = 100_000;
const FACT_BLOB_PPM: u64 = 300_000;
const FACT_STATES: u64 = 2;
const STAGED_PPM: u64 = 150_000;
const STAGED_CRASH_PPM: u64 = 300_000;
const EVENTS_AHEAD: Range<u64> = 0..6;
const FILTER_BYTES: Range<u64> = 1..64;
const FILTER_HASHES: Range<u64> = 1..4;
const RESUBMIT_PPM: u64 = 80_000;
const COMPAT_RECENT_ROWS: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadScopeMutation(Mutation, ScopeMutation, TurnLogMutation, ClaudeMutation);

const SCOPE_MUTATIONS: [ScopeMutation; 3] = [
    ScopeMutation::ListingSkipsDropped,
    ScopeMutation::AdoptionSkipsCheckpoint,
    ScopeMutation::AdoptInRequest,
];

const TURN_LOG_MUTATIONS: [(&str, TurnLogMutation); 3] = [
    (
        "snapshot_resets_recovery",
        TurnLogMutation::SnapshotResetsRecovery,
    ),
    ("warm_loads_history", TurnLogMutation::WarmLoadsHistory),
    ("read_keeps_warm", TurnLogMutation::ReadKeepsWarm),
];

const CLAUDE_MUTATIONS: [(&str, ClaudeMutation); 1] = [(
    "agent_reopen_reads_history",
    ClaudeMutation::AgentReopenReadsHistory,
)];

impl ThreadScopeMutation {
    pub const NONE: Self = Self(
        Mutation::None,
        ScopeMutation::None,
        TurnLogMutation::None,
        ClaudeMutation::None,
    );

    pub fn parse(name: &str) -> Option<Self> {
        if let Some(mutation) = SCOPE_MUTATIONS
            .into_iter()
            .find(|scope| scope.name() == name)
        {
            return Some(Self(
                Mutation::None,
                mutation,
                TurnLogMutation::None,
                ClaudeMutation::None,
            ));
        }
        if let Some((_, mutation)) = TURN_LOG_MUTATIONS
            .into_iter()
            .find(|(named, _)| *named == name)
        {
            return Some(Self(
                Mutation::None,
                ScopeMutation::None,
                mutation,
                ClaudeMutation::None,
            ));
        }
        if let Some((_, mutation)) = CLAUDE_MUTATIONS
            .into_iter()
            .find(|(named, _)| *named == name)
        {
            return Some(Self(
                Mutation::None,
                ScopeMutation::None,
                TurnLogMutation::None,
                mutation,
            ));
        }
        Mutation::parse(name).map(|mutation| {
            Self(
                mutation,
                ScopeMutation::None,
                TurnLogMutation::None,
                ClaudeMutation::None,
            )
        })
    }

    pub fn name(self) -> &'static str {
        if let Some((name, _)) = TURN_LOG_MUTATIONS
            .into_iter()
            .find(|(_, mutation)| *mutation == self.2)
        {
            return name;
        }
        if let Some((name, _)) = CLAUDE_MUTATIONS
            .into_iter()
            .find(|(_, mutation)| *mutation == self.3)
        {
            return name;
        }
        match self.1 {
            ScopeMutation::None => self.0.name(),
            scope => scope.name(),
        }
    }

    pub fn names() -> String {
        Mutation::ALL
            .iter()
            .map(|mutation| mutation.name())
            .chain(SCOPE_MUTATIONS.iter().map(|mutation| mutation.name()))
            .chain(TURN_LOG_MUTATIONS.iter().map(|(name, _)| *name))
            .chain(CLAUDE_MUTATIONS.iter().map(|(name, _)| *name))
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadScopeStats {
    pub turn_ops: u64,
    pub turn_reopens: u64,
    pub hold_ops: u64,
    pub hold_reopens: u64,
    pub hold_settles: u64,
    pub wake_ops: u64,
    pub over_foreign: u64,
    pub staged: u64,
    pub staged_crashes: u64,
    pub window_checks: u64,
    pub resubmits: u64,
    pub moved_max: u64,
    pub claim_bytes_max: u64,
    pub claim_rows_max: u64,
    pub history_bytes_max: u64,
    pub import_ops: u64,
    pub store_ops: u64,
    pub store_reopens: u64,
    pub journal_ops: u64,
    pub journal_reopens: u64,
    pub seeds: u64,
    pub steps: u64,
    pub sends: u64,
    pub committed: u64,
    pub refused: u64,
    pub releases: u64,
    pub crashes: u64,
    pub resident_checks: u64,
    pub cold_checks: u64,
    pub legacy_checks: u64,
    pub legacy_threads: u64,
    pub legacy_turns: u64,
    pub legacy_retired: u64,
    pub legacy_blobs: u64,
    pub legacy_paged_rows: u64,
    pub adopted_checks: u64,
    pub adoptions: u64,
    pub adoption_rows: u64,
    pub loads: u64,
    pub load_rows_max: u64,
    pub checkpoints: u64,
    pub checkpoint_parts: u64,
    pub checkpoint_parts_kept: u64,
    pub evictions: u64,
    pub forwards: u64,
    pub jobs: u64,
    pub jobs_routed: u64,
    pub fact_logs: u64,
    pub fact_logs_routed: u64,
    pub fact_commits: u64,
    pub compat_flips: u64,
    pub compat_adoptions: u64,
    pub compat_commands: u64,
    pub session_threads: u64,
    pub session_turns: u64,
    pub session_turns_max: u64,
    pub session_bytes_max: u64,
    pub session_checks: u64,
    pub session_blobs_checked: u64,
    pub session_stored_bytes_max: u64,
    pub session_caches: u64,
    pub session_rows_bytes: u64,
    pub adoption_bytes: u64,
    pub adopted_bounded: u64,
    pub adopted_parts_max: u64,
    pub adopted_bytes_max: u64,
    pub stamps_settled: u64,
    pub stamps_uncertain: u64,
    pub stamps_dropped: u64,
    pub stamps_fenced: u64,
}

impl ThreadScopeStats {
    pub fn add(&mut self, other: &Self) {
        self.turn_ops += other.turn_ops;
        self.turn_reopens += other.turn_reopens;
        self.hold_ops += other.hold_ops;
        self.hold_reopens += other.hold_reopens;
        self.hold_settles += other.hold_settles;
        self.wake_ops += other.wake_ops;
        self.over_foreign += other.over_foreign;
        self.staged += other.staged;
        self.staged_crashes += other.staged_crashes;
        self.window_checks += other.window_checks;
        self.resubmits += other.resubmits;
        self.moved_max = self.moved_max.max(other.moved_max);
        self.claim_bytes_max = self.claim_bytes_max.max(other.claim_bytes_max);
        self.claim_rows_max = self.claim_rows_max.max(other.claim_rows_max);
        self.history_bytes_max = self.history_bytes_max.max(other.history_bytes_max);
        self.import_ops += other.import_ops;
        self.store_ops += other.store_ops;
        self.store_reopens += other.store_reopens;
        self.journal_ops += other.journal_ops;
        self.journal_reopens += other.journal_reopens;
        self.seeds += other.seeds;
        self.steps += other.steps;
        self.sends += other.sends;
        self.committed += other.committed;
        self.refused += other.refused;
        self.releases += other.releases;
        self.crashes += other.crashes;
        self.resident_checks += other.resident_checks;
        self.cold_checks += other.cold_checks;
        self.legacy_checks += other.legacy_checks;
        self.legacy_threads += other.legacy_threads;
        self.legacy_turns += other.legacy_turns;
        self.legacy_retired += other.legacy_retired;
        self.legacy_blobs += other.legacy_blobs;
        self.legacy_paged_rows += other.legacy_paged_rows;
        self.adopted_checks += other.adopted_checks;
        self.adoptions += other.adoptions;
        self.adoption_rows += other.adoption_rows;
        self.loads += other.loads;
        self.load_rows_max = self.load_rows_max.max(other.load_rows_max);
        self.checkpoints += other.checkpoints;
        self.checkpoint_parts += other.checkpoint_parts;
        self.checkpoint_parts_kept += other.checkpoint_parts_kept;
        self.evictions += other.evictions;
        self.forwards += other.forwards;
        self.jobs += other.jobs;
        self.jobs_routed += other.jobs_routed;
        self.fact_logs += other.fact_logs;
        self.fact_logs_routed += other.fact_logs_routed;
        self.fact_commits += other.fact_commits;
        self.compat_flips += other.compat_flips;
        self.compat_adoptions += other.compat_adoptions;
        self.compat_commands += other.compat_commands;
        self.session_threads += other.session_threads;
        self.session_turns += other.session_turns;
        self.session_turns_max = self.session_turns_max.max(other.session_turns_max);
        self.session_bytes_max = self.session_bytes_max.max(other.session_bytes_max);
        self.session_checks += other.session_checks;
        self.session_blobs_checked += other.session_blobs_checked;
        self.session_stored_bytes_max = self
            .session_stored_bytes_max
            .max(other.session_stored_bytes_max);
        self.session_caches += other.session_caches;
        self.session_rows_bytes += other.session_rows_bytes;
        self.adoption_bytes += other.adoption_bytes;
        self.adopted_bounded += other.adopted_bounded;
        self.adopted_parts_max = self.adopted_parts_max.max(other.adopted_parts_max);
        self.adopted_bytes_max = self.adopted_bytes_max.max(other.adopted_bytes_max);
        self.stamps_settled += other.stamps_settled;
        self.stamps_uncertain += other.stamps_uncertain;
        self.stamps_dropped += other.stamps_dropped;
        self.stamps_fenced += other.stamps_fenced;
    }
}

type Wrong = Arc<Atom<Option<String>>>;

pub(crate) fn flag(wrong: &Wrong, message: String) {
    wrong.update(|wrong| {
        if wrong.is_none() {
            *wrong = Some(message.clone());
        }
    });
}

#[derive(Default)]
struct Net {
    nodes: Atom<Vec<(String, ThreadLogs)>>,
}

impl Route<ThreadFold> for Net {
    fn forward<'a>(
        &'a self,
        holder: &'a str,
        scope: &'a str,
        command: ThreadCommand,
    ) -> StoreFuture<'a, Result<Committed<ThreadReply>, ScopeError>> {
        Box::pin(async move {
            let target = self.nodes.read(|nodes| {
                nodes
                    .iter()
                    .find(|(name, _)| name == holder)
                    .map(|(_, logs)| logs.clone())
            });
            match target {
                Some(logs) => logs.scopes().serve(scope, command).await,
                None => Err(ScopeError::Unreachable {
                    holder: holder.to_owned(),
                }),
            }
        })
    }
}

pub(crate) struct Node {
    name: String,
    pub(crate) logs: ThreadLogs,
    handles: BTreeMap<String, ThreadLog>,
}

pub(crate) struct World {
    time: Arc<Clock>,
    db: Arc<AtomicU64>,
    store: Arc<ModelParts>,
    witness: Arc<MemoryCheckpoints<ModelParts>>,
    net: Arc<Net>,
    reader: PartedLog<ModelParts>,
    host: Arc<dyn LogHost>,
    fold: ThreadFold,
    thread: ThreadPolicy,
    policy: Policy,
    count: NonZeroU32,
    threads: Vec<String>,
    legacy: BTreeMap<String, (Legacy, View)>,
    sessions: RefCell<BTreeMap<String, Sessions>>,
    bases: RefCell<BTreeMap<String, (u64, ThreadState)>>,
    answered: RefCell<BTreeMap<String, BTreeMap<u64, String>>>,
    notes: Cell<u64>,
    gone: Cell<(u64, u64)>,
    pub(crate) wrong: Wrong,
}

impl World {
    pub(crate) const fn clock(&self) -> &Arc<Clock> {
        &self.time
    }

    pub(crate) fn node(&self, name: &str) -> Node {
        self.node_wrapped(name, |log| (log, None))
    }

    pub(crate) fn node_wrapped(
        &self,
        name: &str,
        wrap: impl FnOnce(Arc<dyn LogHost>) -> (Arc<dyn LogHost>, Option<ClaimHook>),
    ) -> Node {
        let db = Arc::clone(&self.db);
        let log = Arc::new(
            PartedLog::fenced(Arc::clone(&self.store), self.count, name)
                .with_clock(Arc::new(move || db.load(Ordering::Relaxed)))
                .with_lease_bounds(GRACE_MS, MARGIN_MS),
        );
        let leases = PartedLeases::new(Arc::clone(&log), name, TTL_MS);
        let db = Arc::clone(&self.db);
        let (log, claimed) = wrap(log as Arc<dyn LogHost>);
        let logs = ThreadLogs::new(Wiring {
            fold: Arc::new(ThreadFold::new(self.thread)),
            log,
            leases: Arc::new(leases),
            checkpoints: Arc::clone(&self.witness) as Arc<dyn Checkpoints>,
            route: Arc::clone(&self.net) as Arc<dyn Route<ThreadFold>>,
            policy: self.policy,
            clock: Arc::new(move || db.load(Ordering::Relaxed)),
            claimed,
        });
        Node {
            name: name.to_owned(),
            logs,
            handles: BTreeMap::new(),
        }
    }

    pub(crate) fn foreign(
        &self,
        thread: &str,
        seq: u64,
        records: &[Record],
    ) -> Result<(), StoreError> {
        let row = nanocode_turnlog::encode(records)?;
        block_on(self.reader.append(thread, &[(seq, row)]))
    }

    pub(crate) fn foreign_row(
        &self,
        thread: &str,
        seq: u64,
        row: String,
    ) -> Result<(), StoreError> {
        block_on(self.reader.append(thread, &[(seq, row)]))
    }

    pub(crate) fn tick(&self) -> bool {
        self.time.advance_to_next()
    }

    fn note(&self) -> u64 {
        let note = self.notes.get() + 1;
        self.notes.set(note);
        note
    }

    pub(crate) fn host(&self) -> Arc<dyn LogHost> {
        Arc::clone(&self.host)
    }

    pub(crate) const fn fold(&self) -> &ThreadFold {
        &self.fold
    }

    pub(crate) fn expire(&self) {
        self.db
            .fetch_add(TTL_MS + GRACE_MS + MARGIN_MS, Ordering::Relaxed);
    }

    pub(crate) fn checkpoint_stats(&self) -> nanocode_scope::MemoryStats {
        self.witness.stats()
    }

    pub(crate) fn checkpointed(&self, thread: &str) -> bool {
        !self.witness.manifests(thread).is_empty()
    }

    pub(crate) const fn admitting(mut self, admit_ms: u64) -> Self {
        self.policy.admit_ms = admit_ms;
        self
    }

    pub(crate) fn keeping(mut self, rows: usize) -> Self {
        self.thread.recent_rows = rows;
        self.fold = ThreadFold::new(self.thread);
        self
    }

    pub(crate) fn rows(&self, thread: &str) -> Vec<(u64, String)> {
        block_on(self.reader.scan(thread, 0, SCAN_END)).expect("the model store answers every scan")
    }

    fn refold(&self, thread: &str, rows: &[(u64, String)]) -> Result<ThreadState, StoreError> {
        let (from, mut state) = match self.bases.borrow().get(thread) {
            Some((seq, base)) => (*seq, base.clone()),
            None => (0, self.fold.empty()),
        };
        for (seq, row) in rows.iter().filter(|(seq, _)| *seq >= from) {
            self.fold.apply(&mut state, *seq, row)?;
        }
        Ok(state)
    }
}

pub(crate) fn publish(world: &World, nodes: &[Node]) {
    world.net.nodes.set(
        nodes
            .iter()
            .map(|node| (node.name.clone(), node.logs.clone()))
            .collect(),
    );
}

struct Woken(AtomicBool);

impl std::task::Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn drive<T>(time: &Arc<Clock>, future: impl Future<Output = T>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    let woken = Arc::new(Woken(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&woken));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Some(value);
        }
        if woken.0.swap(false, Ordering::SeqCst) {
            continue;
        }
        if !time.advance_to_next() {
            return None;
        }
    }
}

pub(crate) fn driven<T>(world: &World, what: &str, future: impl Future<Output = T>) -> Option<T> {
    let driven = drive(&world.time, future);
    if driven.is_none() {
        flag(
            &world.wrong,
            format!("deadline: {what} waits with no timer left to fire"),
        );
    }
    driven
}

pub(crate) fn reach(
    world: &World,
    node: &Node,
    thread: &str,
) -> Option<Result<ThreadLog, ScopeError>> {
    let scopes = node.logs.scopes();
    let before = scopes.stats().usage.adopt.calls;
    let reached = driven(world, "a get", node.logs.get(thread))?;
    let read = scopes.stats().usage.adopt.calls - before;
    if read > 0 {
        flag(
            &world.wrong,
            format!(
                "I8: a get of {thread} on {} made {read} adoption reads; only a backfill adopts",
                node.name
            ),
        );
    }
    if !matches!(reached, Err(ScopeError::Unadopted)) {
        return Some(reached);
    }
    let backfilled = driven(world, "a backfill", scopes.backfill())?;
    if let Some((scope, error)) = backfilled.failed.first() {
        flag(
            &world.wrong,
            format!("the backfill of {scope} on {} failed: {error}", node.name),
        );
    }
    driven(world, "a get after its backfill", node.logs.get(thread))
}

pub(crate) fn raw(value: &serde_json::Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).expect("a JSON value encodes")
}

fn turn_of(rng: &mut Rng, turns: u64) -> String {
    format!("t{}", rng.below(turns.max(1)))
}

fn wake_command(rng: &mut Rng, state: &ThreadState) -> ThreadCommand {
    let armed: Vec<(String, u64)> = state
        .wakes()
        .map(|(key, wake)| (key.to_owned(), wake.generation))
        .collect();
    let records = if !armed.is_empty() && rng.chance(400_000) {
        let (key, generation) = armed[rng.pick(armed.len())].clone();
        vec![Record::Disarmed { generation, key }]
    } else {
        vec![Record::Armed {
            generation: state.generation() + 1,
            key: format!("w{}", rng.below(3)),
            wake: Wake {
                due_at: Some(rng.below(10_000)),
                on: None,
                payload: raw(&json!({ "wake": rng.below(100) })),
            },
        }]
    };
    ThreadCommand::Write { records }
}

pub(crate) fn command(
    rng: &mut Rng,
    turns: &mut u64,
    state: Option<&ThreadState>,
) -> ThreadCommand {
    let at = rng.below(1_000_000);
    let pick = rng.below(100);
    match pick {
        0..20 => {
            let request_id = format!("t{turns}");
            *turns += 1;
            ThreadCommand::Submit {
                submission: Submission {
                    request_id,
                    prompt: Prompt::new(format!("prompt {}", rng.below(1_000))),
                    hand: None,
                    metadata: raw(&json!({})),
                },
                at,
            }
        }
        20..34 => ThreadCommand::Start {
            request_id: turn_of(rng, *turns),
            at,
        },
        34..44 => ThreadCommand::Finish {
            request_id: turn_of(rng, *turns),
            state: if rng.chance(800_000) {
                TurnState::Completed
            } else {
                TurnState::Failed
            },
            fields: Map::new(),
            at,
        },
        44..49 => ThreadCommand::Cancel {
            request_id: turn_of(rng, *turns),
            fields: Map::new(),
            at,
        },
        49..63 => ThreadCommand::Append {
            entries: (0..1 + rng.below(3))
                .map(|at| LogEntry {
                    request_id: rng.chance(500_000).then(|| turn_of(rng, *turns)),
                    event: raw(&json!({ "type": "note", "n": at })),
                })
                .collect(),
            at,
        },
        63..75 => ThreadCommand::Write {
            records: vec![Record::Cell {
                cell: rng.below(CELLS),
                fresh: rng.chance(200_000),
                session: format!("s{}", rng.below(SESSIONS)),
                update: raw(&json!({ "update": rng.below(1_000) })),
            }],
        },
        75..78 => ThreadCommand::Write {
            records: vec![Record::Forget {
                cell: rng.below(CELLS),
                session: format!("s{}", rng.below(SESSIONS)),
            }],
        },
        78..83 => ThreadCommand::Facts {
            facts: vec![(
                Fact::Decision(Decision::Host {
                    key: format!("h{}", rng.below(3)),
                    value: raw(&json!(rng.below(1_000))),
                }),
                None,
            )],
        },
        83..87 => ThreadCommand::Write {
            records: vec![Record::Stored {
                key: format!("k{}", rng.below(4)),
                value: format!("v{}", rng.below(1_000)),
            }],
        },
        87..90 => ThreadCommand::Write {
            records: vec![Record::Checkpoint {
                payload: format!("payload {}", rng.below(1_000)),
                revision: rng.below(100),
                state: format!("state{}", rng.below(2)),
            }],
        },
        90..93 => ThreadCommand::Write {
            records: vec![Record::Hold {
                hold: Hold::Waiting {
                    call_id: format!("c{}", rng.below(1_000_000)),
                    card: raw(&json!({})),
                    input: raw(&json!({})),
                    tool: "approve".to_owned(),
                    turn: None,
                },
                key: format!("hold{}", rng.below(1_000_000)),
            }],
        },
        93..97 => match state {
            Some(state) => wake_command(rng, state),
            None => ThreadCommand::Pending,
        },
        _ => scoped(rng, *turns, state),
    }
}

fn scoped(rng: &mut Rng, turns: u64, state: Option<&ThreadState>) -> ThreadCommand {
    let owner = rng
        .chance(500_000)
        .then(|| format!("state{}", rng.below(2)));
    let open = state.and_then(|state| {
        let intents: Vec<(String, Option<String>)> = state
            .intents()
            .map(|(call_id, held, _)| (call_id.to_owned(), held.map(str::to_owned)))
            .collect();
        (!intents.is_empty()).then(|| intents[rng.pick(intents.len())].clone())
    });
    let fact = match rng.below(4) {
        0 => Fact::Decision(Decision::Time {
            unix_millis: rng.below(1_000_000),
        }),
        1 => {
            let id = rng.below(1_000_000);
            return ThreadCommand::Facts {
                facts: vec![(
                    Fact::Intent(Intent {
                        turn: rng.chance(500_000).then(|| turn_of(rng, turns)),
                        call_id: format!("i{id}"),
                        tool: "deploy".to_owned(),
                        target: IntentTarget::Nested {
                            parent: format!("p{id}"),
                            input: raw(&json!({ "intent": id })),
                        },
                    }),
                    owner,
                )],
            };
        }
        2 => match open {
            Some((call_id, held)) => {
                return ThreadCommand::Facts {
                    facts: vec![(
                        Fact::EffectResult(EffectResult {
                            call_id,
                            output: raw(&json!({ "done": true })),
                        }),
                        held,
                    )],
                };
            }
            None => Fact::Decision(Decision::Time {
                unix_millis: rng.below(1_000_000),
            }),
        },
        _ => {
            return ThreadCommand::Write {
                records: vec![Record::Recovery {
                    at: None,
                    attempt: 1 + rng.below(3),
                    turn: turn_of(rng, turns),
                    unknown: Vec::new(),
                }],
            };
        }
    };
    ThreadCommand::Facts {
        facts: vec![(fact, Some(format!("state{}", rng.below(2))))],
    }
}

fn job_landed(world: &World, thread: &str, note: u64, event: u64, fact: u64) -> bool {
    let Ok(state) = world.refold(thread, &world.rows(thread)) else {
        return false;
    };
    let event = state
        .events()
        .find(|held| held.seq == event)
        .and_then(|held| serde_json::from_str::<serde_json::Value>(held.event.get()).ok());
    let fact = state
        .facts()
        .find(|(seq, _)| *seq == fact)
        .map(|(_, kept)| &kept.fact);
    let host = matches!(
        fact,
        Some(Fact::Decision(Decision::Host { key, value }))
            if key == "job" && value.get() == note.to_string()
    );
    event.is_some_and(|event| event["n"] == note) && host
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Tally {
    inputs: u64,
    bytes: u64,
    decisions: Vec<u64>,
}

impl DurableFold for Tally {
    const FAMILY: &'static str = "sim.tally";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, _seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Input(input) => {
                self.inputs += 1;
                self.bytes += input.value.get().len() as u64;
            }
            Fact::Decision(Decision::Backend(value)) => self.decisions.push(*value),
            _ => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        Ok(Vec::new())
    }
}

fn fact_input(rng: &mut Rng) -> OwnedFact<u64> {
    let bytes = if rng.chance(FACT_BLOB_PPM) {
        VALUE_INLINE_BYTES + usize::try_from(rng.below(64)).expect("a length fits")
    } else {
        1 + usize::try_from(rng.below(32)).expect("a length fits")
    };
    Fact::Input(Input {
        turn: "tally".to_owned(),
        kind: InputKind::Prompt,
        value: raw(&json!("x".repeat(bytes))),
        metadata: raw(&json!(null)),
        hand: None,
        first: None,
    })
}

fn fact_log(
    rng: &mut Rng,
    world: &World,
    log: ThreadLog,
    thread: &str,
    stats: &mut ThreadScopeStats,
) {
    let routed = log.state().is_none();
    let facts = ThreadFacts::new(Arc::new(log));
    let scoped = facts.scoped(
        &format!("state{}", rng.below(FACT_STATES)),
        rng.chance(500_000),
    );
    stats.fact_logs += 1;
    let first = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped));
    let second = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped));
    let (Some(Ok(first)), Some(Ok(second))) = (first, second) else {
        if routed {
            stats.fact_logs_routed += 1;
        } else {
            stats.refused += 1;
        }
        return;
    };
    if routed {
        flag(
            &world.wrong,
            format!("facts: a fact log of {thread} replayed through a node that routes the thread"),
        );
        return;
    }
    let ahead: OwnedFact<u64> = Fact::Decision(Decision::Backend(rng.below(1_000)));
    let behind = fact_input(rng);
    let Some(Ok(_)) = driven(world, "a fact commit", commit_fact(&*scoped, first, ahead)) else {
        stats.refused += 1;
        return;
    };
    let committed = driven(
        world,
        "a stale fact commit",
        commit_fact(&*scoped, second, behind),
    );
    let Some(Ok((caught, seq))) = committed else {
        stats.refused += 1;
        return;
    };
    stats.fact_commits += 2;
    let Some(Ok(fresh)) = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped)) else {
        return;
    };
    if fresh.seq() != caught.seq() || fresh.state() != caught.state() {
        flag(
            &world.wrong,
            format!(
                "facts: an append on {thread} behind the log's last fact committed fact {seq} and folds {:?} at {}, but a replay folds {:?} at {}",
                caught.state(),
                caught.seq(),
                fresh.state(),
                fresh.seq()
            ),
        );
    }
}

fn answer(world: &World, thread: &str, seq: u64, body: &serde_json::Value) {
    world
        .answered
        .borrow_mut()
        .entry(thread.to_owned())
        .or_default()
        .insert(seq, body.to_string());
}

fn staged(
    rng: &mut Rng,
    world: &World,
    log: &ThreadLog,
    thread: &str,
    stats: &mut ThreadScopeStats,
) -> bool {
    let body = json!({ "type": "staged", "n": world.note() });
    let event = raw(&body);
    let at = rng.below(1_000_000);
    let ran = log.job_staged(move |staged| staged.event(at, None, event.clone()));
    stats.sends += 1;
    match driven(world, "a staged job", ran) {
        Some(Ok(seq)) => {
            stats.staged += 1;
            answer(world, thread, seq, &body);
            rng.chance(STAGED_CRASH_PPM)
        }
        Some(Err(_)) | None => {
            stats.refused += 1;
            false
        }
    }
}

fn job(rng: &mut Rng, world: &World, log: &ThreadLog, thread: &str, stats: &mut ThreadScopeStats) {
    let note = world.note();
    let at = rng.below(1_000_000);
    let routed = log.state().is_none();
    let ran = log.job(move |staged| {
        let event = staged.event(at, None, raw(&json!({ "type": "job", "n": note })))?;
        let host = Fact::Decision(Decision::Host {
            key: "job".to_owned(),
            value: raw(&json!(note)),
        });
        let fact = staged.fact(&host, None)?;
        Ok((event, fact))
    });
    stats.sends += 1;
    match driven(world, "a job", ran) {
        Some(Ok(_)) if routed => flag(
            &world.wrong,
            format!("a job on {thread} ran through a node that routes the thread to its holder"),
        ),
        Some(Ok((event, fact))) => {
            stats.committed += 1;
            stats.jobs += 1;
            answer(world, thread, event, &json!({ "type": "job", "n": note }));
            if !job_landed(world, thread, note, event, fact) {
                flag(
                    &world.wrong,
                    format!(
                        "a job on {thread} answered event {event} and fact {fact}, which its committed rows do not hold"
                    ),
                );
            }
        }
        Some(Err(ScopeError::Routed { .. })) => stats.jobs_routed += 1,
        Some(Err(_)) | None => stats.refused += 1,
    }
}

fn send(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    turns: &mut BTreeMap<String, u64>,
    stats: &mut ThreadScopeStats,
) -> Option<usize> {
    let at = rng.pick(nodes.len());
    let thread = world.threads[rng.pick(world.threads.len())].clone();
    let reached = reach(world, &nodes[at], &thread)?;
    let Ok(log) = reached else {
        stats.refused += 1;
        return None;
    };
    if log.state().is_some() {
        adopted(world, &thread, stats);
        if sessions(world, &thread, &log, stats) && rng.chance(SESSION_CHECK_PPM) {
            agrees(rng, world, log, &thread, stats);
            return None;
        }
    }
    if rng.chance(FACT_LOG_PPM) {
        fact_log(rng, world, log, &thread, stats);
        return None;
    }
    if log.state().is_some() && rng.chance(STAGED_PPM) {
        let crash = staged(rng, world, &log, &thread, stats);
        nodes[at].handles.insert(thread, log);
        return crash.then_some(at);
    }
    if rng.chance(RESUBMIT_PPM) {
        let counter = turns.get(&thread).copied().unwrap_or(0);
        resubmit(rng, world, &log, &thread, counter, stats);
        if log.state().is_some() {
            nodes[at].handles.insert(thread, log);
        }
        return None;
    }
    if rng.chance(JOB_PPM) {
        job(rng, world, &log, &thread, stats);
        if log.state().is_some() {
            nodes[at].handles.insert(thread, log);
        }
        return None;
    }
    let state = log.state();
    let counter = turns.entry(thread.clone()).or_insert(0);
    let command = command(rng, counter, state.as_deref());
    stats.sends += 1;
    match driven(world, "a send", log.send(command)) {
        Some(Ok(_)) => stats.committed += 1,
        Some(Err(_)) | None => stats.refused += 1,
    }
    if log.state().is_some() {
        nodes[at].handles.insert(thread, log);
    }
    None
}

fn admitted_in(rows: &[(u64, String)], id: &str) -> bool {
    records(rows, 0, u64::MAX)
        .iter()
        .any(|record| match record {
            Record::Admitted { turn, .. } => turn == id,
            Record::Fact {
                fact: Fact::Input(input),
                state: None,
                ..
            } => input.turn == id && matches!(input.kind, InputKind::Prompt),
            _ => false,
        })
}

fn resubmit(
    rng: &mut Rng,
    world: &World,
    log: &ThreadLog,
    thread: &str,
    turns: u64,
    stats: &mut ThreadScopeStats,
) {
    if turns == 0 {
        return;
    }
    let request_id = turn_of(rng, turns);
    let admitted = admitted_in(&world.rows(thread), &request_id);
    let submission = Submission {
        request_id: request_id.clone(),
        prompt: Prompt::new(format!("prompt {}", rng.below(1_000))),
        hand: None,
        metadata: raw(&json!({})),
    };
    stats.sends += 1;
    match driven(world, "a resubmit", log.submit(&submission)) {
        Some(Ok(Submitted::Accepted { .. })) if admitted => flag(
            &world.wrong,
            format!("{thread} admitted {request_id} again, which its committed rows already admit"),
        ),
        Some(Ok(_)) => {
            stats.committed += 1;
            stats.resubmits += 1;
        }
        Some(Err(_)) | None => stats.refused += 1,
    }
}

fn finished_turn(record: &Record) -> Option<&str> {
    match record {
        Record::Status {
            status: Status::Finished { .. },
            turn,
        } => Some(turn),
        Record::Fact {
            fact:
                Fact::Marker(Marker::Status {
                    status: TurnStatus::Finished { .. },
                    turn,
                }),
            state: None,
            ..
        } => Some(turn),
        _ => None,
    }
}

fn moved(world: &World, thread: &str, manifest: &Manifest, state: &ThreadState, from: u64) -> bool {
    let mut ends: BTreeMap<String, u64> = BTreeMap::new();
    for (seq, body) in world
        .rows(thread)
        .iter()
        .filter(|(seq, _)| *seq < manifest.seq)
    {
        for record in scope_records(body) {
            if let Some(turn) = finished_turn(&record) {
                ends.insert(turn.to_owned(), *seq);
            }
        }
    }
    let stale: Vec<(String, u64)> = state
        .turns()
        .filter(|turn| matches!(turn.phase, Phase::Finished { .. }))
        .filter(|turn| state.start_row(&turn.id).is_some())
        .filter_map(|turn| {
            let row = *ends.get(&turn.id)?;
            (row < from).then(|| (turn.id.clone(), row))
        })
        .collect();
    if stale.is_empty() {
        return true;
    }
    flag(
        &world.wrong,
        format!(
            "the checkpoint of {thread} at row {} holds finished turns {stale:?}, which ended before its window from row {from}",
            manifest.seq
        ),
    );
    false
}

fn resident(world: &World, nodes: &[Node], stats: &mut ThreadScopeStats) {
    for node in nodes {
        for (thread, log) in &node.handles {
            let (Some(state), Some(next)) = (log.state(), log.next()) else {
                continue;
            };
            let rows: Vec<(u64, String)> = world
                .rows(thread)
                .into_iter()
                .filter(|(seq, _)| *seq < next)
                .collect();
            stats.resident_checks += 1;
            match world.refold(thread, &rows) {
                Ok(refold) if refold.digest() == state.digest() => {}
                Ok(_) => flag(
                    &world.wrong,
                    format!(
                        "{} holds {thread} at row {next} with a state that differs from a refold of its committed rows",
                        node.name
                    ),
                ),
                Err(error) => flag(
                    &world.wrong,
                    format!("committed rows of {thread} do not refold: {error}"),
                ),
            }
        }
    }
}

fn decoded(world: &World, manifest: &Manifest) -> Result<ThreadState, String> {
    let digests: Vec<String> = manifest.parts.iter().map(|(_, at)| at.clone()).collect();
    let found: BTreeMap<String, String> = match block_on(world.witness.parts(&digests)) {
        Ok(found) => found.into_iter().collect(),
        Err(error) => return Err(error.to_string()),
    };
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for (name, at) in &manifest.parts {
        let Some(body) = found.get(at) else {
            return Err(format!("part {name} has no body"));
        };
        parts.push(Part {
            name: name.clone(),
            body: body.clone(),
        });
    }
    world.fold.decode(parts).map_err(|error| error.to_string())
}

fn cached_blobs(state: &ThreadState) -> BTreeSet<Digest> {
    state
        .hosts()
        .filter(|(key, _)| {
            key.strip_prefix(FOLD_SNAPSHOT_KEY)
                .is_some_and(|rest| rest.starts_with('\n'))
        })
        .filter_map(|(_, value)| serde_json::from_str::<CachedAt>(value.get()).ok())
        .map(|at| at.blob)
        .collect()
}

fn left(
    world: &World,
    thread: &str,
    manifest: &Manifest,
    state: &ThreadState,
    stats: &mut ThreadScopeStats,
) -> bool {
    let finished: Vec<&str> = state
        .turns()
        .filter(|turn| matches!(turn.phase, Phase::Finished { .. }))
        .map(|turn| turn.id.as_str())
        .collect();
    if !finished.is_empty() {
        flag(
            &world.wrong,
            format!(
                "the adopted checkpoint of {thread} at row {} holds finished turns {finished:?}, which belong to its old log",
                manifest.seq
            ),
        );
        return false;
    }
    let cached = cached_blobs(state);
    let held: Vec<u32> = (0_u32..)
        .map_while(|index| {
            state
                .blob_digest(BlobRef::from_index(index))
                .map(|digest| (index, digest))
        })
        .filter(|(_, digest)| !cached.contains(*digest) && state.blob_body(digest).is_some())
        .map(|(index, _)| index)
        .collect();
    if !held.is_empty() {
        flag(
            &world.wrong,
            format!(
                "the adopted checkpoint of {thread} at row {} holds the bodies of blobs {held:?}, which no cached fold names",
                manifest.seq
            ),
        );
        return false;
    }
    let digests: Vec<String> = manifest.parts.iter().map(|(_, at)| at.clone()).collect();
    let bytes: u64 = block_on(world.witness.parts(&digests))
        .map(|found| found.iter().map(|(_, body)| body.len() as u64).sum())
        .unwrap_or(0);
    stats.adopted_bounded += 1;
    stats.adopted_parts_max = stats.adopted_parts_max.max(manifest.parts.len() as u64);
    stats.adopted_bytes_max = stats.adopted_bytes_max.max(bytes);
    true
}

fn adopted(world: &World, thread: &str, stats: &mut ThreadScopeStats) {
    let Some((legacy, theirs)) = world.legacy.get(thread) else {
        return;
    };
    if world.bases.borrow().contains_key(thread) {
        return;
    }
    let Some(first) = world.witness.manifests(thread).first().cloned() else {
        flag(
            &world.wrong,
            format!("{thread} was claimed over its turn log history with no checkpoint after it"),
        );
        return;
    };
    if first.seq != legacy.next {
        flag(
            &world.wrong,
            format!(
                "the first checkpoint of {thread} is at row {}, not at its turn log's head {}",
                first.seq, legacy.next
            ),
        );
        return;
    }
    let state = match decoded(world, &first) {
        Ok(state) => state,
        Err(error) => {
            flag(
                &world.wrong,
                format!("the adopted checkpoint of {thread} does not decode: {error}"),
            );
            return;
        }
    };
    if !left(world, thread, &first, &state, stats) {
        return;
    }
    let paging = thread_history::Paging::new(None, usize::MAX, None);
    let events = driven(
        world,
        "an adopted history read",
        thread_history::events(&world.host, thread, &state, 0, paging),
    );
    let facts = driven(
        world,
        "an adopted fact read",
        thread_history::facts(&world.host, thread, &state, 0, None),
    );
    let turns = driven(
        world,
        "an adopted turn probe",
        threadlegacy::turns(&world.host, thread, &state, legacy),
    );
    let (Some(Ok(events)), Some(Ok(facts)), Some(Ok(turns))) = (events, facts, turns) else {
        flag(
            &world.wrong,
            format!("the adopted history of {thread} does not read back"),
        );
        return;
    };
    stats.adopted_checks += 1;
    let ours = threadlegacy::ours(&state, legacy, turns, &events.events, &facts);
    if let Some(difference) = threadlegacy::differs(theirs, &ours) {
        flag(
            &world.wrong,
            format!(
                "the adopted state of {thread} differs from its turn log's own view: {difference}"
            ),
        );
        return;
    }
    world
        .bases
        .borrow_mut()
        .insert(thread.to_owned(), (first.seq, state));
}

fn sessions(world: &World, thread: &str, log: &ThreadLog, stats: &mut ThreadScopeStats) -> bool {
    if !session_base(world, thread, stats) {
        return false;
    }
    let Some(state) = log.state() else {
        return false;
    };
    held(world, thread, &state, stats)
}

fn session_base(world: &World, thread: &str, stats: &mut ThreadScopeStats) -> bool {
    let Some(next) = world
        .sessions
        .borrow()
        .get(thread)
        .map(|sessions| sessions.next)
    else {
        return false;
    };
    if !world.bases.borrow().contains_key(thread) {
        let Some(first) = world.witness.manifests(thread).first().cloned() else {
            flag(
                &world.wrong,
                format!(
                    "{thread} was claimed over its old writer's sessions with no checkpoint after them"
                ),
            );
            return false;
        };
        if first.seq != next {
            flag(
                &world.wrong,
                format!(
                    "the first checkpoint of {thread} is at row {}, not at its old writer's head {next}",
                    first.seq
                ),
            );
            return false;
        }
        match decoded(world, &first) {
            Ok(state) => {
                if !left(world, thread, &first, &state, stats) {
                    return false;
                }
                world
                    .bases
                    .borrow_mut()
                    .insert(thread.to_owned(), (first.seq, state));
            }
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("the adopted checkpoint of {thread} does not decode: {error}"),
                );
                return false;
            }
        }
    }
    true
}

fn held(world: &World, thread: &str, state: &ThreadState, stats: &mut ThreadScopeStats) -> bool {
    let sessions = world.sessions.borrow();
    let Some(sessions) = sessions.get(thread) else {
        return false;
    };
    let checked = threadsessions::held(state, thread, sessions).and_then(|held| {
        threadsessions::kept(state, thread, &world.rows(thread)).map(|kept| (held, kept))
    });
    match checked {
        Ok((held, kept)) => {
            stats.session_blobs_checked += held + kept.blobs;
            stats.session_stored_bytes_max = stats.session_stored_bytes_max.max(kept.bytes);
            true
        }
        Err(error) => {
            flag(&world.wrong, format!("sessions: {error}"));
            false
        }
    }
}

fn agrees(
    rng: &mut Rng,
    world: &World,
    log: ThreadLog,
    thread: &str,
    stats: &mut ThreadScopeStats,
) {
    let Some(mut sessions) = world.sessions.borrow_mut().remove(thread) else {
        return;
    };
    let checked = driven(
        world,
        "a session fold",
        threadsessions::agrees(rng, &ThreadFacts::new(Arc::new(log)), thread, &mut sessions),
    );
    world
        .sessions
        .borrow_mut()
        .insert(thread.to_owned(), sessions);
    match checked {
        Some(Ok(checked)) => stats.session_checks += checked,
        Some(Err(error)) => flag(&world.wrong, format!("sessions: {error}")),
        None => {}
    }
}

fn records(rows: &[(u64, String)], from: u64, to: u64) -> Vec<Record> {
    rows.iter()
        .filter(|(seq, _)| (from..to).contains(seq))
        .flat_map(|(_, body)| scope_records(body))
        .collect()
}

fn scope_records(body: &str) -> Vec<Record> {
    match nanocode_turnlog::standalone_plain(body) {
        Ok(Some(plain)) => nanocode_turnlog::decode(&plain).unwrap_or_default(),
        _ => nanocode_turnlog::decode(body).unwrap_or_default(),
    }
}

fn bounded(
    world: &World,
    thread: &str,
    manifest: &Manifest,
    state: &ThreadState,
    stats: &mut ThreadScopeStats,
) {
    let rows = world.rows(thread);
    let recent = world.thread.recent_rows as u64;
    let floor = state.legacy().map_or(0, |legacy| legacy.next);
    let from = manifest.seq.saturating_sub(recent).max(floor);
    let window = records(&rows, from, manifest.seq);
    let events: Vec<u64> = window
        .iter()
        .filter_map(|record| match record {
            Record::Event { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    let facts: Vec<u64> = window
        .iter()
        .filter_map(|record| match record {
            Record::Fact { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    let held: Vec<u64> = state.events().map(|event| event.seq).collect();
    let kept: Vec<u64> = state.facts().map(|(seq, _)| seq).collect();
    stats.window_checks += 1;
    stats.moved_max = stats
        .moved_max
        .max(state.moved().map_or(0, |moved| moved.turns));
    if !moved(world, thread, manifest, state, from) {
        return;
    }
    if held != events || kept != facts {
        flag(
            &world.wrong,
            format!(
                "the checkpoint of {thread} at row {} holds events {held:?} and facts {kept:?}, but its last {recent} rows hold events {events:?} and facts {facts:?}",
                manifest.seq
            ),
        );
        return;
    }
    let tail = rows.iter().filter(|(seq, _)| *seq >= manifest.seq);
    let (tail_rows, tail_bytes) = tail.fold((0_u64, 0_u64), |(count, bytes), (_, body)| {
        (count + 1, bytes + body.len() as u64)
    });
    if tail_rows > world.policy.checkpoint_rows as u64 {
        flag(
            &world.wrong,
            format!(
                "a cold claim of {thread} reads {tail_rows} rows past its checkpoint at row {}, more than checkpoint_rows {}",
                manifest.seq, world.policy.checkpoint_rows
            ),
        );
        return;
    }
    let digests: Vec<String> = manifest.parts.iter().map(|(_, at)| at.clone()).collect();
    let part_bytes: u64 = block_on(world.witness.parts(&digests))
        .map(|found| found.iter().map(|(_, body)| body.len() as u64).sum())
        .unwrap_or(0);
    let history: u64 = rows.iter().map(|(_, body)| body.len() as u64).sum();
    stats.claim_bytes_max = stats.claim_bytes_max.max(part_bytes + tail_bytes);
    stats.claim_rows_max = stats.claim_rows_max.max(tail_rows);
    stats.history_bytes_max = stats.history_bytes_max.max(history);
}

fn renumbered(world: &World, thread: &str) {
    let answered = world.answered.borrow();
    let Some(answered) = answered.get(thread) else {
        return;
    };
    let rows = world.rows(thread);
    for record in records(&rows, 0, u64::MAX) {
        let Record::Event { event, seq, .. } = record else {
            continue;
        };
        if let Some(body) = answered.get(&seq)
            && body != event.get()
        {
            flag(
                &world.wrong,
                format!(
                    "event {seq} of {thread} was answered to its writer as {body}, but the thread committed {} there",
                    event.get()
                ),
            );
            return;
        }
    }
}

fn cold(
    world: &World,
    thread: &str,
    stats: &mut ThreadScopeStats,
) -> Option<Result<ThreadState, String>> {
    let manifest = world.witness.manifests(thread).last().cloned()?;
    let mut state = match decoded(world, &manifest) {
        Ok(state) => state,
        Err(error) => return Some(Err(error)),
    };
    bounded(world, thread, &manifest, &state, stats);
    for (seq, row) in world
        .rows(thread)
        .iter()
        .filter(|(seq, _)| *seq >= manifest.seq)
    {
        if let Err(error) = world.fold.apply(&mut state, *seq, row) {
            return Some(Err(error.to_string()));
        }
    }
    Some(Ok(state))
}

fn colds(world: &World, stats: &mut ThreadScopeStats) {
    for thread in &world.threads {
        if !world.witness.manifests(thread).is_empty() {
            adopted(world, thread, stats);
            session_base(world, thread, stats);
        }
        renumbered(world, thread);
        let Some(loaded) = cold(world, thread, stats) else {
            continue;
        };
        stats.cold_checks += 1;
        if let Ok(loaded) = &loaded {
            held(world, thread, loaded, stats);
        }
        let refold = world.refold(thread, &world.rows(thread));
        match (loaded, refold) {
            (Ok(loaded), Ok(refold)) if loaded.digest() == refold.digest() => {}
            (Ok(_), Ok(_)) => flag(
                &world.wrong,
                format!(
                    "a cold claim of {thread} from its checkpoint and tail differs from a refold of its rows"
                ),
            ),
            (Err(error), _) => flag(
                &world.wrong,
                format!("a cold claim of {thread} from its checkpoint fails: {error}"),
            ),
            (_, Err(error)) => flag(
                &world.wrong,
                format!("committed rows of {thread} do not refold: {error}"),
            ),
        }
    }
}

fn phase(phase: &Phase) -> &'static str {
    match phase {
        Phase::Queued { .. } => "queued",
        Phase::Running {
            cancelling: true, ..
        } => "cancelling",
        Phase::Running { .. } => "running",
        Phase::Finished { .. } => "finished",
    }
}

fn legacy(world: &World, stats: &mut ThreadScopeStats) {
    for thread in &world.threads {
        if world.legacy.contains_key(thread) || world.sessions.borrow().contains_key(thread) {
            continue;
        }
        let rows = world.rows(thread);
        let decoded = match nanocode_turnlog::rows(&rows) {
            Ok(decoded) => decoded,
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("the turn log reader refuses the rows of {thread}: {error}"),
                );
                return;
            }
        };
        let mut fold = Fold::default();
        for row in &decoded {
            if let Err(error) = fold.load(row.seq + 1, &row.plain) {
                flag(
                    &world.wrong,
                    format!(
                        "the turn log fold refuses row {} of {thread}: {error}",
                        row.seq
                    ),
                );
                return;
            }
        }
        let Ok(ours) = world.refold(thread, &rows) else {
            return;
        };
        stats.legacy_checks += 1;
        let theirs: Vec<(String, &str, String, u64)> = fold
            .turns()
            .iter()
            .map(|turn| {
                (
                    turn.id.clone(),
                    phase(&turn.phase),
                    turn.digest.clone(),
                    turn.first,
                )
            })
            .collect();
        let mut mine: Vec<(String, &str, String, u64)> = Vec::new();
        for turn in fold.turns() {
            let held = match ours.found(&turn.id, &Probes::default()) {
                Found::Turn(held) => Some(held),
                Found::Absent => None,
                Found::Probe(probing) => driven(
                    world,
                    "a history probe",
                    thread_history::probe(&world.host, thread, &probing),
                )
                .and_then(Result::ok)
                .and_then(|probe| probe.turn),
            };
            if let Some(held) = held {
                mine.push((
                    held.id.clone(),
                    phase(&held.phase),
                    held.digest.clone(),
                    held.first,
                ));
            }
        }
        let unknown: Vec<&str> = ours
            .turns()
            .map(|turn| turn.id.as_str())
            .filter(|id| !theirs.iter().any(|(held, _, _, _)| held == id))
            .collect();
        let mut attempts = recovered(&fold);
        attempts.2.retain(|(id, _)| ours.turn(id).is_some());
        let first = ours.events().next().map_or(u64::MAX, |event| event.seq);
        let recent: Vec<u64> = fold
            .events()
            .iter()
            .map(|event| event.seq)
            .filter(|seq| *seq >= first)
            .collect();
        let same = theirs == mine
            && unknown.is_empty()
            && fold.last_event() <= ours.last_event()
            && recent == ours.events().map(|event| event.seq).collect::<Vec<_>>()
            && fold
                .cells()
                .map(|(session, cell, updates)| (session.to_owned(), cell, updates.len()))
                .collect::<Vec<_>>()
                == ours
                    .cells()
                    .map(|(session, cell, updates)| (session.to_owned(), cell, updates.len()))
                    .collect::<Vec<_>>()
            && fold.holds().count() == ours.holds().count()
            && fold.next_fact() == ours.next_fact()
            && fold.open_calls().count() == ours.open_calls().count()
            && attempts == recovered_ours(&ours);
        if !same {
            flag(
                &world.wrong,
                format!(
                    "the resident fold of {thread} disagrees with the turn log fold over the same rows: theirs {theirs:?}, ours {mine:?}"
                ),
            );
        }
    }
}

type Recovered = (
    Vec<String>,
    Vec<(String, Option<String>)>,
    Vec<(String, u64)>,
    bool,
);

fn recovered(fold: &Fold) -> Recovered {
    (
        fold.unknown(),
        fold.intents()
            .map(|(call_id, state, _)| (call_id.to_owned(), state.map(str::to_owned)))
            .collect(),
        fold.turns()
            .iter()
            .map(|turn| (turn.id.clone(), fold.next_attempt(&turn.id)))
            .collect(),
        fold.recovering(),
    )
}

fn recovered_ours(state: &ThreadState) -> Recovered {
    let mut turns: Vec<(String, u64)> = state
        .turns()
        .map(|turn| (turn.id.clone(), state.next_attempt(&turn.id)))
        .collect();
    turns.sort_by_key(|(id, _)| state.turn(id).map_or(0, |turn| turn.first));
    (
        state.unknown(),
        state
            .intents()
            .map(|(call_id, held, _)| (call_id.to_owned(), held.map(str::to_owned)))
            .collect(),
        turns,
        state.recovering(),
    )
}

fn crash(
    world: &World,
    nodes: &mut Vec<Node>,
    at: usize,
    generation: &mut u64,
    stats: &mut ThreadScopeStats,
) {
    let gone = nodes.remove(at);
    let runtime = gone.logs.scopes().stats();
    let (adoptions, rows) = world.gone.get();
    world
        .gone
        .set((adoptions + runtime.adoptions, rows + runtime.adoption_rows));
    stats.crashes += 1;
    *generation += 1;
    nodes.push(world.node(&format!("n{at}g{generation}")));
    drop(gone);
    publish(world, nodes);
}

fn step(
    rng: &mut Rng,
    world: &World,
    nodes: &mut Vec<Node>,
    generation: &mut u64,
    turns: &mut BTreeMap<String, u64>,
    stats: &mut ThreadScopeStats,
) {
    stats.steps += 1;
    world
        .db
        .fetch_add(rng.below(TICK_MS_MAX), Ordering::Relaxed);
    if rng.chance(CRASH_PPM) && nodes.len() > 1 {
        let at = rng.pick(nodes.len());
        crash(world, nodes, at, generation, stats);
    } else if rng.chance(RELEASE_PPM) {
        let at = rng.pick(nodes.len());
        let part = nanocode_partitions::Partition(
            u32::try_from(rng.below(u64::from(PARTS))).expect("a partition fits"),
        );
        if driven(world, "a release", nodes[at].logs.scopes().release(part)).is_some() {
            stats.releases += 1;
        }
    } else if rng.chance(TICK_PPM) {
        let at = rng.pick(nodes.len());
        driven(world, "a tick", nodes[at].logs.scopes().tick());
    } else if let Some(at) = send(rng, world, nodes, turns, stats) {
        stats.staged_crashes += 1;
        crash(world, nodes, at, generation, stats);
    }
    for node in nodes.iter() {
        driven(world, "a renewal", node.logs.scopes().renew());
    }
    resident(world, nodes, stats);
    colds(world, stats);
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: ThreadScopeStats,
    shape: String,
}

fn run_seed(seed: u64, mutation: ThreadScopeMutation) -> Outcome {
    let time = Clock::new();
    crate::host::entered(seed, &time, || seeded(seed, mutation, &time))
}

pub(crate) fn world(rng: &mut Rng, time: &Arc<Clock>) -> World {
    let db = Arc::new(AtomicU64::new(START_MS));
    let clock = Arc::clone(&db);
    let store =
        Arc::new(ModelParts::new().with_clock(Arc::new(move || clock.load(Ordering::Relaxed))));
    let count = NonZeroU32::new(PARTS).expect("partitions are counted from one");
    block_on(store.migrate(count)).expect("the model store migrates");
    let policy = Policy {
        checkpoint_rows: usize::try_from(rng.range(CHECKPOINT_ROWS.start, CHECKPOINT_ROWS.end))
            .expect("a row bound fits"),
        checkpoint_ms: CHECKPOINT_MS,
        resident_bytes_max: usize::try_from(rng.range(RESIDENT_BYTES.start, RESIDENT_BYTES.end))
            .expect("a byte bound fits"),
        forward_ms: FORWARD_MS,
        admit_ms: ADMIT_MS,
    };
    let rows = policy.checkpoint_rows as u64;
    let thread = ThreadPolicy {
        part_rows: usize::try_from(rng.range(PART_ROWS.start, PART_ROWS.end))
            .expect("a part bound fits"),
        finished_max: None,
        row_bytes_max: None,
        recent_rows: usize::try_from(rng.range(rows, 2 * rows + 1)).expect("a row bound fits"),
        events_ahead: rng.range(EVENTS_AHEAD.start, EVENTS_AHEAD.end),
        finished_filter: FinishedFilter {
            bytes: usize::try_from(rng.range(FILTER_BYTES.start, FILTER_BYTES.end))
                .expect("a filter bound fits"),
            hashes: u32::try_from(rng.range(FILTER_HASHES.start, FILTER_HASHES.end))
                .expect("a hash count fits"),
        },
    };
    World {
        time: Arc::clone(time),
        db,
        store: Arc::clone(&store),
        witness: Arc::new(MemoryCheckpoints::new(Arc::clone(&store))),
        net: Arc::new(Net::default()),
        reader: PartedLog::new(Arc::clone(&store), count),
        host: Arc::new(PartedLog::new(Arc::clone(&store), count)),
        fold: ThreadFold::new(thread),
        thread,
        policy,
        count,
        threads: (0..rng.range(THREADS.start, THREADS.end))
            .map(|at| format!("thread-{at}"))
            .collect(),
        legacy: BTreeMap::new(),
        sessions: RefCell::default(),
        bases: RefCell::default(),
        answered: RefCell::default(),
        notes: Cell::new(0),
        gone: Cell::new((0, 0)),
        wrong: Arc::default(),
    }
}

fn session_history(
    rng: &mut Rng,
    world: &World,
    host: &Arc<dyn LogHost>,
    thread: &str,
    stats: &mut ThreadScopeStats,
) {
    let written = driven(
        world,
        "an old writer's sessions",
        threadsessions::write(rng, Arc::clone(host), thread),
    );
    match written {
        Some(Ok(sessions)) => {
            stats.session_threads += 1;
            stats.session_turns += sessions.turns;
            stats.session_turns_max = stats.session_turns_max.max(sessions.turns);
            stats.session_bytes_max = stats.session_bytes_max.max(sessions.bytes);
            world
                .sessions
                .borrow_mut()
                .insert(thread.to_owned(), sessions);
        }
        Some(Err(error)) => flag(
            &world.wrong,
            format!("the old writer's sessions of {thread}: {error}"),
        ),
        None => {}
    }
}

fn history(rng: &mut Rng, world: &mut World, stats: &mut ThreadScopeStats) {
    let host: Arc<dyn LogHost> = Arc::new(PartedLog::new(Arc::clone(&world.store), world.count));
    for thread in world.threads.clone() {
        if !rng.chance(LEGACY_PPM) {
            if rng.chance(SESSIONS_PPM) {
                session_history(rng, world, &host, &thread, stats);
            }
            continue;
        }
        let written = driven(
            world,
            "a legacy history",
            threadlegacy::write(rng, Arc::clone(&host), &thread),
        );
        let legacy = match written {
            Some(Ok(legacy)) => legacy,
            Some(Err(error)) => {
                flag(
                    &world.wrong,
                    format!("the legacy history of {thread}: {error}"),
                );
                return;
            }
            None => return,
        };
        let next = world.rows(&thread).last().map_or(0, |(seq, _)| seq + 1);
        let legacy = legacy.at(next);
        let theirs = match driven(
            world,
            "a legacy view",
            threadlegacy::expected(Arc::clone(&host), &thread, &legacy),
        ) {
            Some(Ok(theirs)) => theirs,
            Some(Err(error)) => {
                flag(
                    &world.wrong,
                    format!("the legacy view of {thread}: {error}"),
                );
                return;
            }
            None => return,
        };
        stats.legacy_threads += 1;
        stats.legacy_turns += legacy.turns() as u64;
        stats.legacy_retired += theirs.retired;
        stats.legacy_blobs += theirs.blobs;
        stats.legacy_paged_rows += world
            .rows(&thread)
            .iter()
            .filter(|(_, body)| !body.starts_with('{'))
            .count() as u64;
        world.legacy.insert(thread, (legacy, theirs));
    }
}

fn seeded(seed: u64, mutation: ThreadScopeMutation, time: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "thread_scope");
    let mut stats = ThreadScopeStats {
        seeds: 1,
        ..ThreadScopeStats::default()
    };
    let mut world = world(&mut rng, time);
    history(&mut rng, &mut world, &mut stats);
    let mut nodes: Vec<Node> = (0..rng.range(NODES.start, NODES.end))
        .map(|at| world.node(&format!("n{at}g0")))
        .collect();
    publish(&world, &nodes);
    let steps = rng.range(STEPS.start, STEPS.end);
    let shape = format!(
        "nodes={} threads={} checkpoint_rows={} part_rows={} resident_bytes_max={} steps={steps}",
        nodes.len(),
        world.threads.len(),
        world.policy.checkpoint_rows,
        world.thread.part_rows,
        world.policy.resident_bytes_max
    );
    thread_scope_sim::mutate(mutation.0);
    scope_sim::mutate(mutation.1);
    nanocode_turnlog::sim::mutate(mutation.2);
    nanocode::claude::sim::mutate(mutation.3);
    let mut generation = 0;
    let mut turns = BTreeMap::new();
    for _ in 0..steps {
        step(
            &mut rng,
            &world,
            &mut nodes,
            &mut generation,
            &mut turns,
            &mut stats,
        );
        if world.wrong.read(Option::is_some) {
            break;
        }
    }
    legacy(&world, &mut stats);
    if world.wrong.read(Option::is_none) {
        let turns = self::world(&mut rng, time);
        crate::threadturns::run(&mut rng, &turns, &mut stats);
        turns.net.nodes.set(Vec::new());
        if let Some(wrong) = turns.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let holds = self::world(&mut rng, time);
        crate::threadholds::run(&mut rng, &holds, &mut stats);
        holds.net.nodes.set(Vec::new());
        if let Some(wrong) = holds.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let wakes = self::world(&mut rng, time);
        crate::threadwakes::run(&mut rng, &wakes, &mut stats);
        wakes.net.nodes.set(Vec::new());
        if let Some(wrong) = wakes.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let store = self::world(&mut rng, time);
        crate::threadstore::run(&mut rng, &store, &mut stats);
        store.net.nodes.set(Vec::new());
        if let Some(wrong) = store.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let journals = self::world(&mut rng, time);
        crate::threadjournals::run(&mut rng, &journals, &mut stats);
        journals.net.nodes.set(Vec::new());
        if let Some(wrong) = journals.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let over = self::world(&mut rng, time);
        crate::threadover::run(&mut rng, &over, &mut stats);
        over.net.nodes.set(Vec::new());
        if let Some(wrong) = over.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let imports = self::world(&mut rng, time);
        crate::threadimport::run(&mut rng, &imports, &mut stats);
        imports.net.nodes.set(Vec::new());
        if let Some(wrong) = imports.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let stamps = self::world(&mut rng, time);
        crate::threadstamps::run(&mut rng, &stamps, &mut stats);
        stamps.net.nodes.set(Vec::new());
        if let Some(wrong) = stamps.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let compat = self::world(&mut rng, time).keeping(COMPAT_RECENT_ROWS);
        crate::threadcompat::run(&mut rng, &compat, &mut stats);
        compat.net.nodes.set(Vec::new());
        if let Some(wrong) = compat.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let tail = self::world(&mut rng, time);
        crate::threadtail::run(&mut rng, &tail);
        if let Some(wrong) = tail.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let warm = self::world(&mut rng, time);
        crate::threadwarm::run(&mut rng, &warm);
        if let Some(wrong) = warm.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let opens = self::world(&mut rng, time);
        crate::threadopens::run(&mut rng, &opens);
        opens.net.nodes.set(Vec::new());
        if let Some(wrong) = opens.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    thread_scope_sim::mutate(Mutation::None);
    scope_sim::mutate(ScopeMutation::None);
    nanocode_turnlog::sim::mutate(TurnLogMutation::None);
    nanocode::claude::sim::mutate(ClaudeMutation::None);
    let (adoptions, adoption_rows) = world.gone.get();
    stats.adoptions += adoptions;
    stats.adoption_rows += adoption_rows;
    for node in &nodes {
        let runtime = node.logs.scopes().stats();
        stats.adoptions += runtime.adoptions;
        stats.adoption_rows += runtime.adoption_rows;
        stats.adoption_bytes += runtime.adoption_bytes;
        stats.loads += runtime.loads;
        stats.load_rows_max = stats.load_rows_max.max(runtime.load_rows_max);
        stats.checkpoints += runtime.checkpoints;
        stats.checkpoint_parts += runtime.checkpoint_parts;
        stats.checkpoint_parts_kept += runtime.checkpoint_parts_kept;
        stats.evictions += runtime.evictions;
        stats.forwards += runtime.forwards;
    }
    for (thread, sessions) in world.sessions.borrow().iter() {
        let rows = world.rows(thread);
        stats.session_rows_bytes += rows
            .iter()
            .filter(|(seq, _)| *seq >= sessions.next)
            .map(|(_, body)| body.len() as u64)
            .sum::<u64>();
        match threadsessions::cached(&records(&rows, sessions.next, u64::MAX), thread, sessions) {
            Ok(checked) => stats.session_caches += checked,
            Err(error) => flag(&world.wrong, format!("sessions: {error}")),
        }
    }
    let histories = world.legacy.len() + world.sessions.borrow().len();
    if stats.adoptions > histories as u64 {
        flag(
            &world.wrong,
            format!(
                "{histories} turn log histories were adopted {} times; a claim after the first read the history again",
                stats.adoptions
            ),
        );
    }
    world.net.nodes.set(Vec::new());
    let mut digest = fnv(FNV_OFFSET, &seed.to_le_bytes());
    for thread in &world.threads {
        for (seq, row) in world.rows(thread) {
            digest = fnv(fnv(digest, &seq.to_le_bytes()), row.as_bytes());
        }
        for manifest in world.witness.manifests(thread) {
            digest = fnv(
                fnv(digest, &manifest.seq.to_le_bytes()),
                &manifest.epoch.to_le_bytes(),
            );
        }
    }
    Outcome {
        digest,
        wrong: world.wrong.read(Clone::clone),
        stats,
        shape,
    }
}

pub fn judge(
    seed: u64,
    mutation: ThreadScopeMutation,
    twice: bool,
    verbose: bool,
    totals: &mut ThreadScopeStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
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
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: ThreadScopeMutation) -> String {
    let mut command = format!("nanocode-sim --scenario thread_scope --seed {seed}");
    if mutation != ThreadScopeMutation::NONE {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: ThreadScopeMutation,
    totals: &ThreadScopeStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=thread_scope mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        ThreadScopeMutation::NONE if failed == 0 => ExitCode::SUCCESS,
        ThreadScopeMutation::NONE => ExitCode::FAILURE,
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

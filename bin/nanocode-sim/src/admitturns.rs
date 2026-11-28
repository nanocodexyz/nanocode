use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted},
    turnlog::{OneLog, OneLogOver, Policy, ThreadLogs, ThreadPath, ThreadPaths},
};
use nanocode_agent_core::ExecutionPolicyDisposition;
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::{Atom, Slot};
use nanocode_turnlog::{Compaction, Extension, LogHost, Rider, ScopeRows, StoredCheckpoint, Write};
use serde_json::json;

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
    threadscope::{self, World, driven, flag, publish, raw},
};

const THREADS: Range<u64> = 16..96;
const CAPACITY: Range<u64> = 1..5;
pub(crate) const READ_MS: u64 = 5;
const WAVES_PER_ADMISSION: u64 = 4;
const READS_PER_OPEN_MAX: u64 = 8;
const DEADLINE_MARGIN: u64 = 2;
const NANOS_PER_MS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct TurnStats {
    pub turns: u64,
    pub refusals: u64,
    pub healed: u64,
    pub late: u64,
}

impl TurnStats {
    pub const fn add(&mut self, other: &Self) {
        self.turns += other.turns;
        self.refusals += other.refusals;
        self.healed += other.healed;
        self.late += other.late;
    }
}

pub(crate) struct Slow {
    log: Arc<dyn LogHost>,
    capacity: u32,
    slow: AtomicBool,
    stale: Slot<ScopeRows>,
}

impl Slow {
    pub(crate) fn new(log: Arc<dyn LogHost>, capacity: u32) -> Self {
        Self {
            log,
            capacity,
            slow: AtomicBool::new(false),
            stale: Slot::new(None),
        }
    }

    pub(crate) fn stale_load(&self, rows: ScopeRows) {
        assert!(self.stale.put(rows).is_none(), "one stale load waits at a time");
        assert!(!self.stale.is_empty(), "a stale load waits for the next load");
    }

    pub(crate) fn paced(&self, slow: bool) {
        self.slow.store(slow, Ordering::Relaxed);
    }

    async fn read<T>(&self, call: StoreFuture<'_, Result<T, StoreError>>) -> Result<T, StoreError> {
        if self.slow.load(Ordering::Relaxed) {
            nanocode_platform::sleep(Duration::from_millis(READ_MS)).await;
        }
        call.await
    }
}

impl LogHost for Slow {
    fn call_bound(&self) -> Option<Duration> {
        self.log.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.capacity
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
        Box::pin(self.read(self.log.scans(scans)))
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.read(self.log.scan_ranges(scope, ranges)))
    }

    fn load<'a>(&'a self, scope: &'a str) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        if let Some(rows) = self.stale.take() {
            return Box::pin(std::future::ready(Ok(rows)));
        }
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
        let call = self.log.checkpoint(scope)?;
        Some(Box::pin(self.read(call)))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.log.save_checkpoint(scope, checkpoint)
    }
}

const ANCHOR: &str = "resident-anchor";

struct Legacy {
    anchored: bool,
}

impl ThreadPaths for Legacy {
    fn path(&self, thread: &str) -> ThreadPath {
        if self.anchored && thread == ANCHOR {
            ThreadPath::Resident
        } else {
            ThreadPath::Legacy
        }
    }
}

struct Pod {
    logs: Arc<ThreadLogs>,
    slow: Arc<Slow>,
}

fn pod(world: &World, name: &str, capacity: u32, anchored: bool) -> (threadscope::Node, Pod) {
    let mut kept = None;
    let node = world.node_wrapped(name, |log| {
        let slow = Arc::new(Slow::new(log, capacity));
        kept = Some(Arc::clone(&slow));
        (slow as Arc<dyn LogHost>, None)
    });
    let slow = kept.expect("the node wraps its host once");
    let logs = Arc::new(node.logs.clone().with_paths(Arc::new(Legacy { anchored })));
    (node, Pod { logs, slow })
}

#[derive(Clone)]
enum Ran {
    Accepted { refusals: u64 },
    Late { refusals: u64 },
    Wrong(String),
}

#[derive(Clone, Copy)]
struct Bounds {
    capacity: u32,
    admit_ms: u64,
    deadline_ns: u64,
}

fn submission(thread: &str, turn: &str) -> Submission {
    Submission {
        request_id: turn.to_owned(),
        prompt: Prompt::new(format!("turn {turn} of {thread}")),
        hand: None,
        metadata: raw(&json!(null)),
    }
}

async fn submitted(logs: &Arc<ThreadLogs>, thread: &str, turn: &str) -> Result<(), StoreError> {
    let over = OneLog::over(Arc::clone(logs), thread, &Policy::default()).await?;
    match over.submit(&submission(thread, turn)).await? {
        Submitted::Accepted { .. } | Submitted::Existing(_) => Ok(()),
        answer => Err(StoreError::Backend(format!(
            "the turn submits as {answer:?}"
        ))),
    }
}

fn misnamed(error: &StoreError, bounds: Bounds) -> Option<String> {
    let StoreError::Unadmitted { capacity, admit_ms } = error else {
        return Some(format!(
            "is refused as {error:?}, not as an admission refusal"
        ));
    };
    if *capacity != bounds.capacity || *admit_ms != bounds.admit_ms {
        return Some(format!(
            "is refused as {error:?}, not naming capacity {} and admit_ms {}",
            bounds.capacity, bounds.admit_ms
        ));
    }
    let said = error.to_string();
    let waited = said.contains(&format!("{admit_ms} ms"));
    let bound = said.contains(&format!("{capacity} read admissions"));
    (!waited || !bound).then(|| format!("hears an admission refusal as \"{said}\""))
}

async fn turn(logs: Arc<ThreadLogs>, clock: &Clock, thread: &str, bounds: Bounds) -> Ran {
    let started = clock.now_ns();
    let mut refusals = 0;
    loop {
        let Err(error) = submitted(&logs, thread, "burst").await else {
            return Ran::Accepted { refusals };
        };
        if let Some(wrong) = misnamed(&error, bounds) {
            return Ran::Wrong(format!("I17: the turn of {thread} {wrong}"));
        }
        refusals += 1;
        let disposition =
            nanocode_durability::agent_store_error(error.clone()).execution_policy_disposition();
        let waited_ns = clock.now_ns() - started;
        if disposition != Some(ExecutionPolicyDisposition::Reopen) {
            let deadline_ns = bounds.deadline_ns;
            return Ran::Wrong(format!(
                "I17: the turn of {thread} fails as {disposition:?} on \"{error}\" \
                 after {waited_ns} ns, within its deadline of {deadline_ns} ns"
            ));
        }
        if waited_ns >= bounds.deadline_ns {
            return Ran::Late { refusals };
        }
    }
}

fn seeded_history(world: &World, pod: &Pod, threads: &[String]) -> bool {
    for thread in threads {
        let first = submitted(&pod.logs, thread, "first");
        match driven(world, "an old writer's first turn", first) {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                flag(&world.wrong, format!("the first turn of {thread}: {error}"));
                return false;
            }
            None => return false,
        }
    }
    true
}

struct Plan<'a> {
    clock: &'a Arc<Clock>,
    threads: &'a [String],
    bounds: Bounds,
}

fn burst(world: &World, rng: &mut Rng, plan: &Plan<'_>, pod: &Pod) -> Option<Vec<Ran>> {
    let answers: Atom<Vec<Ran>> = Atom::default();
    let tasks = plan
        .threads
        .iter()
        .map(|thread| {
            let (answers, logs) = (&answers, Arc::clone(&pod.logs));
            Box::pin(async move {
                let ran = turn(logs, plan.clock, thread, plan.bounds).await;
                answers.update(|answers| answers.push(ran.clone()));
            }) as Task<'_>
        })
        .collect();
    let scheduler = Rng::derive(rng.below(u64::MAX), "pod start");
    match Executor::new(Arc::clone(plan.clock), scheduler).run(tasks, None) {
        Ended::Finished => Some(answers.read(Clone::clone)),
        ended => {
            flag(
                &world.wrong,
                format!("a pod start ended {ended:?} with turns still waiting"),
            );
            None
        }
    }
}

fn judged(world: &World, pod: &Pod, answers: &[Ran], stats: &mut TurnStats) {
    stats.turns += answers.len() as u64;
    let mut refusals = 0;
    for answer in answers {
        match answer {
            Ran::Accepted { refusals: heard } => {
                refusals += heard;
                stats.healed += u64::from(*heard > 0);
            }
            Ran::Late { refusals: heard } => {
                refusals += heard;
                stats.late += 1;
            }
            Ran::Wrong(wrong) => flag(&world.wrong, wrong.clone()),
        }
    }
    stats.refusals += refusals;
    let busy = pod.logs.scopes().stats().admissions_busy;
    if busy != refusals {
        flag(
            &world.wrong,
            format!("I17: the admission refused {busy} reads where the turns heard {refusals}"),
        );
    }
    if refusals == 0 {
        flag(
            &world.wrong,
            "I17: a pod start far above the read capacity saw no admission refusal".to_owned(),
        );
    }
}

fn idle(world: &World, pod: &Pod, answers: &[Ran], stats: &mut TurnStats) {
    stats.turns += answers.len() as u64;
    for answer in answers {
        match answer {
            Ran::Accepted { refusals: 0 } => {}
            Ran::Accepted { refusals } | Ran::Late { refusals } => flag(
                &world.wrong,
                format!(
                    "I17: a process with no resident scope refused a legacy open {refusals} times"
                ),
            ),
            Ran::Wrong(wrong) => flag(&world.wrong, wrong.clone()),
        }
    }
    let admitted = pod.logs.scopes().stats().admissions;
    if admitted > 0 {
        flag(
            &world.wrong,
            format!("I17: a process with no resident scope admitted {admitted} legacy reads"),
        );
    }
}

fn anchored(world: &World, pod: &Pod) -> Option<Arc<OneLogOver>> {
    let policy = Policy::default();
    let opened = OneLog::over(Arc::clone(&pod.logs), ANCHOR, &policy);
    match driven(world, "the resident anchor", opened)? {
        Ok(over) if pod.logs.scopes().stats().residents > 0 => Some(over),
        Ok(_) => {
            flag(
                &world.wrong,
                "the resident anchor holds no resident scope".to_owned(),
            );
            None
        }
        Err(error) => {
            flag(&world.wrong, format!("the resident anchor opens: {error}"));
            None
        }
    }
}

fn restarted(
    world: &World,
    rng: &mut Rng,
    plan: &Plan<'_>,
    name: &str,
    anchor: bool,
) -> Option<(Pod, Vec<Ran>)> {
    world.expire();
    let (node, pod) = pod(world, name, plan.bounds.capacity, anchor);
    publish(world, std::slice::from_ref(&node));
    let anchored = if anchor {
        Some(anchored(world, &pod)?)
    } else {
        None
    };
    pod.slow.paced(true);
    let answers = burst(world, rng, plan, &pod)?;
    drop(anchored);
    Some((pod, answers))
}

pub fn run(rng: &mut Rng, clock: &Arc<Clock>, stats: &mut TurnStats) -> Option<String> {
    let capacity = rng.range(CAPACITY.start, CAPACITY.end);
    let count = rng.range(THREADS.start, THREADS.end);
    let waves = count.div_ceil(capacity);
    let admit_ms = READ_MS * rng.range(1, (waves / WAVES_PER_ADMISSION).max(1) + 1);
    let deadline_ms = DEADLINE_MARGIN * waves * READS_PER_OPEN_MAX * READ_MS + admit_ms;
    let world = threadscope::world(rng, clock).admitting(admit_ms);
    let threads: Vec<String> = (0..count).map(|at| format!("resumed-{at}")).collect();
    let plan = Plan {
        clock,
        threads: &threads,
        bounds: Bounds {
            capacity: u32::try_from(capacity).expect("a capacity fits"),
            admit_ms,
            deadline_ns: deadline_ms * NANOS_PER_MS,
        },
    };
    let (writer, before) = pod(&world, "pod-before", plan.bounds.capacity, false);
    publish(&world, std::slice::from_ref(&writer));
    if !seeded_history(&world, &before, &threads) {
        return world.wrong.read(Clone::clone);
    }
    drop(before);
    if let Some((pod, answers)) = restarted(&world, rng, &plan, "pod-idle", false) {
        idle(&world, &pod, &answers, stats);
    }
    if let Some((pod, answers)) = restarted(&world, rng, &plan, "pod-serving", true) {
        judged(&world, &pod, &answers, stats);
    }
    world.wrong.read(Clone::clone)
}

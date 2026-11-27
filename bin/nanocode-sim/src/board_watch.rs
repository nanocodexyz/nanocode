use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_board::{
    Board, Change, Entry, Event, EventId, Fold, Op, TaskId,
    sim::Mutation as BoardRule,
    wake::{Listening, Napping, Notifier, Notifying, Polling, Ring, Timer, Wake},
    watch::{
        self, Attempt, Filter, Handled, Handler, Key, Kind, Lease, Plan, Policy, Registered, Step,
    },
};
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_turnlog::{LogHost, StoredCheckpoint};

use super::{MS, World};
use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, PPM, Rng, fnv},
};

pub const ALL: [BoardRule; 14] = [
    BoardRule::ConsumeIgnoresCursor,
    BoardRule::ConsumeSplits,
    BoardRule::WatchOutOfOrder,
    BoardRule::AckIgnoresLease,
    BoardRule::TakeIgnoresLease,
    BoardRule::TakeIgnoresBackoff,
    BoardRule::TriesUnbounded,
    BoardRule::WatchSeesOwnWrites,
    BoardRule::FireEarly,
    BoardRule::FireReplays,
    BoardRule::FireIgnoresStop,
    BoardRule::UnscheduleIgnored,
    BoardRule::WaitSkipsPoll,
    BoardRule::WatchUnchecked,
];

pub const fn name(rule: BoardRule) -> &'static str {
    match rule {
        BoardRule::ConsumeIgnoresCursor => "consume_ignores_cursor",
        BoardRule::ConsumeSplits => "consume_splits",
        BoardRule::WatchOutOfOrder => "watch_out_of_order",
        BoardRule::AckIgnoresLease => "ack_ignores_lease",
        BoardRule::TakeIgnoresLease => "take_ignores_lease",
        BoardRule::TakeIgnoresBackoff => "take_ignores_backoff",
        BoardRule::TriesUnbounded => "tries_unbounded",
        BoardRule::WatchSeesOwnWrites => "watch_sees_own_writes",
        BoardRule::FireEarly => "fire_early",
        BoardRule::FireReplays => "fire_replays",
        BoardRule::FireIgnoresStop => "fire_ignores_stop",
        BoardRule::UnscheduleIgnored => "unschedule_ignored",
        BoardRule::WaitSkipsPoll => "wait_skips_poll",
        BoardRule::WatchUnchecked => "watch_unchecked",
        _ => "board",
    }
}

pub const TALLY: &str = "agent:tally";
pub const ECHO: &str = "agent:echo";
pub const CLOCK: &str = "agent:clock";

static TALLIED: AtomicU64 = AtomicU64::new(0);
static ECHOED: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static BURIED: AtomicU64 = AtomicU64::new(0);
static FIRED: AtomicU64 = AtomicU64::new(0);
static WOKEN: AtomicU64 = AtomicU64::new(0);

pub fn summary() -> String {
    format!(
        "watch: tallied={} echoed={} failed={} buried={} fired={} woken={}",
        TALLIED.load(Ordering::SeqCst),
        ECHOED.load(Ordering::SeqCst),
        FAILED.load(Ordering::SeqCst),
        BURIED.load(Ordering::SeqCst),
        FIRED.load(Ordering::SeqCst),
        WOKEN.load(Ordering::SeqCst),
    )
}

#[derive(Clone)]
struct Every {
    id: String,
    first_ms: u64,
    every_ms: Option<u64>,
    until_ms: Option<u64>,
    times: Option<u64>,
    cancel_ms: Option<u64>,
}

pub struct Shape {
    seed: u64,
    tallies: u64,
    echoes: u64,
    policy: Policy,
    lease_ms: u64,
    poison_ppm: u64,
    flaky_ppm: u64,
    stall_ppm: u64,
    gap_ppm: u64,
    span_ms: u64,
    listen: bool,
    deaf_ppm: u64,
    schedules: Vec<Every>,
}

impl Shape {
    pub fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "board watch shape");
        let span_ms = rng.range(50, 1500);
        let gaps = [rng.range(10, 120), rng.range(10, 120), rng.range(10, 120)];
        let schedules = vec![
            Every {
                id: "once".to_owned(),
                first_ms: rng.range(0, span_ms),
                every_ms: None,
                until_ms: None,
                times: None,
                cancel_ms: None,
            },
            Every {
                id: "times".to_owned(),
                first_ms: rng.range(0, span_ms),
                every_ms: Some(gaps[0]),
                until_ms: None,
                times: Some(rng.range(1, 6)),
                cancel_ms: None,
            },
            Every {
                id: "until".to_owned(),
                first_ms: rng.range(0, span_ms),
                every_ms: Some(gaps[1]),
                until_ms: Some(rng.range(0, span_ms * 2)),
                times: None,
                cancel_ms: None,
            },
            Every {
                id: "forever".to_owned(),
                first_ms: rng.range(0, span_ms),
                every_ms: Some(gaps[2]),
                until_ms: None,
                times: None,
                cancel_ms: Some(rng.range(0, span_ms * 2)),
            },
        ];
        let backoff_ms = rng.range(1, 40);
        Self {
            seed,
            tallies: rng.range(1, 3),
            echoes: rng.range(1, 3),
            policy: Policy {
                tries: rng.range(1, 5),
                backoff_ms,
                backoff_cap_ms: backoff_ms * rng.range(1, 8),
            },
            lease_ms: rng.range(10, 120),
            poison_ppm: rng.range(0, 150_000),
            flaky_ppm: rng.range(0, 300_000),
            stall_ppm: rng.range(0, 100_000),
            gap_ppm: rng.range(0, 200_000),
            span_ms,
            listen: rng.chance(500_000),
            deaf_ppm: rng.range(0, 1_000_000),
            schedules,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "tallies={} echoes={} tries={} backoff_ms={}..{} watch_lease_ms={} poison_ppm={} flaky_ppm={} listen={} deaf_ppm={}",
            self.tallies,
            self.echoes,
            self.policy.tries,
            self.policy.backoff_ms,
            self.policy.backoff_cap_ms,
            self.lease_ms,
            self.poison_ppm,
            self.flaky_ppm,
            self.listen,
            self.deaf_ppm
        )
    }

    pub fn slack_ms(&self, step_ms: u64, poll_ms: u64, latency_ms: u64, others: u64) -> u64 {
        let retries = self.policy.backoff_cap_ms.saturating_add(self.lease_ms) * self.policy.tries;
        self.span_ms * 2 + retries * 8 + self.drain_ms(step_ms, poll_ms, latency_ms, others) + 2_000
    }

    fn drain_ms(&self, step_ms: u64, poll_ms: u64, latency_ms: u64, others: u64) -> u64 {
        let rows = self.fired_max(poll_ms) + others;
        let run = step_ms + latency_ms * 4 + (self.lease_ms * 2 * self.stall_ppm).div_ceil(PPM);
        let ladder = (1..self.policy.tries)
            .map(|tries| self.policy.backoff(tries))
            .sum::<u64>()
            + run * self.policy.tries;
        let mut chance = self.flaky_ppm;
        let mut flaky = 0;
        for tries in 1..self.policy.tries {
            flaky += (chance * (self.policy.backoff(tries) + run)).div_ceil(PPM);
            chance = chance * self.flaky_ppm / PPM;
        }
        let batches = rows.div_ceil(watch::BATCH as u64) * (self.policy.backoff(1) + run);
        rows * (run + flaky) + (rows * self.poison_ppm).div_ceil(PPM) * ladder + batches
    }

    fn fired_max(&self, poll_ms: u64) -> u64 {
        let late = self.span_ms.max(poll_ms) * 2;
        self.schedules
            .iter()
            .map(|every| match (every.every_ms, every.times) {
                (None, _) => 1,
                (Some(_), Some(times)) => times,
                (Some(gap), None) => {
                    let end = every
                        .until_ms
                        .or(every.cancel_ms.map(|at| at + late))
                        .unwrap_or(every.first_ms);
                    end.saturating_sub(every.first_ms) / gap.max(1) + 1
                }
            })
            .sum()
    }

    fn poison(&self, seq: u64) -> bool {
        let roll = fnv(
            fnv(FNV_OFFSET, &self.seed.to_le_bytes()),
            &seq.to_le_bytes(),
        );
        roll % 1_000_000 < self.poison_ppm
    }
}

struct Lossy {
    notifier: Arc<Notifier>,
    seed: u64,
    rings: AtomicU64,
    deaf_ppm: u64,
}

impl Ring for Lossy {
    fn ring(&self, scope: &str, head: u64) {
        let count = self.rings.fetch_add(1, Ordering::SeqCst);
        let roll = fnv(
            fnv(FNV_OFFSET, &self.seed.to_le_bytes()),
            &count.to_le_bytes(),
        );
        if roll % 1_000_000 >= self.deaf_ppm {
            self.notifier.ring(scope, head);
        }
    }
}

struct Early {
    inner: Arc<dyn LogHost>,
    clock: Arc<Clock>,
    schedules: Vec<Every>,
    broken: Arc<OnceLock<String>>,
}

impl Early {
    fn early(&self, body: &str) -> Option<String> {
        let event = serde_json::from_str::<Event>(body).ok()?;
        let Op::Watch(Step::Fire { id, n }) = event.op else {
            return None;
        };
        let every = self.schedules.iter().find(|every| every.id == id)?;
        let now = self.clock.now_ns() / MS;
        let due = match every.every_ms {
            None if n > 0 => {
                return Some(format!("one-off {id} fired again as fire {n} at {now} ms"));
            }
            None => every.first_ms,
            Some(gap) => every.first_ms + gap * n,
        };
        (now < due)
            .then(|| format!("{id} fired fire {n} at {now} ms, before its earliest time {due} ms"))
    }
}

impl LogHost for Early {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.inner.call_bound()
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
        if let Some(early) = rows.iter().find_map(|(_, body)| self.early(body)) {
            let _ = self.broken.set(early.clone());
            return Box::pin(async move { Err(StoreError::Backend(early)) });
        }
        self.inner.append(scope, rows)
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

pub struct Watched {
    pub shape: Shape,
    waited: std::cell::RefCell<(usize, u64)>,
    notifier: Arc<Notifier>,
    ring: Arc<Lossy>,
    broken: Arc<OnceLock<String>>,
}

impl Watched {
    pub fn new(seed: u64) -> Self {
        let shape = Shape::of(seed);
        let notifier = Arc::new(Notifier::new());
        let ring = Arc::new(Lossy {
            notifier: Arc::clone(&notifier),
            seed,
            rings: AtomicU64::new(0),
            deaf_ppm: shape.deaf_ppm,
        });
        Self {
            shape,
            waited: std::cell::RefCell::new((0, 0)),
            notifier,
            ring,
            broken: Arc::new(OnceLock::new()),
        }
    }

    pub fn wrap(&self, host: Arc<dyn LogHost>, clock: &Arc<Clock>) -> Arc<dyn LogHost> {
        let early = Arc::new(Early {
            inner: host,
            clock: Arc::clone(clock),
            schedules: self.shape.schedules.clone(),
            broken: Arc::clone(&self.broken),
        });
        Arc::new(Notifying::new(
            early,
            Arc::clone(&self.ring) as Arc<dyn Ring>,
        ))
    }

    pub fn broken(&self) -> Option<String> {
        self.broken.get().cloned()
    }

    fn audit(&self, fold: &Fold) {
        let Some(watcher) = fold.marks().watchers().get(ECHO) else {
            return;
        };
        let tries = self.shape.policy.tries;
        let exhausted = watcher
            .lanes
            .iter()
            .find_map(|(thread, lane)| match lane.attempt {
                Attempt::Idle { tries: tried, .. } if tried >= tries => Some(format!(
                    "{thread} idles after {tried} of {tries} tries instead of being buried"
                )),
                _ => None,
            });
        if let Some(broken) =
            exhausted.or_else(|| ticks(&self.shape, fold, &BTreeMap::new(), false).err())
        {
            let _ = self.broken.set(broken);
        }
    }
}

struct Timing(Arc<Clock>);

impl Timer for Timing {
    fn now_ms(&self) -> u64 {
        self.0.now_ns() / MS
    }

    fn sleep_until(&self, at_ms: u64) -> Napping<'_> {
        let ms = at_ms.saturating_sub(self.now_ms()).max(1);
        Box::pin(self.0.sleep(Duration::from_nanos(ms * MS)))
    }
}

fn tally_filter() -> Filter {
    Filter {
        kinds: BTreeSet::from([Kind::Finished, Kind::Cancelled]),
        ..Filter::default()
    }
}

fn echo_filter() -> Filter {
    Filter {
        kinds: BTreeSet::from([Kind::Replied]),
        ..Filter::default()
    }
}

fn wait_filter() -> Filter {
    Filter {
        kinds: BTreeSet::from([Kind::Finished, Kind::Cancelled]),
        labels: BTreeMap::from([("gate".to_owned(), "join".to_owned())]),
        under: None,
    }
}

fn goal() -> TaskId {
    TaskId("goal".to_owned())
}

fn scheduled(board: &Board, shape: &Shape) -> bool {
    let schedules = board.fold().marks().schedules();
    shape.schedules.iter().all(|every| {
        schedules
            .get(&every.id)
            .is_some_and(|schedule| schedule.due_ms().is_none())
    })
}

fn quiet(world: &World, board: &Board) -> bool {
    world.watched.audit(board.fold());
    if world.watched.broken.get().is_some() {
        return true;
    }
    let echoed = board
        .fold()
        .marks()
        .watchers()
        .get(ECHO)
        .is_some_and(|watcher| watcher.lanes.values().all(|lane| lane.pending.is_empty()));
    let fed = world.shape.hooks.iter().all(|hook| {
        let id = EventId(format!("{}#{}", super::access::SOURCE, hook.key));
        world.firehose.borrow().acked.contains(&hook.key) && board.fold().outcome(&id).is_some()
    });
    world.planned(board)
        && fed
        && scheduled(board, &world.watched.shape)
        && echoed
        && board
            .pending(TALLY, None, &tally_filter())
            .entries
            .is_empty()
}

async fn tally(world: &World, epoch: u64, index: u64) {
    let mut board = world.open(&format!("{TALLY}+i{index}e{epoch}")).await;
    let mut rng = Rng::derive(
        world.watched.shape.seed,
        &format!("board tally {index} {epoch}"),
    );
    loop {
        if let Err(error) = board.refresh().await {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        let batch = board.pending(TALLY, None, &tally_filter());
        if batch.entries.is_empty() {
            if quiet(world, &board) {
                return;
            }
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        let effects = batch
            .entries
            .iter()
            .map(|entry| Op::Reply {
                task: entry.task.clone(),
                body: format!("seen:{}", entry.seq),
            })
            .collect::<Vec<_>>();
        world.nap(rng.range(0, world.shape.step_ms)).await;
        match board.consume(world.now_ms(), &batch, effects).await {
            Ok(_) => {
                TALLIED.fetch_add(batch.entries.len() as u64, Ordering::SeqCst);
            }
            Err(error) => world.note(&error),
        }
        world.nap(rng.range(0, world.shape.poll_ms)).await;
    }
}

struct Echo<'w> {
    world: &'w World,
}

impl Handler for Echo<'_> {
    fn run<'a>(&'a self, lease: &'a Lease) -> Handled<'a> {
        Box::pin(async move {
            let world = self.world;
            let shape = &world.watched.shape;
            let mut rng = Rng::derive(shape.seed, &format!("board echo {}", lease.id));
            world.nap(rng.range(0, world.shape.step_ms)).await;
            if rng.chance(shape.stall_ppm) {
                world.nap(shape.lease_ms * 2).await;
            }
            assert!(
                !lease.entries.is_empty(),
                "a watcher lease carries its matches"
            );
            let rows = lease
                .entries
                .iter()
                .map(|entry| entry.seq)
                .collect::<BTreeSet<_>>();
            assert!(
                rows.first() == Some(&lease.at),
                "a watcher lease starts at its head"
            );
            if rows.iter().any(|row| shape.poison(*row)) {
                return Err("poison".to_owned());
            }
            if rng.chance(shape.flaky_ppm) {
                return Err("flaky".to_owned());
            }
            Ok(rows
                .into_iter()
                .map(|row| Op::Reply {
                    task: lease.thread.clone(),
                    body: format!("echo:{row}"),
                })
                .collect())
        })
    }
}

async fn echo(world: &World, epoch: u64, index: u64) {
    let mut board = world.open(&format!("{ECHO}+i{index}e{epoch}")).await;
    let timer = Timing(Arc::clone(&world.clock));
    let shape = &world.watched.shape;
    let watcher = Registered {
        name: ECHO.to_owned(),
        filter: echo_filter(),
        policy: shape.policy,
        lease_ms: shape.lease_ms,
        handler: Box::new(Echo { world }),
    };
    loop {
        match board.serve(&timer, &watcher).await {
            Ok(served) => {
                ECHOED.fetch_add(served.acked, Ordering::SeqCst);
                FAILED.fetch_add(served.failed, Ordering::SeqCst);
                BURIED.fetch_add(served.buried, Ordering::SeqCst);
            }
            Err(error) => world.note(&error),
        }
        if board.refresh().await.is_ok() && quiet(world, &board) {
            return;
        }
        world.nap(world.shape.poll_ms).await;
    }
}

async fn clock(world: &World, epoch: u64) {
    let mut board = world.open(CLOCK).await;
    let shape = &world.watched.shape;
    let mut rng = Rng::derive(shape.seed, &format!("board clock {epoch}"));
    loop {
        if let Err(error) = board.refresh().await {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        if board.fold().task(&goal()).is_some() {
            for every in &shape.schedules {
                let schedule = board.fold().marks().schedules().get(&every.id).cloned();
                let result = match schedule {
                    None => {
                        let plan = Plan {
                            id: every.id.clone(),
                            send: Op::Reply {
                                task: goal(),
                                body: format!("tick:{}", every.id),
                            },
                            first_ms: every.first_ms,
                            every_ms: every.every_ms,
                            until_ms: every.until_ms,
                            times: every.times,
                        };
                        board.schedule(world.now_ms(), plan).await.map(drop)
                    }
                    Some(schedule)
                        if schedule.ended.is_none()
                            && every.cancel_ms.is_some_and(|at| world.now_ms() >= at) =>
                    {
                        let result = board.unschedule(world.now_ms(), &every.id).await.map(drop);
                        let live = board
                            .fold()
                            .marks()
                            .schedules()
                            .get(&every.id)
                            .is_some_and(|schedule| schedule.ended.is_none());
                        if result.is_ok() && live {
                            // Left live, it stays due and this loop unschedules it again until
                            // the run's deadline, so stop the run here.
                            panic!(
                                "the board applied the unschedule of {} but kept it live",
                                every.id
                            );
                        }
                        result
                    }
                    Some(_) => Ok(()),
                };
                if let Err(error) = result {
                    world.note(&error);
                }
            }
        }
        match board.fire_due(world.now_ms()).await {
            Ok(fired) => {
                FIRED.fetch_add(fired, Ordering::SeqCst);
            }
            Err(error) => world.note(&error),
        }
        if quiet(world, &board) {
            return;
        }
        let nap = if rng.chance(shape.gap_ppm) {
            rng.range(world.shape.poll_ms, shape.span_ms.max(world.shape.poll_ms))
        } else {
            world.shape.poll_ms
        };
        world.nap(nap).await;
    }
}

fn consolidating(world: &World) -> usize {
    world
        .shape
        .plan
        .iter()
        .filter(|task| task.labels.get("gate").map(String::as_str) == Some("join"))
        .count()
}

fn joins(fold: &Fold, task: &TaskId) -> bool {
    fold.task(task)
        .is_some_and(|task| task.spec.labels.get("gate").map(String::as_str) == Some("join"))
}

/// A consolidate task's own ending, which every wait must report.
fn awaited(fold: &Fold, entry: &Entry) -> bool {
    entry.change.terminal() && joins(fold, &entry.task)
}

/// An ending the wait may report: a filter's labels also match a task whose
/// root carries them, and moves can leave a finished task under a
/// consolidate task that became a root.
fn admitted(fold: &Fold, entry: &Entry) -> bool {
    awaited(fold, entry)
        || entry.change.terminal() && joins(fold, &watch::root(fold.tasks(), &entry.task))
}

async fn waiter(world: &World, epoch: u64) {
    let mut board = world.open(&format!("agent:waiter+e{epoch}")).await;
    let timer = Timing(Arc::clone(&world.clock));
    let polling = Polling {
        timer: &timer,
        every_ms: world.shape.poll_ms.max(1),
    };
    let listening = Listening {
        notifier: &world.watched.notifier,
        fallback: &polling,
    };
    let wake: &dyn Wake = if world.watched.shape.listen {
        &listening
    } else {
        &polling
    };
    let mut after = 0;
    let mut got = BTreeSet::new();
    while got.len() < consolidating(world) {
        let until = world.now_ms() + world.shape.deadline_ms * 4;
        match board
            .wait(wake, &timer, None, &wait_filter(), after, until)
            .await
        {
            Ok(found) => {
                WOKEN.fetch_add(1, Ordering::SeqCst);
                let from = after;
                for entry in found {
                    if entry.seq <= from || !admitted(board.fold(), &entry) {
                        world.oracle.borrow_mut().wrong.push(format!(
                            "wait after row {from} returned {} {:?} on {} under root {}",
                            entry.seq,
                            entry.change,
                            entry.task,
                            watch::root(board.fold().tasks(), &entry.task)
                        ));
                    }
                    after = after.max(entry.seq);
                    got.insert((entry.seq, entry.task));
                }
                world.watched.waited.replace((got.len(), after));
            }
            Err(error) => {
                world.note(&error);
                world.nap(world.shape.poll_ms).await;
            }
        }
    }
    let want = board
        .fold()
        .entries()
        .iter()
        .filter(|entry| entry.seq <= after && awaited(board.fold(), entry))
        .map(|entry| (entry.seq, entry.task.clone()))
        .collect::<BTreeSet<_>>();
    // Endings admitted through a root may come and go as tasks move, so only
    // consolidate tasks' own endings must all be seen.
    let got = got
        .into_iter()
        .filter(|(seq, task)| {
            board.fold().entries().iter().any(|entry| {
                entry.seq == *seq && entry.task == *task && awaited(board.fold(), entry)
            })
        })
        .collect::<BTreeSet<_>>();
    if want != got {
        world.oracle.borrow_mut().wrong.push(format!(
            "wait saw {} consolidate endings, the board holds {}",
            got.len(),
            want.len()
        ));
    }
}

pub fn stalled(world: &World, fold: &Fold) -> String {
    let marks = fold.marks();
    let schedules = world
        .watched
        .shape
        .schedules
        .iter()
        .filter_map(|every| match marks.schedules().get(&every.id) {
            None => Some(format!("{} unscheduled", every.id)),
            Some(schedule) => schedule
                .due_ms()
                .map(|due| format!("{} due at {due} ms after {} fires", every.id, schedule.sent)),
        })
        .collect::<Vec<_>>();
    let lanes = marks
        .watchers()
        .get(ECHO)
        .map(|watcher| {
            watcher
                .lanes
                .iter()
                .filter(|(_, lane)| !lane.pending.is_empty())
                .map(|(thread, lane)| format!("{thread} {:?} {:?}", lane.pending, lane.attempt))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let position = fold.position(&Key::Cursor {
        actor: TALLY.to_owned(),
        scope: None,
    });
    let untallied = fold
        .entries()
        .iter()
        .filter(|entry| entry.seq > position && entry.change.terminal())
        .count();
    let (waited, after) = *world.watched.waited.borrow();
    format!(
        "schedules {schedules:?}, echo lanes {lanes:?}, {untallied} untallied, waited for {waited} of {} after row {after}",
        consolidating(world)
    )
}

pub type Actor<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

pub fn actors<'a>(world: &'a World, epoch: u64, actors: &mut Vec<Actor<'a>>) {
    let shape = &world.watched.shape;
    for index in 0..shape.tallies {
        actors.push(Box::pin(tally(world, epoch, index)));
    }
    for index in 0..shape.echoes {
        actors.push(Box::pin(echo(world, epoch, index)));
    }
    actors.push(Box::pin(clock(world, epoch)));
    actors.push(Box::pin(waiter(world, epoch)));
}

#[derive(Clone, Default)]
struct Lane {
    lease: Option<(EventId, u64)>,
    tries: u64,
    not_before_ms: u64,
}

fn backoff(policy: &Policy, tries: u64) -> u64 {
    let mut wait = policy.backoff_ms;
    for _ in 1..tries {
        wait = wait.saturating_mul(2);
        if wait >= policy.backoff_cap_ms {
            break;
        }
    }
    wait.min(policy.backoff_cap_ms)
}

fn reply(entry: &Entry) -> Option<&str> {
    match &entry.change {
        Change::Replied { body, .. } => Some(body),
        _ => None,
    }
}

fn steps(
    world: &World,
    rows: &[(u64, String)],
    fold: &Fold,
) -> Result<BTreeMap<String, u64>, String> {
    let policy = world.watched.shape.policy;
    let mut seen = BTreeSet::new();
    let mut now = 0;
    let mut lanes: BTreeMap<TaskId, Lane> = BTreeMap::new();
    let mut cursors: BTreeMap<Key, u64> = BTreeMap::new();
    let mut cancelled = BTreeMap::new();
    for (seq, body) in rows {
        let event: Event = serde_json::from_str(body)
            .map_err(|error| format!("row {seq} is not an event: {error}"))?;
        if seen.contains(&event.id) || !nanocode_board::access::owns(&event.by, &event.id) {
            continue;
        }
        seen.insert(event.id.clone());
        now = now.max(event.at_ms);
        if fold.outcome(&event.id) != Some(&Ok(*seq)) {
            continue;
        }
        let Op::Watch(step) = event.op else {
            continue;
        };
        let held = |lane: &Lane, lease: &EventId, what: &str| match &lane.lease {
            Some((current, _)) if current == lease => Ok(()),
            current => Err(format!(
                "row {seq}: {what} with lease {lease} while the lane holds {current:?}"
            )),
        };
        match step {
            Step::Consume {
                key: key @ Key::Cursor { .. },
                from,
                to,
                ..
            } => {
                let position = cursors.get(&key).copied().unwrap_or_default();
                if from != position || to <= from {
                    return Err(format!(
                        "row {seq}: cursor {key:?} moved {from}..{to} while it stood at {position}"
                    ));
                }
                cursors.insert(key, to);
            }
            Step::Consume {
                key: Key::Watcher { thread, .. },
                lease,
                ..
            } => {
                let lane = lanes.entry(thread).or_default();
                let Some(lease) = lease else {
                    return Err(format!("row {seq}: a watcher ack without a lease"));
                };
                held(lane, &lease, "acked")?;
                *lane = Lane::default();
            }
            Step::Take {
                thread, lease_ms, ..
            } => {
                let lane = lanes.entry(thread.clone()).or_default();
                if let Some((lease, until)) = &lane.lease
                    && *until > now
                {
                    return Err(format!(
                        "row {seq}: {thread} taken at {now} ms while {lease} held it until {until} ms"
                    ));
                }
                if lane.not_before_ms > now {
                    return Err(format!(
                        "row {seq}: {thread} taken at {now} ms inside its backoff to {} ms",
                        lane.not_before_ms
                    ));
                }
                if lane.tries >= policy.tries {
                    return Err(format!(
                        "row {seq}: {thread} taken for try {} of {}",
                        lane.tries + 1,
                        policy.tries
                    ));
                }
                lane.lease = Some((event.id.clone(), now + lease_ms));
                lane.tries += 1;
            }
            Step::Fail { thread, lease, .. } => {
                let lane = lanes.entry(thread).or_default();
                held(lane, &lease, "failed")?;
                if lane.tries >= policy.tries {
                    *lane = Lane::default();
                } else {
                    lane.lease = None;
                    lane.not_before_ms = now + backoff(&policy, lane.tries);
                }
            }
            Step::Bury { thread, .. } => {
                let lane = lanes.entry(thread.clone()).or_default();
                if lane.tries < policy.tries
                    || lane.lease.as_ref().is_some_and(|(_, until)| *until > now)
                {
                    return Err(format!(
                        "row {seq}: {thread} buried after {} tries",
                        lane.tries
                    ));
                }
                *lane = Lane::default();
            }
            Step::Unschedule { id, .. } => {
                cancelled.entry(id).or_insert(*seq);
            }
            Step::Watch { .. }
            | Step::Unwatch { .. }
            | Step::Schedule { .. }
            | Step::Fire { .. } => {}
        }
    }
    Ok(cancelled)
}

fn tallies(fold: &Fold) -> Result<(), String> {
    let mut seen = BTreeMap::new();
    for entry in fold.entries() {
        if let Some(seq) = reply(entry).and_then(|body| body.strip_prefix("seen:")) {
            *seen
                .entry((entry.task.clone(), seq.to_owned()))
                .or_insert(0) += 1;
        }
    }
    for entry in fold
        .entries()
        .iter()
        .filter(|entry| entry.change.terminal())
    {
        let copies = seen
            .remove(&(entry.task.clone(), entry.seq.to_string()))
            .unwrap_or(0);
        if copies != 1 {
            return Err(format!(
                "the tally consumed {} at row {} {copies} times",
                entry.task, entry.seq
            ));
        }
    }
    if let Some(((task, seq), _)) = seen.into_iter().next() {
        return Err(format!(
            "the tally counted {task} at row {seq}, which never ended there"
        ));
    }
    Ok(())
}

/// Roots as they were at a row: lanes index a row under its root when it is
/// written, and a later move sends only later rows to the new root's lane.
struct Roots {
    parents: BTreeMap<TaskId, Option<TaskId>>,
    moves: Vec<(u64, TaskId, Option<TaskId>)>,
}

impl Roots {
    fn new(fold: &Fold) -> Self {
        let parents = fold
            .tasks()
            .iter()
            .map(|(id, task)| (id.clone(), task.spec.parent.clone()))
            .collect();
        let moves = fold
            .entries()
            .iter()
            .filter_map(|entry| match &entry.change {
                Change::Reparented { from, .. } => {
                    Some((entry.seq, entry.task.clone(), from.clone()))
                }
                _ => None,
            })
            .collect();
        Self { parents, moves }
    }

    fn at(&self, task: &TaskId, seq: u64) -> TaskId {
        let mut parents = self.parents.clone();
        for (_, moved, from) in self.moves.iter().rev().take_while(|(at, ..)| *at > seq) {
            parents.insert(moved.clone(), from.clone());
        }
        let mut thread = task.clone();
        for _ in 0..=parents.len() {
            match parents.get(&thread) {
                Some(Some(parent)) => thread = parent.clone(),
                _ => break,
            }
        }
        thread
    }
}

fn echoes(world: &World, fold: &Fold) -> Result<(), String> {
    let shape = &world.watched.shape;
    let marks = fold.marks();
    let Some(watcher) = marks.watchers().get(ECHO) else {
        return Err("the echo watcher was never registered".to_owned());
    };
    let mut sources = BTreeSet::new();
    let mut echoed: BTreeMap<(TaskId, u64), u64> = BTreeMap::new();
    let mut last: BTreeMap<TaskId, u64> = BTreeMap::new();
    let roots = Roots::new(fold);
    for entry in fold.entries() {
        let Some(body) = reply(entry) else {
            continue;
        };
        let mut thread = roots.at(&entry.task, entry.seq);
        if marks.author(&entry.event) == Some(ECHO) {
            let at = body
                .strip_prefix("echo:")
                .and_then(|at| at.parse::<u64>().ok())
                .ok_or_else(|| format!("the echo watcher wrote {body:?}"))?;
            // An echo answers the row it read, in the lane that row was in.
            thread = roots.at(&entry.task, at);
            if let Some(before) = last.get(&thread)
                && *before >= at
            {
                return Err(format!("{thread}: echoed row {at} after row {before}"));
            }
            last.insert(thread.clone(), at);
            *echoed.entry((thread, at)).or_insert(0) += 1;
        } else if entry.seq > watcher.since {
            sources.insert((thread, entry.seq));
        }
    }
    let mut dead: BTreeMap<(TaskId, u64), u64> = BTreeMap::new();
    for letter in marks.dead().iter().filter(|letter| letter.watcher == ECHO) {
        *dead.entry((letter.thread.clone(), letter.at)).or_insert(0) += 1;
    }
    for source in &sources {
        let echoes = echoed.remove(source).unwrap_or(0);
        let buried = dead.remove(source).unwrap_or(0);
        if echoes + buried != 1 {
            return Err(format!(
                "{} row {}: echoed {echoes} times and dead-lettered {buried} times",
                source.0, source.1
            ));
        }
        if echoes == 1 && shape.poison(source.1) {
            return Err(format!(
                "{} row {}: a poison reply was echoed",
                source.0, source.1
            ));
        }
    }
    if let Some(((thread, at), _)) = echoed.into_iter().next().or(dead.into_iter().next()) {
        return Err(format!(
            "{thread}: the watcher handled row {at}, which it does not match"
        ));
    }
    Ok(())
}

fn ticks(
    shape: &Shape,
    fold: &Fold,
    cancelled: &BTreeMap<String, u64>,
    ended: bool,
) -> Result<(), String> {
    for every in &shape.schedules {
        let prefix = format!("fire/{}/", every.id);
        let fires = fold
            .entries()
            .iter()
            .filter_map(|entry| {
                let n = entry.event.0.strip_prefix(&prefix)?.parse::<u64>().ok()?;
                Some((n, entry.at_ms, entry.seq))
            })
            .collect::<Vec<_>>();
        let due = |slot: u64| match every.every_ms {
            None => (slot == 0).then_some(every.first_ms),
            Some(gap) => Some(every.first_ms + gap * slot),
        };
        let mut slot = 0;
        let mut count = 0;
        for (n, at, seq) in fires {
            let what = format!("{} fire {n} at {at} ms (row {seq})", every.id);
            if n != count {
                return Err(format!("{what}: expected fire {count}"));
            }
            let Some(due) = due(slot) else {
                return Err(format!("{what}: a one-off fired twice"));
            };
            if at < due {
                return Err(format!("{what}: slot {slot} was due at {due} ms"));
            }
            if every.until_ms.is_some_and(|until| due > until)
                || every.times.is_some_and(|times| count >= times)
            {
                return Err(format!("{what}: past its end"));
            }
            if cancelled.get(&every.id).is_some_and(|row| seq > *row) {
                return Err(format!("{what}: after it was cancelled"));
            }
            count += 1;
            slot = match every.every_ms {
                Some(gap) if at >= every.first_ms => {
                    (slot + 1).max((at - every.first_ms) / gap + 1)
                }
                _ => slot + 1,
            };
        }
        let more = ended
            && !cancelled.contains_key(&every.id)
            && every.times.is_none_or(|times| count < times)
            && due(slot).is_some_and(|due| every.until_ms.is_none_or(|until| due <= until));
        if more {
            return Err(format!(
                "{} stopped after {count} fires with slot {slot} still due",
                every.id
            ));
        }
    }
    Ok(())
}

pub fn check(world: &World, rows: &[(u64, String)], fold: &Fold) -> Result<(), String> {
    let cancelled = steps(world, rows, fold)?;
    tallies(fold)?;
    echoes(world, fold)?;
    ticks(&world.watched.shape, fold, &cancelled, true)
}

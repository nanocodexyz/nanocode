use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_scope::{
    Adopted, Backfilled, Checkpoints, Committed, MemoryCheckpoints, Part, PartedLeases, Policy,
    Reached, Resident, Route, ScopeError, Scopes, Wiring,
};
use nanocode_sync::{Atom, block_on};
use nanocode_turnlog::{
    LogHost, SCAN_END,
    parted::{ModelParts, PartStore, PartedLog},
};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
};

const PARTS: u32 = 4;
const CAPACITY: Range<u64> = 1..9;
const OLD_PER_LANE: Range<u64> = 1..4;
const PAGES: Range<u64> = 3..9;
const PLAIN_ROWS: Range<u64> = 0..3;
const READ_MS: u64 = 5;
const HEAD_START_MS: u64 = 1;
const READS_PER_SERVED_GET: u64 = 1;
const TTL_MS: u64 = 600_000;
const GRACE_MS: u64 = 300;
const MARGIN_MS: u64 = 100;
const START_MS: u64 = 1_000_000;
const STATE_BYTES: usize = 8;
const NANOS_PER_MS: u64 = 1_000_000;
const NODE: &str = "b0";
const LOAD_READS: Range<u64> = 8..24;
const PERCENTILE: u64 = 99;
const PERCENT: u64 = 100;
const OLD: &str = "old-";

#[derive(Clone, Copy, Debug, Default)]
pub struct BackfillStats {
    pub runs: u64,
    pub adopted: u64,
    pub served: u64,
    pub served_ns_max: u64,
    pub in_flight_max: u64,
    pub load_runs: u64,
    pub load_adopted: u64,
    pub load_lanes_max: u64,
    pub load_p99_ns_max: u64,
}

impl BackfillStats {
    pub fn add(&mut self, other: &Self) {
        self.runs += other.runs;
        self.adopted += other.adopted;
        self.served += other.served;
        self.served_ns_max = self.served_ns_max.max(other.served_ns_max);
        self.in_flight_max = self.in_flight_max.max(other.in_flight_max);
        self.load_runs += other.load_runs;
        self.load_adopted += other.load_adopted;
        self.load_lanes_max = self.load_lanes_max.max(other.load_lanes_max);
        self.load_p99_ns_max = self.load_p99_ns_max.max(other.load_p99_ns_max);
    }
}

struct Aged;

fn parsed(seq: u64, row: &str) -> Result<u64, StoreError> {
    row.parse()
        .map_err(|error| StoreError::Backend(format!("row {seq} {row}: {error}")))
}

impl Resident for Aged {
    type State = u64;
    type Command = u64;
    type Reply = u64;

    fn empty(&self) -> u64 {
        0
    }

    fn decide(&self, state: &u64, command: &u64) -> (Vec<String>, u64) {
        (vec![command.to_string()], state + command)
    }

    fn apply(&self, state: &mut u64, seq: u64, row: &str) -> Result<(), StoreError> {
        *state += parsed(seq, row)?;
        Ok(())
    }

    fn encode(&self, state: &u64) -> Vec<Part> {
        vec![Part {
            name: "sum".to_owned(),
            body: state.to_string(),
        }]
    }

    fn decode(&self, parts: Vec<Part>) -> Result<u64, StoreError> {
        let [part] = <[Part; 1]>::try_from(parts).map_err(|parts| {
            StoreError::Backend(format!("a sum checkpoint has {} parts", parts.len()))
        })?;
        parsed(0, &part.body)
    }

    fn weight(&self, _state: &u64) -> usize {
        STATE_BYTES
    }

    fn adoption<'a>(
        &'a self,
        _log: &'a Arc<dyn LogHost>,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(std::future::ready(Ok(scope.starts_with(OLD))))
    }

    fn adopt<'a>(
        &'a self,
        log: &'a Arc<dyn LogHost>,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Option<Adopted<u64>>, StoreError>> {
        Box::pin(async move {
            let mut adopted = Adopted {
                state: 0,
                next: 0,
                rows: 0,
                bytes: 0,
            };
            loop {
                let page = log.scan(scope, adopted.next, adopted.next + 1).await?;
                let Some((seq, row)) = page.first() else {
                    break;
                };
                adopted.state += parsed(*seq, row)?;
                adopted.rows += 1;
                adopted.bytes += row.len() as u64;
                adopted.next = seq + 1;
            }
            Ok((adopted.next > 0).then_some(adopted))
        })
    }
}

struct Nowhere;

impl Route<Aged> for Nowhere {
    fn forward<'a>(
        &'a self,
        holder: &'a str,
        _scope: &'a str,
        _command: u64,
    ) -> StoreFuture<'a, Result<Committed<u64>, ScopeError>> {
        Box::pin(std::future::ready(Err(ScopeError::Unreachable {
            holder: holder.to_owned(),
        })))
    }
}

struct Paced {
    capacity: u32,
    rows: Atom<BTreeMap<String, Vec<(u64, String)>>>,
    in_flight: AtomicU64,
    in_flight_max: AtomicU64,
}

impl Paced {
    async fn read(&self, scope: &str, from: u64, to: u64) -> Vec<(u64, String)> {
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.in_flight_max.fetch_max(now, Ordering::Relaxed);
        nanocode_platform::sleep(Duration::from_millis(READ_MS)).await;
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.rows.read(|rows| {
            rows.get(scope)
                .map(|rows| {
                    rows.iter()
                        .filter(|(seq, _)| *seq >= from && *seq < to)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }
}

impl LogHost for Paced {
    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn read_capacity(&self) -> u32 {
        self.capacity
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move { Ok(self.read(scope, from, to).await) })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move { Ok(self.read(scope, 0, SCAN_END).await) })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.rows.update(|kept| {
                kept.entry(scope.to_owned())
                    .or_default()
                    .extend(rows.iter().cloned());
            });
            Ok(())
        })
    }
}

struct World {
    clock: Arc<Clock>,
    paced: Arc<Paced>,
    scopes: Scopes<Aged>,
    old: Vec<(String, u64)>,
    plain: Vec<String>,
    capacity: u64,
}

fn world(rng: &mut Rng, clock: &Arc<Clock>) -> World {
    let store = Arc::new(ModelParts::new().with_clock(Arc::new(|| START_MS)));
    let count = NonZeroU32::new(PARTS).expect("partitions are counted from one");
    block_on(store.migrate(count)).expect("the model store migrates");
    let log = Arc::new(
        PartedLog::fenced(Arc::clone(&store), count, NODE)
            .with_clock(Arc::new(|| START_MS))
            .with_lease_bounds(GRACE_MS, MARGIN_MS),
    );
    let capacity = rng.range(CAPACITY.start, CAPACITY.end);
    let mut rows = BTreeMap::new();
    let mut old = Vec::new();
    for at in 0..capacity * rng.range(OLD_PER_LANE.start, OLD_PER_LANE.end) {
        let name = format!("{OLD}{at}");
        let seeded: Vec<(u64, String)> = (0..rng.range(PAGES.start, PAGES.end))
            .map(|seq| (seq, rng.range(1, 10).to_string()))
            .collect();
        old.push((
            name.clone(),
            seeded
                .iter()
                .map(|(seq, row)| parsed(*seq, row).unwrap_or(0))
                .sum(),
        ));
        rows.insert(name, seeded);
    }
    let mut plain = Vec::new();
    for at in 0..rng.range(1, capacity) {
        let name = format!("new-{at}");
        let seeded: Vec<(u64, String)> = (0..rng.range(PLAIN_ROWS.start, PLAIN_ROWS.end))
            .map(|seq| (seq, rng.range(1, 10).to_string()))
            .collect();
        rows.insert(name.clone(), seeded);
        plain.push(name);
    }
    let paced = Arc::new(Paced {
        capacity: u32::try_from(capacity).expect("a capacity fits"),
        rows: Atom::new(rows),
        in_flight: AtomicU64::new(0),
        in_flight_max: AtomicU64::new(0),
    });
    let policy = Policy {
        checkpoint_rows: usize::try_from(PAGES.end).expect("a row bound fits"),
        checkpoint_ms: TTL_MS,
        resident_bytes_max: STATE_BYTES * (old.len() + plain.len()),
        forward_ms: READ_MS,
        admit_ms: TTL_MS,
    };
    let scopes = Scopes::new(Wiring {
        fold: Arc::new(Aged),
        log: Arc::clone(&paced) as Arc<dyn LogHost>,
        leases: Arc::new(PartedLeases::new(log, NODE, TTL_MS)),
        checkpoints: Arc::new(MemoryCheckpoints::new(store)) as Arc<dyn Checkpoints>,
        route: Arc::new(Nowhere),
        policy,
        clock: Arc::new(|| START_MS),
        claimed: None,
    });
    World {
        clock: Arc::clone(clock),
        paced,
        scopes,
        old,
        plain,
        capacity,
    }
}

fn ran(world: &World, rng: &mut Rng, tasks: Vec<Task<'_>>, what: &str) -> Option<String> {
    match Executor::new(
        Arc::clone(&world.clock),
        Rng::derive(rng.below(u64::MAX), what),
    )
    .run(tasks, None)
    {
        Ended::Finished => None,
        ended => Some(format!(
            "{what}: the run ended {ended:?} with calls still waiting"
        )),
    }
}

fn queued(world: &World, rng: &mut Rng) -> Vec<String> {
    let answers: Atom<Vec<String>> = Atom::default();
    let tasks = world
        .old
        .iter()
        .map(|(name, _)| {
            let (answers, scopes) = (&answers, &world.scopes);
            Box::pin(async move {
                match scopes.get(name).await {
                    Err(ScopeError::Unadopted) => {}
                    Ok(_) => answers.update(|answers| {
                        answers.push(format!(
                            "backfill: a get of {name} served it before its backfill"
                        ));
                    }),
                    Err(error) => answers.update(|answers| {
                        answers.push(format!("backfill: a get of {name} answered {error}"));
                    }),
                }
            }) as Task<'_>
        })
        .collect();
    let mut wrong: Vec<String> = ran(world, rng, tasks, "unadopted gets")
        .into_iter()
        .collect();
    wrong.extend(answers.read(Clone::clone));
    let adopt = world.scopes.stats().usage.adopt.calls;
    if adopt > 0 {
        wrong.push(format!(
            "I8: gets of unadopted scopes made {adopt} adoption reads"
        ));
    }
    wrong
}

fn raced(world: &World, rng: &mut Rng, stats: &mut BackfillStats) -> Vec<String> {
    let served: Atom<Vec<(String, u64, Result<(), String>)>> = Atom::default();
    let backfilled: Atom<Vec<Backfilled>> = Atom::default();
    let lanes = usize::try_from(world.capacity).expect("a capacity fits in usize");
    let mut tasks: Vec<Task<'_>> = Vec::with_capacity(world.plain.len() + lanes);
    let (scopes, clock) = (&world.scopes, &world.clock);
    for _ in 0..lanes {
        let backfilled = &backfilled;
        tasks.push(Box::pin(async move {
            let done = scopes.backfill().await;
            backfilled.update(|backfilled| backfilled.push(done.clone()));
        }));
    }
    for name in &world.plain {
        let served = &served;
        tasks.push(Box::pin(async move {
            nanocode_platform::sleep(Duration::from_millis(HEAD_START_MS)).await;
            let started = clock.now_ns();
            let got = match scopes.get(name).await {
                Ok(Reached::Local(_)) => Ok(()),
                Ok(Reached::Routed(routed)) => Err(format!("routed to {}", routed.holder())),
                Err(error) => Err(error.to_string()),
            };
            let ns = clock.now_ns() - started;
            served.update(|served| served.push((name.clone(), ns, got.clone())));
        }));
    }
    let mut wrong: Vec<String> = ran(world, rng, tasks, "a backfill beside serving gets")
        .into_iter()
        .collect();
    let bound_ns = (READ_MS + READ_MS * READS_PER_SERVED_GET) * NANOS_PER_MS;
    for (name, ns, got) in served.read(Clone::clone) {
        stats.served += 1;
        stats.served_ns_max = stats.served_ns_max.max(ns);
        if let Err(error) = got {
            wrong.push(format!(
                "backfill: a serving get of {name} beside a backfill answered {error}"
            ));
        }
        if ns > bound_ns {
            wrong.push(format!(
                "backfill: a serving get of {name} answered after {ns} ns beside a backfill, past one backfill read and its own load ({bound_ns} ns)"
            ));
        }
    }
    let done = backfilled.read(Clone::clone);
    if done.len() != lanes {
        wrong.push(format!(
            "backfill: {} of {lanes} backfill calls answered",
            done.len()
        ));
    }
    let adopted: u64 = done.iter().map(|done| done.adopted).sum();
    stats.adopted += adopted;
    if let Some((scope, error)) = done.iter().flat_map(|done| &done.failed).next() {
        wrong.push(format!("backfill: the backfill of {scope} failed: {error}"));
    }
    if adopted != world.old.len() as u64 {
        wrong.push(format!(
            "backfill: {lanes} backfill calls adopted {adopted} of {} queued scopes",
            world.old.len()
        ));
    }
    wrong
}

fn adopted(world: &World) -> Vec<String> {
    let mut wrong = Vec::new();
    for (name, sum) in &world.old {
        match block_on(world.scopes.get(name)) {
            Ok(Reached::Local(scope)) if *scope.state() == *sum => {}
            Ok(Reached::Local(scope)) => wrong.push(format!(
                "backfill: {name} holds {} after its backfill, not the {sum} its old rows sum to",
                scope.state()
            )),
            Ok(Reached::Routed(routed)) => {
                wrong.push(format!(
                    "backfill: {name} routes to {} after its backfill",
                    routed.holder()
                ));
            }
            Err(error) => wrong.push(format!(
                "backfill: a get of {name} after its backfill: {error}"
            )),
        }
    }
    wrong
}

struct Load {
    latencies: Vec<u64>,
    adopted: u64,
    lanes_max: u64,
    wrong: Vec<String>,
}

fn served(world: &World, rng: &mut Rng, backfill: bool) -> Load {
    let workers = world.capacity;
    let schedules: Vec<Vec<u64>> = (0..workers)
        .map(|_| {
            (0..rng.range(LOAD_READS.start, LOAD_READS.end))
                .map(|_| rng.below(READ_MS * NANOS_PER_MS))
                .collect()
        })
        .collect();
    let latencies: Atom<Vec<u64>> = Atom::default();
    let failures: Atom<Vec<String>> = Atom::default();
    let adopted = AtomicU64::new(0);
    let lanes_max = AtomicU64::new(0);
    let left = AtomicU64::new(workers);
    let (scopes, clock, history) = (&world.scopes, &world.clock, world.scopes.history());
    let mut tasks: Vec<Task<'_>> = Vec::new();
    for (at, schedule) in schedules.iter().enumerate() {
        let name = world.plain[at % world.plain.len()].clone();
        let (latencies, failures, left, history) =
            (&latencies, &failures, &left, Arc::clone(&history));
        tasks.push(Box::pin(async move {
            for think in schedule {
                nanocode_platform::sleep(Duration::from_nanos(*think)).await;
                let started = clock.now_ns();
                if let Err(error) = history.scan(&name, 0, SCAN_END).await {
                    failures.update(|failures| {
                        failures.push(format!(
                            "backfill: a serving read of {name} under load answered {error}"
                        ))
                    });
                }
                let ns = clock.now_ns() - started;
                latencies.update(|latencies| latencies.push(ns));
            }
            left.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    if backfill {
        for _ in 0..workers {
            let (adopted, lanes_max, left) = (&adopted, &lanes_max, &left);
            tasks.push(Box::pin(async move {
                while left.load(Ordering::SeqCst) > 0 {
                    let done = scopes.backfill().await;
                    adopted.fetch_add(done.adopted, Ordering::SeqCst);
                    lanes_max.fetch_max(scopes.stats().backfill_lanes, Ordering::SeqCst);
                    if done.adopted + done.resumed + done.current + done.dropped == 0 {
                        nanocode_platform::sleep(Duration::from_millis(READ_MS)).await;
                    }
                }
            }));
        }
    }
    let mut wrong: Vec<String> = ran(world, rng, tasks, "serving reads under load")
        .into_iter()
        .collect();
    wrong.extend(failures.read(Clone::clone));
    let mut latencies = latencies.read(Clone::clone);
    latencies.sort_unstable();
    Load {
        latencies,
        adopted: adopted.load(Ordering::SeqCst),
        lanes_max: lanes_max.load(Ordering::SeqCst),
        wrong,
    }
}

fn percentile(latencies: &[u64]) -> u64 {
    let Some(last) = latencies.len().checked_sub(1) else {
        return 0;
    };
    let at = usize::try_from(u64::try_from(last).unwrap_or(u64::MAX) * PERCENTILE / PERCENT)
        .unwrap_or(last);
    latencies.get(at).copied().unwrap_or(0)
}

fn loaded(rng: &mut Rng, clock: &Arc<Clock>, stats: &mut BackfillStats) -> Vec<String> {
    let seed = rng.below(u64::MAX);
    let quiet_world = world(&mut Rng::derive(seed, "load world"), clock);
    let busy_world = world(&mut Rng::derive(seed, "load world"), clock);
    let mut wrong = queued(&busy_world, rng);
    let quiet = served(&quiet_world, &mut Rng::derive(seed, "load"), false);
    let busy = served(&busy_world, &mut Rng::derive(seed, "load"), true);
    wrong.extend(quiet.wrong);
    wrong.extend(busy.wrong);
    let (without, with) = (percentile(&quiet.latencies), percentile(&busy.latencies));
    stats.load_runs += 1;
    stats.load_adopted += busy.adopted;
    stats.load_lanes_max = stats.load_lanes_max.max(busy.lanes_max);
    stats.load_p99_ns_max = stats.load_p99_ns_max.max(with);
    if with > without {
        wrong.push(format!(
            "backfill: serving p{PERCENTILE} under load is {with} ns beside a backfill and {without} ns without one, over a host of read capacity {}",
            busy_world.capacity
        ));
    }
    wrong
}

pub fn run(rng: &mut Rng, clock: &Arc<Clock>, stats: &mut BackfillStats) -> Vec<String> {
    let world = world(rng, clock);
    stats.runs += 1;
    let mut wrong = queued(&world, rng);
    wrong.extend(raced(&world, rng, stats));
    let in_flight = world.paced.in_flight_max.load(Ordering::Relaxed);
    stats.in_flight_max = stats.in_flight_max.max(in_flight);
    if in_flight > world.capacity {
        wrong.push(format!(
            "I17: {in_flight} reads in flight at once beside a backfill over a host of read capacity {}",
            world.capacity
        ));
    }
    wrong.extend(adopted(&world));
    wrong.extend(loaded(rng, clock, stats));
    wrong
}

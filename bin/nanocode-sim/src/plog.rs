//! The one partitioned log host (nanocode_turnlog::parted) under nodes that claim, lose and
//! retake partitions, concurrent appenders that collide on seqs and share group commits, lost
//! acknowledgements, serialization failures, followers with cursors and retention.
//!
//! Canaries (one per seeded defect): one owner per partition at any fence, no duplicate or gap in
//! a scope's seqs, all-or-nothing appends, a ring for every committed append and a follower that
//! sees every committed row, a snapshot plus its tail folds like the whole log, retention never
//! drops a row a cursor still needs, and an uncertain append is checked, never re-run.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    num::NonZeroU32,
    ops::Range,
    pin::Pin,
    process::ExitCode,
    sync::Arc,
    task::{Context, Poll},
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::Partition;
use nanocode_sync::{Atom, OrdSet, Semaphore, Signal};
use nanocode_turnlog::{
    LogHost,
    parted::{Entry, Fault, ModelParts, PartStore, PartedLog, sim as parted_sim},
};

use crate::rng::{FNV_OFFSET, Rng, fnv};

pub use parted_sim::Mutation as PlogMutation;

const LEASE_TICKS_MIN: u64 = 8;
const RECOVERIES_AT_ONCE: usize = 1;
const LEASE_TICKS_MAX: u64 = 96;
const HANDOFF_TTL_TICKS: u64 = u64::MAX / 4;
const HANDOFF_APPENDS_MIN: u64 = 2;
const HANDOFF_APPENDS_MAX: u64 = 6;

pub fn names() -> String {
    PlogMutation::ALL
        .iter()
        .map(|mutation| mutation.name())
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlogStats {
    pub seeds: u64,
    pub appends: u64,
    pub acked: u64,
    pub uncertain: u64,
    pub conflicts: u64,
    pub fenced: u64,
    pub claims: u64,
    pub commits: u64,
    pub grouped: u64,
    pub before: u64,
    pub after: u64,
    pub pruned: u64,
    pub followed: u64,
    pub drained: u64,
    pub busy: u64,
}

impl PlogStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.appends += other.appends;
        self.acked += other.acked;
        self.uncertain += other.uncertain;
        self.conflicts += other.conflicts;
        self.fenced += other.fenced;
        self.claims += other.claims;
        self.commits += other.commits;
        self.grouped += other.grouped;
        self.before += other.before;
        self.after += other.after;
        self.pruned += other.pruned;
        self.followed += other.followed;
        self.drained += other.drained;
        self.busy += other.busy;
    }
}

struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

type Rung = Arc<Atom<OrdSet<(String, u64)>>>;
type Task<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Polls the tasks in a seeded random order until all finish; every wait in this scenario is a
/// yield, so a noop waker is enough.
fn drive(rng: &mut Rng, mut tasks: Vec<Task<'_>>, limit: u64) -> Result<(), String> {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut steps = 0;
    while !tasks.is_empty() {
        steps += 1;
        if steps > limit {
            return Err(format!(
                "stuck: {} tasks left after {limit} polls",
                tasks.len()
            ));
        }
        let index = rng.pick(tasks.len());
        if tasks[index].as_mut().poll(&mut context).is_ready() {
            drop(tasks.swap_remove(index));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ack {
    Committed,
    Uncertain,
}

#[derive(Clone, Debug)]
struct Step {
    id: String,
    scope: String,
    rows: usize,
    ack: Ack,
    head: u64,
    node: usize,
}

#[derive(Default)]
struct Ledger {
    steps: Vec<Step>,
    rung: Vec<BTreeSet<(String, u64)>>,
    seen: BTreeMap<(String, u64), String>,
    wrong: Vec<String>,
    stats: PlogStats,
}

struct Shape {
    parts: u32,
    nodes: usize,
    scopes: Vec<String>,
    workers: usize,
    ops: usize,
    before_ppm: u64,
    after_ppm: u64,
    max_rows: usize,
}

impl Shape {
    fn of(rng: &mut Rng) -> Self {
        let parts = u32::try_from(rng.range(1, 5)).unwrap_or(1);
        let nodes = rng.range(2, 5) as usize;
        let scopes = (0..rng.range(2, 7))
            .map(|index| match index % 3 {
                0 => format!("board/p{}", index % u64::from(parts)),
                1 => format!("session:{index}"),
                _ => format!("records/0000000{index}-0000-0000-0000-00000000000{index}"),
            })
            .collect();
        Self {
            parts,
            nodes,
            scopes,
            workers: rng.range(1, 4) as usize,
            ops: rng.range(8, 40) as usize,
            before_ppm: rng.below(60_000),
            after_ppm: rng.below(60_000),
            max_rows: rng.range(1, 9) as usize,
        }
    }

    fn describe(&self) -> String {
        format!(
            "parts={} nodes={} scopes={} workers={} ops={} before={} after={} max_rows={}",
            self.parts,
            self.nodes,
            self.scopes.len(),
            self.workers,
            self.ops,
            self.before_ppm,
            self.after_ppm,
            self.max_rows
        )
    }
}

fn fold(rows: &[(u64, String)]) -> Result<i64, String> {
    let mut state = 0_i64;
    for (seq, body) in rows {
        if let Some(value) = body.strip_prefix('s') {
            state = value
                .parse()
                .map_err(|_| format!("bad snapshot {seq} {body}"))?;
        } else if let Some((_, value)) = body.rsplit_once(':') {
            state += value
                .parse::<i64>()
                .map_err(|_| format!("bad delta {seq} {body}"))?;
        }
    }
    Ok(state)
}

fn step_of(body: &str) -> Option<(&str, usize)> {
    let delta = body.strip_prefix('d')?;
    let (id, rest) = delta.split_once('#')?;
    let (index, _) = rest.split_once(':')?;
    Some((id, index.parse().ok()?))
}

struct Node {
    log: PartedLog<ModelParts>,
}

async fn head(log: &PartedLog<ModelParts>, scope: &str) -> Result<u64, StoreError> {
    let rows = log.scan(scope, 0, i64::MAX as u64).await?;
    Ok(rows.last().map_or(0, |(seq, _)| seq + 1))
}

async fn append_step(
    node: &Node,
    index: usize,
    scope: &str,
    id: String,
    count: usize,
    value: i64,
    ledger: &RefCell<Ledger>,
) {
    let mut tries = 0;
    loop {
        tries += 1;
        let Ok(at) = head(&node.log, scope).await else {
            return;
        };
        let rows: Vec<(u64, String)> = (0..count)
            .map(|row| (at + row as u64, format!("d{id}#{row}:{value}")))
            .collect();
        let last = at + count as u64 - 1;
        let answer = node.log.append(scope, &rows).await;
        let mut ledger = ledger.borrow_mut();
        ledger.stats.appends += 1;
        let step = |ack| Step {
            id: id.clone(),
            scope: scope.to_owned(),
            rows: count,
            ack,
            head: last,
            node: index,
        };
        match answer {
            Ok(()) => {
                ledger.stats.acked += 1;
                ledger.steps.push(step(Ack::Committed));
                return;
            }
            Err(StoreError::Fenced) => {
                ledger.stats.conflicts += 1;
                if tries >= 3 || node.log.held(node.log.partition(scope)).is_none() {
                    return;
                }
            }
            Err(_) => {
                ledger.stats.uncertain += 1;
                ledger.steps.push(step(Ack::Uncertain));
                return;
            }
        }
    }
}

async fn snapshot(node: &Node, scope: &str) {
    let Ok(rows) = node.log.scan(scope, 0, i64::MAX as u64).await else {
        return;
    };
    let Ok(loaded) = node.log.load(scope).await else {
        return;
    };
    if rows.first().is_some_and(|(seq, _)| *seq > 0)
        && loaded
            .first()
            .is_none_or(|(_, body)| !body.starts_with('s'))
    {
        return;
    }
    let base = if rows.first().is_some_and(|(seq, _)| *seq > 0) {
        &loaded
    } else {
        &rows
    };
    let Ok(state) = fold(base) else {
        return;
    };
    let at = rows.last().map_or(0, |(seq, _)| seq + 1);
    let _ = node.log.append(scope, &[(at, format!("s{state}"))]).await;
}

async fn worker(
    shape: &Shape,
    nodes: &[Node],
    index: usize,
    worker: usize,
    seed: u64,
    ledger: &RefCell<Ledger>,
) {
    let mut rng = Rng::derive(seed, &format!("plog worker {index}/{worker}"));
    let node = &nodes[index];
    for op in 0..shape.ops {
        let scope = &shape.scopes[rng.pick(shape.scopes.len())];
        let part = node.log.partition(scope);
        match rng.below(100) {
            0..=9 => {
                let ttl = rng.range(LEASE_TICKS_MIN, LEASE_TICKS_MAX + 1);
                if node.log.claim(part, ttl).await.is_ok() {
                    ledger.borrow_mut().stats.claims += 1;
                }
            }
            10..=11 => {
                let ttl = rng.range(LEASE_TICKS_MIN, LEASE_TICKS_MAX + 1);
                let _ = node.log.renew(part, ttl).await;
            }
            12 => {
                let ttl = rng.range(LEASE_TICKS_MIN, LEASE_TICKS_MAX + 1);
                let _ = node.log.renew_all(ttl).await;
            }
            13..=14 => {
                let _ = node.log.release(part).await;
            }
            15..=19 => snapshot(node, scope).await,
            20..=23 => {
                if let Ok(rows) = node.log.scan(scope, 0, i64::MAX as u64).await
                    && let Some((last, _)) = rows.last()
                    && let Ok(dropped) = node.log.prune(scope, *last).await
                {
                    ledger.borrow_mut().stats.pruned += dropped;
                }
            }
            24..=29 => follow(node, index, part, ledger).await,
            _ => {
                let count = rng.range(1, shape.max_rows as u64 + 1) as usize;
                let value = rng.range(1, 100) as i64;
                let id = format!("{index}.{worker}.{op}");
                append_step(node, index, scope, id, count, value, ledger).await;
            }
        }
    }
}

/// A planned handoff with nothing else running: finish any earlier drain, claim far ahead (so
/// neither expiry nor a peer can refuse a write), queue appends, then release. Every append
/// queued before the release must flush at the held epoch: only a moved epoch may refuse one.
async fn handoff(
    node: &Node,
    index: usize,
    part: Partition,
    appends: u64,
    ledger: &RefCell<Ledger>,
) {
    let _ = node.log.release(part).await;
    if node.log.claim(part, HANDOFF_TTL_TICKS).await.is_err() {
        return;
    }
    let Some(epoch) = node.log.held(part) else {
        return;
    };
    let scope = format!("handoff/{index}/p{}", part.0);
    assert_eq!(
        node.log.partition(&scope),
        part,
        "a /p<n> scope lands on partition n"
    );
    let rows: Vec<Vec<(u64, String)>> = (0..appends)
        .map(|seq| vec![(seq, format!("dh{index}.{}.{seq}#0:1", part.0))])
        .collect();
    let mut queued: Vec<_> = rows
        .iter()
        .map(|rows| node.log.append(&scope, rows))
        .collect();
    let mut answers: Vec<Option<Result<(), StoreError>>> = vec![None; rows.len()];
    let mut release = Box::pin(node.log.release(part));
    let mut released = None;
    let mut admitted = false;
    std::future::poll_fn(|context| {
        if admitted
            && released.is_none()
            && let Poll::Ready(result) = release.as_mut().poll(context)
        {
            released = Some(result);
        }
        for (append, answer) in queued.iter_mut().zip(&mut answers) {
            if answer.is_none()
                && let Poll::Ready(result) = append.as_mut().poll(context)
            {
                *answer = Some(result);
            }
        }
        admitted = true;
        if released.is_some() && answers.iter().all(Option::is_some) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    let moved = node.log.store().fence_row(part).map(|row| row.epoch()) != Some(epoch);
    let mut ledger = ledger.borrow_mut();
    for (seq, answer) in answers.into_iter().flatten().enumerate() {
        let step = |ack| Step {
            id: format!("h{index}.{}.{seq}", part.0),
            scope: scope.clone(),
            rows: 1,
            ack,
            head: seq as u64,
            node: index,
        };
        ledger.stats.appends += 1;
        match answer {
            Ok(()) => {
                ledger.stats.acked += 1;
                ledger.stats.drained += 1;
                ledger.steps.push(step(Ack::Committed));
            }
            Err(StoreError::Fenced) if !moved => ledger.wrong.push(format!(
                "node {index} drained partition {} at epoch {epoch} and refused append {seq}, queued before the release",
                part.0
            )),
            Err(StoreError::Fenced) => ledger.stats.conflicts += 1,
            Err(_) => {
                ledger.stats.uncertain += 1;
                ledger.steps.push(step(Ack::Uncertain));
            }
        }
    }
}

/// Recovery capacity under contention: one gain holds the only permit while its recovery waits,
/// so a second gain must answer Busy when its deadline passes, never wait on for the permit. Then
/// the first releases what it gained and another node gains it at the cursor the release
/// recorded.
async fn gains(nodes: &[Node], part: Partition, patience: u64, ledger: &RefCell<Ledger>) {
    let capacity = Semaphore::new(RECOVERIES_AT_ONCE);
    let parked = Signal::new();
    let (first, second) = (&nodes[0], &nodes[1]);
    let mut holding = Box::pin(first.log.gain(
        part,
        HANDOFF_TTL_TICKS,
        &capacity,
        std::future::pending(),
        |_| async {
            parked.wait().await;
            Ok(())
        },
    ));
    let deadline = async move {
        for _ in 0..patience {
            Yield(false).await;
        }
    };
    let mut waiting = Box::pin(second.log.gain(
        part,
        HANDOFF_TTL_TICKS,
        &capacity,
        deadline,
        |_| async { Ok(()) },
    ));
    let (mut held, mut refused) = (None, None);
    std::future::poll_fn(|context| {
        if held.is_none()
            && let Poll::Ready(answer) = holding.as_mut().poll(context)
        {
            held = Some(answer);
        }
        if refused.is_none()
            && let Poll::Ready(answer) = waiting.as_mut().poll(context)
        {
            refused = Some((answer, held.is_none()));
            parked.fire();
        }
        if held.is_some() && refused.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    match refused {
        Some((Err(nanocode_turnlog::parted::GainError::Busy), _)) => ledger.borrow_mut().stats.busy += 1,
        Some((answer, true)) => ledger.borrow_mut().wrong.push(format!(
            "a gain of partition {} answered {answer:?} while another gain held the only recovery permit",
            part.0
        )),
        _ => {}
    }
    if !matches!(held, Some(Ok(_))) || first.log.release(part).await.is_err() {
        return;
    }
    let committed = first
        .log
        .store()
        .rows(part)
        .iter()
        .map(|(_, _, stored)| stored.pos)
        .max()
        .unwrap_or(0);
    if let Ok((gained, ())) = second
        .log
        .gain(
            part,
            HANDOFF_TTL_TICKS,
            &capacity,
            std::future::pending(),
            |_| async { Ok(()) },
        )
        .await
        && gained.cursor < committed
    {
        ledger.borrow_mut().wrong.push(format!(
            "partition {} was released at position {committed} and gained at cursor {}",
            part.0, gained.cursor
        ));
    }
}

async fn follow(node: &Node, index: usize, part: Partition, ledger: &RefCell<Ledger>) {
    let reader = format!("reader{index}");
    let mut after = node.log.store().cursor_of(part, &reader);
    loop {
        let Ok(entries) = node.log.follow(part, after, 16).await else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        record(&mut after, index, &entries, ledger);
        if node.log.cursor(part, &reader, after).await.is_err() {
            return;
        }
    }
}

fn record(after: &mut u64, index: usize, entries: &[Entry], ledger: &RefCell<Ledger>) {
    let mut ledger = ledger.borrow_mut();
    for entry in entries {
        if entry.pos <= *after {
            ledger.wrong.push(format!(
                "follower {index} went back from pos {} to {}",
                *after, entry.pos
            ));
        }
        *after = entry.pos;
        ledger.stats.followed += 1;
        if let Some(body) = ledger.seen.get(&(entry.scope.to_string(), entry.seq))
            && *body != entry.body
        {
            let message = format!(
                "{} seq {} read as {body} then {}",
                entry.scope, entry.seq, entry.body
            );
            ledger.wrong.push(message);
        }
        ledger
            .seen
            .insert((entry.scope.to_string(), entry.seq), entry.body.clone());
    }
}

fn run(seed: u64, mutation: PlogMutation, stats: &mut PlogStats) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, "plog");
    let shape = Shape::of(&mut rng);
    let count = NonZeroU32::new(shape.parts).ok_or("no partitions")?;
    let faults_rng = Arc::new(Atom::new(Rng::derive(seed, "plog faults")));
    let pause_rng = Arc::new(Atom::new(Rng::derive(seed, "plog pauses")));
    let (before, after) = (shape.before_ppm, shape.after_ppm);
    let model = Arc::new(
        ModelParts::new()
            .with_faults(Arc::new(move |op| {
                if !matches!(op, "commit" | "claim" | "release") {
                    return Fault::None;
                }
                faults_rng.update(|rng| {
                    if rng.chance(before) {
                        Fault::Before
                    } else if rng.chance(after) {
                        Fault::After
                    } else {
                        Fault::None
                    }
                })
            }))
            .with_sleep(Arc::new(move || -> StoreFuture<'static, ()> {
                let yields = pause_rng.update(|rng| rng.below(3));
                Box::pin(async move {
                    for _ in 0..yields {
                        Yield(false).await;
                    }
                })
            })),
    );
    nanocode_sync::block_on(model.migrate(count)).map_err(|error| error.to_string())?;
    nanocode_sync::block_on(model.prepare(count)).map_err(|error| error.to_string())?;
    let ledger = RefCell::new(Ledger {
        rung: vec![BTreeSet::new(); shape.nodes],
        ..Ledger::default()
    });
    let rungs: Vec<Rung> = (0..shape.nodes).map(|_| Arc::default()).collect();
    let nodes: Vec<Node> = (0..shape.nodes)
        .map(|index| {
            let clock = Arc::clone(&model);
            let log = PartedLog::fenced(Arc::clone(&model), count, &format!("node{index}"))
                .with_clock(Arc::new(move || clock.now()))
                .with_max_rows(shape.max_rows * 3);
            let rung = Arc::clone(&rungs[index]);
            log.set_ring(Arc::new(move |scope, head| {
                rung.update(|rung| rung.insert((scope.to_owned(), head)));
            }));
            Node { log }
        })
        .collect();
    for index in 0..shape.nodes {
        for part in 0..shape.parts {
            let reader = format!("reader{index}");
            nanocode_sync::block_on(model.cursor(Partition(part), &reader, 0))
                .map_err(|error| error.to_string())?;
        }
    }
    parted_sim::mutate(mutation);
    let tasks: Vec<Task<'_>> = (0..shape.nodes)
        .flat_map(|index| (0..shape.workers).map(move |work| (index, work)))
        .map(|(index, work)| -> Task<'_> {
            Box::pin(worker(&shape, &nodes, index, work, seed, &ledger))
        })
        .collect();
    let driven = drive(&mut rng, tasks, 2_000_000);
    let handoffs: Vec<(usize, Partition, u64)> = (0..shape.nodes)
        .flat_map(|index| (0..shape.parts).map(move |part| (index, Partition(part))))
        .map(|(index, part)| {
            let appends = rng.range(HANDOFF_APPENDS_MIN, HANDOFF_APPENDS_MAX + 1);
            (index, part, appends)
        })
        .collect();
    let gained = Partition(u32::try_from(rng.below(u64::from(shape.parts))).unwrap_or(0));
    let patience = rng.range(1, 9);
    let planned: Vec<Task<'_>> = vec![Box::pin(async {
        for (index, part, appends) in &handoffs {
            handoff(&nodes[*index], *index, *part, *appends, &ledger).await;
        }
        gains(&nodes, gained, patience, &ledger).await;
    })];
    let handed = drive(&mut rng, planned, 2_000_000);
    let calm: Vec<Task<'_>> = (0..shape.nodes)
        .flat_map(|index| (0..shape.parts).map(move |part| (index, part)))
        .map(|(index, part)| -> Task<'_> {
            Box::pin(follow(&nodes[index], index, Partition(part), &ledger))
        })
        .collect();
    let drained = drive(&mut rng, calm, 2_000_000);
    parted_sim::mutate(PlogMutation::None);
    driven?;
    handed?;
    drained?;
    let mut ledger = ledger.into_inner();
    for (index, rung) in rungs.iter().enumerate() {
        ledger.rung[index] = rung.read(|rung| rung.iter().cloned().collect());
    }
    let model_stats = model.stats();
    for node in &nodes {
        let log_stats = node.log.stats();
        ledger.stats.commits += log_stats.commits;
        ledger.stats.grouped += log_stats.appends.saturating_sub(log_stats.commits);
        ledger.stats.fenced += log_stats.fenced;
    }
    ledger.stats.before = model_stats.before;
    ledger.stats.after = model_stats.after;
    *stats = ledger.stats;
    stats.seeds = 1;
    parted_sim::mutate(mutation);
    let verdict = check(
        &shape,
        &model,
        &nodes,
        &ledger,
        model_stats.unique_violations,
    );
    parted_sim::mutate(PlogMutation::None);
    verdict
}

fn check(
    shape: &Shape,
    model: &ModelParts,
    nodes: &[Node],
    ledger: &Ledger,
    unique_violations: u64,
) -> Result<u64, String> {
    if let Some(wrong) = ledger.wrong.first() {
        return Err(wrong.clone());
    }
    if unique_violations > 0 {
        return Err(format!(
            "{unique_violations} batches hit a unique violation: one batch carried a seq twice"
        ));
    }
    let mut present: BTreeMap<(String, u64), String> = BTreeMap::new();
    for part in 0..shape.parts {
        for (scope, seq, stored) in model.rows(Partition(part)) {
            if let Some(epoch) = stored.epoch
                && epoch != stored.fence
            {
                return Err(format!(
                    "{scope} seq {seq} was written with fence epoch {epoch} while the partition was at {}: two owners",
                    stored.fence
                ));
            }
            if let Some(seen) = ledger.seen.get(&(scope.to_string(), seq))
                && *seen != stored.body
            {
                return Err(format!(
                    "{scope} seq {seq} was followed as {seen} but holds {}: a seq was written twice",
                    stored.body
                ));
            }
            present.insert((scope.to_string(), seq), stored.body);
        }
    }
    let mut all = ledger.seen.clone();
    all.extend(present.clone());
    let mut by_step: HashMap<&str, BTreeSet<usize>> = HashMap::new();
    let mut seqs_of_step: HashMap<(&str, usize), usize> = HashMap::new();
    for body in all.values() {
        if let Some((id, row)) = step_of(body) {
            by_step.entry(id).or_default().insert(row);
            *seqs_of_step.entry((id, row)).or_default() += 1;
        }
    }
    if let Some(((id, row), times)) = seqs_of_step.iter().find(|(_, times)| **times > 1) {
        return Err(format!(
            "step {id} row {row} is in the log {times} times: an append was re-run"
        ));
    }
    for step in &ledger.steps {
        let rows = by_step.get(step.id.as_str()).map_or(0, BTreeSet::len);
        match step.ack {
            Ack::Committed if rows != step.rows => {
                return Err(format!(
                    "acknowledged step {} on {} has {rows} of its {} rows",
                    step.id, step.scope, step.rows
                ));
            }
            Ack::Uncertain if rows != 0 && rows != step.rows => {
                return Err(format!(
                    "uncertain step {} on {} landed {rows} of its {} rows",
                    step.id, step.scope, step.rows
                ));
            }
            _ => {}
        }
        if step.ack == Ack::Committed
            && !ledger.rung[step.node].contains(&(step.scope.clone(), step.head))
        {
            return Err(format!(
                "node {} committed {} through seq {} and rang no watcher",
                step.node, step.scope, step.head
            ));
        }
    }
    for ((scope, seq), body) in &all {
        if !ledger.seen.contains_key(&(scope.clone(), *seq)) {
            return Err(format!(
                "{scope} seq {seq} ({body}) was committed and no follower saw it"
            ));
        }
    }
    let mut digest = FNV_OFFSET;
    for scope in &shape.scopes {
        let rows: Vec<(u64, String)> = all
            .iter()
            .filter(|((row_scope, _), _)| row_scope == scope)
            .map(|((_, seq), body)| (*seq, body.clone()))
            .collect();
        for pair in rows.windows(2) {
            if pair[1].0 != pair[0].0 + 1 {
                return Err(format!(
                    "{scope} jumps from seq {} to {}",
                    pair[0].0, pair[1].0
                ));
            }
        }
        let whole = fold(&rows)?;
        let loaded =
            nanocode_sync::block_on(nodes[0].log.load(scope)).map_err(|error| error.to_string())?;
        let from_snapshot = fold(&loaded)?;
        if loaded
            .first()
            .is_some_and(|(seq, _)| rows.first().is_some_and(|(first, _)| seq != first))
            && loaded
                .first()
                .is_none_or(|(_, body)| !body.starts_with('s'))
        {
            return Err(format!(
                "{scope} loads from seq {:?} without its snapshot",
                loaded.first().map(|(seq, _)| seq)
            ));
        }
        if whole != from_snapshot {
            return Err(format!(
                "{scope}: snapshot plus tail folds to {from_snapshot}, the whole log to {whole}"
            ));
        }
        digest = fnv(digest, format!("{scope}={whole};").as_bytes());
    }
    Ok(fnv(digest, format!("{}", all.len()).as_bytes()))
}

pub struct PlogOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: PlogStats,
    shape: String,
}

pub fn simulate(seed: u64, mutation: PlogMutation) -> PlogOutcome {
    let mut rng = Rng::derive(seed, "plog");
    let shape = Shape::of(&mut rng).describe();
    let mut stats = PlogStats::default();
    let (digest, wrong) = match run(seed, mutation, &mut stats) {
        Ok(digest) => (digest, None),
        Err(message) => (0, Some(message)),
    };
    PlogOutcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: PlogMutation) -> PlogOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        parted_sim::mutate(PlogMutation::None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        PlogOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: PlogStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: PlogMutation,
    twice: bool,
    verbose: bool,
    totals: &mut PlogStats,
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

pub fn replay(seed: u64, mutation: PlogMutation) -> String {
    let mut command = format!("nanocode-sim --scenario plog --seed {seed}");
    if mutation != PlogMutation::None {
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
    mutation: PlogMutation,
    totals: &PlogStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=plog mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        PlogMutation::None if failed == 0 => ExitCode::SUCCESS,
        PlogMutation::None => ExitCode::FAILURE,
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

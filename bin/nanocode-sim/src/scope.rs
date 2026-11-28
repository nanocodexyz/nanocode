use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    ops::Range,
    pin::Pin,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::{Partition, Row};
use nanocode_scope::{
    Carry, Checkpoints, Committed, Fence, Flushed, Listing, Manifest, MemoryCheckpoints, Part,
    PartedLeases, Policy, Reached, Resident, Route, Routed, Scope, ScopeError, Scopes, Stats,
    Wiring, Work, Written,
    sim::{self as scope_sim, Mutation},
};
use nanocode_sync::{Atom, Signal, block_on};

use nanocode_turnlog::{
    LogHost, Rider, SCAN_END, Write,
    parted::{ModelParts, PartStore, PartedLog},
};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const PARTS: u32 = 2;
const SCOPES: Range<u64> = 3..7;
const NODES: Range<u64> = 2..4;
const STEPS: Range<u64> = 150..400;
const BUCKETS: u8 = 4;
const KEEP: usize = 4;
const ENTRY_BYTES: usize = 16;
const TTL_MS: u64 = 1_000;
const GRACE_MS: u64 = 300;
const MARGIN_MS: u64 = 100;
const LAPSE_MS: u64 = TTL_MS + GRACE_MS + MARGIN_MS;
const TICK_MS_MAX: u64 = 120;
const START_MS: u64 = 1_000_000;
const CHECKPOINT_ROWS: Range<u64> = 3..10;
const CHECKPOINT_MS: u64 = 4_000;
const RESIDENT_BYTES: Range<u64> = 200..700;
const FORWARD_MS: u64 = 50;
const ADMIT_MS: u64 = TTL_MS;
const WATCHDOG_MS: u64 = 1_000;
const READ_WAIT_MS: u64 = 1_000;
const VALUES_MAX: u64 = 3;
const RENEW_PPM: u64 = 300_000;
const CRASH_PPM: u64 = 20_000;
const PAUSE_PPM: u64 = 20_000;
const UNPAUSE_PPM: u64 = 15_000;
const RELEASE_PPM: u64 = 40_000;
const STALL_PPM: u64 = 80_000;
const TICK_PPM: u64 = 50_000;
const STALE_PPM: u64 = 120_000;
const ABANDON_PPM: u64 = 300_000;
const OVERSIZE_PPM: u64 = 20_000;
const DOWN_ROUTE_PPM: u64 = 200_000;
const BURST_PPM: u64 = 60_000;
const BURST_SENDS: Range<u64> = 3..7;
const STAGE_PPM: u64 = 80_000;
const STAGES: Range<u64> = 1..4;
const REPLACE_PPM: u64 = 10_000;
const STALLS_BEFORE_REPLACE: u64 = 3;
const RIDER_HOST: usize = 7;
const RIDER_SQL: &str = "UPDATE rider SET seen = $1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeMutation(Mutation);

impl ScopeMutation {
    pub const NONE: Self = Self(Mutation::None);

    pub fn parse(name: &str) -> Option<Self> {
        Mutation::parse(name).map(Self)
    }

    pub const fn name(self) -> &'static str {
        self.0.name()
    }

    pub fn names() -> String {
        Mutation::ALL
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScopeStats {
    pub seeds: u64,
    pub steps: u64,
    pub sends: u64,
    pub committed: u64,
    pub routed: u64,
    pub refused: u64,
    pub stalls: u64,
    pub abandoned: u64,
    pub crashes: u64,
    pub pauses: u64,
    pub releases: u64,
    pub stale_plays: u64,
    pub departures_checked: u64,
    pub loads: u64,
    pub load_rows: u64,
    pub load_rows_max: u64,
    pub checkpoints: u64,
    pub checkpoint_parts: u64,
    pub checkpoint_parts_kept: u64,
    pub checkpoint_part_bytes: u64,
    pub checkpoints_stale: u64,
    pub evictions: u64,
    pub forwards: u64,
    pub forwards_unreachable: u64,
    pub forward_ms_max: u64,
    pub resident_bytes_max: u64,
    pub races: u64,
    pub races_refused: u64,
    pub races_missed: u64,
    pub bursts: u64,
    pub burst_sends: u64,
    pub burst_decides_checked: u64,
    pub staged: u64,
    pub flushes: u64,
    pub carried: u64,
    pub replacements: u64,
    pub release_drops: u64,
}

impl ScopeStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.steps += other.steps;
        self.sends += other.sends;
        self.committed += other.committed;
        self.routed += other.routed;
        self.refused += other.refused;
        self.stalls += other.stalls;
        self.abandoned += other.abandoned;
        self.crashes += other.crashes;
        self.pauses += other.pauses;
        self.releases += other.releases;
        self.stale_plays += other.stale_plays;
        self.departures_checked += other.departures_checked;
        self.loads += other.loads;
        self.load_rows += other.load_rows;
        self.load_rows_max = self.load_rows_max.max(other.load_rows_max);
        self.checkpoints += other.checkpoints;
        self.checkpoint_parts += other.checkpoint_parts;
        self.checkpoint_parts_kept += other.checkpoint_parts_kept;
        self.checkpoint_part_bytes += other.checkpoint_part_bytes;
        self.checkpoints_stale += other.checkpoints_stale;
        self.evictions += other.evictions;
        self.forwards += other.forwards;
        self.forwards_unreachable += other.forwards_unreachable;
        self.forward_ms_max = self.forward_ms_max.max(other.forward_ms_max);
        self.resident_bytes_max = self.resident_bytes_max.max(other.resident_bytes_max);
        self.races += other.races;
        self.races_refused += other.races_refused;
        self.races_missed += other.races_missed;
        self.bursts += other.bursts;
        self.burst_sends += other.burst_sends;
        self.burst_decides_checked += other.burst_decides_checked;
        self.staged += other.staged;
        self.flushes += other.flushes;
        self.carried += other.carried;
        self.replacements += other.replacements;
        self.release_drops += other.release_drops;
    }

    fn runtime(&mut self, stats: &Stats) {
        self.loads += stats.loads;
        self.load_rows += stats.load_rows;
        self.load_rows_max = self.load_rows_max.max(stats.load_rows_max);
        self.checkpoints += stats.checkpoints;
        self.checkpoint_parts += stats.checkpoint_parts;
        self.checkpoint_parts_kept += stats.checkpoint_parts_kept;
        self.checkpoint_part_bytes += stats.checkpoint_part_bytes;
        self.evictions += stats.evictions;
        self.forwards += stats.forwards;
        self.forwards_unreachable += stats.forwards_unreachable;
        self.forward_ms_max = self.forward_ms_max.max(stats.forward_ms_max);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Bucket {
    count: u64,
    sum: u64,
    last: Vec<(u64, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cells(BTreeMap<u8, Bucket>);

pub struct Add {
    bucket: u8,
    values: Vec<u64>,
    decided: Arc<AtomicU64>,
}

struct Buckets;

fn encoded(bucket: &Bucket) -> String {
    let mut body = format!("{} {}", bucket.count, bucket.sum);
    for (seq, value) in &bucket.last {
        body.push_str(&format!(" {seq}.{value}"));
    }
    body
}

fn decoded(body: &str) -> Option<Bucket> {
    let mut words = body.split(' ');
    let count = words.next()?.parse().ok()?;
    let sum = words.next()?.parse().ok()?;
    let mut last = Vec::new();
    for word in words {
        let (seq, value) = word.split_once('.')?;
        last.push((seq.parse().ok()?, value.parse().ok()?));
    }
    Some(Bucket { count, sum, last })
}

impl Resident for Buckets {
    type State = Cells;
    type Command = Add;
    type Reply = u64;

    fn empty(&self) -> Cells {
        Cells::default()
    }

    fn decide(&self, state: &Cells, add: &Add) -> (Vec<String>, u64) {
        add.decided.fetch_add(1, Ordering::Relaxed);
        let rows = add
            .values
            .iter()
            .map(|value| format!("{}:{value}", add.bucket))
            .collect();
        let count = state.0.get(&add.bucket).map_or(0, |bucket| bucket.count);
        (rows, count + add.values.len() as u64)
    }

    fn apply(&self, state: &mut Cells, seq: u64, row: &str) -> Result<(), StoreError> {
        let parsed = row.split_once(':').and_then(|(bucket, value)| {
            Some((bucket.parse::<u8>().ok()?, value.parse::<u64>().ok()?))
        });
        let Some((bucket, value)) = parsed else {
            return Err(StoreError::Backend(format!(
                "row {seq} {row} is not a bucket value"
            )));
        };
        let entry = state.0.entry(bucket).or_default();
        entry.count += 1;
        entry.sum += value;
        entry.last.push((seq, value));
        if entry.last.len() > KEEP {
            entry.last.remove(0);
        }
        Ok(())
    }

    fn encode(&self, state: &Cells) -> Vec<Part> {
        (0..BUCKETS)
            .map(|bucket| Part {
                name: format!("b{bucket}"),
                body: encoded(&state.0.get(&bucket).cloned().unwrap_or_default()),
            })
            .collect()
    }

    fn decode(&self, parts: Vec<Part>) -> Result<Cells, StoreError> {
        let mut cells = Cells::default();
        for part in parts {
            let bucket = part
                .name
                .strip_prefix('b')
                .and_then(|bucket| bucket.parse::<u8>().ok());
            let (Some(bucket), Some(value)) = (bucket, decoded(&part.body)) else {
                return Err(StoreError::Backend(format!(
                    "checkpoint part {} does not decode",
                    part.name
                )));
            };
            if value.count > 0 {
                cells.0.insert(bucket, value);
            }
        }
        Ok(cells)
    }

    fn weight(&self, state: &Cells) -> usize {
        state
            .0
            .values()
            .map(|bucket| ENTRY_BYTES * (bucket.last.len() + 1))
            .sum()
    }
}

type Wrong = Arc<Atom<Option<String>>>;

fn flag(wrong: &Wrong, message: String) {
    wrong.update(|wrong| {
        if wrong.is_none() {
            *wrong = Some(message.clone());
        }
    });
}

struct Witness {
    inner: MemoryCheckpoints<ModelParts>,
    store: Arc<ModelParts>,
    wrong: Wrong,
    stale: AtomicU64,
    calls: AtomicU64,
}

impl Witness {
    fn latest_seq(&self, scope: &str) -> u64 {
        self.inner
            .manifests(scope)
            .last()
            .map_or(0, |manifest| manifest.seq)
    }
}

impl Witness {
    fn listed(&self, manifest: &Manifest) {
        let Some(landed) = self.inner.manifests(&manifest.scope).last().cloned() else {
            return;
        };
        if landed.seq != manifest.seq {
            return;
        }
        let mut expected = manifest.parts.clone();
        expected.sort();
        let mut assembled = landed.parts.clone();
        assembled.sort();
        if assembled != expected {
            flag(
                &self.wrong,
                format!(
                    "listing: the checkpoint of {} at row {} assembles to {} parts, but its state encodes {}",
                    manifest.scope,
                    manifest.seq,
                    assembled.len(),
                    expected.len()
                ),
            );
        }
        let whole = Listing::Whole(landed.parts).encoded().len();
        if landed.changed_bytes > whole {
            flag(
                &self.wrong,
                format!(
                    "listing: the checkpoint of {} at row {} chains {} bytes of changes since its whole listing, more than the {whole} bytes a whole listing takes",
                    manifest.scope, manifest.seq, landed.changed_bytes
                ),
            );
        }
    }
}

impl Checkpoints for Witness {
    fn latest<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Option<Manifest>, StoreError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.latest(scope)
    }

    fn parts<'a>(
        &'a self,
        digests: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<(String, String)>, StoreError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.parts(digests)
    }

    fn write<'a>(
        &'a self,
        fence: &'a Fence,
        manifest: &'a Manifest,
        listing: &'a Listing,
        parts: &'a [(String, String)],
    ) -> StoreFuture<'a, Result<Written, StoreError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let previous = self.inner.manifests(&manifest.scope).last().cloned();
            let row = self.store.fence_row(fence.part);
            let written = self.inner.write(fence, manifest, listing, parts).await?;
            if written == Written::Stale {
                self.stale.fetch_add(1, Ordering::Relaxed);
                return Ok(written);
            }
            let live = matches!(&row, Some(Row::Held { holder, epoch, .. }) if *holder == fence.holder && *epoch == fence.epoch);
            if !live {
                flag(
                    &self.wrong,
                    format!(
                        "I11: a checkpoint of {} at row {} landed from {} at epoch {} while the lease row is {row:?}",
                        manifest.scope, manifest.seq, fence.holder, fence.epoch
                    ),
                );
            }
            self.listed(manifest);
            if let Some(previous) = previous
                && previous.seq != manifest.seq
            {
                let kept: BTreeSet<&String> =
                    previous.parts.iter().map(|(_, digest)| digest).collect();
                let resent = parts
                    .iter()
                    .filter(|(digest, _)| kept.contains(digest))
                    .count();
                if resent > 0 {
                    flag(
                        &self.wrong,
                        format!(
                            "WAL: a checkpoint of {} at row {} writes {resent} parts unchanged since its checkpoint at row {}",
                            manifest.scope, manifest.seq, previous.seq
                        ),
                    );
                }
            }
            Ok(written)
        })
    }
}

type Landed = (String, u64, Vec<(&'static str, Vec<String>)>);

#[derive(Default)]
struct Served {
    reads: [AtomicU64; 3],
    writes: [AtomicU64; 3],
}

impl Served {
    fn add(counters: &[AtomicU64; 3], rows: &[(u64, String)]) {
        let bytes: usize = rows.iter().map(|(_, body)| body.len()).sum();
        let [calls, counted, sized] = counters;
        calls.fetch_add(1, Ordering::Relaxed);
        counted.fetch_add(rows.len() as u64, Ordering::Relaxed);
        sized.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read(&self, answer: &Result<Vec<(u64, String)>, StoreError>) {
        Self::add(&self.reads, answer.as_ref().map_or(&[], Vec::as_slice));
    }

    fn work(counters: &[AtomicU64; 3]) -> (u64, u64, u64) {
        let [calls, rows, bytes] = counters;
        (
            calls.load(Ordering::Relaxed),
            rows.load(Ordering::Relaxed),
            bytes.load(Ordering::Relaxed),
        )
    }
}

const fn counted(work: Work) -> (u64, u64, u64) {
    (work.calls, work.rows, work.bytes)
}

fn summed(works: &[Work]) -> (u64, u64, u64) {
    works.iter().fold((0, 0, 0), |(calls, rows, bytes), work| {
        (calls + work.calls, rows + work.rows, bytes + work.bytes)
    })
}

struct Tap {
    served: Served,
    riders: Atom<Vec<Landed>>,
    log: Arc<PartedLog<ModelParts>>,
    witness: Arc<Witness>,
    limit: usize,
    gate: Atom<Option<(Arc<str>, Arc<Signal>)>>,
    wrong: Wrong,
}

impl Tap {
    fn gated(&self, scope: &str) -> Option<Arc<Signal>> {
        self.gate.read(|gate| {
            gate.as_ref()
                .filter(|(gated, _)| gated.as_ref() == scope)
                .map(|(_, signal)| Arc::clone(signal))
        })
    }

    fn check(&self, scope: &str, from: u64, rows: &[(u64, String)]) {
        let expected = self.witness.latest_seq(scope);
        if from < expected {
            flag(
                &self.wrong,
                format!(
                    "I8: a claim of {scope} reads rows from {from}, below its checkpoint at row {expected}"
                ),
            );
        }
        if from > expected {
            flag(
                &self.wrong,
                format!("a claim of {scope} skips rows {expected}..{from} past its checkpoint"),
            );
        }
        if rows.len() > self.limit {
            flag(
                &self.wrong,
                format!(
                    "I8: a claim of {scope} replays {} tail rows past its checkpoint, more than the {} the checkpoint bound allows",
                    rows.len(),
                    self.limit
                ),
            );
        }
    }
}

impl LogHost for Tap {
    fn call_bound(&self) -> Option<Duration> {
        self.log.call_bound()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let answer = self.log.scan(scope, from, to).await;
            self.served.read(&answer);
            let rows = answer?;
            self.check(scope, from, &rows);
            Ok(rows)
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let answer = self.log.load(scope).await;
            self.served.read(&answer);
            answer
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if let Some(signal) = self.gated(scope) {
                signal.wait().await;
            }
            let answer = self.log.append(scope, rows).await;
            Served::add(&self.served.writes, rows);
            answer
        })
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let Write::Append { rows, .. } = write else {
            return None;
        };
        Some(Box::pin(async move {
            if let Some(signal) = self.gated(scope) {
                signal.wait().await;
            }
            Served::add(&self.served.writes, rows);
            if rows.is_empty() {
                flag(
                    &self.wrong,
                    format!("rider: a rider of {scope} commits in a write without its rows"),
                );
                return Ok(());
            }
            self.log.append(scope, rows).await?;
            let statements: Vec<(&'static str, Vec<String>)> = rider
                .statements
                .iter()
                .map(|statement| {
                    let params = statement.params.iter().map(|param| (*param).to_owned());
                    (statement.sql, params.collect())
                })
                .collect();
            let end = rows.last().map_or(0, |(seq, _)| seq + 1);
            let landed: Landed = (scope.to_owned(), end, statements);
            self.riders.update(|riders| riders.push(landed.clone()));
            Ok(())
        }))
    }
}

#[derive(Default)]
struct Net {
    nodes: Atom<Vec<(String, Option<Scopes<Buckets>>)>>,
    down: Atom<BTreeSet<String>>,
}

impl Route<Buckets> for Net {
    fn forward<'a>(
        &'a self,
        holder: &'a str,
        scope: &'a str,
        command: Add,
    ) -> StoreFuture<'a, Result<Committed<u64>, ScopeError>> {
        Box::pin(async move {
            if self.down.read(|down| down.contains(holder)) {
                std::future::pending::<()>().await;
            }
            let target = self.nodes.read(|nodes| {
                nodes
                    .iter()
                    .find(|(name, _)| name == holder)
                    .and_then(|(_, scopes)| scopes.clone())
            });
            match target {
                Some(scopes) => scopes.serve(scope, command).await,
                None => Err(ScopeError::Unreachable {
                    holder: holder.to_owned(),
                }),
            }
        })
    }
}

struct Node {
    name: String,
    log: Arc<PartedLog<ModelParts>>,
    tap: Arc<Tap>,
    scopes: Scopes<Buckets>,
    clock: Arc<AtomicU64>,
    paused: bool,
    handles: BTreeMap<String, Scope<Buckets>>,
}

struct World {
    time: Arc<Clock>,
    db: Arc<AtomicU64>,
    store: Arc<ModelParts>,
    witness: Arc<Witness>,
    net: Arc<Net>,
    reader: PartedLog<ModelParts>,
    wrong: Wrong,
    policy: Policy,
    count: NonZeroU32,
    scopes: Vec<String>,
    metered: Atom<Vec<(Scopes<Buckets>, Arc<Tap>)>>,
}

impl World {
    fn node(&self, name: &str) -> Node {
        let clock = Arc::new(AtomicU64::new(self.db.load(Ordering::Relaxed)));
        let local = Arc::clone(&clock);
        let log = Arc::new(
            PartedLog::fenced(Arc::clone(&self.store), self.count, name)
                .with_clock(Arc::new(move || local.load(Ordering::Relaxed)))
                .with_lease_bounds(GRACE_MS, MARGIN_MS),
        );
        let leases = PartedLeases::new(Arc::clone(&log), name, TTL_MS);
        let tap = Arc::new(Tap {
            served: Served::default(),
            riders: Atom::default(),
            log: Arc::clone(&log),
            witness: Arc::clone(&self.witness),
            limit: self.policy.checkpoint_rows,
            gate: Atom::default(),
            wrong: Arc::clone(&self.wrong),
        });
        let local = Arc::clone(&clock);
        let scopes = Scopes::new(Wiring {
            fold: Arc::new(Buckets),
            log: Arc::clone(&tap) as Arc<dyn LogHost>,
            leases: Arc::new(leases),
            checkpoints: Arc::clone(&self.witness) as Arc<dyn Checkpoints>,
            route: Arc::clone(&self.net) as Arc<dyn Route<Buckets>>,
            policy: self.policy,
            clock: Arc::new(move || local.load(Ordering::Relaxed)),
            claimed: None,
        });
        self.metered
            .update(|metered| metered.push((scopes.clone(), Arc::clone(&tap))));
        Node {
            name: name.to_owned(),
            log,
            tap,
            scopes,
            clock,
            paused: false,
            handles: BTreeMap::new(),
        }
    }

    fn rows(&self, scope: &str) -> Vec<(u64, String)> {
        block_on(self.reader.scan(scope, 0, SCAN_END)).expect("the model store answers every scan")
    }

    fn refold(&self, scope: &str, below: u64) -> Cells {
        let mut cells = Cells::default();
        for (seq, row) in self.rows(scope).iter().filter(|(seq, _)| *seq < below) {
            Buckets
                .apply(&mut cells, *seq, row)
                .expect("committed rows fold");
        }
        cells
    }

    fn head(&self, scope: &str) -> u64 {
        self.rows(scope).last().map_or(0, |(seq, _)| seq + 1)
    }
}

fn metered(world: &World) {
    let mut checkpoint_calls = 0;
    for (scopes, tap) in world.metered.read(Clone::clone) {
        let usage = scopes.stats().usage;
        checkpoint_calls += usage.checkpoint.calls;
        let reads = summed(&[usage.load, usage.adopt, usage.history]);
        let served = Served::work(&tap.served.reads);
        if reads != served {
            flag(
                &world.wrong,
                format!(
                    "I16: the meter counts reads (calls, rows, bytes) {reads:?} where the host served {served:?}"
                ),
            );
        }
        let writes = counted(usage.append);
        let served = Served::work(&tap.served.writes);
        if writes != served {
            flag(
                &world.wrong,
                format!(
                    "I16: the meter counts appends (calls, rows, bytes) {writes:?} where the host served {served:?}"
                ),
            );
        }
    }
    let served = world.witness.calls.load(Ordering::Relaxed);
    if checkpoint_calls != served {
        flag(
            &world.wrong,
            format!(
                "I16: the meter counts {checkpoint_calls} checkpoint store calls where the store served {served}"
            ),
        );
    }
}

fn publish(world: &World, nodes: &[Node]) {
    world.net.nodes.set(
        nodes
            .iter()
            .map(|node| (node.name.clone(), Some(node.scopes.clone())))
            .collect(),
    );
    world.net.down.set(
        nodes
            .iter()
            .filter(|node| node.paused)
            .map(|node| node.name.clone())
            .collect(),
    );
}

fn add(rng: &mut Rng, limit: usize) -> Add {
    let values = if rng.chance(OVERSIZE_PPM) {
        limit as u64 + 1
    } else {
        1 + rng.below(VALUES_MAX)
    };
    Add {
        bucket: u8::try_from(rng.below(u64::from(BUCKETS))).expect("a bucket fits in u8"),
        values: (0..values).map(|_| rng.below(1_000)).collect(),
        decided: Arc::default(),
    }
}

fn held_by_paused(world: &World, nodes: &[Node], scope: &str) -> bool {
    let part = world.reader.partition(scope);
    matches!(world.store.fence_row(part), Some(Row::Held { holder, .. }) if nodes.iter().any(|node| node.paused && node.name == holder))
}

fn send(rng: &mut Rng, world: &World, nodes: &mut [Node], stats: &mut ScopeStats) {
    let at = rng.pick(nodes.len());
    let scope = world.scopes[rng.pick(world.scopes.len())].clone();
    if !nodes[at].paused && held_by_paused(world, nodes, &scope) && !rng.chance(DOWN_ROUTE_PPM) {
        return;
    }
    stats.sends += 1;
    let command = add(rng, world.policy.checkpoint_rows);
    let node = &mut nodes[at];
    let sent =
        match driven(world, "a get", node.scopes.get(&scope)).unwrap_or(Err(ScopeError::Busy)) {
            Err(ScopeError::Store(StoreError::Backend(error))) => {
                flag(
                    &world.wrong,
                    format!("claim: a get of {scope} on {} failed: {error}", node.name),
                );
                Err(None)
            }
            Err(_) => Err(None),
            Ok(Reached::Local(handle)) => {
                node.handles.insert(scope.clone(), handle.clone());
                driven(world, "a send", handle.send(command))
                    .unwrap_or(Err(ScopeError::Busy))
                    .map_err(Some)
            }
            Ok(Reached::Routed(routed)) => {
                stats.routed += 1;
                routed_live(world, node, &scope, routed.holder());
                forwarded(world, &scope, &routed, command)
            }
        };
    match sent {
        Ok(_) => stats.committed += 1,
        Err(_) => stats.refused += 1,
    }
}

fn routed_live(world: &World, node: &Node, scope: &str, holder: &str) {
    if node.paused {
        return;
    }
    let part = world.reader.partition(scope);
    let now = world.db.load(Ordering::Relaxed);
    if let Some(Row::Held {
        holder: held,
        expires_at,
        ..
    }) = world.store.fence_row(part)
        && held == holder
        && expires_at + GRACE_MS < now
    {
        flag(
            &world.wrong,
            format!(
                "lease: {} routed {scope} to {holder}, whose lease expired at {expires_at} with {GRACE_MS} ms grace, before the database clock's {now}",
                node.name
            ),
        );
    }
}

fn forwarded(
    world: &World,
    scope: &str,
    routed: &Routed<Buckets>,
    command: Add,
) -> Result<Committed<u64>, Option<ScopeError>> {
    let holder = routed.holder().to_owned();
    let watched =
        nanocode_platform::timeout(Duration::from_millis(WATCHDOG_MS), routed.send(command));
    match drive(&world.time, watched) {
        Some(Ok(sent)) => sent.map_err(Some),
        Some(Err(_)) | None => {
            flag(
                &world.wrong,
                format!(
                    "deadline: a forward of {scope} to {holder} waited past {WATCHDOG_MS} ms under a {FORWARD_MS} ms forward deadline"
                ),
            );
            Err(None)
        }
    }
}

fn stall(rng: &mut Rng, world: &World, nodes: &mut [Node], stats: &mut ScopeStats) {
    let at = rng.pick(nodes.len());
    if nodes[at].paused {
        return;
    }
    let scope = world.scopes[rng.pick(world.scopes.len())].clone();
    let node = &mut nodes[at];
    let Some(Ok(Reached::Local(handle))) = driven(world, "a get", node.scopes.get(&scope)) else {
        return;
    };
    node.handles.insert(scope.clone(), handle.clone());
    let signal = Arc::new(Signal::new());
    node.tap
        .gate
        .set(Some((Arc::from(scope.as_str()), Arc::clone(&signal))));
    let command = Add {
        bucket: u8::try_from(rng.below(u64::from(BUCKETS))).expect("a bucket fits in u8"),
        values: vec![rng.below(1_000)],
        decided: Arc::default(),
    };
    let mut sending = Box::pin(handle.send(command));
    let stalled = sending
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending();
    if stalled {
        stats.stalls += 1;
        let reader = handle.clone();
        let (sent, read) = mpsc::channel();
        let thread = std::thread::spawn(move || drop(sent.send(reader.state())));
        match read.recv_timeout(Duration::from_millis(READ_WAIT_MS)) {
            Ok(state) => {
                let committed = world.refold(&scope, handle.next());
                if *state != committed {
                    flag(
                        &world.wrong,
                        format!(
                            "I10: a read of {scope} during a stalled append sees a fold of rows that did not commit"
                        ),
                    );
                }
            }
            Err(_) => flag(
                &world.wrong,
                format!("I9: a read of {scope} waited {READ_WAIT_MS} ms behind a stalled append"),
            ),
        }
        if rng.chance(ABANDON_PPM) {
            stats.abandoned += 1;
            drop(sending);
        } else {
            signal.fire();
            let _ = driven(world, "a stalled send", sending);
        }
        signal.fire();
        drop(thread.join());
    } else {
        drop(sending);
    }
    node.tap.gate.set(None);
}

fn stale(rng: &mut Rng, world: &World, nodes: &mut [Node], stats: &mut ScopeStats) {
    let paused: Vec<usize> = (0..nodes.len()).filter(|at| nodes[*at].paused).collect();
    if paused.is_empty() {
        return;
    }
    let node = &mut nodes[paused[rng.pick(paused.len())]];
    stats.stale_plays += 1;
    let _ = driven(world, "a tick", node.scopes.tick());
    let handles: Vec<Scope<Buckets>> = node.handles.values().cloned().collect();
    if handles.is_empty() {
        return;
    }
    let handle = &handles[rng.pick(handles.len())];
    if rng.chance(500_000) {
        let _ = driven(world, "a stale drain", node.scopes.drain(handle.name()));
        return;
    }
    let command = Add {
        bucket: 0,
        values: vec![rng.below(1_000)],
        decided: Arc::default(),
    };
    let _ = driven(world, "a stale send", handle.send(command));
}

fn epoch_of(world: &World, scope: &str) -> Option<u64> {
    match world.store.fence_row(world.reader.partition(scope)) {
        Some(Row::Held { epoch, .. } | Row::Released { epoch, .. }) => Some(epoch),
        None => None,
    }
}

async fn all<T>(
    mut sends: Vec<Pin<Box<dyn Future<Output = T> + '_>>>,
    mut answers: Vec<Option<T>>,
) -> Vec<T> {
    assert_eq!(sends.len(), answers.len(), "every send has its answer slot");
    std::future::poll_fn(|context| {
        for (send, answer) in sends.iter_mut().zip(answers.iter_mut()) {
            if answer.is_none()
                && let Poll::Ready(value) = send.as_mut().poll(context)
            {
                *answer = Some(value);
            }
        }
        if answers.iter().all(Option::is_some) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    answers.into_iter().flatten().collect()
}

fn burst(rng: &mut Rng, world: &World, nodes: &mut [Node], stats: &mut ScopeStats) {
    let at = rng.pick(nodes.len());
    if nodes[at].paused {
        return;
    }
    let scope = world.scopes[rng.pick(world.scopes.len())].clone();
    let node = &mut nodes[at];
    let Some(Ok(Reached::Local(handle))) = driven(world, "a get", node.scopes.get(&scope)) else {
        return;
    };
    node.handles.insert(scope.clone(), handle.clone());
    let before = epoch_of(world, &scope);
    let signal = Arc::new(Signal::new());
    node.tap
        .gate
        .set(Some((Arc::from(scope.as_str()), Arc::clone(&signal))));
    let commands: Vec<Add> = (0..rng.range(BURST_SENDS.start, BURST_SENDS.end))
        .map(|_| Add {
            bucket: u8::try_from(rng.below(u64::from(BUCKETS))).expect("a bucket fits in u8"),
            values: (0..=rng.below(VALUES_MAX))
                .map(|_| rng.below(1_000))
                .collect(),
            decided: Arc::default(),
        })
        .collect();
    let counts: Vec<Arc<AtomicU64>> = commands
        .iter()
        .map(|command| Arc::clone(&command.decided))
        .collect();
    stats.bursts += 1;
    stats.burst_sends += commands.len() as u64;
    let mut sends: Vec<Pin<Box<dyn Future<Output = Result<Committed<u64>, ScopeError>> + '_>>> =
        commands
            .into_iter()
            .map(|command| Box::pin(handle.send(command)) as Pin<Box<dyn Future<Output = _>>>)
            .collect();
    let early: Vec<Option<Result<Committed<u64>, ScopeError>>> = sends
        .iter_mut()
        .map(
            |send| match send.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(answer) => Some(answer),
                Poll::Pending => None,
            },
        )
        .collect();
    signal.fire();
    node.tap.gate.set(None);
    let answers = driven(world, "a burst", all(sends, early)).unwrap_or_default();
    if before.is_none() || epoch_of(world, &scope) != before {
        return;
    }
    for (answer, count) in answers.iter().zip(&counts) {
        if answer.is_err() {
            continue;
        }
        stats.burst_decides_checked += 1;
        let decided = count.load(Ordering::Relaxed);
        if decided != 1 {
            flag(
                &world.wrong,
                format!(
                    "decide: a command to {scope} committed at one epoch after its owner decided it {decided} times"
                ),
            );
            return;
        }
    }
}

fn staging(rng: &mut Rng, world: &World, nodes: &mut [Node], stats: &mut ScopeStats) {
    let at = rng.pick(nodes.len());
    if nodes[at].paused {
        return;
    }
    let scope = world.scopes[rng.pick(world.scopes.len())].clone();
    let node = &mut nodes[at];
    let Some(Ok(Reached::Local(handle))) = driven(world, "a get", node.scopes.get(&scope)) else {
        return;
    };
    node.handles.insert(scope.clone(), handle.clone());
    for _ in 0..rng.range(STAGES.start, STAGES.end) {
        let command = Add {
            bucket: u8::try_from(rng.below(u64::from(BUCKETS))).expect("a bucket fits in u8"),
            values: vec![rng.below(1_000)],
            decided: Arc::default(),
        };
        match driven(world, "a stage", handle.stage(command)) {
            Some(Ok(_)) => stats.staged += 1,
            Some(Err(_)) | None => return,
        }
        if *handle.state() != world.refold(&scope, handle.next()) {
            flag(
                &world.wrong,
                format!("staged: a read of {scope} sees a staged row before it commits"),
            );
            return;
        }
    }
    let carry = Carry {
        host: RIDER_HOST,
        statements: vec![(RIDER_SQL, vec![format!("{}", rng.below(1_000))])],
    };
    match rng.below(4) {
        0 => {}
        1 => {
            let answer = driven(world, "a flush", handle.flush(None));
            flushed(world, &scope, &node.tap, answer, None, stats);
        }
        2 => {
            let answer = driven(world, "a carrying flush", handle.flush(Some(carry.clone())));
            flushed(world, &scope, &node.tap, answer, Some(&carry), stats);
        }
        _ => {
            let command = Add {
                bucket: 0,
                values: vec![rng.below(1_000)],
                decided: Arc::default(),
            };
            let _ = driven(world, "a send after stages", handle.send(command));
        }
    }
}

fn flushed(
    world: &World,
    scope: &str,
    tap: &Tap,
    answer: Option<Result<Flushed, ScopeError>>,
    carry: Option<&Carry>,
    stats: &mut ScopeStats,
) {
    let Some(Ok(flushed)) = answer else {
        return;
    };
    stats.flushes += 1;
    let Some(next) = flushed.next else {
        return;
    };
    let head = world.head(scope);
    if head < next {
        flag(
            &world.wrong,
            format!(
                "durable: a flush of {scope} answered at row {next} with the log's head at {head}"
            ),
        );
        return;
    }
    if !flushed.carried {
        return;
    }
    stats.carried += 1;
    let Some(carry) = carry else {
        flag(
            &world.wrong,
            format!("rider: a flush of {scope} without a rider says it carried one"),
        );
        return;
    };
    let landed = tap.riders.read(|riders| {
        riders.iter().any(|(at, end, statements)| {
            at == scope && *end == next && *statements == carry.statements
        })
    });
    if !landed {
        flag(
            &world.wrong,
            format!(
                "rider: the rider of a flush of {scope} at row {next} did not land in the write of its rows"
            ),
        );
    }
}

fn replace(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    generation: &mut u64,
    stats: &mut ScopeStats,
) {
    stats.replacements += 1;
    for _ in 0..STALLS_BEFORE_REPLACE {
        stall(rng, world, nodes, stats);
    }
    for at in 0..nodes.len() {
        if !nodes[at].paused {
            let held: Vec<Partition> = (0..PARTS)
                .map(Partition)
                .filter(|part| nodes[at].log.held(*part).is_some())
                .collect();
            for part in held {
                let released = driven(world, "a shutdown release", nodes[at].scopes.release(part));
                if let Some(Row::Held { holder, .. }) = world.store.fence_row(part)
                    && holder == nodes[at].name
                {
                    flag(
                        &world.wrong,
                        format!(
                            "deploy: {} shut down still holding partition {}, so every get of its scopes routes to it until its lease lapses: {released:?}",
                            nodes[at].name, part.0
                        ),
                    );
                }
            }
            stats.release_drops += nodes[at].scopes.stats().release_drops;
        }
        *generation += 1;
        nodes[at] = world.node(&format!("n{at}g{generation}"));
    }
}

fn residents(world: &World, node: &Node) -> Vec<(String, u64)> {
    let _ = world;
    node.scopes
        .residents()
        .into_iter()
        .map(|(scope, epoch, _)| (scope, epoch))
        .collect()
}

fn departures(world: &World, node: &Node, before: &[(String, u64)], stats: &mut ScopeStats) {
    let after: BTreeSet<String> = node
        .scopes
        .residents()
        .into_iter()
        .map(|(scope, _, _)| scope)
        .collect();
    for (scope, epoch) in before {
        if after.contains(scope) {
            continue;
        }
        let part = node.log.partition(scope);
        let ours = match world.store.fence_row(part) {
            Some(Row::Held {
                holder,
                epoch: held,
                ..
            }) => holder == node.name && held == *epoch,
            Some(Row::Released {
                epoch: released, ..
            }) => released == *epoch,
            None => false,
        };
        if !ours {
            continue;
        }
        stats.departures_checked += 1;
        let head = world.head(scope);
        let checkpointed = world.witness.latest_seq(scope);
        if checkpointed != head {
            flag(
                &world.wrong,
                format!(
                    "I12: {} dropped {scope} at row {head} with its last checkpoint at row {checkpointed}",
                    node.name
                ),
            );
        }
    }
}

fn folds(world: &World, nodes: &[Node]) {
    for node in nodes {
        for (scope, handle) in &node.handles {
            let next = handle.next();
            let state = handle.state();
            if *state != world.refold(scope, next) {
                flag(
                    &world.wrong,
                    format!(
                        "I10: {} publishes a fold of {scope} at row {next} that is not the fold of its committed rows",
                        node.name
                    ),
                );
                return;
            }
        }
    }
}

fn advance(rng: &mut Rng, world: &World, nodes: &mut [Node]) {
    let now = world
        .db
        .fetch_add(rng.below(TICK_MS_MAX + 1), Ordering::Relaxed);
    let _ = now;
    let now = world.db.load(Ordering::Relaxed);
    for node in nodes.iter_mut().filter(|node| !node.paused) {
        node.clock.store(now, Ordering::Relaxed);
        if rng.chance(RENEW_PPM) {
            let _ = driven(world, "a renewal", node.scopes.renew());
        }
    }
}

fn step(
    rng: &mut Rng,
    world: &World,
    nodes: &mut Vec<Node>,
    generation: &mut u64,
    race: &mut Option<Race>,
    stats: &mut ScopeStats,
) {
    stats.steps += 1;
    advance(rng, world, nodes);
    let before: Vec<Vec<(String, u64)>> = nodes.iter().map(|node| residents(world, node)).collect();
    let crashed = match race.take() {
        Some(raced) => {
            let (crashed, next) = play(rng, world, nodes, generation, raced, stats);
            *race = next;
            crashed
        }
        None => roll(rng, world, nodes, generation, stats),
    };
    publish(world, nodes);
    for (at, node) in nodes.iter().enumerate() {
        if crashed != Some(at) {
            departures(world, node, &before[at], stats);
        }
    }
    folds(world, nodes);
}

fn roll(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    generation: &mut u64,
    stats: &mut ScopeStats,
) -> Option<usize> {
    let mut crashed = None;
    let roll = rng.below(1_000_000);
    let mut bands = [
        CRASH_PPM,
        PAUSE_PPM,
        UNPAUSE_PPM,
        RELEASE_PPM,
        STALL_PPM,
        TICK_PPM,
        STALE_PPM,
        BURST_PPM,
        STAGE_PPM,
        REPLACE_PPM,
    ]
    .into_iter();
    let mut floor = 0;
    let mut band = || {
        let width = bands.next().unwrap_or(0);
        floor += width;
        roll < floor
    };
    if band() {
        let at = rng.pick(nodes.len());
        stats.crashes += 1;
        *generation += 1;
        let name = if rng.chance(500_000) {
            nodes[at].name.clone()
        } else {
            format!("n{at}g{generation}")
        };
        nodes[at] = world.node(&name);
        crashed = Some(at);
    } else if band() {
        let at = rng.pick(nodes.len());
        if !nodes[at].paused {
            stats.pauses += 1;
            nodes[at].paused = true;
        }
    } else if band() {
        let now = world.db.load(Ordering::Relaxed);
        for node in nodes.iter_mut().filter(|node| node.paused) {
            node.paused = false;
            node.clock.store(now, Ordering::Relaxed);
        }
    } else if band() {
        let at = rng.pick(nodes.len());
        let held: Vec<Partition> = (0..PARTS)
            .map(Partition)
            .filter(|part| nodes[at].log.held(*part).is_some())
            .collect();
        if !held.is_empty() && !nodes[at].paused {
            let part = held[rng.pick(held.len())];
            stats.releases += 1;
            let _ = driven(world, "a release", nodes[at].scopes.release(part));
        }
    } else if band() {
        publish(world, nodes);
        stall(rng, world, nodes, stats);
    } else if band() {
        let at = rng.pick(nodes.len());
        let _ = driven(world, "a tick", nodes[at].scopes.tick());
    } else if band() {
        publish(world, nodes);
        stale(rng, world, nodes, stats);
    } else if band() {
        publish(world, nodes);
        burst(rng, world, nodes, stats);
    } else if band() {
        publish(world, nodes);
        staging(rng, world, nodes, stats);
    } else if band() {
        publish(world, nodes);
        replace(rng, world, nodes, generation, stats);
        crashed = Some(nodes.len());
    } else {
        publish(world, nodes);
        send(rng, world, nodes, stats);
    }
    crashed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    BeforeCheckpoint,
    AfterCheckpoint,
    Release,
    CrashLapse,
    NextAppend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Prime,
    Pause,
    Forward,
    Lapse,
    Claim,
    Checkpoint,
    Crash,
    Stale,
}

impl Point {
    const ALL: [Self; 5] = [
        Self::BeforeCheckpoint,
        Self::AfterCheckpoint,
        Self::Release,
        Self::CrashLapse,
        Self::NextAppend,
    ];

    const fn acts(self) -> &'static [Act] {
        match self {
            Self::BeforeCheckpoint | Self::Release | Self::NextAppend => &[
                Act::Prime,
                Act::Pause,
                Act::Forward,
                Act::Lapse,
                Act::Claim,
                Act::Stale,
            ],
            Self::AfterCheckpoint => &[
                Act::Prime,
                Act::Pause,
                Act::Forward,
                Act::Lapse,
                Act::Claim,
                Act::Checkpoint,
                Act::Stale,
            ],
            Self::CrashLapse => &[
                Act::Prime,
                Act::Pause,
                Act::Forward,
                Act::Lapse,
                Act::Claim,
                Act::Crash,
                Act::Lapse,
                Act::Stale,
            ],
        }
    }

    fn uncheckpointed(self, policy: &Policy) -> u64 {
        match self {
            Self::NextAppend => policy.checkpoint_rows as u64,
            Self::BeforeCheckpoint | Self::AfterCheckpoint | Self::Release | Self::CrashLapse => 1,
        }
    }
}

struct Race {
    point: Point,
    at: usize,
    scope: String,
    old: usize,
    new: usize,
}

impl Race {
    const fn new(point: Point, scope: String) -> Self {
        Self {
            point,
            at: 0,
            scope,
            old: 0,
            new: 0,
        }
    }
}

fn play(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    generation: &mut u64,
    mut race: Race,
    stats: &mut ScopeStats,
) -> (Option<usize>, Option<Race>) {
    let act = race.point.acts()[race.at];
    publish(world, nodes);
    let mut crashed = None;
    let played = match act {
        Act::Prime => prime(rng, world, nodes, &mut race, stats),
        Act::Pause => {
            stats.pauses += 1;
            nodes[race.old].paused = true;
            true
        }
        Act::Forward => forward(rng, world, nodes, &race, stats),
        Act::Lapse => {
            lapse(world, nodes);
            true
        }
        Act::Claim => claim(world, &mut nodes[race.new], &race.scope),
        Act::Checkpoint => driven(world, "a drain", nodes[race.new].scopes.drain(&race.scope))
            .is_some_and(|drained| drained.is_ok()),
        Act::Crash => {
            stats.crashes += 1;
            *generation += 1;
            nodes[race.new] = world.node(&format!("n{}g{generation}", race.new));
            crashed = Some(race.new);
            true
        }
        Act::Stale => stale_race(rng, world, &mut nodes[race.old], &race, stats),
    };
    if !played {
        if act == Act::Prime {
            return (crashed, Some(race));
        }
        stats.races_missed += 1;
        return (crashed, None);
    }
    race.at += 1;
    if race.at == race.point.acts().len() {
        stats.races += 1;
        return (crashed, None);
    }
    (crashed, Some(race))
}

fn prime(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    race: &mut Race,
    stats: &mut ScopeStats,
) -> bool {
    let now = world.db.load(Ordering::Relaxed);
    for node in nodes.iter_mut().filter(|node| node.paused) {
        node.paused = false;
        node.clock.store(now, Ordering::Relaxed);
    }
    publish(world, nodes);
    let part = world.reader.partition(&race.scope);
    let holder = match world.store.fence_row(part) {
        Some(Row::Held { holder, .. }) => nodes.iter().position(|node| node.name == holder),
        Some(Row::Released { .. }) | None => None,
    };
    let old = holder.unwrap_or_else(|| rng.pick(nodes.len()));
    let new = (old + 1 + rng.pick(nodes.len() - 1)) % nodes.len();
    let node = &mut nodes[old];
    let _ = driven(world, "a renewal", node.scopes.renew());
    let Some(Ok(Reached::Local(handle))) = driven(world, "a get", node.scopes.get(&race.scope))
    else {
        return false;
    };
    node.handles.insert(race.scope.clone(), handle.clone());
    let target = world.witness.latest_seq(&race.scope) + race.point.uncheckpointed(&world.policy);
    if handle.next() < target {
        stats.sends += 1;
        let command = Add {
            bucket: u8::try_from(rng.below(u64::from(BUCKETS))).expect("a bucket fits in u8"),
            values: (handle.next()..target).map(|_| rng.below(1_000)).collect(),
            decided: Arc::default(),
        };
        match driven(world, "a send", handle.send(command)) {
            Some(Ok(_)) => stats.committed += 1,
            Some(Err(_)) | None => stats.refused += 1,
        }
    }
    let target = world.witness.latest_seq(&race.scope) + race.point.uncheckpointed(&world.policy);
    race.old = old;
    race.new = new;
    handle.next() >= target
}

fn forward(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    race: &Race,
    stats: &mut ScopeStats,
) -> bool {
    let old = nodes[race.old].name.clone();
    let node = &mut nodes[race.new];
    let _ = driven(world, "a renewal", node.scopes.renew());
    let Some(Ok(Reached::Routed(routed))) = driven(world, "a get", node.scopes.get(&race.scope))
    else {
        return false;
    };
    if routed.holder() != old {
        return false;
    }
    stats.sends += 1;
    stats.routed += 1;
    let command = Add {
        bucket: 0,
        values: vec![rng.below(1_000)],
        decided: Arc::default(),
    };
    match forwarded(world, &race.scope, &routed, command) {
        Ok(_) => stats.committed += 1,
        Err(_) => stats.refused += 1,
    }
    true
}

fn lapse(world: &World, nodes: &mut [Node]) {
    let now = world.db.fetch_add(LAPSE_MS, Ordering::Relaxed) + LAPSE_MS;
    for node in nodes.iter_mut().filter(|node| !node.paused) {
        node.clock.store(now, Ordering::Relaxed);
        let _ = driven(world, "a renewal", node.scopes.renew());
    }
}

fn claim(world: &World, node: &mut Node, scope: &str) -> bool {
    let _ = driven(world, "a renewal", node.scopes.renew());
    let Some(Ok(Reached::Local(handle))) = driven(world, "a get", node.scopes.get(scope)) else {
        return false;
    };
    node.handles.insert(scope.to_owned(), handle);
    true
}

fn stale_race(
    rng: &mut Rng,
    world: &World,
    node: &mut Node,
    race: &Race,
    stats: &mut ScopeStats,
) -> bool {
    stats.stale_plays += 1;
    let refused = world.witness.stale.load(Ordering::Relaxed);
    match race.point {
        Point::Release => {
            let part = node.log.partition(&race.scope);
            let _ = driven(world, "a stale release", node.scopes.release(part));
        }
        Point::NextAppend => {
            let Some(handle) = node.handles.get(&race.scope).cloned() else {
                return false;
            };
            let command = Add {
                bucket: 0,
                values: vec![rng.below(1_000)],
                decided: Arc::default(),
            };
            let _ = driven(world, "a stale send", handle.send(command));
        }
        Point::BeforeCheckpoint | Point::AfterCheckpoint | Point::CrashLapse => {
            let _ = driven(world, "a stale drain", node.scopes.drain(&race.scope));
        }
    }
    let refused = world.witness.stale.load(Ordering::Relaxed) > refused;
    stats.races_refused += u64::from(refused);
    refused || world.wrong.read(Option::is_some)
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: ScopeStats,
    shape: String,
}

fn driven<T>(world: &World, what: &str, future: impl Future<Output = T>) -> Option<T> {
    let driven = drive(&world.time, future);
    if driven.is_none() {
        flag(
            &world.wrong,
            format!("deadline: {what} waits with no timer left to fire"),
        );
    }
    driven
}

fn drive<T>(time: &Arc<Clock>, future: impl Future<Output = T>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Some(value);
        }
        if !time.advance_to_next() {
            return None;
        }
    }
}

fn run_seed(seed: u64, mutation: ScopeMutation) -> Outcome {
    let time = Clock::new();
    crate::host::entered(seed, &time, || seeded(seed, mutation, &time))
}

fn seeded(seed: u64, mutation: ScopeMutation, time: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "scope");
    let mut stats = ScopeStats {
        seeds: 1,
        ..ScopeStats::default()
    };
    let db = Arc::new(AtomicU64::new(START_MS));
    let clock = Arc::clone(&db);
    let store =
        Arc::new(ModelParts::new().with_clock(Arc::new(move || clock.load(Ordering::Relaxed))));
    let count = NonZeroU32::new(PARTS).expect("partitions are counted from one");
    block_on(store.migrate(count)).expect("the model store migrates");
    let wrong: Wrong = Arc::default();
    let witness = Arc::new(Witness {
        inner: MemoryCheckpoints::new(Arc::clone(&store)),
        store: Arc::clone(&store),
        wrong: Arc::clone(&wrong),
        stale: AtomicU64::new(0),
        calls: AtomicU64::new(0),
    });
    let policy = Policy {
        checkpoint_rows: usize::try_from(rng.range(CHECKPOINT_ROWS.start, CHECKPOINT_ROWS.end))
            .expect("a row bound fits"),
        checkpoint_ms: CHECKPOINT_MS,
        resident_bytes_max: usize::try_from(rng.range(RESIDENT_BYTES.start, RESIDENT_BYTES.end))
            .expect("a byte bound fits"),
        forward_ms: FORWARD_MS,
        admit_ms: ADMIT_MS,
    };
    let world = World {
        time: Arc::clone(time),
        db,
        store: Arc::clone(&store),
        witness,
        net: Arc::new(Net::default()),
        reader: PartedLog::new(Arc::clone(&store), count),
        wrong,
        policy,
        count,
        scopes: (0..rng.range(SCOPES.start, SCOPES.end))
            .map(|at| format!("scope-{at}"))
            .collect(),
        metered: Atom::default(),
    };
    let mut nodes: Vec<Node> = (0..rng.range(NODES.start, NODES.end))
        .map(|at| world.node(&format!("n{at}g0")))
        .collect();
    let steps = rng.range(STEPS.start, STEPS.end);
    let shape = format!(
        "nodes={} scopes={} checkpoint_rows={} resident_bytes_max={} steps={steps}",
        nodes.len(),
        world.scopes.len(),
        policy.checkpoint_rows,
        policy.resident_bytes_max
    );
    let first = rng.pick(Point::ALL.len());
    let races = Point::ALL.len() as u64;
    let mut planned = (0..races).map(|at| {
        let point = Point::ALL[(first + at as usize) % Point::ALL.len()];
        (steps * (at + 1) / (races + 1), point)
    });
    let mut next = planned.next();
    let mut race = None;
    scope_sim::mutate(mutation.0);
    let mut generation = 0;
    for at in 0..steps {
        if race.is_none()
            && let Some((start, point)) = next
            && at >= start
        {
            let scope = world.scopes[rng.pick(world.scopes.len())].clone();
            race = Some(Race::new(point, scope));
            next = planned.next();
        }
        step(
            &mut rng,
            &world,
            &mut nodes,
            &mut generation,
            &mut race,
            &mut stats,
        );
        if world.wrong.read(Option::is_some) {
            break;
        }
    }
    scope_sim::mutate(Mutation::None);
    metered(&world);
    for node in &nodes {
        let runtime = node.scopes.stats();
        stats.runtime(&runtime);
        stats.resident_bytes_max = stats.resident_bytes_max.max(runtime.resident_bytes);
    }
    stats.checkpoints_stale = world.witness.stale.load(Ordering::Relaxed);
    world.net.nodes.set(Vec::new());
    let mut digest = fnv(FNV_OFFSET, &seed.to_le_bytes());
    for scope in &world.scopes {
        for (seq, row) in world.rows(scope) {
            digest = fnv(fnv(digest, &seq.to_le_bytes()), row.as_bytes());
        }
        for manifest in world.witness.inner.manifests(scope) {
            digest = fnv(
                fnv(digest, &manifest.seq.to_le_bytes()),
                &manifest.epoch.to_le_bytes(),
            );
        }
    }
    let wrong = world.wrong.read(Clone::clone);
    Outcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn judge(
    seed: u64,
    mutation: ScopeMutation,
    twice: bool,
    verbose: bool,
    totals: &mut ScopeStats,
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

pub fn replay(seed: u64, mutation: ScopeMutation) -> String {
    let mut command = format!("nanocode-sim --scenario scope --seed {seed}");
    if mutation != ScopeMutation::NONE {
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
    mutation: ScopeMutation,
    totals: &ScopeStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=scope mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        ScopeMutation::NONE if failed == 0 => ExitCode::SUCCESS,
        ScopeMutation::NONE => ExitCode::FAILURE,
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

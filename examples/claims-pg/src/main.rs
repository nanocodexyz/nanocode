//! Board claims on a real Postgres 16+: a claim+finish loop over the partitioned board (one
//! shards::Committer per partition) on PartedLog::fenced over SqlParts, with the NOTIFY sender
//! (SqlParts::notifier) running as it does in a node.
//!
//! examples/claims-pg/run.sh starts a private Postgres and runs this once per database:
//! cargo run --release -p nanocode-claims-pg -- postgres://nc@127.0.0.1:5544/claims parts=16
//!
//! Options are key=value: parts (board partitions, 16), log_parts (log partitions, = parts),
//! claimers (64), tasks (20000), batch (committer group, 256), conns (pool, 64), threads (4),
//! max_rows (rows per log commit, 512), pipeline (1: send each finish with the next claim rather
//! than before it, so both can share a log commit; 1, the default; 0 for the old
//! claim-then-finish order), prefetch (k: each claimer keeps k claims in flight, sending claim
//! i+k with finish i, so a claimer's k claims and k finishes share log commits; 1, the default,
//! is the pipeline loop above; k > 1 needs pipeline=1), sync_notify (1: the NOTIFY transaction is
//! durable, as the bench always ran it; 0: unsynced, as a node sends it through PgSql), prof (1:
//! print PHASE lines at the claim phase's start and end and PROF lines with pool wait and
//! transaction time per operation and the client's CPU per thread). The board must be fresh: give
//! each run its own database.
//!
//! Several nodes on one database, as production runs them: nodes (1) and node (0) give this
//! process the node-th of nodes contiguous ranges of the parts board partitions. It claims only
//! those partitions' fence rows (its own epochs, as node{node}), opens only their boards, and posts
//! and claims tasks (tasks per node) homed there; examples/claims-pg/run.sh starts every node of an
//! argument set with nodes=N. start_ms (a unix time in ms, 0) holds the claim phase until then so
//! the nodes start together. race (1): race claimers of a partition walk the same tasks in the same
//! order, so every claim races race ways and the losers' refusals count as lost, not as failures.
//! Log leases last lease_ms (10000) on the database clock and every node renews all of its own in
//! one statement three times a lease; grace_ms (2000) and margin_ms (500) are the lease bounds.
//! adopt (a node index, off) with adopt_ms (unix ms): at adopt_ms, gain that node's log partitions,
//! each in its own task under recover (16) recovery permits (waiting up to recover_wait_ms, 60000,
//! for one), retrying every retry_ms (50) while
//! its lease is live (a killed node's until expiry plus grace, a releasing node's until its
//! release lands), and claim what it left on each partition's boards as soon as that partition
//! has recovered (serve_all=1: once all have), skipping tasks it finished or holds. release_ms
//! (unix ms, off) is the planned side: at release_ms the node drains and releases every log
//! partition it holds. A node that loses its partitions stops its claimers at the first fenced
//! write and counts them as fenced. The ADOPT line gives the time from adopt_ms until the first,
//! the median and the last gained partition served.
//! Prints one METRIC line: claims/s over the whole claim phase and over its first 90% (before the
//! tail where only some partitions still have tasks), claim latency, Postgres transactions per
//! claim, board writes per log append and log appends per Postgres commit. checkpoint_every (0, the
//! default: none) turns fold checkpoints on at that cadence, as nanocode_board::checkpoint's
//! checkpoint_every does.

use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU32,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nanocode_board::{
    Applied, Board, BoardError, ClaimId, NewTask, Op, Outcome, TaskId, Write,
    access::{Can, Grant, Identity, Verifier, Who},
    shards::{self, Committer},
};
use nanocode_durability_core::{
    Settle, Sql, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
};
use nanocode_partitions::{Partition, partition_of};
use nanocode_store_sql::{PgOptions, PgPool, PgSql, PgTimes};
use nanocode_sync::{Inbox, Semaphore, SetOnce};
use nanocode_turnlog::{
    LogHost,
    parted::{GainError, PartedLog, SqlParts},
};

const OWNER: &str = "user:lead";
const SCOPE: &str = "board";
const LEASE_MS: u64 = 600_000;
const RENEWALS_PER_LEASE: u64 = 3;

/// Counts transactions, each one pipelined round trip on PgSql.
struct Counted {
    inner: PgSql,
    trips: AtomicU64,
    sync_notify: bool,
}

impl Sql for Counted {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.inner.call_bound()
    }

    fn dialect(&self) -> SqlDialect {
        self.inner.dialect()
    }

    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        self.trips.fetch_add(1, Ordering::Relaxed);
        self.inner.transaction(operation, statements)
    }

    fn write<'a>(
        &'a self,
        settle: Settle,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        if self.sync_notify {
            return self.transaction(operation, statements);
        }
        self.trips.fetch_add(1, Ordering::Relaxed);
        self.inner.write(settle, operation, statements)
    }
}

/// What prof=1 collects per operation from PgSql::timed: pool wait and transaction time.
#[derive(Default)]
struct OpTimes {
    count: u64,
    waited_us: u64,
    ran_us: Vec<u32>,
}

static PROFILE: LazyLock<Inbox<(&'static str, u64, u32)>> = LazyLock::new(Inbox::default);

fn timed(operation: &'static str, times: PgTimes) {
    PROFILE.push((
        operation,
        u64::try_from(times.waited.as_micros()).unwrap_or(u64::MAX),
        u32::try_from(times.ran.as_micros()).unwrap_or(u32::MAX),
    ));
}

/// Each thread's name and CPU ticks (user + system) from /proc/self/task, on Linux.
fn thread_ticks() -> BTreeMap<String, u64> {
    let mut ticks = BTreeMap::new();
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return ticks;
    };
    for task in tasks.flatten() {
        let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let used = fields.get(11..13).map_or(0, |pair| {
            pair.iter().filter_map(|f| f.parse::<u64>().ok()).sum()
        });
        let name = format!(
            "{}#{}",
            &stat[open + 1..close],
            task.file_name().to_string_lossy()
        );
        ticks.insert(name, used);
    }
    ticks
}

fn print_profile(claims: f64, elapsed: f64, before: &BTreeMap<String, u64>) {
    let mut profile: BTreeMap<&'static str, OpTimes> = BTreeMap::new();
    for (operation, waited_us, ran_us) in PROFILE.drain() {
        let entry = profile.entry(operation).or_default();
        entry.count += 1;
        entry.waited_us += waited_us;
        entry.ran_us.push(ran_us);
    }
    for (operation, mut times) in profile {
        times.ran_us.sort_unstable();
        let ran: Vec<u64> = times.ran_us.iter().map(|us| u64::from(*us)).collect();
        let total: u64 = ran.iter().sum();
        println!(
            "PROF op={operation} n={} per_claim={:.3} per_s={:.0} wait_avg_us={:.1} ran_avg_us={:.1} ran_p50_ms={:.3} ran_p99_ms={:.3} ran_max_ms={:.3}",
            times.count,
            times.count as f64 / claims.max(1.0),
            times.count as f64 / elapsed,
            times.waited_us as f64 / (times.count as f64).max(1.0),
            total as f64 / (times.count as f64).max(1.0),
            percentile(&ran, 0.5),
            percentile(&ran, 0.99),
            percentile(&ran, 1.0),
        );
    }
    // Ticks are 1/100 s on Linux.
    let after = thread_ticks();
    let mut busy: Vec<(String, f64)> = after
        .iter()
        .map(|(name, ticks)| {
            let used = ticks - before.get(name).copied().unwrap_or(0);
            (name.clone(), used as f64 / 100.0 / elapsed)
        })
        .filter(|(_, share)| *share > 0.01)
        .collect();
    busy.sort_by(|a, b| b.1.total_cmp(&a.1));
    let total: f64 = busy.iter().map(|(_, share)| share).sum();
    let threads: Vec<String> = busy
        .iter()
        .map(|(name, share)| format!("{name}:{:.0}%", share * 100.0))
        .collect();
    println!(
        "PROF cpu cores={total:.2} cpu_us_per_claim={:.1} threads={}",
        total * elapsed * 1e6 / claims.max(1.0),
        threads.join(","),
    );
}

struct Tokens;

impl Verifier for Tokens {
    fn verify(&self, credential: &str) -> Option<String> {
        credential.strip_prefix("token:").map(str::to_owned)
    }
}

struct Options {
    url: String,
    parts: u32,
    log_parts: u32,
    claimers: usize,
    tasks: usize,
    batch: usize,
    conns: usize,
    threads: usize,
    max_rows: usize,
    pipeline: bool,
    prefetch: usize,
    sync_notify: bool,
    prof: bool,
    node: u32,
    nodes: u32,
    race: usize,
    start_ms: u64,
    adopt: Option<u32>,
    adopt_ms: u64,
    checkpoint_every: u64,
    lease_ms: u64,
    grace_ms: u64,
    margin_ms: u64,
    retry_ms: u64,
    recover: usize,
    recover_wait_ms: u64,
    serve_all: bool,
    release_ms: u64,
}

impl Options {
    fn arguments() -> Result<(String, BTreeMap<String, usize>), String> {
        let mut url = "postgres://nc@127.0.0.1:5544/claims".to_owned();
        let mut values: BTreeMap<String, usize> = BTreeMap::new();
        for arg in std::env::args().skip(1) {
            match arg.split_once('=') {
                Some((key, value)) if !arg.starts_with("postgres") => {
                    let value = value.parse().map_err(|_| format!("bad number in {arg}"))?;
                    values.insert(key.to_owned(), value);
                }
                _ => url = arg,
            }
        }
        let known = [
            "parts",
            "log_parts",
            "claimers",
            "tasks",
            "batch",
            "conns",
            "threads",
            "max_rows",
            "pipeline",
            "prefetch",
            "sync_notify",
            "prof",
            "node",
            "nodes",
            "race",
            "start_ms",
            "adopt",
            "adopt_ms",
            "checkpoint_every",
            "lease_ms",
            "grace_ms",
            "margin_ms",
            "retry_ms",
            "recover",
            "recover_wait_ms",
            "serve_all",
            "release_ms",
        ];
        if let Some(key) = values.keys().find(|key| !known.contains(&key.as_str())) {
            return Err(format!("unknown option {key}"));
        }
        Ok((url, values))
    }

    fn parse() -> Result<Self, String> {
        let (url, values) = Self::arguments()?;
        let get = |key: &str, default: usize| values.get(key).copied().unwrap_or(default);
        let parts = u32::try_from(get("parts", 16)).map_err(|_| "parts too large")?;
        let nodes = u32::try_from(get("nodes", 1).max(1)).map_err(|_| "nodes too large")?;
        let node = u32::try_from(get("node", 0)).map_err(|_| "node too large")?;
        if node >= nodes || nodes > parts {
            return Err(format!(
                "node={node} nodes={nodes} parts={parts}: need node < nodes <= parts"
            ));
        }
        let adopt = match values.get("adopt") {
            Some(adopt) => {
                let adopt = u32::try_from(*adopt).map_err(|_| "adopt too large")?;
                if adopt >= nodes || adopt == node {
                    return Err(format!("adopt={adopt} must be another node below nodes"));
                }
                Some(adopt)
            }
            None => None,
        };
        Ok(Self {
            url,
            parts,
            log_parts: u32::try_from(get("log_parts", parts as usize))
                .map_err(|_| "log_parts too large")?,
            claimers: get("claimers", 64).max(1),
            tasks: get("tasks", 20_000),
            batch: get("batch", 256),
            conns: get("conns", 64),
            threads: get("threads", 4).max(1),
            max_rows: get("max_rows", 512),
            pipeline: get("pipeline", 1) != 0,
            prefetch: get("prefetch", 1).max(1),
            sync_notify: get("sync_notify", 1) != 0,
            prof: get("prof", 0) != 0,
            node,
            nodes,
            race: get("race", 1).max(1),
            start_ms: get("start_ms", 0) as u64,
            adopt,
            adopt_ms: get("adopt_ms", 0) as u64,
            checkpoint_every: get("checkpoint_every", 0) as u64,
            lease_ms: get("lease_ms", 10_000) as u64,
            grace_ms: get("grace_ms", 2_000) as u64,
            margin_ms: get("margin_ms", 500) as u64,
            retry_ms: get("retry_ms", 50).max(1) as u64,
            recover: get("recover", 16).max(1),
            recover_wait_ms: get("recover_wait_ms", 60_000) as u64,
            serve_all: get("serve_all", 0) != 0,
            release_ms: get("release_ms", 0) as u64,
        })
    }
}

/// Renews every log lease this node holds in one statement, RENEWALS_PER_LEASE times a lease.
async fn renewing(log: Arc<PartedLog<SqlParts<Counted>>>, lease_ms: u64) {
    let every = Duration::from_millis(lease_ms / RENEWALS_PER_LEASE);
    loop {
        nanocode_executor::sleep(every).await;
        match log.renew_all(lease_ms).await {
            Ok(lost) if !lost.is_empty() => {
                eprintln!("claims-pg: lost log partitions {lost:?}");
            }
            Ok(_) => {}
            Err(error) => eprintln!("claims-pg: renew_all: {error}"),
        }
    }
}

/// A planned handoff: at release_ms, drains and releases every log partition this node holds, so
/// a gainer claims them at once at their committed cursors.
async fn releasing(log: Arc<PartedLog<SqlParts<Counted>>>, release_ms: u64, parts: Vec<Partition>) {
    let wait = release_ms.saturating_sub(now_ms());
    nanocode_executor::sleep(Duration::from_millis(wait)).await;
    let began = Instant::now();
    let mut released = 0;
    for part in parts {
        match log.release(part).await {
            Ok(()) => released += 1,
            Err(error) => eprintln!("claims-pg: release {}: {error}", part.0),
        }
    }
    println!(
        "RELEASED partitions={released} drain_release_ms={:.1}",
        began.elapsed().as_secs_f64() * 1e3
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn micros(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    assert!((0.0..=1.0).contains(&p));
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index] as f64 / 1000.0
}

fn write(by: &str, key: String, op: Op) -> Write {
    Write {
        by: Some(by.to_owned()),
        key: Some(key),
        at_ms: now_ms(),
        op,
    }
}

static FIRST_ERROR: SetOnce<String> = SetOnce::const_new();

fn note(error: &BoardError) {
    if !FIRST_ERROR.initialized() {
        let _ = FIRST_ERROR.set(format!("{error:?}"));
    }
}

/// Owner writes every partition needs before anyone posts: the charter and an open grant.
async fn charter(committers: &[Committer]) -> Result<(), BoardError> {
    let everyone = Grant {
        who: vec![Who::Everyone],
        can: BTreeMap::from([(
            "*".to_owned(),
            vec![Can::Read, Can::Post, Can::Label, Can::Rank],
        )]),
    };
    for committer in committers {
        committer
            .submit(write(OWNER, "charter".to_owned(), Op::Charter {}))
            .await?;
        committer
            .submit(write(
                OWNER,
                "grant".to_owned(),
                Op::Grant {
                    grant: everyone.clone(),
                },
            ))
            .await?;
    }
    Ok(())
}

fn task(index: usize) -> NewTask {
    NewTask {
        id: TaskId(format!("t{index}")),
        parent: None,
        spec: format!("root {index}"),
        labels: BTreeMap::new(),
        needs: Vec::new(),
        terms: nanocode_board::tasks::Terms::default(),
    }
}

/// The board partitions node owns of parts, split into nodes contiguous ranges.
fn range_of(node: u32, nodes: u32, parts: u32) -> std::ops::Range<u32> {
    assert!(node < nodes);
    let share =
        |n: u32| u32::try_from(u64::from(n) * u64::from(parts) / u64::from(nodes)).unwrap_or(parts);
    share(node)..share(node + 1)
}

/// The log partitions the board partitions of range land on.
fn log_parts_of(
    log: &PartedLog<SqlParts<Counted>>,
    count: NonZeroU32,
    range: std::ops::Range<u32>,
) -> Vec<Partition> {
    let mut parts: Vec<Partition> = range
        .map(|part| log.partition(&shards::scope_of(SCOPE, count, Partition(part))))
        .collect();
    parts.sort_unstable_by_key(|part| part.0);
    parts.dedup();
    parts
}

/// The first tasks task indices homed in range, as the board routes a root task without labels,
/// per partition of range. With one node these are 0..tasks.
fn homes_of(range: std::ops::Range<u32>, count: NonZeroU32, tasks: usize) -> Vec<Vec<usize>> {
    let mut homes: Vec<Vec<usize>> = vec![Vec::new(); range.len()];
    let (mut found, mut index) = (0, 0);
    while found < tasks {
        let home = partition_of(format!("t{index}").as_bytes(), count).0;
        if range.contains(&home) {
            homes[(home - range.start) as usize].push(index);
            found += 1;
        }
        index += 1;
    }
    homes
}

/// The tasks claimer rank of a partition's peers walks: with race 1 a disjoint share; with race
/// K, groups of K claimers share one list in the same order, so each claim races K ways.
fn share_of(home: &[usize], rank: usize, peers: usize, race: usize) -> Vec<usize> {
    let groups = (peers / race).max(1);
    home.iter()
        .copied()
        .skip(rank % groups)
        .step_by(groups)
        .collect()
}

const fn fenced(error: &BoardError) -> bool {
    matches!(
        error,
        BoardError::Store(StoreError::Fenced)
            | BoardError::Unsettled {
                error: StoreError::Fenced,
                ..
            }
    )
}

/// What one claimer measured.
#[derive(Default)]
struct Claimed {
    latencies: Vec<u64>,
    done_at: Vec<u64>,
    refused: u64,
    /// Claims another claimer won (race) or a dead node left finished or held (adopt).
    lost: u64,
    /// Claimers stopped because this node lost the partition's fence.
    fenced: u64,
}

impl Claimed {
    fn merge(&mut self, other: Self) {
        self.latencies.extend(other.latencies);
        self.done_at.extend(other.done_at);
        self.refused += other.refused;
        self.lost += other.lost;
        self.fenced += other.fenced;
    }
}

async fn finish(
    committer: Committer,
    by: String,
    index: usize,
    claim: ClaimId,
) -> Result<(), BoardError> {
    let finish = Op::Finish {
        task: TaskId(format!("t{index}")),
        claim,
        outcome: Outcome::Done(String::new()),
    };
    committer
        .submit(write(&by, format!("finish:{index}"), finish))
        .await
        .map(drop)
}

/// One claimer's loop. tolerant: a rejected claim is lost to another claimer (race, adopt)
/// rather than a failure.
async fn claimer(
    committer: Committer,
    by: String,
    tasks: Vec<usize>,
    started: Instant,
    pipeline: bool,
    tolerant: bool,
) -> Claimed {
    let mut claimed = Claimed::default();
    let mut finishing = None;
    for index in tasks {
        let began = Instant::now();
        let claim = Op::Claim {
            task: TaskId(format!("t{index}")),
            lease_ms: LEASE_MS,
            fence: None,
            cap: None,
        };
        let applied = match committer
            .submit(write(&by, format!("claim:{index}"), claim))
            .await
        {
            Ok(applied) => applied,
            Err(error) if fenced(&error) => {
                claimed.fenced += 1;
                break;
            }
            Err(BoardError::Rejected(_)) if tolerant => {
                claimed.lost += 1;
                continue;
            }
            Err(error) => {
                note(&error);
                claimed.refused += 1;
                continue;
            }
        };
        claimed.latencies.push(micros(began));
        if let Some(previous) = finishing.take()
            && !claimed.settle(previous, started).await
        {
            break;
        }
        let finish = finish(
            committer.clone(),
            by.clone(),
            index,
            ClaimId(applied.event.0),
        );
        if pipeline {
            finishing = Some(nanocode_executor::spawn(finish));
        } else if !claimed
            .settle(nanocode_executor::spawn(finish), started)
            .await
        {
            break;
        }
    }
    if let Some(previous) = finishing {
        claimed.settle(previous, started).await;
    }
    claimed
}

async fn claim(committer: Committer, by: String, index: usize) -> Result<Applied, BoardError> {
    let claim = Op::Claim {
        task: TaskId(format!("t{index}")),
        lease_ms: LEASE_MS,
        fence: None,
        cap: None,
    };
    committer
        .submit(write(&by, format!("claim:{index}"), claim))
        .await
}

/// prefetch=k: keep k claims in flight. When the oldest claim lands, its finish and the claim of
/// the task k places further on go to the committer together, so in steady state a claimer's
/// group carries up to k finishes and k claims. A worker would run task i while claims i+1..i+k
/// wait; here the work takes no time.
async fn prefetching(
    committer: Committer,
    by: String,
    tasks: Vec<usize>,
    started: Instant,
    prefetch: usize,
    tolerant: bool,
) -> Claimed {
    let mut claimed = Claimed::default();
    let mut tasks = tasks.into_iter();
    let mut claims = VecDeque::new();
    let mut finishing = VecDeque::new();
    for index in tasks.by_ref().take(prefetch) {
        let handle = nanocode_executor::spawn(claim(committer.clone(), by.clone(), index));
        claims.push_back((index, Instant::now(), handle));
    }
    while let Some((index, began, handle)) = claims.pop_front() {
        match handle.await {
            Ok(Ok(applied)) => {
                claimed.latencies.push(micros(began));
                let finish = finish(
                    committer.clone(),
                    by.clone(),
                    index,
                    ClaimId(applied.event.0),
                );
                finishing.push_back(nanocode_executor::spawn(finish));
            }
            Ok(Err(error)) if fenced(&error) => {
                claimed.fenced += 1;
                break;
            }
            Ok(Err(BoardError::Rejected(_))) if tolerant => claimed.lost += 1,
            Ok(Err(error)) => {
                note(&error);
                claimed.refused += 1;
            }
            Err(error) => {
                note(&BoardError::Dropped(format!("claim: {error}")));
                claimed.refused += 1;
            }
        }
        if let Some(next) = tasks.next() {
            let handle = nanocode_executor::spawn(claim(committer.clone(), by.clone(), next));
            claims.push_back((next, Instant::now(), handle));
        }
        if finishing.len() > prefetch
            && let Some(previous) = finishing.pop_front()
            && !claimed.settle(previous, started).await
        {
            break;
        }
    }
    // Claims still in flight after a fence settle as lost work rather than leaking.
    for (_, _, handle) in claims {
        let _ = handle.await;
    }
    for previous in finishing {
        claimed.settle(previous, started).await;
    }
    claimed
}

impl Claimed {
    /// Waits for a finish; false when the node lost the partition and the claimer should stop.
    async fn settle(
        &mut self,
        finish: nanocode_executor::JoinHandle<Result<(), BoardError>>,
        started: Instant,
    ) -> bool {
        match finish.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if fenced(&error) => {
                self.fenced += 1;
                return false;
            }
            Ok(Err(error)) => {
                note(&error);
                self.refused += 1;
            }
            Err(_) => self.refused += 1,
        }
        self.done_at.push(micros(started));
        true
    }
}

fn fail(what: &str) -> std::process::ExitCode {
    eprintln!("claims-pg: {what}");
    std::process::ExitCode::FAILURE
}

/// Opens the boards and starts a committer on each.
async fn open_boards(
    host: &Arc<dyn LogHost>,
    count: NonZeroU32,
    boards: &[u32],
    identity: &Identity,
    batch: usize,
) -> Result<Opened, BoardError> {
    let mut committers = Vec::new();
    let mut drives = Vec::new();
    for part in boards {
        let scope = shards::scope_of(SCOPE, count, Partition(*part));
        let board = Board::open(Arc::clone(host), &scope, identity.clone()).await?;
        let (committer, drive) = Committer::start(board, batch);
        committers.push(committer);
        drives.push(nanocode_executor::spawn(async move { drive.await.1 }));
    }
    Ok((committers, drives))
}

/// What taking a dead or departing node's partitions cost and claimed.
struct Adopted {
    claimed: Claimed,
    takeover_ms: f64,
    serving_ms: Vec<f64>,
    seconds: f64,
    stats: Vec<shards::Stats>,
}

type Opened = (
    Vec<Committer>,
    Vec<nanocode_executor::JoinHandle<shards::Stats>>,
);

/// One gained log partition, recovered: its boards' committers and when it started serving.
struct Recovered {
    boards: Vec<u32>,
    opened: Opened,
    serving_ms: f64,
}

/// Gains one of victim's log partitions in its own task: retries the gain while the victim's
/// lease is live (a crash: until expiry plus grace on the database clock; a planned handoff:
/// until the victim's release lands), then recovers the boards that land on it.
async fn gain_one(
    log: Arc<PartedLog<SqlParts<Counted>>>,
    options: Arc<Options>,
    count: NonZeroU32,
    part: Partition,
    boards: Vec<u32>,
    capacity: Arc<Semaphore>,
    identity: Identity,
    began: Instant,
) -> Result<Recovered, String> {
    let patience = Duration::from_millis(options.lease_ms + options.grace_ms + options.retry_ms);
    let deadline = began + patience;
    let room = Duration::from_millis(options.recover_wait_ms);
    let batch = options.batch;
    loop {
        let recover = |_| {
            let host: Arc<dyn LogHost> = Arc::clone(&log) as Arc<dyn LogHost>;
            let (identity, boards) = (identity.clone(), boards.clone());
            async move {
                open_boards(&host, count, &boards, &identity, batch)
                    .await
                    .map_err(|error| StoreError::Backend(format!("open board: {error:?}")))
            }
        };
        match log
            .gain(
                part,
                options.lease_ms,
                &capacity,
                nanocode_executor::sleep(room),
                recover,
            )
            .await
        {
            Ok((_, opened)) => {
                return Ok(Recovered {
                    boards,
                    opened,
                    serving_ms: began.elapsed().as_secs_f64() * 1e3,
                });
            }
            Err(GainError::Fenced) if Instant::now() < deadline => {
                nanocode_executor::sleep(Duration::from_millis(options.retry_ms)).await;
            }
            Err(error) => {
                return Err(format!(
                    "gain log partition {} after {:.0} ms: {error:?}",
                    part.0,
                    began.elapsed().as_secs_f64() * 1e3
                ));
            }
        }
    }
}

/// Waits until adopt_ms, then gains victim's log partitions, each in its own task under a
/// recovery capacity of recover permits, and starts claiming on each partition's boards as soon as
/// it has recovered (serve_all=1: only once every partition has, as one readiness gate). It
/// claims what the victim left, skipping tasks it finished or holds.
async fn adopt(
    log: Arc<PartedLog<SqlParts<Counted>>>,
    options: Arc<Options>,
    count: NonZeroU32,
    victim: u32,
    identity: Identity,
) -> Result<Adopted, String> {
    let wait = options.adopt_ms.saturating_sub(now_ms());
    nanocode_executor::sleep(Duration::from_millis(wait)).await;
    let began = Instant::now();
    let range = range_of(victim, options.nodes, options.parts);
    let mut gains = start_gains(&log, &options, count, &range, &identity, began);
    let homes = homes_of(range.clone(), count, options.tasks);
    let mine = range_of(options.node, options.nodes, options.parts)
        .len()
        .max(1);
    let per_part = (options.claimers / mine).max(1);
    let started = Instant::now();
    let mut serving_ms = Vec::new();
    let mut handles = Vec::new();
    let mut drives = Vec::new();
    let mut committers = Vec::new();
    let mut waiting = Vec::new();
    while let Some(joined) = gains.join_next().await {
        let recovered = joined.map_err(|error| format!("gain task: {error}"))??;
        if options.serve_all {
            waiting.push(recovered);
            continue;
        }
        serving_ms.push(recovered.serving_ms);
        serve(&options, &homes, range.start, per_part, started, recovered, &mut handles, &mut committers, &mut drives);
    }
    for mut recovered in waiting {
        recovered.serving_ms = began.elapsed().as_secs_f64() * 1e3;
        serving_ms.push(recovered.serving_ms);
        serve(&options, &homes, range.start, per_part, started, recovered, &mut handles, &mut committers, &mut drives);
    }
    let takeover_ms = serving_ms.iter().copied().fold(0.0, f64::max);
    let mut claimed = Claimed::default();
    for handle in handles {
        if let Ok(one) = handle.await {
            claimed.merge(one);
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    drop(committers);
    let mut stats = Vec::new();
    for drive in drives {
        if let Ok(one) = drive.await {
            stats.push(one);
        }
    }
    serving_ms.sort_by(f64::total_cmp);
    Ok(Adopted {
        claimed,
        takeover_ms,
        serving_ms,
        seconds,
        stats,
    })
}

fn start_gains(
    log: &Arc<PartedLog<SqlParts<Counted>>>,
    options: &Arc<Options>,
    count: NonZeroU32,
    range: &std::ops::Range<u32>,
    identity: &Identity,
    began: Instant,
) -> nanocode_executor::JoinSet<Result<Recovered, String>> {
    let capacity = Arc::new(Semaphore::new(options.recover));
    let mut gains = nanocode_executor::JoinSet::new();
    for part in log_parts_of(log, count, range.clone()) {
        let boards: Vec<u32> = range
            .clone()
            .filter(|board| {
                log.partition(&shards::scope_of(SCOPE, count, Partition(*board))) == part
            })
            .collect();
        gains.spawn(gain_one(
            Arc::clone(log),
            Arc::clone(options),
            count,
            part,
            boards,
            Arc::clone(&capacity),
            identity.clone(),
            began,
        ));
    }
    gains
}

/// Starts per_part claimers on each board of a recovered partition.
#[allow(clippy::too_many_arguments)]
fn serve(
    options: &Options,
    homes: &[Vec<usize>],
    first: u32,
    per_part: usize,
    started: Instant,
    recovered: Recovered,
    handles: &mut Vec<nanocode_executor::JoinHandle<Claimed>>,
    committers: &mut Vec<Committer>,
    drives: &mut Vec<nanocode_executor::JoinHandle<shards::Stats>>,
) {
    let (opened, more) = recovered.opened;
    for (board, committer) in recovered.boards.iter().zip(opened) {
        let home = &homes[(board - first) as usize];
        for rank in 0..per_part {
            let tasks = share_of(home, rank, per_part, options.race);
            handles.push(nanocode_executor::spawn(claimer(
                committer.clone(),
                format!("agent:n{}a{board}r{rank}", options.node),
                tasks,
                started,
                options.pipeline,
                true,
            )));
        }
        committers.push(committer);
    }
    drives.extend(more);
}

fn main() -> std::process::ExitCode {
    let options = match Options::parse() {
        Ok(options) => Arc::new(options),
        Err(error) => return fail(&error),
    };
    match run(&options) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => fail(&error),
    }
}

struct Node {
    runtime: nanocode_executor::Runtime,
    sql: Arc<Counted>,
    log: Arc<PartedLog<SqlParts<Counted>>>,
    count: NonZeroU32,
}

struct Phase {
    all: Claimed,
    elapsed: f64,
    start_unix_ms: u64,
    end_unix_ms: u64,
}

struct Totals {
    trips: u64,
    rounds: u64,
    writes: u64,
    commits: u64,
    appends: u64,
    late_ms: u64,
}

fn run(options: &Arc<Options>) -> Result<bool, String> {
    let node = connect(options)?;
    let Node {
        runtime,
        sql,
        log,
        count,
    } = &node;
    let count = *count;
    let range = range_of(options.node, options.nodes, options.parts);
    let fences = fences_of(options, log, count, &range)?;
    claim_fences(&node, options, fences)?;
    let host: Arc<dyn LogHost> = Arc::clone(log) as Arc<dyn LogHost>;
    let identity = Identity::verify(&Tokens, &format!("token:agent:node{}", options.node))
        .map_err(|error| format!("identity: {error:?}"))?;
    let boards: Vec<u32> = range.clone().collect();
    let (committers, drives) = runtime
        .block_on(open_boards(&host, count, &boards, &identity, options.batch))
        .map_err(|error| format!("open board: {error:?}"))?;

    // This node's tasks per partition of its range.
    let homes = homes_of(range.clone(), count, options.tasks);
    post_all(runtime, &committers, &homes).map_err(|error| format!("posting: {error:?}"))?;

    let adopting = options.adopt.map(|victim| {
        runtime.spawn(adopt(
            Arc::clone(log),
            Arc::clone(options),
            count,
            victim,
            identity.clone(),
        ))
    });
    let late_ms = hold_start(options);
    let trips_before = sql.trips.load(Ordering::Relaxed);
    let log_before = log.stats();
    let phase = claim_all(runtime, options, &committers, &homes, range.len());
    let trips = sql.trips.load(Ordering::Relaxed) - trips_before;
    let stats = log.stats();
    drop(committers);
    let (rounds, writes) = drain_drives(runtime, drives);
    let adopted = adopting.map(|handle| {
        runtime
            .block_on(handle)
            .map_err(|error| format!("adopt task: {error}"))
            .and_then(|adopted| adopted)
    });
    let totals = Totals {
        trips,
        rounds,
        writes,
        commits: stats.commits - log_before.commits,
        appends: stats.appends - log_before.appends,
        late_ms,
    };
    let mut ok = report(options, phase, &totals);
    if let Some(adopted) = adopted {
        ok &= report_adopted(options, adopted);
    }
    Ok(ok)
}

fn connect(options: &Options) -> Result<Node, String> {
    let (Some(count), Some(log_count)) = (
        NonZeroU32::new(options.parts),
        NonZeroU32::new(options.log_parts),
    ) else {
        return Err("parts and log_parts must be at least 1".to_owned());
    };
    nanocode_board::checkpoint::checkpoint_every(options.checkpoint_every);
    let Ok(runtime) = nanocode_executor::Builder::new_multi_thread()
        .worker_threads(options.threads)
        .build()
    else {
        return Err("no runtime".to_owned());
    };
    let pg = PgOptions::parse(&options.url).map_err(|error| format!("bad url: {error}"))?;
    let pool = PgPool::new(pg, options.conns);
    let mut inner = PgSql::reading(pool.clone(), pool);
    if options.prof {
        inner = inner.timed(timed);
    }
    let sql = Arc::new(Counted {
        inner,
        trips: AtomicU64::new(0),
        sync_notify: options.sync_notify,
    });
    let store = Arc::new(SqlParts::new(Arc::clone(&sql)));
    runtime.spawn(store.notifier());
    let name = if options.nodes == 1 {
        "bench".to_owned()
    } else {
        format!("node{}", options.node)
    };
    let log = Arc::new(
        PartedLog::fenced(Arc::clone(&store), log_count, &name)
            .with_max_rows(options.max_rows)
            .with_lease_bounds(options.grace_ms, options.margin_ms),
    );
    Ok(Node {
        runtime,
        sql,
        log,
        count,
    })
}

fn fences_of(
    options: &Options,
    log: &PartedLog<SqlParts<Counted>>,
    count: NonZeroU32,
    range: &std::ops::Range<u32>,
) -> Result<Vec<Partition>, String> {
    // Each node fences only the log partitions its boards land on, which must be its own.
    if options.nodes == 1 {
        return Ok((0..options.log_parts).map(Partition).collect());
    }
    let mine = log_parts_of(log, count, range.clone());
    for other in (0..options.nodes).filter(|other| *other != options.node) {
        let theirs = log_parts_of(log, count, range_of(other, options.nodes, options.parts));
        if let Some(shared) = mine.iter().find(|part| theirs.contains(part)) {
            return Err(format!(
                "log partition {} holds boards of node {} and node {other}: use log_parts = parts",
                shared.0, options.node
            ));
        }
    }
    Ok(mine)
}

fn claim_fences(node: &Node, options: &Options, fences: Vec<Partition>) -> Result<(), String> {
    let log = &node.log;
    let setup = node.runtime.block_on(async {
        log.migrate().await?;
        for part in &fences {
            log.claim(*part, options.lease_ms).await?;
        }
        Ok::<(), StoreError>(())
    });
    setup.map_err(|error| format!("log setup: {error}"))?;
    node.runtime
        .spawn(renewing(Arc::clone(log), options.lease_ms));
    if options.release_ms > 0 {
        node.runtime
            .spawn(releasing(Arc::clone(log), options.release_ms, fences));
    }
    Ok(())
}

fn post_all(
    runtime: &nanocode_executor::Runtime,
    committers: &[Committer],
    homes: &[Vec<usize>],
) -> Result<(), BoardError> {
    // Post every task, many posts in flight so they group-commit.
    runtime.block_on(async {
        charter(committers).await?;
        let all: Arc<Vec<(usize, usize)>> = Arc::new(
            homes
                .iter()
                .enumerate()
                .flat_map(|(local, home)| home.iter().map(move |index| (local, *index)))
                .collect(),
        );
        let posters = 256.min(all.len().max(1));
        let handles: Vec<_> = (0..posters)
            .map(|poster| {
                let committers = committers.to_vec();
                let all = Arc::clone(&all);
                runtime.spawn(async move {
                    for &(local, index) in all.iter().skip(poster).step_by(posters) {
                        let post = Op::Post { task: task(index) };
                        committers[local]
                            .submit(write(OWNER, format!("post:t{index}"), post))
                            .await?;
                    }
                    Ok::<(), BoardError>(())
                })
            })
            .collect();
        for handle in handles {
            handle
                .await
                .map_err(|error| BoardError::Dropped(format!("poster: {error}")))??;
        }
        Ok::<(), BoardError>(())
    })
}

fn hold_start(options: &Options) -> u64 {
    // Hold the claim phase until start_ms, so several nodes start together.
    let late_ms = if options.start_ms == 0 {
        0
    } else {
        now_ms().saturating_sub(options.start_ms)
    };
    if options.start_ms > now_ms() {
        std::thread::sleep(std::time::Duration::from_millis(
            options.start_ms - now_ms(),
        ));
    }
    late_ms
}

fn claim_all(
    runtime: &nanocode_executor::Runtime,
    options: &Options,
    committers: &[Committer],
    homes: &[Vec<usize>],
    parts: usize,
) -> Phase {
    // Claimer i works local partition i % parts, splitting its tasks with that partition's other
    // claimers: claim a task, finish it, take the next.
    let ticks_before = if options.prof {
        let _ = PROFILE.drain();
        println!("PHASE claim_start unix_ms={}", now_ms());
        thread_ticks()
    } else {
        BTreeMap::new()
    };
    let start_unix_ms = now_ms();
    let started = Instant::now();
    let tolerant = options.race > 1;
    let handles: Vec<_> = (0..options.claimers)
        .map(|index| {
            let part = index % parts;
            let peers = options.claimers / parts + usize::from(part < options.claimers % parts);
            let mine = share_of(&homes[part], index / parts, peers, options.race);
            let by = if options.nodes == 1 {
                format!("agent:w{index}")
            } else {
                format!("agent:n{}w{index}", options.node)
            };
            let committer = committers[part].clone();
            let (pipeline, prefetch) = (options.pipeline, options.prefetch);
            runtime.spawn(async move {
                if pipeline && prefetch > 1 {
                    prefetching(committer, by, mine, started, prefetch, tolerant).await
                } else {
                    claimer(committer, by, mine, started, pipeline, tolerant).await
                }
            })
        })
        .collect();
    let mut all = Claimed::default();
    runtime.block_on(async {
        for handle in handles {
            if let Ok(claimed) = handle.await {
                all.merge(claimed);
            }
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let end_unix_ms = now_ms();
    if options.prof {
        println!("PHASE claim_end unix_ms={}", now_ms());
        print_profile(options.tasks as f64, elapsed, &ticks_before);
    }
    Phase {
        all,
        elapsed,
        start_unix_ms,
        end_unix_ms,
    }
}

fn drain_drives(
    runtime: &nanocode_executor::Runtime,
    drives: Vec<nanocode_executor::JoinHandle<shards::Stats>>,
) -> (u64, u64) {
    let (mut rounds, mut writes) = (0, 0);
    runtime.block_on(async {
        for drive in drives {
            if let Ok(stats) = drive.await {
                rounds += stats.appends;
                writes += stats.writes;
            }
        }
    });
    (rounds, writes)
}

fn report(options: &Options, phase: Phase, totals: &Totals) -> bool {
    let Phase {
        all,
        elapsed,
        start_unix_ms,
        end_unix_ms,
    } = phase;
    let Claimed {
        mut latencies,
        mut done_at,
        refused,
        lost,
        fenced,
    } = all;
    latencies.sort_unstable();
    done_at.sort_unstable();
    let claims = latencies.len() as f64;
    let tenth = done_at.len() * 9 / 10;
    let steady = done_at
        .get(tenth)
        .map_or(0.0, |at| tenth as f64 / (*at as f64 / 1e6));
    let Totals {
        trips,
        rounds,
        writes,
        commits,
        appends,
        late_ms,
    } = *totals;
    println!(
        "METRIC parts={} log_parts={} claimers={} tasks={} batch={} pipeline={} prefetch={} claims_per_s={:.0} steady_per_s={steady:.0} p50_ms={:.2} p99_ms={:.2} max_ms={:.2} pg_txns_per_claim={:.2} writes_per_append={:.2} appends_per_commit={:.2} refused={refused} seconds={elapsed:.2} first_error={:?} node={} nodes={} race={} claims={} trips={trips} lost={lost} fenced={fenced} late_ms={late_ms} start_unix_ms={start_unix_ms} end_unix_ms={end_unix_ms}",
        options.parts,
        options.log_parts,
        options.claimers,
        options.tasks,
        options.batch,
        u8::from(options.pipeline),
        options.prefetch,
        claims / elapsed,
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        percentile(&latencies, 1.0),
        trips as f64 / claims.max(1.0),
        writes as f64 / (rounds as f64).max(1.0),
        appends as f64 / (commits as f64).max(1.0),
        FIRST_ERROR.get(),
        options.node,
        options.nodes,
        options.race,
        latencies.len(),
    );
    refused == 0 && (fenced > 0 || latencies.len() == options.tasks)
}

fn report_adopted(options: &Options, adopted: Result<Adopted, String>) -> bool {
    match adopted {
        Ok(Adopted {
            claimed,
            takeover_ms,
            serving_ms,
            seconds,
            stats,
        }) => {
            let mut latencies = claimed.latencies;
            latencies.sort_unstable();
            let at = |q: f64| {
                serving_ms
                    .get(((serving_ms.len().max(1) - 1) as f64 * q).round() as usize)
                    .copied()
                    .unwrap_or(0.0)
            };
            println!(
                "ADOPT node={} from={} serve_all={} recover={} partitions={} first_serving_ms={:.1} p50_serving_ms={:.1} takeover_ms={takeover_ms:.1} claims={} claims_per_s={:.0} p99_ms={:.2} lost={} refused={} fenced={} seconds={seconds:.2} appends={}",
                options.node,
                options.adopt.unwrap_or_default(),
                u8::from(options.serve_all),
                options.recover,
                serving_ms.len(),
                at(0.0),
                at(0.5),
                latencies.len(),
                latencies.len() as f64 / seconds.max(1e-9),
                percentile(&latencies, 0.99),
                claimed.lost,
                claimed.refused,
                claimed.fenced,
                stats.iter().map(|one| one.appends).sum::<u64>(),
            );
            claimed.refused == 0 && claimed.fenced == 0
        }
        Err(error) => {
            eprintln!("claims-pg: {error}");
            false
        }
    }
}

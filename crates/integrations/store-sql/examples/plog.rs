//! The partitioned log (nanocode_turnlog::parted) on a real Postgres 16+: a conformance pass over
//! SqlParts, then measured appends/s and claims/s at several concurrencies.
//!
//! cargo run --release -p nanocode-store-sql --example plog -- postgres://nc@127.0.0.1:5544/plog
//!
//! Prints one RESULT line per check and one METRIC line per measurement.

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use nanocode_durability_core::{Sql, StoreError};
use nanocode_partitions::{Partition, Row};
use nanocode_store_sql::{PgOptions, PgPool, PgSql, log_wake};
use nanocode_sync::SwapOption;
use nanocode_turnlog::{
    LogHost,
    parted::{PartStore, PartedLog, SqlParts},
};

static FIRST_ERROR: SwapOption<String> = SwapOption::const_empty();

type Log = PartedLog<SqlParts<Counted>>;

/// Counts transactions, each one pipelined round trip on PgSql.
struct Counted {
    inner: PgSql,
    trips: AtomicU64,
}

impl Sql for Counted {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.inner.call_bound()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.inner.row_bytes_max()
    }

    fn dialect(&self) -> nanocode_durability_core::SqlDialect {
        self.inner.dialect()
    }

    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [nanocode_durability_core::SqlStatement<'a>],
    ) -> nanocode_durability_core::StoreFuture<
        'a,
        Result<Vec<Vec<nanocode_durability_core::SqlRow>>, StoreError>,
    > {
        self.trips.fetch_add(1, Ordering::Relaxed);
        self.inner.transaction(operation, statements)
    }
}

fn check(name: &str, ok: bool, detail: &str, failed: &mut u32) {
    println!(
        "RESULT {} {name} {detail}",
        if ok { "pass" } else { "FAIL" }
    );
    if !ok {
        *failed += 1;
    }
}

const CONFORMANCE_PARTS: u32 = 4;
const CONFORMANCE_LEASE_MS: u64 = 1_000;
const CONFORMANCE_GRACE_MS: u64 = 500;
const CONFORMANCE_MARGIN_MS: u64 = 100;
const CLOCK_SLACK_MS: u64 = 250;

fn node(sql: &Arc<Counted>, name: &str) -> Result<Log, StoreError> {
    let count = NonZeroU32::new(CONFORMANCE_PARTS).ok_or(StoreError::Fenced)?;
    Ok(PartedLog::fenced(
        Arc::new(SqlParts::new(Arc::clone(sql))),
        count,
        &format!("{name}-{}", std::process::id()),
    )
    .with_lease_bounds(CONFORMANCE_GRACE_MS, CONFORMANCE_MARGIN_MS))
}

fn sibling(log: &Log, scope: &str, part: Partition, wanted: bool) -> String {
    (0..)
        .map(|index| format!("{scope}/other{index}"))
        .find(|other| (log.partition(other) == part) == wanted)
        .unwrap_or_default()
}

async fn committed(log: &Log, part: Partition) -> Result<u64, StoreError> {
    let entries = log.follow(part, 0, 1_000_000).await?;
    Ok(entries.last().map_or(0, |entry| entry.pos))
}

async fn conformance(sql: Arc<Counted>, failed: &mut u32) -> Result<(), StoreError> {
    let (a, b) = (node(&sql, "node-a")?, node(&sql, "node-b")?);
    a.migrate().await?;
    a.prepare().await?;
    let scope = format!("conf/{}", std::process::id());
    let part = a.partition(&scope);
    check(
        "unclaimed append is fenced",
        matches!(
            a.append(&scope, &[(0, "d0".into())]).await,
            Err(StoreError::Fenced)
        ),
        "",
        failed,
    );
    let other = sibling(&a, &scope, part, true);
    let epoch = a.claim(part, CONFORMANCE_LEASE_MS).await?;
    a.append(&scope, &[(0, "d0:1".into()), (1, "d1:2".into())])
        .await?;
    let conflict = a
        .append(&scope, &[(1, "dX:9".into()), (2, "dY:9".into())])
        .await;
    let rows = a.scan(&scope, 0, u64::MAX).await?;
    check(
        "conflicting seq rejects only its append, all-or-nothing",
        matches!(conflict, Err(StoreError::Fenced)) && rows.len() == 2,
        &format!("rows={}", rows.len()),
        failed,
    );
    let (x, y, z) = futures_join(
        a.append(&scope, &[(2, "d2:1".into())]),
        a.append(&scope, &[(2, "d2b:1".into())]),
        a.append(&other, &[(0, "d0:1".into())]),
    )
    .await;
    check(
        "one group commit: one of two same-seq appends lands, the other scope lands",
        (x.is_ok() ^ y.is_ok()) && z.is_ok(),
        &format!("{:?} {:?} {:?}", x.is_ok(), y.is_ok(), z.is_ok()),
        failed,
    );
    let taken = b.claim(part, CONFORMANCE_LEASE_MS).await;
    check(
        "a live lease is never claimed by another node",
        matches!(taken, Err(StoreError::Fenced)),
        &format!("{taken:?}"),
        failed,
    );
    a.release(part).await?;
    let position = committed(&a, part).await?;
    let b_epoch = b.claim(part, CONFORMANCE_LEASE_MS).await;
    check(
        "a released lease is claimed at once at a higher epoch, resuming at the committed position",
        b_epoch.as_ref().is_ok_and(|next| *next > epoch) && b.resume(part) == Some(position),
        &format!("{b_epoch:?} resume={:?} committed={position}", b.resume(part)),
        failed,
    );
    let stale = a.append(&scope, &[(3, "d3:1".into())]).await;
    check(
        "the old owner is fenced after the handoff",
        matches!(stale, Err(StoreError::Fenced)),
        "",
        failed,
    );
    reads(&b, &scope, part, failed).await?;
    leases(&sql, &b, &scope, part, failed).await?;
    b.cursor(part, &format!("reader-{scope}"), u64::MAX / 2)
        .await?;
    b.release(part).await?;
    check(
        "released partition refuses appends",
        matches!(
            b.append(&scope, &[(5, "d5:1".into())]).await,
            Err(StoreError::Fenced)
        ),
        "",
        failed,
    );
    Ok(())
}

async fn reads(b: &Log, scope: &str, part: Partition, failed: &mut u32) -> Result<(), StoreError> {
    b.append(scope, &[(3, "s5".into()), (4, "d4:1".into())])
        .await?;
    let loaded = b.load(scope).await?;
    check(
        "load starts at the latest snapshot",
        loaded
            .first()
            .is_some_and(|(seq, body)| *seq == 3 && body == "s5")
            && loaded.len() == 2,
        &format!("{loaded:?}"),
        failed,
    );
    let entries = b.follow(part, 0, 1_000_000).await?;
    let mine: Vec<_> = entries
        .iter()
        .filter(|entry| entry.scope.as_ref() == scope)
        .collect();
    let ordered = mine
        .windows(2)
        .all(|pair| pair[0].pos < pair[1].pos && pair[0].seq < pair[1].seq);
    check(
        "follow returns the partition in position order",
        ordered && mine.len() == 5,
        &format!("rows={}", mine.len()),
        failed,
    );
    let floor = mine.get(1).map_or(0, |entry| entry.pos);
    b.cursor(part, &format!("reader-{scope}"), floor).await?;
    let dropped = b.prune(scope, 4).await?;
    let left = b.scan(scope, 0, u64::MAX).await?;
    check(
        "prune keeps rows a cursor has not read and the snapshot",
        dropped == 2 && left.first().is_some_and(|(seq, _)| *seq == 2),
        &format!("dropped={dropped} first={:?}", left.first()),
        failed,
    );
    let more: Vec<(u64, String)> = (5..=14).map(|seq| (seq, format!("d{seq}:1"))).collect();
    b.append(scope, &more).await?;
    let scanned = b.scan(scope, 0, u64::MAX).await?;
    let loaded = b.load(scope).await?;
    let in_order = |rows: &[(u64, String)]| rows.windows(2).all(|pair| pair[1].0 == pair[0].0 + 1);
    check(
        "scan and load return more than nine rows in seq order",
        in_order(&scanned)
            && scanned.last().is_some_and(|(seq, _)| *seq == 14)
            && in_order(&loaded)
            && loaded.len() == 12,
        &format!(
            "scan={:?} load={:?}",
            scanned.iter().map(|row| row.0).collect::<Vec<_>>(),
            loaded.iter().map(|row| row.0).collect::<Vec<_>>()
        ),
        failed,
    );
    Ok(())
}

/// renew_all over two held partitions, then a crash handoff: the holder stops renewing, a peer is
/// refused while the lease is live and lands once expiry plus grace passed on the database clock,
/// and the old holder's next renew_all reports the lease lost.
async fn leases(
    sql: &Arc<Counted>,
    b: &Log,
    scope: &str,
    part: Partition,
    failed: &mut u32,
) -> Result<(), StoreError> {
    let second = b.partition(&sibling(b, scope, part, false));
    b.claim(second, CONFORMANCE_LEASE_MS).await?;
    let before = b.store().fence(second).await?;
    nanocode_executor::sleep(Duration::from_millis(CLOCK_SLACK_MS)).await;
    let lost = b.renew_all(CONFORMANCE_LEASE_MS).await;
    let after = b.store().fence(second).await?;
    let extended = match (&before, &after) {
        (Row::Held { expires_at: was, .. }, Row::Held { expires_at: now, .. }) => now > was,
        _ => false,
    };
    check(
        "renew_all renews every held lease in one statement",
        lost.as_ref().is_ok_and(Vec::is_empty) && extended,
        &format!("{lost:?} {before:?} -> {after:?}"),
        failed,
    );
    let c = node(sql, "node-c")?;
    let early = c.claim(second, CONFORMANCE_LEASE_MS).await;
    nanocode_executor::sleep(Duration::from_millis(
        CONFORMANCE_LEASE_MS + CONFORMANCE_GRACE_MS + CLOCK_SLACK_MS,
    ))
    .await;
    let late = c.claim(second, CONFORMANCE_LEASE_MS).await;
    check(
        "a crashed holder's lease is refused while live and claimed past expiry plus grace",
        matches!(early, Err(StoreError::Fenced)) && late.is_ok(),
        &format!("early={early:?} late={late:?}"),
        failed,
    );
    let gone = b.renew_all(CONFORMANCE_LEASE_MS).await;
    check(
        "renew_all reports the lease the old holder lost, and keeps the rest",
        gone.as_ref().is_ok_and(|lost| *lost == [second]) && b.held(part).is_some(),
        &format!("{gone:?}"),
        failed,
    );
    c.release(second).await
}

async fn futures_join<A: std::future::Future, B: std::future::Future, C: std::future::Future>(
    a: A,
    b: B,
    c: C,
) -> (A::Output, B::Output, C::Output) {
    let (mut a, mut b, mut c) = (Box::pin(a), Box::pin(b), Box::pin(c));
    let (mut ra, mut rb, mut rc) = (None, None, None);
    std::future::poll_fn(|context| {
        use std::task::Poll;
        if ra.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(context)
        {
            ra = Some(value);
        }
        if rb.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(context)
        {
            rb = Some(value);
        }
        if rc.is_none()
            && let Poll::Ready(value) = c.as_mut().poll(context)
        {
            rc = Some(value);
        }
        if ra.is_some() && rb.is_some() && rc.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (
        ra.unwrap_or_else(|| unreachable!()),
        rb.unwrap_or_else(|| unreachable!()),
        rc.unwrap_or_else(|| unreachable!()),
    )
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index] as f64 / 1000.0
}

#[derive(Clone, Copy)]
enum Kind {
    /// Each worker appends one row to its own scope.
    Append,
    /// Every worker claims the next seq of one shared scope (read head, append there, retry on
    /// conflict): a board claim.
    Claim,
}

fn measure(
    runtime: &nanocode_executor::Runtime,
    log: &Arc<Log>,
    sql: &Arc<Counted>,
    kind: Kind,
    workers: usize,
    seconds: u64,
    run: &str,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let retries = Arc::new(AtomicU64::new(0));
    let trips_before = sql.trips.load(Ordering::Relaxed);
    let stats_before = log.stats();
    let started = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|worker| {
            let (log, stop, retries) = (Arc::clone(log), Arc::clone(&stop), Arc::clone(&retries));
            let scope = match kind {
                Kind::Append => format!("bench/{run}/{workers}/{worker}"),
                Kind::Claim => format!("claim/{run}"),
            };
            runtime.spawn(async move {
                let mut mine = Vec::new();
                let mut next = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    let began = Instant::now();
                    match kind {
                        Kind::Append => {
                            if log
                                .append(&scope, &[(next, format!("d{worker}#{next}:1"))])
                                .await
                                .inspect_err(|error| {
                                    FIRST_ERROR.rcu(|first| {
                                        first.clone().or_else(|| Some(Arc::new(error.to_string())))
                                    });
                                })
                                .is_ok()
                            {
                                next += 1;
                            } else {
                                retries.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Kind::Claim => loop {
                            let head = log
                                .scan(&scope, next, u64::MAX)
                                .await
                                .ok()
                                .and_then(|rows| rows.last().map(|(seq, _)| seq + 1))
                                .unwrap_or(next);
                            next = head;
                            if log
                                .append(&scope, &[(head, format!("claim {worker}"))])
                                .await
                                .is_ok()
                            {
                                next = head + 1;
                                break;
                            }
                            retries.fetch_add(1, Ordering::Relaxed);
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                        },
                    }
                    mine.push(u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
                mine
            })
        })
        .collect();
    let mut all = Vec::new();
    runtime.block_on(async {
        nanocode_executor::sleep(Duration::from_secs(seconds)).await;
        stop.store(true, Ordering::Relaxed);
        for handle in handles {
            if let Ok(mine) = handle.await {
                all.extend(mine);
            }
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    all.sort_unstable();
    let stats = log.stats();
    let commits = stats.commits - stats_before.commits;
    let appends = stats.appends - stats_before.appends;
    let trips = sql.trips.load(Ordering::Relaxed) - trips_before;
    let name = match kind {
        Kind::Append => "appends",
        Kind::Claim => "claims",
    };
    let ops = all.len() as f64;
    println!(
        "METRIC kind={name} concurrency={workers} per_s={:.0} p50_ms={:.2} p99_ms={:.2} round_trips_per_op={:.2} appends_per_commit={:.2} retries={} seconds={elapsed:.1} first_error={:?}",
        ops / elapsed,
        percentile(&all, 0.5),
        percentile(&all, 0.99),
        trips as f64 / ops.max(1.0),
        appends as f64 / (commits as f64).max(1.0),
        retries.load(Ordering::Relaxed),
        FIRST_ERROR.swap(None),
    );
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "postgres://nc@127.0.0.1:5544/plog".to_owned());
    let seconds: u64 = args.next().and_then(|text| text.parse().ok()).unwrap_or(10);
    let connections: usize = args.next().and_then(|text| text.parse().ok()).unwrap_or(16);
    let parts: u32 = args.next().and_then(|text| text.parse().ok()).unwrap_or(16);
    let Ok(runtime) = nanocode_executor::Builder::new_multi_thread()
        .worker_threads(8)
        .build()
    else {
        eprintln!("no runtime");
        return std::process::ExitCode::FAILURE;
    };
    let options = match PgOptions::parse(&url) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("bad url: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let pool = PgPool::new(options.clone(), connections);
    let sql = Arc::new(Counted {
        inner: PgSql::reading(pool.clone(), pool),
        trips: AtomicU64::new(0),
    });
    let mut failed = 0;
    if let Err(error) = runtime.block_on(conformance(Arc::clone(&sql), &mut failed)) {
        println!("RESULT FAIL conformance aborted {error}");
        failed += 1;
    }
    let rung = Arc::new(AtomicU64::new(0));
    let heard = Arc::clone(&rung);
    let wake_options = options;
    runtime.spawn(async move {
        let heard = heard;
        let _ = log_wake::listen(&wake_options, &|| {}, &|_, _| {
            heard.fetch_add(1, Ordering::Relaxed);
        })
        .await;
    });
    let Some(count) = NonZeroU32::new(parts) else {
        return std::process::ExitCode::FAILURE;
    };
    let log = Arc::new(PartedLog::fenced(
        Arc::new(SqlParts::new(Arc::clone(&sql))),
        count,
        "bench",
    ));
    let run = format!(
        "{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
    let claimed = runtime.block_on(async {
        log.migrate().await?;
        log.prepare().await?;
        for part in 0..parts {
            let _ = log.claim(Partition(part), u64::MAX / 4).await;
        }
        Ok::<(), StoreError>(())
    });
    if let Err(error) = claimed {
        println!("RESULT FAIL bench setup {error}");
        return std::process::ExitCode::FAILURE;
    }
    let held = (0..parts)
        .filter(|part| log.held(Partition(*part)).is_some())
        .count();
    if held != parts as usize {
        println!(
            "RESULT FAIL bench setup claimed {held} of {parts} partitions: another holder's lease is live; release it or wait for it to lapse"
        );
        return std::process::ExitCode::FAILURE;
    }
    for workers in [1, 16, 64] {
        measure(&runtime, &log, &sql, Kind::Append, workers, seconds, &run);
    }
    for workers in [1, 16, 64] {
        measure(
            &runtime,
            &log,
            &sql,
            Kind::Claim,
            workers,
            seconds,
            &format!("{run}-{workers}"),
        );
    }
    let committed = log.stats().appends;
    std::thread::sleep(Duration::from_millis(500));
    let heard = rung.load(Ordering::Relaxed);
    check(
        "listener heard a wake for committed appends",
        heard > 0,
        &format!("heard={heard} committed={committed}"),
        &mut failed,
    );
    runtime.block_on(async {
        for part in 0..parts {
            let _ = log.release(Partition(part)).await;
        }
    });
    if failed == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

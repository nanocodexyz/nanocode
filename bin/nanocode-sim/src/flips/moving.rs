use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_turnlog::{
    Compaction, Extension, LEGACY_OPEN_RETRIES, LogHost, Rider, ScopeRows, StoredCheckpoint, Write,
};

use super::FlipsStats;
use crate::{
    admitturns::READ_MS,
    rng::Rng,
    threadlegacy,
    threadscope::{World, driven, flag},
};

const MOVING: &str = "flips-moving";
const MOVES: u32 = 2 * LEGACY_OPEN_RETRIES;
const MOVE_WRITES: u32 = 8;
const BOUNDED_PPM: u64 = 500_000;
const BOUND_MOVES: Range<u64> = 2..2 * LEGACY_OPEN_RETRIES as u64 + 4;

struct Moving {
    log: Arc<dyn LogHost>,
    seed: u64,
    bound: Option<Duration>,
    loaded: AtomicBool,
    moves: AtomicU32,
    unsuperseded: AtomicBool,
}

trait Seqs {
    fn seqs(&self) -> Vec<u64>;
}

impl Seqs for ScopeRows {
    fn seqs(&self) -> Vec<u64> {
        self.iter().map(|(seq, _)| *seq).collect()
    }
}

impl Seqs for Vec<ScopeRows> {
    fn seqs(&self) -> Vec<u64> {
        self.iter().flat_map(Seqs::seqs).collect()
    }
}

impl Moving {
    async fn written(&self, moves: u32, write: u32) -> Result<(), StoreError> {
        let mut rng = Rng::derive(self.seed, &format!("flips-moving-{moves}-{write}"));
        let prefix = format!("m{moves}w{write}");
        threadlegacy::write_named(&mut rng, Arc::clone(&self.log), MOVING, &prefix)
            .await
            .map(|_| ())
            .map_err(StoreError::NotCommitted)
    }

    async fn read<'a, T: Seqs, F: Future<Output = Result<T, StoreError>> + 'a>(
        &'a self,
        call: impl Fn() -> F,
    ) -> Result<T, StoreError> {
        let moving =
            self.loaded.swap(false, Ordering::AcqRel) && self.moves.load(Ordering::Acquire) < MOVES;
        if !moving {
            return call().await;
        }
        let moves = self.moves.fetch_add(1, Ordering::AcqRel);
        nanocode_platform::sleep(Duration::from_millis(READ_MS)).await;
        let before = call().await?.seqs();
        for write in 0..MOVE_WRITES {
            self.written(moves, write).await?;
            let after = call().await?;
            let seqs = after.seqs();
            if before.iter().any(|seq| !seqs.contains(seq)) {
                return Ok(after);
            }
        }
        self.unsuperseded.store(true, Ordering::Release);
        Err(StoreError::NotCommitted(format!(
            "the old writer of {MOVING} superseded none of {before:?} in {MOVE_WRITES} histories"
        )))
    }
}

impl LogHost for Moving {
    fn call_bound(&self) -> Option<Duration> {
        self.bound
    }

    fn read_capacity(&self) -> u32 {
        self.log.read_capacity()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        Box::pin(self.read(move || self.log.scan(scope, from, to)))
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.read(move || self.log.scans(scans)))
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.read(move || self.log.scan_ranges(scope, ranges)))
    }

    fn load<'a>(&'a self, scope: &'a str) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        self.loaded.store(true, Ordering::Release);
        self.log.load(scope)
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

fn opened(seed: u64, world: &World, rng: &mut Rng) -> Option<Moving> {
    let host = world.host();
    let written = driven(
        world,
        "a moving old writer's history",
        threadlegacy::write_named(rng, Arc::clone(&host), MOVING, "moving"),
    )?;
    if let Err(error) = written {
        flag(&world.wrong, format!("flips: a moving old writer: {error}"));
        return None;
    }
    let bound = rng
        .chance(BOUNDED_PPM)
        .then(|| Duration::from_millis(READ_MS * rng.range(BOUND_MOVES.start, BOUND_MOVES.end)));
    Some(Moving {
        log: host,
        seed,
        bound,
        loaded: AtomicBool::new(false),
        moves: AtomicU32::new(0),
        unsuperseded: AtomicBool::new(false),
    })
}

pub(super) fn checked(seed: u64, world: &World, stats: &mut FlipsStats) -> Option<()> {
    let mut rng = Rng::derive(seed, "flips-moving");
    let moving = Arc::new(opened(seed, world, &mut rng)?);
    let bound = moving.bound;
    let started = nanocode_platform::now();
    let opened = driven(
        world,
        "an old writer's open while the log's writer keeps landing snapshots",
        nanocode_turnlog::legacy::open(Arc::clone(&moving) as Arc<dyn LogHost>, MOVING),
    )?;
    let waited = nanocode_platform::elapsed(started);
    let moves = moving.moves.load(Ordering::Acquire);
    if moving.unsuperseded.load(Ordering::Acquire) {
        flag(
            &world.wrong,
            format!("flips: {MOVING} kept every row an open read past {MOVE_WRITES} histories"),
        );
        return None;
    }
    let wrong = match opened {
        Ok(_) if moves == 0 => {
            stats.unmoved += 1;
            return Some(());
        }
        Ok(_) => {
            format!("lands after {moves} moving snapshots, past {LEGACY_OPEN_RETRIES} reopens")
        }
        Err(StoreError::Queued { ahead, depth, .. }) => {
            match refused(ahead, depth, waited, bound) {
                None => {
                    stats.refused += 1;
                    return Some(());
                }
                Some(wrong) => wrong,
            }
        }
        Err(error) => format!("answers {error} after {moves} moving snapshots"),
    };
    flag(
        &world.wrong,
        format!("flips: an old writer's open while its log moves (bound {bound:?}) {wrong}"),
    );
    None
}

fn refused(ahead: u32, depth: u32, waited: Duration, bound: Option<Duration>) -> Option<String> {
    let passes = LEGACY_OPEN_RETRIES + 1;
    let pass = Duration::from_millis(READ_MS);
    if ahead > passes {
        return Some(format!("names {ahead} snapshots, past {passes} passes"));
    }
    if depth != 0 {
        return Some(format!("leaves {depth} jobs queued"));
    }
    if bound.is_none() && ahead != passes {
        return Some(format!(
            "refuses after {ahead} snapshots of {passes} passes, unbounded"
        ));
    }
    let deadline = bound.map_or(pass * passes, |bound| bound + pass);
    if waited > deadline {
        return Some(format!("refuses after {waited:?}, past {deadline:?}"));
    }
    None
}

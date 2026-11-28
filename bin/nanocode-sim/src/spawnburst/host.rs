use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::Atom;
use nanocode_turnlog::{Compaction, LogHost, MemoryLog};

use super::SpawnBurstStats;
use crate::rng::{FNV_OFFSET, Rng, fnv};

pub(super) const CRASH_PPM: u64 = 300_000;
const HOST_CALL_BOUND_MS: u64 = 15_000;
const LATENCY_MS_MIN: u64 = 1;
const LATENCY_MS_MAX: u64 = 5_000;
const STALL_PPM_MAX: u64 = 100_000;
const WRITES_PER_SPAWN: u64 = 4;

pub(super) const fn call_bound() -> Duration {
    Duration::from_millis(HOST_CALL_BOUND_MS)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Faults {
    latency_ms_max: u64,
    stall_ppm: u64,
}

impl Faults {
    pub(super) fn of(rng: &mut Rng) -> Self {
        Self {
            latency_ms_max: rng.range(LATENCY_MS_MIN, LATENCY_MS_MAX),
            stall_ppm: rng.range(0, STALL_PPM_MAX),
        }
    }

    pub(super) const fn burst_ms_max(self, spawns: u64) -> u64 {
        spawns
            .saturating_mul(WRITES_PER_SPAWN)
            .saturating_mul(self.latency_ms_max)
            .saturating_add(LATENCY_MS_MIN)
    }

    pub(super) fn describe(self) -> String {
        format!(
            "latency_ms<={} stall_ppm={}",
            self.latency_ms_max, self.stall_ppm
        )
    }
}

pub(super) struct FaultHost {
    log: MemoryLog,
    parent: &'static str,
    seed: u64,
    faults: Faults,
    calls: AtomicU64,
    appends: AtomicU64,
    stalls: AtomicU64,
    scans: AtomicU64,
    parent_appends: AtomicU64,
    parent_scans: AtomicU64,
    child_appends: AtomicU64,
    scoped_scans: Atom<BTreeMap<String, u64>>,
    digest: AtomicU64,
    calm: AtomicBool,
    poisoned: AtomicBool,
}

impl FaultHost {
    pub(super) fn new(seed: u64, faults: Faults, parent: &'static str) -> Self {
        Self {
            log: MemoryLog::new(),
            parent,
            seed,
            faults,
            calls: AtomicU64::new(0),
            appends: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            scans: AtomicU64::new(0),
            parent_appends: AtomicU64::new(0),
            parent_scans: AtomicU64::new(0),
            child_appends: AtomicU64::new(0),
            scoped_scans: Atom::default(),
            calm: AtomicBool::new(true),
            poisoned: AtomicBool::new(false),
            digest: AtomicU64::new(fnv(FNV_OFFSET, &seed.to_le_bytes())),
        }
    }

    pub(super) fn storm(&self) {
        self.calm.store(false, Ordering::Relaxed);
    }

    pub(super) fn poison(&self) {
        self.poisoned.store(true, Ordering::Relaxed);
    }

    pub(super) fn poisoned(&self) -> bool {
        self.poisoned.swap(false, Ordering::Relaxed)
    }

    fn delay(&self) -> Duration {
        if self.calm.load(Ordering::Relaxed) {
            return Duration::ZERO;
        }
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let mut rng = Rng::derive(self.seed ^ call, "spawn-burst-call");
        let latency_ms = rng.range(LATENCY_MS_MIN, self.faults.latency_ms_max);
        if rng.chance(self.faults.stall_ppm) {
            self.stalls.fetch_add(1, Ordering::Relaxed);
            return Duration::from_millis(rng.range(latency_ms, HOST_CALL_BOUND_MS));
        }
        Duration::from_millis(latency_ms)
    }

    fn bursting(&self, scope: &str) -> Option<&AtomicU64> {
        if self.calm.load(Ordering::Relaxed) {
            return None;
        }
        Some(if scope == self.parent {
            &self.parent_appends
        } else {
            &self.child_appends
        })
    }

    pub(super) fn scans_of(&self, scope: &str) -> u64 {
        self.scoped_scans
            .read(|scans| scans.get(scope).copied().unwrap_or(0))
    }

    fn scanned(&self, scope: &str) {
        self.scans.fetch_add(1, Ordering::Relaxed);
        self.scoped_scans
            .update(|scans| *scans.entry(scope.to_owned()).or_insert(0) += 1);
        if scope == self.parent && !self.calm.load(Ordering::Relaxed) {
            self.parent_scans.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn landed(&self, scope: &str, rows: &[(u64, String)]) {
        self.appends.fetch_add(1, Ordering::Relaxed);
        if let Some(appends) = self.bursting(scope) {
            appends.fetch_add(1, Ordering::Relaxed);
        }
        let mut digest = self.digest.load(Ordering::Relaxed);
        for (seq, body) in rows {
            digest = fnv(fnv(digest, &seq.to_le_bytes()), body.as_bytes());
        }
        self.digest.store(digest, Ordering::Relaxed);
    }

    pub(super) fn digest(&self) -> u64 {
        self.digest.load(Ordering::Relaxed)
    }

    pub(super) fn count(&self, stats: &mut SpawnBurstStats) {
        stats.appends += self.appends.load(Ordering::Relaxed);
        stats.stalls += self.stalls.load(Ordering::Relaxed);
        stats.scans += self.scans.load(Ordering::Relaxed);
        stats.parent_appends += self.parent_appends.load(Ordering::Relaxed);
        stats.parent_scans += self.parent_scans.load(Ordering::Relaxed);
        stats.child_appends += self.child_appends.load(Ordering::Relaxed);
    }
}

impl LogHost for FaultHost {
    fn call_bound(&self) -> Option<Duration> {
        Some(call_bound())
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        let delay = self.delay();
        self.scanned(scope);
        Box::pin(async move {
            nanocode_platform::sleep(delay).await;
            self.log.scan(scope, from, to).await
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        let delay = self.delay();
        Box::pin(async move {
            nanocode_platform::sleep(delay).await;
            self.log.load(scope).await
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let delay = self.delay();
        Box::pin(async move {
            nanocode_platform::sleep(delay).await;
            self.log.append(scope, rows).await?;
            self.landed(scope, rows);
            Ok(())
        })
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let delay = self.delay();
        Box::pin(async move {
            nanocode_platform::sleep(delay).await;
            self.log.append_compacting(scope, rows, compaction).await?;
            self.landed(scope, rows);
            Ok(())
        })
    }
}

impl std::fmt::Debug for FaultHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FaultHost")
            .field("faults", &self.faults)
            .finish_non_exhaustive()
    }
}

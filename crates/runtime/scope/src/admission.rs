use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use nanocode_durability_core::StoreError;
use nanocode_sync::{AcquireError, Notify, Semaphore, SemaphorePermit};

use crate::sim::{self, Mutation};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub(crate) admissions: u64,
    pub(crate) busy: u64,
    pub(crate) backfill_reads: u64,
    pub(crate) queued: u64,
    pub(crate) lanes: u64,
}

pub(crate) struct Admission {
    reads: Semaphore,
    capacity: u32,
    admit_ms: u64,
    serving: AtomicU64,
    backfilling: AtomicU64,
    freed: Notify,
    admissions: AtomicU64,
    busy: AtomicU64,
    backfill_reads: AtomicU64,
    queued: AtomicU64,
    lanes: AtomicU64,
    clean: AtomicU64,
    base: nanocode_platform::Instant,
    idle_since_ns: AtomicU64,
    read_ns: AtomicU64,
}

const BUSY: u64 = u64::MAX;

struct Demand<'a>(&'a Admission);

impl Drop for Demand<'_> {
    fn drop(&mut self) {
        let before = self.0.serving.fetch_sub(1, Ordering::SeqCst);
        assert!(before > 0, "a serving read leaves the demand it joined");
        if before == 1 {
            self.0
                .idle_since_ns
                .store(self.0.clock_ns(), Ordering::SeqCst);
        }
        self.0.freed.notify_waiters();
    }
}

struct Lane<'a> {
    admission: &'a Admission,
    permit: Option<SemaphorePermit<'a>>,
    queued: u64,
    started: nanocode_platform::Instant,
}

impl Drop for Lane<'_> {
    fn drop(&mut self) {
        drop(self.permit.take());
        let before = self.admission.backfilling.fetch_sub(1, Ordering::SeqCst);
        assert!(before > 0, "a backfill read leaves the lane it took");
        let read_ns =
            u64::try_from(nanocode_platform::elapsed(self.started).as_nanos()).unwrap_or(BUSY - 1);
        self.admission.read_ns.store(read_ns, Ordering::SeqCst);
        self.admission.widened(self.queued);
        self.admission.freed.notify_waiters();
    }
}

impl Admission {
    pub(crate) fn new(capacity: u32, admit_ms: u64) -> Self {
        assert!(
            admit_ms > 0,
            "a read waits for admission a while before Busy"
        );
        assert!(capacity > 0, "a host admits at least one read at a time");
        let permits = usize::try_from(capacity).expect("a read capacity fits in usize");
        Self {
            reads: Semaphore::new(permits),
            capacity,
            admit_ms,
            serving: AtomicU64::new(0),
            backfilling: AtomicU64::new(0),
            freed: Notify::new(),
            admissions: AtomicU64::new(0),
            busy: AtomicU64::new(0),
            backfill_reads: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            lanes: AtomicU64::new(0),
            clean: AtomicU64::new(0),
            base: nanocode_platform::now(),
            idle_since_ns: AtomicU64::new(0),
            read_ns: AtomicU64::new(0),
        }
    }

    pub(crate) const fn capacity(&self) -> u32 {
        self.capacity
    }

    pub(crate) fn admitted_so_far(&self) -> Admitted {
        Admitted {
            admissions: self.admissions.load(Ordering::Relaxed),
            busy: self.busy.load(Ordering::Relaxed),
            backfill_reads: self.backfill_reads.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            lanes: self.lanes.load(Ordering::Relaxed),
        }
    }

    fn clock_ns(&self) -> u64 {
        u64::try_from(nanocode_platform::elapsed(self.base).as_nanos()).unwrap_or(BUSY - 1)
    }

    fn idle_wait_ns(&self) -> Option<u64> {
        let since = self.idle_since_ns.load(Ordering::SeqCst);
        if since == BUSY || self.serving.load(Ordering::SeqCst) > 0 {
            return None;
        }
        let idle = self.clock_ns().saturating_sub(since);
        Some(self.read_ns.load(Ordering::SeqCst).saturating_sub(idle))
    }

    fn paced() -> bool {
        !sim::mutated(Mutation::BackfillIgnoresServingLoad)
    }

    fn cut(&self) {
        let held = self.backfilling.load(Ordering::SeqCst);
        if held == 0 || !Self::paced() {
            return;
        }
        let before = self
            .lanes
            .update(Ordering::SeqCst, Ordering::SeqCst, |lanes| {
                lanes.min(held) / 2
            });
        assert!(
            self.lanes.load(Ordering::SeqCst) <= before,
            "a queued serving read never widens the backfill"
        );
        self.clean.store(0, Ordering::SeqCst);
    }

    fn widened(&self, queued: u64) {
        if self.queued.load(Ordering::SeqCst) != queued {
            return;
        }
        let lanes = self.lanes.load(Ordering::SeqCst);
        let clean = self.clean.fetch_add(1, Ordering::SeqCst) + 1;
        if clean < lanes.max(1) || lanes >= u64::from(self.capacity) {
            return;
        }
        self.clean.store(0, Ordering::SeqCst);
        let widened = self
            .lanes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |held| {
                (held == lanes).then_some(lanes + 1)
            });
        assert!(
            widened.is_err() || lanes < u64::from(self.capacity),
            "the backfill never widens past the read capacity"
        );
    }

    pub(crate) fn passes(&self) -> u64 {
        let capacity = u64::from(self.capacity);
        if !Self::paced() {
            return capacity;
        }
        let lanes = self.lanes.load(Ordering::SeqCst);
        let allowed = match self.idle_wait_ns() {
            Some(0) => lanes.max(1),
            Some(_) | None => lanes,
        };
        assert!(
            allowed <= capacity,
            "the backfill holds at most the read capacity"
        );
        allowed
    }

    pub(crate) async fn admitted<T, E: From<StoreError>>(
        &self,
        read: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        if sim::mutated(Mutation::AdmissionUnbounded) {
            return read.await;
        }
        if self.serving.fetch_add(1, Ordering::SeqCst) == 0 {
            self.idle_since_ns.store(BUSY, Ordering::SeqCst);
        }
        let demand = Demand(self);
        let deadline = nanocode_platform::sleep(Duration::from_millis(self.admit_ms));
        let free = self.reads.try_acquire().ok();
        if free.is_none() {
            self.queued.fetch_add(1, Ordering::SeqCst);
            self.cut();
        }
        let acquired = match free {
            Some(permit) => Ok(permit),
            None => self.reads.acquire_within(deadline).await,
        };
        let permit = match acquired {
            Ok(permit) => permit,
            Err(AcquireError::Busy) => {
                self.busy.fetch_add(1, Ordering::Relaxed);
                return Err(E::from(StoreError::Unadmitted {
                    capacity: self.capacity,
                    admit_ms: self.admit_ms,
                }));
            }
            Err(AcquireError::Closed) => panic!("the read admission is never closed"),
        };
        self.admissions.fetch_add(1, Ordering::Relaxed);
        let answer = read.await;
        drop(permit);
        drop(demand);
        answer
    }

    fn lane(&self) -> Option<Lane<'_>> {
        let serving = self.serving.load(Ordering::SeqCst);
        let backfilling = self.backfilling.load(Ordering::SeqCst);
        if serving + backfilling >= u64::from(self.capacity) {
            return None;
        }
        if backfilling >= self.passes() {
            return None;
        }
        let queued = self.queued.load(Ordering::SeqCst);
        let permit = self.reads.try_acquire().ok()?;
        self.backfilling.fetch_add(1, Ordering::SeqCst);
        Some(Lane {
            admission: self,
            permit: Some(permit),
            queued,
            started: nanocode_platform::now(),
        })
    }

    pub(crate) async fn backfilled<T, E: From<StoreError>>(
        &self,
        read: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        if sim::mutated(Mutation::AdmissionUnbounded)
            || sim::mutated(Mutation::BackfillTakesServingLanes)
            || sim::mutated(Mutation::AdoptInRequest)
        {
            return read.await;
        }
        let lane = loop {
            let freed = self.freed.notified();
            let mut freed = std::pin::pin!(freed);
            freed.as_mut().enable();
            if let Some(lane) = self.lane() {
                break lane;
            }
            match self.idle_wait_ns().filter(|wait| *wait > 0) {
                Some(wait) => {
                    let waited =
                        nanocode_platform::timeout(Duration::from_nanos(wait), freed).await;
                    drop(waited);
                }
                None => freed.await,
            }
        };
        assert!(
            self.backfilling.load(Ordering::SeqCst) <= u64::from(self.capacity),
            "backfill reads never pass the read capacity"
        );
        self.backfill_reads.fetch_add(1, Ordering::Relaxed);
        let answer = read.await;
        drop(lane);
        answer
    }
}

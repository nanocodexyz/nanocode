use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::FutureExt;

use crate::{Compaction, Extension, LogHost, Rider, ScopeRows, StoredCheckpoint, Write};

pub(crate) struct Bounded {
    pub(crate) host: Arc<dyn LogHost>,
    pub(crate) within: Duration,
    pub(crate) answered: Arc<AtomicU64>,
}

pub(crate) fn own(host: &dyn LogHost) -> Option<Duration> {
    if crate::sim::mutated(crate::sim::Mutation::HostBoundIgnored) {
        return None;
    }
    host.call_bound()
}

pub(crate) fn bounded(raw: &Arc<dyn LogHost>, within: Option<Duration>) -> Arc<dyn LogHost> {
    counted(raw, within, &Arc::default())
}

pub(crate) fn counted(
    raw: &Arc<dyn LogHost>,
    within: Option<Duration>,
    answered: &Arc<AtomicU64>,
) -> Arc<dyn LogHost> {
    match within {
        Some(within) => Arc::new(Bounded {
            host: Arc::clone(raw),
            within,
            answered: Arc::clone(answered),
        }),
        None => Arc::clone(raw),
    }
}

impl Bounded {
    async fn timed<T>(
        &self,
        call: &'static str,
        scope: &str,
        future: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let answer = within(self.within, call, scope, future).await;
        self.answered.fetch_add(1, Ordering::AcqRel);
        answer
    }
}

pub(crate) fn unanswered(call: &str, scope: &str, within: Duration) -> StoreError {
    StoreError::Backend(format!(
        "turn log host {call} of {scope} did not answer within {} ms, so its outcome is unknown",
        within.as_millis()
    ))
}

pub(crate) async fn within<T>(
    within: Duration,
    call: &'static str,
    scope: &str,
    future: impl Future<Output = Result<T, StoreError>>,
) -> Result<T, StoreError> {
    let mut future = std::pin::pin!(future);
    if let Some(done) = future.as_mut().now_or_never() {
        return done;
    }
    if crate::sim::mutated(crate::sim::Mutation::HostUnbounded) {
        return future.await;
    }
    nanocode_platform::timeout(within, future)
        .await
        .unwrap_or_else(|_| Err(unanswered(call, scope, within)))
}

impl LogHost for Bounded {
    fn call_bound(&self) -> Option<std::time::Duration> {
        Some(self.within)
    }

    fn read_capacity(&self) -> u32 {
        self.host.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.host.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(self.timed("scan",
            scope,
            self.host.scan(scope, from, to),
        ))
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        let scope = scans.first().map_or("", |(scope, _)| *scope);
        Box::pin(self.timed("scans", scope, self.host.scans(scans)))
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [std::ops::Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(self.timed("scan_ranges",
            scope,
            self.host.scan_ranges(scope, ranges),
        ))
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(self.timed("load", scope, self.host.load(scope)))
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(self.timed("append",
            scope,
            self.host.append(scope, rows),
        ))
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(self.timed("append",
            scope,
            self.host.append_compacting(scope, rows, compaction),
        ))
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let extending = self.host.extend(scope, extension)?;
        Some(Box::pin(self.timed("extend", scope, extending)))
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let carrying = self.host.carry(scope, write, rider)?;
        Some(Box::pin(self.timed("append", scope, carrying)))
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        let reading = self.host.checkpoint(scope)?;
        Some(Box::pin(self.timed("checkpoint", scope, reading)))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let saving = self.host.save_checkpoint(scope, checkpoint)?;
        Some(Box::pin(self.timed("checkpoint save",
            scope,
            saving,
        )))
    }
}

use std::{
    future::Future,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::{Atom, OrdMap};
use nanocode_turnlog::{Compaction, Extension, LogHost, Rider, ScopeRows, StoredCheckpoint, Write};

use crate::{
    admission::Admission,
    checkpoint::{Checkpoints, Fence, Listing, Manifest, Written},
    sim::{self, Mutation},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    pub calls: u64,
    pub rows: u64,
    pub bytes: u64,
    pub ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub load: Work,
    pub append: Work,
    pub checkpoint: Work,
    pub adopt: Work,
    pub history: Work,
    pub legacy: Work,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Load,
    Append,
    Checkpoint,
    Adopt,
    History,
    Legacy,
}

#[derive(Default)]
struct Counters {
    calls: AtomicU64,
    rows: AtomicU64,
    bytes: AtomicU64,
    ms: AtomicU64,
}

impl Counters {
    fn add(&self, work: Work) {
        assert!(work.calls > 0, "a counted call is one call or more");
        self.calls.fetch_add(work.calls, Ordering::Relaxed);
        self.rows.fetch_add(work.rows, Ordering::Relaxed);
        self.bytes.fetch_add(work.bytes, Ordering::Relaxed);
        self.ms.fetch_add(work.ms, Ordering::Relaxed);
    }

    fn work(&self) -> Work {
        Work {
            calls: self.calls.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            ms: self.ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
pub(crate) struct Tally {
    load: Counters,
    append: Counters,
    checkpoint: Counters,
    adopt: Counters,
    history: Counters,
    legacy: Counters,
}

impl Tally {
    const fn class(&self, class: Class) -> &Counters {
        match class {
            Class::Load => &self.load,
            Class::Append => &self.append,
            Class::Checkpoint => &self.checkpoint,
            Class::Adopt => &self.adopt,
            Class::History => &self.history,
            Class::Legacy => &self.legacy,
        }
    }

    fn add(&self, class: Class, work: Work) {
        self.class(class).add(work);
    }

    pub(crate) fn usage(&self) -> Usage {
        Usage {
            load: self.load.work(),
            append: self.append.work(),
            checkpoint: self.checkpoint.work(),
            adopt: self.adopt.work(),
            history: self.history.work(),
            legacy: self.legacy.work(),
        }
    }
}

pub(crate) struct Meter {
    host: Arc<dyn LogHost>,
    checkpoints: Arc<dyn Checkpoints>,
    admission: Admission,
    totals: Tally,
    tallies: Atom<OrdMap<Arc<str>, Arc<Tally>>>,
}

pub(crate) struct Logs {
    pub(crate) load: Arc<dyn LogHost>,
    pub(crate) append: Arc<dyn LogHost>,
    pub(crate) adopt: Arc<dyn LogHost>,
    pub(crate) history: Arc<dyn LogHost>,
    pub(crate) legacy: Arc<dyn LogHost>,
}

struct View {
    meter: Arc<Meter>,
    class: Class,
}

fn sent(rows: &[(u64, String)]) -> (u64, u64) {
    let bytes: usize = rows.iter().map(|(_, body)| body.len()).sum();
    (rows.len() as u64, bytes as u64)
}

fn compacted(rows: &[(u64, String)], compaction: &Compaction<'_>) -> (u64, u64) {
    let (rows, bytes) = sent(rows);
    let (archived, archived_bytes) = sent(compaction.archived);
    (rows + archived, bytes + archived_bytes)
}

fn listed(parts: &[(String, String)]) -> (u64, u64) {
    let bytes: usize = parts
        .iter()
        .map(|(name, body)| name.len() + body.len())
        .sum();
    (parts.len() as u64, bytes as u64)
}

fn millis(started: nanocode_platform::Instant) -> u64 {
    u64::try_from(nanocode_platform::elapsed(started).as_millis()).unwrap_or(u64::MAX)
}

impl Meter {
    pub(crate) fn new(
        host: Arc<dyn LogHost>,
        checkpoints: Arc<dyn Checkpoints>,
        admit_ms: u64,
    ) -> (Arc<Self>, Logs) {
        let admission = Admission::new(host.read_capacity(), admit_ms);
        let meter = Arc::new(Self {
            host,
            checkpoints,
            admission,
            totals: Tally::default(),
            tallies: Atom::default(),
        });
        let view = |class| -> Arc<dyn LogHost> {
            Arc::new(View {
                meter: Arc::clone(&meter),
                class,
            })
        };
        let logs = Logs {
            load: view(Class::Load),
            append: view(Class::Append),
            adopt: view(Class::Adopt),
            history: view(Class::History),
            legacy: view(Class::Legacy),
        };
        (meter, logs)
    }

    pub(crate) const fn admission(&self) -> &Admission {
        &self.admission
    }

    pub(crate) fn usage(&self) -> Usage {
        self.totals.usage()
    }

    pub(crate) fn enter(&self, scope: &Arc<str>) -> Arc<Tally> {
        let tally = Arc::new(Tally::default());
        self.tallies.update(|tallies| {
            tallies.insert(Arc::clone(scope), Arc::clone(&tally));
        });
        tally
    }

    pub(crate) fn leave(&self, scope: &str, tally: &Arc<Tally>) {
        self.tallies.update(|tallies| {
            if tallies
                .get(scope)
                .is_some_and(|kept| Arc::ptr_eq(kept, tally))
            {
                tallies.remove(scope);
            }
        });
    }

    fn count(&self, class: Class, scope: &str, rows: u64, bytes: u64, ms: u64) {
        if class == Class::Append && sim::mutated(Mutation::MeterSkipsWriterTask) {
            return;
        }
        let work = Work {
            calls: 1,
            rows,
            bytes,
            ms,
        };
        self.totals.add(class, work);
        if let Some(tally) = self.tallies.read(|tallies| tallies.get(scope).cloned()) {
            tally.add(class, work);
        }
    }

    async fn read<T>(
        &self,
        class: Class,
        scope: &str,
        call: impl Future<Output = Result<T, StoreError>>,
        measure: impl FnOnce(&T) -> (u64, u64),
    ) -> Result<T, StoreError> {
        let started = nanocode_platform::now();
        let answer = call.await;
        let (rows, bytes) = answer.as_ref().map_or((0, 0), measure);
        self.count(class, scope, rows, bytes, millis(started));
        answer
    }

    async fn written<T>(
        &self,
        class: Class,
        scope: &str,
        (rows, bytes): (u64, u64),
        call: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let started = nanocode_platform::now();
        let answer = call.await;
        self.count(class, scope, rows, bytes, millis(started));
        answer
    }

    pub(crate) async fn latest(&self, scope: &str) -> Result<Option<Manifest>, StoreError> {
        self.read(
            Class::Checkpoint,
            scope,
            self.checkpoints.latest(scope),
            |manifest| {
                manifest
                    .as_ref()
                    .map_or((0, 0), |manifest| listed(&manifest.parts))
            },
        )
        .await
    }

    pub(crate) async fn parts(
        &self,
        scope: &str,
        digests: &[String],
    ) -> Result<Vec<(String, String)>, StoreError> {
        let found = self
            .read(
                Class::Checkpoint,
                scope,
                self.checkpoints.parts(digests),
                |found| listed(found),
            )
            .await?;
        assert!(
            found.len() <= digests.len(),
            "the checkpoint store answers at most the parts asked for"
        );
        Ok(found)
    }

    pub(crate) async fn write(
        &self,
        fence: &Fence,
        manifest: &Manifest,
        listing: &Listing,
        parts: &[(String, String)],
    ) -> Result<Written, StoreError> {
        let (rows, bytes) = listed(parts);
        let measured = (rows + 1, bytes + listing.encoded().len() as u64);
        let written = self.checkpoints.write(fence, manifest, listing, parts);
        self.written(Class::Checkpoint, &manifest.scope, measured, written)
            .await
    }

    async fn admitted<T>(
        &self,
        class: Class,
        call: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        match class {
            Class::History => self.admission.admitted(call).await,
            Class::Legacy if self.serving() => self.admission.admitted(call).await,
            Class::Adopt => self.admission.backfilled(call).await,
            Class::Legacy | Class::Load | Class::Append | Class::Checkpoint => call.await,
        }
    }

    fn serving(&self) -> bool {
        sim::mutated(Mutation::LegacyAdmittedIdle)
            || self.tallies.read(|tallies| !tallies.is_empty())
    }
}

impl View {
    async fn scanned(
        &self,
        scope: &str,
        call: impl Future<Output = Result<ScopeRows, StoreError>>,
    ) -> Result<ScopeRows, StoreError> {
        let meter = &self.meter;
        let read = meter.read(self.class, scope, call, |rows| sent(rows));
        meter.admitted(self.class, read).await
    }
}

impl LogHost for View {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.meter.host.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.meter.host.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.meter.host.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        Box::pin(self.scanned(scope, self.meter.host.scan(scope, from, to)))
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let meter = &self.meter;
            let started = nanocode_platform::now();
            let answer = meter.admitted(self.class, meter.host.scans(scans)).await;
            let ms = millis(started);
            match &answer {
                Ok(found) => {
                    assert_eq!(found.len(), scans.len(), "a scans call answers each scope");
                    for ((scope, _), rows) in scans.iter().zip(found) {
                        let (rows, bytes) = sent(rows);
                        meter.count(self.class, scope, rows, bytes, ms);
                    }
                }
                Err(_) => {
                    for (scope, _) in scans {
                        meter.count(self.class, scope, 0, 0, ms);
                    }
                }
            }
            answer
        })
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let meter = &self.meter;
            let call = meter.host.scan_ranges(scope, ranges);
            let read = meter.read(self.class, scope, call, |found| {
                found
                    .iter()
                    .map(|rows| sent(rows))
                    .fold((0, 0), |(rows, bytes), (more, sized)| {
                        (rows + more, bytes + sized)
                    })
            });
            meter.admitted(self.class, read).await
        })
    }

    fn load<'a>(&'a self, scope: &'a str) -> StoreFuture<'a, Result<ScopeRows, StoreError>> {
        Box::pin(self.scanned(scope, self.meter.host.load(scope)))
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let call = self.meter.host.append(scope, rows);
        Box::pin(self.meter.written(self.class, scope, sent(rows), call))
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let measured = compacted(rows, &compaction);
        let call = self.meter.host.append_compacting(scope, rows, compaction);
        Box::pin(self.meter.written(self.class, scope, measured, call))
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let call = self.meter.host.extend(scope, extension)?;
        let measured = (1, extension.tail.len() as u64);
        Some(Box::pin(
            self.meter.written(self.class, scope, measured, call),
        ))
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let measured = match &write {
            Write::Append { rows, compaction } => compacted(rows, compaction),
            Write::Extend(extension) => (1, extension.tail.len() as u64),
        };
        let call = self.meter.host.carry(scope, write, rider)?;
        Some(Box::pin(
            self.meter.written(self.class, scope, measured, call),
        ))
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        let call = self.meter.host.checkpoint(scope)?;
        let meter = &self.meter;
        let read = meter.read(self.class, scope, call, |saved| {
            saved
                .as_ref()
                .map_or((0, 0), |saved| (1, saved.body.len() as u64))
        });
        Some(Box::pin(meter.admitted(self.class, read)))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let call = self.meter.host.save_checkpoint(scope, checkpoint)?;
        let measured = (1, checkpoint.body.len() as u64);
        Some(Box::pin(
            self.meter.written(self.class, scope, measured, call),
        ))
    }
}

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_durability_core::{Digest, StoreError, StoreFuture, StoredFact};
use nanocode_sync::Slot;
use serde_json::value::RawValue;

use crate::{
    Compaction, Event, Extension, Fold, LogHost, Rider, ScopeRows, StoredCheckpoint, TurnLog, Write,
};

pub enum Reading {
    Loaded(Vec<(u64, String)>),
    Below(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub rows: u64,
    pub bytes: u64,
}

pub struct Adoption {
    log: TurnLog,
    host: Arc<Counted>,
    next: u64,
}

struct Counted {
    log: Arc<dyn LogHost>,
    loaded: Slot<Vec<(u64, String)>>,
    below: u64,
    rows: AtomicU64,
    bytes: AtomicU64,
}

impl Counted {
    fn kept(&self, mut rows: Vec<(u64, String)>) -> Vec<(u64, String)> {
        rows.retain(|(seq, _)| *seq < self.below);
        self.count(&rows);
        rows
    }

    fn count(&self, rows: &[(u64, String)]) {
        let bytes: usize = rows.iter().map(|(_, body)| body.len()).sum();
        self.rows.fetch_add(rows.len() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

fn refused(scope: &str) -> ! {
    panic!("the adoption of {scope} reads the turn log and writes nothing to it")
}

impl LogHost for Counted {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.log.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.log.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.log.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let to = to.min(self.below);
            if from >= to {
                return Ok(Vec::new());
            }
            Ok(self.kept(self.log.scan(scope, from, to).await?))
        })
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let scanned = self.log.scans(scans).await?;
            Ok(scanned.into_iter().map(|rows| self.kept(rows)).collect())
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            if let Some(rows) = self.loaded.take() {
                return Ok(rows);
            }
            Ok(self.kept(self.log.load(scope).await?))
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        _rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        refused(scope)
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        _rows: &'a [(u64, String)],
        _compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        refused(scope)
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        _extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        _write: Write<'a>,
        _rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        let read = self.log.checkpoint(scope)?;
        Some(Box::pin(async move {
            let checkpoint = read.await?;
            if let Some(checkpoint) = &checkpoint {
                self.rows.fetch_add(1, Ordering::Relaxed);
                self.bytes
                    .fetch_add(checkpoint.body.len() as u64, Ordering::Relaxed);
            }
            Ok(checkpoint)
        }))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        _checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }
}

pub async fn read_all(
    log: &Arc<dyn LogHost>,
    scope: &str,
    reading: Reading,
) -> Result<Adoption, StoreError> {
    let (below, loaded) = match reading {
        Reading::Loaded(rows) => (u64::MAX, Some(rows)),
        Reading::Below(below) => (below, None),
    };
    let host = Arc::new(Counted {
        log: Arc::clone(log),
        loaded: Slot::new(None),
        below,
        rows: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    });
    if let Some(rows) = loaded {
        host.count(&rows);
        host.loaded.put(rows);
    }
    let opened = TurnLog::open(Arc::clone(&host) as Arc<dyn LogHost>, scope).await?;
    let next = opened.load_all().await?;
    assert!(
        next <= below,
        "the turn log of {scope} loads past row {below}"
    );
    assert!(
        host.loaded.take().is_none(),
        "the turn log of {scope} opens without reading the rows it was handed"
    );
    Ok(Adoption {
        log: opened,
        host,
        next,
    })
}

impl Adoption {
    #[must_use]
    pub const fn next(&self) -> u64 {
        self.next
    }

    #[must_use]
    pub fn usage(&self) -> Usage {
        Usage {
            rows: self.host.rows.load(Ordering::Relaxed),
            bytes: self.host.bytes.load(Ordering::Relaxed),
        }
    }

    pub async fn read<R>(&self, read: impl FnOnce(&Fold) -> R + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        self.log.read(read).await
    }

    pub async fn history<S>(
        &self,
        after: u64,
        kept: S,
        take: impl FnMut(&mut S, &Event) -> bool + Send + 'static,
    ) -> Result<S, StoreError>
    where
        S: Send + 'static,
    {
        self.log.history(after, kept, take).await
    }

    pub async fn facts_from<S>(
        &self,
        after: u64,
        kept: S,
        take: impl FnMut(&mut S, u64, Option<&str>, &StoredFact) + Send + 'static,
    ) -> Result<S, StoreError>
    where
        S: Send + 'static,
    {
        self.log.facts_from(after, kept, take).await
    }

    pub async fn blobs(
        &self,
        digests: &[Digest],
    ) -> Result<BTreeMap<Digest, Box<RawValue>>, StoreError> {
        self.log.blobs(digests).await
    }
}

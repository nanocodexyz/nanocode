use std::{
    collections::BTreeMap,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_durability_core::{Digest, StoreError, StoreFuture, StoredFact};
use nanocode_sync::{Atom, Slot};
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
    scope: Arc<str>,
    loaded: Slot<Vec<(u64, String)>>,
    below: u64,
    rows: AtomicU64,
    bytes: AtomicU64,
    scanned: Atom<Vec<Arc<Scanned>>>,
}

struct Scanned {
    rows: Range<u64>,
    read: Vec<(u64, String)>,
}

impl Counted {
    fn reused(&self, rows: &Range<u64>) -> Option<Vec<(u64, String)>> {
        if crate::sim::mutated(crate::sim::Mutation::AdoptionRescans) {
            return None;
        }
        self.scanned.read(|scanned| {
            let mut held: Vec<&Scanned> = scanned
                .iter()
                .filter(|held| held.rows.start < rows.end && rows.start < held.rows.end)
                .map(Arc::as_ref)
                .collect();
            held.sort_by_key(|held| held.rows.start);
            let mut covered = rows.start;
            for held in &held {
                if held.rows.start > covered {
                    break;
                }
                covered = covered.max(held.rows.end);
            }
            if covered < rows.end {
                return None;
            }
            let mut read = BTreeMap::new();
            for held in held {
                for (seq, body) in held.read.iter().filter(|(seq, _)| rows.contains(seq)) {
                    read.entry(*seq).or_insert_with(|| body.clone());
                }
            }
            Some(read.into_iter().collect())
        })
    }

    fn hold(&self, rows: Range<u64>, read: &[(u64, String)]) {
        assert!(
            read.iter().all(|(seq, _)| rows.contains(seq)),
            "an adoption holds only the rows of the range it read"
        );
        assert!(
            read.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "an adoption holds the rows it read in seq order"
        );
        let held = Arc::new(Scanned {
            rows,
            read: read.to_vec(),
        });
        self.scanned
            .update(|scanned| scanned.push(Arc::clone(&held)));
    }

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
            assert_eq!(
                scope, &*self.scope,
                "an adoption reads only the scope it opened"
            );
            let to = to.min(self.below);
            if from >= to {
                return Ok(Vec::new());
            }
            if let Some(reused) = self.reused(&(from..to)) {
                return Ok(reused);
            }
            let read = self.kept(self.log.scan(scope, from, to).await?);
            self.hold(from..to, &read);
            Ok(read)
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
            assert_eq!(
                scope, &*self.scope,
                "an adoption loads only the scope it opened"
            );
            let rows = match self.loaded.take() {
                Some(rows) => rows,
                None => self.kept(self.log.load(scope).await?),
            };
            if let (Some((first, _)), Some((last, _))) = (rows.first(), rows.last()) {
                self.hold(*first..last + 1, &rows);
            }
            Ok(rows)
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
        scope: Arc::from(scope),
        loaded: Slot::new(None),
        below,
        rows: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        scanned: Atom::default(),
    });
    let snapshot = loaded
        .as_ref()
        .and_then(|rows| rows.first())
        .map(|(seq, _)| *seq);
    if let Some(rows) = loaded {
        host.count(&rows);
        host.loaded.put(rows);
    }
    let error = match opened(&host, scope).await {
        Ok(adoption) => return Ok(adoption),
        Err(error) => error,
    };
    let Some(expected) = snapshot else {
        return Err(error);
    };
    if crate::sim::mutated(crate::sim::Mutation::AdoptionRaceFails) {
        return Err(error);
    }
    let newest = host.log.load(scope).await?;
    host.count(&newest);
    match newest.first() {
        Some((actual, _)) if *actual > expected => Err(StoreError::Conflict {
            expected,
            actual: *actual,
        }),
        _ => Err(error),
    }
}

async fn opened(host: &Arc<Counted>, scope: &str) -> Result<Adoption, StoreError> {
    let below = host.below;
    let opened = TurnLog::open(Arc::clone(host) as Arc<dyn LogHost>, scope).await?;
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
        host: Arc::clone(host),
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

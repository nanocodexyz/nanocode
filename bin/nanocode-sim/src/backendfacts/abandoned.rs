use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use nanocode::claude::durability::{ClaudeFacts, ClaudeFold};
use nanocode_backend_codex::durability::{CodexFacts, CodexFold};
use nanocode_durability_core::{
    Append, Decision, Digest, Fact, FactLog, FactSink, FoldError, OwnedFact, Replay, StoreError,
    StoreFuture, StoredFact,
};
use nanocode_sync::Atom;
use serde_json::value::RawValue;

use super::Family;
use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const SCOPE: &str = "backend-facts-abandoned";
const LANDED_PPM: u64 = 500_000;
const STALL_POLLS_MAX: usize = 64;
const DRIVE_POLLS_MAX: usize = 4096;
const OPEN: u8 = 0;
const BEFORE_WRITE: u8 = 1;
const AFTER_WRITE: u8 = 2;

#[derive(Default)]
struct StallLog {
    facts: Atom<Vec<(u64, StoredFact)>>,
    blobs: Atom<BTreeMap<Digest, Box<RawValue>>>,
    stall: AtomicU8,
    stalled: AtomicBool,
}

struct Sink {
    next: u64,
    facts: Vec<(u64, StoredFact)>,
    blobs: BTreeMap<Digest, Box<RawValue>>,
}

impl FactSink for Sink {
    fn next(&self) -> u64 {
        self.next
    }

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError> {
        self.blobs.insert(*digest, json.to_owned());
        Ok(())
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError> {
        let seq = self.next;
        self.facts.push((seq, fact));
        self.next += 1;
        Ok(seq)
    }
}

impl StallLog {
    fn last(&self) -> u64 {
        self.facts
            .read(|facts| facts.last().map_or(0, |(seq, _)| *seq))
    }

    fn after(&self, after: u64) -> Replay {
        Replay {
            snapshot: None,
            facts: self.facts.read(|facts| {
                facts
                    .iter()
                    .filter(|(seq, _)| *seq > after)
                    .cloned()
                    .collect()
            }),
            blobs: self.blobs.read(Clone::clone),
        }
    }

    async fn stalled_here(&self) {
        self.stalled.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
    }
}

impl FactLog for StallLog {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(async move { Ok(self.after(0)) })
    }

    fn append(&self, after: u64, write: Append) -> StoreFuture<'_, Result<u64, StoreError>> {
        Box::pin(async move {
            let stall = self.stall.swap(OPEN, Ordering::SeqCst);
            if stall == BEFORE_WRITE {
                self.stalled_here().await;
            }
            let mut sink = Sink {
                next: self.last() + 1,
                facts: Vec::new(),
                blobs: BTreeMap::new(),
            };
            let seq = write(self.after(after), &mut sink).map_err(StoreError::from)?;
            self.blobs.update(|blobs| blobs.extend(sink.blobs.clone()));
            self.facts
                .update(|facts| facts.extend(sink.facts.iter().cloned()));
            if stall == AFTER_WRITE {
                self.stalled_here().await;
            }
            Ok(seq)
        })
    }
}

pub(super) trait OwnedFacts: Sized {
    type Fold: Family;
    async fn open(log: Arc<dyn FactLog>) -> Result<Self, String>;
    async fn commit(
        &self,
        fact: OwnedFact<<Self::Fold as nanocode_durability_core::DurableFold>::Decision>,
    ) -> Result<u64, String>;
    async fn seq(&self) -> Result<u64, String>;
}

impl OwnedFacts for ClaudeFacts {
    type Fold = ClaudeFold;

    async fn open(log: Arc<dyn FactLog>) -> Result<Self, String> {
        Self::open(log).map_err(|error| error.to_string())
    }

    async fn commit(
        &self,
        fact: OwnedFact<nanocode::claude::durability::ClaudeDecision>,
    ) -> Result<u64, String> {
        Self::commit(self, fact)
            .await
            .map_err(|error| error.to_string())
    }

    async fn seq(&self) -> Result<u64, String> {
        Self::seq(self).await.map_err(|error| error.to_string())
    }
}

impl OwnedFacts for CodexFacts {
    type Fold = CodexFold;

    async fn open(log: Arc<dyn FactLog>) -> Result<Self, String> {
        Self::open(log).map_err(|error| error.to_string())
    }

    async fn commit(
        &self,
        fact: OwnedFact<nanocode_backend_codex::durability::CodexDecision>,
    ) -> Result<u64, String> {
        Self::commit(self, fact)
            .await
            .map_err(|error| error.to_string())
    }

    async fn seq(&self) -> Result<u64, String> {
        Self::seq(self).await.map_err(|error| error.to_string())
    }
}

fn decided<F: Family>(decision: F::Decision) -> OwnedFact<F::Decision> {
    Fact::Decision(Decision::Backend(decision))
}

async fn abandon<P: OwnedFacts>(facts: &P, log: &StallLog) -> Result<(), String> {
    let mut commit =
        std::pin::pin!(facts.commit(decided::<P::Fold>(P::Fold::notice("abandoned")?)));
    for _ in 0..STALL_POLLS_MAX {
        let polled =
            std::future::poll_fn(|context| std::task::Poll::Ready(commit.as_mut().poll(context)))
                .await;
        if polled.is_ready() {
            return Err("a commit whose append stalls returned".into());
        }
        if log.stalled.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut yielded = false;
        std::future::poll_fn(|_| {
            if std::mem::replace(&mut yielded, true) {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
    }
    Err(format!(
        "the stalled append was not reached in {STALL_POLLS_MAX} polls"
    ))
}

async fn check<P: OwnedFacts>(landed: bool) -> Result<u64, String> {
    let log = Arc::new(StallLog::default());
    let facts = P::open(log.clone()).await?;
    let first = facts
        .commit(decided::<P::Fold>(P::Fold::template()?))
        .await?;
    log.stall.store(
        if landed { AFTER_WRITE } else { BEFORE_WRITE },
        Ordering::SeqCst,
    );
    abandon(&facts, &log).await?;
    let held = log.last();
    if held != first + u64::from(landed) {
        return Err(format!(
            "the log holds fact {held} after an abandoned commit, landed={landed}"
        ));
    }
    let next = facts
        .commit(decided::<P::Fold>(P::Fold::notice("after")?))
        .await
        .map_err(|error| format!("a commit after an abandoned one: {error}"))?;
    if next != held + 1 {
        return Err(format!(
            "a commit after an abandoned one took fact {next}, the log held {held}"
        ));
    }
    let seq = facts.seq().await?;
    if seq != log.last() {
        return Err(format!(
            "the owner's fold is at fact {seq}, the log at {}",
            log.last()
        ));
    }
    Ok(fnv(
        fnv(FNV_OFFSET, P::Fold::NAME.as_bytes()),
        &seq.to_le_bytes(),
    ))
}

fn drive(check: impl Future<Output = Result<u64, String>>) -> Result<u64, String> {
    let mut check = std::pin::pin!(check);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    let mut spawned = Vec::new();
    for _ in 0..DRIVE_POLLS_MAX {
        if let std::task::Poll::Ready(checked) = check.as_mut().poll(&mut context) {
            return checked;
        }
        spawned.extend(crate::host::spawned().into_iter().map(|(owner, _)| owner));
        spawned.retain_mut(|owner| owner.as_mut().poll(&mut context).is_pending());
    }
    Err(format!(
        "the fact owner check did not finish in {DRIVE_POLLS_MAX} polls"
    ))
}

pub(super) fn run(seed: u64, claude: bool) -> Result<u64, String> {
    let landed = Rng::derive(seed, SCOPE).chance(LANDED_PPM);
    crate::host::entered(seed, &Clock::new(), || {
        if claude {
            drive(check::<ClaudeFacts>(landed))
        } else {
            drive(check::<CodexFacts>(landed))
        }
    })
}

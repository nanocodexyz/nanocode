use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::{Atom, Notify};
use nanocode_turnlog::{LogHost, ScopeRows, StoredCheckpoint};

use crate::{
    Board, BoardError, TaskId,
    sim::{Mutation, mutated},
};

pub type Napping<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

pub trait Timer {
    fn now_ms(&self) -> u64;

    fn sleep_until(&self, at_ms: u64) -> Napping<'_>;
}

pub trait Wake {
    fn wait<'a>(&'a self, scope: &'a str, seen: u64, until_ms: u64) -> Napping<'a>;
}

pub trait Ring: Send + Sync {
    fn ring(&self, scope: &str, head: u64);
}

pub trait Alarm {
    fn arm(&self, scope: &str, at_ms: u64);
}

pub struct Polling<'t> {
    pub timer: &'t dyn Timer,
    pub every_ms: u64,
}

impl Wake for Polling<'_> {
    fn wait<'a>(&'a self, _scope: &'a str, _seen: u64, until_ms: u64) -> Napping<'a> {
        assert!(self.every_ms > 0, "a polling wake needs a period");
        if mutated(Mutation::WaitSkipsPoll) {
            return self.timer.sleep_until(until_ms);
        }
        let now = self.timer.now_ms();
        self.timer
            .sleep_until(now.saturating_add(self.every_ms).min(until_ms))
    }
}

#[derive(Default)]
pub struct Notifier {
    heads: Atom<BTreeMap<String, u64>>,
    notify: Notify,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self, scope: &str) -> u64 {
        self.heads
            .read(|heads| heads.get(scope).copied().unwrap_or_default())
    }

    pub async fn changed(&self, scope: &str, seen: u64) {
        loop {
            let notified = self.notify.notified();
            if self.head(scope) > seen {
                return;
            }
            notified.await;
        }
    }

    pub fn heard(&self, payload: &str) {
        let Some((scope, head)) = payload.rsplit_once(':') else {
            return;
        };
        if let Ok(head) = head.parse() {
            self.ring(scope, head);
        }
    }
}

impl Ring for Notifier {
    fn ring(&self, scope: &str, head: u64) {
        let moved = self.heads.update(|heads| {
            let known = heads.entry(scope.to_owned()).or_default();
            let moved = head > *known;
            *known = (*known).max(head);
            moved
        });
        if moved {
            self.notify.notify_waiters();
        }
    }
}

pub struct Listening<'a> {
    pub notifier: &'a Notifier,
    pub fallback: &'a dyn Wake,
}

impl Wake for Listening<'_> {
    fn wait<'a>(&'a self, scope: &'a str, seen: u64, until_ms: u64) -> Napping<'a> {
        Box::pin(Race {
            first: Box::pin(self.notifier.changed(scope, seen)),
            second: self.fallback.wait(scope, seen, until_ms),
        })
    }
}

struct Race<'a> {
    first: Napping<'a>,
    second: Napping<'a>,
}

impl Future for Race<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.first.as_mut().poll(context).is_ready() {
            return Poll::Ready(());
        }
        self.second.as_mut().poll(context)
    }
}

pub struct Notifying {
    inner: Arc<dyn LogHost>,
    ring: Arc<dyn Ring>,
}

impl Notifying {
    pub fn new(inner: Arc<dyn LogHost>, ring: Arc<dyn Ring>) -> Self {
        Self { inner, ring }
    }
}

impl LogHost for Notifying {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.inner.call_bound()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.inner.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.inner.scan(scope, from, to)
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        self.inner.scans(scans)
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.inner.load(scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let appended = self.inner.append(scope, rows).await;
            if appended.is_ok()
                && let Some(head) = rows.iter().map(|(seq, _)| *seq).max()
            {
                self.ring.ring(scope, head);
            }
            appended
        })
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        self.inner.checkpoint(scope)
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.inner.save_checkpoint(scope, checkpoint)
    }
}

pub const LISTEN: &str = "LISTEN nanocode_board";
pub const NOTIFY: &str = "SELECT pg_notify('nanocode_board', $1::text)";

pub fn payload(scope: &str, head: u64) -> String {
    format!("{scope}:{head}")
}

impl Board {
    pub fn due_ms(&self) -> Option<u64> {
        let tasks = self
            .fold
            .next_due()
            .filter(|_| !mutated(Mutation::WakeIgnoresDue));
        self.fold.due_ms().into_iter().chain(tasks).min()
    }

    pub fn arm(&self, alarm: &dyn Alarm) -> Option<u64> {
        let due = self.due_ms()?;
        alarm.arm(&self.scope, due);
        Some(due)
    }

    pub async fn next_ready(
        &mut self,
        wake: &dyn Wake,
        timer: &dyn Timer,
        labels: &BTreeMap<String, String>,
        until_ms: u64,
    ) -> Result<Option<TaskId>, BoardError> {
        loop {
            self.refresh().await?;
            let now = timer.now_ms();
            if let Some(id) = self.fold.pick(&self.writer, labels, now) {
                return Ok(Some(id.clone()));
            }
            if now >= until_ms {
                return Ok(None);
            }
            let wake_at = self
                .due_ms()
                .filter(|due| *due > now)
                .map_or(until_ms, |due| due.min(until_ms));
            wake.wait(&self.scope, self.fold.next() - 1, wake_at).await;
        }
    }
}

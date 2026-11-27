//! One fenced, partitioned, group-committed log host for every log kind.
//!
//! Every scope (a session's turn log, a board slice, a thread's records, a cost log) lands in one
//! partition. A node appends to a partition only while it owns that partition's lease (one lease
//! row per partition on the database clock, nanocode_partitions::Row, driven in process by
//! nanocode_partitions::lease::Lease), and concurrent appends to one partition from any
//! scope and any log kind go down as one transaction: each append is all-or-nothing, a seq that
//! already exists rejects only its own append, and a stale fence rejects the whole batch. The
//! store behind it is a PartStore: SqlParts for Postgres and SQLite, ModelParts for the simulator.

use std::{
    collections::{BTreeSet, HashSet},
    num::NonZeroU32,
    ops::Range,
    sync::Arc,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::{
    Partition, Row,
    lease::{Answer, Claim, ClaimAnswer, Drain, Lease, Policy, Unowned},
    partition_of,
};
use nanocode_sync::{Atom, Map, Slot, Vector, oneshot};

use crate::host::{Compaction, Extension, LogHost, Rider, StoredCheckpoint, Write};

mod gain;
pub mod model;
pub mod sim;
pub mod sql;

pub use gain::{GainError, Gained};
pub use model::{Fault, Faults, ModelParts};
pub use sql::SqlParts;

pub type Rows = Vec<(u64, String)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Append {
    pub scope: Arc<str>,
    pub rows: Rows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Committed,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub pos: u64,
    pub scope: Arc<str>,
    pub seq: u64,
    pub body: String,
}

/// The storage a PartedLog runs on. The epoch is the fence token a write carries: None writes
/// unfenced (single-writer deployments), Some(epoch) is admitted only while the partition's head
/// is at that epoch.
pub trait PartStore: Send + Sync {
    /// The one-shot schema step: creates the tables and the count partitions. The only call that
    /// runs DDL.
    fn migrate(&self, count: NonZeroU32) -> StoreFuture<'_, Result<(), StoreError>>;

    fn read_capacity(&self) -> u32 {
        1
    }

    fn row_bytes_max(&self) -> Option<u64> {
        None
    }

    /// Checks that migrate ran for count partitions at this release's format, without DDL.
    fn prepare(&self, count: NonZeroU32) -> StoreFuture<'_, Result<(), StoreError>>;

    /// One transaction: the head compare-and-set at epoch, then every append all-or-nothing,
    /// each row given the next position. A stale epoch is Err(Fenced) for the whole batch.
    fn commit<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        batch: &'a [Append],
    ) -> StoreFuture<'a, Result<Vec<Verdict>, StoreError>>;

    fn scan<'a>(
        &'a self,
        part: Partition,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Rows, StoreError>>;

    /// The scope's latest snapshot row (a body starting with s) and every row after it.
    fn load<'a>(
        &'a self,
        part: Partition,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Rows, StoreError>>;

    /// Rewrites archived rows and drops superseded ranges, never a row a cursor still needs.
    fn compact<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>>;

    fn extend<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>>;

    /// commit with each landed append's compaction in the same transaction, compactions[i]
    /// belonging to batch[i]; None when this store compacts only in a transaction of its own.
    fn commit_compacting<'a>(
        &'a self,
        _part: Partition,
        _epoch: Option<u64>,
        _batch: &'a [Append],
        _compactions: &'a [Option<Compaction<'a>>],
    ) -> Option<StoreFuture<'a, Result<Vec<Verdict>, StoreError>>> {
        None
    }

    /// Rows of the partition after position after, in position order: the follower feed.
    fn follow(
        &self,
        part: Partition,
        after: u64,
        limit: usize,
    ) -> StoreFuture<'_, Result<Vec<Entry>, StoreError>>;

    /// The partition's lease row, read back.
    fn fence(&self, part: Partition) -> StoreFuture<'_, Result<Row<String>, StoreError>>;

    /// Claims the lease for node on the store's clock: Some((epoch, cursor)) when it landed,
    /// None when the row is held by another node and not past expiry plus grace.
    fn claim_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        ttl_ms: u64,
        grace_ms: u64,
    ) -> StoreFuture<'a, Result<Option<(u64, u64)>, StoreError>>;

    /// Extends node's lease at epoch on the store's clock; false when the epoch moved.
    fn renew_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        epoch: u64,
        ttl_ms: u64,
    ) -> StoreFuture<'a, Result<bool, StoreError>>;

    /// Extends every lease in held (partition, epoch) for node in one statement on the store's
    /// clock; returns the partitions whose renewal landed, the others' epochs moved.
    fn renew_leases<'a>(
        &'a self,
        node: &'a str,
        held: &'a [(Partition, u64)],
        ttl_ms: u64,
    ) -> StoreFuture<'a, Result<Vec<Partition>, StoreError>>;

    /// Hands node's lease at epoch back at the committed position: Some(cursor) when it landed,
    /// None when the epoch moved.
    fn release_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        epoch: u64,
    ) -> StoreFuture<'a, Result<Option<u64>, StoreError>>;

    /// Records that reader consumed the partition through pos (it never moves back).
    fn cursor<'a>(
        &'a self,
        part: Partition,
        reader: &'a str,
        pos: u64,
    ) -> StoreFuture<'a, Result<(), StoreError>>;

    /// Drops the scope's rows below before, never the latest snapshot or anything after it, and
    /// never a row some cursor of the partition has not consumed. Returns the rows dropped.
    fn prune<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        before: u64,
    ) -> StoreFuture<'a, Result<u64, StoreError>>;

    /// The scope's saved checkpoint (see [`LogHost::checkpoint`]); None when this store keeps
    /// none.
    fn checkpoint<'a>(
        &'a self,
        _scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        None
    }

    /// Replaces the scope's checkpoint; None when this store keeps none. A checkpoint is not a
    /// write to the log, so it takes no fence.
    fn save_checkpoint<'a>(
        &'a self,
        _scope: &'a str,
        _checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        None
    }
}

pub type Place = Arc<dyn Fn(&str, NonZeroU32) -> Partition + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
pub type RingFn = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// Where a scope lives: a "/p<n>" suffix names partition n (board slices), a UUID segment hashes
/// its 16 bytes (everything a thread owns lands beside it), anything else hashes the scope's
/// bytes. The hash is nanocode_partitions::partition_of.
#[must_use]
pub fn place(scope: &str, count: NonZeroU32) -> Partition {
    if let Some((_, tail)) = scope.rsplit_once("/p")
        && let Ok(n) = tail.parse::<u32>()
        && n < count.get()
    {
        return Partition(n);
    }
    if let Some(bytes) = scope.split(['/', ':', '@']).find_map(uuid_bytes) {
        return partition_of(&bytes, count);
    }
    partition_of(scope.as_bytes(), count)
}

fn uuid_bytes(segment: &str) -> Option<[u8; 16]> {
    if segment.len() != 36 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    let mut digits = segment.bytes().filter(|byte| *byte != b'-');
    for byte in &mut bytes {
        let high = char::from(digits.next()?).to_digit(16)?;
        let low = char::from(digits.next()?).to_digit(16)?;
        *byte = u8::try_from(high * 16 + low).ok()?;
    }
    digits.next().is_none().then_some(bytes)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub commits: u64,
    pub appends: u64,
    pub rows: u64,
    pub conflicts: u64,
    pub fenced: u64,
}

#[derive(Clone)]
struct Compacting {
    archived: Rows,
    dropped: Range<u64>,
    superseded: Vec<Range<u64>>,
}

impl Compacting {
    fn of(compaction: &Compaction<'_>) -> Option<Self> {
        let empty = compaction.archived.is_empty()
            && compaction.dropped.is_empty()
            && compaction.superseded.iter().all(Range::is_empty);
        (!empty).then(|| Self {
            archived: compaction.archived.to_vec(),
            dropped: compaction.dropped.clone(),
            superseded: compaction.superseded.to_vec(),
        })
    }

    fn borrowed(&self) -> Compaction<'_> {
        Compaction {
            archived: &self.archived,
            dropped: self.dropped.clone(),
            superseded: &self.superseded,
        }
    }
}

enum Turn {
    Done(Result<(), StoreError>),
    Lead(Append, Option<Compacting>),
}

struct Waiter {
    append: Append,
    compacting: Option<Compacting>,
    reply: oneshot::Sender<Turn>,
}

#[derive(Clone)]
struct Queued {
    rows: usize,
    waiter: Arc<Slot<Waiter>>,
}

impl Queued {
    fn new(waiter: Waiter) -> Self {
        Self {
            rows: waiter.append.rows.len(),
            waiter: Arc::new(Slot::new(Some(waiter))),
        }
    }

    fn take(&self) -> Waiter {
        let Some(waiter) = self.waiter.take() else {
            panic!("a queued append leaves its queue once");
        };
        waiter
    }
}

#[derive(Clone, Default)]
struct Queue {
    leading: bool,
    waiting: Vector<Queued>,
}

type Queues = Atom<Map<Partition, Queue>>;

pub struct PartedLog<S: ?Sized> {
    store: Arc<S>,
    count: NonZeroU32,
    place: Place,
    node: Option<Arc<str>>,
    held: Atom<Map<Partition, Lease>>,
    clock: Clock,
    bounds: Policy,
    queues: Queues,
    ring: Atom<Option<RingFn>>,
    stats: Atom<Stats>,
    max_rows: usize,
}

fn renewed(
    held: &mut Map<Partition, Lease>,
    part: Partition,
    epoch: u64,
    answer: Answer,
    sent_at: u64,
    policy: Policy,
) {
    let next = match held.get(&part).copied() {
        Some(Lease::Owned(owned)) if owned.epoch == epoch => {
            Some(owned.renewed(answer, sent_at, policy).map(Lease::Owned))
        }
        Some(Lease::Draining(draining)) if draining.epoch == epoch => Some(
            draining
                .renewed(answer, sent_at, policy)
                .map(Lease::Draining),
        ),
        _ => None,
    };
    match next {
        Some(Ok(lease)) => {
            held.insert(part, lease);
        }
        Some(Err(_)) => {
            held.remove(&part);
        }
        None => {}
    }
}

fn uncertain() -> StoreError {
    StoreError::Backend("group commit ended without an answer; the append may have landed".into())
}

impl<S: PartStore + ?Sized> PartedLog<S> {
    /// An unfenced host: one writer per deployment, appends still grouped per partition.
    pub fn new(store: Arc<S>, count: NonZeroU32) -> Self {
        Self {
            store,
            count,
            place: Arc::new(place),
            node: None,
            held: Atom::default(),
            clock: Arc::new(nanocode_platform::unix_millis),
            bounds: Policy {
                ttl_ms: 0,
                grace_ms: 0,
                margin_ms: 0,
            },
            queues: Atom::default(),
            ring: Atom::default(),
            stats: Atom::default(),
            max_rows: 512,
        }
    }

    /// A fenced host for node: it appends only to partitions it claimed.
    pub fn fenced(store: Arc<S>, count: NonZeroU32, node: &str) -> Self {
        Self {
            node: Some(Arc::from(node)),
            ..Self::new(store, count)
        }
    }

    #[must_use]
    pub fn with_place(mut self, place: Place) -> Self {
        self.place = place;
        self
    }

    /// The local clock (ms) a fenced host self-fences on: it stops writing once now + margin_ms
    /// reaches the send time of its last landed claim or renewal plus its ttl.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// grace_ms: how long past expiry a claim waits, at least the longest write transaction
    /// (statement_timeout). margin_ms: how early the owner stops writing, at least twice the
    /// clock skew between nodes and the database.
    #[must_use]
    pub const fn with_lease_bounds(mut self, grace_ms: u64, margin_ms: u64) -> Self {
        self.bounds.grace_ms = grace_ms;
        self.bounds.margin_ms = margin_ms;
        self
    }

    #[must_use]
    pub fn with_max_rows(mut self, max_rows: usize) -> Self {
        self.max_rows = max_rows.max(1);
        self
    }

    /// Calls ring(scope, head) after every committed append, for local watchers. Postgres also
    /// notifies other nodes on the nanocode_log channel inside the transaction.
    pub fn set_ring(&self, ring: RingFn) {
        self.ring.set(Some(ring));
    }

    pub const fn store(&self) -> &Arc<S> {
        &self.store
    }

    pub const fn count(&self) -> NonZeroU32 {
        self.count
    }

    pub fn partition(&self, scope: &str) -> Partition {
        (self.place)(scope, self.count)
    }

    pub fn stats(&self) -> Stats {
        self.stats.read(|stats| *stats)
    }

    /// The epoch this node holds the partition at, owned or draining.
    pub fn held(&self, part: Partition) -> Option<u64> {
        self.held
            .read(|held| held.get(&part).and_then(Lease::epoch))
    }

    /// The cursor the partition was handed over at when this node claimed it: everything at or
    /// before it was committed by earlier owners.
    pub fn resume(&self, part: Partition) -> Option<u64> {
        self.held.read(|held| match held.get(&part) {
            Some(Lease::Owned(owned)) => Some(owned.cursor),
            Some(Lease::Draining(draining)) => Some(draining.cursor),
            _ => None,
        })
    }

    /// The one-shot schema step (PartStore::migrate); run it at deploy, before anything serves.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        self.store.migrate(self.count).await
    }

    /// Checks that migrate ran, without DDL.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        self.store.prepare(self.count).await
    }

    fn epoch(&self, part: Partition) -> Result<Option<u64>, StoreError> {
        if self.node.is_none() {
            return Ok(None);
        }
        let now = (self.clock)();
        match self.held.read(|held| {
            held.get(&part)
                .filter(|lease| {
                    !(sim::mutated(sim::Mutation::DrainRefusesQueued)
                        && matches!(lease, Lease::Draining(_)))
                })
                .and_then(|lease| lease.writable(now, self.bounds))
        }) {
            Some(Ok(epoch)) => Ok(Some(epoch)),
            Some(Err(_)) | None => Err(StoreError::Fenced),
        }
    }

    fn admit(&self, part: Partition) -> Result<(), StoreError> {
        if self.node.is_none()
            || self
                .held
                .read(|held| held.get(&part).is_some_and(Lease::admits))
        {
            return Ok(());
        }
        Err(StoreError::Fenced)
    }

    fn lose(&self, part: Partition, epoch: u64) {
        self.held.update(|held| {
            if held.get(&part).and_then(Lease::epoch) == Some(epoch) {
                held.remove(&part);
            }
        });
    }

    fn node(&self) -> Result<String, StoreError> {
        self.node.as_deref().map(str::to_owned).ok_or_else(|| {
            StoreError::Backend("an unfenced log host has no node to claim for".into())
        })
    }

    /// Claims the partition's lease for this node, raising its epoch. The store judges expiry on
    /// its own clock: the claim lands only on a released row, this node's own, or one past expiry
    /// plus grace, never on a live lease. An unknown outcome is read back from the row, and a
    /// lease read back as ours grants no time until a renewal lands.
    pub async fn claim(&self, part: Partition, ttl_ms: u64) -> Result<u64, StoreError> {
        let node = self.node()?;
        let policy = Policy {
            ttl_ms,
            ..self.bounds
        };
        let claiming = Unowned.claim((self.clock)());
        let answer = match self
            .store
            .claim_lease(part, &node, ttl_ms, policy.grace_ms)
            .await
        {
            Ok(Some((epoch, cursor))) => ClaimAnswer::Claimed { epoch, cursor },
            Ok(None) => ClaimAnswer::Refused,
            Err(StoreError::Backend(_)) => ClaimAnswer::Unknown,
            Err(error) => return Err(error),
        };
        let owned = match claiming.answered(answer, policy) {
            Claim::Owned(owned) => owned,
            Claim::Unowned(_) => return Err(StoreError::Fenced),
            Claim::Unknown(claiming) => {
                let row = self.store.fence(part).await?;
                let Claim::Owned(owned) = claiming.read_back(&row, &node) else {
                    return Err(StoreError::Fenced);
                };
                self.held
                    .update(|held| held.insert(part, Lease::Owned(owned)));
                return match self.renew(part, ttl_ms).await {
                    Ok(()) => Ok(owned.epoch),
                    Err(StoreError::Fenced) => Err(StoreError::Fenced),
                    Err(_) => Ok(owned.epoch),
                };
            }
        };
        self.held
            .update(|held| held.insert(part, Lease::Owned(owned)));
        Ok(owned.epoch)
    }

    /// Extends the lease at the epoch this node holds; Fenced when the epoch moved (the lease is
    /// lost). An unknown outcome keeps the lease and its old deadline.
    pub async fn renew(&self, part: Partition, ttl_ms: u64) -> Result<(), StoreError> {
        let node = self.node()?;
        let epoch = self.held(part).ok_or(StoreError::Fenced)?;
        let policy = Policy {
            ttl_ms,
            ..self.bounds
        };
        let sent_at = (self.clock)();
        let answer = if self.store.renew_lease(part, &node, epoch, ttl_ms).await? {
            Answer::Landed
        } else {
            Answer::Moved { current: epoch + 1 }
        };
        self.held
            .update(|held| renewed(held, part, epoch, answer, sent_at, policy));
        match answer {
            Answer::Landed | Answer::Unknown => Ok(()),
            Answer::Moved { .. } => Err(StoreError::Fenced),
        }
    }

    /// Renews every lease this node holds, owned or draining, in one store statement. Returns
    /// the partitions whose epoch moved: those leases are lost. An error keeps every lease and
    /// its old deadline.
    pub async fn renew_all(&self, ttl_ms: u64) -> Result<Vec<Partition>, StoreError> {
        let node = self.node()?;
        let mut held: Vec<(Partition, u64)> = self.held.read(|held| {
            held.iter()
                .filter_map(|(part, lease)| lease.epoch().map(|epoch| (*part, epoch)))
                .collect()
        });
        held.sort_unstable();
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let policy = Policy {
            ttl_ms,
            ..self.bounds
        };
        let sent_at = (self.clock)();
        let landed: BTreeSet<Partition> = self
            .store
            .renew_leases(&node, &held, ttl_ms)
            .await?
            .into_iter()
            .collect();
        if let Some(stray) = landed
            .iter()
            .find(|part| !held.iter().any(|(mine, _)| mine == *part))
        {
            return Err(StoreError::Backend(format!(
                "the store renewed partition {} that this node did not ask for",
                stray.0
            )));
        }
        let lost: Vec<Partition> = held
            .iter()
            .filter(|(part, _)| !landed.contains(part))
            .map(|(part, _)| *part)
            .collect();
        self.held.update(|leases| {
            for (part, epoch) in &held {
                let answer = if landed.contains(part) {
                    Answer::Landed
                } else {
                    Answer::Moved { current: epoch + 1 }
                };
                renewed(leases, *part, *epoch, answer, sent_at, policy);
            }
        });
        Ok(lost)
    }

    /// Drains then releases: stops admitting new appends to the partition but keeps its epoch
    /// while every append queued before the call commits, then hands the lease back at the
    /// committed position so the gainer claims at once and resumes there.
    pub async fn release(&self, part: Partition) -> Result<(), StoreError> {
        let node = self.node()?;
        let draining = self.held.update(|held| match held.get(&part).copied() {
            Some(Lease::Owned(owned)) => {
                let draining = owned.drain();
                held.insert(part, Lease::Draining(draining));
                Some(draining)
            }
            Some(Lease::Draining(draining)) => Some(draining),
            _ => None,
        });
        let Some(draining) = draining else {
            return Ok(());
        };
        self.drain(part).await;
        let drain = match self.store.release_lease(part, &node, draining.epoch).await {
            Ok(Some(cursor)) => draining.released(Answer::Landed, cursor),
            Ok(None) => draining.released(
                Answer::Moved {
                    current: draining.epoch + 1,
                },
                0,
            ),
            Err(StoreError::Backend(_)) => {
                let row = self.store.fence(part).await?;
                draining.read_back(&row, &node)
            }
            Err(error) => return Err(error),
        };
        match drain {
            Drain::Released(_) => {
                self.lose(part, draining.epoch);
                Ok(())
            }
            Drain::Lost(_) => {
                self.lose(part, draining.epoch);
                Err(StoreError::Fenced)
            }
            Drain::Unknown(_) => Err(StoreError::Backend(
                "the release's outcome is unknown and the lease is still held; release again"
                    .into(),
            )),
        }
    }

    /// Forgets the partition without touching its row (the node lost it).
    pub fn forget(&self, part: Partition) {
        self.held.update(|held| held.remove(&part));
    }

    /// Resolves once every append queued on the partition before the call has its answer.
    pub async fn drain(&self, part: Partition) {
        let _ = self
            .submit(
                part,
                Append {
                    scope: Arc::from(""),
                    rows: Vec::new(),
                },
                None,
            )
            .await;
    }

    pub async fn follow(
        &self,
        part: Partition,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Entry>, StoreError> {
        self.store.follow(part, after, limit).await
    }

    pub async fn cursor(&self, part: Partition, reader: &str, pos: u64) -> Result<(), StoreError> {
        self.store.cursor(part, reader, pos).await
    }

    pub async fn prune(&self, scope: &str, before: u64) -> Result<u64, StoreError> {
        let part = self.partition(scope);
        let epoch = self.epoch(part)?;
        self.store.prune(part, epoch, scope, before).await
    }

    async fn submit(
        &self,
        part: Partition,
        append: Append,
        compacting: Option<Compacting>,
    ) -> Result<(), StoreError> {
        let (reply, mut answer) = oneshot::channel();
        let queued = Queued::new(Waiter {
            append,
            compacting,
            reply,
        });
        let lead = self.queues.update(|queues| {
            let queue = queues.entry(part).or_default();
            queue.waiting.push_back(queued.clone());
            !std::mem::replace(&mut queue.leading, true)
        });
        if !lead {
            match (&mut answer).await {
                Ok(Turn::Done(result)) => return result,
                Ok(Turn::Lead(append, compacting)) => {
                    let (reply, next) = oneshot::channel();
                    let queued = Queued::new(Waiter {
                        append,
                        compacting,
                        reply,
                    });
                    self.queues.update(|queues| {
                        let queue = queues.entry(part).or_default();
                        let mut waiting = Vector::new();
                        waiting.push_back(queued.clone());
                        waiting.extend(queue.waiting.iter().cloned());
                        queue.waiting = waiting;
                    });
                    answer = next;
                }
                Err(_) => return Err(uncertain()),
            }
        }
        self.lead(part).await;
        match answer.await {
            Ok(Turn::Done(result)) => result,
            Ok(Turn::Lead(..)) | Err(_) => Err(uncertain()),
        }
    }

    async fn lead(&self, part: Partition) {
        let mut leading = Leading {
            queues: &self.queues,
            part,
            done: false,
        };
        loop {
            let batch = self.queues.update(|queues| {
                let queue = queues.entry(part).or_default();
                let mut batch = Vec::new();
                let mut rows = 0;
                while let Some(queued) = queue.waiting.front() {
                    if !batch.is_empty() && rows + queued.rows > self.max_rows {
                        break;
                    }
                    rows += queued.rows;
                    batch.extend(queue.waiting.pop_front());
                }
                if batch.is_empty() {
                    queue.leading = false;
                }
                batch
            });
            if batch.is_empty() {
                leading.done = true;
                return;
            }
            self.commit(part, batch.iter().map(Queued::take).collect())
                .await;
        }
    }

    /// The batch's verdicts and, per append, how its compaction went: in the commit's own
    /// transaction when the store can, else in one fenced transaction each once the batch
    /// landed. Compaction only shrinks the log, so a crash between the two leaves an
    /// uncompacted but complete log.
    async fn committed(
        &self,
        part: Partition,
        epoch: Option<u64>,
        sent: &[Append],
        compactings: &[Option<&Compacting>],
    ) -> (
        Result<Vec<Verdict>, StoreError>,
        Vec<Result<(), StoreError>>,
    ) {
        let mut compacted = vec![Ok(()); sent.len()];
        if sent.is_empty() {
            return (Ok(Vec::new()), compacted);
        }
        let compactions: Vec<Option<Compaction<'_>>> = compactings
            .iter()
            .map(|compacting| compacting.map(Compacting::borrowed))
            .collect();
        if compactions.iter().any(Option::is_some)
            && let Some(together) = self
                .store
                .commit_compacting(part, epoch, sent, &compactions)
        {
            return (together.await, compacted);
        }
        let answer = self.store.commit(part, epoch, sent).await;
        if let Ok(verdicts) = &answer {
            for (index, compaction) in compactions.into_iter().enumerate() {
                if let (Some(compaction), Some(Verdict::Committed)) =
                    (compaction, verdicts.get(index))
                {
                    compacted[index] = self
                        .store
                        .compact(part, epoch, &sent[index].scope, compaction)
                        .await;
                }
            }
        }
        (answer, compacted)
    }

    async fn commit(&self, part: Partition, batch: Vec<Waiter>) {
        let mut seen: HashSet<(Arc<str>, u64)> = HashSet::new();
        let mut sent = Vec::new();
        let mut compactings = Vec::new();
        let mut slots = Vec::with_capacity(batch.len());
        for waiter in &batch {
            let append = &waiter.append;
            if append.rows.is_empty() {
                slots.push(None);
                continue;
            }
            let clash = append
                .rows
                .iter()
                .any(|(seq, _)| seen.contains(&(Arc::clone(&append.scope), *seq)));
            if clash && !sim::mutated(sim::Mutation::BatchDuplicates) {
                slots.push(Some(Err(StoreError::Fenced)));
                continue;
            }
            seen.extend(
                append
                    .rows
                    .iter()
                    .map(|(seq, _)| (Arc::clone(&append.scope), *seq)),
            );
            slots.push(Some(Ok(sent.len())));
            sent.push(append.clone());
            compactings.push(waiter.compacting.as_ref());
        }
        let local = if sent.is_empty() {
            Ok(None)
        } else {
            self.epoch(part)
        };
        let (answer, compacted) = match &local {
            Ok(epoch) => self.committed(part, *epoch, &sent, &compactings).await,
            Err(error) => (Err(error.clone()), vec![Ok(()); sent.len()]),
        };
        let answer = match answer {
            Err(StoreError::Backend(_)) if sim::mutated(sim::Mutation::LostAckRejected) => {
                Ok(vec![Verdict::Conflict; sent.len()])
            }
            answer => answer,
        };
        let fenced = matches!(answer, Err(StoreError::Fenced));
        if fenced && let Ok(Some(epoch)) = local {
            self.lose(part, epoch);
        }
        let ring = self.ring.read(Clone::clone);
        let mut stats = Stats {
            commits: u64::from(!sent.is_empty()),
            fenced: u64::from(fenced),
            ..Stats::default()
        };
        let mut rung = 0;
        for (waiter, slot) in batch.into_iter().zip(slots) {
            let compaction = match slot {
                Some(Ok(at)) => compacted[at].clone(),
                _ => Ok(()),
            };
            let result = match slot {
                None => Ok(()),
                Some(Err(error)) => Err(error),
                Some(Ok(at)) => match &answer {
                    Ok(verdicts) => match verdicts.get(at) {
                        Some(Verdict::Committed) => Ok(()),
                        Some(Verdict::Conflict) => Err(StoreError::Fenced),
                        None => Err(uncertain()),
                    },
                    Err(error) => Err(error.clone()),
                },
            };
            match &result {
                Ok(()) if !waiter.append.rows.is_empty() => {
                    stats.appends += 1;
                    stats.rows += waiter.append.rows.len() as u64;
                    rung += 1;
                    let missed = sim::mutated(sim::Mutation::MissedRing) && rung == 2;
                    if let Some(ring) = &ring
                        && !missed
                        && let Some(head) = waiter.append.rows.iter().map(|(seq, _)| *seq).max()
                    {
                        ring(&waiter.append.scope, head);
                    }
                }
                Err(StoreError::Fenced) => stats.conflicts += 1,
                _ => {}
            }
            let _ = waiter.reply.send(Turn::Done(result.and(compaction)));
        }
        self.stats.update(|total| {
            total.commits += stats.commits;
            total.appends += stats.appends;
            total.rows += stats.rows;
            total.conflicts += stats.conflicts;
            total.fenced += stats.fenced;
        });
    }
}

/// Hands the partition's lead to the next waiter when the leading future is dropped mid-batch;
/// the batch in flight answers uncertain through its dropped replies.
struct Leading<'a> {
    queues: &'a Queues,
    part: Partition,
    done: bool,
}

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        while let Some(queued) = self.queues.update(|queues| {
            let queue = queues.entry(self.part).or_default();
            let next = queue.waiting.pop_front();
            if next.is_none() {
                queue.leading = false;
            }
            next
        }) {
            let waiter = queued.take();
            if waiter
                .reply
                .send(Turn::Lead(waiter.append, waiter.compacting))
                .is_ok()
            {
                return;
            }
        }
    }
}

impl<S: PartStore + ?Sized> LogHost for PartedLog<S> {
    fn call_bound(&self) -> Option<std::time::Duration> {
        None
    }

    fn read_capacity(&self) -> u32 {
        self.store.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.store.row_bytes_max()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.store.scan(self.partition(scope), scope, from, to)
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.store.load(self.partition(scope), scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if rows.is_empty() {
                return Ok(());
            }
            let part = self.partition(scope);
            self.admit(part)?;
            let append = Append {
                scope: Arc::from(scope),
                rows: rows.to_vec(),
            };
            self.submit(part, append, None).await
        })
    }

    /// The rows and their compaction go down through the partition's group commit, in one
    /// transaction where the store can (PartStore::commit_compacting).
    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let compacting = Compacting::of(&compaction);
            if rows.is_empty() {
                let Some(compacting) = compacting else {
                    return Ok(());
                };
                let part = self.partition(scope);
                let epoch = self.epoch(part)?;
                return self
                    .store
                    .compact(part, epoch, scope, compacting.borrowed())
                    .await;
            }
            let part = self.partition(scope);
            self.admit(part)?;
            let append = Append {
                scope: Arc::from(scope),
                rows: rows.to_vec(),
            };
            self.submit(part, append, compacting).await
        })
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        Some(Box::pin(async move {
            let part = self.partition(scope);
            let epoch = self.epoch(part)?;
            self.store.extend(part, epoch, scope, extension).await?;
            if let Some(ring) = self.ring.read(Clone::clone) {
                ring(scope, extension.seq);
            }
            Ok(())
        }))
    }

    fn carry<'a>(
        &'a self,
        _scope: &'a str,
        _write: Write<'a>,
        _rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        None
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        self.store.checkpoint(scope)
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.store.save_checkpoint(scope, checkpoint)
    }
}

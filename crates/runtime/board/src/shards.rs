//! Partitioned boards: tasks hash onto P partitions, each its own log scope with its own seq.
//!
//! A task lives in one partition for life, picked by one function, `partition_of`, over its
//! routing key: a task that names a thread sits in the partition of the thread's UUID as 16 raw
//! bytes, one that names a project in its project's, whether or not it has a parent; any other
//! child sits in its parent's, and any other root in its own id's. A child placed away from its
//! parent is posted with the parent named by its `parent` label instead of its fold link, and the
//! merged view joins the two (`Merged::parent`, `Merged::children`). Single-task events (claim,
//! renew, finish, reply, relabel, rank) go to the task's partition, so claims on tasks in
//! different partitions never meet in one log, and claims on different tasks in one partition
//! share a group commit rather than racing for a seq. Grants and revokes go to every partition. A
//! move across partitions lands through the outbox. Reads fold each partition on its own and
//! merge the folds into one view of the whole board.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    sync::Arc,
};

use nanocode_partitions::{Partition, partition_of};
use nanocode_sync::{mpsc, oneshot};
use nanocode_turnlog::LogHost;

use crate::{
    Applied, Board, BoardError, Fold, NewTask, Op, Task, TaskId, Write, access,
    deliver::{Delivery, Key, Outbox},
    sim::{Mutation, mutated},
    tasks::TaskOp,
};

/// The label a task names its thread with; its value is the thread's UUID.
pub const THREAD_LABEL: &str = "thread";

pub const PROJECT_LABEL: &str = "project";

pub const PARENT_LABEL: &str = "parent";

pub const CHILD_LABEL: &str = "child.";

/// The scope of partition `index` of the board at `scope`: one partition keeps the bare scope.
pub fn scope_of(scope: &str, count: NonZeroU32, partition: Partition) -> String {
    if count.get() == 1 {
        scope.to_owned()
    } else {
        format!("{scope}/p{}", partition.0)
    }
}

/// A UUID's 16 raw bytes from its hyphenated or plain hex form.
pub fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = text.bytes().filter(|byte| *byte != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0; 16];
    for (index, pair) in hex.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}

/// The key a root task hashes by: its thread's UUID bytes, else its id's bytes.
pub fn home_key(task: &NewTask) -> Vec<u8> {
    own_key(task).unwrap_or_else(|| task.id.0.as_bytes().to_vec())
}

pub fn own_key(task: &NewTask) -> Option<Vec<u8>> {
    let key = |label: &str| {
        task.labels.get(label).map(|value| {
            uuid_bytes(value).map_or_else(|| value.as_bytes().to_vec(), |bytes| bytes.to_vec())
        })
    };
    task.labels
        .get(THREAD_LABEL)
        .and_then(|thread| uuid_bytes(thread))
        .map(|bytes| bytes.to_vec())
        .or_else(|| key(PROJECT_LABEL))
}

/// Where one write goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    One(Partition),
    Every,
    Cross { from: Partition, to: Partition },
    Refused(String),
}

/// The folds of every partition, read together.
pub struct Merged<'a> {
    folds: Vec<&'a Fold>,
}

impl<'a> Merged<'a> {
    pub const fn new(folds: Vec<&'a Fold>) -> Self {
        Self { folds }
    }

    pub fn count(&self) -> NonZeroU32 {
        NonZeroU32::new(u32::try_from(self.folds.len()).unwrap_or(u32::MAX))
            .unwrap_or(NonZeroU32::MIN)
    }

    pub fn fold(&self, partition: Partition) -> Option<&'a Fold> {
        self.folds.get(partition.0 as usize).copied()
    }

    fn shown(&self) -> impl Iterator<Item = (Partition, &'a Fold)> + '_ {
        let skip = usize::from(mutated(Mutation::MergeSkipsPartition) && self.folds.len() > 1);
        self.folds
            .iter()
            .take(self.folds.len() - skip)
            .enumerate()
            .map(|(index, fold)| (Partition(index as u32), *fold))
    }

    /// The partition that holds this task, if any partition's fold has it.
    pub fn home(&self, id: &TaskId) -> Option<Partition> {
        self.shown()
            .find(|(_, fold)| fold.task(id).is_some())
            .map(|(partition, _)| partition)
    }

    pub fn task(&self, id: &TaskId) -> Option<&'a Task> {
        self.shown().find_map(|(_, fold)| fold.task(id))
    }

    /// Every task on the board, in id order.
    pub fn tasks(&self) -> BTreeMap<&'a TaskId, &'a Task> {
        self.shown()
            .flat_map(|(_, fold)| fold.tasks().iter())
            .map(|(id, task)| (id, &**task))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.shown().map(|(_, fold)| fold.tasks().len()).sum()
    }

    pub fn parent(&self, id: &TaskId) -> Option<TaskId> {
        let task = self.task(id)?;
        task.spec.parent.clone().or_else(|| {
            task.spec
                .labels
                .get(PARENT_LABEL)
                .map(|parent| TaskId(parent.clone()))
        })
    }

    pub fn children(&self, id: &TaskId) -> Vec<&'a TaskId> {
        self.shown()
            .flat_map(|(_, fold)| fold.tasks().iter())
            .filter(|(_, task)| {
                task.spec.parent.as_ref() == Some(id)
                    || (task.spec.parent.is_none()
                        && task.spec.labels.get(PARENT_LABEL) == Some(&id.0))
            })
            .map(|(child, _)| child)
            .collect()
    }

    pub fn place(&self, by: &str, op: Op) -> (Route, Op) {
        let route = self.route(by, &op);
        let Op::Post { mut task } = op else {
            return (route, op);
        };
        if let (Route::One(partition), Some(parent)) = (&route, task.parent.clone())
            && self.home(&parent).is_some_and(|home| home != *partition)
        {
            task.parent = None;
            task.labels.insert(PARENT_LABEL.to_owned(), parent.0);
        }
        (route, Op::Post { task })
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Claimable tasks across partitions: ranked first by rank, then by when they were posted,
    /// then by partition and id, so the order doesn't depend on which partition answered first.
    pub fn ready(&self, at_ms: u64) -> Vec<(Partition, &'a TaskId)> {
        let mut ready: Vec<(u64, Partition, &'a TaskId)> = self
            .shown()
            .flat_map(|(partition, fold)| {
                fold.ready(at_ms).map(move |id| {
                    let rank = fold.task(id).and_then(|task| task.rank).unwrap_or(u64::MAX);
                    (rank, partition, id)
                })
            })
            .collect();
        ready.sort();
        ready
            .into_iter()
            .map(|(_, partition, id)| (partition, id))
            .collect()
    }

    /// Rows folded across every partition.
    pub fn rows(&self) -> u64 {
        self.shown().map(|(_, fold)| fold.next()).sum()
    }

    /// Records held across every partition (Fold::records).
    pub fn held(&self) -> u64 {
        self.shown().map(|(_, fold)| fold.records() as u64).sum()
    }

    /// Where a write goes, given what the folds know about the tasks it names.
    pub fn route(&self, by: &str, op: &Op) -> Route {
        let count = self.count();
        let hash = |key: &[u8]| partition_of(key, count);
        let home = |id: &TaskId| self.home(id).unwrap_or_else(|| hash(id.0.as_bytes()));
        if mutated(Mutation::RouteIgnoresTask) {
            return Route::One(hash(by.as_bytes()));
        }
        match op {
            Op::Post { task } => Route::One(match (own_key(task), &task.parent) {
                (Some(key), _) if !mutated(Mutation::ChildFollowsParent) => hash(&key),
                (_, Some(parent)) => home(parent),
                (_, None) => hash(&home_key(task)),
            }),
            Op::Tasks(TaskOp::Batch { name, tasks }) => Route::One(
                tasks
                    .iter()
                    .find_map(|planned| match &planned.parent {
                        Some(crate::tasks::Need::Task(parent)) => Some(home(parent)),
                        _ => None,
                    })
                    .unwrap_or_else(|| hash(name.0.as_bytes())),
            ),
            Op::Move { task, parent } => {
                let from = home(task);
                let to = parent.as_ref().map_or(from, home);
                let linked = self
                    .task(task)
                    .is_some_and(|task| task.spec.labels.contains_key(PARENT_LABEL));
                if to != from || linked {
                    Route::Cross { from, to }
                } else {
                    Route::One(from)
                }
            }
            Op::Epoch { partition, .. } if *partition < count.get() => {
                Route::One(Partition(*partition))
            }
            Op::Epoch { partition, .. } => Route::Refused(format!(
                "partition {partition} is not one of the board's {count}"
            )),
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} => Route::Every,
            _ => Route::One(op.task().map_or(Partition(0), home)),
        }
    }
}

impl Merged<'_> {
    pub fn moves(
        &self,
        by: &str,
        seq: u64,
        task: &TaskId,
        parent: Option<&TaskId>,
    ) -> Result<Vec<Delivery>, String> {
        let from = self
            .home(task)
            .ok_or_else(|| format!("{task} is not on the board"))?;
        let moving = self
            .task(task)
            .ok_or_else(|| format!("{task} is not on the board"))?;
        if moving.finished() {
            return Err(format!("{task} is finished"));
        }
        let to = self.destination(task, parent)?;
        let mut steps = local_steps(
            task,
            parent,
            from,
            to,
            moving.spec.parent.is_some(),
            moving.spec.labels.contains_key(PARENT_LABEL),
        );
        self.push_marks(&mut steps, task, parent, from, to);
        self.deliveries(by, seq, steps)
    }

    fn destination(
        &self,
        task: &TaskId,
        parent: Option<&TaskId>,
    ) -> Result<Option<Partition>, String> {
        Ok(match parent {
            None => None,
            Some(parent) => {
                if self.task(parent).is_none_or(Task::finished) {
                    return Err(format!("{parent} is not open on the board"));
                }
                let mut at = Some(parent.clone());
                while let Some(id) = at {
                    if &id == task {
                        return Err(format!("{task} is above {parent}"));
                    }
                    at = self.parent(&id);
                }
                self.home(parent)
            }
        })
    }

    fn push_marks(
        &self,
        steps: &mut Vec<(Partition, Op)>,
        task: &TaskId,
        parent: Option<&TaskId>,
        from: Partition,
        to: Option<Partition>,
    ) {
        let old = self.parent(task);
        let mark = CHILD_LABEL.to_owned() + &task.0;
        if let Some(old) = old.filter(|old| Some(old) != parent)
            && let Some(at) = self.home(&old)
            && at != from
            && !mutated(Mutation::CrossMoveSkipsUnmark)
        {
            steps.push((
                at,
                Op::Relabel {
                    task: old,
                    set: BTreeMap::new(),
                    unset: BTreeSet::from([mark.clone()]),
                },
            ));
        }
        if let (Some(parent), Some(to)) = (parent, to)
            && to != from
        {
            steps.push((
                to,
                Op::Relabel {
                    task: parent.clone(),
                    set: BTreeMap::from([(mark, from.0.to_string())]),
                    unset: BTreeSet::new(),
                },
            ));
        }
    }

    fn deliveries(
        &self,
        by: &str,
        seq: u64,
        steps: Vec<(Partition, Op)>,
    ) -> Result<Vec<Delivery>, String> {
        let count = self.count();
        steps
            .into_iter()
            .enumerate()
            .map(|(index, (partition, op))| {
                Ok(Delivery {
                    key: Key {
                        source: by.to_owned(),
                        seq,
                        index: u32::try_from(index).map_err(|error| error.to_string())?,
                    },
                    target: scope_of("", count, partition),
                    part: partition.0,
                    payload: serde_json::to_string(&op).map_err(|error| error.to_string())?,
                })
            })
            .collect()
    }
}

fn local_steps(
    task: &TaskId,
    parent: Option<&TaskId>,
    from: Partition,
    to: Option<Partition>,
    local_parent: bool,
    labelled: bool,
) -> Vec<(Partition, Op)> {
    let gate = |parent: Option<&TaskId>| Op::Move {
        task: task.clone(),
        parent: parent.cloned(),
    };
    let label = |set: Option<&TaskId>| Op::Relabel {
        task: task.clone(),
        set: set
            .map(|parent| BTreeMap::from([(PARENT_LABEL.to_owned(), parent.0.clone())]))
            .unwrap_or_default(),
        unset: if set.is_some() {
            BTreeSet::new()
        } else {
            BTreeSet::from([PARENT_LABEL.to_owned()])
        },
    };
    let mut steps: Vec<(Partition, Op)> = Vec::new();
    match (parent, to) {
        (Some(parent), Some(to)) if to != from => {
            if local_parent {
                steps.push((from, label(Some(parent))));
                if !mutated(Mutation::CrossMoveKeepsParent) {
                    steps.push((from, gate(None)));
                }
            } else {
                steps.push((from, gate(None)));
                steps.push((from, label(Some(parent))));
            }
        }
        _ => {
            steps.push((from, gate(parent)));
            if labelled {
                steps.push((from, label(None)));
            }
        }
    }
    steps
}

pub fn delivery_key(key: &Key) -> String {
    format!("move:{}:{}", key.seq, key.index)
}

pub async fn deliver(
    outbox: &mut Outbox,
    committers: &[Committer],
    at_ms: u64,
    mut persist: impl FnMut(&Outbox) -> bool,
) -> Result<u64, BoardError> {
    let mut settled = 0;
    while let Some(delivery) = outbox.next().cloned() {
        let op: Op =
            serde_json::from_str(&delivery.payload).map_err(|error| BoardError::Corrupt {
                seq: delivery.key.seq,
                detail: error.to_string(),
            })?;
        let committer = committers.get(delivery.part as usize).ok_or_else(|| {
            BoardError::Dropped(format!("no committer for partition {}", delivery.part))
        })?;
        if mutated(Mutation::CrossMoveIngestsFirst) {
            let _ = outbox.ingest(&delivery.key);
            if !persist(outbox) {
                return Ok(settled);
            }
        }
        let verdict = committer
            .submit(Write {
                by: Some(delivery.key.source.clone()),
                key: Some(delivery_key(&delivery.key)),
                at_ms,
                op,
            })
            .await;
        match verdict {
            Ok(_) => {}
            Err(BoardError::Rejected(_)) => {
                let rest: Vec<Key> = outbox
                    .pending_of(&delivery.key.source, delivery.key.seq)
                    .cloned()
                    .collect();
                for key in rest {
                    let _ = outbox.ingest(&key);
                }
            }
            Err(error) => return Err(error),
        }
        let _ = outbox.ingest(&delivery.key);
        settled += 1;
        if !persist(outbox) {
            return Ok(settled);
        }
    }
    Ok(settled)
}

#[derive(Clone, Debug, Default)]
pub struct ReadyIndex {
    folded: Vec<u64>,
    order: BTreeSet<(u64, Partition, TaskId)>,
    ranks: BTreeMap<(Partition, TaskId), u64>,
}

impl ReadyIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn catch_up(&mut self, merged: &Merged<'_>) {
        let count = merged.folds.len();
        assert!(
            self.folded.len() <= count,
            "a board does not lose partitions"
        );
        self.folded.resize(count, 0);
        for (index, fold) in merged.folds.iter().enumerate() {
            let partition = Partition(index as u32);
            let entries = fold.entries();
            let from = entries.partition_point(|entry| entry.seq < self.folded[index]);
            let fresh = &entries[from..];
            let Some(last) = fresh.last() else {
                continue;
            };
            let skip = usize::from(mutated(Mutation::MergedIndexSkipsRows));
            for entry in &fresh[..fresh.len() - skip] {
                self.refresh(partition, fold, &entry.task);
            }
            self.folded[index] = last.seq + 1;
        }
    }

    fn refresh(&mut self, partition: Partition, fold: &Fold, id: &TaskId) {
        let key = (partition, id.clone());
        if let Some(rank) = self.ranks.remove(&key) {
            assert!(
                self.order.remove(&(rank, partition, id.clone())),
                "an indexed task is in the order"
            );
        }
        let Some(task) = fold.task(id) else {
            return;
        };
        if task.phase.terminal() {
            return;
        }
        let rank = if mutated(Mutation::MergedIndexIgnoresRank) {
            u64::MAX
        } else {
            task.rank.unwrap_or(u64::MAX)
        };
        self.order.insert((rank, partition, id.clone()));
        self.ranks.insert(key, rank);
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn ready<'a>(
        &'a self,
        merged: &'a Merged<'a>,
        at_ms: u64,
    ) -> impl Iterator<Item = (Partition, &'a TaskId)> + 'a {
        self.order
            .iter()
            .filter(move |(_, partition, id)| {
                merged
                    .fold(*partition)
                    .is_some_and(|fold| fold.claimable(id, at_ms).is_ok())
            })
            .map(|(_, partition, id)| (*partition, id))
    }
}

type Reply = oneshot::Sender<Result<Applied, BoardError>>;

/// A handle that queues writes for one partition's committer.
#[derive(Clone)]
pub struct Committer {
    queue: mpsc::Sender<(Write, Reply)>,
}

impl Committer {
    /// A committer for `board` and the loop that drives it: run the loop on one task, hand
    /// clones of the committer to every writer. Each pass takes every queued write, up to
    /// `batch`, and commits them as one group. The loop returns the board once every committer
    /// is dropped.
    pub fn start(
        board: Board,
        batch: usize,
    ) -> (Self, impl std::future::Future<Output = (Board, Stats)>) {
        let group_writes_max = batch.max(1);
        let (queue, mut writes) = mpsc::channel::<(Write, Reply)>(group_writes_max);
        let drive = async move {
            let mut board = board;
            let mut stats = Stats::default();
            let mut taken = Vec::new();
            while writes.recv_many(&mut taken, group_writes_max).await > 0 {
                let (group, replies): (Vec<Write>, Vec<Reply>) = taken.drain(..).unzip();
                stats.groups += 1;
                stats.writes += group.len() as u64;
                stats.largest = stats.largest.max(group.len() as u64);
                let (verdicts, rounds) = board.submit_group(group).await;
                stats.appends += rounds;
                for (reply, verdict) in replies.into_iter().zip(verdicts) {
                    if verdict.is_ok() {
                        stats.applied += 1;
                    }
                    let _ = reply.send(verdict);
                }
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if yielded {
                        std::task::Poll::Ready(())
                    } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                })
                .await;
            }
            (board, stats)
        };
        (Self { queue }, drive)
    }

    pub async fn submit(&self, write: Write) -> Result<Applied, BoardError> {
        let (reply, verdict) = oneshot::channel();
        self.queue
            .send((write, reply))
            .await
            .map_err(|_| BoardError::Dropped("the partition's committer stopped".into()))?;
        verdict.await.unwrap_or_else(|_| {
            Err(BoardError::Dropped(
                "the partition's committer stopped".into(),
            ))
        })
    }
}

/// What one committer did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub groups: u64,
    pub writes: u64,
    pub applied: u64,
    pub appends: u64,
    pub largest: u64,
}

/// Catches every partition's board up with its log in one read where they share a host (one round trip and one
/// snapshot, rather than a scan per partition); boards on other hosts refresh one by one.
pub async fn refresh(boards: &mut [Board]) -> Result<(), BoardError> {
    let Some(host) = boards.first().map(|board| Arc::clone(board.host())) else {
        return Ok(());
    };
    if boards.iter().any(|board| !Arc::ptr_eq(board.host(), &host)) {
        for board in boards.iter_mut() {
            board.refresh().await?;
        }
        return Ok(());
    }
    let scans: Vec<(&str, u64)> = boards
        .iter()
        .map(|board| (board.scope(), board.fold().next()))
        .collect();
    let rows = host.scans(&scans).await?;
    drop(scans);
    for (board, rows) in boards.iter_mut().zip(rows) {
        board.fold_rows(rows)?;
    }
    Ok(())
}

/// Opens the board for every partition of `scope` on one host.
pub async fn open(
    host: &Arc<dyn LogHost>,
    scope: &str,
    count: NonZeroU32,
    identity: &access::Identity,
) -> Result<Vec<Board>, BoardError> {
    let mut boards = Vec::with_capacity(count.get() as usize);
    for index in 0..count.get() {
        let scope = scope_of(scope, count, Partition(index));
        boards.push(Board::open(Arc::clone(host), &scope, identity.clone()).await?);
    }
    Ok(boards)
}

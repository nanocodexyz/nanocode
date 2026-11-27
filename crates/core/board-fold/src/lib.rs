use std::collections::{BTreeMap, BTreeSet};

use nanocode_durability_core::StoreError;
use serde::{Deserialize, Serialize};

pub mod access;
pub mod brief;
mod compact;
pub mod deliver;
pub mod delivery;
pub mod intent;
pub mod mirror;
pub mod retain;
pub mod schedule;
pub mod sim;
pub mod stage;
pub mod steer;
pub mod tasks;
pub mod watch;

use sim::{Mutation, mutated};
use stage::{Act, Stage};
use tasks::{Moved, Occurrence, Refusal, TaskOp, Terms};

const FIRST: u64 = 1;

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

id!(TaskId);
id!(ClaimId);
id!(EventId);

pub mod blocks;
pub mod group;
mod ids;
pub mod messages;

pub use compact::Spread;
pub use retain::{Evicted, Retain};

/// The version a board writes its fold checkpoints under, and the only one it reopens from: the
/// snapshot format and a hash of the fold's source, so a build whose fold changed replays the
/// whole log once instead of trusting a fold an older build made. Bump the format when the
/// fold's state changes in a way no source hash sees.
pub const CHECKPOINT_VERSION: &str = concat!("board-fold/1/", env!("NANOCODE_BOARD_FOLD_SOURCE"));
pub use messages::{MessageId, Say};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTask {
    pub id: TaskId,
    pub parent: Option<TaskId>,
    pub spec: String,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Terms::is_default")]
    pub terms: Terms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Fence {
    pub partition: u32,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Done(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cap {
    Partition(u32),
    Label {
        max: u32,
        key: String,
        value: String,
    },
}

impl Cap {
    pub const fn max(&self) -> u32 {
        match self {
            Self::Partition(max) | Self::Label { max, .. } => *max,
        }
    }

    pub fn covers(&self, task: &Task) -> bool {
        match self {
            Self::Partition(_) => true,
            Self::Label { key, value, .. } => task.spec.labels.get(key) == Some(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Post {
        task: NewTask,
    },
    Claim {
        task: TaskId,
        lease_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fence: Option<Fence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<Cap>,
    },
    Renew {
        task: TaskId,
        claim: ClaimId,
        lease_ms: u64,
    },
    Release {
        task: TaskId,
        claim: ClaimId,
        note: String,
    },
    Finish {
        task: TaskId,
        claim: ClaimId,
        outcome: Outcome,
    },
    Cancel {
        task: TaskId,
        reason: String,
    },
    Reply {
        task: TaskId,
        body: String,
    },
    Relabel {
        task: TaskId,
        set: BTreeMap<String, String>,
        unset: BTreeSet<String>,
    },
    Move {
        task: TaskId,
        parent: Option<TaskId>,
    },
    Rank {
        task: TaskId,
        rank: Option<u64>,
    },
    Grant {
        grant: access::Grant,
    },
    Revoke {
        grant: EventId,
    },
    Epoch {
        partition: u32,
        epoch: u64,
    },
    Charter {},
    TextLimit {
        chars: Option<u64>,
    },
    Tasks(TaskOp),
    Sync {
        sync: mirror::SyncOp,
    },
    Say {
        say: Say,
    },
    Watch(watch::Step),
    Steer(steer::SteerOp),
}

impl Op {
    pub const fn task(&self) -> Option<&TaskId> {
        match self {
            Self::Post { task } => Some(&task.id),
            Self::Claim { task, .. }
            | Self::Renew { task, .. }
            | Self::Release { task, .. }
            | Self::Finish { task, .. }
            | Self::Cancel { task, .. }
            | Self::Reply { task, .. }
            | Self::Relabel { task, .. }
            | Self::Rank { task, .. }
            | Self::Move { task, .. } => Some(task),
            Self::Tasks(op) => Some(op.task()),
            Self::Sync { sync } => sync.task(),
            Self::Steer(op) => Some(op.task()),
            Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::Epoch { .. }
            | Self::Charter {}
            | Self::TextLimit { .. }
            | Self::Say { .. }
            | Self::Watch(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub by: String,
    pub at_ms: u64,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Open,
    Claimed {
        claim: ClaimId,
        worker: String,
        until_ms: u64,
    },
    Done {
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker: Option<String>,
    },
    Failed {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker: Option<String>,
    },
    Cancelled {
        reason: String,
    },
    Blocked {
        kind: String,
        reason: String,
    },
}

impl Phase {
    pub const fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub spec: NewTask,
    pub phase: Phase,
    pub attempts: u64,
    pub replies: Vec<(String, String)>,
    pub assignee: Option<String>,
    pub rank: Option<u64>,
    #[serde(default)]
    pub due_at_ms: Option<u64>,
    #[serde(default)]
    pub occurrence: Option<Occurrence>,
    /// The cap the current claim was taken under: renewing a lapsed claim takes a slot again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<Cap>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    Exists,
    UnknownTask,
    UnknownParent(TaskId),
    UnknownNeed(TaskId),
    Waiting(TaskId),
    Held { claim: ClaimId, until_ms: u64 },
    NotHolder,
    Terminal,
    Access(access::Denied),
    Tasks(Refusal),
    Sync(mirror::Refusal),
    Intent(intent::Refusal),
    Said(messages::Refusal),
    Watch(watch::Refusal),
    Steer(steer::Refusal),
    Illegal { from: String, to: String },
    Stale { fence: Fence },
    Full { cap: Cap },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Posted,
    Claimed {
        claim: ClaimId,
        worker: String,
        until_ms: u64,
        took_over: Option<ClaimId>,
    },
    Renewed {
        claim: ClaimId,
        until_ms: u64,
    },
    Released {
        claim: ClaimId,
        note: String,
    },
    Finished(Outcome),
    Cancelled {
        reason: String,
    },
    Replied {
        by: String,
        body: String,
    },
    Tasks(Moved),
    Relabeled {
        set: BTreeMap<String, String>,
        unset: BTreeSet<String>,
    },
    Reparented {
        from: Option<TaskId>,
        to: Option<TaskId>,
    },
    Ranked {
        rank: Option<u64>,
    },
    Synced(mirror::Synced),
    Steered(steer::Steered),
    Reported {
        child: TaskId,
        report: intent::Report,
    },
}

impl Change {
    pub const fn terminal(&self) -> bool {
        matches!(self, Self::Finished(_) | Self::Cancelled { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub event: EventId,
    pub at_ms: u64,
    pub task: TaskId,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub seq: u64,
    pub event: EventId,
}

#[derive(Clone, Debug)]
pub enum BoardError {
    Store(StoreError),
    Corrupt { seq: u64, detail: String },
    Rejected(Rejection),
    Unsettled { event: EventId, error: StoreError },
    Dropped(String),
    Row { bytes: u64, max: u64 },
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "board store: {error}"),
            Self::Corrupt { seq, detail } => write!(formatter, "board row {seq}: {detail}"),
            Self::Rejected(rejection) => {
                write!(formatter, "board rejected the write: {rejection:?}")
            }
            Self::Unsettled { event, error } => {
                write!(formatter, "board write {event} is unsettled: {error}")
            }
            Self::Dropped(why) => write!(formatter, "board middleware dropped the write: {why}"),
            Self::Row { bytes, max } => write!(
                formatter,
                "board row of {bytes} bytes is over the host's {max}-byte row bound"
            ),
        }
    }
}

impl From<StoreError> for BoardError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fold {
    next: u64,
    now_ms: u64,
    #[serde(with = "crate::pairs")]
    tasks: BTreeMap<TaskId, Box<Task>>,
    #[serde(with = "crate::pairs")]
    children: BTreeMap<TaskId, BTreeSet<TaskId>>,
    #[serde(with = "crate::pairs")]
    dependents: BTreeMap<TaskId, BTreeSet<TaskId>>,
    #[serde(with = "crate::pairs")]
    seen: BTreeMap<EventId, Result<u64, Rejection>>,
    entries: Vec<Entry>,
    access: access::Access,
    mirror: mirror::Mirror,
    live: BTreeSet<TaskId>,
    #[serde(skip)]
    posted: BTreeMap<TaskId, usize>,
    #[serde(with = "crate::pairs")]
    labelled: BTreeMap<String, BTreeMap<String, ids::Ids>>,
    said: messages::Said,
    marks: watch::Marks,
    #[serde(default)]
    steers: steer::Steers,
    #[serde(default)]
    intents: intent::Intents,
    #[serde(default)]
    retention: retain::Retention,
    #[serde(skip)]
    refused: Option<nanocode_machine::Illegal<Stage>>,
}

impl Default for Fold {
    fn default() -> Self {
        Self::new()
    }
}

impl Fold {
    pub const fn new() -> Self {
        Self {
            next: FIRST,
            now_ms: 0,
            tasks: BTreeMap::new(),
            children: BTreeMap::new(),
            dependents: BTreeMap::new(),
            seen: BTreeMap::new(),
            entries: Vec::new(),
            access: access::Access::new(),
            mirror: mirror::Mirror::new(),
            live: BTreeSet::new(),
            posted: BTreeMap::new(),
            labelled: BTreeMap::new(),
            said: messages::Said::new(),
            marks: watch::Marks::new(),
            steers: steer::Steers::new(),
            intents: intent::Intents::new(),
            retention: retain::Retention::new(),
            refused: None,
        }
    }

    pub const fn next(&self) -> u64 {
        self.next
    }

    /// What the fold holds now, in records: tasks, entries, dedupe outcomes and said messages. Unlike
    /// next (every row ever folded), it stays flat on a board whose retention keeps up.
    pub fn records(&self) -> usize {
        self.tasks.len() + self.entries.len() + self.seen.len() + self.said.len()
    }

    pub const fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub const fn tasks(&self) -> &BTreeMap<TaskId, Box<Task>> {
        &self.tasks
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id).map(|task| &**task)
    }

    pub fn epoch(&self, partition: u32) -> u64 {
        self.access.epoch(partition)
    }

    pub fn outcome(&self, event: &EventId) -> Option<&Result<u64, Rejection>> {
        self.seen.get(event)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn minted(&self, writer: &str) -> u64 {
        let prefix = format!("{writer}.");
        self.seen
            .range(EventId(prefix.clone())..)
            .map_while(|(id, _)| id.0.strip_prefix(&prefix))
            .filter_map(|count| count.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .max(self.retention.minted(writer))
    }

    pub fn claimable(&self, id: &TaskId, at_ms: u64) -> Result<Option<ClaimId>, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        let now = self.now_ms.max(at_ms);
        match task.may(Act::Claim, now) {
            Ok(Stage::Lapsed)
                if mutated(Mutation::LeasesNeverExpire)
                    && !mutated(Mutation::ClaimIgnoresLease) =>
            {
                return Err(task.refusal(Act::Claim, Stage::Held));
            }
            Ok(_) => {}
            Err(refused) => {
                let loose = match task.stage(now) {
                    Stage::Held => mutated(Mutation::ClaimIgnoresLease),
                    Stage::Blocked => mutated(Mutation::ClaimIgnoresBlock),
                    Stage::Scheduled => mutated(Mutation::DueIgnored),
                    _ => false,
                };
                if !loose {
                    return Err(refused);
                }
            }
        }
        if let Phase::Claimed { claim, .. } = &task.phase {
            return Ok(Some(claim.clone()));
        }
        if mutated(Mutation::ClaimIgnoresNeeds) {
            return Ok(None);
        }
        match task.spec.needs.iter().find(|need| {
            !matches!(
                self.tasks.get(*need).map(|need| &need.phase),
                Some(Phase::Done { .. })
            )
        }) {
            Some(need) => Err(Rejection::Waiting(need.clone())),
            None => Ok(None),
        }
    }

    pub fn ready(&self, at_ms: u64) -> impl Iterator<Item = &TaskId> + '_ {
        self.live
            .iter()
            .filter(move |id| self.claimable(id, at_ms).is_ok())
    }

    pub fn labelled<'a>(&'a self, key: &str, value: &str) -> impl Iterator<Item = &'a TaskId> + 'a {
        self.labelled
            .get(key)
            .and_then(|values| values.get(value))
            .into_iter()
            .flatten()
    }

    pub fn ready_labelled<'a>(
        &'a self,
        key: &str,
        value: &str,
        at_ms: u64,
    ) -> impl Iterator<Item = &'a TaskId> + 'a {
        self.labelled(key, value)
            .filter(move |id| self.claimable(id, at_ms).is_ok())
    }

    fn reindex(&mut self, id: &TaskId) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let live = !task.phase.terminal();
        for (key, value) in &task.spec.labels {
            if live {
                self.labelled
                    .entry(key.clone())
                    .or_default()
                    .entry(value.clone())
                    .or_default()
                    .insert(id.clone());
            } else if let Some(ids) = self
                .labelled
                .get_mut(key)
                .and_then(|values| values.get_mut(value))
            {
                ids.remove(id);
            }
        }
        if live {
            self.live.insert(id.clone());
        } else {
            self.live.remove(id);
        }
    }

    fn unlabel(&mut self, id: &TaskId, keys: impl Iterator<Item = String>) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        for key in keys {
            if let Some(value) = task.spec.labels.get(&key)
                && let Some(ids) = self
                    .labelled
                    .get_mut(&key)
                    .and_then(|values| values.get_mut(value))
            {
                ids.remove(id);
            }
        }
    }

    pub fn next_due(&self) -> Option<u64> {
        self.tasks
            .values()
            .filter(|task| task.phase == Phase::Open)
            .filter_map(|task| task.due_at_ms)
            .filter(|due_at_ms| *due_at_ms > self.now_ms)
            .min()
    }

    pub fn settled(&self) -> bool {
        self.tasks.values().all(|task| task.finished())
    }

    fn holder(&self, id: &TaskId, held: &ClaimId) -> Result<&Task, Rejection> {
        self.holds(id, held, Act::Renew)
    }

    fn holds(&self, id: &TaskId, held: &ClaimId, act: Act) -> Result<&Task, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        task.may(act, self.now_ms)?;
        match &task.phase {
            Phase::Claimed { claim, .. } if claim == held => Ok(task),
            _ => Err(Rejection::NotHolder),
        }
    }

    pub fn check(&self, at_ms: u64, op: &Op) -> Result<(), Rejection> {
        match op {
            Op::Post { task } => {
                if self.tasks.contains_key(&task.id)
                    && !mutated(Mutation::PostOverwrites)
                    && !self.reposted(task)
                {
                    return Err(Rejection::Exists);
                }
                if self.retention.gone(&task.id) {
                    return Err(Rejection::Exists);
                }
                if let Some(parent) = &task.parent
                    && !self.tasks.contains_key(parent)
                {
                    return Err(Rejection::UnknownParent(parent.clone()));
                }
                task.terms.check()?;
                match task
                    .needs
                    .iter()
                    .find(|need| !self.tasks.contains_key(*need))
                {
                    Some(need) => Err(Rejection::UnknownNeed(need.clone())),
                    None => Ok(()),
                }
            }
            Op::Claim {
                task, fence, cap, ..
            } => {
                self.fenced(fence.as_ref())?;
                self.claimable(task, at_ms)?;
                self.room(task, cap.as_ref(), at_ms)
            }
            Op::Finish { task, .. } if mutated(Mutation::FinishIgnoresClaim) => {
                match self.tasks.get(task).map(|task| &task.phase) {
                    None => Err(Rejection::UnknownTask),
                    Some(phase) if phase.terminal() => Err(Rejection::Terminal),
                    Some(Phase::Open) => Err(Rejection::NotHolder),
                    Some(_) => Ok(()),
                }
            }
            Op::Finish {
                task,
                claim,
                outcome,
            } => {
                self.holds(task, claim, Act::Finish)?;
                self.unheeded(task)?;
                self.scheduled(task, self.now_ms.max(at_ms), outcome)
                    .map(drop)
            }
            Op::Renew { task, claim, .. } => {
                let held = self.holds(task, claim, Act::Renew)?;
                if held.stage(self.now_ms.max(at_ms)) == Stage::Lapsed
                    && !mutated(Mutation::RenewIgnoresCap)
                {
                    self.room(task, held.cap.as_ref(), at_ms)
                } else {
                    Ok(())
                }
            }
            Op::Release { task, claim, .. } => self.holds(task, claim, Act::Release).map(drop),
            Op::Cancel { task, .. } => match self.tasks.get(task) {
                None => Err(Rejection::UnknownTask),
                Some(task) => task.may(Act::Cancel, self.now_ms.max(at_ms)).map(drop),
            },
            Op::Reply { task, .. } | Op::Relabel { task, .. } | Op::Rank { task, .. } => {
                self.tasks.get(task).map(drop).ok_or(Rejection::UnknownTask)
            }
            Op::Move { task, parent } => self.movable(task, parent.as_ref()),
            Op::Epoch { partition, epoch } => self.advances(*partition, *epoch),
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} | Op::TextLimit { .. } => Ok(()),
            Op::Tasks(op) => self.check_tasks(at_ms, op),
            Op::Sync { sync } => self.check_sync(sync),
            Op::Say { .. } => Ok(()),
            Op::Watch(step) => self.check_step(at_ms, step),
            Op::Steer(op) => self.check_steer(op),
        }
    }

    pub fn children(&self, task: &TaskId) -> impl Iterator<Item = &TaskId> {
        self.children.get(task).into_iter().flatten()
    }

    fn fenced(&self, fence: Option<&Fence>) -> Result<(), Rejection> {
        let Some(fence) = fence else {
            return Ok(());
        };
        let held = Fence {
            partition: fence.partition,
            epoch: self.access.epoch(fence.partition),
        };
        if held == *fence || mutated(Mutation::EpochFenceIgnored) {
            Ok(())
        } else {
            Err(Rejection::Stale { fence: held })
        }
    }

    pub fn held(&self, cap: &Cap, at_ms: u64) -> usize {
        let now = self.now_ms.max(at_ms);
        self.live
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .filter(|task| task.stage(now) == Stage::Held && cap.covers(task))
            .count()
    }

    fn room(&self, task: &TaskId, cap: Option<&Cap>, at_ms: u64) -> Result<(), Rejection> {
        let Some(cap) = cap else {
            return Ok(());
        };
        let now = self.now_ms.max(at_ms);
        let max = cap.max() as usize;
        let others = self
            .live
            .iter()
            .filter(|id| *id != task)
            .filter_map(|id| self.tasks.get(id))
            .filter(|held| held.stage(now) == Stage::Held)
            .filter(|held| cap.covers(held) || mutated(Mutation::ClaimCapIgnoresLabel))
            .take(max)
            .count();
        if others < max || mutated(Mutation::ClaimCapIgnored) {
            Ok(())
        } else {
            Err(Rejection::Full { cap: cap.clone() })
        }
    }

    fn advances(&self, partition: u32, epoch: u64) -> Result<(), Rejection> {
        let epoch_held = self.access.epoch(partition);
        if epoch > epoch_held {
            Ok(())
        } else {
            Err(Rejection::Stale {
                fence: Fence {
                    partition,
                    epoch: epoch_held,
                },
            })
        }
    }

    fn movable(&self, task: &TaskId, parent: Option<&TaskId>) -> Result<(), Rejection> {
        match self.tasks.get(task) {
            None => return Err(Rejection::UnknownTask),
            Some(moving) if moving.finished() => return Err(Rejection::Terminal),
            Some(_) => {}
        }
        let Some(parent) = parent else {
            return Ok(());
        };
        match self.tasks.get(parent) {
            None => return Err(Rejection::UnknownParent(parent.clone())),
            Some(above) if above.finished() => return Err(Rejection::Terminal),
            Some(_) => {}
        }
        if mutated(Mutation::MoveAllowsCycle) {
            return Ok(());
        }
        if watch::under(&self.tasks, parent, task) {
            return Err(Rejection::Tasks(Refusal::Cycle(task.0.clone())));
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.snapshot_spread(Spread::for_fold(self))
    }

    pub fn snapshot_spread(&self, spread: Spread) -> Vec<u8> {
        compact::snapshot(self, spread).expect("a fold encodes")
    }

    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(self).expect("a fold serializes")
    }

    pub fn restore(snapshot: &[u8]) -> Result<Self, BoardError> {
        Self::restore_spread(snapshot, Spread::for_snapshot(snapshot))
    }

    pub fn restore_spread(snapshot: &[u8], spread: Spread) -> Result<Self, BoardError> {
        let corrupt = |detail: String| BoardError::Corrupt {
            seq: 0,
            detail: format!("fold snapshot: {detail}"),
        };
        let mut fold: Self = if snapshot.starts_with(compact::MAGIC) {
            compact::restore(snapshot, spread).map_err(|error| corrupt(error.to_string()))?
        } else {
            serde_json::from_slice(snapshot).map_err(|error| corrupt(error.to_string()))?
        };
        fold.posted = fold.restored_posts();
        fold.restore_retention();
        if mutated(Mutation::SnapshotSkipsRows) {
            fold.next += 1;
        }
        if mutated(Mutation::SnapshotLosesDedupe) {
            fold.seen.retain(|_, verdict| verdict.is_ok());
        }
        Ok(fold)
    }

    pub fn apply(&mut self, seq: u64, body: &str) -> Result<(), BoardError> {
        let event: Event = serde_json::from_str(body).map_err(|error| BoardError::Corrupt {
            seq,
            detail: error.to_string(),
        })?;
        self.apply_event(seq, &event)
    }

    pub fn apply_event(&mut self, seq: u64, event: &Event) -> Result<(), BoardError> {
        if seq != self.next {
            return Err(BoardError::Corrupt {
                seq,
                detail: format!("expected row {}", self.next),
            });
        }
        self.next = seq + 1;
        let replayed = self.seen.contains_key(&event.id) && !mutated(Mutation::ReplaysApply);
        if replayed || !access::owns(&event.by, &event.id) {
            return Ok(());
        }
        self.now_ms = self.now_ms.max(event.at_ms);
        if let Some(original) = self.repost(event) {
            self.seen.insert(event.id.clone(), Ok(original));
            return Ok(());
        }
        let verdict = self.judge(event).map(|()| seq);
        self.seen.insert(event.id.clone(), verdict.clone());
        self.heard(&event.id, &event.op);
        let from = self.entry_count();
        if verdict.is_ok() {
            self.access.record(&event.by, &event.id, &event.op);
            self.transition(seq, event);
            if let Some(illegal) = self.refused.take() {
                self.seen
                    .insert(event.id.clone(), Err(stage::rejection(illegal)));
                self.retained(from, seq);
                return Ok(());
            }
            self.intend(seq, event);
            self.hear(seq, event);
            self.index_marks();
            self.retained(from, seq);
        } else if mutated(Mutation::RefusalsCountAttempts) {
            self.miscount(&event.op);
        } else if mutated(Mutation::DeliveryRefusalsWrite) {
            self.mirror.leak(&event.op);
        }
        Ok(())
    }

    pub(crate) fn record(&mut self, seq: u64, event: &Event, task: &TaskId, change: Change) {
        self.observe(seq, event, task, &change);
        if !mutated(Mutation::ReadyIndexStale) {
            self.reindex(task);
        }
        let terminal = change.terminal().then(|| change.clone());
        if change == Change::Posted {
            let at = self.entry_count();
            self.posted.entry(task.clone()).or_insert(at);
        }
        self.entries.push(Entry {
            seq,
            event: event.id.clone(),
            at_ms: self.now_ms,
            task: task.clone(),
            change,
        });
        if let Some(change) = terminal {
            self.reported(seq, event, task, &change);
        }
    }

    fn transition(&mut self, seq: u64, event: &Event) {
        let now = self.now_ms;
        if let Op::Sync { sync } = &event.op {
            self.transition_sync(seq, event, sync);
            return;
        }
        if let Op::Watch(step) = &event.op {
            self.apply_step(seq, event, step);
            return;
        }
        if let Op::Steer(op) = &event.op {
            self.apply_steer(seq, event, op);
            return;
        }
        if let Op::TextLimit { chars } = &event.op {
            self.limit_text(*chars);
            return;
        }
        let Some(id) = event.op.task().cloned() else {
            return;
        };
        let change = match &event.op {
            Op::Post { task } => {
                self.place(seq, event, task, task.id.clone());
                let doomed = task
                    .parent
                    .iter()
                    .chain(&task.needs)
                    .find(|above| self.dooms(above))
                    .cloned();
                if let Some(above) = doomed
                    && !mutated(Mutation::PostIgnoresDoomedNeeds)
                {
                    self.cancel(seq, event, &id, format!("{above} did not finish"));
                }
                return;
            }
            Op::Claim { lease_ms, cap, .. } => {
                let until_ms = now.saturating_add(*lease_ms);
                let claim = ClaimId(event.id.0.clone());
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let took_over = match &task.phase {
                    Phase::Claimed { claim, .. } => Some(claim.clone()),
                    _ => None,
                };
                let claimed = Phase::Claimed {
                    claim: claim.clone(),
                    worker: event.by.clone(),
                    until_ms,
                };
                if let Err(illegal) = task.enter(claimed, now) {
                    self.refused = Some(illegal);
                    return;
                }
                if !mutated(Mutation::AttemptsNotCounted) {
                    task.attempts += 1;
                }
                task.cap.clone_from(cap);
                Change::Claimed {
                    claim,
                    worker: event.by.clone(),
                    until_ms,
                    took_over,
                }
            }
            Op::Renew {
                claim, lease_ms, ..
            } => {
                let until_ms = now.saturating_add(*lease_ms);
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let Phase::Claimed { worker, .. } = &task.phase else {
                    return;
                };
                let renewed = Phase::Claimed {
                    claim: claim.clone(),
                    worker: worker.clone(),
                    until_ms,
                };
                if let Err(illegal) = task.enter(renewed, now) {
                    self.refused = Some(illegal);
                    return;
                }
                Change::Renewed {
                    claim: claim.clone(),
                    until_ms,
                }
            }
            Op::Release { claim, note, .. } => {
                if !self.enter(&id, Phase::Open, None, now) {
                    return;
                }
                Change::Released {
                    claim: claim.clone(),
                    note: note.clone(),
                }
            }
            Op::Finish { outcome, .. } => {
                if let Ok(Some(due_at_ms)) = self.scheduled(&id, now, outcome)
                    && let Outcome::Failed(error) = outcome
                {
                    if !self.enter(&id, Phase::Open, Some(Some(due_at_ms)), now) {
                        return;
                    }
                    self.record(
                        seq,
                        event,
                        &id,
                        Change::Tasks(Moved::Retrying {
                            error: error.clone(),
                            due_at_ms,
                        }),
                    );
                    return;
                }
                let worker = match self.tasks.get(&id).map(|task| &task.phase) {
                    Some(Phase::Claimed { worker, .. })
                        if !mutated(Mutation::FinishForgetsWorker) =>
                    {
                        Some(worker.clone())
                    }
                    _ => None,
                };
                let finished = match outcome {
                    Outcome::Done(result) => Phase::Done {
                        result: result.clone(),
                        worker,
                    },
                    Outcome::Failed(error) => Phase::Failed {
                        error: error.clone(),
                        worker,
                    },
                };
                if !self.enter(&id, finished, None, now) {
                    return;
                }
                self.record(seq, event, &id, Change::Finished(outcome.clone()));
                if matches!(outcome, Outcome::Failed(_)) && self.cascades(&id) {
                    self.cascade(seq, event, &id);
                }
                self.recur(seq, event, &id);
                return;
            }
            Op::Cancel { reason, .. } => {
                self.cancel(seq, event, &id, reason.clone());
                return;
            }
            Op::Reply { body, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.replies.push((event.by.clone(), body.clone()));
                }
                Change::Replied {
                    by: event.by.clone(),
                    body: body.clone(),
                }
            }
            Op::Relabel { set, unset, .. } => {
                if !mutated(Mutation::LabelIndexStale) {
                    self.unlabel(&id, unset.iter().chain(set.keys()).cloned());
                }
                if let Some(task) = self.tasks.get_mut(&id) {
                    let labels = &mut task.spec.labels;
                    if !mutated(Mutation::RelabelKeepsUnset) {
                        labels.retain(|key, _| !unset.contains(key));
                    }
                    labels.extend(set.clone());
                }
                Change::Relabeled {
                    set: set.clone(),
                    unset: unset.clone(),
                }
            }
            Op::Rank { rank, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.rank = *rank;
                }
                Change::Ranked { rank: *rank }
            }
            Op::Move { parent, .. } => {
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let from = std::mem::replace(&mut task.spec.parent, parent.clone());
                if !mutated(Mutation::MoveKeepsChildren) {
                    if let Some(old) = &from
                        && let Some(below) = self.children.get_mut(old)
                    {
                        below.remove(&id);
                        if below.is_empty() {
                            self.children.remove(old);
                        }
                    }
                    if let Some(new) = parent {
                        self.children
                            .entry(new.clone())
                            .or_default()
                            .insert(id.clone());
                    }
                }
                Change::Reparented {
                    from,
                    to: parent.clone(),
                }
            }
            Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::Epoch { .. }
            | Op::Charter {}
            | Op::TextLimit { .. }
            | Op::Watch(_) => return,
            Op::Tasks(op) => {
                self.steer(seq, event, op);
                return;
            }
            Op::Sync { .. } | Op::Say { .. } | Op::Steer(_) => return,
        };
        self.record(seq, event, &id, change);
    }

    pub(crate) fn cancel(&mut self, seq: u64, event: &Event, id: &TaskId, reason: String) {
        if self.tasks.get(id).is_none_or(|task| task.finished()) {
            return;
        }
        let cancelled = Phase::Cancelled {
            reason: reason.clone(),
        };
        if !self.enter(id, cancelled, None, self.now_ms) {
            return;
        }
        self.drop_steers(seq, event, id);
        self.record(seq, event, id, Change::Cancelled { reason });
        self.cascade(seq, event, id);
    }

    fn cascade(&mut self, seq: u64, event: &Event, id: &TaskId) {
        let below = self
            .children
            .get(id)
            .into_iter()
            .chain(self.dependents.get(id))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        for next in below {
            self.cancel(seq, event, &next, format!("{id} did not finish"));
        }
    }
}

pub enum Step {
    Append { seq: u64, body: String },
    Done(Applied),
}

enum Attempt {
    Fresh,
    Appended {
        seq: u64,
        result: Result<(), StoreError>,
    },
}

pub struct Pending {
    event: Event,
    body: String,
    attempt: Attempt,
}

impl Fold {
    pub fn submit(&self, event: Event) -> Result<Pending, BoardError> {
        let body = serde_json::to_string(&event).map_err(|error| BoardError::Corrupt {
            seq: self.next,
            detail: error.to_string(),
        })?;
        Ok(Pending {
            event,
            body,
            attempt: Attempt::Fresh,
        })
    }
}

pub fn row_fits(scope: &str, body: &str, max: Option<u64>) -> Result<(), BoardError> {
    let Some(max) = max else {
        return Ok(());
    };
    let bytes = (scope.len() + body.len()) as u64;
    assert!(bytes >= body.len() as u64);
    if bytes > max && !mutated(Mutation::RowBoundIgnored) {
        return Err(BoardError::Row { bytes, max });
    }
    Ok(())
}

impl Pending {
    pub const fn event(&self) -> &Event {
        &self.event
    }

    pub fn fits(&self, scope: &str, max: Option<u64>) -> Result<(), BoardError> {
        row_fits(scope, &self.body, max)
    }

    pub fn read(
        &mut self,
        fold: &mut Fold,
        rows: Result<Vec<(u64, String)>, StoreError>,
    ) -> Result<Step, BoardError> {
        let attempt = std::mem::replace(&mut self.attempt, Attempt::Fresh);
        let rows = match (rows, &attempt) {
            (Ok(rows), _) => rows,
            (
                Err(_),
                Attempt::Appended {
                    result: Err(StoreError::Fenced),
                    ..
                },
            ) if !mutated(Mutation::FencedUnsettled) => {
                return Err(BoardError::Store(StoreError::Fenced));
            }
            (Err(error), Attempt::Appended { .. }) => {
                return Err(BoardError::Unsettled {
                    event: self.event.id.clone(),
                    error,
                });
            }
            (Err(error), _) => return Err(BoardError::Store(error)),
        };
        for (seq, body) in rows {
            fold.apply(seq, &body)?;
        }
        if let Some(verdict) = fold.outcome(&self.event.id) {
            return verdict
                .clone()
                .map(|seq| {
                    Step::Done(Applied {
                        seq,
                        event: self.event.id.clone(),
                    })
                })
                .map_err(BoardError::Rejected);
        }
        if let Attempt::Appended { seq, result } = attempt {
            match result {
                Ok(()) => {
                    return Err(BoardError::Corrupt {
                        seq,
                        detail: format!(
                            "the store acknowledged {} but holds another row",
                            self.event.id
                        ),
                    });
                }
                Err(StoreError::Fenced)
                    if fold.next <= seq && !mutated(Mutation::FencedUnsettled) =>
                {
                    return Err(BoardError::Store(StoreError::Fenced));
                }
                Err(error) if fold.next <= seq => {
                    return Err(BoardError::Unsettled {
                        event: self.event.id.clone(),
                        error,
                    });
                }
                Err(_) => {}
            }
        }
        if fold.repost(&self.event).is_none() {
            fold.judge(&self.event).map_err(BoardError::Rejected)?;
        }
        Ok(Step::Append {
            seq: fold.next,
            body: self.body.clone(),
        })
    }

    pub fn optimistic(&self, fold: &Fold) -> Option<Step> {
        if fold.outcome(&self.event.id).is_some() {
            return None;
        }
        if fold.repost(&self.event).is_none() {
            fold.judge(&self.event).ok()?;
        }
        Some(Step::Append {
            seq: fold.next,
            body: self.body.clone(),
        })
    }

    pub fn acknowledged(
        &mut self,
        fold: &mut Fold,
        seq: u64,
        result: Result<(), StoreError>,
    ) -> Result<Option<Applied>, BoardError> {
        if result.is_err() {
            self.appended(seq, result)?;
            return Ok(None);
        }
        if !mutated(Mutation::AckSkipsFold) {
            fold.apply(seq, &self.body)?;
        }
        let Some(verdict) = fold.outcome(&self.event.id) else {
            if mutated(Mutation::AckSkipsFold) {
                return Ok(Some(Applied {
                    seq,
                    event: self.event.id.clone(),
                }));
            }
            return Err(BoardError::Corrupt {
                seq,
                detail: format!(
                    "the store acknowledged {} but it did not fold",
                    self.event.id
                ),
            });
        };
        verdict
            .clone()
            .map(|seq| {
                Some(Applied {
                    seq,
                    event: self.event.id.clone(),
                })
            })
            .map_err(BoardError::Rejected)
    }

    pub fn appended(&mut self, seq: u64, result: Result<(), StoreError>) -> Result<(), BoardError> {
        if let Err(error) = &result
            && mutated(Mutation::ErrorsLoseWrites)
        {
            return Err(BoardError::Store(error.clone()));
        }
        self.attempt = Attempt::Appended { seq, result };
        Ok(())
    }
}

mod pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &BTreeMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Vec::<(K, V)>::deserialize(deserializer).map(|pairs| pairs.into_iter().collect())
    }
}

#[cfg(test)]
mod renew_cap_tests {
    use std::collections::BTreeMap;

    use crate::access::{Can, Grant, Who};
    use crate::tasks::Terms;
    use crate::{Cap, ClaimId, Event, EventId, Fold, NewTask, Op, Rejection, TaskId};

    struct Board {
        fold: Fold,
        minted: u64,
    }

    impl Board {
        fn write(&mut self, by: &str, at_ms: u64, op: Op) -> (EventId, Result<u64, Rejection>) {
            self.minted += 1;
            let id = EventId(format!("{by}.{}", self.minted));
            let body = serde_json::to_string(&Event {
                id: id.clone(),
                by: by.to_owned(),
                at_ms,
                op,
            })
            .expect("an event encodes");
            self.fold
                .apply(self.fold.next(), &body)
                .expect("the row applies");
            let outcome = self.fold.outcome(&id).cloned().expect("an outcome");
            (id, outcome)
        }
    }

    fn pooled(id: &str) -> Op {
        Op::Post {
            task: NewTask {
                id: TaskId(id.to_owned()),
                parent: None,
                spec: id.to_owned(),
                labels: BTreeMap::from([("pool".to_owned(), "lent".to_owned())]),
                needs: Vec::new(),
                terms: Terms::default(),
            },
        }
    }

    /// A claim that lapsed while another worker filled the cap stays out on renew.
    #[test]
    fn renewing_a_lapsed_claim_respects_its_cap() {
        for cap in [
            Cap::Partition(1),
            Cap::Label {
                max: 1,
                key: "pool".to_owned(),
                value: "lent".to_owned(),
            },
        ] {
            let mut board = Board {
                fold: Fold::default(),
                minted: 0,
            };
            assert!(board.write("user:lead", 0, Op::Charter {}).1.is_ok());
            let grant = Grant {
                who: vec![Who::Everyone],
                can: BTreeMap::from([("*".to_owned(), Can::ALL.to_vec())]),
            };
            assert!(board.write("user:lead", 0, Op::Grant { grant }).1.is_ok());
            assert!(board.write("user:lead", 0, pooled("a")).1.is_ok());
            assert!(board.write("user:lead", 0, pooled("b")).1.is_ok());
            let claim = |task: &str| Op::Claim {
                task: TaskId(task.to_owned()),
                lease_ms: 100,
                fence: None,
                cap: Some(cap.clone()),
            };
            let (first, verdict) = board.write("agent:w0", 10, claim("a"));
            assert!(verdict.is_ok(), "{verdict:?}");
            let (_, verdict) = board.write("agent:w1", 50, claim("b"));
            assert_eq!(verdict, Err(Rejection::Full { cap: cap.clone() }));
            let (_, verdict) = board.write("agent:w1", 200, claim("b"));
            assert!(verdict.is_ok(), "the first claim lapsed: {verdict:?}");
            let renew = Op::Renew {
                task: TaskId("a".to_owned()),
                claim: ClaimId(first.0),
                lease_ms: 100,
            };
            let (_, verdict) = board.write("agent:w0", 210, renew);
            assert_eq!(verdict, Err(Rejection::Full { cap: cap.clone() }));
            assert_eq!(board.fold.held(&cap, 210), 1);
        }
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use nanocode_durability_core::StoreError;
use nanocode_turnlog::LogHost;
use serde::{Deserialize, Serialize};

pub mod access;
pub mod sim;

use sim::{Mutation, mutated};

const FIRST: u64 = 1;
const END: u64 = i64::MAX as u64;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTask {
    pub id: TaskId,
    pub parent: Option<TaskId>,
    pub spec: String,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<TaskId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Done(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Post { task: NewTask },
    Claim { task: TaskId, lease_ms: u64 },
    Renew { task: TaskId, claim: ClaimId, lease_ms: u64 },
    Release { task: TaskId, claim: ClaimId, note: String },
    Finish { task: TaskId, claim: ClaimId, outcome: Outcome },
    Cancel { task: TaskId, reason: String },
    Reply { task: TaskId, body: String },
    Grant { grant: access::Grant },
    Revoke { grant: EventId },
    Charter {},
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
            | Self::Reply { task, .. } => Some(task),
            Self::Grant { .. } | Self::Revoke { .. } | Self::Charter {} => None,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Open,
    Claimed {
        claim: ClaimId,
        worker: String,
        until_ms: u64,
    },
    Done {
        result: String,
    },
    Failed {
        error: String,
    },
    Cancelled {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub spec: NewTask,
    pub phase: Phase,
    pub attempts: u64,
    pub replies: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl Change {
    pub const fn terminal(&self) -> bool {
        matches!(self, Self::Finished(_) | Self::Cancelled { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "board store: {error}"),
            Self::Corrupt { seq, detail } => write!(formatter, "board row {seq}: {detail}"),
            Self::Rejected(rejection) => write!(formatter, "board rejected the write: {rejection:?}"),
            Self::Unsettled { event, error } => {
                write!(formatter, "board write {event} is unsettled: {error}")
            }
        }
    }
}

impl From<StoreError> for BoardError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    next: u64,
    now_ms: u64,
    tasks: BTreeMap<TaskId, Task>,
    children: BTreeMap<TaskId, BTreeSet<TaskId>>,
    dependents: BTreeMap<TaskId, BTreeSet<TaskId>>,
    seen: BTreeMap<EventId, Result<u64, Rejection>>,
    entries: Vec<Entry>,
    access: access::Access,
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
        }
    }

    pub const fn next(&self) -> u64 {
        self.next
    }

    pub const fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub const fn tasks(&self) -> &BTreeMap<TaskId, Task> {
        &self.tasks
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn outcome(&self, event: &EventId) -> Option<&Result<u64, Rejection>> {
        self.seen.get(event)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn claimable(&self, id: &TaskId, at_ms: u64) -> Result<Option<ClaimId>, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        let now = self.now_ms.max(at_ms);
        match &task.phase {
            Phase::Open => {}
            Phase::Claimed {
                claim, until_ms, ..
            } if (*until_ms > now || mutated(Mutation::LeasesNeverExpire))
                && !mutated(Mutation::ClaimIgnoresLease) =>
            {
                return Err(Rejection::Held {
                    claim: claim.clone(),
                    until_ms: *until_ms,
                });
            }
            Phase::Claimed { claim, .. } => return Ok(Some(claim.clone())),
            _ => return Err(Rejection::Terminal),
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
        self.tasks
            .keys()
            .filter(move |id| self.claimable(id, at_ms).is_ok())
    }

    pub fn settled(&self) -> bool {
        self.tasks.values().all(|task| task.phase.terminal())
    }

    fn holder(&self, id: &TaskId, held: &ClaimId) -> Result<&Task, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        match &task.phase {
            Phase::Claimed { claim, .. } if claim == held => Ok(task),
            Phase::Claimed { .. } | Phase::Open => Err(Rejection::NotHolder),
            _ => Err(Rejection::Terminal),
        }
    }

    pub fn check(&self, at_ms: u64, op: &Op) -> Result<(), Rejection> {
        match op {
            Op::Post { task } => {
                if self.tasks.contains_key(&task.id) && !mutated(Mutation::PostOverwrites) {
                    return Err(Rejection::Exists);
                }
                if let Some(parent) = &task.parent
                    && !self.tasks.contains_key(parent)
                {
                    return Err(Rejection::UnknownParent(parent.clone()));
                }
                match task.needs.iter().find(|need| !self.tasks.contains_key(*need)) {
                    Some(need) => Err(Rejection::UnknownNeed(need.clone())),
                    None => Ok(()),
                }
            }
            Op::Claim { task, .. } => self.claimable(task, at_ms).map(drop),
            Op::Finish { task, .. } if mutated(Mutation::FinishIgnoresClaim) => {
                match self.tasks.get(task).map(|task| &task.phase) {
                    None => Err(Rejection::UnknownTask),
                    Some(phase) if phase.terminal() => Err(Rejection::Terminal),
                    Some(Phase::Open) => Err(Rejection::NotHolder),
                    Some(_) => Ok(()),
                }
            }
            Op::Renew { task, claim, .. }
            | Op::Release { task, claim, .. }
            | Op::Finish { task, claim, .. } => self.holder(task, claim).map(drop),
            Op::Cancel { task, .. } => match self.tasks.get(task) {
                None => Err(Rejection::UnknownTask),
                Some(task) if task.phase.terminal() => Err(Rejection::Terminal),
                Some(_) => Ok(()),
            },
            Op::Reply { task, .. } => self
                .tasks
                .get(task)
                .map(drop)
                .ok_or(Rejection::UnknownTask),
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} => Ok(()),
        }
    }

    pub fn apply(&mut self, seq: u64, body: &str) -> Result<(), BoardError> {
        if seq != self.next {
            return Err(BoardError::Corrupt {
                seq,
                detail: format!("expected row {}", self.next),
            });
        }
        let event: Event = serde_json::from_str(body).map_err(|error| BoardError::Corrupt {
            seq,
            detail: error.to_string(),
        })?;
        self.next = seq + 1;
        if self.seen.contains_key(&event.id) || !access::owns(&event.by, &event.id) {
            return Ok(());
        }
        self.now_ms = self.now_ms.max(event.at_ms);
        let verdict = self.admit(&event.by, event.at_ms, &event.op).map(|()| seq);
        self.seen.insert(event.id.clone(), verdict.clone());
        if verdict.is_ok() {
            self.access.record(&event.by, &event.id, &event.op);
            self.transition(seq, &event);
        }
        Ok(())
    }

    fn record(&mut self, seq: u64, event: &Event, task: &TaskId, change: Change) {
        self.entries.push(Entry {
            seq,
            event: event.id.clone(),
            at_ms: self.now_ms,
            task: task.clone(),
            change,
        });
    }

    fn transition(&mut self, seq: u64, event: &Event) {
        let now = self.now_ms;
        let Some(id) = event.op.task().cloned() else {
            return;
        };
        let change = match &event.op {
            Op::Post { task } => {
                if let Some(parent) = &task.parent {
                    self.children
                        .entry(parent.clone())
                        .or_default()
                        .insert(task.id.clone());
                }
                for need in &task.needs {
                    self.dependents
                        .entry(need.clone())
                        .or_default()
                        .insert(task.id.clone());
                }
                self.tasks.insert(
                    task.id.clone(),
                    Task {
                        spec: task.clone(),
                        phase: Phase::Open,
                        attempts: 0,
                        replies: Vec::new(),
                    },
                );
                self.record(seq, event, &id, Change::Posted);
                let doomed = task
                    .parent
                    .iter()
                    .chain(&task.needs)
                    .find(|above| {
                        matches!(
                            self.tasks.get(*above).map(|above| &above.phase),
                            Some(Phase::Failed { .. } | Phase::Cancelled { .. })
                        )
                    })
                    .cloned();
                if let Some(above) = doomed
                    && !mutated(Mutation::PostIgnoresDoomedNeeds)
                {
                    self.cancel(seq, event, &id, format!("{above} did not finish"));
                }
                return;
            }
            Op::Claim { lease_ms, .. } => {
                let until_ms = now.saturating_add(*lease_ms);
                let claim = ClaimId(event.id.0.clone());
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let took_over = match &task.phase {
                    Phase::Claimed { claim, .. } => Some(claim.clone()),
                    _ => None,
                };
                task.attempts += 1;
                task.phase = Phase::Claimed {
                    claim: claim.clone(),
                    worker: event.by.clone(),
                    until_ms,
                };
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
                if let Some(Task {
                    phase: Phase::Claimed { until_ms: until, .. },
                    ..
                }) = self.tasks.get_mut(&id)
                {
                    *until = until_ms;
                }
                Change::Renewed {
                    claim: claim.clone(),
                    until_ms,
                }
            }
            Op::Release { claim, note, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.phase = Phase::Open;
                }
                Change::Released {
                    claim: claim.clone(),
                    note: note.clone(),
                }
            }
            Op::Finish { outcome, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.phase = match outcome {
                        Outcome::Done(result) => Phase::Done {
                            result: result.clone(),
                        },
                        Outcome::Failed(error) => Phase::Failed {
                            error: error.clone(),
                        },
                    };
                }
                self.record(seq, event, &id, Change::Finished(outcome.clone()));
                if matches!(outcome, Outcome::Failed(_)) {
                    self.cascade(seq, event, &id);
                }
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
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} => return,
        };
        self.record(seq, event, &id, change);
    }

    fn cancel(&mut self, seq: u64, event: &Event, id: &TaskId, reason: String) {
        let Some(task) = self.tasks.get_mut(id) else {
            return;
        };
        if task.phase.terminal() {
            return;
        }
        task.phase = Phase::Cancelled {
            reason: reason.clone(),
        };
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

pub struct Board {
    host: Arc<dyn LogHost>,
    scope: String,
    writer: String,
    identity: access::Identity,
    minted: u64,
    fold: Fold,
}

impl Board {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
    ) -> Result<Self, BoardError> {
        let rows = host.load(scope).await?;
        let mut board = Self {
            host,
            scope: scope.to_owned(),
            writer: identity.actor().to_string(),
            identity,
            minted: 0,
            fold: Fold::new(),
        };
        board.fold_rows(rows)?;
        Ok(board)
    }

    pub const fn fold(&self) -> &Fold {
        &self.fold
    }

    pub fn writer(&self) -> &str {
        &self.writer
    }

    fn fold_rows(&mut self, rows: Vec<(u64, String)>) -> Result<(), BoardError> {
        for (seq, body) in rows {
            self.fold.apply(seq, &body)?;
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<(), BoardError> {
        let rows = self.host.scan(&self.scope, self.fold.next, END).await?;
        self.fold_rows(rows)
    }

    pub async fn submit(&mut self, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        self.minted += 1;
        let id = EventId(format!("{}.{}", self.writer, self.minted));
        self.submit_as(id, at_ms, op).await
    }

    pub async fn submit_keyed(&mut self, key: &str, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        if mutated(Mutation::KeysIgnored) {
            return self.submit(at_ms, op).await;
        }
        self.submit_as(EventId(format!("{}#{key}", self.writer)), at_ms, op).await
    }

    async fn submit_as(&mut self, id: EventId, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        let event = Event {
            id,
            by: self.writer.clone(),
            at_ms,
            op,
        };
        let body = serde_json::to_string(&event).map_err(|error| BoardError::Corrupt {
            seq: self.fold.next,
            detail: error.to_string(),
        })?;
        loop {
            self.refresh().await?;
            if let Some(verdict) = self.fold.outcome(&event.id) {
                return self.settled(&event.id, verdict.clone());
            }
            self.fold.admit(&event.by, at_ms, &event.op).map_err(BoardError::Rejected)?;
            let seq = self.fold.next;
            let appended = self.host.append(&self.scope, &[(seq, body.clone())]).await;
            if let Err(error) = &appended
                && mutated(Mutation::ErrorsLoseWrites)
            {
                return Err(BoardError::Store(error.clone()));
            }
            match self.refresh().await {
                Ok(()) => {}
                Err(BoardError::Store(error)) => {
                    return Err(BoardError::Unsettled {
                        event: event.id,
                        error,
                    });
                }
                Err(error) => return Err(error),
            }
            if let Some(verdict) = self.fold.outcome(&event.id) {
                return self.settled(&event.id, verdict.clone());
            }
            match appended {
                Ok(()) => {
                    return Err(BoardError::Corrupt {
                        seq,
                        detail: format!("the store acknowledged {} but holds another row", event.id),
                    });
                }
                Err(error) if self.fold.next <= seq => {
                    return Err(BoardError::Unsettled {
                        event: event.id,
                        error,
                    });
                }
                Err(_) => {}
            }
        }
    }

    fn settled(
        &self,
        event: &EventId,
        verdict: Result<u64, Rejection>,
    ) -> Result<Applied, BoardError> {
        verdict
            .map(|seq| Applied {
                seq,
                event: event.clone(),
            })
            .map_err(BoardError::Rejected)
    }
}

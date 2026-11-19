use std::{
    collections::BTreeMap,
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
};

use serde::{Deserialize, Serialize};

use crate::{
    Applied, Board, BoardError, Change, ClaimId, Event, Fold, NewTask, Op, Outcome, Phase,
    Rejection, Task, TaskId,
    access::{Actor, Can},
    sim::{Mutation, mutated},
};

pub const BATCH: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFail {
    #[default]
    Cascade,
    Wait,
}

impl OnFail {
    pub fn is_cascade(&self) -> bool {
        *self == Self::Cascade
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "OnFail::is_cascade")]
    pub on_fail: OnFail,
}

impl Terms {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    Task(TaskId),
    Key(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planned {
    pub key: String,
    pub parent: Option<Need>,
    pub spec: String,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<Need>,
    #[serde(default, skip_serializing_if = "Terms::is_default")]
    pub terms: Terms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "act", rename_all = "snake_case")]
pub enum TaskOp {
    Block {
        task: TaskId,
        claim: Option<ClaimId>,
        kind: String,
        reason: String,
    },
    Unblock {
        task: TaskId,
    },
    Reopen {
        task: TaskId,
    },
    Assign {
        task: TaskId,
        to: Option<String>,
    },
    Batch {
        name: TaskId,
        tasks: Vec<Planned>,
    },
}

impl TaskOp {
    pub const fn task(&self) -> &TaskId {
        match self {
            Self::Block { task, .. }
            | Self::Unblock { task }
            | Self::Reopen { task }
            | Self::Assign { task, .. } => task,
            Self::Batch { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Assigned(String),
    Blocked,
    NotBlocked,
    NotFailed,
    EmptyBatch,
    TooLarge(usize),
    DuplicateKey(String),
    UnknownKey(String),
    Cycle(String),
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Moved {
    Blocked {
        kind: String,
        reason: String,
        ended: Option<ClaimId>,
    },
    Unblocked,
    Reopened,
    Assigned {
        to: Option<String>,
    },
}

pub fn acts_for(by: &str, assignee: &str) -> bool {
    Actor::parse(by).is_ok_and(|by| Actor::parse(assignee).is_ok_and(|to| to.covers(&by)))
}

pub fn member(name: &TaskId, key: &str) -> TaskId {
    TaskId(format!("{name}/{key}"))
}

pub fn resolve(name: &TaskId, tasks: &[Planned]) -> Vec<NewTask> {
    let id = |need: &Need| match need {
        Need::Task(task) => task.clone(),
        Need::Key(key) => member(name, key),
    };
    tasks
        .iter()
        .map(|planned| NewTask {
            id: member(name, &planned.key),
            parent: planned.parent.as_ref().map(id),
            spec: planned.spec.clone(),
            labels: planned.labels.clone(),
            needs: planned.needs.iter().map(id).collect(),
            terms: planned.terms.clone(),
        })
        .collect()
}

fn cyclic(at: usize, edges: &[Vec<usize>], marks: &mut [u8]) -> bool {
    match marks[at] {
        1 => return true,
        2 => return false,
        _ => {}
    }
    marks[at] = 1;
    if edges[at].iter().any(|next| cyclic(*next, edges, marks)) {
        return true;
    }
    marks[at] = 2;
    false
}

impl Task {
    pub fn finished(&self) -> bool {
        match &self.phase {
            Phase::Failed { .. } => self.spec.terms.on_fail == OnFail::Cascade,
            phase => phase.terminal(),
        }
    }

    fn cascades(&self) -> bool {
        self.spec.terms.on_fail == OnFail::Cascade || mutated(Mutation::FailIgnoresPolicy)
    }
}

impl Fold {
    pub(crate) fn dooms(&self, above: &TaskId) -> bool {
        self.tasks.get(above).is_some_and(|task| match task.phase {
            Phase::Cancelled { .. } => true,
            Phase::Failed { .. } => task.cascades(),
            _ => false,
        })
    }

    pub(crate) fn cascades(&self, id: &TaskId) -> bool {
        self.tasks.get(id).is_some_and(Task::cascades)
    }

    pub fn admits(&self, task: &TaskId, actor: &str) -> Result<(), Rejection> {
        match self.tasks.get(task).and_then(|task| task.assignee.as_ref()) {
            Some(to) if !acts_for(actor, to) && !mutated(Mutation::ClaimIgnoresAssignee) => {
                Err(Rejection::Tasks(Refusal::Assigned(to.clone())))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn assigned(&self, by: &str, op: &Op) -> Result<(), Rejection> {
        match op {
            Op::Claim { task, .. } => self.admits(task, by),
            _ => Ok(()),
        }
    }

    pub fn until(&self, task: &TaskId, claim: &ClaimId) -> Option<u64> {
        match self.tasks.get(task).map(|task| &task.phase) {
            Some(Phase::Claimed {
                claim: held,
                until_ms,
                ..
            }) if held == claim => Some(*until_ms),
            _ => None,
        }
    }

    pub fn pick(
        &self,
        actor: &str,
        labels: &BTreeMap<String, String>,
        at_ms: u64,
    ) -> Option<&TaskId> {
        let me = Actor::parse(actor).ok()?;
        let mut unassigned = None;
        for entry in &self.entries {
            if entry.change != Change::Posted {
                continue;
            }
            let Some(task) = self.tasks.get(&entry.task) else {
                continue;
            };
            if !labels
                .iter()
                .all(|(key, value)| task.spec.labels.get(key) == Some(value))
                || self.claimable(&entry.task, at_ms).is_err()
                || self.admits(&entry.task, actor).is_err()
                || !self.access.may(&me, &entry.task.0, Can::Label)
            {
                continue;
            }
            if task.assignee.is_some() || mutated(Mutation::NextIgnoresAssignee) {
                return Some(&entry.task);
            }
            unassigned.get_or_insert(&entry.task);
        }
        unassigned
    }

    pub(crate) fn check_tasks(&self, at_ms: u64, op: &TaskOp) -> Result<(), Rejection> {
        if let TaskOp::Batch { name, tasks } = op {
            return self.check_batch(name, tasks);
        }
        let task = self.tasks.get(op.task()).ok_or(Rejection::UnknownTask)?;
        match op {
            TaskOp::Block { claim, .. } => match (&task.phase, claim) {
                (Phase::Claimed { claim: held, .. }, Some(claim)) if held == claim => Ok(()),
                (Phase::Open | Phase::Claimed { .. }, Some(_)) => Err(Rejection::NotHolder),
                (
                    Phase::Claimed {
                        claim, until_ms, ..
                    },
                    None,
                ) if *until_ms > self.now_ms.max(at_ms) => Err(Rejection::Held {
                    claim: claim.clone(),
                    until_ms: *until_ms,
                }),
                (Phase::Open | Phase::Claimed { .. }, None) => Ok(()),
                (Phase::Blocked { .. }, _) => Err(Rejection::Tasks(Refusal::Blocked)),
                _ => Err(Rejection::Terminal),
            },
            TaskOp::Unblock { .. } => match task.phase {
                Phase::Blocked { .. } => Ok(()),
                _ if mutated(Mutation::UnblockAnything) => Ok(()),
                _ => Err(Rejection::Tasks(Refusal::NotBlocked)),
            },
            TaskOp::Reopen { .. } => match task.phase {
                Phase::Failed { .. } if !task.finished() => Ok(()),
                _ if mutated(Mutation::ReopenAnything) => Ok(()),
                Phase::Failed { .. } => Err(Rejection::Terminal),
                _ => Err(Rejection::Tasks(Refusal::NotFailed)),
            },
            TaskOp::Assign { .. } if task.finished() => Err(Rejection::Terminal),
            TaskOp::Assign { .. } | TaskOp::Batch { .. } => Ok(()),
        }
    }

    fn check_batch(&self, name: &TaskId, tasks: &[Planned]) -> Result<(), Rejection> {
        if tasks.is_empty() {
            return Err(Rejection::Tasks(Refusal::EmptyBatch));
        }
        if tasks.len() > BATCH {
            return Err(Rejection::Tasks(Refusal::TooLarge(tasks.len())));
        }
        let mut keys = BTreeMap::new();
        for (index, planned) in tasks.iter().enumerate() {
            if keys.insert(planned.key.as_str(), index).is_some() {
                return Err(Rejection::Tasks(Refusal::DuplicateKey(planned.key.clone())));
            }
            if self.tasks.contains_key(&member(name, &planned.key)) {
                return Err(Rejection::Exists);
            }
        }
        let mut edges = vec![Vec::new(); tasks.len()];
        for (index, planned) in tasks.iter().enumerate() {
            match &planned.parent {
                Some(Need::Task(parent)) if !self.tasks.contains_key(parent) => {
                    return Err(Rejection::UnknownParent(parent.clone()));
                }
                Some(Need::Key(key)) if keys.get(key.as_str()).is_none_or(|at| *at >= index) => {
                    return Err(Rejection::Tasks(Refusal::UnknownKey(key.clone())));
                }
                _ => {}
            }
            for need in &planned.needs {
                match need {
                    Need::Task(task) if !self.tasks.contains_key(task) => {
                        return Err(Rejection::UnknownNeed(task.clone()));
                    }
                    Need::Task(_) => {}
                    Need::Key(key) => match keys.get(key.as_str()) {
                        Some(at) => edges[index].push(*at),
                        None => return Err(Rejection::Tasks(Refusal::UnknownKey(key.clone()))),
                    },
                }
            }
        }
        if mutated(Mutation::BatchIgnoresCycles) {
            return Ok(());
        }
        let mut marks = vec![0; tasks.len()];
        match (0..tasks.len()).find(|at| cyclic(*at, &edges, &mut marks)) {
            Some(at) => Err(Rejection::Tasks(Refusal::Cycle(tasks[at].key.clone()))),
            None => Ok(()),
        }
    }

    fn place(&mut self, seq: u64, event: &Event, task: &NewTask) {
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
                assignee: task.terms.assignee.clone(),
            },
        );
        self.record(seq, event, &task.id, Change::Posted);
    }

    pub(crate) fn steer(&mut self, seq: u64, event: &Event, op: &TaskOp) {
        let id = op.task().clone();
        let change = match op {
            TaskOp::Batch { name, tasks } => {
                assert!(tasks.len() <= BATCH, "a checked batch fits one write");
                let placed = resolve(name, tasks);
                for task in &placed {
                    self.place(seq, event, task);
                }
                for task in &placed {
                    let doomed = task
                        .parent
                        .iter()
                        .chain(&task.needs)
                        .find(|above| self.dooms(above))
                        .cloned();
                    if let Some(above) = doomed
                        && !mutated(Mutation::PostIgnoresDoomedNeeds)
                    {
                        self.cancel(seq, event, &task.id, format!("{above} did not finish"));
                    }
                }
                return;
            }
            TaskOp::Block { kind, reason, .. } => {
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let ended = match &task.phase {
                    Phase::Claimed { claim, .. } => Some(claim.clone()),
                    _ => None,
                };
                task.phase = Phase::Blocked {
                    kind: kind.clone(),
                    reason: reason.clone(),
                };
                Moved::Blocked {
                    kind: kind.clone(),
                    reason: reason.clone(),
                    ended,
                }
            }
            TaskOp::Unblock { .. } | TaskOp::Reopen { .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.phase = Phase::Open;
                }
                if matches!(op, TaskOp::Reopen { .. }) {
                    Moved::Reopened
                } else {
                    Moved::Unblocked
                }
            }
            TaskOp::Assign { to, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.assignee.clone_from(to);
                }
                Moved::Assigned { to: to.clone() }
            }
        };
        self.record(seq, event, &id, Change::Tasks(change));
    }
}

pub trait Pace {
    fn now_ms(&self) -> u64;
    fn sleep(&self, ms: u64) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

#[derive(Clone, Debug)]
pub struct Lease {
    pub task: TaskId,
    pub claim: ClaimId,
    until: Arc<AtomicU64>,
}

impl Lease {
    pub fn until_ms(&self) -> u64 {
        self.until.load(Ordering::Acquire)
    }
}

async fn race<A: Future>(
    mut work: Pin<&mut A>,
    mut keep: Pin<&mut impl Future<Output = BoardError>>,
) -> Result<A::Output, BoardError> {
    std::future::poll_fn(|context| {
        if let Poll::Ready(output) = work.as_mut().poll(context) {
            return Poll::Ready(Ok(output));
        }
        keep.as_mut().poll(context).map(Err)
    })
    .await
}

impl Board {
    pub async fn batch(
        &mut self,
        at_ms: u64,
        name: TaskId,
        tasks: Vec<Planned>,
    ) -> Result<Vec<TaskId>, BoardError> {
        let placed = resolve(&name, &tasks);
        let key = format!("batch/{name}");
        self.submit_keyed(&key, at_ms, Op::Tasks(TaskOp::Batch { name, tasks }))
            .await?;
        if placed
            .iter()
            .any(|task| self.fold.task(&task.id).is_none_or(|landed| landed.spec != *task))
        {
            return Err(BoardError::Rejected(Rejection::Tasks(Refusal::Duplicate)));
        }
        Ok(placed.into_iter().map(|task| task.id).collect())
    }

    pub async fn run<W, F>(
        &mut self,
        task: &TaskId,
        lease_ms: u64,
        pace: &impl Pace,
        work: W,
    ) -> Result<Applied, BoardError>
    where
        W: FnOnce(Lease) -> F,
        F: Future<Output = Outcome>,
    {
        assert!(lease_ms >= 3, "a lease renews at a third of itself");
        let claimed = self
            .submit(
                pace.now_ms(),
                Op::Claim {
                    task: task.clone(),
                    lease_ms,
                },
            )
            .await?;
        let claim = ClaimId(claimed.event.0);
        let until = self
            .fold
            .until(task, &claim)
            .ok_or(BoardError::Rejected(Rejection::NotHolder))?;
        let lease = Lease {
            task: task.clone(),
            claim: claim.clone(),
            until: Arc::new(AtomicU64::new(until)),
        };
        let outcome = {
            let work = pin!(work(lease.clone()));
            let keep = pin!(self.keep(&lease, lease_ms, pace));
            race(work, keep).await?
        };
        self.submit(
            pace.now_ms(),
            Op::Finish {
                task: task.clone(),
                claim,
                outcome,
            },
        )
        .await
    }

    async fn keep(&mut self, lease: &Lease, lease_ms: u64, pace: &impl Pace) -> BoardError {
        let third = lease_ms / 3;
        let mut wake = lease.until_ms().saturating_sub(lease_ms - third);
        loop {
            pace.sleep(wake.saturating_sub(pace.now_ms())).await;
            let renew = Op::Renew {
                task: lease.task.clone(),
                claim: lease.claim.clone(),
                lease_ms,
            };
            match self.submit(pace.now_ms(), renew).await {
                Ok(_) => match self.fold.until(&lease.task, &lease.claim) {
                    Some(until) => {
                        lease.until.store(until, Ordering::Release);
                        wake = until.saturating_sub(lease_ms - third);
                    }
                    None => wake = pace.now_ms() + third,
                },
                Err(BoardError::Rejected(_)) if mutated(Mutation::RunIgnoresLoss) => {
                    lease.until.store(pace.now_ms() + lease_ms, Ordering::Release);
                    wake = pace.now_ms() + third;
                }
                Err(error @ (BoardError::Rejected(_) | BoardError::Corrupt { .. })) => {
                    return error;
                }
                Err(BoardError::Store(_) | BoardError::Unsettled { .. }) => {
                    wake = pace.now_ms() + third;
                }
            }
        }
    }
}

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::{
    Change, ClaimId, Event, EventId, Fold, Rejection, TaskId,
    sim::{Mutation, mutated},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "act", rename_all = "snake_case")]
pub enum SteerOp {
    Send {
        task: TaskId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<ClaimId>,
        body: String,
    },
    Heed {
        task: TaskId,
        claim: ClaimId,
        upto: u64,
    },
}

impl SteerOp {
    pub const fn task(&self) -> &TaskId {
        match self {
            Self::Send { task, .. } | Self::Heed { task, .. } => task,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    pub seq: u64,
    pub by: String,
    pub at_ms: u64,
    pub to: Option<ClaimId>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    Unheeded { upto: u64 },
    NotPending { upto: u64 },
    Text { chars: u64, max: u64 },
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Steered {
    Sent {
        by: String,
        to: Option<ClaimId>,
        body: String,
    },
    Heeded {
        claim: ClaimId,
        upto: u64,
        count: u64,
    },
    Dropped {
        upto: u64,
        count: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steers {
    #[serde(with = "crate::pairs")]
    queues: BTreeMap<TaskId, VecDeque<Steer>>,
}

impl Steers {
    pub(crate) fn holds(&self, task: &TaskId) -> bool {
        self.queues.contains_key(task)
    }

    pub const fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
        }
    }
}

impl Fold {
    pub fn steers(&self, task: &TaskId) -> impl Iterator<Item = &Steer> + '_ {
        self.steers.queues.get(task).into_iter().flatten()
    }

    pub fn steers_for(
        &self,
        task: &TaskId,
        claim: &ClaimId,
    ) -> Result<impl Iterator<Item = &Steer> + '_, Rejection> {
        self.holder(task, claim)?;
        let since = match self.seen.get(&EventId(claim.0.clone())) {
            Some(Ok(seq)) if mutated(Mutation::SteerLostOnMove) => *seq,
            _ => 0,
        };
        Ok(self.steers(task).filter(move |steer| steer.seq > since))
    }

    pub(crate) fn unheeded(&self, task: &TaskId) -> Result<(), Rejection> {
        match self.steers(task).last() {
            Some(steer) if !mutated(Mutation::FinishIgnoresSteers) => {
                Err(Rejection::Steer(Refusal::Unheeded { upto: steer.seq }))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn check_steer(&self, op: &SteerOp) -> Result<(), Rejection> {
        let task = self.tasks.get(op.task()).ok_or(Rejection::UnknownTask)?;
        match op {
            SteerOp::Send { .. } if task.finished() && !mutated(Mutation::SteerToTerminal) => {
                Err(Rejection::Terminal)
            }
            SteerOp::Send {
                task,
                claim: Some(claim),
                body,
            } => {
                if !mutated(Mutation::SteerByClaimFollows) {
                    self.holder(task, claim)?;
                }
                self.steer_text(body)
            }
            SteerOp::Send { body, .. } => self.steer_text(body),
            SteerOp::Heed { task, claim, upto } => {
                if !mutated(Mutation::HeedFromStaleClaim) {
                    self.holder(task, claim)?;
                }
                if self.steers(task).any(|steer| steer.seq == *upto) {
                    Ok(())
                } else {
                    Err(Rejection::Steer(Refusal::NotPending { upto: *upto }))
                }
            }
        }
    }

    pub(crate) fn apply_steer(&mut self, seq: u64, event: &Event, op: &SteerOp) {
        let id = op.task().clone();
        let change = match op {
            SteerOp::Send { claim, body, .. } => {
                self.steers
                    .queues
                    .entry(id.clone())
                    .or_default()
                    .push_back(Steer {
                        seq,
                        by: event.by.clone(),
                        at_ms: self.now_ms,
                        to: claim.clone(),
                        body: body.clone(),
                    });
                Steered::Sent {
                    by: event.by.clone(),
                    to: claim.clone(),
                    body: body.clone(),
                }
            }
            SteerOp::Heed { claim, upto, .. } => {
                let Some(queue) = self.steers.queues.get_mut(&id) else {
                    return;
                };
                let before = queue.len();
                if !mutated(Mutation::SteerRedeliveredAfterHeed) {
                    queue.retain(|steer| steer.seq > *upto);
                }
                let count = (before - queue.len()) as u64;
                if queue.is_empty() {
                    self.steers.queues.remove(&id);
                }
                Steered::Heeded {
                    claim: claim.clone(),
                    upto: *upto,
                    count,
                }
            }
        };
        self.record(seq, event, &id, Change::Steered(change));
    }
}

impl Fold {
    /// Cancelling a task drops the steers no holder heeded and records how many, so
    /// whoever watches it learns they were never acted on instead of waiting forever.
    pub(crate) fn drop_steers(&mut self, seq: u64, event: &Event, id: &TaskId) {
        if mutated(Mutation::SteerKeptOnCancel) {
            return;
        }
        let Some(queue) = self.steers.queues.remove(id) else {
            return;
        };
        let Some(upto) = queue.back().map(|steer| steer.seq) else {
            return;
        };
        let change = Steered::Dropped {
            upto,
            count: queue.len() as u64,
        };
        self.record(seq, event, id, Change::Steered(change));
    }
}

impl Fold {
    fn steer_text(&self, body: &str) -> Result<(), Rejection> {
        if body.is_empty() {
            return Err(Rejection::Steer(Refusal::Empty));
        }
        match self.over_limit(body) {
            Some((chars, max)) => Err(Rejection::Steer(Refusal::Text { chars, max })),
            None => Ok(()),
        }
    }
}

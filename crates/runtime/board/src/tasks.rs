use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
};

pub use nanocode_board_fold::tasks::*;

use crate::{
    Applied, Board, BoardError, ClaimId, NewTask, Op, Outcome, Rejection, TaskId,
    sim::{Mutation, mutated},
    steer::{Refusal as SteerRefusal, Steer},
};

pub const RENEWS_PER_LEASE: u64 = 3;

pub const fn renew_after(lease_ms: u64) -> u64 {
    lease_ms / RENEWS_PER_LEASE
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

pub enum Step {
    More,
    Done(Outcome),
}

pub trait Steps {
    fn step(
        &mut self,
        lease: Lease,
        steers: Vec<Steer>,
    ) -> Pin<Box<dyn Future<Output = Step> + '_>>;
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

fn placed_as(landed: &NewTask, planned: &NewTask) -> bool {
    landed.id == planned.id
        && landed.spec == planned.spec
        && landed.needs == planned.needs
        && landed.terms == planned.terms
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
        if placed.iter().any(|task| {
            self.fold
                .task(&task.id)
                .is_none_or(|landed| !placed_as(&landed.spec, task))
        }) {
            return Err(BoardError::Rejected(Rejection::Tasks(Refusal::Duplicate)));
        }
        Ok(placed.into_iter().map(|task| task.id).collect())
    }

    pub async fn post_due(
        &mut self,
        at_ms: u64,
        task: NewTask,
        terms: Terms,
    ) -> Result<Applied, BoardError> {
        let task = NewTask { terms, ..task };
        let key = format!("post/{}", task.id);
        let applied = self
            .submit_keyed(&key, at_ms, Op::Post { task: task.clone() })
            .await?;
        if self
            .fold
            .task(&task.id)
            .is_none_or(|landed| landed.spec != task)
        {
            return Err(BoardError::Rejected(Rejection::Tasks(Refusal::Duplicate)));
        }
        Ok(applied)
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
        let lease = self.lease(task, lease_ms, pace).await?;
        let outcome = {
            let work = pin!(work(lease.clone()));
            let keep = pin!(self.keep(&lease, lease_ms, pace));
            race(work, keep).await?
        };
        self.submit(
            pace.now_ms(),
            Op::Finish {
                task: task.clone(),
                claim: lease.claim.clone(),
                outcome,
            },
        )
        .await
    }

    async fn lease(
        &mut self,
        task: &TaskId,
        lease_ms: u64,
        pace: &impl Pace,
    ) -> Result<Lease, BoardError> {
        assert!(lease_ms >= 3, "a lease renews at a third of itself");
        let claimed = self
            .submit(
                pace.now_ms(),
                Op::Claim {
                    task: task.clone(),
                    lease_ms,
                    fence: None,
                    cap: None,
                },
            )
            .await?;
        let claim = ClaimId(claimed.event.0);
        let until = self
            .fold
            .until(task, &claim)
            .ok_or(BoardError::Rejected(Rejection::NotHolder))?;
        Ok(Lease {
            task: task.clone(),
            claim,
            until: Arc::new(AtomicU64::new(until)),
        })
    }

    pub async fn work(
        &mut self,
        task: &TaskId,
        lease_ms: u64,
        pace: &impl Pace,
        steps: &mut impl Steps,
    ) -> Result<Applied, BoardError> {
        let lease = self.lease(task, lease_ms, pace).await?;
        let mut heard = None;
        let mut done = None;
        loop {
            let steers = self
                .boundary(pace.now_ms(), &lease.task, &lease.claim, heard.take())
                .await?;
            heard = steers.last().map(|steer| steer.seq);
            if heard.is_none()
                && let Some(outcome) = done.take()
            {
                let finish = Op::Finish {
                    task: task.clone(),
                    claim: lease.claim.clone(),
                    outcome: Outcome::clone(&outcome),
                };
                match self.submit(pace.now_ms(), finish).await {
                    Err(BoardError::Rejected(Rejection::Steer(SteerRefusal::Unheeded {
                        ..
                    }))) => {
                        done = Some(outcome);
                        continue;
                    }
                    finished => return finished,
                }
            }
            let step = {
                let step = pin!(steps.step(lease.clone(), steers));
                let keep = pin!(self.keep(&lease, lease_ms, pace));
                race(step, keep).await?
            };
            if let Step::Done(outcome) = step {
                done = Some(outcome);
            }
        }
    }

    async fn keep(&mut self, lease: &Lease, lease_ms: u64, pace: &impl Pace) -> BoardError {
        let third = renew_after(lease_ms);
        assert!(third <= lease_ms);
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
                    None => return BoardError::Rejected(Rejection::NotHolder),
                },
                Err(BoardError::Rejected(_)) if mutated(Mutation::RunIgnoresLoss) => {
                    lease
                        .until
                        .store(pace.now_ms() + lease_ms, Ordering::Release);
                    wake = pace.now_ms() + third;
                }
                Err(
                    error @ (BoardError::Rejected(_)
                    | BoardError::Corrupt { .. }
                    | BoardError::Dropped(_)
                    | BoardError::Row { .. }),
                ) => {
                    return error;
                }
                Err(BoardError::Store(_) | BoardError::Unsettled { .. }) => {
                    wake = pace.now_ms() + third;
                }
            }
        }
    }
}

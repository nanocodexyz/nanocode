//! Group commit: many pending events become one multi-row append at the fold's next seq.
//!
//! A writer that holds several events at once (concurrent submitters behind one partition's
//! committer) appends them as rows `next..next + n` in one round trip. The host takes the rows
//! all or nothing; the fold then judges each row in order, so two claims racing for one task in
//! the same batch both land and the claim fence in the fold rejects the later one. Events whose
//! precondition fails against the fold before the batch (a claim on a task posted earlier in the
//! same batch) wait for the next round instead of being rejected early.

use nanocode_durability_core::StoreError;

use crate::{
    Applied, BoardError, Event, Fold,
    sim::{Mutation, mutated},
};

struct Member {
    event: Event,
    body: String,
    verdict: Option<Result<Applied, BoardError>>,
}

struct Landing {
    first: u64,
    members: Vec<usize>,
    error: StoreError,
}

pub struct Group {
    members: Vec<Member>,
    planned: Vec<usize>,
    landing: Option<Landing>,
    rounds: u64,
    fresh: bool,
}

pub enum GroupStep {
    /// Every member is settled: take the verdicts with `Group::settled`.
    Done,
    /// Append these rows, in one call, starting at `first`.
    Append {
        first: u64,
        rows: Vec<(u64, String)>,
    },
    /// The fold may be behind the log and would refuse some writes: scan from `from` and hand
    /// the rows to `Group::read` before refusing them.
    Scan { from: u64 },
}

impl Fold {
    pub fn group(&self, events: Vec<Event>) -> Group {
        let members = events
            .into_iter()
            .map(|event| match serde_json::to_string(&event) {
                Ok(body) => Member {
                    event,
                    body,
                    verdict: None,
                },
                Err(error) => Member {
                    event,
                    body: String::new(),
                    verdict: Some(Err(BoardError::Corrupt {
                        seq: self.next,
                        detail: error.to_string(),
                    })),
                },
            })
            .collect();
        Group {
            members,
            planned: Vec::new(),
            landing: None,
            rounds: 0,
            fresh: false,
        }
    }
}

impl Group {
    pub fn bound(&mut self, scope: &str, max: Option<u64>) {
        for member in self
            .members
            .iter_mut()
            .filter(|member| member.verdict.is_none())
        {
            if let Err(error) = crate::row_fits(scope, &member.body, max) {
                member.verdict = Some(Err(error));
            }
        }
        assert!(self.planned.is_empty());
    }

    pub const fn len(&self) -> usize {
        self.members.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Appends this group made: one per round, plus retries after a lost race.
    pub const fn rounds(&self) -> u64 {
        self.rounds
    }

    fn settle_known(&mut self, fold: &Fold) {
        for member in self
            .members
            .iter_mut()
            .filter(|member| member.verdict.is_none())
        {
            if let Some(verdict) = fold.outcome(&member.event.id) {
                member.verdict = Some(
                    verdict
                        .clone()
                        .map(|seq| Applied {
                            seq,
                            event: member.event.id.clone(),
                        })
                        .map_err(BoardError::Rejected),
                );
            }
        }
    }

    /// The next step against the fold as it stands: the rows to append, or done.
    pub fn plan(&mut self, fold: &Fold) -> GroupStep {
        self.settle_known(fold);
        let mut planned = Vec::new();
        let mut waiting = Vec::new();
        for (index, member) in self.members.iter().enumerate() {
            if member.verdict.is_some() {
                continue;
            }
            match fold.judge(&member.event) {
                Ok(()) => planned.push(index),
                Err(rejection) => waiting.push((index, rejection)),
            }
        }
        if planned.is_empty() {
            if !self.fresh && !waiting.is_empty() {
                return GroupStep::Scan { from: fold.next };
            }
            for (index, rejection) in waiting {
                self.members[index].verdict = Some(Err(BoardError::Rejected(rejection)));
            }
            self.planned.clear();
            return GroupStep::Done;
        }
        let first = fold.next;
        let gap = u64::from(mutated(Mutation::GroupSkipsSeq));
        let rows = planned
            .iter()
            .enumerate()
            .map(|(offset, &index)| {
                (
                    first + offset as u64 * (1 + gap),
                    self.members[index].body.clone(),
                )
            })
            .collect();
        self.planned = planned;
        self.rounds += 1;
        GroupStep::Append { first, rows }
    }

    /// The host took every row of the last plan: fold them and settle their members.
    pub fn acknowledged(&mut self, fold: &mut Fold, first: u64) -> Result<(), BoardError> {
        let planned = std::mem::take(&mut self.planned);
        self.fresh = true;
        for (offset, &index) in planned.iter().enumerate() {
            fold.apply_event(first + offset as u64, &self.members[index].event)?;
        }
        for (offset, &index) in planned.iter().enumerate() {
            let member = &mut self.members[index];
            if mutated(Mutation::GroupVerdictFromPlan) {
                member.verdict = Some(Ok(Applied {
                    seq: first + offset as u64,
                    event: member.event.id.clone(),
                }));
                continue;
            }
            let Some(verdict) = fold.outcome(&member.event.id) else {
                return Err(BoardError::Corrupt {
                    seq: first + offset as u64,
                    detail: format!(
                        "the store acknowledged {} but it did not fold",
                        member.event.id
                    ),
                });
            };
            member.verdict = Some(
                verdict
                    .clone()
                    .map(|seq| Applied {
                        seq,
                        event: member.event.id.clone(),
                    })
                    .map_err(BoardError::Rejected),
            );
        }
        Ok(())
    }

    /// The host refused or lost the last plan's append: rescan before planning again.
    pub fn failed(&mut self, first: u64, error: StoreError) {
        let members = std::mem::take(&mut self.planned);
        self.landing = Some(Landing {
            first,
            members,
            error,
        });
    }

    /// Rows scanned from the fold's next seq after a failed append.
    pub fn read(
        &mut self,
        fold: &mut Fold,
        rows: Result<Vec<(u64, String)>, StoreError>,
    ) -> Result<(), BoardError> {
        let landing = self.landing.take();
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                let landed: &[usize] = landing.as_ref().map_or(&[], |landing| &landing.members);
                for (index, member) in self.members.iter_mut().enumerate() {
                    if member.verdict.is_some() {
                        continue;
                    }
                    let fenced = landing.as_ref().is_some_and(|landing| {
                        matches!(landing.error, StoreError::Fenced)
                            && !mutated(Mutation::FencedUnsettled)
                    });
                    member.verdict = Some(Err(if fenced && landed.contains(&index) {
                        BoardError::Store(StoreError::Fenced)
                    } else if landed.contains(&index) {
                        BoardError::Unsettled {
                            event: member.event.id.clone(),
                            error: error.clone(),
                        }
                    } else {
                        BoardError::Store(error.clone())
                    }));
                }
                return Ok(());
            }
        };
        for (seq, body) in rows {
            fold.apply(seq, &body)?;
        }
        self.fresh = true;
        self.settle_known(fold);
        let Some(landing) = landing else {
            return Ok(());
        };
        let open: Vec<usize> = landing
            .members
            .iter()
            .copied()
            .filter(|&index| self.members[index].verdict.is_none())
            .collect();
        if open.is_empty() {
            return Ok(());
        }
        if open.len() < landing.members.len() {
            return Err(BoardError::Corrupt {
                seq: landing.first,
                detail: format!(
                    "the store landed {} of a group of {}",
                    landing.members.len() - open.len(),
                    landing.members.len()
                ),
            });
        }
        if fold.next <= landing.first {
            let fenced =
                matches!(landing.error, StoreError::Fenced) && !mutated(Mutation::FencedUnsettled);
            for index in open {
                let member = &mut self.members[index];
                member.verdict = Some(Err(if fenced {
                    BoardError::Store(StoreError::Fenced)
                } else {
                    BoardError::Unsettled {
                        event: member.event.id.clone(),
                        error: landing.error.clone(),
                    }
                }));
            }
        }
        Ok(())
    }

    /// Each member's verdict, in submission order.
    pub fn settled(self) -> Vec<Result<Applied, BoardError>> {
        self.members
            .into_iter()
            .map(|member| {
                member.verdict.unwrap_or_else(|| {
                    Err(BoardError::Dropped(
                        "the group ended before this write settled".into(),
                    ))
                })
            })
            .collect()
    }
}

impl Group {
    /// Settles every open member with `error` after a fault the group can't recover from.
    pub fn fail_open(&mut self, error: &BoardError) {
        self.planned.clear();
        self.landing = None;
        for member in self
            .members
            .iter_mut()
            .filter(|member| member.verdict.is_none())
        {
            member.verdict = Some(Err(error.clone()));
        }
    }
}

use std::{ops::Range, sync::Arc, time::Duration};

use nanocode_durability_core::StoreError;
use nanocode_sync::Atom;
use nanocode_turnlog::{Point, TurnLog};

use super::{World, host_record};
use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
};

const BURST: Range<u64> = 6..201;
const WITHIN_MS: u64 = 1_000;
const COMMIT_MS: u64 = 400;
const HOLD_BOUNDS: u32 = 3;
const POSTED_AFTER_MS: u64 = 1;
const GROUP_APPENDS_MAX: u64 = 2;

type Wrong = Atom<Vec<String>>;

fn wrong(wrong: &Wrong, message: String) {
    wrong.update(|held| held.push(message.clone()));
}

fn ran(clock: &Arc<Clock>, rng: &mut Rng, tasks: Vec<Task<'_>>) -> Result<(), String> {
    match Executor::new(Arc::clone(clock), Rng::derive(rng.below(u64::MAX), "queue"))
        .run(tasks, None)
    {
        Ended::Finished => Ok(()),
        ended => Err(format!("the writer queue phase ended {ended:?}")),
    }
}

fn first(wrong: &Wrong) -> Result<(), String> {
    wrong.read(|held| held.first().cloned()).map_or(Ok(()), Err)
}

async fn grouped(world: &World, log: &TurnLog, key: String, failed: &Wrong) {
    let record = host_record(&key, "1");
    let answer = log
        .append_within(move |writing| {
            Box::pin(async move {
                writing.push(record)?;
                writing.commit_grouped(Point::Progress).await
            })
        })
        .await;
    match answer {
        Err(error) => wrong(failed, format!("grouped append {key} was refused: {error}")),
        Ok(()) => match world.holding("queue", &key) {
            Ok(seqs) if seqs.is_empty() => wrong(
                failed,
                format!("grouped append {key} answered before its group committed"),
            ),
            Ok(_) => {}
            Err(error) => wrong(failed, error),
        },
    }
}

async fn plain(log: &TurnLog, key: String, failed: &Wrong) {
    let record = host_record(&key, "1");
    let answer: Result<u64, StoreError> = log
        .write_within(move |writing| {
            Box::pin(async move {
                writing.push(record)?;
                writing.commit(Point::Progress).await
            })
        })
        .await;
    if let Err(error) = answer {
        wrong(
            failed,
            format!("append {key} was refused while the writer drained its queue: {error}"),
        );
    }
}

impl World {
    fn delayed(&self, delay: Option<Duration>) {
        self.model
            .database
            .update(|database| database.delay = delay);
    }

    pub(super) async fn queue(&mut self, clock: &Arc<Clock>, rng: &mut Rng) -> Result<(), String> {
        let within = Duration::from_millis(WITHIN_MS);
        let log = self
            .open("queue")
            .await
            .map_err(|error| format!("open queue: {error}"))?
            .host_calls_within(Some(within));
        let burst = rng.range(BURST.start, BURST.end);
        self.delayed(Some(Duration::from_millis(COMMIT_MS)));
        let before = self.appends();
        let failed = Wrong::default();
        let tasks = (0..burst)
            .map(|index| -> Task<'_> {
                Box::pin(grouped(self, &log, format!("grouped-{index}"), &failed))
            })
            .collect();
        ran(clock, rng, tasks)?;
        first(&failed)?;
        let appends = self.appends() - before;
        if appends > GROUP_APPENDS_MAX {
            return Err(format!(
                "{burst} concurrent grouped appends took {appends} host appends, not one group"
            ));
        }
        self.stats.grouped_appends += burst;
        let tasks = (0..burst)
            .map(|index| -> Task<'_> { Box::pin(plain(&log, format!("plain-{index}"), &failed)) })
            .collect();
        ran(clock, rng, tasks)?;
        first(&failed)?;
        self.delayed(None);
        self.held(clock, rng, &log, within).await
    }

    async fn held(
        &mut self,
        clock: &Arc<Clock>,
        rng: &mut Rng,
        log: &TurnLog,
        within: Duration,
    ) -> Result<(), String> {
        let refused = Atom::new(None::<Result<(), StoreError>>);
        let hold = within * HOLD_BOUNDS;
        let holder: Task<'_> = Box::pin(async move {
            let held: Result<(), StoreError> = log
                .write_within(move |_| {
                    Box::pin(async move {
                        nanocode_platform::sleep(hold).await;
                        Ok(())
                    })
                })
                .await;
            assert!(held.is_ok(), "a held job that touches no host answers");
        });
        let refused_ref = &refused;
        let queued: Task<'_> = Box::pin(async move {
            nanocode_platform::sleep(Duration::from_millis(POSTED_AFTER_MS)).await;
            let record = host_record("held-behind", "1");
            let answer = log
                .write_within(move |writing| {
                    Box::pin(async move {
                        writing.push(record)?;
                        writing.commit(Point::Progress).await.map(drop)
                    })
                })
                .await;
            refused_ref.update(|held| *held = Some(answer.clone()));
        });
        ran(clock, rng, vec![holder, queued])?;
        let deadline_ms = u64::try_from(within.as_millis()).expect("a sim deadline fits");
        match refused.read(Clone::clone) {
            Some(Err(StoreError::Queued {
                ahead: 1,
                waited_ms,
                ..
            })) if waited_ms == deadline_ms => {}
            answer => {
                return Err(format!(
                    "a job queued behind a held writer answered {answer:?}, not Queued behind 1 job after {deadline_ms} ms"
                ));
            }
        }
        let after: Result<u64, StoreError> = log
            .write_within(|writing| Box::pin(writing.commit(Point::Progress)))
            .await;
        after.map_err(|error| format!("the writer did not drain after a refusal: {error}"))?;
        if log.queued() != 0 {
            return Err(format!("{} refused jobs stayed queued", log.queued()));
        }
        let landed = self.holding("queue", "held-behind")?;
        if !landed.is_empty() {
            return Err(format!(
                "a job refused before it started still ran and landed at {landed:?}"
            ));
        }
        self.stats.queued_refusals += 1;
        Ok(())
    }
}

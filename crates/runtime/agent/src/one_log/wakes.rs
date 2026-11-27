use std::sync::Arc;

use nanocode_turnlog::{
    ArmedWake, Point, Record, Rider, Unfit, Wake, WakeRider, Writing,
    wake_sim::{Mutation, mutated},
};
use serde_json::{Map, Value, value::RawValue};

use super::{Lifecycle, OneLog, invalid, logged, raw, record_of, upgraded};
use crate::{
    durability::{SqlStatement, StoreError},
    session_log::{LoggedEvent, Submission, TurnRecord, now, submission_digest},
};

#[derive(Debug)]
pub enum WakeError {
    Full { max: usize },
    Store(StoreError),
    Unfit(Unfit),
}

impl std::fmt::Display for WakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { max } => write!(
                formatter,
                "this session already holds its {max} wakes: disarm one or raise the policy's wakes_max"
            ),
            Self::Store(error) => error.fmt(formatter),
            Self::Unfit(unfit) => unfit.fmt(formatter),
        }
    }
}

impl std::error::Error for WakeError {}

impl From<StoreError> for WakeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Heard {
    pub full: Vec<(String, u64)>,
    pub heard: Vec<(String, u64)>,
}

#[derive(Clone, Debug)]
pub enum Fire {
    Fired {
        event: LoggedEvent,
        next: Option<u64>,
        record: TurnRecord,
    },
    NotDue,
    Stale,
    Taken(TurnRecord),
}

impl OneLog {
    pub async fn index_wakes(&self, index: Arc<dyn WakeRider>) -> Result<(), StoreError> {
        self.log
            .answered_within("wake index prepare", index.prepare())
            .await?;
        let this = self.weak();
        self.writes(move |writing| {
                Box::pin(async move {
                    let this = upgraded(&this);
                    let summary = writing.fold().wakes().summary();
                    this.log
                        .answered_within("wake index write", index.write(this.scope(), &summary))
                        .await?;
                    this.wake_index.set(Some(index));
                    Ok(())
                })
            })
            .await
    }

    async fn commit_wakes(&self, writing: &mut Writing, point: Point) -> Result<(), StoreError> {
        let Some(index) = self.wake_index.read(Clone::clone) else {
            writing.commit(point).await?;
            return Ok(());
        };
        let held = index.statements(self.scope(), &writing.fold().wakes().summary());
        let statements = held
            .iter()
            .map(|(sql, params)| SqlStatement {
                sql,
                params: params.iter().map(String::as_str).collect(),
            })
            .collect::<Vec<_>>();
        let rider = Rider {
            host: index.host(),
            statements: &statements,
        };
        let (_, carried) = writing.commit_carrying(point, rider).await?;
        // An error from here on would make the caller unwind a commit that is already durable, so
        // a rider the log's host did not carry is written on its own instead, and its failure
        // only leaves the index stale until the next index_wakes.
        if !carried {
            let summary = writing.fold().wakes().summary();
            let written = self
                .log
                .answered_within("wake index write", index.write(self.scope(), &summary))
                .await;
            if let Err(error) = written {
                nanocode_trace::warn!(
                    %error,
                    scope = self.scope(),
                    "turn log committed its wakes, but its host did not carry the wake index's rider and writing the index on its own failed; it stays stale until index_wakes"
                );
            }
        }
        Ok(())
    }

    pub async fn arm(&self, key: &str, wake: Wake) -> Result<u64, WakeError> {
        wake.fit().map_err(WakeError::Unfit)?;
        let this = self.weak();
        let key = key.to_owned();
        self.writes(move |writing| {
                Box::pin(async move {
                    let this = upgraded(&this);
                    let key = key.as_str();
                    let prior = writing.fold().wakes().get(key).cloned();
                    if let Some(max) = this.wakes_max
                        && prior.is_none()
                        && writing.fold().wakes().len() >= max
                        && !mutated(Mutation::Unbounded)
                    {
                        return Err(WakeError::Full { max });
                    }
                    let generation = writing.fold().wakes().next_generation();
                    writing.push(Record::Armed {
                        generation,
                        key: key.to_owned(),
                        wake,
                    })?;
                    if let Err(error) = this.commit_wakes(writing, Point::Progress).await {
                        writing.unwake(key, prior);
                        return Err(error.into());
                    }
                    assert_eq!(
                        writing
                            .fold()
                            .wakes()
                            .get(key)
                            .map(|armed| armed.generation),
                        Some(generation),
                        "an armed wake holds the generation it was armed at"
                    );
                    Ok(generation)
                })
            })
            .await
    }

    pub async fn disarm(&self, key: &str) -> Result<Option<u64>, StoreError> {
        let this = self.weak();
        let key = key.to_owned();
        self.writes(move |writing| {
                Box::pin(async move {
                    let this = upgraded(&this);
                    let key = key.as_str();
                    let Some(prior) = writing.fold().wakes().get(key).cloned() else {
                        return Ok(None);
                    };
                    let generation = prior.generation;
                    writing.push(Record::Disarmed {
                        generation,
                        key: key.to_owned(),
                    })?;
                    if let Err(error) = this.commit_wakes(writing, Point::Progress).await {
                        writing.unwake(key, Some(prior));
                        return Err(error);
                    }
                    assert!(
                        writing.fold().wakes().get(key).is_none(),
                        "a disarmed wake is gone"
                    );
                    Ok(Some(generation))
                })
            })
            .await
    }

    pub async fn hear(
        &self,
        topic: &str,
        fields: &Map<String, Value>,
        event: Box<RawValue>,
    ) -> Result<Heard, StoreError> {
        let this = self.weak();
        let topic = topic.to_owned();
        let fields = fields.clone();
        self.writes(move |writing| {
                Box::pin(async move {
                    let this = upgraded(&this);
                    let topic = topic.as_str();
                    let fields = &fields;
                    let mut heard = Heard::default();
                    let mut priors = Vec::new();
                    for (key, armed) in writing.fold().wakes().iter() {
                        if !armed.hears(topic, fields) {
                            continue;
                        }
                        assert!(armed.wake.on.is_some(), "a wake that hears has a trigger");
                        let found = (key.to_owned(), armed.generation);
                        if armed.full() {
                            heard.full.push(found);
                        } else {
                            priors.push((key.to_owned(), armed.clone()));
                            heard.heard.push(found);
                        }
                    }
                    if heard.heard.is_empty() {
                        return Ok(heard);
                    }
                    for (key, generation) in &heard.heard {
                        writing.push(Record::Heard {
                            event: event.clone(),
                            generation: *generation,
                            key: key.clone(),
                        })?;
                    }
                    let committed = if mutated(Mutation::IndexSkipsHear) {
                        writing.commit(Point::Progress).await.map(drop)
                    } else {
                        this.commit_wakes(writing, Point::Progress).await
                    };
                    if let Err(error) = committed {
                        for (key, prior) in priors {
                            writing.unwake(&key, Some(prior));
                        }
                        return Err(error);
                    }
                    Ok(heard)
                })
            })
            .await
    }

    pub async fn wake(&self, key: &str) -> Option<ArmedWake> {
        let key = key.to_owned();
        self.log
            .read(move |fold| fold.wakes().get(&key).cloned())
            .await
    }

    pub async fn due_wakes(&self, now: u64) -> Vec<(String, ArmedWake)> {
        self.log
            .read(move |fold| {
                fold.wakes()
                    .due(now)
                    .map(|(key, armed)| (key.to_owned(), armed.clone()))
                    .collect()
            })
            .await
    }

    pub async fn next_due(&self) -> Option<u64> {
        self.log.read(|fold| fold.wakes().next_due()).await
    }

    pub async fn fire(
        &self,
        key: &str,
        generation: u64,
        now_ms: u64,
        submission: &Submission,
        next: Option<Wake>,
    ) -> Result<Fire, WakeError> {
        if let Some(next) = &next {
            next.fit().map_err(WakeError::Unfit)?;
        }
        let digest = submission_digest(&submission.prompt, submission.hand.as_deref())?;
        let this = self.weak();
        let key = key.to_owned();
        let submission = submission.clone();
        self.writes(move |writing| {
                Box::pin(async move {
                    let this = upgraded(&this);
                    let key = key.as_str();
                    let submission = &submission;
                    let id = submission.request_id.as_str();
                    let prior = match writing.fold().wakes().get(key) {
                        Some(armed) if armed.generation == generation => armed.clone(),
                        _ => return Ok(Fire::Stale),
                    };
                    if !prior.due(now_ms) {
                        return Ok(Fire::NotDue);
                    }
                    if let Some(turn) = writing.fold().turn(id) {
                        return Ok(Fire::Taken(record_of(turn)?));
                    }
                    let first = writing.next_event();
                    let at = now();
                    let body = raw(&Lifecycle {
                        kind: "queued",
                        request_id: id,
                    })?;
                    let fired = this
                        .fired(
                            writing, key, generation, submission, digest, first, at, &body, next,
                        )
                        .await;
                    let next = match fired {
                        Ok(next) => next,
                        Err(error) => {
                            writing.unadmit(id, first);
                            writing.unwake(key, Some(prior));
                            return Err(error.into());
                        }
                    };
                    let record = record_of(
                        writing
                            .fold()
                            .turn(id)
                            .ok_or_else(|| invalid(format!("fired turn {id} is missing")))?,
                    )?;
                    assert_eq!(
                        writing
                            .fold()
                            .wakes()
                            .get(key)
                            .map(|armed| armed.generation),
                        next,
                        "a fired wake is gone or rearmed at its next generation"
                    );
                    Ok(Fire::Fired {
                        event: logged(first, Some(id), at, body),
                        next,
                        record,
                    })
                })
            })
            .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one fire's parts, pushed in one writer job"
    )]
    async fn fired(
        &self,
        writing: &mut Writing,
        key: &str,
        generation: u64,
        submission: &Submission,
        digest: String,
        first: u64,
        at: u64,
        body: &RawValue,
        next: Option<Wake>,
    ) -> Result<Option<u64>, StoreError> {
        let id = submission.request_id.as_str();
        writing.push(Record::Admitted {
            digest,
            first,
            hand: submission.hand.clone(),
            metadata: submission.metadata.clone(),
            prompt: raw(&submission.prompt)?,
            turn: id.to_owned(),
        })?;
        writing.push(Record::Event {
            at,
            event: body.to_owned(),
            seq: first,
            turn: Some(id.to_owned()),
        })?;
        if mutated(Mutation::FiredApart) {
            writing.commit(Point::Admission).await?;
        }
        writing.push(Record::Fired {
            generation,
            key: key.to_owned(),
            turn: id.to_owned(),
        })?;
        let rearmed = match next {
            Some(wake) => {
                let generation = writing.fold().wakes().next_generation();
                writing.push(Record::Armed {
                    generation,
                    key: key.to_owned(),
                    wake,
                })?;
                Some(generation)
            }
            None => None,
        };
        self.commit_wakes(writing, Point::Admission).await?;
        Ok(rearmed)
    }
}

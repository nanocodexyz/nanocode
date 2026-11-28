use super::*;
use crate::format::FormatFuture;

#[inline(never)]
pub(super) fn load<'a>(
    this: &'a SessionStore,
    id: &'a str,
) -> FormatFuture<'a, Result<StoredSession>> {
    Box::pin(async move {
        let state = this.require(id).await?;
        let (checkpoint, checkpoint_payload, hydrated) =
            match this.loaded_checkpoint(id, &state, false).await? {
                Some((checkpoint, payload, records, hydrated)) => (
                    Some(checkpoint),
                    Some((payload, Arc::new(records))),
                    hydrated,
                ),
                None => (None, None, None),
            };
        let record = match (state.session(), &checkpoint) {
            (Some(record), _) => record.clone(),
            (None, Some(checkpoint)) => this.infer_record(id, checkpoint.head())?,
            (None, None) => {
                return Err(Error::SessionNotFound {
                    session_id: id.to_owned(),
                });
            }
        };
        let turns = this.state_turns(id, &state).await?;
        let admitted = admitted_turns(&turns, &state);
        let progress = this.progress(id, &state, record.family()).await?;
        let transcript = match (&progress, &checkpoint) {
            (Some((_, view, steers)), _) => {
                let mut transcript = this.transcript(view, record.family(), &admitted)?;
                transcript.extend(steers.iter().cloned().map(TranscriptItem::User));
                Transcript::Read(transcript)
            }
            (None, Some(_)) => Transcript::Checkpoint {
                family: record.family(),
                steers: admitted.iter().map(|turn| turn.steers).collect(),
            },
            (None, None) => Transcript::Read(Vec::new()),
        };
        drop(admitted);
        let mut stored = StoredSession {
            summary: SessionSummary {
                record,
                preview: None,
            },
            transcript,
            turns,
            in_progress: progress.map(|(operation, _, _)| operation),
            checkpoint,
            formats: this.formats.clone(),
            loaded: Loaded {
                store: this.store.downgrade(),
                state_id: id.to_owned(),
                checkpoint_key: state
                    .latest_checkpoint()
                    .map(|payload| payload.key().to_owned()),
                checkpoint_payload,
                hydrated,
                record_inferred: state.session().is_none(),
            },
        };
        stored.summary.preview = match stored.turns.iter().find_map(|turn| turn.preview.clone()) {
            Some(preview) => Some(preview),
            None => stored
                .transcript()?
                .into_iter()
                .find_map(|item| match item {
                    TranscriptItem::User(text) => Some(text),
                    _ => None,
                }),
        };
        Ok(stored)
    })
}

#[inline(never)]
pub(super) fn advance<'a>(
    this: &'a SessionStore,
    id: &'a str,
    mut previous: StoredSession,
) -> FormatFuture<'a, Result<(StoredSession, Advance)>> {
    Box::pin(async move {
        let state = this.require(id).await?;
        let usable = previous.loaded.store.is(&this.store) && previous.loaded.state_id == id;
        let head_key = state
            .latest_checkpoint()
            .map(|payload| payload.key().to_owned());
        let (checkpoint, checkpoint_payload, hydrated, advance) = if usable
            && head_key.is_some()
            && head_key == previous.loaded.checkpoint_key
            && previous.checkpoint.is_some()
        {
            (
                previous.checkpoint.take(),
                previous.loaded.checkpoint_payload.take(),
                previous.loaded.hydrated.take(),
                Advance {
                    kept: true,
                    ..Advance::default()
                },
            )
        } else {
            let earlier = match previous.checkpoint.take() {
                Some(checkpoint) if usable => Some(crate::Previous {
                    checkpoint,
                    hydrated: previous.loaded.hydrated.take(),
                }),
                _ => None,
            };
            match this.advanced_checkpoint(id, &state, earlier).await? {
                Some(((checkpoint, payload, records, hydrated), advance)) => (
                    Some(checkpoint),
                    Some((payload, Arc::new(records))),
                    hydrated,
                    advance,
                ),
                None => (
                    None,
                    None,
                    None,
                    Advance {
                        whole: true,
                        ..Advance::default()
                    },
                ),
            }
        };
        let record = match (state.session(), &checkpoint) {
            (Some(record), _) => record.clone(),
            (None, Some(checkpoint)) => this.infer_record(id, checkpoint.head())?,
            (None, None) => {
                return Err(Error::SessionNotFound {
                    session_id: id.to_owned(),
                });
            }
        };
        let turns = this.state_turns(id, &state).await?;
        let preview = turns
            .iter()
            .find_map(|turn| turn.preview.clone())
            .or(previous.summary.preview);
        Ok((
            StoredSession {
                summary: SessionSummary { record, preview },
                transcript: Transcript::Read(Vec::new()),
                turns,
                in_progress: None,
                checkpoint,
                formats: this.formats.clone(),
                loaded: Loaded {
                    store: this.store.downgrade(),
                    state_id: id.to_owned(),
                    checkpoint_key: head_key,
                    checkpoint_payload,
                    hydrated,
                    record_inferred: state.session().is_none(),
                },
            },
            advance,
        ))
    })
}

#[inline(never)]
pub(super) fn load_resumable<'a>(
    this: &'a SessionStore,
    id: &'a str,
    defers: bool,
) -> FormatFuture<'a, Result<ResumableSession>> {
    Box::pin(async move {
        let state = this.require_head(id).await?;
        let (checkpoint, checkpoint_payload, hydrated) =
            match this.loaded_checkpoint(id, &state, defers).await? {
                Some((checkpoint, payload, records, hydrated)) => (
                    Some(checkpoint),
                    Some((payload, Arc::new(records))),
                    hydrated,
                ),
                None => (None, None, None),
            };
        let record = match (state.session(), &checkpoint) {
            (Some(record), _) => record.clone(),
            (None, Some(checkpoint)) => this.infer_record(id, checkpoint.head())?,
            (None, None) => {
                return Err(Error::SessionNotFound {
                    session_id: id.to_owned(),
                });
            }
        };
        let pending = this
            .stored_turns(id, state.pending_operations(), true)
            .await?;
        Ok(ResumableSession {
            checkpoint,
            pending,
            record,
            formats: this.formats.clone(),
            loaded: Loaded {
                store: this.store.downgrade(),
                state_id: id.to_owned(),
                checkpoint_key: state
                    .latest_checkpoint()
                    .map(|payload| payload.key().to_owned()),
                checkpoint_payload,
                hydrated,
                record_inferred: state.session().is_none(),
            },
        })
    })
}

#[inline(never)]
pub(super) fn turns<'a>(
    this: &'a SessionStore,
    id: &'a str,
) -> FormatFuture<'a, Result<Vec<StoredTurn>>> {
    Box::pin(async move {
        let state = this.require(id).await?;
        this.state_turns(id, &state).await
    })
}

#[inline(never)]
pub(super) fn pending_turns<'a>(
    this: &'a SessionStore,
    id: &'a str,
) -> FormatFuture<'a, Result<Vec<StoredTurn>>> {
    Box::pin(async move {
        let state = this.require_head(id).await?;
        this.stored_turns(id, state.pending_operations(), false)
            .await
    })
}

#[inline(never)]
pub(super) fn resumable<'a>(
    this: &'a SessionStore,
    id: &'a str,
) -> FormatFuture<'a, Result<Option<Resumable>>> {
    Box::pin(async move {
        let Some(state) = this.peek_head(id).await? else {
            return Ok(None);
        };
        let Some(record) = state.session().cloned() else {
            return match this.load(id).await {
                Ok(stored) => Ok(Some(Resumable {
                    record: stored.summary.record,
                    pending: stored
                        .turns
                        .into_iter()
                        .filter(|turn| turn.status == TurnStatus::Pending)
                        .collect(),
                    checkpointed: stored.checkpoint.is_some(),
                })),
                Err(Error::SessionNotFound { .. }) => Ok(None),
                Err(error) => Err(error),
            };
        };
        Ok(Some(Resumable {
            pending: this
                .stored_turns(id, state.pending_operations(), false)
                .await?,
            checkpointed: state.latest_checkpoint().is_some(),
            record,
        }))
    })
}

#[inline(never)]
pub(super) fn turn<'a>(
    this: &'a SessionStore,
    id: &'a str,
    request_id: &'a str,
) -> FormatFuture<'a, Result<Option<StoredTurn>>> {
    Box::pin(async move {
        let mut state = this.require_head(id).await?;
        if state.operation(request_id).is_none() {
            state
                .hydrate(&mut this.store.clone(), id, &mut HashSet::new(), true)
                .await?;
        }
        let Some(operation) = state.operation(request_id) else {
            return Ok(None);
        };
        Ok(this
            .stored_turns(id, vec![(request_id, operation)], false)
            .await?
            .pop())
    })
}

use super::*;

#[inline(never)]
pub(super) fn resume_turn<'a>(
    this: &'a Agent,
    id: String,
    input: serde_json::Value,
) -> HandleFuture<'a, Result<()>> {
    Box::pin(async move {
        let prompt = this.backend.decode_durable_prompt(input)?;
        let claim = this.claim_request(&id).await;
        let key = claim.admitted.key;
        let (events, _turn_events) = this.events.mirrored_channel();
        let events = events.with_turn_id(id.clone());
        let turn = this
            .backend
            .submit(BackendPrompt {
                key,
                prompt,
                request_id: Some(id),
                carried_steers: Vec::new(),
                cancel_on_admission: false,
                events,
            })
            .await?;
        drop(tracked(Some(claim), turn.result));
        Ok(())
    })
}

#[inline(never)]
pub(super) fn resume_interrupted<'a>(
    this: &'a Agent,
    request_id: Option<&'a str>,
) -> HandleFuture<'a, Result<bool>> {
    Box::pin(async move {
        loop {
            let Some(resumption) = this.interrupted.turns.load_full() else {
                return Ok(false);
            };
            if resumption.claimed.swap(true, Ordering::AcqRel) {
                resumption.done.wait().await;
                continue;
            }
            let interrupted =
                request_id.is_some_and(|id| resumption.turns.iter().any(|(turn, _)| turn == id));
            let mut claim = ResumptionClaim {
                interrupted: &this.interrupted,
                resumption,
                resumed: 0,
                remaining: None,
            };
            let (result, remaining) = this
                .resume_turns(&claim.resumption.turns, request_id, &mut claim.resumed)
                .await;
            claim.remaining = Some(remaining);
            return result.map(|()| interrupted);
        }
    })
}

#[inline(never)]
pub(super) fn resume_turns<'a>(
    this: &'a Agent,
    turns: &'a [InterruptedTurn],
    request_id: Option<&'a str>,
    progress: &'a mut usize,
) -> HandleFuture<'a, (Result<()>, Vec<InterruptedTurn>)> {
    Box::pin(async move {
        let (resumed, mut kept) =
            match request_id.and_then(|id| turns.iter().position(|(turn, _)| turn == id)) {
                Some(position) => (&turns[..position], turns[position + 1..].to_vec()),
                None => (turns, Vec::new()),
            };
        for (index, (id, input)) in resumed.iter().enumerate() {
            match this.resume_turn(id.clone(), input.clone()).await {
                Ok(()) => {}
                Err(error)
                    if error.execution_policy_disposition()
                        == Some(crate::ExecutionPolicyDisposition::Retry) => {}
                Err(error) => {
                    let mut remaining = resumed[index..].to_vec();
                    remaining.append(&mut kept);
                    return (Err(error), remaining);
                }
            }
            *progress = index + 1;
        }
        (Ok(()), kept)
    })
}

#[inline(never)]
pub(super) fn claim_seed<'a>(
    this: &'a Agent,
    prompt: Prompt,
) -> HandleFuture<'a, Result<(Prompt, Option<SeedClaim>)>> {
    Box::pin(async move {
        let pending = {
            let state = this.seed.load();
            if state.in_flight {
                None
            } else {
                state.transcript.clone()
            }
        };
        let Some(seed) = pending else {
            return Ok((prompt, None));
        };
        let committed = match this.backend.transcript().await {
            Ok(transcript) => !transcript.is_empty(),
            Err(NanocodeError::UnsupportedCapability { .. }) => false,
            Err(error) => return Err(error),
        };
        let mut claimed = false;
        this.seed.rcu(|state| {
            claimed = !committed && !state.in_flight && state.transcript.is_some();
            SeedState {
                transcript: if committed {
                    None
                } else {
                    state.transcript.clone()
                },
                in_flight: state.in_flight || claimed,
            }
        });
        if !claimed {
            return Ok((prompt, None));
        }
        let transcript = seed
            .iter()
            .cloned()
            .chain(prompt.transcript().iter().cloned())
            .collect::<Vec<_>>();
        Ok((
            prompt.with_transcript(transcript),
            Some(SeedClaim {
                seed: Arc::clone(&this.seed),
                completed: false,
            }),
        ))
    })
}

#[inline(never)]
pub(super) fn claim_request<'a>(this: &'a Agent, id: &'a str) -> HandleFuture<'a, RequestClaim> {
    Box::pin(async move {
        loop {
            let admitted = Arc::new(Admitted {
                key: this.next_key(),
                settled: Signal::new(),
            });
            let previous = this.requests.0.rcu(|requests| {
                let mut requests = std::collections::HashMap::clone(requests);
                requests
                    .entry(id.to_owned())
                    .or_insert_with(|| Arc::clone(&admitted));
                requests
            });
            match previous.get(id) {
                Some(running) => running.settled.wait().await,
                None => {
                    return RequestClaim {
                        requests: Arc::clone(&this.requests),
                        id: id.to_owned(),
                        admitted,
                    };
                }
            }
        }
    })
}

#[inline(never)]
pub(super) fn admit<'a>(
    this: &'a Agent,
    id: Option<&'a str>,
) -> HandleFuture<'a, Result<(Option<RequestClaim>, BackendTurnKey, bool)>> {
    Box::pin(async move {
        if id.is_some_and(|id| id.trim().is_empty()) {
            return Err(NanocodeError::InvalidRequest(
                "request ID must not be empty".to_owned(),
            ));
        }
        let interrupted = this.resume_interrupted(id).await?;
        let Some(id) = id else {
            return Ok((None, this.next_key(), false));
        };
        let claim = this.claim_request(id).await;
        let key = claim.admitted.key;
        Ok((Some(claim), key, interrupted))
    })
}

#[inline(never)]
pub(super) fn abandon_created<'a>(this: &'a Agent) -> HandleFuture<'a, ()> {
    Box::pin(async move {
        let _ = this.backend.persist_initial().await;
        let _ = this.backend.discard_initial().await;
        let _ = this.shutdown().await;
    })
}

#[inline(never)]
pub(super) fn spawn_with_host_context<'a>(
    this: &'a AgentHandle,
    options: SpawnOptions,
    host_context: Option<Arc<str>>,
) -> HandleFuture<'a, Result<(Agent, AgentEvents)>> {
    Box::pin(async move {
        options.validate_harness()?;
        this.native.ensure_available(this.clone()).await?;
        created(
            this.factory
                .as_ref()
                .unwrap_or(&this.native)
                .spawn(this.clone(), options, host_context)
                .await,
        )
        .await
    })
}

#[inline(never)]
pub(super) fn restore_runtime<'a>(
    this: &'a AgentHandle,
    checkpoint: SessionCheckpoint,
    host_context: Option<Arc<str>>,
) -> HandleFuture<'a, Result<(Agent, AgentEvents)>> {
    Box::pin(async move {
        this.native.ensure_available(this.clone()).await?;
        let restored = if let Some(factory) = &this.factory {
            factory
                .restore(this.clone(), checkpoint, host_context)
                .await
        } else if checkpoint.family() == this.harness_family() {
            this.native
                .restore(this.clone(), checkpoint, host_context)
                .await
        } else {
            return Err(NanocodeError::UnsupportedCapability {
                capability: "cross_family_restore",
            });
        };
        created(restored).await
    })
}

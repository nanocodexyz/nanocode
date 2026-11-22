use super::*;
use serde_json::json;

pub(super) fn compact(this: &Driver) -> BackendFuture<Result<()>> {
    let state = this.state.clone();
    Box::pin(async move {
        let (accepted, receipt) = oneshot::channel();
        let task = async move {
            let cleanup = state.clone();
            let cancellation = Arc::new(Cancellation::default());
            let cleanup_cancellation = cancellation.clone();
            let result = async move {
                let _admission = state.admission.enter().await;
                state.compaction_cancel.set(Some(cancellation.clone()));
                if state.life.stopped() {
                    return Err(NanocodeError::AgentStopped);
                }

                for cancel in state.cancellations.get().values() {
                    cancel.cancel();
                }
                loop {
                    let notified = state.idle.notified();
                    let mut notified = std::pin::pin!(notified);
                    notified.as_mut().enable();
                    if state.cancellations.get().is_empty() {
                        break;
                    }
                    notified.await;
                }
                let mut context = state.conversation.acquire().await;
                let mut operation = None;
                let mut _compaction_attempt = None;
                if let Some(policy) = &state.policy {
                    let (id, admission) = policy
                        .admit(
                            durable::candidate_id("compact"),
                            json!({"provider":"claude","kind":"compact"}),
                            true,
                        )
                        .await?;
                    match admission {
                        Admission::Completed { .. } => return Ok(()),
                        Admission::Failed { error, .. } => {
                            return Err(NanocodeError::ReplayedExecutionFailed(error));
                        }
                        Admission::Cancelled => return Err(NanocodeError::TurnCancelled),
                        Admission::Execute | Admission::Resume => {
                            if let Err(error) = policy.begin_attempt(id.clone()).await {
                                retire_unstarted(policy.as_ref(), id).await;
                                return Err(error);
                            }
                            _compaction_attempt = Some(AttemptGuard::begin(state.clone()));
                        }
                    }
                    operation = Some(id);
                }
                let mut journal = Journal::default();
                let mut cursor = state
                    .cursor(
                        &mut context,
                        operation.as_deref(),
                        state.speed(),
                        None,
                        HashSet::new(),
                        &mut journal,
                    )
                    .await?;
                state
                    .stage_cursor(&mut cursor, &mut context, &mut journal)
                    .await?;
                state.flush_journal(&mut journal).await?;
                let compaction_started = nanocode_platform::now();
                State::publish(
                    &state.session_events,
                    AgentEventKind::ModelCompactionStarted,
                    &json!({
                        "after_model_call_index": 0,
                        "active_context_tokens": context.active_context_tokens,
                        "auto_compact_token_limit": cursor.threshold,
                    }),
                );
                let result = state
                    .compact_locked(
                        &mut context,
                        &cancellation,
                        CompactionMode::Manual,
                        &cursor,
                        "manual-compact",
                    )
                    .await;
                let (kind, payload) = match &result {
                    Ok(_) => (
                        AgentEventKind::ModelCompactionCompleted,
                        State::compacted_payload(0, compaction_started),
                    ),
                    Err(error) => (
                        AgentEventKind::ModelCompactionFailed,
                        json!({
                            "after_model_call_index": 0,
                            "duration_ns": u64::try_from(nanocode_platform::elapsed(compaction_started).as_nanos()).unwrap_or(u64::MAX),
                            "error": error.to_string(),
                        }),
                    ),
                };
                State::publish(&state.session_events, kind, &payload);
                state.publish_round_boundary(&context).await;
                if let (Some(policy), Some(id)) = (&state.policy, operation) {
                    if result
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.execution_policy_disposition().is_some())
                    {
                        state.life.stop();
                    } else {
                        let checkpoint = serde_json::to_value(state.snapshot(&context).await?)
                            .map_err(provider_error)?;
                        let settled = match &result {
                            Ok(_) => policy.complete(id, checkpoint, Value::Null).await,
                            Err(NanocodeError::TurnCancelled) => {
                                policy.cancel(id, checkpoint).await
                            }
                            Err(error) => policy.fail(id, checkpoint, error.to_string()).await,
                        };
                        if settled.is_err() {
                            state.life.stop();
                        }
                        settled.map_err(|error| {
                            match error.execution_policy_disposition() {
                                Some(
                                    nanocode_agent_core::ExecutionPolicyDisposition::Retry,
                                ) => durable::recovery_error(error),
                                _ => error,
                            }
                        })?;
                    }
                }
                result
                    .map(|_| ())
                    .map_err(|error| match error.execution_policy_disposition() {
                        Some(nanocode_agent_core::ExecutionPolicyDisposition::Retry) => {
                            durable::recovery_error(error)
                        }
                        _ => error,
                    })
            }
            .await;
            cleanup.compaction_cancel.update(|registered| {
                if registered
                    .as_ref()
                    .is_some_and(|token| Arc::ptr_eq(token, &cleanup_cancellation))
                {
                    *registered = None;
                }
            });
            if result
                .as_ref()
                .err()
                .is_some_and(|error| error.execution_policy_disposition().is_some())
            {
                cleanup.life.stop();
            }
            let _ = accepted.send(result);
        };
        spawn_task(Box::pin(task));
        receipt.await.unwrap_or(Err(NanocodeError::TurnStopped))
    })
}

pub(super) fn submit(
    this: &Driver,
    mut request: BackendPrompt,
) -> BackendFuture<Result<BackendTurn>> {
    let state = this.state.clone();
    let parent = nanocode_trace::Span::current();
    Box::pin(async move {
        state.store_prompt_images(&mut request.prompt).await?;
        if !request.carried_steers.is_empty()
            && !state
                .policy
                .as_ref()
                .is_some_and(|policy| policy.supports_steering())
        {
            return Err(unsupported(
                "Claude carried steers require an execution policy with steering receipts",
            ));
        }
        for (_, prompt) in &mut request.carried_steers {
            state.store_prompt_images(prompt).await?;
        }
        let (accepted, receipt) = oneshot::channel();
        let task = async move {
            let result = async move {
                let _admission = state.admission.enter().await;
                if state.life.stopped() {
                    return Err(NanocodeError::AgentStopped);
                }
                let resolution = crate::prompt::ImageResolution::of(&state.model());
                let Some(policy) = state.policy.clone() else {
                    if request.request_id.is_some() {
                        return Err(unsupported(
                            "Claude request_id requires an attached durability policy",
                        ));
                    }
                    request.prompt =
                        crate::prompt::freeze(request.prompt, resolution, &state.images).await?;
                    let cancellation = Arc::new(Cancellation::default());
                    return Ok(start_turn(&state, request, resolution, None, cancellation).await);
                };
                let automatic = request.request_id.is_none();
                let candidate = request
                    .request_id
                    .clone()
                    .unwrap_or_else(|| durable::candidate_id("turn"));
                let input = json!({"provider":"claude","kind":"prompt","prompt":request.prompt});
                let (id, admission) = policy.admit(candidate, input, automatic).await?;

                let order = state.admission_order.fetch_add(1, Ordering::SeqCst);
                request.request_id = Some(id.clone());
                request.events = request.events.with_turn_id(id.clone());
                let mut terminal = match admission {
                    Admission::Completed { output, checkpoint } => Some(
                        durable::replay(id.clone(), output)
                            .map(|completed| state.replayed_boundary(completed, checkpoint)),
                    ),
                    Admission::Failed { error, .. } => {
                        Some(Err(NanocodeError::ReplayedExecutionFailed(error)))
                    }
                    Admission::Cancelled => Some(Err(NanocodeError::TurnCancelled)),
                    Admission::Execute | Admission::Resume => None,
                };
                let mut attempt = None;
                if terminal.is_none() {
                    match policy.begin_attempt(id.clone()).await {
                        Ok(()) => attempt = Some(AttemptGuard::begin(state.clone())),
                        Err(error) => {
                            let blocked = error.execution_policy_disposition()
                                == Some(nanocode_agent_core::ExecutionPolicyDisposition::Retry);

                            if blocked
                                && request.cancel_on_admission
                                && policy.cancel_unstarted(id.clone()).await.unwrap_or(false)
                            {
                                terminal = Some(Err(NanocodeError::TurnCancelled));
                            } else if blocked && state.local_blocker(order) {
                                return Ok(defer_turn(
                                    state.clone(),
                                    policy,
                                    request,
                                    resolution,
                                    id,
                                    order,
                                    automatic,
                                )
                                .await);
                            } else {
                                if automatic {
                                    retire_unstarted(policy.as_ref(), id).await;
                                } else {
                                    let _ = policy.release(id).await;
                                }
                                if blocked && state.life.stopped() {
                                    return Err(NanocodeError::AgentStopped);
                                }
                                return Err(error);
                            }
                        }
                    }
                }
                if let Some(result) = terminal {
                    state.accepted_turns.fetch_add(1, Ordering::SeqCst);
                    let (status, kind) = match &result {
                        Ok(_) => ("completed", AgentEventKind::RunCompleted),
                        Err(NanocodeError::TurnCancelled) => {
                            ("cancelled", AgentEventKind::RunFailed)
                        }
                        Err(_) => ("failed", AgentEventKind::RunFailed),
                    };

                    state.emit(
                        &request.events,
                        kind,
                        json!({
                            "status":status,"model":state.model(),
                            "effort":state.thinking().as_str(),
                            "transport":"messages_sse","orchestration":"claude",
                            "replayed":true,"model_calls":0,"tool_calls":0,
                            "duration_ms":0,"duration_ns":0,
                            "final_message":result.as_ref().ok().map(TurnResult::final_message),
                            "usage":result.as_ref().ok().and_then(TurnResult::usage),
                            "error":result.as_ref().err().map(ToString::to_string),
                            "estimated_cost":null,"cost_usd":null,"cost_status":"other"
                        }),
                    );
                    return Ok(BackendTurn {
                        request_id: Some(id),
                        result: Box::pin(async move { result }),
                    });
                }
                let (prompt, resolution) = match crate::prompt::freeze_admitted(
                    request.prompt,
                    policy.as_ref(),
                    &id,
                    resolution,
                    &state.images,
                )
                .await
                {
                    Ok(admitted) => admitted,
                    Err(error) => {
                        fail_begun(&state, policy.as_ref(), id, &error).await;
                        return Err(error);
                    }
                };
                request.prompt = prompt;
                let cancellation = Arc::new(Cancellation::default());
                Ok(start_turn(&state, request, resolution, attempt, cancellation).await)
            }
            .await;
            let _ = accepted.send(result);
        }
        .instrument(parent);
        spawn_task(Box::pin(task));
        receipt.await.unwrap_or(Err(NanocodeError::TurnStopped))
    })
}

pub(super) fn shutdown(this: &Driver) -> BackendFuture<Result<()>> {
    let state = this.state.clone();
    Box::pin(async move {
        let first_shutdown = state.life.close();
        state.kept.set(None);
        state.kept_changed.notify_waiters();
        if let Some(cancel) = state.compaction_cancel.get().as_ref() {
            cancel.cancel();
        }
        for cancel in state.cancellations.get().values() {
            cancel.stop();
        }
        let _admission = state.admission.enter().await;
        loop {
            let notified = state.idle.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            let cancels = state.cancellations.get();
            if cancels.is_empty() {
                break;
            }
            for cancel in cancels.values() {
                cancel.stop();
            }
            drop(cancels);
            notified.await;
        }
        let end_event = crate::ClaudeLifecycleEvent::SessionEnd {
            reason: "other".into(),
        };
        if first_shutdown
            && state
                .tool_hooks
                .iter()
                .any(|hook| hook.handles_lifecycle(&end_event))
        {
            let mut context = state.conversation.acquire().await;
            if let Some(opened) = Option::clone(&state.lifecycle_opened.get()) {
                let mut operation = None;
                let mut deliver = true;
                if let Some(policy) = &state.policy {
                    let (id, admission) = policy
                        .admit(
                            format!("claude-session-end-{opened}"),
                            json!({"provider":"claude","kind":"session_end"}),
                            false,
                        )
                        .await?;
                    deliver = matches!(admission, Admission::Execute | Admission::Resume);
                    if deliver {
                        policy.begin_attempt(id.clone()).await?;
                    }
                    operation = Some(id);
                }
                if deliver {
                    let invocation = crate::ClaudeLifecycleInvocation {
                        session_id: state.session_id.clone(),
                        root_session_id: state.lineage.root_session_id.clone(),
                        turn_id: operation
                            .clone()
                            .unwrap_or_else(|| durable::candidate_id("session-end")),
                        event_id: format!("{opened}:session-end"),
                        model: state.model(),
                        instruction_revision: None,
                        event: crate::ClaudeLifecycleEvent::SessionEnd {
                            reason: "other".into(),
                        },
                    };
                    let outcome = crate::hooks::run_lifecycle_hooks(
                        &state.tool_hooks,
                        &invocation,
                        state.policy.as_deref(),
                    )
                    .await?;
                    State::hook_context(&mut context, &outcome);
                    if let (Some(policy), Some(operation)) = (&state.policy, operation) {
                        policy
                            .complete(
                                operation,
                                serde_json::to_value(state.snapshot(&context).await?)
                                    .map_err(provider_error)?,
                                Value::Null,
                            )
                            .await?;
                    }
                }
            }
        }
        state.release_local().await
    })
}

pub(super) fn route(
    this: &Driver,
    request: BackendPrompt,
) -> BackendFuture<Result<BackendPromptRoute>> {
    let state = this.state.clone();
    let mut prompt = request.prompt.clone();
    let request_id = request.request_id.clone();
    let start = this.submit(request);
    Box::pin(async move {
        state.store_prompt_images(&mut prompt).await?;
        if state.life.stopped() {
            return Err(NanocodeError::AgentStopped);
        }
        let accepted = {
            let mut turns = state.steering.acquire().await;
            if let (Some(policy), Some(id)) = (&state.policy, &request_id)
                && policy
                    .identified_steer_accepted(
                        id.clone(),
                        serde_json::to_string(&prompt).map_err(provider_error)?,
                    )
                    .await?
            {
                return Ok(BackendPromptRoute::Steered);
            }
            let unfinished = match (&state.policy, &request_id) {
                (Some(policy), Some(id)) => policy.unfinished_operation(id.clone()).await?,
                _ => false,
            };
            if !unfinished
                && let Some((_, turn)) = turns
                    .iter_mut()
                    .filter(|(_, turn)| turn.accepting)
                    .min_by_key(|(key, _)| key.0)
            {
                Some(if state.accept_steer(turn, request_id, prompt).await? {
                    state.start_code_preempt().await
                } else {
                    Vec::new()
                })
            } else {
                None
            }
        };
        match accepted {
            Some(preempts) => {
                State::finish_code_preempt(preempts);
                Ok(BackendPromptRoute::Steered)
            }
            None => start.await.map(BackendPromptRoute::Started),
        }
    })
}

pub(super) fn cancel(this: &Driver, key: BackendTurnKey) -> BackendFuture<Result<()>> {
    let state = this.state.clone();
    Box::pin(async move {
        {
            let cancels = state.cancellations.get();
            cancels
                .get(&key)
                .ok_or(NanocodeError::TurnNotCancellable)?
                .cancel();
        }
        if let Some(turn) = state.steering.acquire().await.get_mut(&key) {
            turn.accepting = false;
        }
        if state.policy.is_some() {
            loop {
                let notified = state.idle.notified();
                let mut notified = std::pin::pin!(notified);
                notified.as_mut().enable();
                if !state.cancellations.get().contains_key(&key) {
                    break;
                }
                notified.await;
            }
            if state.life.stopped() {
                return Err(NanocodeError::ExecutionPolicyOwnerStopped);
            }
        }
        Ok(())
    })
}

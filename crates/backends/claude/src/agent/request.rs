use super::*;
use serde_json::json;

pub(super) fn request_response<'a>(
    state: &'a State,
    messages: &'a [HistoryMessage],
    tools: Option<Vec<ClaudeToolSpec>>,
    cancel: &'a Cancellation,
    events: Option<&'a AgentEventPublisher>,
    index: u32,
    context: ResponseContext<'a>,
) -> AgentFuture<'a, std::result::Result<ResponseOutcome, ResponseFailure>> {
    Box::pin(async move {
        let started = nanocode_platform::now();
        let elapsed_ns =
            || u64::try_from(nanocode_platform::elapsed(started).as_nanos()).unwrap_or(u64::MAX);
        let completed = |response: &crate::MessageResponse,
                         attempt: u32,
                         first_event: u64,
                         first_output: Option<u64>,
                         dispatch: Option<u64>,
                         mut timing: telemetry::CallTiming,
                         thread: Option<ThreadMode>,
                         hedge: HedgeOutcome,
                         speed: Option<crate::Speed>| {
            let Some(events) = events else { return };
            let duration_ns = elapsed_ns();
            timing.total_ns = duration_ns;
            telemetry::record_model_response(
                &nanocode_trace::Span::current(),
                response,
                &telemetry::ModelCallTiming {
                    attempt,
                    first_event_ns: first_event,
                    first_output_ns: first_output,
                    dispatch_ns: dispatch,
                },
            );

            let input_tokens = response
                .usage
                .input_tokens
                .saturating_add(response.usage.cache_read_input_tokens)
                .saturating_add(response.usage.cache_creation_input_tokens);
            state.emit(
                events,
                AgentEventKind::ModelCallCompleted,
                event::ModelCallCompleted {
                    call_index: index.saturating_add(1),
                    model: &response.model,
                    response_id: &response.id,
                    attempt,
                    connection_generation: 0,
                    status: response.stop_reason.unwrap_or(StopReason::Unknown),
                    duration_ns,
                    time_to_first_event_ns: first_event,
                    time_to_first_output_ns: first_output,
                    time_to_dispatch_ns: dispatch,
                    tool_calls: response
                        .content
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                        .count(),
                    usage: event::ModelCallUsage {
                        input_tokens,
                        input_tokens_details: event::InputTokensDetails {
                            cached_tokens: response.usage.cache_read_input_tokens,
                            cache_write_tokens: response.usage.cache_creation_input_tokens,
                        },
                        output_tokens: response.usage.output_tokens,
                        total_tokens: input_tokens.saturating_add(response.usage.output_tokens),
                    },
                    speed: match speed {
                        Some(crate::Speed::Fast) => "fast",
                        Some(crate::Speed::Standard) | None => "standard",
                    },
                    timing,
                    message_thread: thread.map(|mode| event::MessageThread {
                        mode: mode.as_str(),
                        create_reason: mode.create_reason().map(CreateReason::as_str),
                    }),
                    hedge,
                },
            );
        };
        let client = state
            .client
            .restore_wire_profile(context.wire_profile)
            .for_session(&state.session_id);
        let mut request = context
            .template
            .cloned()
            .unwrap_or_else(|| state.request_template(state.speed(), EffortPin::Unpinned));
        if let Some(tools) = tools {
            request.tools = tools;
        }
        let mut recovery = (!context.disable_tools
            && request
                .tools
                .iter()
                .any(|tool| matches!(tool, ClaudeToolSpec::Server(_))))
        .then(ServerRecovery::default);

        let mut suffix: Vec<HistoryMessage> = Vec::new();
        request.container = context.container.map(str::to_owned);
        if request.diagnostics.is_some() {
            request.diagnostics = Some(json!({"previous_message_id":context.previous_message_id}));
        }
        request.tool_choice = context.force_text.then(|| json!({"type":"none"}));

        let (cleared, floors) =
            state.cleared(wire_messages(messages, &suffix, request.effort_updates));
        let (mut sent, kept) = images::evict_images(
            request_images(&state.model, state.context_window_tokens),
            cleared,
        );
        let kept = if kept.iter().any(Option::is_some) {
            kept
        } else {
            floors
        };
        request
            .cache_floors_with(
                state.wire_cache.layout(&sent),
                &mut sent,
                kept,
                client.uses_subscription(),
                HistoryMessage::make_mut,
            )
            .map_err(provider_error)?;
        request
            .cache_prefix_with(state.wire_cache.layout(&sent), client.uses_subscription())
            .map_err(provider_error)?;
        telemetry::record_prompt(
            &nanocode_trace::Span::current(),
            &request,
            &sent,
            client.uses_subscription(),
            &state.wire_cache,
        );
        if cancel.is_cancelled() && context.effect.is_none() {
            return Err(NanocodeError::TurnCancelled.into());
        }
        let mut wire = client
            .prepare_cached_wire(&request, &sent, true, Some(&state.wire_cache))
            .map_err(provider_error)?;
        let needs_upgrade = state.code_only && !durable::is_code_only_catalog(&request.tools);

        let mut journal = context.journal;
        let mut preempt = context.preempt;
        let admitted = match &context.effect {
            Some(effect) => {
                let writes = journal
                    .as_deref_mut()
                    .map(Journal::take)
                    .unwrap_or_default();
                let advanced = writes
                    .iter()
                    .any(|write| matches!(write, crate::execution::JournalWrite::Advance(_)));
                let admitted = effect.begin_model(&wire, writes).await?;
                if advanced {
                    state.persistence_ready.notify_waiters();
                }
                admitted
            }
            None => Step::Execute,
        };
        let upgrade = match admitted {
            Step::Interrupted => {
                return Err(durable::recovery_error(
                    "model step was admitted with an interrupted tool outcome",
                )
                .into());
            }
            Step::Replay(value) if value.get("refused").is_some() => {
                return Err(
                    NanocodeError::Response(nanocode_agent_core::BackendError(Box::new(
                        crate::ClaudeError::Protocol(
                            value["refused"].as_str().unwrap_or_default().to_owned(),
                        ),
                    )))
                    .into(),
                );
            }
            Step::Replay(value) if value.get("code_only_tools").is_some() => Some(
                serde_json::from_value::<durable::CodeOnlyUpgrade>(value)
                    .map_err(durable::recovery_error)?,
            ),
            Step::Replay(value) => {
                let response = serde_json::from_value(value).map_err(durable::recovery_error)?;
                completed(
                    &response,
                    0,
                    0,
                    None,
                    None,
                    telemetry::CallTiming {
                        replayed: true,
                        ..telemetry::CallTiming::default()
                    },
                    None,
                    HedgeOutcome::Unneeded,
                    None,
                );
                return Ok(ResponseOutcome {
                    hedge: HedgeOutcome::Unneeded,
                    message: response,
                    recorded: None,
                    upgrade: None,
                    replayed: true,
                });
            }
            Step::Execute if needs_upgrade => {
                let upgrade =
                    durable::CodeOnlyUpgrade::new(state.code_only_tools(), context.disable_tools);
                if let Some(effect) = &context.effect {
                    effect
                        .complete(serde_json::to_value(&upgrade).map_err(provider_error)?)
                        .await?;
                }
                Some(upgrade)
            }
            Step::Execute => None,
        };

        let replacement_effect = upgrade.as_ref().and_then(|_| {
            context
                .effect
                .as_ref()
                .map(|effect| effect.scoped("code-only"))
        });
        let active_effect = replacement_effect.as_ref().or(context.effect.as_ref());
        if let Some(upgrade) = &upgrade {
            request.tools = upgrade.code_only_tools.clone();
            suffix.push(Message::text(Role::User, &upgrade.notice).into());
            let (cleared, floors) =
                state.cleared(wire_messages(messages, &suffix, request.effort_updates));
            let kept;
            (sent, kept) = images::evict_images(
                request_images(&state.model, state.context_window_tokens),
                cleared,
            );
            let kept = if kept.iter().any(Option::is_some) {
                kept
            } else {
                floors
            };
            request
                .cache_floors_with(
                    state.wire_cache.layout(&sent),
                    &mut sent,
                    kept,
                    client.uses_subscription(),
                    HistoryMessage::make_mut,
                )
                .map_err(provider_error)?;
            wire = client
                .prepare_cached_wire(&request, &sent, true, Some(&state.wire_cache))
                .map_err(provider_error)?;

            recovery = Some(ServerRecovery {
                retirement_notice: Some(upgrade.notice.clone()),
                ..ServerRecovery::default()
            });
            if let Some(effect) = &replacement_effect
                && let Step::Replay(value) = effect.begin_model(&wire, Vec::new()).await?
            {
                let response = serde_json::from_value(value).map_err(durable::recovery_error)?;
                completed(
                    &response,
                    0,
                    0,
                    None,
                    None,
                    telemetry::CallTiming {
                        replayed: true,
                        ..telemetry::CallTiming::default()
                    },
                    None,
                    HedgeOutcome::Unneeded,
                    None,
                );
                return Ok(ResponseOutcome {
                    hedge: HedgeOutcome::Unneeded,
                    message: response,
                    recorded: None,
                    upgrade: Some(upgrade.clone()),
                    replayed: true,
                });
            }
        }

        let inlined = images::inline_images(&state.images, &sent).await?;
        if let Some(inlined) = &inlined {
            wire = client
                .prepare_cached_wire(&request, inlined, true, Some(&state.wire_cache))
                .map_err(provider_error)?;
        }
        let request = Arc::new(request);
        let sent = Arc::new(sent);
        let outgoing = inlined.as_deref().unwrap_or(&sent);
        let thread = state
            .message_thread
            .as_ref()
            .filter(|_| context.threaded && !client.uses_subscription());
        let max_attempts = if context.disable_tools { 3 } else { 11 };
        let mut attempt = 0;
        let mut waited = Duration::ZERO;
        let mut stalled = false;
        let mut dispatched: Option<u64> = None;
        let mut shown: Option<String> = None;
        loop {
            if cancel.is_cancelled() {
                return Err(ResponseFailure {
                    error: NanocodeError::TurnCancelled,
                    recovery: if upgrade.is_some() { recovery } else { None },
                });
            }
            attempt += 1;
            let attempt_span =
                telemetry::attempt_span(index.saturating_add(1), attempt, max_attempts);
            let attempt_started = nanocode_platform::now();
            let attempt_started_ns = elapsed_ns();
            let mut accepted = false;
            let mut first_event = None;
            let mut first_output = None;
            let mut response_headers_ns = None;
            let mut send_ns = None;
            let mut response_completed_ns = None;
            let mut pipeline = telemetry::CallPipeline::default();

            let mut message_id: Option<String> = None;

            let turn = match thread {
                Some(thread) => {
                    let turn = thread
                        .read(|thread| thread.plan(&request, outgoing, nanocode_platform::now()))
                        .map_err(provider_error)?;
                    client
                        .attach_thread(&mut wire, &request, outgoing, &turn)
                        .map_err(provider_error)?;
                    if let ThreadMode::Create(reason) = turn.mode() {
                        nanocode_trace::debug!(
                            target: "nanocode_claude",
                            session_id = %state.session_id,
                            model_call_index = index.saturating_add(1),
                            reason = reason.as_str(),
                            "Claude sends the whole conversation to create a message thread"
                        );
                    }
                    Some(turn)
                }
                None => None,
            };
            dispatched.get_or_insert_with(&elapsed_ns);

            let parked = active_effect
                .filter(|_| {
                    state.parking.enabled() && matches!(state.lineage.origin, Origin::Subagent)
                })
                .and_then(|effect| {
                    let request = wire.durable_input().ok()?;
                    let (operation, step) = effect.key();
                    let key = parked::Key {
                        operation: operation.to_owned(),
                        step: step.to_owned(),
                        request: request.get().into(),
                    };
                    state.parking.enter(key, |key| {
                        parked::ModelCall::start(
                            client.clone(),
                            wire.clone(),
                            key,
                            attempt_span.clone(),
                        )
                    })
                });
            let hedging = state.hedging.as_deref().filter(|_| turn.is_none());
            let opening = async {
                match (&parked, hedging) {
                    (Some((call, _)), _) => (
                        HedgeOutcome::Unneeded,
                        call.opened().await.map(|()| call.stream()),
                    ),
                    (None, Some(hedging)) => {
                        let permit = client.admit().await;
                        let hedged = hedging
                            .race_when(
                                &request.model,
                                client.stream_admitted(&wire, permit),
                                || {
                                    client
                                        .try_admit()
                                        .map(|permit| client.stream_hedge(&wire, permit))
                                },
                            )
                            .await;
                        (hedged.outcome, hedged.result)
                    }
                    (None, None) => (HedgeOutcome::Unneeded, client.stream_prepared(&wire).await),
                }
            };
            let (hedge, opened) = nanocode_sync::select! {
                () = cancel.cancelled() => {
                    telemetry::cancel_attempt(&attempt_span);
                    return Err(ResponseFailure { error: NanocodeError::TurnCancelled, recovery });
                }
                () = steered(preempt.as_deref_mut().filter(|_| preemptible(recovery.as_ref()))) => {
                    telemetry::cancel_attempt(&attempt_span);
                    return Err(ResponseFailure { error: NanocodeError::TurnCancelled, recovery: None });
                }
                result = opening.instrument(attempt_span.clone()) => result,
            };
            telemetry::record_hedge(&attempt_span, hedge);
            if opened.is_ok() {
                response_headers_ns = Some(elapsed_ns());
                send_ns = Some(telemetry::elapsed_ns(attempt_started));
                state.requested.set(Some(attempt_started));
                state
                    .requested_at
                    .store(nanocode_platform::unix_millis() / 1000, Ordering::SeqCst);
                if events.is_some() && request.cache_control.is_some() && turn.is_none() {
                    state.keep(keep::KeptRequest {
                        request: Arc::clone(&request),
                        sent: Arc::clone(&sent),
                        wire_profile: context.wire_profile.cloned(),
                        origin: context.origin.clone(),
                        reach: nanocode_platform::elapsed(attempt_started),
                    });
                }
            }
            let result = match opened {
                Err(error) => Err(error),
                Ok(mut stream) => {
                    accepted = true;
                    let cancelled = cancel.cancelled();
                    let mut cancelled = std::pin::pin!(cancelled);
                    let mut accumulator: Option<MessageAccumulator> = None;
                    let mut thinking_item: Option<(usize, String)> = None;
                    let mut tool_blocks: HashMap<usize, (String, String)> = HashMap::new();
                    loop {
                        let receive_started = nanocode_platform::now();
                        let mut next = nanocode_sync::select! {
                            () = &mut cancelled => {
                                telemetry::cancel_attempt(&attempt_span);
                                return Err(ResponseFailure { error: NanocodeError::TurnCancelled, recovery });
                            }
                            () = steered(preempt.as_deref_mut().filter(|_| preemptible(recovery.as_ref()))) => {
                                telemetry::cancel_attempt(&attempt_span);
                                return Err(ResponseFailure { error: NanocodeError::TurnCancelled, recovery: None });
                            }
                            event = stream.next().instrument(attempt_span.clone()) => event,
                        };
                        pipeline.receive_decode_ns = pipeline
                            .receive_decode_ns
                            .saturating_add(telemetry::elapsed_ns(receive_started));
                        let ended = loop {
                            let event = match next {
                                Some(Ok(event)) => event,
                                Some(Err(error)) => break Some(Err(error)),
                                None => break Some(Err(ClaudeError::IncompleteStream)),
                            };
                            pipeline.event_count = pipeline.event_count.saturating_add(1);
                            if first_event.is_none() {
                                first_event = Some(elapsed_ns());
                            }
                            if let Some(recovery) = &mut recovery {
                                recovery.observe(&event);
                            }
                            if let StreamEvent::MessageStart { message } = &event {
                                message_id =
                                    Some(shown.get_or_insert_with(|| message.id.clone()).clone());
                            }
                            if let StreamEvent::ContentBlockStart {
                                index: block,
                                content_block: ContentBlock::ToolUse { id, name, .. },
                            } = &event
                                && state.tool_input_deltas
                            {
                                tool_blocks.insert(*block, (id.clone(), name.clone()));
                            }
                            let shown = match &event {
                                StreamEvent::ContentBlockStart {
                                    index: block,
                                    content_block: ContentBlock::Thinking { thinking, .. },
                                } if !thinking.is_empty() => {
                                    Some((AgentEventKind::ReasoningSummaryDelta, *block, thinking))
                                }
                                StreamEvent::ContentBlockDelta {
                                    index: block,
                                    delta: ContentDelta::TextDelta { text },
                                } if !text.is_empty() => {
                                    Some((AgentEventKind::AssistantDelta, *block, text))
                                }
                                StreamEvent::ContentBlockDelta {
                                    index: block,
                                    delta: ContentDelta::ThinkingDelta { thinking },
                                } if !thinking.is_empty() => {
                                    Some((AgentEventKind::ReasoningSummaryDelta, *block, thinking))
                                }
                                StreamEvent::ContentBlockDelta {
                                    index: block,
                                    delta: ContentDelta::InputJsonDelta { partial_json },
                                } if !partial_json.is_empty()
                                    && tool_blocks.contains_key(block) =>
                                {
                                    Some((AgentEventKind::ToolInputDelta, *block, partial_json))
                                }
                                _ => None,
                            };
                            if let Some((kind, block, text)) = shown {
                                let model_elapsed_ns = elapsed_ns();
                                first_output.get_or_insert(model_elapsed_ns);
                                if let Some(events) = events {
                                    if kind == AgentEventKind::ReasoningSummaryDelta
                                        && let Some(id) = &message_id
                                        && thinking_item
                                            .as_ref()
                                            .is_none_or(|(cached, _)| *cached != block)
                                    {
                                        thinking_item =
                                            Some((block, format!("{id}:thinking:{block}")));
                                    }
                                    let tool = (kind == AgentEventKind::ToolInputDelta)
                                        .then(|| tool_blocks.get(&block))
                                        .flatten();
                                    let item_id = match (kind, &thinking_item) {
                                        (AgentEventKind::ToolInputDelta, _) => None,
                                        (
                                            AgentEventKind::ReasoningSummaryDelta,
                                            Some((_, item)),
                                        ) => Some(item.as_str()),
                                        _ => message_id.as_deref(),
                                    };
                                    let emit_started = nanocode_platform::now();
                                    let _ = events.emit_text_delta(
                                        kind,
                                        TextDeltaRef {
                                            model_call_index: index.saturating_add(1),
                                            item_id: item_id.map(std::borrow::Cow::Borrowed),
                                            call_id: tool.map(|(id, _)| {
                                                std::borrow::Cow::Borrowed(id.as_str())
                                            }),
                                            tool: tool.map(|(_, name)| {
                                                std::borrow::Cow::Borrowed(name.as_str())
                                            }),
                                            phase: None,
                                            text: std::borrow::Cow::Borrowed(text),
                                            model_elapsed_ns: Some(model_elapsed_ns),
                                        },
                                        None,
                                    );
                                    pipeline.display_delta_emit_ns = pipeline
                                        .display_delta_emit_ns
                                        .saturating_add(telemetry::elapsed_ns(emit_started));
                                    pipeline.display_delta_count =
                                        pipeline.display_delta_count.saturating_add(1);
                                    pipeline.display_delta_bytes =
                                        pipeline.display_delta_bytes.saturating_add(
                                            u64::try_from(text.len()).unwrap_or(u64::MAX),
                                        );
                                }
                            }
                            let pushed = match &mut accumulator {
                                Some(accumulator) => accumulator.push(event),
                                None => MessageAccumulator::new(event).map(|started| {
                                    accumulator = Some(started);
                                    false
                                }),
                            };
                            match pushed {
                                Err(error) => break Some(Err(error)),
                                Ok(true) => {
                                    response_completed_ns = Some(elapsed_ns());
                                    let collect_started = nanocode_platform::now();
                                    let collected = accumulator.take().map_or(
                                        Err(ClaudeError::IncompleteStream),
                                        MessageAccumulator::finish,
                                    );
                                    pipeline.collect_ns = telemetry::elapsed_ns(collect_started);
                                    break Some(collected);
                                }
                                Ok(false) => {}
                            }
                            let receive_started = nanocode_platform::now();
                            let ready = stream.next().now_or_never();
                            pipeline.receive_decode_ns = pipeline
                                .receive_decode_ns
                                .saturating_add(telemetry::elapsed_ns(receive_started));
                            match ready {
                                Some(ready) => next = ready,
                                None => break None,
                            }
                        };
                        if let Some(events) = events {
                            let flush_started = nanocode_platform::now();
                            let _ = events.flush_deltas();
                            pipeline.display_delta_emit_ns = pipeline
                                .display_delta_emit_ns
                                .saturating_add(telemetry::elapsed_ns(flush_started));
                        }
                        if let Some(ended) = ended {
                            break ended;
                        }
                    }
                }
            };
            if let Some((_, parked)) = parked
                && !parked.leave()
            {
                cancel.cancelled().await;
                telemetry::cancel_attempt(&attempt_span);
                return Err(ResponseFailure {
                    error: NanocodeError::TurnCancelled,
                    recovery,
                });
            }
            telemetry::record_attempt(
                &attempt_span,
                &telemetry::AttemptTiming {
                    accepted,
                    first_event_ns: first_event,
                    first_output_ns: first_output,
                    duration_ns: u64::try_from(
                        nanocode_platform::elapsed(attempt_started).as_nanos(),
                    )
                    .unwrap_or(u64::MAX),
                },
            );
            let error = match result {
                Ok(mut response) => {
                    telemetry::complete_attempt(&attempt_span);
                    let blank = |block: &ContentBlock| matches!(block, ContentBlock::Text { text, .. } if text.trim().is_empty());
                    if !response.content.iter().all(blank) {
                        response.content.retain(|block| !blank(block));
                    }
                    if state.upstream_context {
                        upstream_tool_defaults(&mut response.content);
                    }
                    if let (Some(thread), Some(turn)) = (thread, &turn) {
                        let completed_at = nanocode_platform::now();
                        thread
                            .modify(|thread| thread.complete(turn, &response, completed_at))
                            .map_err(provider_error)?;
                    }
                    if let Some(shown) = shown.take() {
                        response.id = shown;
                    }
                    let mut recorded = None;
                    state
                        .client
                        .clone()
                        .for_session(&state.session_id)
                        .clear_root_region();
                    if let Some(effect) = active_effect {
                        let completion;
                        (completion, recorded) =
                            effect.model_completion(&mut response, state.pages_messages())?;
                        match journal.as_deref_mut() {
                            Some(journal) => journal.complete(completion),
                            None => state.policy_writes(effect, vec![completion]).await?,
                        }
                    }
                    completed(
                        &response,
                        attempt,
                        first_event.unwrap_or_default(),
                        first_output,
                        dispatched,
                        telemetry::CallTiming {
                            replayed: false,
                            preparation_ns: dispatched,
                            attempt_started_ns: Some(attempt_started_ns),
                            send_ns,
                            response_headers_ns,
                            first_event_ns: first_event,
                            first_output_ns: first_output,
                            response_completed_ns,
                            total_ns: 0,
                            pipeline: Some(pipeline),
                        },
                        turn.as_ref().map(ThreadTurn::mode),
                        hedge,
                        request.speed,
                    );
                    return Ok(ResponseOutcome {
                        hedge,
                        message: response,
                        recorded,
                        upgrade,
                        replayed: false,
                    });
                }
                Err(error) => error,
            };
            if let (Some(thread), Some(turn)) = (thread, &turn)
                && thread.update(|thread| thread.recover(turn, &error)) == ThreadRecovery::Resend
            {
                telemetry::fail_attempt(&attempt_span, &error, Some(0), false);
                nanocode_trace::debug!(
                    target: "nanocode_claude",
                    session_id = %state.session_id,
                    model_call_index = index.saturating_add(1),
                    %error,
                    "Claude could not use the conversation's message thread; sending the request again"
                );
                attempt -= 1;
                continue;
            }

            let patient = !accepted
                && !context.disable_tools
                && matches!(&error, ClaudeError::Http { status, .. } if retry::patient_status(*status));
            let uncertain = !patient
                && (accepted
                    || matches!(&error, ClaudeError::Transport(error) if !error.is_unsent())
                    || matches!(
                        &error,
                        ClaudeError::StreamError { .. } | ClaudeError::IncompleteStream
                    )
                    || matches!(&error, ClaudeError::Http { status, .. } if *status >= 500));
            let retry_after = match &error {
                ClaudeError::Http { retry_after, .. } => *retry_after,
                _ => None,
            };
            let jitter = 100
                + (u64::from(index) * 31 + u64::from(attempt) * 17) % (RETRY_JITTER_PERCENT + 1);
            let backoff = Duration::from_millis(
                RETRY_BASE_MS
                    .saturating_mul(2_u64.saturating_pow(attempt - 1))
                    .min(RETRY_CAP_MS)
                    * jitter
                    / 100,
            );
            let delay = retry_after.map_or(backoff, |delay| delay.max(backoff));
            let throttled = error.is_throttled() && !context.disable_tools;
            let attempts = if throttled {
                THROTTLED_ATTEMPTS
            } else {
                max_attempts
            };

            let stall = accepted
                && matches!(
                    &error,
                    ClaudeError::Transport(crate::transport::HttpError::Transport {
                        timeout: true,
                        unsent: false,
                        ..
                    })
                );
            let stalled_again = stall && stalled;
            stalled |= stall;
            let delay = if patient {
                retry::patient_delay(delay, waited, attempts.saturating_sub(attempt))
            } else {
                delay
            };
            let exhausted = if patient {
                attempt >= attempts || waited + delay > retry::PATIENCE
            } else {
                attempt >= attempts
                    || delay > Duration::from_secs(60)
                    || (throttled && waited + delay > THROTTLED_WAIT)
                    || stalled_again
            };

            if exhausted || !error.is_transient() || (uncertain && recovery.is_some()) {
                telemetry::fail_attempt(&attempt_span, &error, None, retry_after.is_some());
                let error = match error {
                    ClaudeError::Transport(crate::transport::HttpError::Transport {
                        detail,
                        unsent,
                        ..
                    }) if stalled_again => {
                        ClaudeError::Transport(crate::transport::HttpError::Transport {
                            detail: format!("{detail}, twice in a row"),
                            retryable: false,
                            timeout: false,
                            unsent,
                        })
                    }
                    error => error,
                };
                let error =
                    NanocodeError::Response(nanocode_agent_core::BackendError(Box::new(error)));
                if (unsigned_thinking(&error)
                    || request_too_large(&error)
                    || (refused_summary(&error)
                        && (request.compaction.is_some()
                            || error.to_string().contains("compaction")))
                    || (refused_tool_changes(&error)
                        && sent
                            .iter()
                            .flat_map(|message| &message.content)
                            .any(tool_change)))
                    && let Some(effect) = active_effect
                {
                    effect.refuse(&error).await?;
                }
                return Err(ResponseFailure {
                    error,
                    recovery: if uncertain || upgrade.is_some() {
                        recovery
                    } else {
                        None
                    },
                });
            }
            let delay_ns = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
            telemetry::fail_attempt(&attempt_span, &error, Some(delay_ns), retry_after.is_some());
            drop(attempt_span);
            if let Some(events) = events {
                state.emit(
                    events,
                    AgentEventKind::ModelAttemptRetrying,
                    json!({
                        "model_call_index": index.saturating_add(1),
                        "error_class": telemetry::retry_class(&error),
                        "attempt": attempt,
                        "next_attempt": attempt + 1,
                        "max_attempts": attempts,
                        "delay_ns": delay_ns,
                        "server_requested_delay": retry_after.is_some(),
                        "throttled": throttled,
                        "error": error.to_string(),
                    }),
                );
            }
            waited += delay;

            nanocode_sync::select! {
                () = cancel.cancelled() => {}
                () = sleep(delay) => {}
            }
        }
    })
}

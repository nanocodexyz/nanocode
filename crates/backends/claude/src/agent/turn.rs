use super::*;
use serde_json::json;

pub(super) fn call_tool<'a>(
    state: &'a State,
    id: &'a str,
    name: &'a str,
    input: &'a Value,
    handler: &'a Handler,
    events: &'a AgentEventPublisher,
    control: (&'a Cursor, &'a Cancellation, Option<ClaudeToolReply>),
) -> AgentFuture<'a, Result<ContentBlock>> {
    Box::pin(async move {
        let (cursor, cancel, recovered) = control;
        let index = cursor.index;
        state.emit(
            events,
            AgentEventKind::ToolCall,
            event::ToolCall {
                call_id: id,
                tool: name,
                arguments: std::borrow::Cow::Borrowed(input),
                model_call_index: index,
                parent_call_id: None,
                truncated: None,
                original_bytes: None,
            },
        );
        let began = nanocode_platform::now();
        let mut started = StartedToolCall {
            state,
            events,
            id,
            name,
            began,
            open: true,
        };
        let invocation = state.invocation(id, events, cursor);

        let mut live = LiveNestedCalls {
            state,
            events,
            model_call_index: index,
            fallback_parent: id,
            open: Vec::new(),
            completed: HashSet::new(),
            unknown: Vec::new(),
        };
        let mut stopped = false;
        let outcome = if let Some(reply) = recovered {
            Ok(reply)
        } else if matches!(name, "exec" | "wait") {
            let (sender, mut updates) = nanocode_sync::mpsc::unbounded_channel();
            let stopper = sender.clone();
            let mut invocation = invocation;
            invocation.progress = Some(ClaudeToolProgress(sender));
            let mut call = handler(input.clone(), invocation);
            let outcome = loop {
                nanocode_sync::select! {
                    Some(update) = updates.recv() => live.publish(update),
                    outcome = &mut call => break Some(outcome),
                    () = cancel.cancelled() => {
                        if cancel.stopped() {
                            return Ok(ContentBlock::tool_result_content(
                                id,
                                ToolResultContent::Text(StartedToolCall::session_stopped(nanocode_platform::elapsed(began))),
                                true,
                            ));
                        }
                        break None;
                    }
                }
            };
            while let Ok(update) = updates.try_recv() {
                live.publish(update);
            }
            match outcome {
                Some(outcome) => outcome,
                None => {
                    drop(call);
                    stopped = true;
                    let mut stopped_unknown = false;
                    let stop = state.stop_code_cells(&stopper);
                    drop(stopper);
                    let mut stop = std::pin::pin!(stop);
                    let mut partial = Vec::new();
                    let mut collect = |update: &ClaudeNestedToolUpdate| {
                        if let ClaudeNestedToolUpdate::Terminated { call_id, output } = update {
                            partial.push((call_id.clone(), output.clone()));
                        }
                    };
                    loop {
                        nanocode_sync::select! {
                            Some(update) = updates.recv() => {
                                stopped_unknown |= update.outcome_unknown();
                                collect(&update);
                                live.publish(update);
                            }
                            () = &mut stop => break,
                        }
                    }
                    while let Ok(update) = updates.try_recv() {
                        stopped_unknown |= update.outcome_unknown();
                        collect(&update);
                        live.publish(update);
                    }
                    stopped_unknown |= !live.open.is_empty();
                    let mut text = String::from(if stopped_unknown {
                        StartedToolCall::STOPPED_UNKNOWN
                    } else {
                        StartedToolCall::STOPPED
                    });
                    let own = partial
                        .iter()
                        .position(|(origin, _)| origin == id)
                        .or_else(|| (name == "wait" && !partial.is_empty()).then_some(0));
                    if let Some(index) = own {
                        text.push_str(" Output before it stopped:\n");
                        text.push_str(&partial[index].1);
                    }
                    Ok(ClaudeToolReply::success(ToolResultContent::Text(text)))
                }
            }
        } else {
            handler(input.clone(), invocation).await
        };
        let (content, is_error, metadata, structured_result) = match outcome {
            Ok(reply) => (
                reply.content,
                reply.is_error,
                reply.metadata,
                reply.structured_result,
            ),
            Err(reason) if reason == ClaudeTools::HOST_INTERRUPTED => {
                live.settle_unknown();
                return Err(durable::recovery_error(
                    "Claude tool host execution interrupted",
                ));
            }
            Err(reason) => {
                live.settle_unknown();
                (ToolResultContent::Text(reason), true, None, None)
            }
        };

        started.open = false;
        let is_error = is_error || stopped;

        if matches!(name, "exec" | "wait")
            && let Some(code) = metadata
                .as_ref()
                .and_then(|value| value.get("_nanocode_code"))
            && let Some(calls) = code.get("calls").and_then(Value::as_array)
        {
            let parent = json!(code.get("origin_call_id"));
            for call in calls {
                if call
                    .get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|call_id| live.completed.contains(call_id))
                {
                    continue;
                }
                state.publish_nested_receipt(events, index, call, &parent);
            }
        }

        if metadata
            .as_ref()
            .and_then(|value| value.get("_nanocode_code"))
            .is_some_and(|code| code.get("running").and_then(Value::as_bool) != Some(false))
        {
            live.release_running();
        }
        live.settle_unknown();
        if matches!(name, "exec" | "wait") {
            let mut calls = metadata
                .as_ref()
                .and_then(|value| value.get("_nanocode_code"))
                .and_then(|code| code.get("calls"))
                .and_then(Value::as_array)
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(nested_call_summary)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            calls.append(&mut live.unknown);
            if !calls.is_empty() {
                state.code_call_summaries.update(|summaries| {
                    summaries.insert(id.to_owned(), calls.clone());
                });
            }
        }
        let mut content = content;
        images::prepare_tool_images(
            &mut content,
            crate::prompt::ImageResolution::of(&cursor.template.model),
            &state.images,
        )
        .await?;
        let (content, original_bytes) = match name {
            "Read" => (content, None),
            "exec" | "wait" => bound_tool_result(
                content,
                state
                    .tool_result_tokens
                    .map_or(TOOL_RESULT_TOKEN_LIMIT, std::num::NonZeroU64::get),
            ),
            _ => bound_tool_result(content, DIRECT_TOOL_RESULT_TOKEN_LIMIT),
        };
        state.emit(
            events,
            AgentEventKind::ToolResult,
            event::ToolResult::new(
                event::Call {
                    call_id: id,
                    tool: name,
                    status: if stopped {
                        "cancelled"
                    } else if is_error {
                        "failed"
                    } else {
                        "completed"
                    },
                    duration_ns: nanocode_platform::elapsed(began)
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64,
                },
                &content,
                original_bytes,
                structured_result.as_ref(),
                metadata.as_ref(),
            ),
        );
        Ok(ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content,
            is_error: (is_error || name == "Bash").then_some(is_error),
            extra: std::collections::BTreeMap::new(),
        })
    })
}

pub(super) fn summarize_locked<'a>(
    state: &'a State,
    context: &'a mut Conversation,
    cancel: &'a Cancellation,
    mode: CompactionMode,
    cursor: &'a Cursor,
    step: &'a str,
) -> AgentFuture<'a, Result<Usage>> {
    Box::pin(async move {
        let summarized_notices = context.recovery_notices.len();
        let mut messages = context.packed_messages();
        if messages.is_empty() {
            return Err(unsupported("Claude cannot compact empty history"));
        }
        let trigger = match mode {
            CompactionMode::Manual => "manual",
            _ => "auto",
        };
        let outcome = state
            .lifecycle(
                cursor,
                cancel,
                &format!("{step}-pre"),
                crate::ClaudeLifecycleEvent::PreCompact {
                    trigger: trigger.into(),
                    custom_instructions: String::new(),
                },
            )
            .await?;
        State::hook_context(context, &outcome);
        match outcome.decision {
            crate::ClaudeLifecycleDecision::Block(reason)
            | crate::ClaudeLifecycleDecision::Stop(reason) => {
                return Err(unsupported(&format!(
                    "PreCompact hook blocked compaction: {reason}"
                )));
            }
            crate::ClaudeLifecycleDecision::Continue => {}
        }

        let mut retained = if context.pending_continuation {
            let start = unfinished_server_turn_start(&messages)
                .or_else(|| current_server_turn_start(&messages))
                .or_else(|| {
                    messages
                        .iter()
                        .rposition(|message| message.role() == Role::Assistant)
                })
                .unwrap_or(messages.len());
            messages.split_off(start)
        } else {
            Vec::new()
        };
        let retained_count = retained.len();
        let tools = cursor.template.tools.clone();
        let mut template = cursor.template.clone();
        if matches!(mode, CompactionMode::ContextRecovery) {
            template.effort_updates = false;
            match recovery_thinking(&template.model) {
                RecoveryThinking::Disabled => {
                    template.thinking = Some(json!({"type":"disabled"}));
                    template.output_config = None;
                }
                RecoveryThinking::AdaptiveLow => {
                    template.thinking = Some(json!({
                        "type":"adaptive",
                        "block_binding":{"prefix_mismatch_behavior":"drop_block"},
                    }));
                    template.output_config = Some(crate::OutputConfig {
                        effort: crate::Effort::Low,
                    });
                }
                RecoveryThinking::BetweenTools => {
                    template.thinking = Some(json!({"type":"between_tools"}));
                    template.output_config = Some(crate::OutputConfig {
                        effort: crate::Effort::Low,
                    });
                }
            }
        }
        let served = if matches!(mode, CompactionMode::ContextRecovery)
            || state.summary_refused.load(Ordering::SeqCst)
            || state.holds_back_formats()
            || template
                .model
                .parse::<ModelSelection>()
                .ok()
                .and_then(|model| model.as_claude())
                .is_none()
        {
            None
        } else {
            state
                .summarized_by_server(context, &messages, &tools, cancel, cursor, step, &template)
                .await?
        };
        if state.summary_refused.load(Ordering::SeqCst) {
            plain_summaries(&mut messages);
            plain_summaries(&mut retained);
            plain_summaries(&mut context.messages);
        }
        let (response, kept, continued, summary) = match served {
            Some((response, block)) => {
                let summary = match &block {
                    ContentBlock::Compaction { content, .. } => content.clone(),
                    _ => String::new(),
                };
                let mut content = vec![block];
                if !matches!(mode, CompactionMode::Manual) {
                    content.push(ContentBlock::text(COMPACT_RESUME));
                }
                if !retained.is_empty() {
                    content.push(ContentBlock::text(COMPACT_KEPT));
                }
                let mut kept = vec![HistoryMessage::from(Message {
                    role: Role::User,
                    content,
                    output_config: None,
                })];
                kept.append(&mut retained);
                (response, kept, String::new(), summary)
            }
            None => {
                history::media_placeholders(&mut messages);
                messages.push(Message::text(Role::User, COMPACTION_INSTRUCTIONS).into());
                let forced = format!("{step}-text");
                let mut response = None;
                let mut stripped_thinking = false;
                for (force_text, step) in [(false, step), (true, forced.as_str())] {
                    let outcome = loop {
                        let effect = cursor.effect(state, step);
                        let outcome = state
                            .response(
                                &messages,
                                Some(tools.clone()),
                                cancel,
                                None,
                                0,
                                ResponseContext {
                                    origin: cursor.host_context(state),
                                    disable_tools: true,
                                    force_text,
                                    container: context.container.as_deref(),
                                    previous_message_id: context.previous_message_id.as_deref(),
                                    template: Some(&template),
                                    wire_profile: cursor.wire_profile.as_ref(),
                                    effect: if stripped_thinking {
                                        effect.map(|effect| effect.scoped("unsigned-thinking"))
                                    } else {
                                        effect
                                    },
                                    journal: None,
                                    threaded: false,
                                    preempt: None,
                                },
                            )
                            .await;
                        match outcome {
                            Err(failure)
                                if !stripped_thinking && unsigned_thinking(&failure.error) =>
                            {
                                history::strip_thinking(&mut messages);
                                history::strip_thinking(&mut retained);
                                history::strip_thinking(&mut context.messages);
                                stripped_thinking = true;
                            }
                            outcome => break outcome.map_err(|failure| failure.error)?,
                        }
                    };
                    if let Some(upgrade) = outcome.upgrade {
                        context.recovery_notices.push(upgrade.notice);
                    }
                    let calls_tools = outcome
                        .message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
                    response = Some(outcome.message);
                    if !calls_tools {
                        break;
                    }
                }
                let response =
                    response.ok_or_else(|| provider_error("compaction made no request"))?;
                if response.stop_reason != Some(StopReason::EndTurn)
                    || response.role != Role::Assistant
                {
                    return Err(provider_error("compaction summary did not end normally"));
                }
                let summary = response
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text, .. } => Ok(Some(text.as_str())),
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                            Ok(None)
                        }
                        _ => Err(provider_error("compaction returned a tool block")),
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("");
                if summary.trim().is_empty() {
                    return Err(provider_error("compaction returned empty summary"));
                }
                let mut continued = format!(
                    "{COMPACT_TOOLS_OPEN}{}{COMPACT_TOOLS_CLOSE}\n{}",
                    tools_used(&messages),
                    compact_summary(&summary)
                );
                if !matches!(mode, CompactionMode::Manual) {
                    continued.push('\n');
                    continued.push_str(COMPACT_RESUME);
                }
                if !retained.is_empty() {
                    continued.push_str("\n\n");
                    continued.push_str(COMPACT_KEPT);
                }
                (response, retained, continued, summary)
            }
        };

        if cursor.tool_search {
            let references = client_discovered_tools(&kept);
            state
                .discovered
                .update(|discovered| discovered.retain(|name| references.contains(name.as_str())));
        }

        state.kept.set(None);
        state.kept_changed.notify_waiters();
        let replaced = std::mem::replace(&mut context.messages, kept);
        let dropped = replaced.len().saturating_sub(retained_count);
        context.compacted.extend(
            replaced
                .into_iter()
                .take(dropped)
                .filter(|message| !summary_message(message)),
        );
        context.previous_message_id = Some(response.id);
        context.summary = continued;
        context.recovery_notices.drain(..summarized_notices);

        let packed = json!({
            "system": cursor.template.system,
            "tools": tools,
            "messages": context.packed_messages(),
        });
        context.active_context_tokens = estimate_text_tokens(&packed.to_string());
        context.auto_compaction_suppressed = true;

        context.rapid_compactions = match mode {
            CompactionMode::Automatic if context.rounds_since_compaction < 3 => {
                context.rapid_compactions.saturating_add(1)
            }
            CompactionMode::Automatic | CompactionMode::ContextRecovery => 1,
            CompactionMode::Manual => 0,
        };
        context.rounds_since_compaction = 0;
        let outcome = state
            .lifecycle(
                cursor,
                cancel,
                &format!("{step}-post"),
                crate::ClaudeLifecycleEvent::PostCompact {
                    trigger: trigger.into(),
                    compact_summary: summary,
                },
            )
            .await?;
        State::hook_context(context, &outcome);
        Ok(response.usage)
    })
}

pub(super) fn carry_steer<'a>(
    state: &'a State,
    turn: &'a mut TurnSteering,
    policy: &'a dyn ClaudeExecutionPolicy,
    operation: &'a str,
    id: &'a str,
    prompt: &'a Prompt,
) -> AgentFuture<'a, Result<()>> {
    Box::pin(async move {
        let input_json = serde_json::to_string(prompt).map_err(provider_error)?;
        let frozen = crate::prompt::freeze(prompt.clone(), turn.images, &state.images).await?;
        let journaled = serde_json::to_string(&frozen).map_err(provider_error)?;
        let Some((index, after)) = policy
            .carry_steer(
                operation.to_owned(),
                id.to_owned(),
                input_json.clone(),
                journaled,
            )
            .await?
        else {
            return Ok(());
        };
        turn.next_index = turn.next_index.max(index);
        turn.receipts.insert(id.to_owned(), (input_json, false));
        let turn_id = turn
            .events
            .turn_id()
            .unwrap_or(turn.events.request_id())
            .to_owned();
        State::publish(
            &turn.events,
            AgentEventKind::InputAccepted,
            &event::InputAccepted {
                session_id: &state.session_id,
                turn_id: &turn_id,
                item_id: format!("{turn_id}:steer:{id}"),
                kind: "steer",
                request_id: Some(id),
                input: &frozen.instruction,
            },
        );
        turn.pending.push_back(PendingSteer {
            prompt: frozen,
            message_id: Some(id.to_owned()),
            index,
            after,
            boundary: None,
            durable: true,
        });
        Ok(())
    })
}

pub(super) fn accept_steer<'a>(
    state: &'a State,
    turn: &'a mut TurnSteering,
    id: Option<String>,
    prompt: Prompt,
) -> AgentFuture<'a, Result<bool>> {
    Box::pin(async move {
        if id.as_ref().is_some_and(|id| id.is_empty()) {
            return Err(NanocodeError::InvalidRequest(
                "steer identity must not be empty".into(),
            ));
        }
        let input_json = serde_json::to_string(&prompt).map_err(provider_error)?;
        if state.policy.is_none()
            && let Some(id) = &id
            && let Some((input, withdrawn)) = turn.receipts.get(id)
        {
            if input != &input_json {
                return Err(NanocodeError::InvalidRequest(format!(
                    "steering message `{id}` already has different input"
                )));
            }
            if *withdrawn {
                return Err(NanocodeError::InvalidRequest(format!(
                    "steering message `{id}` was withdrawn"
                )));
            }
            return Ok(false);
        }
        if !turn.accepting {
            return Err(NanocodeError::TurnNotSteerable);
        }
        let frozen = crate::prompt::freeze(prompt, turn.images, &state.images).await?;
        let local_index = || {
            turn.next_index
                .checked_add(1)
                .ok_or_else(|| unsupported("Claude steer counter exhausted"))
        };
        let (index, durable) = if let (Some(policy), Some(operation)) =
            (&state.policy, &turn.operation)
            && policy.supports_steering()
        {
            let journaled = serde_json::to_string(&frozen).map_err(provider_error)?;
            let Some(index) = policy
                .accept_steer(
                    operation.clone(),
                    id.clone(),
                    turn.model_call_index,
                    input_json.clone(),
                    journaled,
                )
                .await?
            else {
                return Ok(false);
            };
            (index, true)
        } else {
            if state.policy.is_some() && id.is_some() {
                return Err(NanocodeError::InvalidRequest(
                    "Claude execution policy does not support identified steering receipts".into(),
                ));
            }
            (local_index()?, false)
        };
        turn.next_index = index;
        if let Some(id) = &id {
            turn.receipts.insert(id.clone(), (input_json, false));
        }
        let turn_id = turn
            .events
            .turn_id()
            .unwrap_or(turn.events.request_id())
            .to_owned();
        let item = id
            .clone()
            .unwrap_or_else(|| nanocode_platform::uuid_v7().to_string());
        State::publish(
            &turn.events,
            AgentEventKind::InputAccepted,
            &event::InputAccepted {
                session_id: &state.session_id,
                turn_id: &turn_id,
                item_id: format!("{turn_id}:steer:{item}"),
                kind: "steer",
                request_id: id.as_deref(),
                input: &frozen.instruction,
            },
        );
        turn.pending.push_back(PendingSteer {
            prompt: frozen,
            message_id: id,
            index,
            after: turn.model_call_index,
            boundary: None,
            durable,
        });
        turn.preempt
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(true)
    })
}

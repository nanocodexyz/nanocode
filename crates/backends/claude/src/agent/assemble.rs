use super::*;
use serde_json::json;

pub(super) fn build(mut builder: ClaudeBuilder) -> Result<(Agent, AgentEvents)> {
    if let Ok(model) = builder
        .claude
        .model
        .parse::<nanocode_agent_core::ClaudeModel>()
        .map(ModelSelection::Claude)
    {
        model
            .capabilities(ModelTransport::Native)
            .check_fast_mode(builder.fast_mode)?;
    }
    let session_id = builder
        .policy
        .as_ref()
        .map(|policy| policy.state_id().to_owned())
        .or_else(|| builder.session_id.clone())
        .unwrap_or_else(|| nanocode_platform::uuid_v7().to_string());
    let mut recipe = builder.clone();
    recipe.session_id = None;
    recipe.restored = None;
    recipe.initial_developer_notices = Vec::new();
    recipe.lineage = None;
    recipe.conversation_id = None;
    recipe.system_context = None;

    recipe.policy = None;
    recipe.child_journal = None;
    recipe.subagent_type = Some("general-purpose".into());
    let native_factory = Arc::new(ClaudeNativeFactory {
        recipe,
        state: Atom::new(Weak::new()),
    });
    let selected_model = ModelSelection::new(HarnessFamily::Claude, builder.claude.model.clone());
    let mut handle = AgentHandle::new(
        Arc::<str>::from(session_id.as_str()),
        selected_model,
        native_factory.clone(),
    )
    .with_root_session_id(
        builder
            .lineage
            .as_ref()
            .map_or(session_id.as_str(), |lineage| {
                lineage.root_session_id.as_str()
            }),
    )
    .with_native_model_id(builder.claude.model.as_str())
    .with_child_journal(builder.child_journal.clone());
    if let Some(factory) = &builder.spawn_factory {
        handle = handle.with_spawn_factory(factory.clone());
    }
    let mut custom_tool_search = false;
    let mut adapter_cleanup = Vec::new();
    let mut adapter_shutdown = Vec::new();
    let mut code_turn_cleanup = Vec::new();
    let mut code_turn_preempt = Vec::new();
    let mut code_turn_settle = Vec::new();
    let mut code_quiet = Vec::new();
    let mut agent_bound = Vec::new();
    let mut turn_hold = Vec::new();
    let mut round_notices = Vec::new();
    let mut working_directory = None;
    let mut nested_tools = None;
    let mut recovery = None;
    if let Some(factory) = &builder.tools_factory {
        let native = factory(handle.clone())?;
        nested_tools = native.nested_tools;
        recovery = native.recovery;
        custom_tool_search = native.custom_tool_search;
        builder.client_tool_search |= custom_tool_search;
        builder.tools.extend(native.tools);
        builder.dynamic_tools.extend(native.dynamic);
        adapter_cleanup.extend(native.adapter_cleanup);
        adapter_shutdown.extend(native.adapter_shutdown);
        code_turn_cleanup.extend(native.code_turn_cleanup);
        code_turn_preempt.extend(native.code_turn_preempt);
        code_turn_settle.extend(native.code_turn_settle);
        code_quiet.extend(native.code_quiet);
        agent_bound.extend(native.agent_bound);
        turn_hold.extend(native.turn_hold);
        round_notices.extend(native.round_notices);
        working_directory = native.working_directory;
    }

    if builder.claude.model.trim().is_empty()
        || builder.max_tokens == Some(0)
        || builder.context_window_tokens == 0
        || builder.auto_compact_window_tokens == Some(0)
    {
        return Err(unsupported("Claude model and max_tokens must be nonempty"));
    }
    if builder.max_tokens.is_none() && model_max_tokens(&builder.claude.model).is_none() {
        return Err(unsupported(
            "Unknown Claude model: configure max_tokens explicitly",
        ));
    }
    if builder.system_blocks.as_ref().is_some_and(|blocks| {
        blocks.is_empty()
            || blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) != Some("text")
                    || block.get("text").and_then(Value::as_str).is_none()
            })
    }) {
        return Err(unsupported(
            "system_blocks must be nonempty Claude text blocks",
        ));
    }
    if builder.upstream_context {
        builder
            .tools
            .sort_by_cached_key(|(definition, _)| {
                match crate::upstream::tool_definition(&definition.name) {
                    Some(_) => (false, definition.name.clone()),
                    None => (true, String::new()),
                }
            });
    }
    let mut handlers = HashMap::new();
    let mut definitions = Vec::new();
    for (mut definition, handler) in builder.tools {
        crate::instructions::upstream_tool(
            &builder.claude.model,
            builder.codesign,
            &mut definition,
        );
        if definition.defer_loading
            && !builder.client_tool_search
            && !builder
                .server_tools
                .iter()
                .any(|tool| tool.kind.starts_with("tool_search_tool_"))
        {
            return Err(unsupported("deferred Claude tool needs client_tool_search"));
        }
        if definition.name.trim().is_empty()
            || handlers.insert(definition.name.clone(), handler).is_some()
        {
            return Err(unsupported("duplicate or empty Claude tool name"));
        }
        definitions.push(definition);
    }
    let discovered = Arc::new(Atom::new(HashSet::<String>::new()));
    if custom_tool_search && !handlers.contains_key("ToolSearch") {
        return Err(unsupported(
            "custom_tool_search requires a ToolSearch handler",
        ));
    }
    if builder.client_tool_search && !custom_tool_search {
        if handlers.contains_key("ToolSearch") || handlers.contains_key("DeferredToolPlaceholder") {
            return Err(unsupported("reserved Claude discovery tool name"));
        }
        let catalog = definitions.clone();
        handlers.insert(
            "ToolSearch".into(),
            Arc::new(move |input, _context| {
                let catalog = catalog.clone();
                Box::pin(async move {
                    let fields = input
                        .as_object()
                        .ok_or("ToolSearch input must be an object")?;
                    if fields
                        .keys()
                        .any(|key| !matches!(key.as_str(), "query" | "max_results"))
                    {
                        return Err("unsupported ToolSearch option".into());
                    }
                    let query = input
                        .get("query")
                        .and_then(Value::as_str)
                        .ok_or("ToolSearch requires query")?
                        .trim();
                    if query.is_empty() || query.len() > 512 {
                        return Err("ToolSearch query must be 1–512 bytes".into());
                    }
                    let limit = match input.get("max_results") {
                        None => 5,
                        Some(value) => value
                            .as_u64()
                            .filter(|n| *n >= 1)
                            .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                            .ok_or("max_results must be a positive integer")?,
                    };
                    let selected = query.strip_prefix("select:");
                    let matches = catalog
                        .iter()
                        .filter(|tool| {
                            tool.defer_loading
                                && selected.map_or_else(
                                    || {
                                        tool.name.to_lowercase().contains(&query.to_lowercase())
                                            || tool
                                                .description
                                                .to_lowercase()
                                                .contains(&query.to_lowercase())
                                    },
                                    |name| name.trim() == tool.name,
                                )
                        })
                        .take(limit)
                        .collect::<Vec<_>>();
                    if matches.is_empty() {
                        return Ok(ClaudeToolReply::success(ToolResultContent::Text(
                            "No matching tools".into(),
                        )));
                    }
                    let references = matches
                        .into_iter()
                        .map(|tool| json!({"type":"tool_reference","tool_name":tool.name}))
                        .collect::<Vec<_>>();
                    Ok(ClaudeToolReply::success(ToolResultContent::Blocks(
                        references,
                    )))
                })
            }),
        );
        handlers.insert(
            "DeferredToolPlaceholder".into(),
            Arc::new(|_, _context| {
                Box::pin(async {
                    Err("DeferredToolPlaceholder is not callable; use ToolSearch".into())
                })
            }),
        );
        definitions.push(ToolDefinition {
            name: "ToolSearch".into(),
            description: "Find deferred tools by name or purpose; use select:ToolName for an exact match.".into(),
            input_schema: json!({"type":"object","properties":{"query":{"type":"string"},"max_results":{"type":"integer","minimum":1}},"required":["query"],"additionalProperties":false}).into(),
            strict: None, defer_loading: false, cache_control: None,
        });
        definitions.push(ToolDefinition {
            name: "DeferredToolPlaceholder".into(),
            description: "Placeholder for deferred tools; call ToolSearch to load one.".into(),
            input_schema: json!({"type":"object","properties":{}}).into(),
            strict: None,
            defer_loading: false,
            cache_control: None,
        });
    }
    for hooks in builder.tool_hooks.iter().rev() {
        for (name, handler) in &mut handlers {
            *handler = hooked_handler(name.clone(), handler.clone(), hooks.clone());
        }
    }
    if builder.tool_repair
        && let Some(handler) = handlers.get_mut("Bash")
    {
        *handler = repair::bash_handler(handler.clone());
    }
    if let Some(adapter) = &builder.tools_adapter {
        if !builder.server_tools.is_empty() {
            return Err(unsupported("client tool adapters cannot wrap server tools"));
        }
        let dynamic = std::mem::take(&mut builder.dynamic_tools)
            .into_iter()
            .map(|factory| {
                let hooks = builder.tool_hooks.clone();
                let model = builder.claude.model.clone();
                let codesign = builder.codesign;
                Arc::new(move || {
                    let mut catalog = factory();
                    for (definition, handler) in &mut catalog.tools {
                        crate::instructions::upstream_tool(&model, codesign, definition);
                        for hook in hooks.iter().rev() {
                            *handler = hooked_handler(
                                definition.name.clone(),
                                handler.clone(),
                                hook.clone(),
                            );
                        }
                    }
                    catalog
                }) as DynamicToolsFactory
            })
            .collect();
        let catalog = ClaudeTools {
            tools: definitions
                .drain(..)
                .map(|d| {
                    let handler = handlers.remove(&d.name).expect("validated tool handler");
                    (d, handler)
                })
                .collect(),
            dynamic,
            custom_tool_search: false,
            adapter_cleanup: Vec::new(),
            adapter_shutdown: Vec::new(),
            code_turn_cleanup: Vec::new(),
            code_turn_preempt: Vec::new(),
            code_turn_settle: Vec::new(),
            code_quiet: Vec::new(),
            nested_tools: nested_tools.clone(),
            recovery: recovery.clone(),
            shared_catalog: None,
            shared_tool_names: HashSet::new(),
            shared_shutdown: None,
            agent_bound: Vec::new(),
            turn_hold: Vec::new(),
            round_notices: Vec::new(),
            working_directory: None,
        };
        let adapted = adapter(catalog)?;
        nested_tools = adapted.nested_tools;
        recovery = adapted.recovery;
        for (mut definition, handler) in adapted.tools {
            crate::instructions::upstream_tool(
                &builder.claude.model,
                builder.codesign,
                &mut definition,
            );
            if definition.name.is_empty()
                || !definition.input_schema.is_object()
                || handlers.insert(definition.name.clone(), handler).is_some()
            {
                return Err(unsupported("invalid or duplicate adapted Claude tool"));
            }
            definitions.push(definition);
        }
        builder.dynamic_tools = adapted.dynamic;
        adapter_cleanup.extend(adapted.adapter_cleanup);
        adapter_shutdown.extend(adapted.adapter_shutdown);
        code_turn_cleanup.extend(adapted.code_turn_cleanup);
        code_turn_preempt.extend(adapted.code_turn_preempt);
        code_turn_settle.extend(adapted.code_turn_settle);
        code_quiet.extend(adapted.code_quiet);
        agent_bound.extend(adapted.agent_bound);
        turn_hold.extend(adapted.turn_hold);
        round_notices.extend(adapted.round_notices);
        working_directory = adapted.working_directory.or(working_directory);
        builder.client_tool_search = false;
    }
    if let Some(deadline) = builder.deadline {
        for (name, handler) in &mut handlers {
            *handler = deadline_handler(name.clone(), handler.clone(), deadline);
        }
    }
    if let (Some(deadline), Some(percent)) = (builder.deadline, builder.yield_share) {
        for (name, handler) in &mut handlers {
            if matches!(name.as_str(), "exec" | "wait") {
                *handler = yield_share_handler(name.clone(), handler.clone(), deadline, percent);
            }
        }
    }
    if builder.tool_repair
        && let Some(handler) = handlers.get_mut("exec")
    {
        *handler = repair::exec_handler(handler.clone());
    }
    if let Some(floor) = builder.yield_floor {
        for (name, handler) in &mut handlers {
            if matches!(name.as_str(), "exec" | "wait") {
                *handler = yield_floor_handler(name.clone(), handler.clone(), floor);
            }
        }
    }
    if let Some(every) = builder.stall_nudge {
        let stall = Arc::new(crate::deadline::Stall::new(every));
        for (name, handler) in &mut handlers {
            if matches!(name.as_str(), "exec" | "wait" | "Bash") {
                *handler = stall_nudge_handler(handler.clone(), stall.clone(), builder.deadline);
            }
        }
    }
    let mut names = handlers.keys().map(String::as_str).collect::<HashSet<_>>();
    for tool in &builder.server_tools {
        if tool.kind.is_empty() || tool.name.is_empty() || !names.insert(&tool.name) {
            return Err(unsupported(
                "duplicate or empty Claude server tool name/type",
            ));
        }
    }
    if builder
        .session_id
        .as_ref()
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err(unsupported("Claude session ID must not be empty"));
    }
    if let (Some(session_id), Some(policy)) = (&builder.session_id, &builder.policy)
        && session_id != policy.state_id()
    {
        return Err(unsupported(
            "durable Claude session ID must equal the policy state ID",
        ));
    }
    let restore_span =
        nanocode_trace::info_span!(target: "nanocode", "claude.build.restore").entered();
    let mut restored = builder.restored.unwrap_or_default();
    restored
        .developer_notices
        .extend(std::mem::take(&mut builder.initial_developer_notices));
    let stopped =
        nanocode_trace::info_span!(target: "nanocode", "claude.build.stopped").in_scope(|| {
            stopped_background_notices(
                &session_id,
                &restored.conversation.messages,
                &restored.developer_notices,
            )
        });
    restored.developer_notices.extend(stopped);
    let kept_prefix = restored
        .context
        .as_ref()
        .filter(|kept| kept.build.is_some() && kept.build == crate::environment::build())
        .filter(|_| {
            restored
                .model
                .as_deref()
                .is_none_or(|model| model == builder.claude.model)
        })
        .and_then(|kept| {
            kept.prefix
                .clone()
                .or_else(|| kept.template.as_deref().and_then(crate::durability::prefix))
        })
        .filter(|kept| durable::is_code_only_catalog(&kept.tools) == builder.code_only);
    match &mut restored.context {
        Some(kept) if kept_prefix.is_some() => kept.prefix.clone_from(&kept_prefix),
        _ => nanocode_trace::info_span!(target: "nanocode", "claude.build.strip")
            .in_scope(|| strip_tool_changes(&mut restored.conversation.messages)),
    }

    let accepted_turns = u64::from(
        !restored.conversation.messages.is_empty()
            || !restored.conversation.summary.is_empty()
            || !restored.conversation.admitted_tool_ids.is_empty()
            || !restored.conversation.recovery_notices.is_empty()
            || restored.conversation.previous_message_id.is_some(),
    );
    #[cfg(not(target_family = "wasm"))]
    if let Some(tasks) = &restored.tasks {
        builder
            .task_board
            .as_ref()
            .ok_or_else(|| unsupported("restoring Claude task state requires the task board"))?
            .restore(tasks.clone())
            .map_err(provider_error)?;
    }
    #[cfg(not(not(target_family = "wasm")))]
    if restored.tasks.is_some() {
        return Err(unsupported(
            "Claude task restoration requires a native target with tools and a task board",
        ));
    }

    let lineage = builder
        .lineage
        .or_else(|| restored.lineage.clone())
        .unwrap_or_else(|| Lineage::root(session_id.as_str()));
    let conversation_id = builder
        .conversation_id
        .or_else(|| restored.conversation_id.clone())
        .unwrap_or_else(|| session_id.clone());
    discovered.set(restored.discovered.clone());
    let clone_span = nanocode_trace::info_span!(target: "nanocode", "claude.build.clone").entered();
    let mut round_boundary = restored.clone();
    round_boundary.lineage = Some(lineage.clone());
    round_boundary.conversation_id = Some(conversation_id.clone());
    let round_boundary = Arc::new(round_boundary);
    drop(clone_span);
    drop(restore_span);
    let (runtime, events) = BackendRuntime::new(session_id.clone());
    let runtime = runtime.with_lineage(lineage.clone());
    let session_events = runtime.events();
    let system_blocks = builder.system_blocks.map(Arc::new).or_else(|| {
        builder.system.is_empty().then(|| {
            crate::instructions::default_system_blocks(
                builder.code_only,
                &builder.claude.model,
                builder.codesign,
                builder.upstream_context,
            )
        })
    });
    let fixed_date = builder
        .environment
        .as_ref()
        .is_some_and(crate::ClaudeEnvironment::fixes_date);
    let given_context = builder.system_context;
    let kept_context = restored.context.clone();
    let requested_at = kept_context.as_ref().map_or(0, |context| context.requested);
    let finish_check = builder.finish_check
        || builder.finish_ambiguity
        || builder.finish_tool_truth
        || builder.finish_held_out
        || builder.finish_retries.is_some()
        || !(builder.skip_finish_check
            || (builder.codesign
                && builder.claude.model.starts_with("claude-opus-")
                && builder.late_finish_check.is_none()
                && builder.finish_check_min_requests.is_none()));
    let state = Arc::new(State {
        session_events,
        subagent_type: builder.subagent_type,
        subagent_type_resolver: builder.subagent_type_resolver,
        lifecycle_opened: Atom::default(),
        client: builder.claude.client.bind_subscription_session(&session_id),
        model: builder.claude.model,
        max_tokens: builder.max_tokens,
        effort: Atom::new(builder.effort),
        automatic_cache: builder.automatic_cache,
        cache_one_hour: AtomicBool::new(builder.cache_one_hour),
        requested: Atom::default(),
        requested_at: AtomicU64::new(requested_at),
        parking: parked::Parking::default(),
        thinking_mode: AtomicThinkingMode::new(builder.thinking_mode),
        keep_thinking: builder.keep_thinking,
        fast_mode: AtomicBool::new(builder.fast_mode),
        message_diagnostics: builder.message_diagnostics,
        tool_input_deltas: builder.tool_input_deltas,
        hedging: builder.hedging,
        message_thread: builder.message_threads.then(Atom::default),
        omit_custom_model_thinking: builder.omit_custom_model_thinking,
        refusal_continuations: builder.refusal_continuations,
        refusal_fallback_scope: builder.refusal_fallback_scope,
        steer_preempts_model: builder.steer_preempts_model,
        wire_cache: crate::WireCache::default(),
        context_window_tokens: builder.context_window_tokens,
        auto_compact_window_tokens: builder.auto_compact_window_tokens,
        auto_compact_token_limit: Atom::default(),
        images: builder.images,
        session_id,
        lineage,
        conversation_id,
        workspace: builder.workspace,
        workspace_resolver: builder.workspace_resolver,
        hand_environment: builder.hand_environment,
        child_workspace_init: builder.child_workspace_init,
        system_resolver: builder.system_resolver,
        host_context: builder.host_context,
        system: builder.system,
        system_blocks,
        system_value: std::sync::OnceLock::new(),
        tools: definitions,
        adapter_cleanup,
        adapter_shutdown,
        code_turn_cleanup,
        code_turn_preempt,
        code_turn_settle,
        code_quiet,
        turn_hold,
        round_notices,
        deadline: builder.deadline.map(if builder.stop_at_target {
            crate::deadline::Reminders::stopping_at_target
        } else {
            crate::deadline::Reminders::new
        }),
        headless: builder.headless,
        brief_final: builder.brief_final,
        brief_finish: builder.brief_finish,
        escalation: builder.escalation,
        fold_finish: builder.fold_finish,
        lean_cells: builder.lean_cells,
        low_effort_rounds: builder.low_effort_rounds,
        persist_finish: builder.persist_finish,
        persistence_nudge: builder.persistence_nudge,
        codesign: builder.codesign,
        finish_ambiguity: builder.finish_ambiguity,
        finish_tool_truth: builder.finish_tool_truth,
        finish_held_out: builder.finish_held_out,
        finish_check,
        late_finish_check: builder.late_finish_check,
        finish_check_min_requests: builder.finish_check_min_requests,
        finish_retries: builder.finish_retries,
        result_effort: builder.result_effort,
        stop_at_target: builder.stop_at_target,
        tool_result_tokens: builder.tool_result_tokens,
        tool_result_clearing: builder.tool_result_clearing,
        clearing_cache_floors: builder.clearing_cache_floors,
        working_directory: working_directory.map(crate::environment::WorkingDirectory::new),
        nested_tools,
        recovery,
        dynamic_tools: builder.dynamic_tools,
        tool_hooks: builder.tool_hooks,
        server_tools: builder.server_tools,
        handlers,
        discovered,
        client_tool_search: builder.client_tool_search,
        upstream_context: builder.upstream_context,
        code_only: builder.code_only,
        parallel_tools: builder.parallel_tools,
        parallel_safe_tools: builder.parallel_safe_tools,
        developer_notices: Atom::new(restored.developer_notices),
        persistence_update: Sequencer::new(),
        persistence_ready: Notify::new(),
        paging: Slot::default(),
        conversation: Owned::new(restored.conversation),
        round_boundary: Atom::new(round_boundary),
        side_view: Atom::default(),
        side_request: Atom::default(),
        kept: Atom::default(),
        keeping: AtomicBool::new(false),
        kept_changed: Arc::new(Notify::new()),
        refreshes: AtomicU64::new(0),
        refreshed: Atom::default(),
        this: std::sync::OnceLock::new(),
        context: std::sync::OnceLock::new(),
        given_context,
        environment: builder.environment,
        fixed_date,
        kept_context,
        kept_prefix: Atom::new(kept_prefix),
        sent_prefix: Atom::default(),
        offered: std::sync::OnceLock::new(),
        boundary_stale: AtomicBool::new(false),
        summary_refused: AtomicBool::new(false),
        policy: builder.policy,
        admission: Sequencer::new(),
        idle: Notify::new(),
        attempts: std::sync::atomic::AtomicUsize::new(0),
        admission_order: AtomicU64::new(0),
        waiters: Atom::default(),
        compaction_cancel: Atom::default(),
        #[cfg(not(target_family = "wasm"))]
        task_board: builder.task_board,
        cancellations: Atom::default(),
        life: AgentLife::new(),
        accepted_turns: AtomicU64::new(accepted_turns),
        steering: Owned::default(),
        live_nested_starts: Atom::default(),
        code_call_summaries: Atom::default(),
    });
    native_factory.state.set(Arc::downgrade(&state));
    let _ = state.this.set(Arc::downgrade(&state));
    let steering = Arc::downgrade(&state);
    let steer: Steer = Arc::new(move |text| {
        let state = steering.clone();
        Box::pin(async move {
            let Some(state) = state.upgrade() else {
                return false;
            };
            let accepted = {
                let mut turns = state.steering.acquire().await;
                let Some((_, turn)) = turns
                    .iter_mut()
                    .filter(|(_, turn)| turn.accepting)
                    .min_by_key(|(key, _)| key.0)
                else {
                    return false;
                };
                match state.accept_steer(turn, None, text.into()).await {
                    Ok(true) => Ok(state.start_code_preempt().await),
                    Ok(false) => Ok(Vec::new()),
                    Err(error) => Err(error),
                }
            };
            let Ok(preempts) = accepted else {
                return false;
            };
            State::finish_code_preempt(preempts);
            true
        })
    });
    let client = state.client.clone();
    spawn_task(Box::pin(async move { drop(client.preconnect().await) }));
    let driver = Driver { state, handle };
    let agent = runtime.bind(driver);
    for bound in &agent_bound {
        bound(&agent, &steer);
    }
    Ok((agent, events))
}

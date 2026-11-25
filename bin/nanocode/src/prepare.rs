use super::*;

#[inline(never)]
pub(super) fn prepare<'a>(
    args: &'a SessionArgs,
    clients: ModelClients,
    notify: &'a mut (dyn FnMut(&str) + Send),
    headless: bool,
) -> nanocode_sync::future::BoxFuture<'a, Result<Prepared>> {
    Box::pin(async move {
        let workspace = match &args.workspace {
            Some(path) => std::fs::canonicalize(path)
                .map_err(|error| format!("workspace {}: {error}", path.display()))?,
            None => std::env::current_dir()?,
        };
        let home = host::home_dir(args.home.as_deref())?;
        let gateway = host::gateway(&args.gateway)?;
        if args.cyber_access_program.is_some() && gateway.is_some() {
            return Err(
                "--cyber-access-program needs OpenAI itself; it is unavailable with --gateway"
                    .into(),
            );
        }
        if gateway.is_some() && clients.supplies_openai() {
            return Err(
                "this binary supplies its own Codex client; --gateway is unavailable".into(),
            );
        }
        let saved = defaults::load(&home).await?;
        let saved_harness = saved.harness()?;
        let saved_model = saved.model.clone().filter(|_| {
            args.model.is_none()
                && args
                    .harness
                    .is_none_or(|harness| Some(harness) == saved_harness)
        });
        let requested_model = args.model.clone().or_else(|| saved_model.clone());
        let requested_harness = args
            .harness
            .or_else(|| saved_harness.filter(|_| args.model.is_none()));
        let antigravity = match requested_harness {
            Some(harness) => harness == crate::HarnessArg::Antigravity,
            None => requested_model
                .as_deref()
                .is_some_and(|model| model.starts_with("gemini-")),
        };
        if !antigravity
            && requested_model
                .as_deref()
                .is_none_or(|model| clients.resolve(model).is_err())
        {
            clients.listed().await?;
        }
        let (route, model) = match (&requested_model, requested_harness) {
            (model, _) if antigravity => (
                host::Route::Native,
                nanocode::antigravity::AntigravityHarness::selection(
                    model
                        .clone()
                        .unwrap_or_else(|| nanocode::antigravity::DEFAULT_MODEL.to_owned()),
                ),
            ),
            (Some(model), harness) => {
                let (route, model) =
                    clients.resolve(model).map_err(|error| match &saved_model {
                        Some(_) => format!(
                            "the default model saved in {}: {error}; change it with nc default",
                            defaults::path(&home).display()
                        )
                        .into(),
                        None => error,
                    })?;
                let model = match harness {
                    Some(harness) if host::follows_harness(route, &model) => {
                        host::on_harness(&model, &family(harness))
                    }
                    _ => model,
                };
                if let Some(harness) = harness
                    && family(harness) != model.family()
                {
                    return Err(format!(
                        "model {} does not belong to the {} harness",
                        route.label(&model),
                        family(harness)
                    )
                    .into());
                }
                (route, model)
            }
            (None, harness) => {
                let requested = harness.map(family);
                let model = match clients.codex_models().transpose()?.and_then(<[_]>::first) {
                    Some(model)
                        if requested
                            .as_ref()
                            .is_none_or(|family| *family == nanocode::HarnessFamily::Codex) =>
                    {
                        nanocode::ModelSelection::Codex(*model)
                    }
                    _ => requested
                        .unwrap_or(nanocode::HarnessFamily::Claude)
                        .default_model(),
                };
                (host::Route::Native, model)
            }
        };
        if let Some(gateway) = &gateway
            && route == host::Route::Native
        {
            gateway.check(&model)?;
        }
        let defaulted = requested_model.is_none()
            && requested_harness.is_none()
            && clients.codex_models().is_none();
        let capabilities = model.capabilities(nanocode::ModelTransport::Native);
        let saved_thinking = saved
            .thinking()?
            .filter(|thinking| capabilities.supports_thinking(*thinking));
        let saved_tier = saved
            .tier()?
            .filter(|tier| capabilities.supports_service_tier(*tier));
        let thinking = match (&args.thinking, saved_thinking) {
            (Some(value), _) => host::parse_thinking(value)?,
            (None, Some(thinking)) => thinking,
            (None, None) if defaulted && capabilities.supports_thinking(Thinking::Xhigh) => {
                Thinking::Xhigh
            }
            (None, None) => model.default_thinking(),
        };
        model
            .capabilities(nanocode::ModelTransport::Native)
            .check_thinking(thinking)?;
        let tier = match &args.tier {
            Some(value) => host::parse_tier(value)?,
            None => saved_tier.unwrap_or(ServiceTier::Standard),
        };
        model
            .capabilities(nanocode::ModelTransport::Native)
            .check_service_tier(tier)?;
        let (store, archiver) = if args.ephemeral {
            (None, None)
        } else {
            let (store, archiver) = if headless {
                host::open_headless_store(&home).await?
            } else {
                host::open_store(&home).await?
            };
            (Some(store), Some(archiver))
        };
        nanocode::codex_tools::apply_patch::set_lenient_matching(args.lenient_patches);
        if let Some(min_wait_ms) = args.min_wait_ms {
            nanocode_process::set_min_wait_ms(min_wait_ms);
        }
        if let Some(time_limit) = args.time_limit {
            nanocode_process::set_time_limit(std::time::Duration::from_secs(time_limit));
        }
        let runner_configs = host::runner_configs(&args.remote)?;
        let config = HostConfig {
            workspace,
            store,
            archiver,
            subagents: match (args.no_subagents, args.subagents) {
                (true, _) => host::Subagents::Off,
                (false, false) => host::Subagents::NativeModels,
                (false, true) => host::Subagents::AllModels,
            },
            runners: runner_configs.runners,
            runner_servers: runner_configs.servers,
            tab: runner_configs.tab,
            gateway,
            google: args.google.resolved_config().await?,
            clients,
            headless,
            cyber_access_program: args.cyber_access_program.map(Into::into),
            context_window: args.context_window,
            max_output_tokens: args
                .max_output_tokens
                .map(u32::try_from)
                .transpose()
                .map_err(|error| format!("--max-output-tokens: {error}"))?,
            compact_instructions: args.compact_instructions,
            codex_direct: args.codex_orchestration == Some(cli::CodexOrchestrationArg::Direct),
            codesign: !args.no_codesign,
            no_reasoning_replay: args.no_reasoning_replay,
            tool_output_token_limit: args
                .tool_output_token_limit
                .map(usize::try_from)
                .transpose()
                .map_err(|error| format!("--tool-output-token-limit: {error}"))?,
            lean_tool_arguments: args.lean_tool_arguments,
            working_state: working_state_policy(args)?,
            hedge: hedge_policy(args)?,
        };
        let host = nanocode_trace::Instrument::instrument(
            Host::build(config, &model, thinking),
            nanocode_trace::info_span!(target: "nanocode", "nc.host.build"),
        )
        .await?;
        let mut runner_events = host.runtime.runner_events();
        let initialized = nanocode_trace::Instrument::instrument(
            host.runtime.initialize_runners(),
            nanocode_trace::info_span!(target: "nanocode", "nc.runners.initialize"),
        )
        .await;
        if let Err(error) = initialized {
            let mut failures = Vec::new();
            while let Ok(event) = runner_events.try_recv() {
                failures.extend(runner_failure(&event));
            }
            return Err(match failures.is_empty() {
                true => error.into(),
                false => failures.join("; ").into(),
            });
        }
        for server in host.runtime.runner_servers() {
            notify(&host::listener_notice(&server));
        }
        if let Some(link) = &host.config.tab {
            notify(&host::tab_notice(link));
        }
        Ok(Prepared {
            host,
            model,
            route,
            thinking,
            tier,
            home,
            runner_events,
        })
    })
}

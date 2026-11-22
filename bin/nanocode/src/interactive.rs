use super::*;

#[inline(never)]
pub(super) fn interactive(
    mut args: SessionArgs,
    clients: ModelClients,
) -> nanocode_sync::future::LocalBoxFuture<'static, Result<()>> {
    Box::pin(async move {
        let reload = match &args.reload_state {
            Some(path) => Some(app::reload::read(path).await?),
            None => None,
        };
        if let Some(state) = &reload {
            args.resume = if state.moved {
                Some(String::new())
            } else {
                state.session.clone()
            };
            args.prompt = None;
            args.continue_last = false;
            args.model = Some(host::Route::of(state.provider.as_deref())?.label(&state.model));
            args.harness = None;
            args.thinking = Some(state.thinking.to_string());
            args.tier = Some(state.tier.as_str().to_owned());
        }
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return Err(
                "the interactive interface needs a terminal; use `nanocode exec <prompt>` for scripts"
                    .into(),
            );
        }
        let terminal = nanocode_tui::terminal::TerminalSession::enter(
            nanocode_tui::terminal::TerminalOptions {
                mouse_capture: !args.no_mouse,
                keyboard_enhancement: true,
                alternate_screen: !args.no_alt_screen,
            },
        )?;
        let prepared = prepare(&args, clients, &mut |_| {}, false).await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(terminal);
                return Err(error);
            }
        };
        let (resume, pick_session) = match args.resume.as_deref() {
            Some("") => (None, true),
            Some(id) => (Some(id.to_owned()), false),
            None if args.continue_last => match latest_session(&prepared.host).await {
                Ok(id) => (Some(id), false),
                Err(error) => {
                    drop(terminal);
                    return Err(error);
                }
            },
            None => (None, false),
        };
        if (resume.is_some() || pick_session) && prepared.host.config.store.is_none() {
            drop(terminal);
            return Err("--resume and --continue require durability; remove --ephemeral".into());
        }
        app::run(
            app::Launch {
                host: prepared.host,
                model: prepared.model,
                route: prepared.route,
                thinking: prepared.thinking,
                tier: prepared.tier,
                resume,
                pick_session,
                prompt: args.prompt,
                history_path: Some(prepared.home.join("history.jsonl")),
                home: prepared.home.clone(),
                thinking_override: args
                    .thinking
                    .as_deref()
                    .map(host::parse_thinking)
                    .transpose()?,
                tier_override: args.tier.as_deref().map(host::parse_tier).transpose()?,
                reload,
                reload_path: args.reload_state,
                runner_events: prepared.runner_events,
            },
            terminal,
        )
        .await
    })
}

#[inline(never)]
pub(super) fn list_sessions(
    args: SessionsArgs,
) -> nanocode_sync::future::BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        let home = host::home_dir(args.home.as_deref())?;
        let (store, _) = host::open_store(&home).await?;
        if let Some(id) = &args.session {
            let transcript = store.load(id).await?.into_transcript()?;
            let printed = print_transcript(&transcript);
            store.flush().await?;
            return Ok(unless_closed(printed)?);
        }
        let workspace = std::env::current_dir()?;
        let limit = args.limit.map_or(Ok(sessions::PAGE), usize::try_from)?;
        let mut page = if args.all {
            store
                .list_page(None, limit.saturating_add(1), |record| {
                    sessions::user_facing(record.lineage.origin)
                })
                .await?
        } else {
            store
                .newest_page(workspace.as_path(), limit.saturating_add(1), |record| {
                    sessions::user_facing(record.lineage.origin)
                })
                .await?
        };
        let more = page.len() > limit;
        page.truncate(limit);
        let elsewhere = page.is_empty()
            && !args.all
            && !store
                .list_page(None, 1, |record| {
                    sessions::user_facing(record.lineage.origin)
                })
                .await?
                .is_empty();
        let printed = print_sessions(&page);
        if more && printed.is_ok() {
            eprintln!("nanocode: more sessions than the {limit} shown; --limit <n> lists more");
        }
        if elsewhere {
            eprintln!(
                "nanocode: no saved sessions in {}; nanocode sessions --all lists every workspace's",
                workspace.display()
            );
        }
        store.flush().await?;
        Ok(unless_closed(printed)?)
    })
}

#[inline(never)]
pub(super) fn hand(args: HandArgs) -> nanocode_sync::future::BoxFuture<'static, Result<()>> {
    Box::pin(async move {
        use nanocode::hand::{Hand, HandEvent};
        let directory = match &args.directory {
            Some(path) => std::fs::canonicalize(path)?,
            None => std::env::current_dir()?,
        };
        let mut tools = nanocode::hand::tools(directory.clone())?.into_builder();
        if args.cua {
            let computer = nanocode_computer_repl::ComputerTools::connect(
                nanocode_computer_repl::ComputerConfig::detect()?,
            )
            .await?;
            tools = tools.tool(computer.js()).tool(computer.reset());
        }
        if let Some(after) = args.cell_yield_ms {
            tools = tools.cell_yield(std::time::Duration::from_millis(after));
        }
        let tools = tools.build()?;
        let name = match args.name {
            Some(name) => name,
            None => Hand::machine_name()?,
        };
        let hand = match (args.url, args.listen) {
            (Some(url), _) => Hand::connect(url, args.token, tools),
            (None, Some(address)) => Hand::listen(address, args.token, tools),
            (None, None) => return Err("pass the brain URL or --listen <ADDR>".into()),
        }
        .name(&name)
        .directory(directory.clone());
        let hand = match &args.pricing {
            Some(path) => hand.pricing(nanocode::hand::HandPricing::rules(
                &std::fs::read_to_string(path)?,
            )?),
            None => hand,
        };
        let hand = if args.tools_only {
            hand.tools_only()
        } else {
            hand
        };
        eprintln!("nanocode hand {name} in {}", directory.display());
        if let Some(after) = args.cell_yield_ms {
            eprintln!(
                "hand: a cell whose call names no yield_time_ms yields after {after} ms; a brain that names none and gives up on an operation sooner takes the cell for lost: set the brain's cell yield instead"
            );
        }
        let run = hand.run(|event| match event {
            HandEvent::Connecting { endpoint } => eprintln!("hand: connecting {endpoint}"),
            HandEvent::Connected { endpoint, .. } => eprintln!("hand: connected to {endpoint}"),
            HandEvent::Listening { endpoint } => eprintln!("hand: listening {endpoint}"),
            HandEvent::BrainConnected { peer } => eprintln!("hand: brain connected from {peer}"),
            HandEvent::Disconnected { reason } => eprintln!("hand: disconnected ({reason})"),
            HandEvent::Retrying { delay } => eprintln!("hand: retrying in {}ms", delay.as_millis()),
            HandEvent::Revoked => eprintln!("hand: the brain disconnected this hand; stopping"),
            HandEvent::Replaced => {
                eprintln!("hand: another hand attached under this name; stopping")
            }
            other => eprintln!("hand: {other:?}"),
        });
        let mut run = std::pin::pin!(run);
        nanocode_sync::select! {
            result = &mut run => {
                let left = leave_reader();
                result?;
                left?;
            }
            signal = nanocode_executor::signal::ctrl_c() => {
                signal?;
                leave_reader()?;
            }
            signal = terminated() => {
                signal?;
                eprintln!("hand: stopped; what its commands left running is read until it ends");
                hand_over()?;
            }
        }
        Ok(())
    })
}

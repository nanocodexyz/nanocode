use super::*;

#[inline(never)]
pub(super) fn run(
    launch: Launch,
    mut terminal: TerminalSession,
) -> nanocode_sync::future::LocalBoxFuture<'static, Result<()>> {
    Box::pin(async move {
        let (tx, mut rx) = unbounded_channel();
        let history = launch
            .history_path
            .as_ref()
            .map(|path| crate::history::load(path))
            .unwrap_or_default();
        let mut app = App {
            host: launch.host.clone(),
            agent: None,
            events: None,
            generation: 0,
            model: launch.model.clone(),
            route: launch.route,
            thinking: launch.thinking,
            tier: launch.tier,
            transcript: Transcript::new(TranscriptPolicy {
                collapsed_preview_lines: TOOL_PREVIEW_LINES,
                collapsed_diff_lines: DIFF_PREVIEW_LINES,
            }),
            mapper: EventMapper::default(),
            composer: Composer::new("Ask anything · / for commands", History::new(history)),
            queue: Queue::default(),
            turn: TurnState::Idle,
            overlay: None,
            tree: AgentTree::default(),
            show_tree: true,
            show_panel: true,
            panel_refresh: false,
            theme: Theme::default(),
            status: None,
            starting: true,
            pending_switch: None,
            spinner: Spinner::new(Instant::now(), SPINNER_FRAME),
            toggle_mouse: false,
            home: launch.home.clone(),
            finder: None,
            finder_dismissed: None,
            workspace_files: None,
            files_loading: false,
            attachments: Vec::new(),
            attachments_by_id: HashMap::new(),
            next_attachments: Vec::new(),
            resumes: HashMap::new(),
            resuming: false,
            hold_queue: false,
            moved: false,
            reload_file: None,
            reloaded: launch.reload.is_some(),
            pending_copy: None,
            interrupting: false,
            thinking_override: launch.thinking_override,
            tier_override: launch.tier_override,
            awaiting_terminal: false,
            computer_use_notice: None,
            google_codes: None,
            session_started: false,
            tx,
            quit: false,
            reload: false,
            reload_all: false,
            submissions: nanocode_executor::JoinSet::new(),
            reload_side: None,
            reload_focus: None,
            quit_armed: None,
            esc_armed: None,
            shells: Vec::new(),
            stashed_draft: None,
            dragging: None,
            autoscroll_at: None,
            clock_deadline: None,
            btw: HashMap::new(),
            next_btw: 0,
            next_local: 0,
            history_path: launch.history_path.clone(),
            spans: crate::telemetry::TurnSpans::default(),
            focus: None,
            views: HashMap::new(),
            panes: crate::side::Panes::default(),
        };
        if !terminal.options().alternate_screen {
            app.transcript.keep_scrollback();
        }
        if let Some(state) = launch.reload {
            app.restore_reload(state);
        }
        for server in app.host.runtime.runner_servers() {
            app.notice(NoticeLevel::Info, crate::host::listener_notice(&server));
        }
        if let Some(link) = &app.host.config.tab {
            app.notice(NoticeLevel::Info, crate::host::tab_notice(link));
        }
        forward_runners(launch.runner_events, app.tx.clone());
        if let Some(notices) = app.host.config.clients.notifying() {
            forward_notices(notices, app.tx.clone());
        }
        if let Some(updates) = app.host.runtime.take_subagent_updates() {
            forward_subagents(updates, app.tx.clone());
        }
        if let Some(prompt) = launch
            .prompt
            .clone()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            app.enqueue_follow_up(prompt);
        }
        if launch.pick_session {
            app.starting = false;
            app.open_session_picker();
            if let Some(path) = launch.reload_path {
                nanocode_executor::fs::remove_file(path).await?;
            }
        } else if app.host.runtime.runner_pool().awaiting_replacement() {
            if let Some(path) = launch.reload_path {
                nanocode_executor::fs::remove_file(path).await?;
            }
            app.notice(
                NoticeLevel::Info,
                "waiting for a hand to connect; the session starts on it",
            );
            app.connect(launch.resume.clone());
        } else {
            app.reload_file = launch.reload_path;
            app.connect(launch.resume.clone());
        }
        let mut instance = None;
        let outcome = app.event_loop(&mut terminal, &mut rx, &mut instance).await;
        if let Err(error) = terminal.close(&app.transcript.release_scrollback()) {
            eprintln!("nanocode: could not print the transcript: {error}");
        }
        let session = app.session_id();
        let moved = app.moved;
        let executor = app.reload.then(|| app.reload_executor());
        let hands = app.reload.then(|| app.reload_hands());
        if app.reload_all
            && let Some(instance) = &instance
            && let Err(error) = instance.broadcast().await
        {
            eprintln!("nanocode: could not reload every window: {error}");
        }
        if let Some(agent) = app.agent.as_ref()
            && let Err(error) = agent.flush().await
            && !moved
        {
            eprintln!("nanocode: could not flush the session: {error}");
        }
        let left = app.reload && outcome.is_ok() && leave_commands(&app.host).await;
        if !left {
            if let Err(error) = app.host.runtime.shutdown().await
                && !moved
            {
                eprintln!("nanocode: shutdown did not complete cleanly: {error}");
            }
            if let Err(error) = crate::host::archive(&app.host.config).await {
                eprintln!("nanocode: could not archive the session: {error}");
            }
            while let Some(result) = app.submissions.join_next().await {
                if let Err(error) = result {
                    eprintln!("nanocode: prompt task failed during shutdown: {error}");
                }
            }
        }
        if app.reload && outcome.is_ok() {
            while let Some((generation, event)) = app.next_buffered_event() {
                app.generated_event(generation, &event);
            }
            while let Ok(event) = rx.try_recv() {
                match event {
                    AppEvent::SteerSent { .. } => {
                        app.handle_app(event);
                    }
                    AppEvent::Side {
                        pane,
                        update:
                            update @ (crate::side::SideUpdate::Event(_)
                            | crate::side::SideUpdate::Forked(_)
                            | crate::side::SideUpdate::Restored { .. }),
                    } => {
                        app.side_update(pane, update);
                    }
                    _ => {}
                }
            }
            let mut state = app.reload_state();
            if let Some(executor) = executor {
                state.executor = executor;
            }
            if let Some(hands) = hands {
                state.hands = hands;
            }
            let instance = instance.ok_or("reload before the reload socket started")?;
            let state = instance.save(&state).await?;
            instance.close().await?;
            return Err(reload(session, state).into());
        }
        if let Some(instance) = instance {
            instance.close().await?;
        }
        if let Some(agent_session) = session {
            eprintln!("{} resume {agent_session}", invoked_name());
        }
        outcome
    })
}

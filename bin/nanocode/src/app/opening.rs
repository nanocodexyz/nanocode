use super::*;

#[inline(never)]
pub(super) fn open_session<'a>(
    host: &'a Host,
    session_id: &'a str,
    thinking_override: Option<Thinking>,
    tier_override: Option<ServiceTier>,
) -> nanocode_sync::future::BoxFuture<'a, Result<Ready>> {
    Box::pin(async move {
        let store = host
            .config
            .store
            .as_ref()
            .ok_or("resume requires durability; remove --ephemeral")?;
        let stored = nanocode_trace::Instrument::instrument(
            store.load(session_id),
            nanocode_trace::info_span!(target: "nanocode", "nc.session.load", session.id = session_id),
        )
        .await?;
        let model = stored.summary.record.model.clone();
        let route = Route::of(stored.summary.record.provider.as_deref())?;
        if !host.config.clients.serves_route(route) {
            return Err(format!(
                "session {session_id} runs {}, which this build does not offer",
                route.label(&model)
            )
            .into());
        }
        let checkpoint = stored.session_checkpoint()?;
        let checkpoint_thinking = checkpoint.as_ref().map(SessionCheckpoint::thinking);
        let recorded_tier = checkpoint
            .as_ref()
            .and_then(SessionCheckpoint::service_tier)
            .or_else(|| {
                stored
                    .summary
                    .record
                    .initial
                    .map(|initial| initial.service_tier)
            });
        let opened_thinking = checkpoint_thinking.unwrap_or_else(|| model.default_thinking());
        let thinking = thinking_override
            .or(checkpoint_thinking)
            .or_else(|| {
                stored
                    .summary
                    .record
                    .initial
                    .map(|initial| initial.thinking)
            })
            .unwrap_or(opened_thinking);
        model
            .capabilities(nanocode::ModelTransport::Native)
            .check_thinking(thinking)?;
        let pending = pending_turns(&stored.turns, stored.in_progress.as_deref())?;
        if !pending.is_empty()
            && let Some(archiver) = &host.config.archiver
            && let Some(pid) = archiver.live_owner(session_id).await?
        {
            return Err(format!(
                "session {session_id} is running a turn in nanocode process {pid}; let that turn finish or stop it there, then resume"
            )
            .into());
        }
        let prompts = stored
            .turns
            .iter()
            .filter(|turn| turn.status != nanocode::durability::TurnStatus::Pending)
            .filter_map(|turn| turn.preview.clone())
            .collect();
        let (agent, events) = nanocode_trace::Instrument::instrument(
            host.open(store, &stored),
            nanocode_trace::info_span!(target: "nanocode", "nc.session.restore", session.id = session_id),
        )
        .await?;
        if thinking != opened_thinking {
            agent.set_thinking(thinking).await?;
        }
        let tier = match tier_override {
            Some(tier) => {
                model
                    .capabilities(nanocode::ModelTransport::Native)
                    .check_service_tier(tier)?;
                apply_tier(&agent, &model, tier).await?;
                tier
            }
            None => recorded_tier.unwrap_or_default(),
        };
        Ok(Ready {
            notice: format!(
                "resumed session {session_id} · {} · thinking {thinking}",
                route.label(&model)
            ),
            agent,
            events,
            model,
            route,
            thinking,
            tier,
            history: Some(stored.into_transcript()?),
            reset_transcript: true,
            pending,
            prompts,
        })
    })
}

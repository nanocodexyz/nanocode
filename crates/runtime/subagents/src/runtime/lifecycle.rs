use super::*;

#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn attach_started<'a>(
    reg: &'a Arc<Registry>,
    root_session_id: &'a str,
    id: AgentId,
    agent: Agent,
    events: nanocode_agent_core::AgentEvents,
    model: Option<nanocode_agent_core::ModelSelection>,
    thinking: Option<nanocode_agent_core::Thinking>,
    reasoning_mode: nanocode_agent_core::ReasoningMode,
    schema: String,
) -> RegistryFuture<'a, std::io::Result<()>> {
    Box::pin(async move {
        let checkpoint = if reg.store_for(root_session_id).is_some() {
            match agent.checkpoint().await {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    let _ = agent.shutdown().await;
                    return Err(std::io::Error::other(error));
                }
            }
        } else {
            None
        };
        let registry = Arc::clone(reg);
        let root = root_session_id.to_owned();
        let updates = reg.updates.clone();
        let (start, ready) = oneshot::channel();
        let attached = reg
            .state
            .call(move |state| {
                let Some(session) = state
                    .scopes
                    .get_mut(&root)
                    .and_then(|scope| scope.sessions.get_mut(&id))
                    .filter(|session| {
                        matches!(session.status, AgentStatus::Pending)
                            && matches!(session.stored_runtime, StoredRuntime::Unstarted(_))
                    })
                else {
                    return Err(agent);
                };
                session.descriptor.model = model;
                session.descriptor.thinking = thinking;
                session.descriptor.reasoning_mode = reasoning_mode;
                let event_task = forward_events(
                    root.clone(),
                    id,
                    session.descriptor.parent,
                    events,
                    ready,
                    Arc::downgrade(&registry),
                    updates,
                );
                let (harness, harness_task) = harness::spawn(
                    root.clone(),
                    id,
                    session.descriptor.context(),
                    agent,
                    registry.capacity.clone(),
                    Arc::downgrade(&registry),
                    schema,
                    None,
                );
                if let Some(snapshot) = checkpoint {
                    registry.record_checkpoint(&root, id, &session.marks, snapshot);
                }
                session.residency = Residency::Resident(harness);
                session.harness_task = Some(harness_task);
                session.event_task = Some(event_task);
                session.stored_runtime = StoredRuntime::None;
                Ok(Arc::clone(&session.marks))
            })
            .await;
        match attached {
            Ok(marks) => {
                let _ = start.send((0, marks));
                reg.changed();
                Ok(())
            }
            Err(agent) => {
                let _ = agent.shutdown().await;
                Err(std::io::Error::other(format!(
                    "subagent {id} was stopped before it started"
                )))
            }
        }
    })
}

#[inline(never)]
pub(super) fn harness_turn_finished<'a>(
    reg: &'a Arc<Registry>,
    root_session_id: &'a str,
    id: AgentId,
    result: AgentResult<TurnResult>,
) -> RegistryFuture<'a, ()> {
    Box::pin(async move {
        let registry = Arc::clone(reg);
        let root = root_session_id.to_owned();
        let finished = reg
            .state
            .call(move |state| {
                let root_session_id = root.as_str();
                let session = state
                    .scopes
                    .get_mut(root_session_id)
                    .and_then(|scope| scope.sessions.get_mut(&id))?;
                let turn = session.turn.take()?;
                let settled = turn.revision;
                let submitted_output = match turn.stage {
                    TurnStage::Submitted(output) => Some(output),
                    TurnStage::Working | TurnStage::Steering => None,
                };
                if matches!(result, Err(NanocodeError::TurnStopped))
                    && submitted_output.is_none()
                    && !matches!(session.status, AgentStatus::Closing | AgentStatus::Closed)
                {
                    return Some((
                        session.status.clone(),
                        None,
                        None,
                        false,
                        Arc::clone(&session.marks),
                    ));
                }

                session.resume_attempts = 0;
                session.in_flight_calls = Vec::new();
                session.in_flight_omitted = 0;
                session.in_flight_progress = durable::OmittedProgress::default();
                if let Ok(TurnResult {
                    usage: Some(usage), ..
                }) = &result
                {
                    session.usage.record(usage);
                }
                let follow_up_output = session.follow_up_output.take();
                let kept = result.is_ok() && submitted_output.is_none() && follow_up_output.is_some();

                if let Some(output) = &submitted_output {
                    session.last_output = Some(output.clone());
                }
                if matches!(session.status, AgentStatus::Closing | AgentStatus::Closed) {
                    session.status.clone()
                } else {
                    match result {
                        Ok(_) => complete_session(session, submitted_output.or(follow_up_output)),
                        Err(NanocodeError::TurnCancelled | NanocodeError::TurnStopped)
                            if submitted_output.is_none() =>
                        {
                            AgentStatus::Interrupted
                        }
                        Err(error) => match submitted_output {
                            Some(output) => complete_session(session, Some(output)),
                            None => AgentStatus::Failed {
                                error: error.to_string(),
                            },
                        },
                    }
                }
                .clone_into(&mut session.status);

                let harness = session
                    .harness()
                    .cloned()
                    .filter(|_| registry.begin_pending_checkpoint(root_session_id));
                let delivered = (session.settled_revision == Some(settled)
                    || (kept && session.settled_revision.is_some()))
                    && session.completion == Completion::None;
                session.settled_revision = Some(settled);

                let completion = (session.status.wakes_parent()
                    && session.descriptor.parent.is_none()
                    && !delivered)
                    .then_some(settled);
                if let Some(revision) = completion {
                    session.settle_completion(revision);
                }
                Some((
                    session.status.clone(),
                    completion,
                    harness,
                    true,
                    Arc::clone(&session.marks),
                ))
            })
            .await;
        let Some((status, completion, harness, settled, lane)) = finished else {
            return;
        };
        reg.enqueue_status(
            root_session_id,
            id,
            lane,
            OrderedJob::Settle {
                status,
                completion,
                harness,
            },
        );
        reg.changed();
        if !settled {
            return;
        }
        let registry = Arc::clone(reg);
        let root_session_id = root_session_id.to_owned();
        let locks = registry.scope_locks(&root_session_id);
        if !locks.evicting.start() {
            return;
        }
        drop(platform::spawn(async move {
            loop {
                registry.enforce_resident_limit(&root_session_id).await;
                if locks.evicting.finish() {
                    break;
                }
            }
        }));
    })
}

#[inline(never)]
pub(super) fn launch_initial_turn<'a>(
    reg: &'a Arc<Registry>,
    root_session_id: &'a str,
    id: AgentId,
    prompt: String,
    capacity: TurnCapacity,
) -> RegistryFuture<'a, std::io::Result<()>> {
    Box::pin(async move {
        let root = root_session_id.to_owned();
        let harness = reg
            .state
            .call(move |state| state.harness_in_scope(&root, id))
            .await?;
        harness.start(prompt, capacity).await
    })
}

#[inline(never)]
pub(super) fn wake<'a>(
    reg: &'a Arc<Registry>,
    root_session_id: &'a str,
    id: AgentId,
) -> RegistryFuture<'a, ()> {
    Box::pin(async move {
        let root = root_session_id.to_owned();
        let asleep = reg
            .state
            .call(move |state| {
                state
                    .scopes
                    .get(&root)
                    .and_then(|scope| scope.sessions.get(&id))
                    .is_some_and(|session| session.hibernated.is_some())
            })
            .await;
        if !asleep {
            return;
        }
        if let Err(error) = reg
            .rehydrate(root_session_id, id, MessagePurpose::Coordinate)
            .instrument(nanocode_trace::info_span!(
                "subagent.wake",
                session.root_id = root_session_id
            ))
            .await
        {
            nanocode_trace::warn!(%id, %error, "could not wake a hibernated subagent");
            let failure = format!("agent {id} could not wake: {error}");
            reg.settle_hibernated(
                root_session_id,
                &[id],
                || NanocodeError::InvalidRequest(failure.clone()),
                &failure,
            )
            .await;
        }
    })
}

#[inline(never)]
pub(super) fn stop_and_close<'a>(
    reg: &'a Registry,
    root_session_id: String,
    ids: Vec<AgentId>,
    harnesses: Vec<HarnessHandle>,
    detach: bool,
) -> RegistryFuture<'a, std::io::Result<Vec<AgentSummary>>> {
    Box::pin(async move {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = nanocode_platform::now() + AGENT_STOP_TIMEOUT;
        let closing_result = reg.close_harnesses(harnesses, deadline).await;
        if let Some(registry) = reg.shared() {
            registry
                .settle_hibernated(
                    &root_session_id,
                    &ids,
                    || NanocodeError::TurnStopped,
                    "message cancelled because the agent closed",
                )
                .await;
        }
        reg.wait_until_inactive(&root_session_id, &ids, deadline)
            .await?;

        drop(closing_result);
        let (
            ClosedSessions {
                summaries,
                harness_tasks,
                event_tasks,
            },
            released,
        ) = {
            let root = root_session_id.clone();
            let closing = ids.clone();
            reg.state
                .call(move |state| {
                    let closed = state.finish_close(&root, &closing)?;
                    let released = state
                        .scopes
                        .get(&root)
                        .is_some_and(|scope| scope.life.released());
                    Ok::<_, std::io::Error>((closed, released))
                })
                .await?
        };
        for summary in summaries.iter().filter(|_| !released) {
            let update = AgentUpdate::Status {
                id: summary.agent_id,
                status: AgentStatus::Closed,
            };
            reg.coordinate(&root_session_id, &update);
            let _ = send_update(&reg.updates, &root_session_id, update, detach);
        }
        let closed_sessions = {
            let root = root_session_id.clone();
            let closing = ids.clone();
            reg.state
                .call(move |state| {
                    state
                        .scopes
                        .get(&root)
                        .map(|scope| {
                            closing
                                .iter()
                                .filter_map(|id| {
                                    scope
                                        .sessions
                                        .get(id)
                                        .map(|session| session.descriptor.session_id.clone())
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .await
        };
        reg.session_handles.update(|handles| {
            for session in &closed_sessions {
                handles.remove(session);
            }
        });
        reg.changed();
        reg.wait_for_tasks(harness_tasks, deadline, "subagent harnesses")
            .await?;
        reg.wait_for_tasks(event_tasks, deadline, "subagent event streams")
            .await?;
        Ok(summaries)
    })
}

#[inline(never)]
pub(super) fn close_batch<'a>(
    reg: &'a Registry,
    root_session_id: &'a str,
    ids: Vec<AgentId>,
) -> RegistryFuture<'a, std::io::Result<()>> {
    Box::pin(async move {
        let root = root_session_id.to_owned();
        if reg
            .state
            .call(move |state| {
                state
                    .scopes
                    .get(&root)
                    .is_some_and(|scope| scope.life.closing())
            })
            .await
        {
            return Ok(());
        }
        let mut errors = Vec::new();
        for id in ids.into_iter().rev() {
            if let Err(error) = reg.close(root_session_id, id).await {
                errors.push(format!("subagent {id}: {error}"));
            }
        }
        if reg.store_for(root_session_id).is_some()
            && let Err(error) = reg.persist_durably(root_session_id).await
        {
            errors.push(format!("persisting startup cleanup: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(std::io::Error::other(errors.join("; ")))
        }
    })
}

#[inline(never)]
pub(super) fn close<'a>(
    reg: &'a Registry,
    session_id: &'a str,
    id: AgentId,
) -> RegistryFuture<'a, std::io::Result<Vec<AgentSummary>>> {
    Box::pin(async move {
        reg.await_restored(session_id).await?;
        let _warm = reg.warm_subtree(session_id, id).await?;
        let locks = reg.session_locks(session_id).await;
        let _closing = locks.closing.enter().await;
        let _paused = reg.pause_messages(session_id).await;
        let CloseRequest {
            root_session_id,
            ids,
            harnesses,
            status_updates,
        } = {
            let session = session_id.to_owned();
            reg.state
                .call(move |state| state.request_close(&session, id))
                .await?
        };
        for (id, status, lane) in status_updates {
            reg.enqueue_status(&root_session_id, id, lane, OrderedJob::Status(status));
        }
        reg.changed();
        let summaries = reg
            .stop_and_close(root_session_id.clone(), ids, harnesses, false)
            .await?;
        let root = root_session_id.clone();
        let frozen = reg
            .state
            .call(move |state| {
                state
                    .scopes
                    .get(&root)
                    .is_some_and(|scope| scope.life.frozen())
            })
            .await;
        if reg.store_for(&root_session_id).is_some() && !frozen {
            reg.persist_durably(&root_session_id)
                .await
                .map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!("agent {id} closed, but its closure is not durable yet: {error}"),
                    )
                })?;
        }
        Ok(summaries)
    })
}

#[inline(never)]
pub(super) fn hibernated<'a>(
    reg: &'a Arc<Registry>,
    root_session_id: &'a str,
    id: AgentId,
    stored: StoredRuntime,
    hibernated: Hibernated,
    lock: Turn,
) -> RegistryFuture<'a, ()> {
    Box::pin(async move {
        let finished = hibernated.call.finished();
        let root = root_session_id.to_owned();
        let asleep = reg
            .state
            .call(move |state| {
                let session = state
                    .scopes
                    .get_mut(&root)
                    .and_then(|scope| scope.sessions.get_mut(&id))?;
                session.residency = Residency::Evicted;
                session.stored_runtime = stored;
                session.hibernated = Some(hibernated);
                session.hibernations += 1;
                drop(session.harness_task.take());
                drop(session.event_task.take());
                Some((
                    session.descriptor.session_id.clone(),
                    session.turn.as_ref().is_some_and(|turn| turn.interrupted),
                ))
            })
            .await;
        let interrupted = asleep.as_ref().is_some_and(|(_, interrupted)| *interrupted);
        if let Some((session_id, _)) = asleep {
            reg.session_handles.update(|handles| {
                handles.remove(&session_id);
            });
            if let Some(released) = Option::clone(&reg.session_released.get()) {
                released(&session_id);
            }
        }
        drop(lock);
        reg.changed();
        let registry = Arc::downgrade(reg);
        let root = root_session_id.to_owned();
        let capacity = reg.capacity.clone();
        drop(platform::spawn(async move {
            if !interrupted {
                finished.await;
                let granted = capacity.acquire(Waiter::Resident).await;
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                let scoped = root.clone();
                registry
                    .state
                    .call(move |state| {
                        if let Some(hibernated) = state
                            .scopes
                            .get_mut(&scoped)
                            .and_then(|scope| scope.sessions.get_mut(&id))
                            .and_then(|session| session.hibernated.as_mut())
                        {
                            hibernated.capacity = Some(granted);
                        }
                    })
                    .await;
            }
            if let Some(registry) = registry.upgrade() {
                Box::pin(registry.wake(&root, id)).await;
            }
        }));
    })
}

#[inline(never)]
pub(super) fn evict<'a>(
    reg: &'a Registry,
    root_session_id: &'a str,
    eviction: ResidentEviction,
) -> RegistryFuture<'a, bool> {
    Box::pin(async move {
        let ResidentEviction {
            id,
            harness,
            marks,
            _lock,
        } = eviction;
        let snapshot = if reg.store_for(root_session_id).is_some()
            && reg.journal_holds_conversation(root_session_id, &marks)
        {
            Ok(None)
        } else {
            harness.snapshot().await.map(Some)
        };
        match snapshot {
            Ok(snapshot) => {
                let stored = match snapshot {
                    None => None,
                    Some(snapshot)
                        if reg.store_for(root_session_id).is_some()
                            && reg.record_checkpoint(
                                root_session_id,
                                id,
                                &marks,
                                snapshot.clone(),
                            ) =>
                    {
                        None
                    }
                    Some(snapshot) => Some(snapshot),
                };
                let root = root_session_id.to_owned();
                reg.state
                    .call(move |state| {
                        if let Some(session) = state
                            .scopes
                            .get_mut(&root)
                            .and_then(|scope| scope.sessions.get_mut(&id))
                        {
                            session.stored_runtime = stored
                                .map_or(StoredRuntime::Journaled, |stored| {
                                    StoredRuntime::Checkpoint(Box::new(stored))
                                });
                        }
                    })
                    .await;
            }
            Err(_) => {
                let root = root_session_id.to_owned();
                reg.state
                    .call(move |state| {
                        if let Some(session) = state
                            .scopes
                            .get_mut(&root)
                            .and_then(|scope| scope.sessions.get_mut(&id))
                        {
                            session.residency = Residency::Resident(harness);
                        }
                    })
                    .await;
                return false;
            }
        }
        reg.changed();
        if harness.close().await.is_err() {
            reg.harness_closed(root_session_id, id).await;
        }

        let root = root_session_id.to_owned();
        let tasks = reg
            .state
            .call(move |state| {
                let session = state
                    .scopes
                    .get_mut(&root)
                    .and_then(|scope| scope.sessions.get_mut(&id));
                session.map(|session| (session.harness_task.take(), session.event_task.take()))
            })
            .await;
        if let Some((harness_task, event_task)) = tasks {
            if let Some(task) = harness_task {
                let _ = task.await;
            }
            if let Some(task) = event_task {
                let _ = task.await;
            }
        }
        true
    })
}

#[inline(never)]
pub(super) fn close_all<'a>(
    this: &'a SubagentControl,
    root_session_id: &'a str,
) -> RegistryFuture<'a, std::io::Result<()>> {
    Box::pin(async move { this.registry.close_all(root_session_id).await.map(drop) })
}

#[inline(never)]
pub(super) fn deferred_resume<'a>(
    this: &'a SubagentControl,
    root_session_id: &'a str,
) -> RegistryFuture<'a, std::io::Result<Vec<AgentId>>> {
    Box::pin(async move {
        this.registry.await_restored(root_session_id).await?;
        Ok(this
            .registry
            .deferred_resume
            .read(|deferred| {
                deferred
                    .get(root_session_id)
                    .map(|ids| ids.iter().copied().collect())
            })
            .unwrap_or_default())
    })
}

#[inline(never)]
pub(super) fn resume_restored<'a>(
    this: &'a SubagentControl,
    root_session_id: &'a str,
) -> RegistryFuture<'a, std::io::Result<Resumed>> {
    Box::pin(async move {
        this.registry.await_restored(root_session_id).await?;
        let mut ids = Vec::new();
        this.registry.deferred_resume.update(|deferred| {
            ids = deferred
                .remove(root_session_id)
                .map(|ids| ids.into_iter().collect())
                .unwrap_or_default();
        });
        let work = this.registry.take_deferred_work(root_session_id);
        let results = if ids.is_empty() {
            Vec::new()
        } else {
            this.registry.pending_resume.update(|pending| {
                pending
                    .entry(root_session_id.to_owned())
                    .or_default()
                    .extend(ids.iter().copied());
            });
            this.registry.changed();
            this.registry.resume_interrupted(root_session_id).await
        };
        if let Some(work) = work {
            this.registry
                .start_restored_work(root_session_id, work)
                .await;
        }
        Ok(results)
    })
}

#[inline(never)]
pub(super) fn discard_restored<'a>(
    this: &'a SubagentControl,
    root_session_id: &'a str,
) -> RegistryFuture<'a, std::io::Result<()>> {
    Box::pin(async move {
        this.registry.await_restored(root_session_id).await?;
        let mut ids = std::collections::BTreeSet::new();
        this.registry.deferred_resume.update(|deferred| {
            ids = deferred
                .remove(root_session_id)
                .map(|ids| ids.into_iter().collect())
                .unwrap_or_default();
        });
        let messages = match this.registry.take_deferred_work(root_session_id) {
            Some(work) => {
                ids.extend(work.unstarted);
                ids.extend(work.messages.iter().map(|message| message.to));
                work.messages
            }
            None => Vec::new(),
        };
        let closed = if ids.is_empty() {
            Ok(())
        } else {
            this.registry
                .close_batch(root_session_id, ids.into_iter().collect())
                .await
        };
        for message in messages {
            this.registry
                .message_failed(
                    root_session_id,
                    message.id,
                    format!(
                        "message {} was discarded with restored agent {}",
                        message.id, message.to
                    ),
                )
                .await;
        }
        closed
    })
}

#[inline(never)]
pub(super) fn cancel_all<'a>(
    this: &'a SubagentControl,
    root_session_id: &'a str,
) -> RegistryFuture<'a, ()> {
    Box::pin(async move {
        this.registry.cancel_all(root_session_id).await;
    })
}

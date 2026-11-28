use crate::compaction;
use crate::nanotools::{Catalog, NanoTools};
use crate::prompts;
use crate::tools::Builtins;
use nanocode_agent_core::{
    Agent, AgentSessionContext, Capabilities, CostStatus, ForkRequest, HarnessFamily, Lineage,
    ModelSelection, NanocodeError, ReportedTurnUsage, Result, SessionCheckpoint, SpawnOptions,
    Thinking, TurnResult, TurnUsage,
    backend::{
        BackendFuture, BackendPrompt, BackendPromptRoute, BackendTurn, BackendTurnKey,
        LifecycleBackend, ParkedCall,
    },
    events::{AgentEventKind, AgentEventPublisher, AgentEvents},
    input::{PromptInput, UserInput},
};
use nanocode_backend_kimi_client::{
    Client, ClientError, Delta, Message, Origin, Reply, Request, TOOL_OUTPUT_EMPTY, ToolCall,
    estimate_message, estimate_text, render_tool_output,
};
use nanocode_sync::Signal;
use nanocode_sync::future::{Either, select};
use nanocode_tool_host::ToolContext;
use serde_json::{Value, json, value::RawValue};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const STOP_STREAK: u32 = 12;

pub struct Config {
    pub client: Client,
    pub model: ModelSelection,
    pub thinking: Thinking,
    pub effort: &'static str,
    pub window: u64,
    pub system: String,
    pub tools: NanoTools,
    pub catalog: Catalog,
    pub builtins: Option<Arc<Builtins>>,
    pub root: bool,
    pub spill: Option<std::path::PathBuf>,
    pub zone: String,
    pub today: crate::harness::Clock,
    pub session: String,
    pub owner: Option<nanocode_durability::DurableOwner>,
}

#[derive(Clone, Default)]
pub struct State {
    messages: Vec<Message>,
    calls: u64,
    date: Option<String>,
    disclosed: bool,
    rebase: Option<(usize, u64)>,
}

struct Inner {
    config: Config,
    state: Mutex<State>,
    cancel: Mutex<Option<Arc<Signal>>>,
    stopped: AtomicBool,
    /// The `/files` ids of the images uploaded so far (`media_key`), as Kimi Code's upload cache.
    uploads: Mutex<std::collections::BTreeMap<u64, String>>,
    /// The endpoint answered an upload with no `/files` route (404, 405, 501): images stay inline.
    no_files: std::sync::atomic::AtomicBool,
}

pub struct Driver {
    inner: Arc<Inner>,
}

fn emit(events: &AgentEventPublisher, kind: AgentEventKind, value: Value) {
    if let Ok(raw) = RawValue::from_string(value.to_string()) {
        drop(events.publish_next(kind, Arc::from(raw)));
    }
}

fn prompt_text(input: &PromptInput) -> String {
    match input {
        PromptInput::Text(text) => text.clone(),
        PromptInput::Content(items) => items
            .iter()
            .filter_map(|item| match item {
                UserInput::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let body = entries
                .iter()
                .map(|(key, value)| {
                    format!("{}:{}", Value::String((*key).clone()), canonical(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

fn call_key(call: &ToolCall) -> String {
    let args = match serde_json::from_str::<Value>(&call.arguments) {
        Ok(value) => canonical(&value),
        Err(_) => call.arguments.clone(),
    };
    format!("{} {args}", call.name)
}

fn advance(streak: &mut (Option<String>, u32), key: &str) {
    if streak.0.as_deref() == Some(key) {
        streak.1 += 1;
    } else {
        *streak = (Some(key.to_owned()), 1);
    }
}

pub fn local_date(at: std::time::SystemTime) -> String {
    let seconds = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
        + i64::from(nanocode_platform::utc_offset_seconds(at));
    let shifted = seconds.div_euclid(86_400) + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// V8's `JSON.parse` message where it can be told from serde's error, serde's otherwise.
fn json_error(error: &serde_json::Error) -> String {
    match error.classify() {
        serde_json::error::Category::Eof => "Unexpected end of JSON input".to_owned(),
        _ => error.to_string(),
    }
}

/// Assistant steps since the last TodoList write and since the last stale todo-list reminder, counted
/// the way Kimi Code's `getTodoListReminderStepCounts` walks the history.
fn todo_steps(messages: &[Message]) -> (usize, usize) {
    let mut found_write = false;
    let mut found_reminder = false;
    let (mut since_write, mut since_reminder) = (0, 0);
    let marker = format!("<system-reminder>\n{}", crate::tools::TODO_REMINDER_PREFIX);
    for message in messages.iter().rev() {
        match message {
            Message::Assistant { tool_calls, .. } => {
                let wrote = tool_calls.iter().any(|call| {
                    call.name == "TodoList"
                        && serde_json::from_str::<Value>(&call.arguments)
                            .is_ok_and(|args| args.get("todos").is_some_and(Value::is_array))
                });
                found_write |= wrote;
                since_write += usize::from(!found_write);
                since_reminder += usize::from(!found_reminder);
            }
            Message::User { content, .. } => {
                found_reminder |= content.starts_with(&marker);
                if found_write && found_reminder {
                    break;
                }
            }
            Message::Tool { .. } => {}
        }
    }
    (since_write, since_reminder)
}

fn unique(calls: &mut [ToolCall], messages: &[Message]) {
    let mut used = messages
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.iter().map(|call| call.id.clone()).collect()
            }
            Message::Tool { tool_call_id, .. } => vec![tool_call_id.clone()],
            Message::User { .. } => Vec::new(),
        })
        .collect::<BTreeSet<_>>();
    for call in calls {
        if used.contains(&call.id) {
            let base = call.id.clone();
            let mut suffix = 2;
            while used.contains(&format!("{base}__{suffix}")) {
                suffix += 1;
            }
            call.id = format!("{base}__{suffix}");
        }
        used.insert(call.id.clone());
    }
}

impl Inner {
    fn load(&self) -> State {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn save(&self, state: &State) {
        if let Ok(mut current) = self.state.lock() {
            *current = state.clone();
        }
    }

    fn overhead(&self) -> u64 {
        estimate_text(&self.config.system) + self.config.catalog.sizes
    }

    fn remind(&self, state: &mut State) {
        let today = (self.config.today)();
        let zone = &self.config.zone;
        if !state.disclosed {
            state
                .messages
                .push(Message::user(prompts::date(&today, zone), Origin::Reminder));
            state.messages.push(Message::user(
                prompts::reminder(prompts::AUTO_MODE),
                Origin::Reminder,
            ));
            state.disclosed = true;
            state.date = Some(today);
        } else if state.date.as_deref() != Some(today.as_str()) {
            state.messages.push(Message::user(
                prompts::date_changed(&today, zone),
                Origin::Reminder,
            ));
            state.date = Some(today);
        }
    }

    /// Kimi Code's per-step injections: completion notices of background tasks, then the stale todo-list
    /// reminder (main agent only).
    fn inject(&self, state: &mut State) {
        let Some(builtins) = &self.config.builtins else {
            return;
        };
        for notice in builtins.notifications() {
            state.messages.push(Message::user(notice, Origin::Reminder));
        }
        if !self.config.root {
            return;
        }
        let (since_write, since_reminder) = todo_steps(&state.messages);
        if let Some(text) =
            crate::tools::todo_reminder(&builtins.open_todos(), since_write, since_reminder)
        {
            state
                .messages
                .push(Message::user(prompts::reminder(&text), Origin::Reminder));
        }
    }

    /// Kimi Code's media resolution before a request: each inline image not uploaded yet goes to `/files`,
    /// one at a time; a refused key fails the request, and any other failure leaves the image inline until
    /// the next request tries again. ponytail: the ids live as long as the agent, where Kimi Code keeps them
    /// on disk across sessions. An endpoint without a `/files` route (a proxy such as Cloudflare's AI Gateway)
    /// turns uploads off for the agent, where Kimi Code would try again on every request.
    async fn upload_images<'a>(
        &self,
        messages: impl Iterator<Item = &'a Message>,
    ) -> Result<std::collections::BTreeMap<u64, String>> {
        for (url, name) in nanocode_backend_kimi_client::inline_images(messages) {
            let key = nanocode_backend_kimi_client::media_key(url);
            if self.no_files.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if self.uploaded().contains_key(&key) {
                continue;
            }
            let Some((mime, data)) = url
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
            else {
                continue;
            };
            let Ok(bytes) = nanocode_sys::base64::STANDARD.decode(data) else {
                continue;
            };
            match self
                .config
                .client
                .upload(&bytes, &name, mime, "image")
                .await
            {
                Ok(id) => {
                    self.uploaded().insert(key, id);
                }
                Err(
                    error @ ClientError::Status {
                        status: 401 | 403, ..
                    },
                ) => {
                    return Err(NanocodeError::InvalidRequest(error.to_string()));
                }
                Err(ClientError::Status {
                    status: 404 | 405 | 501,
                    ..
                }) => self
                    .no_files
                    .store(true, std::sync::atomic::Ordering::Relaxed),
                Err(_) => {}
            }
        }
        Ok(self.uploaded().clone())
    }

    fn uploaded(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<u64, String>> {
        self.uploads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn request(
        &self,
        messages: &[Message],
        extra: Option<&Message>,
        max_completion_tokens: u64,
        call: u64,
        events: &AgentEventPublisher,
    ) -> Result<Reply> {
        let config = &self.config;
        let uploads = self.upload_images(messages.iter().chain(extra)).await?;
        let body = Request {
            model: config.model.as_str(),
            system: &config.system,
            messages,
            extra,
            tools: &config.catalog.declarations,
            max_completion_tokens,
            prompt_cache_key: &config.session,
            effort: config.effort,
            uploads: &uploads,
        }
        .encode();
        emit(
            events,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": call,
                "model_call_index": call,
                "model": config.model.as_str(),
                "reasoning_mode": "standard",
                "effort": config.effort,
            }),
        );
        let started = nanocode_platform::now();
        let nanos = |since: nanocode_platform::Instant| {
            u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
        };
        let mut first_event = None;
        let mut first_output = None;
        let mut on_delta = |delta: Delta<'_>| {
            first_event.get_or_insert_with(|| nanos(started));
            if let Delta::Text(text) = delta
                && extra.is_none()
            {
                first_output.get_or_insert_with(|| nanos(started));
                emit(
                    events,
                    AgentEventKind::AssistantDelta,
                    json!({"model_call_index": call, "item_id": format!("call-{call}"), "text": text}),
                );
            }
        };
        let reply = match config.client.stream(&body, &mut on_delta).await {
            Ok(reply) => reply,
            Err(error) => {
                emit(
                    events,
                    AgentEventKind::ModelCallFailed,
                    json!({
                        "call_index": call,
                        "model_call_index": call,
                        "model": config.model.as_str(),
                        "duration_ns": nanos(started),
                        "error": error.to_string(),
                    }),
                );
                return Err(NanocodeError::InvalidRequest(error.to_string()));
            }
        };
        let usage = reply.usage.unwrap_or_default();
        emit(
            events,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": call,
                "model_call_index": call,
                "model": config.model.as_str(),
                "attempt": 1,
                "connection_generation": 0,
                "status": reply.finish_reason.as_deref().unwrap_or("stop"),
                "duration_ns": nanos(started),
                "time_to_first_event_ns": first_event.unwrap_or_else(|| nanos(started)),
                "time_to_first_output_ns": first_output,
                "tool_calls": reply.tool_calls.len(),
                "usage": {
                    "input_tokens": usage.prompt_tokens,
                    "input_tokens_details": {"cached_tokens": usage.cached_tokens, "cache_write_tokens": 0},
                    "output_tokens": usage.completion_tokens,
                    "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
                    "total_tokens": usage.total(),
                },
            }),
        );
        Ok(reply)
    }

    async fn compact(&self, state: &mut State, events: &AgentEventPublisher) -> Result<u64> {
        emit(events, AgentEventKind::ModelCompactionStarted, json!({}));
        let window = self.config.window;
        let instruction = Message::user(prompts::compaction_instruction(), Origin::Prompt);
        let instruction_tokens = estimate_message(&instruction);
        let cap = window.min(compaction::MAX_OUTPUT_TOKENS);
        let margin = (window as f64 * compaction::MARGIN_RATIO).ceil() as u64;
        let overhead = self.overhead();
        let original = state.messages.clone();
        let weights = compaction::weights(&original, overhead);
        let mut start =
            compaction::pre_shrink(&original, &weights, overhead, instruction_tokens, window);
        let mut attempts = 0;
        let (summary, output) = loop {
            attempts += 1;
            let estimated = match start {
                0 => compaction::history_tokens(overhead, &original),
                _ => overhead + weights[start..].iter().sum::<u64>(),
            } + instruction_tokens;
            let max = cap.min(window.saturating_sub(estimated + margin)).max(1);
            let reply = self
                .request(
                    &original[start..],
                    Some(&instruction),
                    max,
                    state.calls,
                    events,
                )
                .await;
            state.calls += 1;
            let reply = match reply {
                Ok(reply) => reply,
                Err(error) => {
                    emit(
                        events,
                        AgentEventKind::ModelCompactionFailed,
                        json!({"error": error.to_string()}),
                    );
                    return Err(error);
                }
            };
            let summary = reply.content.trim();
            if reply.finish_reason.as_deref() != Some("length") && !summary.is_empty() {
                break (
                    summary.to_owned(),
                    reply.usage.map(|usage| usage.completion_tokens),
                );
            }
            if attempts >= compaction::ATTEMPTS || original.len() - start <= 1 {
                let error = "Kimi's compaction ended without a usable summary".to_owned();
                emit(
                    events,
                    AgentEventKind::ModelCompactionFailed,
                    json!({"error": error}),
                );
                return Err(NanocodeError::InvalidRequest(error));
            }
            start = compaction::without_oldest(&original, start);
        };
        state.messages = compaction::compacted(&state.messages, &summary);
        let after = overhead
            + state
                .messages
                .iter()
                .map(|message| match message {
                    Message::User {
                        content,
                        origin: Origin::Summary,
                    } => output.unwrap_or_else(|| estimate_text(content)),
                    message => estimate_message(message),
                })
                .sum::<u64>();
        state.rebase = Some((state.messages.len(), after));
        state.disclosed = false;
        emit(events, AgentEventKind::ModelCompactionCompleted, json!({}));
        Ok(after)
    }

    async fn execute(&self, call: &ToolCall, turn: &str) -> crate::tools::Outcome {
        use crate::tools::Outcome;
        let args = match call.arguments.as_str() {
            "" => Ok(json!({})),
            text => serde_json::from_str::<Value>(text),
        };
        let args = match args {
            Ok(args) => args,
            Err(error) => {
                return Outcome::error(format!(
                    "Invalid args for tool \"{}\": {}",
                    call.name,
                    json_error(&error)
                ));
            }
        };
        let builtin = self
            .config
            .builtins
            .as_ref()
            .filter(|builtins| builtins.handles(&call.name));
        if builtin.is_none() && !self.config.tools.handles(&call.name) {
            return Outcome::error(format!("Tool \"{}\" not found", call.name));
        }
        if let Some(schema) = self.config.catalog.schemas.get(&call.name)
            && let Some(errors) = crate::validate::errors(schema, &args)
        {
            return Outcome::error(format!("Invalid args for tool \"{}\": {errors}", call.name));
        }
        if let Some(builtins) = builtin {
            return builtins.call(&call.id, &call.name, &args).await;
        }
        let context = ToolContext::new(
            self.config.model.as_str(),
            &self.config.session,
            &call.id,
            &[],
            usize::MAX,
        )
        .with_turn_id(Some(turn));
        let (output, failed) = match self.config.tools.call(&call.name, &args, context).await {
            Ok(body) => (body, false),
            Err(error) => (error, true),
        };
        Outcome {
            output: match output.is_empty() {
                true => TOOL_OUTPUT_EMPTY.to_owned(),
                false => output,
            },
            failed,
            ..Outcome::default()
        }
    }

    async fn run(
        self: Arc<Self>,
        input: PromptInput,
        events: AgentEventPublisher,
        request_id: String,
    ) -> Result<TurnResult> {
        let window = self.config.window;
        let mut state = self.load();
        state
            .messages
            .push(Message::user(prompt_text(&input), Origin::Prompt));
        if let Some(builtins) = &self.config.builtins {
            builtins.turn();
        }
        let mut streak: (Option<String>, u32) = (None, 0);
        let mut final_message = String::new();
        let mut floor = None;
        let mut totals = ReportedTurnUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
            estimated_cost: None,
            cost_status: CostStatus::UsageNotReported,
        };
        loop {
            self.remind(&mut state);
            self.inject(&mut state);
            let counted = compaction::counted(&state.messages, state.rebase);
            if floor.is_none_or(|floor| counted > floor)
                && compaction::should_compact(counted, window)
            {
                floor = Some(self.compact(&mut state, &events).await?);
                self.remind(&mut state);
            }
            self.save(&state);
            let used = compaction::request_used(
                &state.messages,
                estimate_text(&self.config.system),
                self.config.catalog.listing,
            );
            let call_index = state.calls;
            let mut reply = self
                .request(
                    &state.messages,
                    None,
                    window.saturating_sub(used).max(1),
                    call_index,
                    &events,
                )
                .await?;
            state.calls += 1;
            if let Some(usage) = reply.usage {
                totals.input_tokens += usage.prompt_tokens;
                totals.cached_input_tokens += usage.cached_tokens;
                totals.output_tokens += usage.completion_tokens;
                totals.reasoning_output_tokens += usage.reasoning_tokens;
                totals.total_tokens += usage.total();
            }
            unique(&mut reply.tool_calls, &state.messages);
            if !reply.content.trim().is_empty() {
                final_message.clone_from(&reply.content);
                emit(
                    &events,
                    AgentEventKind::AssistantMessage,
                    json!({"model_call_index": call_index, "item_id": format!("call-{call_index}"), "text": reply.content}),
                );
            }
            let calls = reply.tool_calls.clone();
            state.messages.push(Message::Assistant {
                content: reply.content,
                reasoning: reply.reasoning,
                tool_calls: reply.tool_calls,
                usage: reply.usage,
            });
            if calls.is_empty() {
                self.save(&state);
                break;
            }
            let keys = calls.iter().map(call_key).collect::<Vec<_>>();
            let mut stop = false;
            let mut injected = None;
            // Kimi Code starts every call of a step at once and records the results in call order; the
            // tools that hold its environment's process capability (Bash, Glob, Grep, Write, Edit) still
            // run one at a time, in call order.
            for call in &calls {
                emit(
                    &events,
                    AgentEventKind::ToolCall,
                    json!({"call_id": call.id, "tool": call.name, "arguments": call.arguments, "model_call_index": call_index}),
                );
            }
            let process = nanocode_sync::Semaphore::new(1);
            let outcomes = nanocode_sync::future::join_all(calls.iter().map(|call| async {
                let started = nanocode_platform::now();
                let _turn = match matches!(
                    call.name.as_str(),
                    "Bash" | "Glob" | "Grep" | "Write" | "Edit"
                ) {
                    true => process.acquire().await.ok(),
                    false => None,
                };
                let outcome = self.execute(call, &request_id).await;
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                (outcome, elapsed)
            }))
            .await;
            for ((index, call), (outcome, duration_ms)) in calls.iter().enumerate().zip(outcomes) {
                let crate::tools::Outcome {
                    output,
                    failed,
                    note: notice,
                    media,
                } = outcome;
                let output = match call.name == "Read" && self.config.builtins.is_some() {
                    true => output,
                    false => {
                        crate::tools::spill::truncate(
                            &call.name,
                            &call.id,
                            output,
                            self.config.spill.as_deref(),
                            self.config
                                .builtins
                                .as_ref()
                                .and_then(|builtins| builtins.host()),
                        )
                        .await
                    }
                };
                let mut running = streak.clone();
                for key in &keys[..=index] {
                    advance(&mut running, key);
                }
                let mut note = String::new();
                match running.1 {
                    count if count > STOP_STREAK => {
                        note = format!("\n\n{}", prompts::repeated(count).unwrap_or_default());
                        stop = true;
                    }
                    STOP_STREAK => injected = prompts::repeated(STOP_STREAK),
                    count => {
                        if let Some(text) = prompts::repeated(count) {
                            note = format!("\n\n{text}");
                        }
                    }
                }
                let mut content = render_tool_output(&output, failed) + &note;
                if let Some(notice) = &notice {
                    content = format!("{content}\n{notice}");
                }
                emit(
                    &events,
                    AgentEventKind::ToolResult,
                    json!({"call_id": call.id, "tool": call.name, "result": content, "status": if failed { "failed" } else { "completed" }}),
                );
                state.messages.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: output,
                    failed,
                    note,
                    notice,
                    media,
                    duration_ms: Some(duration_ms),
                });
            }
            for key in &keys {
                advance(&mut streak, key);
            }
            if let Some(text) = injected {
                state.messages.push(Message::user(text, Origin::Reminder));
            }
            self.save(&state);
            if stop {
                break;
            }
        }
        Ok(TurnResult {
            request_id: Some(request_id),
            turn_id: None,
            final_message,
            usage: Some(TurnUsage::from_reported(totals)),
            boundary: None,
        })
    }

    fn checkpoint(&self) -> SessionCheckpoint {
        SessionCheckpoint::native(
            self.config.session.clone(),
            self.config.model.clone(),
            self.config.thinking,
            Lineage::root(self.config.session.clone()),
            self.config.session.clone(),
            None,
            true,
            self.payload(),
        )
    }

    async fn persist(&self) -> Result<()> {
        let Some(owner) = &self.config.owner else {
            return Ok(());
        };
        let payload = nanocode_durability::EncodedPayload::encode(&self.payload())
            .map_err(nanocode_durability::agent_error)?;
        owner
            .commit_checkpoint(payload)
            .await
            .map_err(nanocode_durability::agent_error)
    }

    fn payload(&self) -> Value {
        let state = self.load();
        json!({
            "format": crate::durability::FORMAT,
            "model": self.config.model.as_str(),
            "thinking": self.config.thinking,
            "messages": state.messages,
            "calls": state.calls,
            "date": state.date,
            "disclosed": state.disclosed,
            "rebase": state.rebase,
            "todos": self.config.builtins.as_ref().map(|builtins| builtins.todos()),
        })
    }
}

fn unsupported<T: Send + 'static>(capability: &'static str) -> BackendFuture<Result<T>> {
    Box::pin(async move { Err(NanocodeError::UnsupportedCapability { capability }) })
}

fn done() -> BackendFuture<Result<()>> {
    Box::pin(async { Ok(()) })
}

impl Driver {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(State::default()),
                cancel: Mutex::new(None),
                stopped: AtomicBool::new(false),
                uploads: Mutex::default(),
                no_files: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub fn restore(self, checkpoint: &SessionCheckpoint) -> Result<Self> {
        let payload: Value = checkpoint
            .payload()
            .decode()
            .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?;
        self.restore_payload(&payload)
    }

    pub fn restore_payload(self, payload: &Value) -> Result<Self> {
        let messages: Vec<Message> = payload
            .get("messages")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?
            .unwrap_or_default();
        if let Ok(mut state) = self.inner.state.lock() {
            state.messages = messages;
            state.calls = payload.get("calls").and_then(Value::as_u64).unwrap_or(0);
            state.date = payload
                .get("date")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state.disclosed = payload
                .get("disclosed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            state.rebase = payload
                .get("rebase")
                .cloned()
                .and_then(|rebase| serde_json::from_value(rebase).ok());
        }
        if let (Some(builtins), Some(todos)) = (&self.inner.config.builtins, payload.get("todos")) {
            builtins.restore_todos(todos);
        }
        Ok(self)
    }

    fn start(&self, prompt: BackendPrompt) -> BackendTurn {
        let inner = Arc::clone(&self.inner);
        let request_id = prompt
            .request_id
            .clone()
            .unwrap_or_else(|| format!("{}-{}", inner.config.session, prompt.key.0));
        let cancel = Arc::new(Signal::new());
        if let Ok(mut slot) = inner.cancel.lock() {
            *slot = Some(Arc::clone(&cancel));
        }
        let events = prompt.events.clone();
        let id = request_id.clone();
        let (sender, receiver) = nanocode_sync::oneshot::channel();
        nanocode_platform::spawn(async move {
            let run =
                Box::pin(Arc::clone(&inner).run(prompt.prompt.instruction.clone(), events, id));
            let stop = Box::pin(async move { cancel.wait().await });
            let result = match select(run, stop).await {
                Either::Left((result, _)) => result,
                Either::Right(_) => {
                    let mut state = inner.load();
                    let answered = state
                        .messages
                        .iter()
                        .filter_map(|message| match message {
                            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                            _ => None,
                        })
                        .collect::<BTreeSet<_>>();
                    if let Some(position) = state.messages.iter().rposition(|message| {
                        matches!(message, Message::Assistant { tool_calls, .. }
                            if tool_calls.iter().any(|call| !answered.contains(&call.id)))
                    }) {
                        state.messages.truncate(position);
                    }
                    inner.save(&state);
                    Err(NanocodeError::TurnCancelled)
                }
            };
            let result = match inner.persist().await {
                Ok(()) => result,
                Err(error) => Err(error),
            };
            drop(sender.send(result));
        });
        let result =
            Box::pin(async move { receiver.await.unwrap_or(Err(NanocodeError::TurnStopped)) });
        BackendTurn {
            request_id: Some(request_id),
            result,
        }
    }

    fn cancel_turn(&self) -> BackendFuture<Result<()>> {
        if let Ok(slot) = self.inner.cancel.lock()
            && let Some(cancel) = slot.as_ref()
        {
            cancel.fire();
        }
        done()
    }
}

impl LifecycleBackend for Driver {
    fn harness_family(&self) -> HarnessFamily {
        HarnessFamily::new(crate::harness::FAMILY)
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::NONE;
        capabilities.checkpoint = true;
        capabilities.compaction = true;
        capabilities
    }

    fn submit(&self, prompt: BackendPrompt) -> BackendFuture<Result<BackendTurn>> {
        if !prompt.carried_steers.is_empty() {
            return Box::pin(async {
                Err(NanocodeError::UnsupportedCapability {
                    capability: "carried_steers",
                })
            });
        }
        let turn = self.start(prompt);
        Box::pin(async move { Ok(turn) })
    }

    fn route(&self, prompt: BackendPrompt) -> BackendFuture<Result<BackendPromptRoute>> {
        let turn = self.start(prompt);
        Box::pin(async move { Ok(BackendPromptRoute::Started(turn)) })
    }

    fn steer(
        &self,
        _key: BackendTurnKey,
        _prompt: nanocode_agent_core::input::Prompt,
    ) -> BackendFuture<Result<()>> {
        unsupported("steering")
    }

    fn cancel(&self, _key: BackendTurnKey) -> BackendFuture<Result<()>> {
        self.cancel_turn()
    }

    fn cancel_active(&self) -> BackendFuture<Result<()>> {
        self.cancel_turn()
    }

    fn set_harness_model(&self, _model: ModelSelection) -> BackendFuture<Result<()>> {
        unsupported("model_switch")
    }

    fn set_thinking(&self, _thinking: Thinking) -> BackendFuture<Result<()>> {
        unsupported("thinking_switch")
    }

    fn set_fast_mode(&self, _enabled: bool) -> BackendFuture<Result<()>> {
        done()
    }

    fn compact(&self) -> BackendFuture<Result<()>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let (events, _receiver) =
                nanocode_agent_core::backend::BackendRuntime::new(inner.config.session.clone());
            let publisher = events.events();
            let mut state = inner.load();
            if state.messages.is_empty() {
                return Ok(());
            }
            inner.compact(&mut state, &publisher).await?;
            inner.save(&state);
            Ok(())
        })
    }

    fn append_developer_message(
        &self,
        _text: String,
    ) -> BackendFuture<Result<AgentSessionContext>> {
        unsupported("developer_messages")
    }

    fn context(&self) -> BackendFuture<Result<AgentSessionContext>> {
        unsupported("context")
    }

    fn checkpoint(&self) -> BackendFuture<Result<SessionCheckpoint>> {
        let checkpoint = self.inner.checkpoint();
        Box::pin(async move { Ok(checkpoint) })
    }

    fn spawn(&self, _options: SpawnOptions) -> BackendFuture<Result<(Agent, AgentEvents)>> {
        unsupported("spawn")
    }

    fn fork(&self, _request: ForkRequest) -> BackendFuture<Result<(Agent, AgentEvents)>> {
        unsupported("fork")
    }

    fn flush(&self) -> BackendFuture<Result<()>> {
        done()
    }

    fn shutdown(&self) -> BackendFuture<Result<()>> {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let builtins = self.inner.config.builtins.clone();
        Box::pin(async move {
            if let Some(builtins) = builtins {
                builtins.shutdown().await;
            }
            Ok(())
        })
    }

    fn set_parking(&self, _parking: bool) {}

    fn parked(&self, _after: u64) -> BackendFuture<u64> {
        Box::pin(std::future::pending())
    }

    fn park(&self) -> BackendFuture<Result<Option<Arc<dyn ParkedCall>>>> {
        Box::pin(async { Ok(None) })
    }

    fn unpark(&self, _call: Arc<dyn ParkedCall>) {}

    fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }
}

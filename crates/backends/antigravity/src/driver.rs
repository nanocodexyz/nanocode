use crate::nanotools::NanoTools;
use crate::profile::{Profile, SKIP_SIGNATURE, Thinking as ProfileThinking};
use crate::prompts;
use crate::request::{Ids, Prefix, Request};
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
use nanocode_backend_antigravity_client::{Client, Content, ExplicitCache, Part, Reply};
use nanocode_backend_antigravity_tools::{
    Engine, NAMES, Outcome, REMINDER, local_timestamp, system_message, with_header,
};
use nanocode_sync::Signal;
use nanocode_sync::future::{Either, select};
use nanocode_tool_host::ToolContext;
use serde_json::{Value, json, value::RawValue};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const USAGE_LOG: &str = "usage.jsonl";

pub struct Config {
    pub client: Client,
    pub profile: Profile,
    pub prefix: Prefix,
    pub ids: Ids,
    pub engine: Engine,
    pub nano: NanoTools,
    pub model: ModelSelection,
    pub thinking: Thinking,
    pub session: String,
    pub cache: Option<Arc<ExplicitCache>>,
    pub logs: PathBuf,
    pub notice: Option<String>,
}

#[derive(Clone, Default)]
pub struct State {
    contents: Vec<Content>,
    step: u64,
    calls: u64,
    streak: u32,
    requests: Vec<String>,
    settings_sent: bool,
    prompt_tokens: u64,
    transcript: Vec<Content>,
    last_execution: Option<String>,
    executions: u64,
}

struct Inner {
    config: Config,
    state: Mutex<State>,
    cancel: Mutex<Option<Arc<Signal>>>,
    stopped: AtomicBool,
    noticed: AtomicBool,
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

fn normalize(parts: Vec<Part>, skip: bool) -> Vec<Part> {
    let mut carry = None;
    let mut first_call = true;
    let mut out = Vec::with_capacity(parts.len());
    for mut part in parts {
        if part.is_thought() {
            if part.thought_signature.is_some() {
                carry = part.thought_signature.take();
            }
            out.push(part);
            continue;
        }
        if part.thought_signature.is_none() {
            part.thought_signature = carry.take();
        }
        if part.function_call.is_some() {
            if first_call && skip && part.thought_signature.is_none() {
                part.thought_signature = Some(SKIP_SIGNATURE.to_owned());
            }
            first_call = false;
        }
        out.push(part);
    }
    out
}

fn visible(parts: &[Part]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_thought())
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn append(part: &mut Part, text: &str) {
    if let Some(response) = part.function_response.as_mut()
        && let Some(map) = response.response.as_object_mut()
        && let Some((_, Value::String(body))) = map.iter_mut().next()
    {
        body.push_str("\n\n");
        body.push_str(text);
    }
}

impl State {
    fn push(&mut self, content: Content) {
        self.transcript.push(content.clone());
        self.contents.push(content);
    }
}

impl Inner {
    fn now() -> std::time::SystemTime {
        nanocode_platform::system_time()
    }

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

    async fn request(
        &self,
        state: &State,
        extra: Option<&Content>,
        max_output_tokens: u32,
        call: u64,
        events: &AgentEventPublisher,
    ) -> Result<Reply> {
        let config = &self.config;
        let cached = match &config.cache {
            Some(cache) => {
                cache
                    .resolve(
                        &config.client,
                        &config.profile.model,
                        &config.prefix.system,
                        &config.prefix.tools,
                    )
                    .await
            }
            None => None,
        };
        let body = Request {
            contents: &state.contents,
            extra,
            prefix: &config.prefix,
            cached: cached.as_deref(),
            profile: &config.profile,
            ids: &config.ids,
            step: state.step,
            call,
            max_output_tokens,
            execution: state.last_execution.as_deref(),
            labels: !config.client.endpoint().route.is_vertex(),
        }
        .encode();
        emit(
            events,
            AgentEventKind::ModelCallStarted,
            json!({
                "call_index": call,
                "model_call_index": call,
                "model": config.profile.model,
                "reasoning_mode": "standard",
                "effort": match config.profile.thinking {
                    ProfileThinking::Level(level) => level.to_ascii_lowercase(),
                    ProfileThinking::Budget(budget) => budget.to_string(),
                },
            }),
        );
        let started = nanocode_platform::now();
        let nanos = |since: nanocode_platform::Instant| {
            u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
        };
        let mut first_event = None;
        let mut first_output = None;
        let mut on_part = |part: &Part| {
            first_event.get_or_insert_with(|| nanos(started));
            if let Some(text) = part.text.as_deref().filter(|_| !part.is_thought()) {
                first_output.get_or_insert_with(|| nanos(started));
                emit(
                    events,
                    AgentEventKind::AssistantDelta,
                    json!({"model_call_index": call, "item_id": format!("call-{call}"), "text": text}),
                );
            }
        };
        let reply = config
            .client
            .stream(&config.profile.model, &body, &mut on_part)
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                emit(
                    events,
                    AgentEventKind::ModelCallFailed,
                    json!({
                        "call_index": call,
                        "model_call_index": call,
                        "model": config.profile.model,
                        "duration_ns": nanos(started),
                        "error": error.to_string(),
                    }),
                );
                return Err(NanocodeError::InvalidRequest(error.to_string()));
            }
        };
        let usage = reply.usage;
        let output_tokens = usage.candidates_token_count + usage.thoughts_token_count;
        emit(
            events,
            AgentEventKind::ModelCallCompleted,
            json!({
                "call_index": call,
                "model_call_index": call,
                "model": config.profile.model,
                "attempt": 1,
                "connection_generation": 0,
                "status": reply
                    .finish_reason
                    .as_deref()
                    .unwrap_or("STOP")
                    .to_ascii_lowercase(),
                "duration_ns": nanos(started),
                "time_to_first_event_ns": first_event.unwrap_or_else(|| nanos(started)),
                "time_to_first_output_ns": first_output,
                "tool_calls": reply.parts.iter().filter(|part| part.function_call.is_some()).count(),
                "usage": {
                    "input_tokens": usage.prompt_token_count,
                    "input_tokens_details": {
                        "cached_tokens": usage.cached_content_token_count,
                        "cache_write_tokens": 0,
                    },
                    "output_tokens": output_tokens,
                    "output_tokens_details": {"reasoning_tokens": usage.thoughts_token_count},
                    "total_tokens": usage.total_token_count.max(usage.prompt_token_count + output_tokens),
                },
            }),
        );
        self.log_usage(call, &config.profile.model, usage);
        Ok(reply)
    }

    fn log_usage(&self, call: u64, model: &str, usage: nanocode_backend_antigravity_client::Usage) {
        use std::io::Write as _;
        let line = json!({
            "call_index": call,
            "model": model,
            "prompt_tokens": usage.prompt_token_count,
            "cache_read_tokens": usage.cached_content_token_count,
            "output_tokens": usage.candidates_token_count,
            "thoughts_tokens": usage.thoughts_token_count,
            "total_tokens": usage.total_token_count,
        });
        let _ = std::fs::create_dir_all(&self.config.logs);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.config.logs.join(USAGE_LOG))
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn write_transcript(&self, state: &State) {
        let _ = std::fs::create_dir_all(&self.config.logs);
        let mut full = String::new();
        let mut compact = String::new();
        for (index, content) in state.transcript.iter().enumerate() {
            let line = content.to_json().encode();
            full.push_str(&line);
            full.push('\n');
            let mut truncated = Vec::new();
            let mut short = content.clone();
            for part in &mut short.parts {
                if let Some(text) = part.text.as_mut()
                    && text.len() > 2_000
                {
                    let mut cut = 2_000;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    truncated.push("text");
                }
                if let Some(response) = part.function_response.as_mut()
                    && let Some(map) = response.response.as_object_mut()
                {
                    for (key, value) in map.iter_mut() {
                        if let Value::String(body) = value
                            && body.len() > 2_000
                        {
                            let mut cut = 2_000;
                            while !body.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            body.truncate(cut);
                            truncated.push(if key == "error" { "error" } else { "output" });
                        }
                    }
                }
            }
            let mut value: Value =
                serde_json::from_str(&short.to_json().encode()).unwrap_or(Value::Null);
            if let Some(map) = value.as_object_mut() {
                map.insert("step".into(), json!(index));
                if !truncated.is_empty() {
                    map.insert("truncated_fields".into(), json!(truncated));
                }
            }
            compact.push_str(&value.to_string());
            compact.push('\n');
        }
        let _ = std::fs::write(self.config.logs.join("transcript_full.jsonl"), full);
        let _ = std::fs::write(self.config.logs.join("transcript.jsonl"), compact);
    }

    async fn compact(&self, state: &mut State, events: &AgentEventPublisher) -> Result<()> {
        emit(events, AgentEventKind::ModelCompactionStarted, json!({}));
        self.write_transcript(state);
        let extra = Content::user(vec![Part::text(prompts::COMPACTION)]);
        let reply = self
            .request(
                state,
                Some(&extra),
                self.config.profile.compaction_output_tokens,
                state.calls,
                events,
            )
            .await;
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
        let summary = visible(&reply.parts);
        let keep = self.config.profile.max_user_requests;
        let requests = state.requests[state.requests.len().saturating_sub(keep)..].to_vec();
        let text = prompts::context_summary(
            &local_timestamp(Self::now()),
            &requests,
            summary.trim(),
            &self.config.engine.snapshots(),
            &self.config.logs,
        );
        state.contents = vec![Content::user(vec![Part::text(text.clone())])];
        state.transcript.push(Content::user(vec![Part::text(text)]));
        state.step += 1;
        state.prompt_tokens = 0;
        state.streak = 0;
        emit(events, AgentEventKind::ModelCompactionCompleted, json!({}));
        Ok(())
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        step: u64,
        call_id: &str,
        turn: &str,
    ) -> (&'static str, String) {
        let created = Self::now();
        let engine = &self.config.engine;
        if NAMES.contains(&name) {
            return match engine.call(name, args, step).await {
                Outcome::Output(body) => {
                    let full = with_header(created, Self::now(), &body);
                    ("output", engine.finish_output(name, step, &full).await)
                }
                Outcome::Background(text) => {
                    let _ = engine.finish_output("read_file", step, &text).await;
                    ("output", text)
                }
                Outcome::Error(error) => ("error", error),
            };
        }
        if !self.config.nano.handles(name) {
            return (
                "error",
                format!("Encountered error in tool validation: tool {name:?} not found"),
            );
        }
        let context = ToolContext::new(
            self.config.model.as_str(),
            &self.config.session,
            call_id,
            &[],
            usize::MAX,
        )
        .with_turn_id(Some(turn));
        match self.config.nano.call(name, args, context).await {
            Ok(body) => {
                let full = with_header(created, Self::now(), &body);
                ("output", engine.finish_output(name, step, &full).await)
            }
            Err(error) => (
                "error",
                format!("Encountered error in tool execution: {error}"),
            ),
        }
    }

    fn finish_execution(&self, state: &mut State) {
        state.executions += 1;
        state.last_execution = Some(crate::harness::new_id(&format!(
            "{}-execution-{}-{:?}",
            self.config.session,
            state.executions,
            Self::now()
        )));
    }

    async fn deliver_idle(&self, state: &mut State) -> bool {
        let messages = self.config.engine.take_messages();
        let delivered = !messages.is_empty();
        for message in messages {
            state.step += 1;
            state.push(Content::user(vec![Part::text(system_message(
                &message.render(),
            ))]));
        }
        delivered
    }

    async fn run(
        self: Arc<Self>,
        input: PromptInput,
        events: AgentEventPublisher,
        request_id: String,
    ) -> Result<TurnResult> {
        let mut state = self.load();
        if let Some(notice) = &self.config.notice
            && !self.noticed.swap(true, Ordering::SeqCst)
        {
            emit(
                &events,
                AgentEventKind::RunError,
                json!({"error": notice, "source": "antigravity", "observational": true}),
            );
        }
        let text = prompt_text(&input);
        if !state.contents.is_empty() {
            state.step += 1;
        }
        let settings = (!state.settings_sent).then(|| self.config.profile.display.clone());
        state.settings_sent = true;
        state.requests.push(text.clone());
        state.push(Content::user(vec![Part::text(prompts::user_request(
            &text,
            &local_timestamp(Self::now()),
            settings.as_deref(),
        ))]));
        let mut final_message = String::new();
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
            if state.prompt_tokens >= self.config.profile.compaction_limit {
                self.compact(&mut state, &events).await?;
                self.save(&state);
            }
            let call_index = state.calls;
            let reply = self
                .request(
                    &state,
                    None,
                    self.config.profile.max_output_tokens,
                    call_index,
                    &events,
                )
                .await?;
            state.calls += 1;
            totals.input_tokens += reply.usage.prompt_token_count;
            totals.cached_input_tokens += reply.usage.cached_content_token_count;
            totals.output_tokens +=
                reply.usage.candidates_token_count + reply.usage.thoughts_token_count;
            totals.reasoning_output_tokens += reply.usage.thoughts_token_count;
            totals.total_tokens += reply.usage.total_token_count;
            state.prompt_tokens = reply.usage.prompt_token_count;
            state.step += 1;
            let parts = normalize(reply.parts, self.config.profile.skip_signature);
            let message = visible(&parts);
            if !message.is_empty() {
                final_message = message.clone();
                emit(
                    &events,
                    AgentEventKind::AssistantMessage,
                    json!({"model_call_index": call_index, "item_id": format!("call-{call_index}"), "text": message}),
                );
            }
            let calls = parts
                .iter()
                .filter_map(|part| part.function_call.clone())
                .collect::<Vec<_>>();
            state.push(Content::model(parts));
            if calls.is_empty() {
                state.streak += 1;
                self.save(&state);
                let engine = &self.config.engine;
                if !engine.has_urgent() && !engine.blocking() {
                    break;
                }
                engine.wait().await;
                self.finish_execution(&mut state);
                if !self.deliver_idle(&mut state).await {
                    break;
                }
                if state.streak >= 2 {
                    state.streak = 0;
                    state.step += 1;
                    state.push(Content::user(vec![Part::text(system_message(REMINDER))]));
                }
                self.save(&state);
                continue;
            }
            let mut responses = Vec::with_capacity(calls.len());
            let mut effective = 0;
            for (index, call) in calls.iter().enumerate() {
                state.step += 1;
                let call_id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("{request_id}-{call_index}-{index}"));
                emit(
                    &events,
                    AgentEventKind::ToolCall,
                    json!({"call_id": call_id, "tool": call.name, "arguments": call.args, "model_call_index": call_index}),
                );
                let (key, body) = self
                    .execute(&call.name, &call.args, state.step, &call_id, &request_id)
                    .await;
                emit(
                    &events,
                    AgentEventKind::ToolResult,
                    json!({"call_id": call_id, "tool": call.name, "result": body, "status": if key == "error" { "failed" } else { "completed" }}),
                );
                let passive = match call.name.as_str() {
                    "schedule" => true,
                    "manage_task" => {
                        call.args.get("Action").and_then(Value::as_str) != Some("list")
                    }
                    _ => false,
                };
                if !passive {
                    effective += 1;
                }
                responses.push(Part::response(call.name.clone(), key, body));
            }
            for message in self.config.engine.take_messages() {
                state.step += 1;
                if let Some(last) = responses.last_mut() {
                    append(last, &system_message(&message.render()));
                }
            }
            state.streak = match effective {
                0 => state.streak,
                1 => state.streak + 1,
                _ => 0,
            };
            if state.streak >= 2 {
                state.streak = 0;
                state.step += 1;
                if let Some(last) = responses.last_mut() {
                    append(last, &system_message(REMINDER));
                }
            }
            state.push(Content::user(responses));
            self.save(&state);
        }
        self.finish_execution(&mut state);
        self.save(&state);
        self.write_transcript(&state);
        Ok(TurnResult {
            request_id: Some(request_id),
            turn_id: None,
            final_message,
            usage: Some(TurnUsage::from_reported(totals)),
            boundary: None,
        })
    }

    fn checkpoint(&self) -> SessionCheckpoint {
        let state = self.load();
        let payload = json!({
            "format": crate::durability::FORMAT,
            "model": self.config.model.as_str(),
            "thinking": self.config.thinking,
            "contents": state.contents.iter().map(|content| serde_json::to_value(content).unwrap_or(Value::Null)).collect::<Vec<_>>(),
            "step": state.step,
            "calls": state.calls,
            "requests": state.requests,
        });
        SessionCheckpoint::native(
            self.config.session.clone(),
            self.config.model.clone(),
            self.config.thinking,
            Lineage::root(self.config.session.clone()),
            self.config.session.clone(),
            None,
            true,
            payload,
        )
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
                noticed: AtomicBool::new(false),
            }),
        }
    }

    pub fn restore(self, checkpoint: &SessionCheckpoint) -> Result<Self> {
        let payload: Value = checkpoint
            .payload()
            .decode()
            .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?;
        let contents: Vec<Content> = payload
            .get("contents")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?
            .unwrap_or_default();
        let number = |field: &str| payload.get(field).and_then(Value::as_u64).unwrap_or(0);
        let requests = payload
            .get("requests")
            .and_then(Value::as_array)
            .map(|requests| {
                requests
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if let Ok(mut state) = self.inner.state.lock() {
            state.settings_sent = !contents.is_empty();
            state.transcript = contents.clone();
            state.contents = contents;
            state.step = number("step");
            state.calls = number("calls");
            state.requests = requests;
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
                    if state.contents.last().is_some_and(|content| {
                        content.role == "model" && content.calls().next().is_some()
                    }) {
                        state.contents.pop();
                    }
                    inner.save(&state);
                    Err(NanocodeError::TurnCancelled)
                }
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
            if state.contents.is_empty() {
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
        let inner = Arc::clone(&self.inner);
        if inner.stopped.swap(true, Ordering::SeqCst) {
            return done();
        }
        Box::pin(async move {
            inner.config.engine.shutdown().await;
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

use std::{path::PathBuf, sync::Arc};

use crate::{
    ClaudeNestedToolUpdate, ClaudeToolInvocation, ClaudeToolProgress, ClaudeToolReply, ClaudeTools,
    ToolDefinition, ToolResultContent,
    agent::TOOL_RESULT_TOKEN_LIMIT,
    prompt::{AUDIO_UNSUPPORTED, IMAGE_FILE_UNSUPPORTED, IMAGE_UNPROCESSABLE},
};
use nanocode_agent_core::{AgentHandle, NanocodeError, Result};
use nanocode_tool_host::{
    ToolContext, ToolDefinition as RuntimeDefinition, ToolInput, ToolOutput, Tools,
    contract::{ToolOutputBody, ToolOutputContent},
    embedded::{
        CodeModeExecution, CodeModeObserver, CodeModeUpdate, NestedToolCall, OwnedToolContext,
    },
    runtime::ToolRuntime,
};
use serde_json::{Value, json};

const SURFACED_DOCUMENTS: usize = 4;

fn exec_schema(cell_yield: Option<std::time::Duration>) -> Arc<Value> {
    type Schema = (std::time::Duration, Arc<Value>);
    static DEFAULT: std::sync::LazyLock<Arc<Value>> = std::sync::LazyLock::new(|| {
        build_exec_schema(nanocode_tool_host::code_mode_spec::DEFAULT_YIELD)
    });
    static LATEST: std::sync::LazyLock<nanocode_sync::Atom<Option<Schema>>> =
        std::sync::LazyLock::new(nanocode_sync::Atom::default);
    let Some(after) = cell_yield else {
        return Arc::clone(&DEFAULT);
    };
    if let Some(schema) = LATEST.read(|latest| {
        latest
            .as_ref()
            .filter(|(known, _)| *known == after)
            .map(|(_, schema)| Arc::clone(schema))
    }) {
        return schema;
    }
    let schema = build_exec_schema(after);
    LATEST.set(Some((after, Arc::clone(&schema))));
    schema
}

fn build_exec_schema(after: std::time::Duration) -> Arc<Value> {
    Arc::new(json!({
        "type": "object",
        "properties": {
            "code": {"type": "string", "description": "JavaScript source to evaluate."},
            "yield_time_ms": {"type": "number", "description": format!("Same as the pragma's `yield_time_ms`: how long to wait before a script that is still running yields. Defaults to {} ms.", after.as_millis())},
            "max_output_tokens": {"type": "number", "description": "Same as the pragma's `max_output_tokens`: the token budget for this call's direct results. Omit for no budget of its own."}
        },
        "required": ["code"],
        "additionalProperties": false,
    }))
}

fn shared_description(name: &str, description: String) -> Arc<str> {
    static DESCRIPTIONS: std::sync::LazyLock<
        nanocode_sync::Atom<std::collections::HashMap<String, Arc<str>>>,
    > = std::sync::LazyLock::new(nanocode_sync::Atom::default);
    if let Some(shared) = DESCRIPTIONS.read(|known| {
        known
            .get(name)
            .filter(|known| ***known == *description)
            .cloned()
    }) {
        return shared;
    }
    let shared: Arc<str> = Arc::from(description);
    DESCRIPTIONS.update(|known| {
        known.insert(name.to_owned(), Arc::clone(&shared));
    });
    shared
}

struct ExecDescription {
    source: Arc<str>,
    shared: Arc<str>,
}

fn exec_description(description: &Arc<str>) -> Arc<str> {
    static LATEST: std::sync::LazyLock<nanocode_sync::Atom<Option<ExecDescription>>> =
        std::sync::LazyLock::new(nanocode_sync::Atom::default);
    if let Some(shared) = LATEST.read(|latest| {
        latest
            .as_ref()
            .filter(|latest| Arc::ptr_eq(&latest.source, description))
            .map(|latest| Arc::clone(&latest.shared))
    }) {
        return shared;
    }
    let shared = shared_description(
        "exec",
        format!(
            "{description}\n\nPass the complete JavaScript source as the `code` string. `yield_time_ms` and `max_output_tokens` may be given as arguments beside `code` instead of in the pragma.\n\nImages reach you at the size the model accepts, whatever their `detail`; an image `file_id` arrives as a note that it was omitted. A result keeps at most {TOOL_RESULT_TOKEN_LIMIT} tokens of text (about 4 characters each), cut in the middle at `…N tokens truncated…`."
        ),
    );
    LATEST.set(Some(ExecDescription {
        source: Arc::clone(description),
        shared: Arc::clone(&shared),
    }));
    shared
}

pub fn install_shared_tools(
    workspace: PathBuf,
    handle: &AgentHandle,
    selected: Tools,
) -> Result<ClaudeTools> {
    install(&selected, workspace, handle, false)
}

pub(super) fn install(
    tools: &Tools,
    workspace: PathBuf,
    handle: &AgentHandle,
    headless: bool,
) -> Result<ClaudeTools> {
    let tools = tools
        .clone()
        .for_session(&handle.session_environment())
        .for_backend(handle.session_id(), "claude")
        .with_code_mode_policy(crate::code_mode_policy::claude_tool_policy());
    let runtime = Arc::new(ToolRuntime::new_with_tools(
        workspace.clone(),
        Vec::new(),
        &tools,
    ));

    let guard = Arc::new(RuntimeGuard {
        control: runtime.control(),
        #[cfg(not(target_family = "wasm"))]
        cancel_on_drop: !tools.has_code_mode_host(),
    });
    let mut native = ClaudeTools::new();
    if cfg!(target_family = "wasm") || tools.has_code_mode_host() {
        let shutdown = Arc::clone(&guard);
        native = native.adapter_shutdown(move || {
            let shutdown = Arc::clone(&shutdown);
            async move { shutdown.control.cancel().await }
        });
        native.shared_shutdown = native.adapter_shutdown.last().cloned();
    }
    let nested = Arc::clone(&runtime);
    native.nested_tools = Some(Arc::new(move |name| nested.contains(name)));
    native.recovers_tools = runtime.recovers_tools();
    let recovery = Arc::clone(&runtime);
    native.recovery = Some(Arc::new(move |name, invocation| {
        let runtime = Arc::clone(&recovery);
        let name = name.to_owned();
        Box::pin(async move { Call::recover(&runtime, &name, &invocation).await })
    }));
    let preempt = Arc::clone(&guard);
    native.code_turn_preempt.push(Arc::new(move || {
        let guard = Arc::clone(&preempt);
        Box::pin(async move { guard.control.preempt_turn().await })
    }));
    let quiet = Arc::clone(&guard);
    native
        .code_quiet
        .push(Arc::new(move || quiet.control.quiet()));
    let settle = Arc::clone(&guard);
    native.code_turn_settle.push(Arc::new(move || {
        let guard = Arc::clone(&settle);
        Box::pin(async move { guard.control.settle_preempt() })
    }));
    native.code_turn_cleanup.push(Arc::new(move |progress| {
        let guard = Arc::clone(&guard);
        Box::pin(async move {
            guard
                .control
                .cancel_turn_with_updates(&mut Progress(Some(&progress)))
                .await;
            guard.control.begin_turn();
        })
    }));
    let cell_yield = tools.cell_yield();
    let held =
        (headless && tools.has_code_mode_host() && handle.root_session_id() == handle.session_id())
            .then(Arc::<Watches>::default);
    if tools.exposure() != nanocode_tool_host::ToolExposure::DirectOnly
        || !tools.has_code_mode_host()
    {
        native = catalog(native, &runtime, handle.session_id(), cell_yield, None)?;
    } else {
        let dynamic = Arc::clone(&runtime);
        let session_id = handle.session_id().to_owned();
        let watches = held.clone();
        native = native.dynamic_tools(move || {
            catalog(ClaudeTools::new(), &dynamic, &session_id, cell_yield, Some(watches.as_ref())).unwrap_or_else(|error| {
                nanocode_trace::warn!(target: "nanocode_claude", %error, "direct hand tools are unavailable");
                ClaudeTools::new()
            })
        });
    }
    if let Some(watches) = held {
        native = hold_hand_jobs(
            native,
            &runtime,
            handle.session_id(),
            cell_yield.unwrap_or(nanocode_tool_host::code_mode_spec::DEFAULT_YIELD),
            watches,
        );
    }
    native.shared_catalog = Some((tools, workspace, handle.clone()));
    Ok(native)
}

fn hold_hand_jobs(
    native: ClaudeTools,
    runtime: &Arc<ToolRuntime>,
    session_id: &str,
    wait: std::time::Duration,
    watches: Arc<Watches>,
) -> ClaudeTools {
    let steer: Arc<nanocode_sync::Atom<Option<crate::agent::Steer>>> = Arc::default();
    let bound = Arc::clone(&steer);
    let runtime = Arc::clone(runtime);
    let session_id: Arc<str> = Arc::from(session_id);
    native
        .agent_bound(move |_, steer| {
            bound.set(Some(Arc::clone(steer)));
        })
        .turn_hold(move || {
            let runtime = Arc::clone(&runtime);
            let session_id = Arc::clone(&session_id);
            let steer = Option::clone(&steer.get());
            let watches = Arc::clone(&watches);
            async move {
                loop {
                    let wait = watches.left().map_or(wait, |left| left.min(wait));
                    let jobs = match runtime.hand_notices(&session_id, wait).await {
                        Ok(jobs) => jobs,
                        Err(error) => {
                            nanocode_trace::warn!(target: "nanocode_claude", %error, "background job notices from the hand are unavailable");
                            return;
                        }
                    };
                    let notices = watches.notices(&runtime, &session_id, jobs.notices).await;
                    let prompt = match notices.is_empty() {
                        true => watches.due(),
                        false => None,
                    };
                    if notices.is_empty() && prompt.is_none() {
                        if jobs.busy {
                            continue;
                        }
                        match watches.left() {
                            Some(left) => {
                                nanocode_platform::sleep(left).await;
                                continue;
                            }
                            None => return,
                        }
                    }
                    let Some(steer) = steer else {
                        nanocode_trace::warn!(target: "nanocode_claude", "background job notices found no bound agent");
                        return;
                    };
                    for notice in notices {
                        if !steer(crate::notification::reminder(&notice)).await {
                            nanocode_trace::debug!("background job notice found no running turn");
                        }
                    }
                    if let Some(prompt) = prompt
                        && !steer(prompt).await
                    {
                        nanocode_trace::debug!("scheduled wakeup found no running turn");
                    }
                    return;
                }
            }
        })
}

#[derive(Default)]
struct Watches {
    monitors: nanocode_sync::Atom<Vec<Monitor>>,
    polls: std::sync::atomic::AtomicU64,
    wakeup: nanocode_sync::Atom<Option<Wakeup>>,
}

#[derive(Clone)]
struct Monitor {
    call_id: String,
    description: String,
    events: u64,
    id: String,
    model: String,
    output: String,
    read: u64,
    started: std::time::SystemTime,
    timeout_ms: u64,
}

#[derive(Clone)]
struct Wakeup {
    at: std::time::SystemTime,
    prompt: String,
}

impl Watches {
    fn left(&self) -> Option<std::time::Duration> {
        self.wakeup.read(|wakeup| {
            wakeup.as_ref().map(|wakeup| {
                wakeup
                    .at
                    .duration_since(nanocode_platform::system_time())
                    .unwrap_or_default()
            })
        })
    }

    fn due(&self) -> Option<String> {
        let now = nanocode_platform::system_time();
        self.wakeup.update(
            |wakeup| match wakeup.as_ref().is_some_and(|wakeup| wakeup.at <= now) {
                true => wakeup.take().map(|wakeup| wakeup.prompt),
                false => None,
            },
        )
    }

    async fn notices(
        &self,
        runtime: &ToolRuntime,
        session_id: &str,
        notices: Vec<String>,
    ) -> Vec<String> {
        let mut texts = Vec::new();
        for monitor in self.monitors.get().iter() {
            let poll = self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let Some((events, read)) = monitor_events(runtime, session_id, monitor, poll).await
            else {
                continue;
            };
            let delivered = u64::from(!events.is_empty());
            self.monitors.update(|monitors| {
                if let Some(known) = monitors.iter_mut().find(|known| known.id == monitor.id) {
                    known.read += read;
                    known.events += delivered;
                }
            });
            if !events.is_empty() {
                texts.push(monitor_notification(monitor, &events));
            }
        }
        for notice in notices {
            let ended = self.monitors.update(|monitors| {
                let at = monitors.iter().position(|monitor| {
                    notice.contains(&format!("<task-id>{}</task-id>", monitor.id))
                })?;
                Some(monitors.remove(at))
            });
            texts.push(match ended {
                Some(monitor) if monitor_expired(&monitor) => {
                    monitor_notification(&monitor, &monitor_expiry(&monitor))
                }
                _ => notice,
            });
        }
        texts
    }
}

const MONITOR_MAX_TIMEOUT_MS: u64 = 600_000;
const WAKEUP_DELAY_SECONDS: std::ops::RangeInclusive<f64> = 60.0..=3600.0;

fn watch_tools(
    native: ClaudeTools,
    runtime: &Arc<ToolRuntime>,
    watches: &Arc<Watches>,
) -> ClaudeTools {
    let monitor = crate::upstream::tool_definition("Monitor")
        .expect("upstream Monitor definition")
        .clone();
    let wakeup = crate::upstream::tool_definition("ScheduleWakeup")
        .expect("upstream ScheduleWakeup definition")
        .clone();
    let runtime = Arc::clone(runtime);
    let started = Arc::clone(watches);
    let scheduled = Arc::clone(watches);
    native
        .tool_with_context(monitor, move |input, invocation| {
            let runtime = Arc::clone(&runtime);
            let watches = Arc::clone(&started);
            async move { start_monitor(&runtime, &watches, &input, &invocation).await }
        })
        .tool_with_context(wakeup, move |input, _| {
            let watches = Arc::clone(&scheduled);
            async move { schedule_wakeup(&watches, &input) }
        })
}

async fn start_monitor(
    runtime: &ToolRuntime,
    watches: &Watches,
    input: &Value,
    invocation: &ClaudeToolInvocation,
) -> std::result::Result<ClaudeToolReply, String> {
    if input.get("ws").is_some() {
        return Err("Monitor cannot open a WebSocket here: this hand runs commands only, so watch the source with a `command`.".into());
    }
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .ok_or("Monitor needs a `command`.")?;
    let description = input
        .get("description")
        .and_then(Value::as_str)
        .ok_or("Monitor needs a `description`.")?;
    let timeout_ms = match input.get("timeout_ms") {
        Some(timeout) => timeout
            .as_u64()
            .ok_or("`timeout_ms` must be a whole number of milliseconds.")?,
        None => {
            let schema = nanocode_backend_claude_tools::upstream::tool("Monitor").input_schema();
            schema
                .as_value()
                .pointer("/properties/timeout_ms/default")
                .and_then(Value::as_u64)
                .expect("upstream Monitor default timeout")
        }
    }
    .min(MONITOR_MAX_TIMEOUT_MS);
    let bash = json!({
        "command": command,
        "description": description,
        "run_in_background": true,
        "timeout": timeout_ms,
    });
    let context = tool_context(invocation).with_run_time(Some(
        nanocode_backend_claude_tools::tools::bash::direct_run_time(&bash),
    ));
    let raw = serde_json::value::to_raw_value(&bash).map_err(|error| error.to_string())?;
    let mut output = runtime
        .execute_tool("Bash", ToolInput::Function(raw), context)
        .await
        .map_err(|error| error.to_string())?;
    if !output.success {
        return tool_reply(output);
    }
    let started = output.take_structured_result();
    let (Some(id), Some(file)) = (
        started.get("task_id").and_then(Value::as_str),
        started.get("output_file").and_then(Value::as_str),
    ) else {
        return Err(format!(
            "Monitor did not start: the hand's Bash returned no background task and output file ({started})."
        ));
    };
    let monitor = Monitor {
        call_id: invocation.call_id.clone(),
        description: description.to_owned(),
        events: 0,
        id: id.to_owned(),
        model: invocation.model.clone(),
        output: file.to_owned(),
        read: 0,
        started: nanocode_platform::system_time(),
        timeout_ms,
    };
    watches
        .monitors
        .update(|monitors| monitors.push(monitor.clone()));
    Ok(ClaudeToolReply {
        content: ToolResultContent::Text(format!(
            "Monitor started (task {id}, expires in {} unless the source ends first; you get one notice at expiry \u{2014} re-arm if you still need the watch). You will be notified on each event. Keep working \u{2014} do not poll or sleep. An event that arrives after your reply is not the user's reply.",
            whole_time(timeout_ms)
        )),
        is_error: false,
        metadata: None,
        structured_result: Some(json!({"taskId": id, "timeoutMs": timeout_ms})),
    })
}

async fn monitor_events(
    runtime: &ToolRuntime,
    session_id: &str,
    monitor: &Monitor,
    poll: u64,
) -> Option<(String, u64)> {
    let input = json!({
        "command": format!(
            "tail -c +{} '{}' | head -c {}",
            monitor.read + 1,
            monitor.output.replace('\'', "'\\''"),
            nanocode_backend_claude_tools::tools::bash::MAX_STREAM_BYTES
        ),
        "description": format!("Read new output of monitor {}", monitor.id),
    });
    let call_id = format!("{}/poll-{poll}", monitor.call_id);
    let context = ToolContext::new(&monitor.model, session_id, &call_id, &[], usize::MAX)
        .with_run_time(Some(
            nanocode_backend_claude_tools::tools::bash::direct_run_time(&input),
        ));
    let raw = serde_json::value::to_raw_value(&input).ok()?;
    let mut output = match runtime
        .execute_tool("Bash", ToolInput::Function(raw), context)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            nanocode_trace::warn!(target: "nanocode_claude", %error, task_id = %monitor.id, "monitor output could not be read from the hand");
            return None;
        }
    };
    let result = output.take_structured_result();
    let stdout = result.get("stdout").and_then(Value::as_str)?;
    let end = stdout.rfind('\n').map_or(0, |at| at + 1);
    let events = stdout[..end]
        .lines()
        .filter(|line| {
            !line.trim().is_empty()
                && !nanocode_backend_claude_tools::tools::bash::closes_output(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((events, end as u64))
}

fn monitor_notification(monitor: &Monitor, event: &str) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<task-notification>\n<task-id>{}</task-id>\n<summary>Monitor event: \"{}\"</summary>\n<event>{}</event>\n</task-notification>",
        monitor.id,
        escape(&monitor.description),
        escape(event)
    )
}

fn monitor_expired(monitor: &Monitor) -> bool {
    nanocode_platform::system_time()
        .duration_since(monitor.started)
        .is_ok_and(|elapsed| elapsed.as_millis() >= u128::from(monitor.timeout_ms))
}

fn monitor_expiry(monitor: &Monitor) -> String {
    let after = whole_time(monitor.timeout_ms);
    match monitor.events {
        0 => format!(
            "[Monitor expired after {after} with no events delivered. Re-arm it if you still need the watch \u{2014} and widen the filter if silence was unexpected.]"
        ),
        1 => format!(
            "[Monitor expired after {after} with 1 event delivered. Re-arm it if you still need the watch.]"
        ),
        events => format!(
            "[Monitor expired after {after} with {events} events delivered. Re-arm it if you still need the watch.]"
        ),
    }
}

fn whole_time(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    [
        (seconds / 3600, "h"),
        (seconds / 60 % 60, "m"),
        (seconds % 60, "s"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, unit)| format!("{count}{unit}"))
    .collect::<Vec<_>>()
    .join(" ")
}

fn schedule_wakeup(
    watches: &Watches,
    input: &Value,
) -> std::result::Result<ClaudeToolReply, String> {
    let outcome = "Then write the loop's outcome for the user as ordinary visible response text \u{2014} and end the turn.";
    if input.get("stop").and_then(Value::as_bool) == Some(true) {
        let cancelled = watches.wakeup.update(Option::take).is_some();
        let text = match cancelled {
            true => format!(
                "Loop stopped \u{2014} cancelled 1 pending wakeup(s); no further dynamic-loop wakeups scheduled. If you armed a Monitor for this loop, TaskStop it now. {outcome}"
            ),
            false => format!(
                "Loop stopped \u{2014} any dynamic loop in this session is ended; there was no pending wakeup to cancel. If you armed a Monitor for this loop, TaskStop it now. {outcome}"
            ),
        };
        return Ok(ClaudeToolReply {
            content: ToolResultContent::Text(text),
            is_error: false,
            metadata: None,
            structured_result: Some(json!({
                "cancelledWakeups": u8::from(cancelled),
                "clampedDelaySeconds": 0,
                "scheduledFor": 0,
                "stopped": true,
                "wasClamped": false,
            })),
        });
    }
    let (Some(delay), Some(_)) = (
        input.get("delaySeconds").and_then(Value::as_f64),
        input.get("reason").and_then(Value::as_str),
    ) else {
        return Err("`delaySeconds` and `reason` are required when `stop` is not true.".into());
    };
    let prompt = input
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("`prompt` is required when `stop` is not true.")?;
    let clamped = delay.clamp(*WAKEUP_DELAY_SECONDS.start(), *WAKEUP_DELAY_SECONDS.end());
    let at = nanocode_platform::system_time() + std::time::Duration::from_secs_f64(clamped);
    watches.wakeup.set(Some(Wakeup {
        at,
        prompt: prompt.to_owned(),
    }));
    let note = match clamped == delay {
        true => String::new(),
        false => format!(" (clamped to {clamped}s from your requested value)"),
    };
    let unix = at
        .duration_since(nanocode_platform::UNIX_EPOCH)
        .unwrap_or_default();
    let local = unix.as_secs() as i64 + i64::from(nanocode_platform::utc_offset_seconds(at));
    let day = local.rem_euclid(86_400);
    Ok(ClaudeToolReply {
        content: ToolResultContent::Text(format!(
            "Next wakeup scheduled for {:02}:{:02}:{:02} (in {}s){note}. If you owe the user a status update this tick, write it now as ordinary response text; then end the turn \u{2014} the harness re-invokes you when the wakeup fires or a task-notification arrives.",
            day / 3600,
            day / 60 % 60,
            day % 60,
            clamped.round()
        )),
        is_error: false,
        metadata: None,
        structured_result: Some(json!({
            "clampedDelaySeconds": clamped,
            "scheduledFor": unix.as_millis() as u64,
            "wasClamped": clamped != delay,
        })),
    })
}

fn catalog(
    mut native: ClaudeTools,
    runtime: &Arc<ToolRuntime>,
    session_id: &str,
    cell_yield: Option<std::time::Duration>,
    watches: Option<Option<&Arc<Watches>>>,
) -> Result<ClaudeTools> {
    let direct = watches.is_some();
    for definition in flatten(runtime.model_specs(session_id)) {
        let strict = match &definition {
            RuntimeDefinition::Function {
                strict: true,
                parameters,
                ..
            } if !recursive_schema(parameters.as_value()) => Some(true),
            _ => None,
        };
        let (name, description, input_schema, call) = match definition {
            RuntimeDefinition::Custom {
                name, description, ..
            } if &*name == "exec" => (
                name,
                exec_description(&description),
                exec_schema(cell_yield).into(),
                Call::Exec,
            ),
            RuntimeDefinition::Function {
                name,
                description,
                parameters,
                ..
            } if &*name == "wait" => (name, description, parameters, Call::Wait),
            RuntimeDefinition::Function {
                name,
                description,
                parameters,
                ..
            } => {
                let call = Call::Function(Arc::from(name.as_ref()));
                let description = match direct && &*name == "Bash" {
                    true => nanocode_backend_claude_tools::upstream::tool("Bash")
                        .description
                        .clone(),
                    false => description,
                };
                (name, description, parameters, call)
            }
            RuntimeDefinition::ToolSearch {
                description,
                parameters,
                ..
            } => (
                "tool_search".into(),
                description,
                parameters,
                Call::Function(Arc::from("tool_search")),
            ),
            other => {
                return Err(NanocodeError::InvalidRequest(format!(
                    "Claude requires a function tool; `{}` cannot be installed directly",
                    other.name()
                )));
            }
        };
        let tool_name = name.to_string();
        native.shared_tool_names.insert(tool_name.clone());
        let definition = ToolDefinition {
            description,
            name: tool_name,
            input_schema,
            strict,
            defer_loading: false,
            cache_control: None,
        };
        let runtime = Arc::clone(runtime);
        native = native.tool_with_context(definition, move |input, invocation| {
            let runtime = Arc::clone(&runtime);
            let call = call.clone();
            async move { call.run(&runtime, input, &invocation).await }
        });
    }
    if let Some(Some(watches)) = watches
        && native.shared_tool_names.contains("Bash")
    {
        native = watch_tools(native, runtime, watches);
    }
    Ok(native)
}

#[derive(Clone)]
enum Call {
    Exec,
    Wait,
    Function(Arc<str>),
}

struct RuntimeGuard {
    control: nanocode_tool_host::runtime::ToolRuntimeControl,
    #[cfg(not(target_family = "wasm"))]
    cancel_on_drop: bool,
}

#[cfg(not(target_family = "wasm"))]
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        if !self.cancel_on_drop {
            return;
        }
        let control = self.control.clone();
        if let Ok(runtime) = nanocode_platform::Handle::try_current() {
            runtime.spawn(async move { control.cancel().await });
        }
    }
}

impl Call {
    async fn run(
        self,
        runtime: &ToolRuntime,
        input: Value,
        invocation: &ClaudeToolInvocation,
    ) -> std::result::Result<ClaudeToolReply, String> {
        let context = tool_context(invocation);
        let mut progress = Progress(invocation.progress.as_ref());
        let execution = match self {
            Self::Exec => {
                let code = input
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("exec requires a `code` string")?;
                let code = nanocode_tool_host::code_mode::source_with_options(
                    code,
                    input.get("yield_time_ms"),
                    input.get("max_output_tokens"),
                )?;
                runtime
                    .execute_code_owned_with_updates(
                        &code,
                        OwnedToolContext::from_context(context),
                        &mut progress,
                    )
                    .await
            }
            Self::Wait => {
                runtime
                    .wait_for_code_with_updates(&input.to_string(), context, &mut progress)
                    .await
            }
            Self::Function(name) => {
                let context = match &*name {
                    "Bash" => context.with_run_time(Some(
                        nanocode_backend_claude_tools::tools::bash::direct_run_time(&input),
                    )),
                    _ => context,
                };
                let raw =
                    serde_json::value::to_raw_value(&input).map_err(|error| error.to_string())?;
                let output = runtime
                    .execute_tool(&name, ToolInput::Function(raw), context)
                    .await
                    .map_err(|error| error.to_string())?;
                return tool_reply(output);
            }
        }
        .map_err(|error| error.to_string())?;
        Ok(code_reply(execution, &invocation.call_id))
    }

    async fn recover(
        runtime: &ToolRuntime,
        name: &str,
        invocation: &ClaudeToolInvocation,
    ) -> std::result::Result<ClaudeToolReply, String> {
        let context = tool_context(invocation);
        if matches!(name, "exec" | "wait") {
            return match runtime.recover_code(context).await {
                Ok(Some(execution)) => Ok(code_reply(execution, &invocation.call_id)),
                Ok(None) => Err("not recovered".into()),
                Err(error) if error.is_unsent() => Err(crate::agent::NOT_DISPATCHED.into()),
                Err(error) => Err(error.to_string()),
            };
        }
        match runtime.recover_tool(name, context).await {
            Some(output) => tool_reply(output),
            None => Err("not recovered".into()),
        }
    }
}

fn tool_context(invocation: &ClaudeToolInvocation) -> ToolContext<'_> {
    ToolContext::new(
        &invocation.model,
        &invocation.session_id,
        &invocation.call_id,
        &[],
        usize::MAX,
    )
    .with_turn_id(Some(&invocation.turn_id))
    .with_host_context(invocation.host_context.as_deref())
    .with_instruction_revision(invocation.instruction_revision)
}

fn tool_reply(mut output: ToolOutput) -> std::result::Result<ClaudeToolReply, String> {
    let structured_result = Some(output.take_structured_result());
    let metadata = output
        .metadata
        .take()
        .map(|value| serde_json::from_str(value.get()))
        .transpose()
        .map_err(|error| error.to_string())?;
    Ok(reply(
        output.output,
        output.success,
        metadata,
        structured_result,
    ))
}

struct Progress<'a>(Option<&'a ClaudeToolProgress>);

impl CodeModeObserver for Progress<'_> {
    fn cell_terminated(&mut self, origin_call_id: &str, output: &str) {
        if let Some(progress) = self.0 {
            progress.update(ClaudeNestedToolUpdate::Terminated {
                call_id: origin_call_id.to_owned(),
                output: output.to_owned(),
            });
        }
    }

    fn update(&mut self, update: CodeModeUpdate<'_>) {
        let Some(progress) = self.0 else {
            return;
        };
        progress.update(match update {
            CodeModeUpdate::NestedCallStarted {
                call_id,
                name,
                input,
            } => ClaudeNestedToolUpdate::Started {
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                input: input.clone(),
            },
            CodeModeUpdate::NestedCallCompleted(call) => ClaudeNestedToolUpdate::Completed(
                nested_call(call, call.input.clone(), call.structured_result.clone()),
            ),
            CodeModeUpdate::Notification(notification) => ClaudeNestedToolUpdate::Notification {
                call_id: notification.call_id.clone(),
                text: notification.text.clone(),
            },
        });
    }
}

fn nested_call(call: &NestedToolCall, input: Value, structured_result: Value) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert("call_id".into(), call.call_id.as_str().into());
    fields.insert("name".into(), call.name.as_str().into());
    fields.insert("input".into(), input);
    fields.insert("output".into(), json!(call.output));
    fields.insert("structured_result".into(), structured_result);
    fields.insert("metadata".into(), json!(call.metadata));
    fields.insert("success".into(), call.success.into());
    fields.insert("started_after_ns".into(), call.started_after_ns.into());
    fields.insert("duration_ns".into(), call.duration_ns.into());
    Value::Object(fields)
}

fn code_reply(mut execution: CodeModeExecution, call_id: &str) -> ClaudeToolReply {
    let documents = execution
        .nested_calls
        .iter()
        .filter(|call| call.name == "Read")
        .filter_map(|call| match &call.output {
            ToolOutputBody::Content(items) => Some(items),
            ToolOutputBody::Text(_) => None,
        })
        .flatten()
        .filter(|item| matches!(item, ToolOutputContent::InputFile { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let calls = execution
        .nested_calls
        .iter_mut()
        .map(|call| {
            let input = std::mem::take(&mut call.input);
            let structured_result = std::mem::take(&mut call.structured_result);
            nested_call(call, input, structured_result)
        })
        .collect();
    let mut code = serde_json::Map::new();
    code.insert("calls".into(), Value::Array(calls));
    code.insert(
        "origin_call_id".into(),
        execution
            .cell
            .as_ref()
            .map_or(call_id, |cell| cell.origin_call_id.as_str())
            .into(),
    );
    code.insert(
        "running".into(),
        execution
            .cell
            .as_ref()
            .is_some_and(|cell| cell.running)
            .into(),
    );
    let mut metadata = serde_json::Map::new();
    metadata.insert("_nanocode_code".into(), Value::Object(code));
    let mut reply = reply(
        execution.output,
        execution.success,
        Some(Value::Object(metadata)),
        None,
    );
    if !execution.notifications.is_empty()
        && let ToolResultContent::Text(text) = &mut reply.content
    {
        reply.content =
            ToolResultContent::Blocks(vec![json!({"type": "text", "text": std::mem::take(text)})]);
    }
    if let ToolResultContent::Blocks(blocks) = &mut reply.content {
        blocks.extend(
            execution
                .notifications
                .into_iter()
                .map(|notification| json!({"type": "text", "text": notification.text})),
        );
    }
    if !documents.is_empty() {
        if let ToolResultContent::Text(text) = &mut reply.content {
            let text = match std::mem::take(text) {
                text if text.is_empty() => "(no text output)".to_owned(),
                text => text,
            };
            reply.content = ToolResultContent::Blocks(vec![json!({"type": "text", "text": text})]);
        }
        if let ToolResultContent::Blocks(blocks) = &mut reply.content {
            let omitted = documents.len().saturating_sub(SURFACED_DOCUMENTS);
            blocks.extend(documents.into_iter().take(SURFACED_DOCUMENTS).map(block));
            if omitted > 0 {
                blocks.push(json!({"type": "text", "text": format!("[{omitted} more PDF document(s) from inner Read calls not shown \u{2014} an exec result surfaces at most {SURFACED_DOCUMENTS} documents; read fewer PDFs per call]")}));
            }
        }
    }
    reply
}

fn flatten(definitions: Vec<RuntimeDefinition>) -> Vec<RuntimeDefinition> {
    let mut flat = Vec::new();
    for definition in definitions {
        match definition {
            RuntimeDefinition::Namespace {
                name: namespace,
                tools,
                ..
            } => {
                for mut tool in tools {
                    match &mut tool {
                        RuntimeDefinition::Function { name, .. }
                        | RuntimeDefinition::Custom { name, .. } => {
                            *name = format!("{namespace}__{name}").into();
                        }
                        _ => continue,
                    }
                    flat.push(tool);
                }
            }
            definition => flat.push(definition),
        }
    }
    flat
}

fn reply(
    output: ToolOutputBody,
    success: bool,
    metadata: Option<Value>,
    structured_result: Option<Value>,
) -> ClaudeToolReply {
    let content = match output {
        ToolOutputBody::Text(text) => ToolResultContent::Text(text),
        ToolOutputBody::Content(items) => {
            ToolResultContent::Blocks(items.into_iter().map(block).collect())
        }
    };
    ClaudeToolReply {
        content,
        is_error: !success,
        metadata,
        structured_result,
    }
}

fn block(item: ToolOutputContent) -> Value {
    match item {
        ToolOutputContent::InputText { text } => json!({"type": "text", "text": text}),
        ToolOutputContent::InputImage { image_url, .. } => {
            match image_url
                .get(..5)
                .filter(|scheme| scheme.eq_ignore_ascii_case("data:"))
                .map(|_| image_url[5..].split_once(";base64,"))
            {
                Some(Some((media_type, data))) => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data},
                }),
                Some(None) => json!({"type": "text", "text": IMAGE_UNPROCESSABLE}),
                None => json!({"type": "image", "source": {"type": "url", "url": image_url}}),
            }
        }
        ToolOutputContent::InputImageFile { .. } => {
            json!({"type": "text", "text": IMAGE_FILE_UNSUPPORTED})
        }
        ToolOutputContent::InputAudio { .. } => json!({"type": "text", "text": AUDIO_UNSUPPORTED}),
        ToolOutputContent::InputFile { file_data } => {
            match file_data
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
            {
                Some((media_type, data)) => json!({
                    "type": "document",
                    "source": {"type": "base64", "media_type": media_type, "data": data},
                }),
                None => json!({"type": "document", "source": {"type": "url", "url": file_data}}),
            }
        }
        other => json!({
            "type": "text",
            "text": serde_json::to_string(&other).unwrap_or_default(),
        }),
    }
}

fn recursive_schema(schema: &Value) -> bool {
    fn visit(value: &Value, root: &Value, references: &mut Vec<String>) -> bool {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                    && let Some(pointer) = reference.strip_prefix('#')
                    && let Some(target) = root.pointer(pointer)
                {
                    if references.iter().any(|active| active == reference) {
                        return true;
                    }
                    references.push(reference.to_owned());
                    let recursive = visit(target, root, references);
                    references.pop();
                    if recursive {
                        return true;
                    }
                }
                object.iter().any(|(key, value)| {
                    !matches!(
                        key.as_str(),
                        "$defs" | "definitions" | "const" | "enum" | "default" | "examples"
                    ) && visit(value, root, references)
                })
            }
            Value::Array(values) => values.iter().any(|value| visit(value, root, references)),
            _ => false,
        }
    }
    visit(schema, schema, &mut Vec::new())
}

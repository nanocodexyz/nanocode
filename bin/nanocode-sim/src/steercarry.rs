use std::{
    collections::VecDeque,
    future::Future,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::{
    ClaudeClient, Nanocode, PromptRequest, Thinking, Tools,
    backend::Prompt,
    claude::transport::{HttpError, HttpFuture, HttpRequest, HttpResponse, HttpTransport},
    durability::SessionStore,
};
use nanocode_durability_core::KeyValueStore;
use nanocode_sync::Atom;
use serde_json::{Value, json};

use crate::{
    disk::Disk,
    executor::{Clock, Ended, Executor, Task},
    faults::{Durability, Profile},
    rng::{FNV_OFFSET, Rng, fnv},
    store::{Mutation as StoreMutation, SimKv},
    turnlog::SimHost,
};

const SESSION: &str = "0190f5a2-7c1e-7000-8000-00000057ee12";
const TOOL: &str = "hold";
const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const BASE_URL: &str = "http://model.sim/v1";
const STEER_ID: &str = "steer-carried";
const STEER_TEXT: &str = "STEERMARK use the second ledger";
const STEER_MARK: &str = "STEERMARK";
const FIRST_REQUEST: &str = "turn-first";
const NEXT_REQUEST: &str = "turn-next";
const AGAIN_REQUEST: &str = "turn-again";
const OTHER_REQUEST: &str = "turn-other";
const CRASH_AFTER: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_millis(5);
const EPOCHS_MAX: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarryMutation {
    None,
    CarryAsText,
}

impl CarryMutation {
    const ALL: [Self; 2] = [Self::None, Self::CarryAsText];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CarryAsText => "carry_steer_as_text",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CarryStats {
    pub seeds: u64,
    pub cases: u64,
    pub epochs: u64,
    pub crashes: u64,
    pub model_calls: u64,
}

impl CarryStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.cases += other.cases;
        self.epochs += other.epochs;
        self.crashes += other.crashes;
        self.model_calls += other.model_calls;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum First {
    Read,
    Unread,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    Never,
    BetweenTurns,
    WhileCarrying,
}

const BACKENDS: [Backend; 2] = [Backend::Claude, Backend::Codex];
const FIRSTS: [First; 3] = [First::Read, First::Unread, First::Rejected];
const CRASHES: [Crash; 3] = [Crash::Never, Crash::BetweenTurns, Crash::WhileCarrying];

#[derive(Default)]
struct Wire {
    bodies: Atom<Vec<(String, bool)>>,
    script: Atom<VecDeque<bool>>,
    hang: AtomicBool,
    failing: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
    calls: AtomicU64,
}

impl Wire {
    async fn reply(&self, body: &[u8]) -> Option<bool> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let failing = self.failing.load(Ordering::Acquire);
        self.bodies.update(|bodies| {
            bodies.push((String::from_utf8_lossy(body).into_owned(), !failing));
        });
        if self.hang.swap(false, Ordering::AcqRel) {
            std::future::pending::<()>().await;
        }
        (!failing).then(|| self.script.update(VecDeque::pop_front).unwrap_or(false))
    }

    fn plan(&self, replies: &[bool]) {
        self.failing.store(false, Ordering::Release);
        self.script.set(replies.iter().copied().collect());
        self.entered.store(false, Ordering::Release);
        self.released.store(false, Ordering::Release);
    }
}

struct Hold(Arc<Wire>);

impl nanocode::FunctionHandler for Hold {
    fn call<'a>(
        &'a self,
        _name: &'a str,
        _input: Value,
        _context: nanocode::tools::ToolContext<'a>,
    ) -> nanocode::HostFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            self.0.entered.store(true, Ordering::Release);
            while !self.0.released.load(Ordering::Acquire) {
                nanocode_platform::sleep(POLL).await;
            }
            Ok(Value::String("held".to_owned()))
        })
    }
}

fn tools(wire: &Arc<Wire>) -> Tools {
    Tools::builder()
        .without_defaults()
        .tool(nanocode::FunctionTool::new(
            TOOL,
            "Wait until the operator releases the hold",
            json!({"type": "object", "properties": {}}),
            Arc::new(Hold(Arc::clone(wire))),
        ))
        .build()
        .expect("the steer carry tools build")
}

struct ClaudeWire(Arc<Wire>);

impl HttpTransport for ClaudeWire {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let Some(tool) = self.0.reply(request.body.as_bytes()).await else {
                return Ok(HttpResponse::new(
                    nanocode_http::types::StatusCode::BAD_REQUEST,
                    json_headers(),
                    Box::pin(nanocode_sync::stream::once(async move {
                        Ok(json!({"type": "error", "error": {"type": "invalid_request_error", "message": "simulated rejection"}}).to_string().into_bytes())
                    })),
                ));
            };
            drop(request);
            let number = self.0.calls.load(Ordering::Acquire);
            let (block, delta, stop) = if tool {
                (
                    json!({"type": "tool_use", "id": format!("toolu_carry{number}"), "name": TOOL, "input": {}}),
                    json!({"type": "input_json_delta", "partial_json": "{}"}),
                    "tool_use",
                )
            } else {
                (
                    json!({"type": "text", "text": ""}),
                    json!({"type": "text_delta", "text": "done"}),
                    "end_turn",
                )
            };
            let sse = crate::server::events(
                CLAUDE_MODEL,
                &format!("carry{number}"),
                &block,
                Some(delta),
                stop,
            );
            Ok(HttpResponse::new(
                nanocode_http::types::StatusCode::OK,
                event_stream(),
                Box::pin(nanocode_sync::stream::once(
                    async move { Ok(sse.into_bytes()) },
                )),
            ))
        })
    }
}

struct CodexWire(Arc<Wire>);

impl nanocode::http::Layer for CodexWire {
    fn send<'a>(
        &'a self,
        request: nanocode::http::Request,
        _next: nanocode::http::Next<'a>,
    ) -> nanocode::http::BoxFuture<'a, Result<nanocode::http::Response, nanocode::http::Error>>
    {
        Box::pin(async move {
            let Some(tool) = self.0.reply(&request.body).await else {
                return Ok(nanocode::http::Response::new(
                    nanocode_http::types::StatusCode::BAD_REQUEST,
                    json_headers(),
                    Box::pin(nanocode_sync::stream::once(async move {
                        Ok(json!({"error": {"type": "invalid_request_error", "message": "simulated rejection"}}).to_string().into_bytes())
                    })),
                ));
            };
            drop(request);
            let number = self.0.calls.load(Ordering::Acquire);
            let id = format!("resp_carry{number}");
            let item = if tool {
                json!({
                    "type": "function_call",
                    "id": format!("fc_carry{number}"),
                    "call_id": format!("call_carry{number}"),
                    "name": TOOL,
                    "arguments": "{}",
                })
            } else {
                json!({
                    "type": "message",
                    "id": format!("msg_carry{number}"),
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}],
                })
            };
            let events = [
                json!({"type": "response.created", "response": {"id": id}}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                json!({"type": "response.completed", "response": {"id": id, "status": "completed", "output": [], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
            ];
            let sse: String = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    )
                })
                .collect();
            Ok(nanocode::http::Response::new(
                nanocode_http::types::StatusCode::OK,
                event_stream(),
                Box::pin(nanocode_sync::stream::once(
                    async move { Ok(sse.into_bytes()) },
                )),
            ))
        })
    }
}

fn json_headers() -> nanocode_http::types::HeaderMap {
    let mut headers = nanocode_http::types::HeaderMap::new();
    headers.insert(
        nanocode_http::types::header::CONTENT_TYPE,
        nanocode_http::types::HeaderValue::from_static("application/json"),
    );
    headers
}

fn event_stream() -> nanocode_http::types::HeaderMap {
    let mut headers = nanocode_http::types::HeaderMap::new();
    headers.insert(
        nanocode_http::types::header::CONTENT_TYPE,
        nanocode_http::types::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        "request-id",
        nanocode_http::types::HeaderValue::from_static("req_sim"),
    );
    headers
}

fn runtime(backend: Backend, wire: &Arc<Wire>, store: SessionStore) -> Result<Nanocode, String> {
    let builder = Nanocode::builder()
        .tools(tools(wire))
        .runner_management(false)
        .thinking(Thinking::Low)
        .workspace(std::env::temp_dir())
        .durability(store);
    match backend {
        Backend::Claude => {
            let client = ClaudeClient::messages_api(
                BASE_URL,
                "sk-sim",
                Duration::from_secs(10),
                Duration::from_secs(600),
            )
            .map_err(|error| format!("claude client: {error}"))?
            .with_transport(ClaudeWire(Arc::clone(wire)));
            builder.claude(nanocode::ClaudeHarness::new(client).direct_tools_for(|_| true))
        }
        Backend::Codex => {
            let openai = nanocode::OpenAi::builder(nanocode::codex::OpenAiAuth::api_key("sk-sim"))
                .api_base_url(BASE_URL)
                .transport(nanocode::codex::transport::ResponsesTransport::Https)
                .http_layer(CodexWire(Arc::clone(wire)))
                .model(nanocode::codex::Model::Luna)
                .build()
                .map_err(|error| format!("openai client: {error}"))?;
            builder.openai(nanocode::CodexHarness::new(openai).direct_tools_for(|_| true))
        }
    }
    .build()
    .map_err(|error| format!("runtime build: {error}"))
}

async fn open(
    runtime: &Nanocode,
    backend: Backend,
    fresh: bool,
) -> Result<nanocode::Agent, String> {
    let opened = if fresh {
        let spawn = runtime.spawn().session_id(SESSION);
        match backend {
            Backend::Claude => {
                spawn
                    .backend::<nanocode::Claude>()
                    .model(nanocode::ClaudeModel::Haiku55)
                    .build()
                    .await
            }
            Backend::Codex => {
                spawn
                    .backend::<nanocode::Codex>()
                    .model(nanocode::codex::Model::Luna)
                    .build()
                    .await
            }
        }
    } else {
        runtime.open_session(SESSION).await
    };
    let (agent, events) = opened.map_err(|error| format!("open: {error}"))?;
    drop(events);
    Ok(agent)
}

async fn first_turn(agent: &nanocode::Agent, wire: &Wire, first: First) -> Result<(), String> {
    wire.plan(match first {
        First::Read => &[true, false],
        First::Unread | First::Rejected => &[true],
    });
    let turn = agent
        .prompt(PromptRequest::new("FIRSTTURN look up the balance").request_id(FIRST_REQUEST))
        .await
        .map_err(|error| format!("first turn: {error}"))?;
    while !wire.entered.load(Ordering::Acquire) {
        nanocode_platform::sleep(POLL).await;
    }
    turn.steer_with_id(STEER_ID.to_owned(), Prompt::new(STEER_TEXT))
        .await
        .map_err(|error| format!("steer: {error}"))?;
    if first == First::Unread {
        turn.cancel()
            .await
            .map_err(|error| format!("cancel: {error}"))?;
    }
    if first == First::Rejected {
        wire.failing.store(true, Ordering::Release);
    }
    wire.released.store(true, Ordering::Release);
    let result = turn.result().await;
    match (first, result) {
        (First::Read, Err(error)) => Err(format!("first turn result: {error}")),
        _ => Ok(()),
    }
}

async fn carry_turn(
    agent: &nanocode::Agent,
    wire: &Wire,
    mutation: CarryMutation,
    request_id: &str,
    hang: bool,
) -> Result<(), String> {
    wire.plan(&[false]);
    wire.hang.store(hang, Ordering::Release);
    let request = match mutation {
        CarryMutation::None => {
            PromptRequest::new("NEXTTURN continue").carry_steer(STEER_ID, Prompt::new(STEER_TEXT))
        }
        CarryMutation::CarryAsText => {
            PromptRequest::new(format!("NEXTTURN continue\n{STEER_TEXT}"))
        }
    }
    .request_id(request_id);
    let turn = agent
        .prompt(request)
        .await
        .map_err(|error| format!("{request_id}: {error}"))?;
    turn.result()
        .await
        .map(drop)
        .map_err(|error| format!("{request_id} result: {error}"))
}

async fn plain_turn(agent: &nanocode::Agent, wire: &Wire) -> Result<(), String> {
    wire.plan(&[false]);
    let turn = agent
        .prompt(PromptRequest::new("OTHERTURN unrelated").request_id(OTHER_REQUEST))
        .await
        .map_err(|error| format!("other turn: {error}"))?;
    turn.result()
        .await
        .map(drop)
        .map_err(|error| format!("other turn result: {error}"))
}

async fn read(store: &SessionStore) -> Result<Option<bool>, String> {
    store
        .steer_read(SESSION, STEER_ID)
        .await
        .map_err(|error| format!("steer_read: {error}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Fresh,
    BetweenTurns,
    Carrying,
    Again,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    Kv,
    Log,
    PrunedKv,
}

struct Case {
    backend: Backend,
    first: First,
    crash: Crash,
    store: Store,
}

struct Media {
    kv: SimKv,
    disk: Arc<Disk>,
}

impl Media {
    async fn open(
        &self,
        store: Store,
    ) -> Result<(SessionStore, Option<nanocode::turnlog::Parts>), String> {
        match store {
            Store::Kv | Store::PrunedKv => {
                let opened = nanocode::session_store(KeyValueStore::new(self.kv.clone()))
                    .map_err(|error| format!("session store: {error}"))?;
                Ok((
                    if store == Store::PrunedKv {
                        opened.with_terminal_receipt_limit(1)
                    } else {
                        opened
                    },
                    None,
                ))
            }
            Store::Log => {
                let host: Arc<dyn nanocode::turnlog::LogHost> =
                    Arc::new(SimHost::new(Arc::clone(&self.disk)));
                let policy = nanocode::turnlog::Policy {
                    effects: nanocode::turnlog::Effects::default(),
                    max_row_bytes: None,
                    restarts: u64::MAX,
                    retain_finished: None,
                    snapshot_ratio: 2,
                    snapshot_rows: None,
                    wakes_max: None,
                };
                let parts = nanocode::turnlog(host, "sim", policy)
                    .await
                    .map_err(|error| format!("turn log: {error}"))?;
                parts.log.settle_durably();
                Ok((parts.store.clone(), Some(parts)))
            }
        }
    }
}

#[derive(Default)]
struct Run {
    wrong: Vec<String>,
    epochs: u64,
    crashes: u64,
}

struct Seen {
    stage: Atom<Stage>,
    first_read: Atom<Option<Option<bool>>>,
    refused: Atom<Option<String>>,
}

async fn epoch_work(
    case: &Case,
    mutation: CarryMutation,
    wire: &Arc<Wire>,
    media: &Media,
    seen: &Seen,
) -> Result<(), String> {
    let (store, _parts) = media.open(case.store).await?;
    let runtime = runtime(case.backend, wire, store.clone())?;
    let at = *seen.stage.get();
    let agent = open(&runtime, case.backend, at == Stage::Fresh).await?;
    if at == Stage::Fresh {
        first_turn(&agent, wire, case.first).await?;
        seen.first_read.set(Some(read(&store).await?));
        seen.stage.set(Stage::BetweenTurns);
        if case.crash == Crash::BetweenTurns {
            return Ok(());
        }
    }
    if case.store == Store::PrunedKv {
        plain_turn(&agent, wire).await?;
        if let Err(error) = carry_turn(&agent, wire, mutation, NEXT_REQUEST, false).await {
            seen.refused.set(Some(error));
        }
        seen.stage.set(Stage::Done);
        return Ok(());
    }
    if *seen.stage.get() != Stage::Again {
        let hang = case.crash == Crash::WhileCarrying && *seen.stage.get() == Stage::BetweenTurns;
        seen.stage.set(Stage::Carrying);
        carry_turn(&agent, wire, mutation, NEXT_REQUEST, hang).await?;
        seen.stage.set(Stage::Again);
    }
    carry_turn(&agent, wire, mutation, AGAIN_REQUEST, false).await?;
    seen.stage.set(Stage::Done);
    Ok(())
}

fn epoch(
    seed: u64,
    clock: &Arc<Clock>,
    label: &str,
    work: impl Future<Output = Result<(), String>>,
) -> (Ended, Option<Result<(), String>>) {
    let result = std::cell::RefCell::new(None);
    let mut executor = Executor::new(Arc::clone(clock), Rng::derive(seed, label));
    let task: Task<'_> = Box::pin(async {
        result.replace(Some(work.await));
    });
    let crash_at = clock.now_ns() + u64::try_from(CRASH_AFTER.as_nanos()).unwrap_or(u64::MAX);
    let ended = executor.run(vec![task], Some(crash_at));
    drop(executor);
    loop {
        let spawned = crate::host::spawned();
        if spawned.is_empty() {
            break;
        }
        drop(spawned);
    }
    (ended, result.into_inner())
}

fn simulate_case(seed: u64, case: &Case, mutation: CarryMutation) -> (Run, u64) {
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let mut profile = Profile::of(seed);
    profile.before_ppm = 0;
    profile.after_ppm = 0;
    profile.torn_ppm = 0;
    profile.read_ppm = 0;
    profile.durability = Durability::OnAck;
    let media = Media {
        kv: SimKv::new(
            seed,
            profile,
            StoreMutation::None,
            false,
            Arc::clone(&clock),
        ),
        disk: Disk::new(),
    };
    let wire = Arc::new(Wire::default());
    let seen = Seen {
        stage: Atom::new(Stage::Fresh),
        first_read: Atom::new(None),
        refused: Atom::new(None),
    };
    let mut run = Run::default();
    let label = format!(
        "steer_carry {:?} {:?} {:?} {:?}",
        case.backend, case.first, case.crash, case.store
    );
    for number in 0..EPOCHS_MAX {
        run.epochs += 1;
        let (ended, result) = epoch(
            seed,
            &clock,
            &format!("{label} {number}"),
            epoch_work(case, mutation, &wire, &media, &seen),
        );
        match (ended, result) {
            (Ended::Finished, Some(Ok(()))) if *seen.stage.get() == Stage::Done => break,
            (Ended::Finished, Some(Ok(()))) | (Ended::Crashed | Ended::Stuck, _) => {
                run.crashes += 1;
            }
            (Ended::Finished, Some(Err(error))) => {
                run.wrong.push(format!("epoch {number}: {error}"));
                break;
            }
            (Ended::Finished, None) => {
                run.wrong
                    .push(format!("epoch {number} ended without a result"));
                break;
            }
        }
    }
    let calls = wire.calls.load(Ordering::Acquire);
    if !run.wrong.is_empty() {
        return (run, calls);
    }
    let stage = *seen.stage.get();
    if stage != Stage::Done {
        run.wrong.push(format!(
            "the carrying turns stopped at {stage:?} after {EPOCHS_MAX} epochs"
        ));
        return (run, calls);
    }
    let expected = Some(Some(case.first != First::Unread));
    let before = *seen.first_read.get();
    if before != expected {
        run.wrong.push(format!(
            "after the first turn steer_read was {before:?}, expected {expected:?}"
        ));
    }
    let most = wire.bodies.read(|bodies| {
        bodies
            .iter()
            .map(|(body, _)| body.matches(STEER_MARK).count())
            .max()
            .unwrap_or(0)
    });
    if case.store == Store::PrunedKv {
        let refused = seen.refused.get();
        if !refused
            .as_deref()
            .is_some_and(|error| error.contains("cannot be carried"))
        {
            run.wrong.push(format!(
                "carrying a steer whose read receipt was pruned ended {refused:?}, not refused"
            ));
        }
        if most > 1 {
            run.wrong.push(format!(
                "after pruning, a model request held the steer {most} times"
            ));
        }
        return (run, calls);
    }
    if most != 1 {
        run.wrong.push(format!(
            "the session's model requests held the steer at most {most} times, not once"
        ));
    }
    let answered = wire.bodies.read(|bodies| {
        bodies
            .iter()
            .any(|(body, answered)| *answered && body.contains(STEER_MARK))
    });
    if !answered && case.first != First::Rejected {
        run.wrong
            .push("no model request that got an answer held the steer".to_owned());
    }
    let after = check_read(seed, &clock, &media, case.store);
    if after != Some(true) {
        run.wrong
            .push(format!("after the carrying turns steer_read was {after:?}"));
    }
    (run, calls)
}

fn check_read(seed: u64, clock: &Arc<Clock>, media: &Media, store: Store) -> Option<bool> {
    let seen = Atom::new(None);
    let (_, result) = epoch(seed, clock, "steer_carry read", async {
        let (opened, _parts) = media.open(store).await?;
        seen.set(read(&opened).await?);
        Ok(())
    });
    result.and_then(Result::ok).and(*seen.get())
}

struct Outcome {
    digest: u64,
    stats: CarryStats,
    wrong: Option<String>,
}

fn simulate(seed: u64, mutation: CarryMutation) -> Outcome {
    let mut stats = CarryStats {
        seeds: 1,
        ..CarryStats::default()
    };
    let mut digest = FNV_OFFSET;
    let mut wrong = Vec::new();
    let mut cases = Vec::new();
    for store in [Store::Kv, Store::Log] {
        for backend in BACKENDS {
            for first in FIRSTS {
                for crash in CRASHES {
                    cases.push(Case {
                        backend,
                        first,
                        crash,
                        store,
                    });
                }
            }
        }
    }
    for backend in BACKENDS {
        cases.push(Case {
            backend,
            first: First::Read,
            crash: Crash::Never,
            store: Store::PrunedKv,
        });
    }
    for case in &cases {
        let (run, calls) = simulate_case(seed, case, mutation);
        stats.cases += 1;
        stats.epochs += run.epochs;
        stats.crashes += run.crashes;
        stats.model_calls += calls;
        let name = format!(
            "{:?} {:?} {:?} {:?}",
            case.backend, case.first, case.crash, case.store
        );
        digest = fnv(
            digest,
            format!("{name} {} {} {calls}", run.epochs, run.crashes).as_bytes(),
        );
        if !run.wrong.is_empty() {
            wrong.push(format!("{name}: {}", run.wrong.join("; ")));
        }
    }
    Outcome {
        digest,
        stats,
        wrong: (!wrong.is_empty()).then(|| wrong.join(" | ")),
    }
}

pub fn judge(
    seed: u64,
    mutation: CarryMutation,
    twice: bool,
    verbose: bool,
    totals: &mut CarryStats,
) -> Option<String> {
    let first = simulate(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {:?}", first.digest, first.stats);
    }
    if first.wrong.is_some() || !twice {
        return first.wrong;
    }
    let second = simulate(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "nondeterminism: digest {:016x} then {:016x}",
            first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: CarryMutation) -> String {
    let mut command = format!("nanocode-sim --scenario steer_carry --seed {seed}");
    if mutation != CarryMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: CarryMutation,
    totals: &CarryStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=steer_carry mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        CarryMutation::None if failed == 0 => ExitCode::SUCCESS,
        CarryMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}

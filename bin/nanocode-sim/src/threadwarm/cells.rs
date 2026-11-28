use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::{
    ClaudeClient, CodeModeExecutors, HarnessFamily, HostFuture, ModelSelection, Nanocode,
    PromptRequest, Tools,
    claude::transport::{HttpError, HttpFuture, HttpRequest, HttpResponse, HttpTransport},
    durability::{SessionRecord, SessionStore},
    session_host::{Delivery, InboxHost, LoggedEvent, Next, PassEnd, TurnInbox, TurnState},
    turnlog::OneLog,
};
use nanocode_code_mode::code_mode::LocalCells;
use nanocode_durability_core::StateStore;
use nanocode_turnlog::Fold;
use serde_json::{Map, Value, json};

use super::{Host, Reopen, Shape, opened, row_bytes_max, submitted};
use crate::{memory, rng::Rng};

const TOOL: &str = "lookup";
const EXECUTOR: &str = "local";
const CELL_CALLS: u64 = 3;
const STORED_KEYS: u64 = 20;
const STORED_BYTES: Range<u64> = 256..4_097;
const SOURCE_BYTES: Range<u64> = 128..1_025;
const TEXT_BYTES: Range<u64> = 128..1_025;

#[derive(Clone, Copy)]
struct Sizes {
    stored: u64,
    source: u64,
    text: u64,
}

struct Wire {
    sizes: Sizes,
    turn: AtomicU64,
    round: AtomicU64,
}

impl Wire {
    fn code(&self, at: u64) -> String {
        let sizes = self.sizes;
        format!(
            "const big = '{}'; for (let j = 0; j < {CELL_CALLS}; j++) await tools.{TOOL}({{ key: 'k' + j }}); \
             store('s' + ({at} % {STORED_KEYS}), 'x'.repeat({}) + {at}); text('r'.repeat({}) + big.length)",
            "L".repeat(usize::try_from(sizes.source).expect("a source length fits")),
            sizes.stored,
            sizes.text
        )
    }

    fn begin(&self, at: u64) {
        self.turn.store(at, Ordering::Release);
        self.round.store(0, Ordering::Release);
    }
}

struct Wired(Arc<Wire>);

impl HttpTransport for Wired {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            drop(request);
            let at = self.0.turn.load(Ordering::Acquire);
            let round = self.0.round.fetch_add(1, Ordering::AcqRel);
            let id = format!("cells{at}r{round}");
            let (block, delta, stop) = if round == 0 {
                (
                    json!({"type": "tool_use", "id": format!("toolu_{id}"), "name": "exec", "input": {}}),
                    json!({"type": "input_json_delta", "partial_json": json!({"code": self.0.code(at)}).to_string()}),
                    "tool_use",
                )
            } else {
                (
                    json!({"type": "text", "text": ""}),
                    json!({"type": "text_delta", "text": "ok"}),
                    "end_turn",
                )
            };
            let sse = crate::server::events(memory::CLAUDE_MODEL, &id, &block, Some(delta), stop);
            Ok(HttpResponse::new(
                nanocode_http::types::StatusCode::OK,
                memory::event_stream(),
                Box::pin(nanocode_sync::stream::once(
                    async move { Ok(sse.into_bytes()) },
                )),
            ))
        })
    }
}

struct Quiet;

impl InboxHost for Quiet {
    fn publish(&self, _events: Vec<LoggedEvent>) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn next<'a>(
        &'a self,
        _request_id: &'a str,
        _end: PassEnd,
        _delivery: &'a Delivery,
    ) -> HostFuture<'a, Next> {
        Box::pin(async { Next::Done })
    }
}

struct Lookup;

impl nanocode::FunctionHandler for Lookup {
    fn call<'a>(
        &'a self,
        _name: &'a str,
        input: Value,
        _context: nanocode::tools::ToolContext<'a>,
    ) -> nanocode::HostFuture<'a, Result<Value, String>> {
        Box::pin(async move { Ok(json!({"key": input.get("key"), "value": 1})) })
    }
}

async fn runtime(
    log: &Arc<OneLog>,
    wire: &Arc<Wire>,
    store: SessionStore,
) -> Result<Nanocode, String> {
    let journals = log
        .journals()
        .await
        .map_err(|error| format!("the cell journals do not open: {error}"))?;
    let executors = CodeModeExecutors::new();
    executors.insert(EXECUTOR, Arc::new(LocalCells::new().journals(journals)));
    executors.set_default(Some(Arc::from(EXECUTOR)));
    let tools = Tools::builder()
        .without_defaults()
        .tool(nanocode::FunctionTool::new(
            TOOL,
            "Look up a key.",
            json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
            Arc::new(Lookup),
        ))
        .code_mode_host(executors)
        .build()
        .map_err(|error| format!("the cell tools build: {error}"))?;
    let client = ClaudeClient::messages_api(
        memory::BASE_URL,
        "sk-sim",
        memory::CONNECT_TIMEOUT,
        memory::IDLE_TIMEOUT,
    )
    .map_err(|error| format!("claude client: {error}"))?
    .with_transport(Wired(Arc::clone(wire)));
    Nanocode::builder()
        .tools(tools)
        .runner_management(false)
        .workspace(std::env::temp_dir())
        .durability(store)
        .claude(nanocode::ClaudeHarness::new(client))
        .build()
        .map_err(|error| format!("runtime build: {error}"))
}

fn session_store(log: &Arc<OneLog>) -> Result<SessionStore, String> {
    nanocode::session_store(log.store()).map_err(|error| error.to_string())
}

async fn cell_turn(
    log: &OneLog,
    agent: &nanocode::Agent,
    wire: &Wire,
    rng: &mut Rng,
    at: u64,
) -> Result<(), String> {
    let request = submitted(log, rng, at).await?;
    wire.begin(at);
    let running = agent
        .prompt(
            PromptRequest::new(format!("turn {at}: run a cell")).request_id(format!("cells-{at}")),
        )
        .await
        .map_err(|error| format!("turn {at}: {error}"))?;
    running
        .result()
        .await
        .map_err(|error| format!("turn {at} result: {error}"))?;
    if wire.round.load(Ordering::Acquire) < 2 {
        return Err(format!("turn {at} ran no cell"));
    }
    let result = Map::from_iter([("text".to_owned(), Value::String(format!("answer {at}")))]);
    match log.finish(&request, TurnState::Completed, &result).await {
        Ok(Some(_)) => Ok(()),
        answer => Err(format!("turn {at} finishes as {answer:?}")),
    }
}

async fn written(host: &Host, rng: &mut Rng, shape: Shape, wire: &Arc<Wire>) -> Result<(), String> {
    let log = opened(host, shape).await?;
    let runtime = runtime(&log, wire, session_store(&log)?).await?;
    let agent = memory::claude_spawn(&runtime, true).await?;
    for at in 0..shape.turns {
        cell_turn(&log, &agent, wire, rng, at).await?;
    }
    memory::close(Some(agent)).await;
    drop(runtime.shutdown().await);
    let mut store = log.store();
    store
        .flush()
        .await
        .map_err(|error| format!("the log does not flush: {error}"))
}

pub(super) async fn reopen(seed: u64, shape: Shape) -> Result<(Reopen, u64), String> {
    let host = Host::new();
    let mut rng = Rng::derive(seed, "warm-cells");
    let wire = Arc::new(Wire {
        sizes: Sizes {
            stored: rng.range(STORED_BYTES.start, STORED_BYTES.end),
            source: rng.range(SOURCE_BYTES.start, SOURCE_BYTES.end),
            text: rng.range(TEXT_BYTES.start, TEXT_BYTES.end),
        },
        turn: AtomicU64::new(0),
        round: AtomicU64::new(0),
    });
    written(&host, &mut rng, shape, &wire).await?;
    let before = host.meter.cost();
    let log = opened(&host, shape).await?;
    let open = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    let before = host.meter.cost();
    let store = session_store(&log)?;
    let runtime = runtime(&log, &wire, store.clone()).await?;
    let record = SessionRecord::root(
        memory::SESSION,
        ModelSelection::new(HarnessFamily::Claude, memory::CLAUDE_MODEL),
        None,
    );
    let (inbox, pump) = TurnInbox::open(
        (*runtime).clone(),
        &store,
        record,
        Arc::clone(&log),
        Arc::new(Quiet),
        Duration::ZERO,
    )
    .await
    .map_err(|error| format!("the inbox does not open: {error}"))?;
    let restore = host
        .meter
        .cost()
        .since(before, log.read(Fold::resident_bytes).await);
    drop(inbox);
    drop(pump);
    drop(runtime.shutdown().await);
    let reopen = Reopen {
        open,
        restore,
        read_all: restore.resident,
    };
    Ok((reopen, row_bytes_max(&host).await?))
}

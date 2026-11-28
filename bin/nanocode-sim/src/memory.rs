use std::{
    future::Future,
    ops::Range,
    path::PathBuf,
    pin::Pin,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use nanocode::{
    ClaudeClient, Nanocode, PromptRequest, Thinking, Tools,
    claude::transport::{HttpError, HttpFuture, HttpRequest, HttpResponse, HttpTransport},
    durability::SessionStore,
};
use nanocode_durability_core::{KeyValue, KeyValueStore, StoreError, StoreFuture, Versioned};
use nanocode_sync::Atom;
use serde_json::{Value, json};

use crate::{
    executor::{Clock, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

pub type Heap = nanocode_sys::alloc::Counted<std::alloc::System>;

pub const RETAINED_PER_HISTORY_BYTE: f64 = 1.05;
pub const RETAINED_PER_MESSAGE_BYTES: usize = 1 << 10;
pub const RETAINED_CONSTANT_BYTES: usize = 2 << 20;
pub const TRANSIENT_PER_LARGEST_MESSAGE: usize = 4;
pub const TRANSIENT_PER_REQUEST_BODY: usize = 1;
pub const TRANSIENT_CONSTANT_BYTES: usize = 4 << 20;

pub(crate) const SESSION: &str = "0190f5a2-7c1e-7000-8000-00000000a11c";
const TOOL: &str = "blob";
const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const BASE_URL: &str = "http://model.sim/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const MIB: f64 = 1_048_576.0;
const RATIO_FLOOR_BYTES: usize = 1 << 20;
const SIZE_PROFILES: &[(&str, usize, usize)] = &[
    ("mixed", 200, 64 << 10),
    ("tools", 1 << 10, 64 << 10),
    ("chat", 64, 2 << 10),
];
const GATE: &str = include_str!("../memory-gate.txt");
const PEAK_FIELD: &str = "peak_bytes=";
const MARGIN_FIELD: &str = "margin_bytes=";
const STEP_TRANSIENT_ALLOC: &str = "step_transient_alloc";
static STEP_TRANSIENT_BYTES: AtomicUsize = AtomicUsize::new(0);
const CACHE_MARKS: &[&[u8]] = &[
    b",\"cache_control\":{\"type\":\"ephemeral\",\"ttl\":\"1h\"}",
    b",\"cache_control\":{\"type\":\"ephemeral\",\"ttl\":\"5m\"}",
    b",\"cache_control\":{\"type\":\"ephemeral\"}",
];

type Opened<'a> = Pin<Box<dyn Future<Output = Result<nanocode::Agent, String>> + 'a>>;
type Build = fn(&Arc<Model>, Option<SessionStore>, Tools) -> Result<Nanocode, String>;

pub struct Backend {
    pub name: &'static str,
    pub array: &'static [u8],
    pub volatile: &'static [&'static [u8]],
    pub edits: &'static [&'static [u8]],
    pub runtime: Build,
    pub spawn: for<'a> fn(&'a Nanocode, bool) -> Opened<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reopen {
    Never,
    Open,
    Loaded,
    Replay,
}

pub struct Mode {
    pub name: &'static str,
    pub durable: bool,
    pub memo: bool,
    pub reopen: Reopen,
}

pub const BACKENDS: &[Backend] = &[
    Backend {
        name: "claude",
        array: b"messages",
        volatile: &[],
        edits: &[b"[Old tool result content cleared]"],
        runtime: claude_runtime,
        spawn: claude_spawn,
    },
    Backend {
        name: "codex",
        array: b"input",
        volatile: &[b"client_metadata"],
        edits: &[],
        runtime: codex_runtime,
        spawn: codex_spawn,
    },
];

pub const MODES: &[Mode] = &[
    Mode {
        name: "paged",
        durable: true,
        memo: false,
        reopen: Reopen::Never,
    },
    Mode {
        name: "memo",
        durable: true,
        memo: true,
        reopen: Reopen::Loaded,
    },
    Mode {
        name: "reload",
        durable: true,
        memo: false,
        reopen: Reopen::Open,
    },
    Mode {
        name: "replay",
        durable: true,
        memo: false,
        reopen: Reopen::Replay,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
struct Peak {
    measured: usize,
    margin: usize,
}

impl Peak {
    const fn limit(self) -> usize {
        self.measured + self.margin
    }
}

#[derive(Clone)]
struct Gated {
    combo: String,
    retained: bool,
    peak: Option<Peak>,
}

impl Gated {
    fn parse(entry: &str) -> Option<Self> {
        let mut fields = entry.split_whitespace();
        let first = fields.next()?;
        let (combo, retained) = match first.split_once('=') {
            Some((combo, "retained")) => (combo, true),
            Some(_) => return None,
            None => (first, false),
        };
        let (mut measured, mut margin) = (None, None);
        for field in fields {
            if let Some(bytes) = field.strip_prefix(PEAK_FIELD) {
                measured = Some(bytes.parse().ok()?);
            } else {
                margin = Some(field.strip_prefix(MARGIN_FIELD)?.parse().ok()?);
            }
        }
        let peak = match (measured, margin) {
            (Some(measured), Some(margin)) => Some(Peak { measured, margin }),
            (None, None) => None,
            _ => return None,
        };
        Some(Self {
            combo: combo.to_owned(),
            retained,
            peak,
        })
    }

    fn file() -> Vec<Self> {
        GATE.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| Self::parse(line).expect("memory-gate.txt lines parse"))
            .collect()
    }
}

#[derive(Clone)]
struct Options {
    turns: u64,
    rounds: u64,
    min: usize,
    max: usize,
    seed: u64,
    only: Vec<Gated>,
    gate: bool,
    gate_transient: bool,
    step_transient: bool,
    verbose: bool,
    retained_k: f64,
    retained_m: usize,
}

impl Options {
    fn parse(args: &[String]) -> Option<Self> {
        let mut options = Self {
            turns: 60,
            rounds: 16,
            min: 200,
            max: 64 << 10,
            seed: 1,
            only: Vec::new(),
            gate: false,
            gate_transient: true,
            step_transient: false,
            verbose: false,
            retained_k: RETAINED_PER_HISTORY_BYTE,
            retained_m: RETAINED_PER_MESSAGE_BYTES,
        };
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--memory-gate" => options.gate = true,
                "--memory-gate-retained" => {
                    options.gate = true;
                    options.gate_transient = false;
                }
                "--verbose" => options.verbose = true,
                "--turns" => options.turns = rest.next()?.parse().ok()?,
                "--rounds" => options.rounds = rest.next()?.parse().ok()?,
                "--min-bytes" => options.min = rest.next()?.parse().ok()?,
                "--max-bytes" => options.max = rest.next()?.parse().ok()?,
                "--seed" => options.seed = rest.next()?.parse().ok()?,
                "--retained-k" => options.retained_k = rest.next()?.parse().ok()?,
                "--retained-m" => options.retained_m = rest.next()?.parse().ok()?,
                "--size-profile" => {
                    let name = rest.next()?;
                    let &(_, min, max) = SIZE_PROFILES
                        .iter()
                        .find(|(profile, _, _)| profile == name)?;
                    options.min = min;
                    options.max = max;
                }
                "--only" => {
                    for entry in rest.next()?.split(',') {
                        options.only.push(Gated::parse(entry)?);
                    }
                }
                "--mutation" if rest.next()? == STEP_TRANSIENT_ALLOC => {
                    options.step_transient = true;
                }
                _ => return None,
            }
        }
        (options.min > 0 && options.min <= options.max && options.rounds > 0).then_some(options)
    }

    fn wants(&self, combo: &str) -> bool {
        self.only.is_empty()
            || self
                .only
                .iter()
                .any(|only| combo.contains(only.combo.as_str()))
    }

    fn gates_transient(&self, combo: &str) -> bool {
        self.gate_transient
            && !self
                .only
                .iter()
                .any(|only| only.retained && combo.contains(only.combo.as_str()))
    }

    fn peak(&self, combo: &str) -> Option<Peak> {
        self.only
            .iter()
            .filter(|only| combo.contains(only.combo.as_str()))
            .find_map(|only| only.peak)
    }
}

pub fn main(args: &[String]) -> ExitCode {
    let Some(options) = Options::parse(args) else {
        eprintln!(
            "usage: nanocode-sim --memory [--memory-gate | --memory-gate-retained] [--only 'BACKEND/MODE[=retained] [peak_bytes=N margin_bytes=N]',...] [--mutation step_transient_alloc] [--turns N] [--rounds N] [--size-profile mixed|tools|chat] [--min-bytes N] [--max-bytes N] [--seed S] [--retained-k K] [--retained-m M] [--verbose]"
        );
        return ExitCode::FAILURE;
    };
    Heap::start();
    println!(
        "memory invariant: retained <= {} x history + {} B x messages + {} MiB, step transient <= {TRANSIENT_PER_REQUEST_BODY} x request body + {TRANSIENT_PER_LARGEST_MESSAGE} x largest message + {} MiB, wire prefix byte-stable",
        options.retained_k,
        options.retained_m,
        RETAINED_CONSTANT_BYTES >> 20,
        TRANSIENT_CONSTANT_BYTES >> 20
    );
    println!(
        "tool results {}..{} bytes, log-uniform",
        options.min, options.max
    );
    println!(
        "{:<14} {:>9} {:>6} {:>9} {:>7} {:>7} {:>7} {:>9} {:>8} {:>9} {:>9} {:>7}  verdict",
        "combo",
        "history",
        "msgs",
        "retained",
        "max",
        "final",
        "B/msg",
        "transient",
        "xbody",
        "peak",
        "limit",
        "prefix"
    );
    let mut breached = 0_usize;
    let mut broken = 0_usize;
    let mut failed = 0_usize;
    for backend in BACKENDS {
        for mode in MODES {
            let combo = format!("{}/{}", backend.name, mode.name);
            if !options.wants(&combo) {
                continue;
            }
            match measure(backend, mode, &options, options.peak(&combo)) {
                Ok(report) => {
                    let line = report.line(&combo);
                    broken += usize::from(report.unstable > 0);
                    let breaches = report.breaches();
                    if breaches.is_empty() {
                        println!("{line}  PASS");
                    } else {
                        breached += usize::from(
                            options.gates_transient(&combo)
                                || breaches.iter().any(|breach| *breach != "TRANSIENT"),
                        );
                        println!("{line}  MEMORY {} VIOLATION", breaches.join("+"));
                    }
                }
                Err(error) => {
                    failed += 1;
                    println!("{combo:<14} ERROR {error}");
                }
            }
        }
    }
    if failed > 0 || broken > 0 || (options.gate && breached > 0) {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryMutation {
    None,
    StepTransientAlloc,
}

impl MemoryMutation {
    const ALL: [Self; 2] = [Self::None, Self::StepTransientAlloc];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StepTransientAlloc => STEP_TRANSIENT_ALLOC,
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryStats {
    pub seeds: u64,
    pub turns: u64,
    pub peak_bytes_max: u64,
}

impl MemoryStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.turns += other.turns;
        self.peak_bytes_max = self.peak_bytes_max.max(other.peak_bytes_max);
    }
}

fn gated_combo(gated: &Gated) -> Option<(&'static Backend, &'static Mode)> {
    BACKENDS.iter().find_map(|backend| {
        MODES
            .iter()
            .find(|mode| gated.combo == format!("{}/{}", backend.name, mode.name))
            .map(|mode| (backend, mode))
    })
}

fn judged(seed: u64, mutation: MemoryMutation, totals: &mut MemoryStats) -> Option<String> {
    let gated = Gated::file();
    assert!(!gated.is_empty(), "memory-gate.txt lists a combo");
    let at = usize::try_from(seed % gated.len() as u64).expect("a combo index fits");
    let entry = gated[at].clone();
    let Some((backend, mode)) = gated_combo(&entry) else {
        return Some(format!("memory-gate.txt names no combo {}", entry.combo));
    };
    let Some(mut options) = Options::parse(&[]) else {
        return Some("the memory defaults do not parse".to_owned());
    };
    options.gate = true;
    options.gate_transient = !entry.retained;
    options.step_transient = mutation == MemoryMutation::StepTransientAlloc;
    options.only = vec![entry.clone()];
    Heap::start();
    let report = match measure(backend, mode, &options, entry.peak) {
        Ok(report) => report,
        Err(error) => return Some(format!("memory {}: {error}", entry.combo)),
    };
    totals.seeds += 1;
    totals.turns += report.turns;
    totals.peak_bytes_max = totals.peak_bytes_max.max(report.peak as u64);
    let breaches = report.breaches();
    let gating = breaches
        .iter()
        .any(|breach| options.gate_transient || *breach != "TRANSIENT");
    gating.then(|| {
        format!(
            "memory {}: {} breached: peak {} B past limit {}, transient {} B",
            entry.combo,
            breaches.join("+"),
            report.peak,
            report
                .peak_limit
                .map_or_else(|| "none".to_owned(), |limit| format!("{limit} B")),
            report.transient
        )
    })
}

pub fn judge(
    seed: u64,
    mutation: MemoryMutation,
    twice: bool,
    verbose: bool,
    totals: &mut MemoryStats,
) -> Option<String> {
    let first = judged(seed, mutation, totals);
    if verbose {
        println!("seed {seed} memory stats={totals:?}");
    }
    if first.is_some() || !twice {
        return first;
    }
    judged(seed, mutation, totals)
}

pub fn replay(seed: u64, mutation: MemoryMutation) -> String {
    let mut command = format!("nanocode-sim --scenario memory --seed {seed}");
    if mutation != MemoryMutation::None {
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
    mutation: MemoryMutation,
    totals: &MemoryStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=memory mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        MemoryMutation::None if failed == 0 => ExitCode::SUCCESS,
        MemoryMutation::None => ExitCode::FAILURE,
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

#[derive(Default)]
struct Report {
    turns: u64,
    history: usize,
    messages: usize,
    largest: usize,
    retained: usize,
    ratio_max: f64,
    ratio_final: f64,
    retained_over: bool,
    transient: usize,
    transient_over: bool,
    peak: usize,
    peak_limit: Option<usize>,
    body: usize,
    requests: u64,
    unstable: u64,
    replayed_calls: u64,
}

impl Report {
    fn line(&self, combo: &str) -> String {
        format!(
            "{combo:<14} {:>8.1}M {:>6} {:>8.1}M {:>7.2} {:>7.2} {:>7.0} {:>8.1}M {:>8.2} {:>9} {:>9} {:>7}",
            self.history as f64 / MIB,
            self.messages,
            self.retained as f64 / MIB,
            self.ratio_max,
            self.ratio_final,
            self.retained.saturating_sub(self.history) as f64 / self.messages.max(1) as f64,
            self.transient as f64 / MIB,
            self.transient as f64 / self.body.max(1) as f64,
            self.peak,
            self.peak_limit
                .map_or_else(|| "-".to_owned(), |limit| limit.to_string()),
            if self.unstable == 0 {
                "stable"
            } else {
                "broken"
            },
        )
    }

    fn breaches(&self) -> Vec<&'static str> {
        let mut breaches = Vec::new();
        if self.retained_over {
            breaches.push("RETAINED");
        }
        if self.transient_over {
            breaches.push("TRANSIENT");
        }
        if self.peak_limit.is_some_and(|limit| self.peak > limit) {
            breaches.push("PEAK");
        }
        if self.unstable > 0 {
            breaches.push("PREFIX");
        }
        if self.replayed_calls > 0 {
            breaches.push("REPLAY");
        }
        breaches
    }

    fn sample(
        &mut self,
        options: &Options,
        (retained, transient, peak): (usize, usize, usize),
        model: &Model,
    ) {
        let (history, messages, largest, body) = (
            model.history(),
            model.messages(),
            model.largest(),
            model.body(),
        );
        self.history = history;
        self.messages = messages;
        self.largest = largest;
        self.retained = retained;
        self.transient = self.transient.max(transient);
        self.peak = self.peak.max(peak);
        let ratio = retained as f64 / history.max(1) as f64;
        if history >= RATIO_FLOOR_BYTES {
            self.ratio_max = self.ratio_max.max(ratio);
        }
        self.ratio_final = ratio;
        let allowed = options.retained_k * history as f64
            + (options.retained_m * messages) as f64
            + RETAINED_CONSTANT_BYTES as f64;
        self.retained_over |= retained as f64 > allowed;
        self.transient_over |= transient
            > TRANSIENT_PER_REQUEST_BODY * body
                + TRANSIENT_PER_LARGEST_MESSAGE * largest
                + TRANSIENT_CONSTANT_BYTES;
        self.body = body;
    }
}

fn measure(
    backend: &Backend,
    mode: &Mode,
    options: &Options,
    peak: Option<Peak>,
) -> Result<Report, String> {
    let label = format!("memory {} {}", backend.name, mode.name);
    let result = std::cell::RefCell::new(None);
    let clock = Clock::new();
    crate::host::enter(options.seed, &clock);
    let mut executor = Executor::new(Arc::clone(&clock), Rng::derive(options.seed, &label));
    let task: Task<'_> = Box::pin(async {
        result.replace(Some(session(backend, mode, options, peak).await));
    });
    let ended = executor.run(vec![task], None);
    let stuck = match ended {
        crate::executor::Ended::Finished => None,
        crate::executor::Ended::Crashed => Some("crashed"),
        crate::executor::Ended::Stuck => Some("stuck"),
    };
    drop(executor);
    loop {
        let spawned = crate::host::spawned();
        if spawned.is_empty() {
            break;
        }
        drop(spawned);
    }
    STEP_TRANSIENT_BYTES.store(0, Ordering::Release);
    result
        .into_inner()
        .unwrap_or_else(|| {
            Err(format!(
                "the session did not finish: the executor {}",
                stuck.unwrap_or("returned early")
            ))
        })
        .map(|mut report| {
            report.peak_limit = peak.map(Peak::limit);
            report
        })
}

async fn session(
    backend: &Backend,
    mode: &Mode,
    options: &Options,
    peak: Option<Peak>,
) -> Result<Report, String> {
    let transient = match (options.step_transient, peak) {
        (false, _) => 0,
        (true, Some(peak)) => peak.limit(),
        (true, None) => return Err(format!("{STEP_TRANSIENT_ALLOC} sizes from a peak limit")),
    };
    STEP_TRANSIENT_BYTES.store(transient, Ordering::Release);
    let disk = Disk::create(backend.name, mode.name)?;
    let outcome = run(backend, mode, options, &disk).await;
    disk.remove();
    outcome
}

async fn run(
    backend: &Backend,
    mode: &Mode,
    options: &Options,
    disk: &Disk,
) -> Result<Report, String> {
    let model = Arc::new(Model::new(backend, options));
    let store = if mode.durable {
        let store = nanocode::session_store(KeyValueStore::new(FileKv::new(disk.0.clone())))
            .map_err(|error| format!("session store: {error}"))?;
        Some(if mode.memo {
            store.with_payload_memo()
        } else {
            store
        })
    } else {
        None
    };
    let runtime = (backend.runtime)(&model, store.clone(), tools())?;
    let baseline = Heap::count().live;
    let mut report = Report::default();
    let mut agent = Some((backend.spawn)(&runtime, mode.durable).await?);
    for turn in 0..options.turns {
        let start = Heap::reset_peak();
        if turn > 0 && matches!(mode.reopen, Reopen::Open | Reopen::Loaded | Reopen::Replay) {
            close(agent.take()).await;
            agent = Some(reopen(&runtime, store.as_ref(), mode.reopen).await?);
        }
        model.begin(turn);
        let current = agent.as_ref().ok_or("no agent")?;
        prompt(current, turn).await?;
        let held = Heap::count().live;
        if mode.reopen == Reopen::Replay {
            close(agent.take()).await;
            let reopened = reopen(&runtime, store.as_ref(), Reopen::Open).await?;
            model.replaying(true);
            let replayed = prompt(&reopened, turn).await;
            model.replaying(false);
            replayed?;
            agent = Some(reopened);
        }
        let end = Heap::count();
        let retained = end.live.saturating_sub(baseline);
        let transient = end.peak.saturating_sub(start.max(end.live).max(held));
        report.sample(options, (retained, transient, end.peak), &model);
        report.turns = turn + 1;
        if options.verbose {
            println!(
                "  turn {turn}: history {:.2} MiB retained {:.2} MiB transient {:.2} MiB peak {} B",
                model.history() as f64 / MIB,
                retained as f64 / MIB,
                transient as f64 / MIB,
                end.peak
            );
        }
    }
    report.requests = model.requests.load(Ordering::Acquire);
    report.unstable = model.unstable.load(Ordering::Acquire);
    report.replayed_calls = model.replayed.load(Ordering::Acquire);
    close(agent.take()).await;
    drop(runtime.shutdown().await);
    Ok(report)
}

pub(crate) async fn close(agent: Option<nanocode::Agent>) {
    if let Some(agent) = agent {
        drop(agent.flush().await);
        drop(agent);
    }
}

pub(crate) async fn reopen(
    runtime: &Nanocode,
    store: Option<&SessionStore>,
    how: Reopen,
) -> Result<nanocode::Agent, String> {
    let opened = match (how, store) {
        (Reopen::Loaded, Some(store)) => {
            let stored = store
                .load(SESSION)
                .await
                .map_err(|error| format!("load: {error}"))?;
            runtime.open_loaded_owned(store, stored).await
        }
        _ => runtime.open_session(SESSION).await,
    };
    let (agent, events) = opened.map_err(|error| format!("reopen: {error}"))?;
    drop(events);
    Ok(agent)
}

pub(crate) async fn prompt(agent: &nanocode::Agent, turn: u64) -> Result<(), String> {
    let request = format!("memory-turn-{turn}");
    let running = agent
        .prompt(PromptRequest::new(format!("turn {turn}: read the blobs")).request_id(&request))
        .await
        .map_err(|error| format!("turn {turn}: {error}"))?;
    running
        .result()
        .await
        .map(drop)
        .map_err(|error| format!("turn {turn} result: {error}"))
}

pub(crate) fn tools() -> Tools {
    Tools::builder()
        .without_defaults()
        .tool(nanocode::FunctionTool::new(
            TOOL,
            "Read a blob of the given size",
            json!({"type": "object", "properties": {"bytes": {"type": "integer"}, "turn": {"type": "integer"}, "round": {"type": "integer"}}, "required": ["bytes"]}),
            Arc::new(Blob),
        ))
        .build()
        .expect("the memory tools build")
}

struct Blob;

impl nanocode::FunctionHandler for Blob {
    fn call<'a>(
        &'a self,
        _name: &'a str,
        input: Value,
        _context: nanocode::tools::ToolContext<'a>,
    ) -> nanocode::HostFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let field = |key: &str| input.get(key).and_then(Value::as_u64).unwrap_or(0);
            let bytes = usize::try_from(field("bytes")).unwrap_or(0);
            let transient = STEP_TRANSIENT_BYTES.load(Ordering::Acquire);
            if transient > 0 && field("round") == 0 {
                let held = vec![0_u8; transient];
                assert_eq!(
                    std::hint::black_box(&held).len(),
                    transient,
                    "the step's transient holds its bytes"
                );
            }
            Ok(Value::String(blob(bytes, field("turn"), field("round"))))
        })
    }
}

fn blob(bytes: usize, turn: u64, round: u64) -> String {
    let mut text = String::with_capacity(bytes + 96);
    let mut line = 0_u64;
    while text.len() < bytes {
        let word = fnv(
            fnv(FNV_OFFSET, &turn.to_le_bytes()),
            &(round << 32 | line).to_le_bytes(),
        );
        text.push_str(&format!(
            "t{turn} r{round} l{line} {word:016x} lorem ipsum dolor sit amet\n"
        ));
        line += 1;
    }
    text.truncate(bytes);
    text
}

pub struct Model {
    array: &'static [u8],
    volatile: &'static [&'static [u8]],
    edits: &'static [&'static [u8]],
    rounds: u64,
    min: usize,
    max: usize,
    seed: u64,
    turn: AtomicU64,
    round: AtomicU64,
    requests: AtomicU64,
    unstable: AtomicU64,
    replayed: AtomicU64,
    replaying: AtomicBool,
    history: AtomicUsize,
    largest: AtomicUsize,
    body: AtomicUsize,
    prefix: Atom<(u64, Vec<u64>)>,
    lengths: Atom<Vec<usize>>,
}

enum Reply {
    Tool { id: String, input: Value },
    Text(String),
}

impl Model {
    pub(crate) fn sized(backend: &Backend, rounds: u64, min: usize, max: usize, seed: u64) -> Self {
        Self::new(
            backend,
            &Options {
                turns: 0,
                rounds,
                min,
                max,
                seed,
                only: Vec::new(),
                gate: false,
                gate_transient: false,
                step_transient: false,
                verbose: false,
                retained_k: 0.0,
                retained_m: 0,
            },
        )
    }

    fn new(backend: &Backend, options: &Options) -> Self {
        Self {
            array: backend.array,
            volatile: backend.volatile,
            edits: backend.edits,
            rounds: options.rounds,
            min: options.min,
            max: options.max,
            seed: options.seed,
            turn: AtomicU64::new(0),
            round: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            unstable: AtomicU64::new(0),
            replayed: AtomicU64::new(0),
            replaying: AtomicBool::new(false),
            history: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
            body: AtomicUsize::new(0),
            prefix: Atom::new((0, Vec::new())),
            lengths: Atom::new(Vec::new()),
        }
    }

    pub(crate) fn begin(&self, turn: u64) {
        self.turn.store(turn, Ordering::Release);
        self.round.store(0, Ordering::Release);
    }

    fn replaying(&self, on: bool) {
        self.replaying.store(on, Ordering::Release);
    }

    fn history(&self) -> usize {
        self.history.load(Ordering::Acquire)
    }

    fn messages(&self) -> usize {
        self.lengths.read(Vec::len)
    }

    fn largest(&self) -> usize {
        self.largest.load(Ordering::Acquire)
    }

    fn body(&self) -> usize {
        self.body.load(Ordering::Acquire)
    }

    fn size(&self, turn: u64, round: u64) -> usize {
        let mut rng = Rng::derive(self.seed, &format!("blob {turn} {round}"));
        let span = (self.max as f64 / self.min as f64).ln();
        let unit = rng.below(1_000_000) as f64 / 1_000_000.0;
        ((self.min as f64) * (span * unit).exp()) as usize
    }

    fn reply(&self, body: &[u8]) -> Reply {
        let number = self.requests.fetch_add(1, Ordering::AcqRel) + 1;
        if self.replaying.load(Ordering::Acquire) {
            self.replayed.fetch_add(1, Ordering::AcqRel);
        }
        self.observe(body);
        let turn = self.turn.load(Ordering::Acquire);
        let round = self.round.fetch_add(1, Ordering::AcqRel);
        if round >= self.rounds {
            return Reply::Text(format!("turn {turn} done"));
        }
        Reply::Tool {
            id: format!("call_{number}"),
            input: json!({"bytes": self.size(turn, round), "turn": turn, "round": round}),
        }
    }

    fn observe(&self, body: &[u8]) {
        if let Some(dump) = std::env::var_os("NANOCODE_SIM_MEMORY_DUMP") {
            let number = self.requests.load(Ordering::Acquire);
            drop(std::fs::create_dir_all(&dump));
            drop(std::fs::write(
                PathBuf::from(&dump).join(format!("{number:05}.json")),
                body,
            ));
        }
        let scan = Scan::of(body, self.array, self.volatile, self.edits);
        self.body.fetch_max(body.len(), Ordering::AcqRel);
        self.largest.fetch_max(scan.largest, Ordering::AcqRel);
        let mut lengths = self.lengths.read(Vec::clone);
        lengths.resize(lengths.len().max(scan.lengths.len()), 0);
        for (kept, seen) in lengths.iter_mut().zip(&scan.lengths) {
            *kept = (*kept).max(*seen);
        }
        self.history.store(lengths.iter().sum(), Ordering::Release);
        self.lengths.set(lengths);
        let stable = self.prefix.read(|(head, items)| {
            items.is_empty()
                || (*head == scan.head
                    && scan.items.len() >= items.len()
                    && items
                        .iter()
                        .zip(&scan.items)
                        .zip(&scan.edited)
                        .all(|((before, now), edited)| before == now || *edited))
        });
        if !stable {
            self.unstable.fetch_add(1, Ordering::AcqRel);
        }
        self.prefix.set((scan.head, scan.items));
    }
}

struct Scan {
    head: u64,
    items: Vec<u64>,
    edited: Vec<bool>,
    lengths: Vec<usize>,
    largest: usize,
}

impl Scan {
    fn of(body: &[u8], array: &[u8], volatile: &[&[u8]], edits: &[&[u8]]) -> Self {
        let mut scan = Self {
            head: FNV_OFFSET,
            items: Vec::new(),
            edited: Vec::new(),
            lengths: Vec::new(),
            largest: 0,
        };
        let mut depth = 0_usize;
        let mut quoted = false;
        let mut escaped = false;
        let mut key = (0_usize, 0_usize);
        let mut string_start = 0_usize;
        let mut inside = false;
        let mut item: Option<(usize, u64)> = None;
        let mut skipping = false;
        let mut at = 0_usize;
        while at < body.len() {
            let byte = body[at];
            if item.is_some()
                && !quoted
                && let Some(mark) = CACHE_MARKS.iter().find(|mark| body[at..].starts_with(mark))
            {
                at += mark.len();
                continue;
            }
            if let Some((_, hash)) = item.as_mut() {
                *hash = fnv(*hash, &[byte]);
            } else if !skipping && (!inside || depth < 2) {
                scan.head = fnv(scan.head, &[byte]);
            }
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                    if depth == 1 {
                        key = (string_start, at);
                    }
                }
                at += 1;
                continue;
            }
            if skipping && depth == 1 && matches!(byte, b',' | b'}') {
                skipping = false;
            }
            match byte {
                b':' if depth == 1 && volatile.contains(&&body[key.0..key.1]) => {
                    skipping = true;
                }
                b'"' => {
                    quoted = true;
                    string_start = at + 1;
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth == 2 && byte == b'[' && &body[key.0..key.1] == array {
                        inside = true;
                    } else if inside && depth == 3 && item.is_none() {
                        item = Some((at, fnv(FNV_OFFSET, &[byte])));
                    }
                }
                b']' | b'}' => {
                    if inside
                        && depth == 3
                        && let Some((start, hash)) = item.take()
                    {
                        let length = at + 1 - start;
                        let slice = &body[start..=at];
                        scan.edited.push(
                            edits.iter().any(|edit| {
                                slice.windows(edit.len()).any(|window| window == *edit)
                            }),
                        );
                        scan.lengths.push(length);
                        scan.largest = scan.largest.max(length);
                        scan.items.push(hash);
                    }
                    if inside && depth == 2 {
                        inside = false;
                        key = (0, 0);
                    }
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
            at += 1;
        }
        scan
    }
}

struct ClaudeWire(Arc<Model>);

impl HttpTransport for ClaudeWire {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let reply = self.0.reply(request.body.as_bytes());
            drop(request);
            let number = self.0.requests.load(Ordering::Acquire);
            let (block, delta, stop) = match reply {
                Reply::Tool { id, input } => (
                    json!({"type": "tool_use", "id": format!("toolu_{id}"), "name": TOOL, "input": {}}),
                    json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                    "tool_use",
                ),
                Reply::Text(text) => (
                    json!({"type": "text", "text": ""}),
                    json!({"type": "text_delta", "text": text}),
                    "end_turn",
                ),
            };
            let sse = crate::server::events(
                CLAUDE_MODEL,
                &format!("mem{number}"),
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

struct CodexWire(Arc<Model>);

impl nanocode::http::Layer for CodexWire {
    fn send<'a>(
        &'a self,
        request: nanocode::http::Request,
        _next: nanocode::http::Next<'a>,
    ) -> nanocode::http::BoxFuture<'a, Result<nanocode::http::Response, nanocode::http::Error>>
    {
        Box::pin(async move {
            let reply = self.0.reply(&request.body);
            drop(request);
            let number = self.0.requests.load(Ordering::Acquire);
            let id = format!("resp_mem{number}");
            let item = match reply {
                Reply::Tool { id: call, input } => json!({
                    "type": "function_call",
                    "id": format!("fc_{call}"),
                    "call_id": call,
                    "name": TOOL,
                    "arguments": input.to_string(),
                }),
                Reply::Text(text) => json!({
                    "type": "message",
                    "id": format!("msg_{number}"),
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}],
                }),
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

fn builder(store: Option<SessionStore>, tools: Tools) -> nanocode::NanocodeBuilder {
    let builder = Nanocode::builder()
        .tools(tools)
        .runner_management(false)
        .thinking(Thinking::Low)
        .workspace(std::env::temp_dir());
    match store {
        Some(store) => builder.durability(store),
        None => builder,
    }
}

pub(crate) fn claude_runtime(
    model: &Arc<Model>,
    store: Option<SessionStore>,
    tools: Tools,
) -> Result<Nanocode, String> {
    let client = ClaudeClient::messages_api(BASE_URL, "sk-sim", CONNECT_TIMEOUT, IDLE_TIMEOUT)
        .map_err(|error| format!("claude client: {error}"))?
        .with_transport(ClaudeWire(Arc::clone(model)));
    builder(store, tools)
        .claude(nanocode::ClaudeHarness::new(client).direct_tools_for(|_| true))
        .build()
        .map_err(|error| format!("runtime build: {error}"))
}

pub(crate) fn claude_spawn(runtime: &Nanocode, durable: bool) -> Opened<'_> {
    Box::pin(async move {
        let spawn = runtime
            .spawn()
            .backend::<nanocode::Claude>()
            .model(nanocode::ClaudeModel::Haiku55);
        let spawn = if durable {
            spawn.session_id(SESSION)
        } else {
            spawn
        };
        let (agent, events) = spawn
            .build()
            .await
            .map_err(|error| format!("spawn: {error}"))?;
        drop(events);
        Ok(agent)
    })
}

fn codex_runtime(
    model: &Arc<Model>,
    store: Option<SessionStore>,
    tools: Tools,
) -> Result<Nanocode, String> {
    let openai = nanocode::OpenAi::builder(nanocode::codex::OpenAiAuth::api_key("sk-sim"))
        .api_base_url(BASE_URL)
        .transport(nanocode::codex::transport::ResponsesTransport::Https)
        .http_layer(CodexWire(Arc::clone(model)))
        .model(nanocode::codex::Model::Luna)
        .build()
        .map_err(|error| format!("openai client: {error}"))?;
    builder(store, tools)
        .openai(nanocode::CodexHarness::new(openai).direct_tools_for(|_| true))
        .build()
        .map_err(|error| format!("runtime build: {error}"))
}

fn codex_spawn(runtime: &Nanocode, durable: bool) -> Opened<'_> {
    Box::pin(async move {
        let spawn = runtime
            .spawn()
            .backend::<nanocode::Codex>()
            .model(nanocode::codex::Model::Luna);
        let spawn = if durable {
            spawn.session_id(SESSION)
        } else {
            spawn
        };
        let (agent, events) = spawn
            .build()
            .await
            .map_err(|error| format!("spawn: {error}"))?;
        drop(events);
        Ok(agent)
    })
}

struct Disk(Arc<PathBuf>);

impl Disk {
    fn create(backend: &str, mode: &str) -> Result<Self, String> {
        let root = std::env::temp_dir().join(format!(
            "nanocode-sim-memory-{}-{backend}-{mode}",
            std::process::id()
        ));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(&root).map_err(|error| format!("store dir: {error}"))?;
        Ok(Self(Arc::new(root)))
    }

    fn remove(&self) {
        drop(std::fs::remove_dir_all(self.0.as_path()));
    }
}

#[derive(Clone)]
struct FileKv {
    root: Arc<PathBuf>,
    versions: Arc<AtomicU64>,
}

impl FileKv {
    fn new(root: Arc<PathBuf>) -> Self {
        Self {
            root,
            versions: Arc::new(AtomicU64::new(0)),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        let name: String = key.bytes().map(|byte| format!("{byte:02x}")).collect();
        self.root.join(name)
    }

    fn read(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        match std::fs::read_to_string(self.path(key)) {
            Ok(mut text) => {
                let split = text.find('\n').unwrap_or(text.len());
                let value = text.split_off((split + 1).min(text.len()));
                text.truncate(split);
                Ok(Some(Versioned {
                    version: text,
                    value,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::Backend(format!("read {key}: {error}"))),
        }
    }

    fn write(&self, key: &str, version: &str, value: &str) -> Result<(), StoreError> {
        let mut text = String::with_capacity(version.len() + 1 + value.len());
        text.push_str(version);
        text.push('\n');
        text.push_str(value);
        std::fs::write(self.path(key), text)
            .map_err(|error| StoreError::Backend(format!("write {key}: {error}")))
    }
}

impl KeyValue for FileKv {
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Result<Option<Versioned>, StoreError>> {
        Box::pin(async move { self.read(key) })
    }

    fn put_values<'a>(
        &'a self,
        entries: &'a [(String, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            for (key, value) in entries {
                let version = self.versions.fetch_add(1, Ordering::AcqRel).to_string();
                self.write(key, &version, value)?;
            }
            Ok(())
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_version: Option<&'a str>,
        version: &'a str,
        value: &'a str,
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let current = self.read(key)?.map(|entry| entry.version);
            if current.as_deref() != expected_version {
                return Ok(false);
            }
            self.write(key, version, value)?;
            Ok(true)
        })
    }

    fn list_keys<'a>(
        &'a self,
        prefix: &'a str,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(async move {
            let entries = std::fs::read_dir(self.root.as_path())
                .map_err(|error| StoreError::Backend(format!("list: {error}")))?;
            let mut keys = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let bytes: Option<Vec<u8>> = (0..name.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(name.get(at..at + 2)?, 16).ok())
                    .collect();
                if let Some(key) = bytes.and_then(|bytes| String::from_utf8(bytes).ok())
                    && key.starts_with(prefix)
                {
                    keys.push(key);
                }
            }
            keys.sort();
            Ok(keys)
        })
    }
}

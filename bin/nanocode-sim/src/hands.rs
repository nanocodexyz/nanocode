use std::{
    cell::RefCell,
    collections::BTreeMap,
    ops::Range,
    process::ExitCode,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_code_mode::embedded::{CodeModeHostError, HostFuture};
use nanocode_code_mode::remote::{
    NOT_RECEIVED, PEER_CLOSED, RemoteCatalog, RemoteContext, RemoteDelivery, RemoteFrame,
    RemoteLink, RemoteOperation, RemoteReply, RemoteTransport,
};
use nanocode_http::StatusCode;
use nanocode_sync::{Atom, Owned, mpsc};
use nanocode_tool_runtime::remote::accept::{
    Acceptor, AcceptorOptions, AcceptorRoute, HandRequests, PendingRequest, Refusal,
};
use serde_json::{Value, json};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

const KEY: &str = "hand";
const SESSION: &str = "session";
const TURN: &str = "turn";
const CALLS_MAX: u64 = 5;
const START_MS_MAX: u64 = 300;
const RUN_MS_MAX: u64 = 400;
const COMMIT_MS_MAX: u64 = 20;
const CRASHES_MAX: u64 = 3;
const DROPS_MAX: u64 = 3;
const EVENT_MS_MAX: u64 = 1_500;
const RESTART_PPM: u64 = 300_000;
const REDIAL_MS_MAX: u64 = 40;
const WRITE_MS_MAX: u64 = 3;
const TICK: Duration = Duration::from_millis(1);
const GRACE: Duration = Duration::from_secs(5);
const SETTLE_HORIZON_MS: u64 = 60_000;
const FRAME_BYTES_MAX: usize = 1 << 20;
const RUNTIMES_MAX: usize = 4;
const ATTACHES_QUEUED_MAX: usize = 64;
const MS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandsMutation {
    None,
    AckBeforeStore,
    SkipRecord,
    ForgetPending,
    CrossReplies,
    MangleRuntime,
}

impl HandsMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::AckBeforeStore,
        Self::SkipRecord,
        Self::ForgetPending,
        Self::CrossReplies,
        Self::MangleRuntime,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AckBeforeStore => "ack_before_store",
            Self::SkipRecord => "skip_record",
            Self::ForgetPending => "forget_pending",
            Self::CrossReplies => "cross_replies",
            Self::MangleRuntime => "mangle_runtime",
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

#[derive(Debug, Default)]
pub struct HandsStats {
    pub seeds: u64,
    pub crashes: u64,
    pub restarts: u64,
    pub drops: u64,
    pub calls: u64,
    pub recovered: u64,
    pub unknown: u64,
}

impl HandsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.crashes += other.crashes;
        self.restarts += other.restarts;
        self.drops += other.drops;
        self.calls += other.calls;
        self.recovered += other.recovered;
        self.unknown += other.unknown;
    }
}

struct Script {
    starts: Vec<u64>,
    runs: Vec<u64>,
    commits: Vec<u64>,
    crashes: Vec<u64>,
    restarts: Vec<u64>,
    drops: Vec<u64>,
}

fn times(rng: &mut Rng, count: u64) -> Vec<u64> {
    let mut times: Vec<u64> = (0..count)
        .map(|_| rng.range(1, EVENT_MS_MAX) * MS)
        .collect();
    times.sort_unstable();
    times
}

impl Script {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "hands script");
        let calls = rng.range(1, CALLS_MAX);
        let starts = (0..calls)
            .map(|_| rng.range(0, START_MS_MAX) * MS)
            .collect();
        let runs = (0..calls).map(|_| rng.range(1, RUN_MS_MAX) * MS).collect();
        let commits = (0..calls)
            .map(|_| rng.range(0, COMMIT_MS_MAX) * MS)
            .collect();
        let crashes = rng.range(0, CRASHES_MAX);
        let crashes = times(&mut rng, crashes);
        let restarts = u64::from(rng.chance(RESTART_PPM));
        let restarts = times(&mut rng, restarts);
        let drops = rng.range(0, DROPS_MAX);
        let drops = times(&mut rng, drops);
        Self {
            starts,
            runs,
            commits,
            crashes,
            restarts,
            drops,
        }
    }

    fn describe(&self) -> String {
        format!(
            "calls={} crashes={} restarts={} drops={}",
            self.starts.len(),
            self.crashes.len(),
            self.restarts.len(),
            self.drops.len()
        )
    }
}

#[derive(Clone)]
struct Row {
    id: u64,
    key: String,
    runtime: String,
    turn_id: Option<String>,
    reply: Option<RemoteReply>,
}

struct Store {
    rows: Atom<BTreeMap<(String, String), Row>>,
    writes: AtomicU64,
    seed: u64,
    mutation: HandsMutation,
}

impl Store {
    async fn write(&self) {
        let count = self.writes.fetch_add(1, Ordering::Relaxed);
        let delay = fnv(self.seed, &count.to_le_bytes()) % WRITE_MS_MAX + 1;
        nanocode_platform::sleep(Duration::from_millis(delay)).await;
    }

    fn unanswered(&self, id: u64) -> bool {
        self.rows
            .read(|rows| rows.values().any(|row| row.id == id && row.reply.is_none()))
    }
}

impl HandRequests for Store {
    fn sent<'a>(&'a self, request: &'a PendingRequest) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.write().await;
            if self.mutation == HandsMutation::SkipRecord {
                return Ok(());
            }
            let runtime = if self.mutation == HandsMutation::MangleRuntime {
                String::new()
            } else {
                request.runtime.clone()
            };
            let row = Row {
                id: request.id,
                key: request.key.clone(),
                runtime,
                turn_id: request.turn_id.clone(),
                reply: None,
            };
            let at = (request.session_id.clone(), request.call_id.clone());
            self.rows.update(|rows| {
                rows.insert(at.clone(), row.clone());
            });
            Ok(())
        })
    }

    fn replied<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
        id: u64,
        reply: &'a RemoteReply,
    ) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.mutation == HandsMutation::AckBeforeStore {
                return Ok(());
            }
            self.write().await;
            let cross = self.mutation == HandsMutation::CrossReplies;
            let runtime = if self.mutation == HandsMutation::MangleRuntime {
                ""
            } else {
                runtime
            };
            self.rows.update(|rows| {
                for row in rows.values_mut() {
                    let same = row.key == key && row.runtime == runtime;
                    if same && (row.id == id || (cross && row.reply.is_none())) {
                        row.reply = Some(reply.clone());
                    }
                }
            });
            Ok(())
        })
    }

    fn pending<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
    ) -> HostFuture<'a, Result<Vec<u64>, String>> {
        Box::pin(async move {
            self.write().await;
            if self.mutation == HandsMutation::ForgetPending {
                return Ok(Vec::new());
            }
            Ok(self.rows.read(|rows| {
                rows.values()
                    .filter(|row| row.key == key && row.runtime == runtime && row.reply.is_none())
                    .map(|row| row.id)
                    .collect()
            }))
        })
    }

    fn find<'a>(
        &'a self,
        session_id: &'a str,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<PendingRequest>, String>> {
        Box::pin(async move {
            self.write().await;
            let at = (session_id.to_owned(), call_id.to_owned());
            Ok(self.rows.read(|rows| {
                rows.get(&at).map(|row| PendingRequest {
                    id: row.id,
                    key: row.key.clone(),
                    runtime: row.runtime.clone(),
                    session_id: session_id.to_owned(),
                    call_id: call_id.to_owned(),
                    turn_id: row.turn_id.clone(),
                    reply: row.reply.clone(),
                })
            }))
        })
    }

    fn settle<'a>(&'a self, session_id: &'a str) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.write().await;
            self.rows
                .update(|rows| rows.retain(|(session, _), _| session != session_id));
            Ok(())
        })
    }
}

struct SimLink {
    incoming: Owned<mpsc::UnboundedReceiver<String>>,
    outgoing: mpsc::UnboundedSender<String>,
    resumes: String,
}

impl RemoteLink for SimLink {
    fn send(
        &self,
        frames: Vec<RemoteFrame>,
        delivery: RemoteDelivery,
    ) -> HostFuture<'_, Result<(), CodeModeHostError>> {
        self.send_encoded(
            frames.into_iter().map(|frame| (frame, None)).collect(),
            delivery,
        )
    }

    fn send_encoded(
        &self,
        frames: Vec<(RemoteFrame, Option<String>)>,
        _: RemoteDelivery,
    ) -> HostFuture<'_, Result<(), CodeModeHostError>> {
        Box::pin(async move {
            for (frame, text) in frames {
                let text = match text {
                    Some(text) => text,
                    None => frame.encode(FRAME_BYTES_MAX)?,
                };
                self.outgoing
                    .send(text)
                    .map_err(|_| CodeModeHostError::new("socket write failed"))?;
            }
            Ok(())
        })
    }

    fn receive(&self) -> HostFuture<'_, Result<Vec<RemoteFrame>, CodeModeHostError>> {
        Box::pin(async move {
            match self.incoming.acquire().await.recv().await {
                Some(text) => Ok(vec![RemoteFrame::decode(&text, FRAME_BYTES_MAX)?]),
                None => Err(CodeModeHostError::new(PEER_CLOSED)),
            }
        })
    }

    fn close<'a>(&'a self, _: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn max_frame_bytes(&self) -> Option<usize> {
        Some(FRAME_BYTES_MAX)
    }

    fn resumes(&self) -> Option<String> {
        Some(self.resumes.clone())
    }
}

#[derive(Clone, Debug)]
enum Outcome {
    Value(Value),
    NotDispatched,
    Unknown,
}

#[derive(Clone, Debug)]
enum CallState {
    Fresh,
    Admitted,
    Committed(Outcome),
}

struct Call {
    state: CallState,
    exposed: bool,
}

#[derive(Default)]
struct Hand {
    runtime: u64,
    running: BTreeMap<u64, (usize, u64, Value)>,
    retained: BTreeMap<u64, Value>,
    executions: BTreeMap<usize, Vec<Value>>,
}

struct World {
    script: Script,
    store: Arc<Store>,
    hand: RefCell<Hand>,
    calls: RefCell<Vec<Call>>,
    restarts: RefCell<Vec<u64>>,
    drops: RefCell<Vec<u64>>,
    rng: RefCell<Rng>,
    clock: Arc<Clock>,
    trace: RefCell<Vec<String>>,
    wrong: RefCell<Vec<String>>,
    stats: RefCell<HandsStats>,
}

impl World {
    fn note(&self, line: String) {
        self.trace.borrow_mut().push(line);
    }

    fn fail(&self, invariant: &str, line: String) {
        self.wrong.borrow_mut().push(format!("{invariant}: {line}"));
    }

    fn done(&self) -> bool {
        self.calls
            .borrow()
            .iter()
            .all(|call| matches!(call.state, CallState::Committed(_)))
    }

    fn now(&self) -> u64 {
        self.clock.now_ns()
    }

    fn due(events: &RefCell<Vec<u64>>, now: u64) -> bool {
        let mut events = events.borrow_mut();
        if events.first().is_some_and(|at| *at <= now) {
            events.remove(0);
            return true;
        }
        false
    }

    fn restart_due(&self) -> bool {
        if !Self::due(&self.restarts, self.now()) {
            return false;
        }
        let mut hand = self.hand.borrow_mut();
        hand.runtime += 1;
        hand.running.clear();
        hand.retained.clear();
        for call in self.calls.borrow_mut().iter_mut() {
            if !matches!(call.state, CallState::Committed(_)) {
                call.exposed = true;
            }
        }
        self.stats.borrow_mut().restarts += 1;
        self.note(format!("hand restarts as runtime {}", hand.runtime));
        true
    }

    fn reply(id: u64, value: Value) -> String {
        RemoteFrame::Reply {
            id,
            result: Ok(Some(value)),
        }
        .encode(FRAME_BYTES_MAX)
        .expect("a simulated reply fits its frame")
    }

    fn receive(&self, text: &str, out: &mpsc::UnboundedSender<String>) {
        let frame = RemoteFrame::decode(text, FRAME_BYTES_MAX).expect("the brain sends frames");
        let mut hand = self.hand.borrow_mut();
        match frame {
            RemoteFrame::Request {
                id,
                operation: RemoteOperation::Call { context, .. },
                ..
            } => {
                if let Some(value) = hand.retained.get(&id) {
                    let _ = out.send(Self::reply(id, value.clone()));
                    return;
                }
                if hand.running.contains_key(&id) {
                    return;
                }
                let index: usize = context.call_id.parse().expect("calls are numbered");
                let runs = hand.executions.entry(index).or_default();
                let value = json!(format!("call {index} run {}", runs.len()));
                runs.push(value.clone());
                let done = self.now() + self.script.runs[index];
                hand.running.insert(id, (index, done, value));
                self.note(format!("hand runs call {index} as request {id}"));
            }
            RemoteFrame::Ack { ids } => {
                for id in ids {
                    if self.store.unanswered(id) {
                        self.fail(
                            "ack_after_store",
                            format!("request {id} was acked before its reply was stored"),
                        );
                    }
                    hand.retained.remove(&id);
                }
            }
            RemoteFrame::Resume { pending } => {
                hand.retained.retain(|id, _| pending.contains(id));
                for id in pending {
                    if let Some(value) = hand.retained.get(&id) {
                        let _ = out.send(Self::reply(id, value.clone()));
                    } else if !hand.running.contains_key(&id) {
                        let refused = RemoteFrame::Reply {
                            id,
                            result: Err(NOT_RECEIVED.to_owned()),
                        };
                        let _ = out.send(refused.encode(FRAME_BYTES_MAX).expect("fits"));
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(&self, out: &mpsc::UnboundedSender<String>) {
        let now = self.now();
        let mut hand = self.hand.borrow_mut();
        let done: Vec<u64> = hand
            .running
            .iter()
            .filter(|(_, (_, at, _))| *at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            let Some((_, _, value)) = hand.running.remove(&id) else {
                continue;
            };
            hand.retained.insert(id, value.clone());
            let _ = out.send(Self::reply(id, value));
        }
    }

    async fn connection(
        &self,
        out: mpsc::UnboundedSender<String>,
        mut incoming: mpsc::UnboundedReceiver<String>,
    ) {
        loop {
            if self.done() || self.restart_due() {
                return;
            }
            if Self::due(&self.drops, self.now()) {
                self.stats.borrow_mut().drops += 1;
                self.note("socket drops".to_owned());
                return;
            }
            self.finish(&out);
            nanocode_sync::select! {
                text = incoming.recv() => match text {
                    Some(text) => self.receive(&text, &out),
                    None => return,
                },
                () = self.clock.sleep(TICK) => {}
            }
        }
    }

    async fn hand(&self, acceptor: &Arc<Acceptor>) {
        loop {
            let redial = self.rng.borrow_mut().range(1, REDIAL_MS_MAX);
            let until = self.now() + redial * MS;
            while self.now() < until {
                if self.done() {
                    return;
                }
                self.restart_due();
                self.clock.sleep(TICK).await;
            }
            let runtime = format!("runtime-{}", self.hand.borrow().runtime);
            let catalog = json!({"type": "catalog", "definitions": [], "runtime_id": runtime, "resumable": true});
            let catalog = RemoteCatalog::decode(&catalog.to_string(), FRAME_BYTES_MAX)
                .expect("the simulated catalog decodes");
            let (to_brain, from_hand) = mpsc::unbounded_channel();
            let (to_hand, from_brain) = mpsc::unbounded_channel();
            let link = SimLink {
                incoming: Owned::new(from_hand),
                outgoing: to_hand,
                resumes: runtime,
            };
            let brain = async {
                match acceptor.attach(KEY.to_owned(), catalog, link).await {
                    Ok(attached) => attached.ended().await,
                    Err(error) => self.note(format!("attach refused: {error}")),
                }
            };
            nanocode_sync::future::join(brain, self.connection(to_brain, from_brain)).await;
        }
    }

    async fn session_call(&self, route: &AcceptorRoute, index: usize) {
        let context =
            RemoteContext::new(SESSION.to_owned(), index.to_string(), Some(TURN.to_owned()));
        let operation = || RemoteOperation::Call {
            name: "run".to_owned(),
            input: json!({}),
            context: context.clone(),
            selected_tools: None,
        };
        let live = |result: Result<Value, CodeModeHostError>| match result {
            Ok(value) => Outcome::Value(value),
            Err(error) if error.is_unsent() => Outcome::NotDispatched,
            Err(_) => Outcome::Unknown,
        };
        let state = self.calls.borrow()[index].state.clone();
        let outcome = match state {
            CallState::Committed(_) => return,
            CallState::Fresh => {
                let start = self.script.starts[index];
                let now = self.now();
                if start > now {
                    self.clock.sleep(Duration::from_nanos(start - now)).await;
                }
                self.calls.borrow_mut()[index].state = CallState::Admitted;
                live(route.call(operation()).await)
            }
            CallState::Admitted => {
                self.stats.borrow_mut().recovered += 1;
                let recovered = route
                    .recover(&context)
                    .expect("the acceptor's route recovers")
                    .await;
                match recovered {
                    Ok(Some(value)) => Outcome::Value(value),
                    Ok(None) => Outcome::Unknown,
                    Err(error) if error.is_unsent() => live(route.call(operation()).await),
                    Err(_) => Outcome::Unknown,
                }
            }
        };
        self.clock
            .sleep(Duration::from_nanos(self.script.commits[index]))
            .await;
        self.note(format!("call {index} commits {outcome:?}"));
        self.calls.borrow_mut()[index].state = CallState::Committed(outcome);
    }

    fn check(&self) {
        let hand = self.hand.borrow();
        for (index, call) in self.calls.borrow().iter().enumerate() {
            let runs = hand.executions.get(&index).map_or(&[][..], Vec::as_slice);
            if runs.len() > 1 {
                self.fail("once", format!("call {index} ran {} times", runs.len()));
            }
            match &call.state {
                CallState::Committed(Outcome::Value(value)) if runs != [value.clone()] => {
                    self.fail(
                        "real",
                        format!("call {index} committed {value} but ran {runs:?}"),
                    );
                }
                CallState::Committed(Outcome::NotDispatched) if !runs.is_empty() => {
                    self.fail(
                        "real",
                        format!("call {index} committed not dispatched but ran"),
                    );
                }
                CallState::Committed(Outcome::Unknown) if !call.exposed => {
                    self.fail(
                        "honest",
                        format!("call {index} committed unknown though its hand never restarted"),
                    );
                }
                CallState::Committed(Outcome::Unknown) => {
                    self.stats.borrow_mut().unknown += 1;
                }
                CallState::Fresh | CallState::Admitted => {
                    self.fail("settles", format!("call {index} never settled"));
                }
                CallState::Committed(_) => {}
            }
        }
    }
}

fn options() -> AcceptorOptions {
    AcceptorOptions {
        path: "/".to_owned(),
        frame_bytes_max: FRAME_BYTES_MAX,
        head_timeout: GRACE,
        catalog_timeout: GRACE,
        ping: GRACE,
        heartbeat_timeout: GRACE,
        operation_timeout: None,
        reconnect_grace: GRACE,
        runtimes_max: RUNTIMES_MAX,
        attaches_queued_max: ATTACHES_QUEUED_MAX,
    }
}

async fn process(world: &World) {
    let (acceptor, _attaches) = Acceptor::new(
        options(),
        Arc::clone(&world.store) as Arc<dyn HandRequests>,
        |_, _| {
            Err(Refusal {
                status: StatusCode::NOT_FOUND,
                body: String::new(),
            })
        },
    );
    let route = acceptor.route(Arc::new(|| Box::pin(async { Some(KEY.to_owned()) })));
    let calls = (0..world.script.starts.len()).map(|index| world.session_call(&route, index));
    let sessions = nanocode_sync::join_in_order(calls);
    nanocode_sync::future::join(sessions, world.hand(&acceptor)).await;
}

pub struct HandsOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: HandsStats,
    pub shape: String,
}

fn simulate(seed: u64, mutation: HandsMutation) -> HandsOutcome {
    let script = Script::of(seed);
    let shape = script.describe();
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let calls = (0..script.starts.len())
        .map(|_| Call {
            state: CallState::Fresh,
            exposed: false,
        })
        .collect();
    let crashes = script.crashes.clone();
    let world = World {
        restarts: RefCell::new(script.restarts.clone()),
        drops: RefCell::new(script.drops.clone()),
        script,
        store: Arc::new(Store {
            rows: Atom::default(),
            writes: AtomicU64::new(0),
            seed,
            mutation,
        }),
        hand: RefCell::new(Hand::default()),
        calls: RefCell::new(calls),
        rng: RefCell::new(Rng::derive(seed, "hands world")),
        clock: Arc::clone(&clock),
        trace: RefCell::new(Vec::new()),
        wrong: RefCell::new(Vec::new()),
        stats: RefCell::new(HandsStats {
            seeds: 1,
            calls: u64::try_from(Script::of(seed).starts.len()).unwrap_or(u64::MAX),
            ..HandsStats::default()
        }),
    };
    let world = Rc::new(world);
    let mut epoch = 0_u64;
    loop {
        let crash = crashes
            .get(usize::try_from(epoch).unwrap_or(usize::MAX))
            .copied();
        let horizon = crash.unwrap_or_else(|| world.now() + SETTLE_HORIZON_MS * MS);
        let tasks: Vec<Task<'_>> = vec![Box::pin(process(&world))];
        let mut executor = Executor::new(
            Arc::clone(&clock),
            Rng::derive(seed ^ epoch, "hands scheduler"),
        );
        let ended = executor.run(tasks, Some(horizon));
        drop(executor);
        drop(crate::host::spawned());
        match ended {
            Ended::Crashed if crash.is_some() => {
                world.stats.borrow_mut().crashes += 1;
                world.note(format!("host crashes in epoch {epoch}"));
                epoch += 1;
            }
            Ended::Crashed => {
                world.note("the run reached its horizon".to_owned());
                break;
            }
            Ended::Stuck => {
                world.fail("settles", "every task waits and no timer is set".to_owned());
                break;
            }
            Ended::Finished => break,
        }
    }
    world.check();
    let trace = world.trace.borrow();
    let mut digest = fnv(FNV_OFFSET, shape.as_bytes());
    for line in trace.iter() {
        digest = fnv(digest, line.as_bytes());
    }
    digest = fnv(digest, &clock.now_ns().to_le_bytes());
    let wrong = world.wrong.borrow().first().cloned();
    let stats = std::mem::take(&mut *world.stats.borrow_mut());
    HandsOutcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: HandsMutation) -> HandsOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        HandsOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: HandsStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge_seed(
    seed: u64,
    mutation: HandsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut HandsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: HandsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario hands --seed {seed}");
    if mutation != HandsMutation::None {
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
    mutation: HandsMutation,
    totals: &HandsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=hands mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        HandsMutation::None if failed == 0 => ExitCode::SUCCESS,
        HandsMutation::None => ExitCode::FAILURE,
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

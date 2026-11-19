mod agents;
mod requestclose;
mod inboxstress;
#[cfg(unix)]
mod alloc;
#[cfg(unix)]
mod capture;
mod cellcalls;
mod cells;
mod children;
mod disk;
#[cfg(target_os = "linux")]
mod dns;
#[cfg(unix)]
mod egress;
mod drive;
mod executor;
mod faults;
mod hands;
mod host;
mod httpd;
mod httpserver;
mod log;
mod longthread;
mod model;
mod mpp;
mod onelog;
mod rng;
mod scripted;
mod simulate;
mod socket;
mod store;
mod streams;
mod sync;
mod tasks;
mod board;
mod tidy;
mod turnlog;
mod turns;
mod whole;
mod workload;
mod world;
mod network;
mod server;
mod websocket;

use std::{collections::BTreeMap, ops::Range, process::ExitCode};

use crate::{
    agents::{AgentStats, AgentsMutation},
    model::Stats,
    onelog::{LiveSeqs, Protections},
    simulate::{Options, run_seed},
    store::Mutation,
    turns::{Invariant, PolicyKind, TurnMutation, TurnOptions, TurnStats},
    httpd::WireMutation,
    httpserver::{HttpServerMutation, HttpServerStats},
    mpp::{MppMutation, MppStats},
    streams::{StreamsMutation, StreamsStats},
    hands::{HandsMutation, HandsStats},
    cellcalls::{CellCallsMutation, CellCallsStats},
    longthread::{LongThreadMutation, LongThreadStats},
    requestclose::RequestCloseStats,
    inboxstress::InboxStressStats,
    sync::{SyncMutation, SyncStats},
    tasks::{TasksMutation, TasksStats},
    board::{BoardMutation, BoardStats},
    websocket::{WebSocketMutation, WebSocketStats},
    whole::{ChildMode, StoreKind, WholeOptions, WholeStats, WireKind},
    children::ChildMutation,
};

const DEFAULT_OPS: u64 = 1000;
const CAUGHT_SHOWN_MAX: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Store,
    Agents,
    Turns,
    Runtime,
    Websocket,
    HttpServer,
    Alloc,
    Egress,
    Mpp,
    Sync,
    Streams,
    Hands,
    CellCalls,
    LongThread,
    RequestClose,
    InboxStress,
    Capture,
    Executor,
    Dns,
    Board,
}

struct Args {
    seeds: Range<u64>,
    options: Options,
    twice: bool,
    scenario: Scenario,
    turns: TurnOptions,
    whole: WholeOptions,
    agents: AgentsMutation,
    websocket: WebSocketMutation,
    http_server: HttpServerMutation,
    mpp: MppMutation,
    sync: SyncMutation,
    streams: StreamsMutation,
    hands: HandsMutation,
    fleet: usize,
    reopen: bool,
    cancel: bool,
    cell_calls: CellCallsMutation,
    long_thread: LongThreadMutation,
    tasks: TasksMutation,
    board: BoardMutation,
    #[cfg(unix)]
    alloc: alloc::AllocMutation,
    #[cfg(unix)]
    egress: egress::EgressMutation,
    #[cfg(unix)]
    capture: capture::CaptureMutation,
    #[cfg(target_os = "linux")]
    dns: dns::DnsMutation,
}

type Caught = BTreeMap<Invariant, (u64, u64, (u64, u64))>;

fn usage() -> ExitCode {
    eprintln!(
        "usage: nanocode-sim --tidy | --mutations | (--seed S | --seeds A..B) [--scenario store|agents|turns|runtime|websocket|http_server|alloc|egress|mpp|sync|streams|hands|cell_calls|long_thread|request_close|inbox_stress|capture|executor|dns|board] [--ops N] [--twice] [--mutation {}] [--records content|slots] [--verbose]\n       turns: [--mutation {}] [--without {}]... [--live-seqs temporary|floor|gap|reuse] [--calm] [--policy reference|turnlog] [--row-limit BYTES] [--reopen-row-limit BYTES] [--long TURNS] [--snapshot-ratio K] [--snapshot-rows N] [--takeover] [--retain TURNS] [--state-bytes N] [--event-bytes N] [--owners N]\n       runtime: [--store memory|kv|log] [--deferred-start] [--calm] [--wire transport|http] [--children local|remote] [--mutation {}|{}]\n       agents: [--mutation {}]
       websocket: [--mutation {}]
       http_server: [--mutation {}]
       alloc: [--mutation overstate_size|forget_free|scribble]
       egress: [--mutation widen_allowlist|corrupt_tunnel_byte|rebind_on_connect|trust_private]
       mpp: [--mutation {}]
       sync: [--mutation {}]
       streams: [--mutation {}]
       hands: [--mutation {}] [--fleet N] [--reopen] [--cancel]
       cell_calls: [--mutation {}]
       long_thread: [--mutation {}]
       capture: [--mutation skip_trim|drop_chunk|skip_redact|redact_then_trim]
       executor: [--mutation {}]
       dns: [--mutation cut_answer|drop_search]
       board: [--mutation {}]",
        Mutation::names(),
        TurnMutation::names(),
        onelog::PROTECTIONS.join("|"),
        WireMutation::NAMES,
        ChildMutation::NAMES,
        AgentsMutation::names(),
        WebSocketMutation::names(),
        HttpServerMutation::names(),
        MppMutation::names(),
        SyncMutation::names(),
        StreamsMutation::names(),
        HandsMutation::names(),
        CellCallsMutation::names(),
        LongThreadMutation::names(),
        TasksMutation::names(),
        BoardMutation::names()
    );
    ExitCode::from(2)
}

fn parse_seeds(value: &str) -> Option<Range<u64>> {
    if let Some((low, high)) = value.split_once("..") {
        return Some(low.parse().ok()?..high.parse().ok()?);
    }
    let seed: u64 = value.parse().ok()?;
    Some(seed..seed.checked_add(1)?)
}

fn parse_args(args: &[String]) -> Option<Args> {
    let mut seeds = None;
    let mut options = Options {
        ops: DEFAULT_OPS,
        mutation: Mutation::None,
        slots: false,
        verbose: false,
    };
    let mut twice = false;
    let mut scenario = Scenario::Store;
    let mut mutation = None;
    let mut turns = TurnOptions {
        calm: false,
        event_bytes: None,
        long: None,
        owners: None,
        retain: None,
        snapshot_ratio: 2,
        snapshot_rows: None,
        takeover: false,
        state_bytes: None,
        mutation: TurnMutation::None,
        policy: PolicyKind::Reference,
        row_limit: None,
        reopen_row_limit: None,
        protections: Protections::ALL,
        verbose: false,
    };
    let mut whole = WholeOptions::default();
    let mut agents = AgentsMutation::None;
    let mut websocket = WebSocketMutation::None;
    let mut http_server = HttpServerMutation::None;
    let mut mpp = MppMutation::None;
    let mut sync = SyncMutation::None;
    let mut streams = StreamsMutation::None;
    let mut hands = HandsMutation::None;
    let mut fleet = 1_usize;
    let mut reopen = false;
    let mut cancel = false;
    let mut cell_calls = CellCallsMutation::None;
    let mut long_thread = LongThreadMutation::None;
    let mut tasks = TasksMutation::None;
    let mut board = BoardMutation::None;
    #[cfg(unix)]
    let mut capture = capture::CaptureMutation::None;
    #[cfg(target_os = "linux")]
    let mut dns = dns::DnsMutation::None;
    #[cfg(unix)]
    let mut alloc = alloc::AllocMutation::None;
    #[cfg(unix)]
    let mut egress = egress::EgressMutation::None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--twice" => twice = true,
            "--verbose" => options.verbose = true,
            "--calm" => turns.calm = true,
            "--store" => whole.store = Some(StoreKind::parse(rest.next()?)?),
            "--deferred-start" => whole.deferred_start = true,
            "--wire" => whole.wire = WireKind::parse(rest.next()?)?,
            "--children" => whole.children = ChildMode::parse(rest.next()?)?,
            "--policy" => turns.policy = PolicyKind::parse(rest.next()?)?,
            "--row-limit" => turns.row_limit = Some(rest.next()?.parse().ok()?),
            "--reopen-row-limit" => turns.reopen_row_limit = Some(rest.next()?.parse().ok()?),
            "--long" => turns.long = Some(rest.next()?.parse().ok()?),
            "--snapshot-ratio" => turns.snapshot_ratio = rest.next()?.parse().ok()?,
            "--snapshot-rows" => turns.snapshot_rows = Some(rest.next()?.parse().ok()?),
            "--takeover" => turns.takeover = true,
            "--retain" => turns.retain = Some(rest.next()?.parse().ok()?),
            "--state-bytes" => turns.state_bytes = Some(rest.next()?.parse().ok()?),
            "--event-bytes" => turns.event_bytes = Some(rest.next()?.parse().ok()?),
            "--owners" => turns.owners = Some(rest.next()?.parse().ok()?),
            "--reopen" => reopen = true,
            "--cancel" => cancel = true,
            "--fleet" => {
                fleet = rest
                    .next()?
                    .parse()
                    .ok()
                    .filter(|count: &usize| *count > 0)?
            }
            "--without" => turns.protections = turns.protections.without(rest.next()?)?,
            "--live-seqs" => turns.protections.live_seqs = LiveSeqs::parse(rest.next()?)?,
            "--seed" | "--seeds" => seeds = Some(parse_seeds(rest.next()?)?),
            "--ops" => options.ops = rest.next()?.parse().ok()?,
            "--scenario" => {
                scenario = match rest.next()?.as_str() {
                    "store" => Scenario::Store,
                    "agents" => Scenario::Agents,
                    "turns" => Scenario::Turns,
                    "runtime" => Scenario::Runtime,
                    "websocket" => Scenario::Websocket,
                    "http_server" => Scenario::HttpServer,
                    "mpp" => Scenario::Mpp,
                    "sync" => Scenario::Sync,
                    "streams" => Scenario::Streams,
                    "hands" => Scenario::Hands,
                    "cell_calls" => Scenario::CellCalls,
                    "long_thread" => Scenario::LongThread,
                    "request_close" => Scenario::RequestClose,
                    "inbox_stress" => Scenario::InboxStress,
                    "executor" => Scenario::Executor,
                    "board" => Scenario::Board,
                    #[cfg(unix)]
                    "capture" => Scenario::Capture,
                    #[cfg(target_os = "linux")]
                    "dns" => Scenario::Dns,
                    #[cfg(unix)]
                    "alloc" => Scenario::Alloc,
                    #[cfg(unix)]
                    "egress" => Scenario::Egress,
                    _ => return None,
                };
            }
            "--mutation" => mutation = Some(rest.next()?.as_str()),
            "--records" => {
                options.slots = match rest.next()?.as_str() {
                    "content" => false,
                    "slots" => true,
                    _ => return None,
                };
            }
            _ => return None,
        }
    }
    match (scenario, mutation) {
        (_, None) => {}
        (Scenario::Store, Some(name)) => options.mutation = Mutation::parse(name)?,
        (Scenario::Turns, Some(name)) => turns.mutation = TurnMutation::parse(name)?,
        (Scenario::Runtime, Some(name)) => match WireMutation::parse(name) {
            Some(mutation) => whole.mutation = mutation,
            None => match ChildMutation::parse(name) {
                Some(mutation) => whole.child_mutation = mutation,
                None => whole.store_mutation = Mutation::parse(name)?,
            },
        },
        (Scenario::Websocket, Some(name)) => websocket = WebSocketMutation::parse(name)?,
        (Scenario::HttpServer, Some(name)) => http_server = HttpServerMutation::parse(name)?,
        (Scenario::Mpp, Some(name)) => mpp = MppMutation::parse(name)?,
        (Scenario::Sync, Some(name)) => sync = SyncMutation::parse(name)?,
        (Scenario::Streams, Some(name)) => streams = StreamsMutation::parse(name)?,
        (Scenario::Hands, Some(name)) => hands = HandsMutation::parse(name)?,
        (Scenario::CellCalls, Some(name)) => cell_calls = CellCallsMutation::parse(name)?,
        (Scenario::LongThread, Some(name)) => long_thread = LongThreadMutation::parse(name)?,
        (Scenario::Executor, Some(name)) => tasks = TasksMutation::parse(name)?,
        (Scenario::RequestClose, Some(_)) => return None,
        (Scenario::InboxStress, Some(_)) => return None,
        (Scenario::Board, Some(name)) => board = BoardMutation::parse(name)?,
        #[cfg(unix)]
        (Scenario::Capture, Some(name)) => capture = capture::CaptureMutation::parse(name)?,
        #[cfg(not(unix))]
        (Scenario::Capture, Some(_)) => return None,
        #[cfg(target_os = "linux")]
        (Scenario::Dns, Some(name)) => dns = dns::DnsMutation::parse(name)?,
        #[cfg(not(target_os = "linux"))]
        (Scenario::Dns, Some(_)) => return None,
        #[cfg(unix)]
        (Scenario::Alloc, Some(name)) => alloc = alloc::AllocMutation::parse(name)?,
        #[cfg(unix)]
        (Scenario::Egress, Some(name)) => egress = egress::EgressMutation::parse(name)?,
        #[cfg(not(unix))]
        (Scenario::Alloc, Some(_)) => return None,
        #[cfg(not(unix))]
        (Scenario::Egress, Some(_)) => return None,
        (Scenario::Agents, Some(name)) => agents = AgentsMutation::parse(name)?,
    }
    if whole.store_mutation != Mutation::None && whole.store != Some(StoreKind::Kv) {
        return None;
    }
    turns.verbose = options.verbose;
    whole.verbose = options.verbose;
    whole.calm = turns.calm;
    Some(Args {
        seeds: seeds?,
        options,
        twice,
        scenario,
        turns,
        whole,
        agents,
        websocket,
        http_server,
        mpp,
        sync,
        streams,
        hands,
        fleet,
        reopen,
        cancel,
        cell_calls,
        long_thread,
        tasks,
        board,
        #[cfg(unix)]
        alloc,
        #[cfg(unix)]
        egress,
        #[cfg(unix)]
        capture,
        #[cfg(target_os = "linux")]
        dns,
    })
}

fn replay(seed: u64, args: &Args) -> String {
    let options = args.options;
    if args.scenario == Scenario::Websocket {
        return websocket::replay(seed, args.websocket);
    }
    if args.scenario == Scenario::HttpServer {
        return httpserver::replay(seed, args.http_server);
    }
    if args.scenario == Scenario::Mpp {
        return mpp::replay(seed, args.mpp);
    }
    if args.scenario == Scenario::Sync {
        return sync::replay(seed, args.sync);
    }
    if args.scenario == Scenario::Streams {
        return streams::replay(seed, args.streams);
    }
    if args.scenario == Scenario::Hands {
        return hands::replay(
            seed,
            args.hands,
            hands::HandsOptions {
                fleet: args.fleet,
                reopen: args.reopen,
                cancel: args.cancel,
            },
        );
    }
    if args.scenario == Scenario::CellCalls {
        return cellcalls::replay(seed, args.cell_calls);
    }
    if args.scenario == Scenario::LongThread {
        return longthread::replay(seed, args.long_thread);
    }
    if args.scenario == Scenario::RequestClose {
        return requestclose::replay(seed);
    }
    if args.scenario == Scenario::InboxStress {
        return inboxstress::replay(seed);
    }
    if args.scenario == Scenario::Agents {
        let mut command = format!("nanocode-sim --scenario agents --seed {seed}");
        if args.agents != AgentsMutation::None {
            command.push_str(" --mutation ");
            command.push_str(args.agents.name());
        }
        return command;
    }
    if args.scenario == Scenario::Executor {
        return tasks::replay(seed, args.tasks);
    }
    if args.scenario == Scenario::Board {
        return board::replay(seed, args.board);
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Capture {
        return capture::replay(seed, args.capture);
    }
    #[cfg(target_os = "linux")]
    if args.scenario == Scenario::Dns {
        return dns::replay(seed, args.dns);
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Alloc {
        return alloc::replay(seed, options.ops, args.alloc);
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Egress {
        return egress::replay(seed, args.egress);
    }
    if args.scenario == Scenario::Runtime {
        let mut command = format!("nanocode-sim --scenario runtime --seed {seed}");
        if let Some(store) = args.whole.store {
            command.push_str(" --store ");
            command.push_str(store.name());
        }
        if args.whole.deferred_start {
            command.push_str(" --deferred-start");
        }
        if args.whole.calm {
            command.push_str(" --calm");
        }
        if args.whole.wire != WireKind::Transport {
            command.push_str(" --wire ");
            command.push_str(args.whole.wire.name());
        }
        if args.whole.children != ChildMode::Local {
            command.push_str(" --children ");
            command.push_str(args.whole.children.name());
        }
        if args.whole.mutation != WireMutation::None {
            command.push_str(" --mutation ");
            command.push_str(args.whole.mutation.name());
        }
        if args.whole.child_mutation != ChildMutation::None {
            command.push_str(" --mutation ");
            command.push_str(args.whole.child_mutation.name());
        }
        if args.whole.store_mutation != Mutation::None {
            command.push_str(" --mutation ");
            command.push_str(args.whole.store_mutation.name());
        }
        return command;
    }
    if args.scenario == Scenario::Turns {
        let mut command = format!("nanocode-sim --scenario turns --seed {seed}");
        if args.turns.calm {
            command.push_str(" --calm");
        }
        if args.turns.policy != PolicyKind::Reference {
            command.push_str(" --policy ");
            command.push_str(args.turns.policy.name());
        }
        if let Some(limit) = args.turns.row_limit {
            command.push_str(&format!(" --row-limit {limit}"));
        }
        if let Some(limit) = args.turns.reopen_row_limit {
            command.push_str(&format!(" --reopen-row-limit {limit}"));
        }
        if let Some(turns) = args.turns.long {
            command.push_str(&format!(" --long {turns}"));
        }
        if args.turns.snapshot_ratio != 2 {
            command.push_str(&format!(" --snapshot-ratio {}", args.turns.snapshot_ratio));
        }
        if let Some(rows) = args.turns.snapshot_rows {
            command.push_str(&format!(" --snapshot-rows {rows}"));
        }
        if args.turns.takeover {
            command.push_str(" --takeover");
        }
        if let Some(keep) = args.turns.retain {
            command.push_str(&format!(" --retain {keep}"));
        }
        if let Some(bytes) = args.turns.state_bytes {
            command.push_str(&format!(" --state-bytes {bytes}"));
        }
        if let Some(bytes) = args.turns.event_bytes {
            command.push_str(&format!(" --event-bytes {bytes}"));
        }
        if let Some(owners) = args.turns.owners {
            command.push_str(&format!(" --owners {owners}"));
        }
        for name in args.turns.protections.off() {
            command.push_str(" --without ");
            command.push_str(name);
        }
        if args.turns.protections.live_seqs != LiveSeqs::Temporary {
            command.push_str(" --live-seqs ");
            command.push_str(args.turns.protections.live_seqs.name());
        }
        if args.turns.mutation != TurnMutation::None {
            command.push_str(" --mutation ");
            command.push_str(args.turns.mutation.name());
        }
        return command;
    }
    let mut command = format!("nanocode-sim --seed {seed} --ops {}", options.ops);
    if options.mutation != Mutation::None {
        command.push_str(" --mutation ");
        command.push_str(options.mutation.name());
    }
    if options.slots {
        command.push_str(" --records slots");
    }
    command
}

fn judge(seed: u64, args: &Args, totals: &mut Stats) -> Option<String> {
    let first = run_seed(seed, args.options);
    totals.add(&first.stats);
    if args.options.verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !args.twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let quiet = Options {
        verbose: false,
        ..args.options
    };
    let second = crate::host::second_pass(|| run_seed(seed, quiet));
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

fn judge_agents(seed: u64, args: &Args, totals: &mut AgentStats) -> Option<String> {
    let first = agents::run_seed(seed, args.agents);
    totals.add(&first.stats);
    if args.options.verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !args.twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = crate::host::second_pass(|| agents::run_seed(seed, args.agents));
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

fn judge_whole(seed: u64, args: &Args, totals: &mut WholeStats) -> Option<String> {
    let first = whole::run_seed(seed, args.whole);
    totals.add(&first.stats);
    if args.options.verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !args.twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let quiet = WholeOptions {
        verbose: false,
        ..args.whole
    };
    let second = crate::host::second_pass(|| whole::run_seed(seed, quiet));
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}, {}",
            first.shape,
            first.digest,
            second.digest,
            whole::divergence(&first.lines, &second.lines)
        )
    })
}

fn judge_turns(
    seed: u64,
    args: &Args,
    totals: &mut TurnStats,
    caught: &mut Caught,
) -> Option<String> {
    let first = turns::run_seed(seed, args.turns);
    totals.add(&first.stats);
    if args.options.verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if let Some((invariant, message)) = &first.wrong {
        let entry = caught
            .entry(*invariant)
            .or_insert((0, seed, (first.lines, seed)));
        entry.0 += 1;
        if first.lines < entry.2.0 {
            entry.2 = (first.lines, seed);
        }
        return Some(format!(
            "{} :: {}: {message}",
            first.shape,
            invariant.name()
        ));
    }
    if !args.twice {
        return None;
    }
    let quiet = TurnOptions {
        verbose: false,
        ..args.turns
    };
    let second = crate::host::second_pass(|| turns::run_seed(seed, quiet));
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

fn report_turns(
    args: &Args,
    passed: u64,
    failed: u64,
    totals: &TurnStats,
    caught: &Caught,
) -> ExitCode {
    for (invariant, (count, first, (lines, shortest))) in caught {
        println!(
            "INVARIANT {} failed={count} first_seed={first} shortest_seed={shortest} trace_lines={lines}",
            invariant.name()
        );
    }
    println!(
        "RESULT seeds={}..{} scenario=turns policy={} calm={} protections_off={:?} live_seqs={} mutation={} passed={passed} failed={failed} twice={} {} stats={totals:?}",
        args.seeds.start,
        args.seeds.end,
        args.turns.policy.name(),
        args.turns.calm,
        args.turns.protections.off(),
        args.turns.protections.live_seqs.name(),
        args.turns.mutation.name(),
        args.twice,
        totals.summary()
    );
    let mutation = args.turns.mutation;
    if mutation != TurnMutation::None {
        if failed > 0 {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            return ExitCode::SUCCESS;
        }
        println!("MUTATION {} NOT caught", mutation.name());
        return ExitCode::FAILURE;
    }
    if args.turns.protections != Protections::ALL || failed == 0 {
        return ExitCode::SUCCESS;
    }
    ExitCode::FAILURE
}

#[cfg(unix)]
fn alloc_experiment(args: &Args) -> bool {
    (args.scenario == Scenario::Alloc && args.alloc != alloc::AllocMutation::None)
        || (args.scenario == Scenario::Egress && args.egress != egress::EgressMutation::None)
        || (args.scenario == Scenario::Capture && args.capture != capture::CaptureMutation::None)
}

#[cfg(not(unix))]
const fn alloc_experiment(_args: &Args) -> bool {
    false
}

const fn mutation_name(whole: &WholeOptions) -> &'static str {
    match (whole.child_mutation, whole.store_mutation) {
        (ChildMutation::None, Mutation::None) => whole.mutation.name(),
        (ChildMutation::None, mutation) => mutation.name(),
        (mutation, _) => mutation.name(),
    }
}

fn mutations() -> ExitCode {
    let mut lines = Vec::new();
    let mut add = |scenario: &str, names: String| {
        for name in names.split('|').filter(|name| !name.is_empty() && *name != "none") {
            lines.push(format!("MUTATION scenario={scenario} mutation={name}"));
        }
    };
    add("store", Mutation::store_names());
    add("turns", TurnMutation::names());
    add("runtime", WireMutation::NAMES.to_owned());
    add("runtime", ChildMutation::NAMES.to_owned());
    add("runtime", Mutation::runtime_names());
    add("agents", AgentsMutation::names());
    add("websocket", WebSocketMutation::names());
    add("http_server", HttpServerMutation::names());
    add("mpp", MppMutation::names());
    add("sync", SyncMutation::names());
    add("streams", StreamsMutation::names());
    add("hands", HandsMutation::names());
    add("cell_calls", CellCallsMutation::names());
    add("long_thread", LongThreadMutation::names());
    add("executor", TasksMutation::names());
    add("board", BoardMutation::names());
    #[cfg(unix)]
    {
        add("alloc", alloc::AllocMutation::names());
        add("egress", egress::EgressMutation::names());
        add("capture", capture::CaptureMutation::names());
    }
    #[cfg(target_os = "linux")]
    add("dns", dns::DnsMutation::names());
    for line in lines {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

fn tidy() -> ExitCode {
    match tidy::check(&tidy::workspace_root()) {
        Ok(problems) if problems.is_empty() => {
            println!("TIDY determinism: clean");
            ExitCode::SUCCESS
        }
        Ok(problems) => {
            for problem in problems {
                println!("TIDY {problem}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("TIDY could not read the sources: {error}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--tidy"] {
        return tidy();
    }
    if args == ["--mutations"] {
        return mutations();
    }
    log::install();
    if let Err(error) = host::install() {
        eprintln!("could not install the simulated platform host: {error}");
        return ExitCode::FAILURE;
    }
    let Some(args) = parse_args(&args) else {
        return usage();
    };
    let mutation = args.options.mutation;
    let experiment = (args.scenario == Scenario::Turns
        && (args.turns.mutation != TurnMutation::None
            || args.turns.protections != Protections::ALL))
        || (args.scenario == Scenario::Runtime
            && (args.whole.mutation != WireMutation::None
                || args.whole.child_mutation != ChildMutation::None
                || args.whole.store_mutation != Mutation::None))
        || (args.scenario == Scenario::Agents && args.agents != AgentsMutation::None)
        || (args.scenario == Scenario::Websocket && args.websocket != WebSocketMutation::None)
        || (args.scenario == Scenario::HttpServer && args.http_server != HttpServerMutation::None)
        || (args.scenario == Scenario::Mpp && args.mpp != MppMutation::None)
        || (args.scenario == Scenario::Sync && args.sync != SyncMutation::None)
        || (args.scenario == Scenario::Streams && args.streams != StreamsMutation::None)
        || (args.scenario == Scenario::Hands && args.hands != HandsMutation::None)
        || (args.scenario == Scenario::CellCalls && args.cell_calls != CellCallsMutation::None)
        || (args.scenario == Scenario::LongThread && args.long_thread != LongThreadMutation::None)
        || (args.scenario == Scenario::Executor && args.tasks != TasksMutation::None)
        || (args.scenario == Scenario::Board && args.board != BoardMutation::None)
        || alloc_experiment(&args);
    let (mut passed, mut failed) = (0_u64, 0_u64);
    let mut totals = Stats::default();
    let mut agent_totals = AgentStats::default();
    let mut turn_totals = TurnStats::default();
    let mut whole_totals = WholeStats::default();
    let mut websocket_totals = WebSocketStats::default();
    let mut http_server_totals = HttpServerStats::default();
    let mut mpp_totals = MppStats::default();
    let mut sync_totals = SyncStats::default();
    let mut streams_totals = StreamsStats::default();
    let mut hands_totals = HandsStats::default();
    let mut cell_calls_totals = CellCallsStats::default();
    let mut long_thread_totals = LongThreadStats::default();
    let mut request_close_totals = RequestCloseStats::default();
    let mut inbox_stress_totals = InboxStressStats::default();
    let mut tasks_totals = TasksStats::default();
    let mut board_totals = BoardStats::default();
    #[cfg(unix)]
    let mut capture_totals = capture::CaptureStats::default();
    #[cfg(target_os = "linux")]
    let mut dns_totals = dns::DnsStats::default();
    #[cfg(unix)]
    let mut alloc_totals = alloc::AllocStats::default();
    #[cfg(unix)]
    let mut egress_totals = egress::EgressStats::default();
    let mut caught = Caught::new();
    for seed in args.seeds.clone() {
        let verdict = match args.scenario {
            Scenario::Store => judge(seed, &args, &mut totals),
            Scenario::Agents => judge_agents(seed, &args, &mut agent_totals),
            Scenario::Turns => judge_turns(seed, &args, &mut turn_totals, &mut caught),
            Scenario::Runtime => judge_whole(seed, &args, &mut whole_totals),
            Scenario::Websocket => websocket::judge(
                seed,
                args.websocket,
                args.twice,
                args.options.verbose,
                &mut websocket_totals,
            ),
            Scenario::HttpServer => httpserver::judge(
                seed,
                args.http_server,
                args.twice,
                args.options.verbose,
                &mut http_server_totals,
            ),
            Scenario::Mpp => mpp::judge_seed(
                seed,
                args.mpp,
                args.twice,
                args.options.verbose,
                &mut mpp_totals,
            ),
            Scenario::Sync => sync::judge_seed(
                seed,
                args.sync,
                args.twice,
                args.options.verbose,
                &mut sync_totals,
            ),
            Scenario::CellCalls => cellcalls::judge(
                seed,
                args.cell_calls,
                args.twice,
                args.options.verbose,
                &mut cell_calls_totals,
            ),
            Scenario::LongThread => longthread::judge(
                seed,
                args.long_thread,
                args.twice,
                args.options.verbose,
                &mut long_thread_totals,
            ),
            Scenario::RequestClose => requestclose::judge(
                seed,
                args.twice,
                args.options.verbose,
                &mut request_close_totals,
            ),
            Scenario::InboxStress => inboxstress::judge(
                seed,
                args.twice,
                args.options.verbose,
                &mut inbox_stress_totals,
            ),
            Scenario::Hands => hands::judge_seed(
                seed,
                args.hands,
                hands::HandsOptions {
                    fleet: args.fleet,
                    reopen: args.reopen,
                    cancel: args.cancel,
                },
                args.twice,
                args.options.verbose,
                &mut hands_totals,
            ),
            Scenario::Streams => streams::judge(
                seed,
                args.streams,
                args.twice,
                args.options.verbose,
                &mut streams_totals,
            ),
            Scenario::Executor => tasks::judge(
                seed,
                args.tasks,
                args.twice,
                args.options.verbose,
                &mut tasks_totals,
            ),
            Scenario::Board => board::judge(
                seed,
                args.board,
                args.twice,
                args.options.verbose,
                &mut board_totals,
            ),
            #[cfg(unix)]
            Scenario::Capture => capture::judge(
                seed,
                args.capture,
                args.twice,
                args.options.verbose,
                &mut capture_totals,
            ),
            #[cfg(not(unix))]
            Scenario::Capture => unreachable!("the capture scenario parses only on unix"),
            #[cfg(target_os = "linux")]
            Scenario::Dns => dns::judge(
                seed,
                args.dns,
                args.twice,
                args.options.verbose,
                &mut dns_totals,
            ),
            #[cfg(not(target_os = "linux"))]
            Scenario::Dns => unreachable!("the dns scenario parses only on linux"),
            #[cfg(unix)]
            Scenario::Alloc => alloc::judge(
                seed,
                args.options.ops,
                args.alloc,
                args.twice,
                args.options.verbose,
                &mut alloc_totals,
            ),
            #[cfg(not(unix))]
            Scenario::Alloc => unreachable!("the alloc scenario parses only on unix"),
            #[cfg(unix)]
            Scenario::Egress => egress::judge_seed(
                seed,
                args.egress,
                args.twice,
                args.options.verbose,
                &mut egress_totals,
            ),
            #[cfg(not(unix))]
            Scenario::Egress => unreachable!("the egress scenario parses only on unix"),
        };
        let Some(message) = verdict else {
            passed += 1;
            continue;
        };
        failed += 1;
        let mut replay = replay(seed, &args);
        if args.twice && !replay.contains("--twice") {
            replay.push_str(" --twice");
        }
        if mutation == Mutation::None && !experiment {
            println!("FAIL seed={seed} Wrong {message} :: replay: {replay}");
        } else if failed <= u64::try_from(CAUGHT_SHOWN_MAX).expect("a small count fits in u64") {
            println!("CAUGHT seed={seed} {message} :: replay: {replay}");
        }
    }
    if args.scenario == Scenario::Websocket {
        return websocket::report(
            &args.seeds,
            passed,
            failed,
            args.twice,
            args.websocket,
            &websocket_totals,
        );
    }
    if args.scenario == Scenario::Streams {
        return streams::report(
            &args.seeds,
            passed,
            failed,
            args.twice,
            args.streams,
            &streams_totals,
        );
    }
    if args.scenario == Scenario::Hands {
        return hands::report(
            &args.seeds,
            passed,
            failed,
            args.twice,
            args.hands,
            hands::HandsOptions {
                fleet: args.fleet,
                reopen: args.reopen,
                cancel: args.cancel,
            },
            &hands_totals,
        );
    }
    if args.scenario == Scenario::CellCalls {
        return cellcalls::report(&args.seeds, passed, failed, args.twice, args.cell_calls, &cell_calls_totals);
    }
    if args.scenario == Scenario::LongThread {
        return longthread::report(&args.seeds, passed, failed, args.twice, args.long_thread, &long_thread_totals);
    }
    if args.scenario == Scenario::RequestClose {
        return requestclose::report(&args.seeds, passed, failed, args.twice, &request_close_totals);
    }
    if args.scenario == Scenario::InboxStress {
        return inboxstress::report(&args.seeds, passed, failed, args.twice, &inbox_stress_totals);
    }
    if args.scenario == Scenario::Board {
        return board::report(&args.seeds, passed, failed, args.twice, args.board, &board_totals);
    }
    if args.scenario == Scenario::Executor {
        return tasks::report(&args.seeds, passed, failed, args.twice, args.tasks, &tasks_totals);
    }
    #[cfg(target_os = "linux")]
    if args.scenario == Scenario::Dns {
        return dns::report(&args.seeds, passed, failed, args.twice, args.dns, &dns_totals);
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Capture {
        return capture::report(&args.seeds, passed, failed, args.twice, args.capture, &capture_totals);
    }
    if args.scenario == Scenario::Sync {
        return sync::report(&args.seeds, passed, failed, args.twice, args.sync, &sync_totals);
    }
    if args.scenario == Scenario::Mpp {
        return mpp::report(&args.seeds, passed, failed, args.twice, args.mpp, &mpp_totals);
    }
    if args.scenario == Scenario::HttpServer {
        return httpserver::report(
            &args.seeds,
            passed,
            failed,
            args.twice,
            args.http_server,
            &http_server_totals,
        );
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Alloc {
        return alloc::report(&args.seeds, passed, failed, args.twice, args.alloc, &alloc_totals);
    }
    #[cfg(unix)]
    if args.scenario == Scenario::Egress {
        return egress::report(&args.seeds, passed, failed, args.twice, args.egress, &egress_totals);
    }
    if args.scenario == Scenario::Turns {
        return report_turns(&args, passed, failed, &turn_totals, &caught);
    }
    if args.scenario == Scenario::Runtime {
        println!(
            "RESULT seeds={}..{} scenario=runtime wire={} children={} mutation={} passed={passed} failed={failed} twice={} stats={whole_totals:?}",
            args.seeds.start,
            args.seeds.end,
            args.whole.wire.name(),
            args.whole.children.name(),
            mutation_name(&args.whole),
            args.twice
        );
        if args.whole.mutation != WireMutation::None
            || args.whole.child_mutation != ChildMutation::None
            || args.whole.store_mutation != Mutation::None
        {
            if failed > 0 {
                println!("MUTATION {} caught by {failed} seeds", mutation_name(&args.whole));
                return ExitCode::SUCCESS;
            }
            println!("MUTATION {} NOT caught", mutation_name(&args.whole));
            return ExitCode::FAILURE;
        }
        return if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if args.scenario == Scenario::Agents {
        println!(
            "RESULT seeds={}..{} scenario=agents mutation={} passed={passed} failed={failed} twice={} stats={agent_totals:?}",
            args.seeds.start,
            args.seeds.end,
            args.agents.name(),
            args.twice
        );
        if args.agents != AgentsMutation::None {
            if failed > 0 {
                println!("MUTATION {} caught by {failed} seeds", args.agents.name());
                return ExitCode::SUCCESS;
            }
            println!("MUTATION {} NOT caught", args.agents.name());
            return ExitCode::FAILURE;
        }
        return if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    println!(
        "RESULT seeds={}..{} mutation={} passed={passed} failed={failed} twice={} stats={totals:?}",
        args.seeds.start,
        args.seeds.end,
        mutation.name(),
        args.twice
    );
    match mutation {
        Mutation::None if failed == 0 => ExitCode::SUCCESS,
        Mutation::None => ExitCode::FAILURE,
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

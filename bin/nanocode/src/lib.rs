#![cfg(not(target_family = "wasm"))]

mod actions;
mod app;
#[cfg(target_os = "linux")]
mod arguments;
mod attempts;
mod cells;
mod cli;
mod commands;
#[cfg(unix)]
mod cua;
mod custom_endpoint;
mod defaults;
mod events;
mod exec;
mod extras;
mod focus;
mod history;
mod host;
mod interactive;
mod login;
mod models;
mod patch;
mod prepare;
mod python;
mod sessions;
mod setup;
mod side;
mod split;
mod stream;
mod telemetry;
mod tempo;
mod vertex_claude;
mod view;

use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use nanocode::{
    ModelSelection, ServiceTier, Thinking, agent::events::SharedTextDelta,
    durability::TranscriptItem,
};
use nanocode_sync::{FutureExt, StreamExt};

use crate::{
    cli::{Cli, Command, GatewayArgs, HandArgs, HarnessArg, SessionArgs, SessionsArgs},
    host::{Host, HostConfig, Result},
};

pub use host::{ModelClients, Panel};

const PIPED_PROMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
const PIPED_PROMPT_LIMIT: usize = 10 * 1024 * 1024;
const ESCALATION_PROMPT: &str = "Continue.";
const EDIT_TOOLS: [&str; 5] = ["Edit", "MultiEdit", "NotebookEdit", "Write", "apply_patch"];
const WRITE_MARKS: [&str; 12] = [
    "> ",
    ">>",
    "<<",
    "tee ",
    "sed -i",
    "apply_patch",
    "write",
    "mv ",
    "cp ",
    "rm ",
    "mkdir ",
    "touch ",
];
const TELEMETRY_SHUTDOWN: std::time::Duration = std::time::Duration::from_millis(500);

fn session_gateway(cli: &Cli) -> Option<GatewayArgs> {
    match &cli.command {
        None | Some(Command::Models | Command::Resume { .. } | Command::Default { .. }) => {
            Some(cli.session.gateway.clone())
        }
        Some(Command::Exec(args)) => Some(args.clone().or(cli.session.clone()).gateway),
        _ => None,
    }
}

fn cli_home(cli: &Cli) -> Result<PathBuf> {
    let explicit_home = match &cli.command {
        Some(Command::Exec(args)) => args.home.as_deref(),
        Some(Command::Sessions(args)) => args.home.as_deref(),
        _ => None,
    }
    .or(cli.session.home.as_deref());
    host::home_dir(explicit_home)
}

fn install_telemetry(
    cli: &Cli,
    home: &Path,
) -> Result<Option<nanocode_observability::ObservabilityGuard>> {
    match telemetry(cli, home, true).install() {
        Err(nanocode_observability::ObservabilityError::LocalFile(path, error))
            if path.starts_with(home) =>
        {
            eprintln!(
                "nanocode: running without local logs and telemetry: cannot write {}: {error}; set --home or NANOCODE_HOME to a writable directory to keep them",
                path.display()
            );
            Ok(Some(telemetry(cli, home, false).install()?))
        }
        installed => Ok(Some(installed?)),
    }
}

fn telemetry(
    cli: &Cli,
    home: &Path,
    local_files: bool,
) -> nanocode_observability::ObservabilityBuilder {
    let args = &cli.observability;
    let interactive = !matches!(
        cli.command,
        Some(
            Command::Exec(_)
                | Command::Hand(_)
                | Command::Sessions(_)
                | Command::Models
                | Command::Default { .. }
        )
    ) && !is_cua(cli);
    let output = match (args.log_filter().is_some(), interactive, local_files) {
        (false, _, _) | (true, true, false) => nanocode_observability::LogOutput::Disabled,
        (true, false, _) => nanocode_observability::LogOutput::Stderr,
        (true, true, true) => {
            nanocode_observability::LogOutput::File(home.join("log").join("nanocode.log"))
        }
    };
    let builder = args
        .builder(
            "nanocode",
            env!("CARGO_PKG_VERSION"),
            output,
            if matches!(cli.command, Some(Command::Hand(_) | Command::Sessions(_))) {
                nanocode_observability::Telemetry::Off
            } else {
                nanocode_observability::Telemetry::Agent
            },
        )
        .shutdown_timeout(TELEMETRY_SHUTDOWN);
    if local_files {
        builder.otlp_directory(home.join("log").join("otel"))
    } else {
        builder
    }
}

const SHELL_UTILITIES: &[psh_engine::util::UtilEntry] = &[patch::UTILITY, python::UTILITY];

#[cfg(unix)]
fn embedded(cli: &Cli) -> bool {
    matches!(&cli.command, Some(cli::Command::Hand(args)) if args.own_processes)
        || nanocode_psh_embedded::embed().is_ok()
}

#[cfg(not(unix))]
const fn embedded(_: &Cli) -> bool {
    true
}

#[cfg(unix)]
const fn is_cua(cli: &Cli) -> bool {
    matches!(cli.command, Some(Command::Cua(_)))
}

#[cfg(not(unix))]
const fn is_cua(_: &Cli) -> bool {
    false
}

pub fn run() -> ExitCode {
    run_with(ModelClients::default())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn version_requested() -> Option<ExitCode> {
    nanocode_sys::entry::only_argument_in(&[c"--version", c"-V"]).then(|| {
        let _ = nanocode_sys::entry::print(
            concat!("nanocode ", env!("CARGO_PKG_VERSION"), "\n").as_bytes(),
        );
        ExitCode::SUCCESS
    })
}

pub fn run_with(clients: ModelClients) -> ExitCode {
    start::<false>(clients)
}

pub fn run_exec() -> ExitCode {
    start::<true>(ModelClients::default())
}

fn start<const EXEC: bool>(clients: ModelClients) -> ExitCode {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(code) = version_requested() {
        return code;
    }
    if let Some(code) =
        nanocode_psh::serve_if_requested(|arguments| psh::run_with(arguments, SHELL_UTILITIES))
    {
        return code;
    }
    #[cfg(unix)]
    if let Some(code) = nanocode_process::read_outputs_if_requested() {
        return code;
    }
    let entered = std::time::Instant::now();
    let cli = Cli::parse();
    let shell = nanocode_psh::shell();
    #[cfg(target_os = "linux")]
    if (cli.session.prompt.is_some()
        || matches!(&cli.command, Some(cli::Command::Exec(args)) if args.prompt.is_some()))
        && let Err(error) = arguments::hide()
    {
        eprintln!("nanocode: the prompt stays visible in this process's arguments: {error}");
    }
    let parsed = entered.elapsed();
    let chosen = shell.map(nanocode_home::shell::choose);
    if !matches!(chosen, Ok(Ok(()))) || !embedded(&cli) {
        eprintln!("nanocode: failed to make psh the shell: {chosen:?}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = cells::apply() {
        eprintln!("nanocode: {error}");
        return ExitCode::FAILURE;
    }
    let clients = match custom_endpoint::apply(clients) {
        Ok(clients) => clients,
        Err(error) => {
            eprintln!("nanocode: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (clients, mut closing) = match session_gateway(&cli) {
        Some(gateway) => match tempo::apply(
            clients,
            &gateway,
            matches!(cli.command, Some(Command::Exec(_))),
        ) {
            Ok(applied) => applied,
            Err(error) => {
                eprintln!("nanocode: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => (clients, None),
    };
    let home = match cli_home(&cli) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("nanocode: failed to resolve the home directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let telemetry_started = std::time::Instant::now();
    let telemetry = match install_telemetry(&cli, &home) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("nanocode: failed to start telemetry: {error}");
            return ExitCode::FAILURE;
        }
    };
    let startup = nanocode_trace::info_span!(
        target: "nanocode",
        "nc.startup",
        process.pid = std::process::id(),
        cli.duration_ms = parsed.as_secs_f64() * 1_000.0,
        telemetry.duration_ms = telemetry_started.elapsed().as_secs_f64() * 1_000.0,
        main_to_span.duration_ms = entered.elapsed().as_secs_f64() * 1_000.0,
        runtime.duration_ms = nanocode_trace::field::Empty
    );
    let one_shot = matches!(cli.command, Some(Command::Exec(_)));
    let runtime_started = std::time::Instant::now();
    let runtime = match startup.in_scope(|| {
        if one_shot {
            nanocode_executor::Builder::new_current_thread()
                .enable_all()
                .build()
        } else {
            nanocode_executor::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
        }
    }) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("nanocode: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    startup.record(
        "runtime.duration_ms",
        runtime_started.elapsed().as_secs_f64() * 1_000.0,
    );
    drop(startup);
    let warming = matches!(
        cli.command,
        None | Some(Command::Exec(_) | Command::Resume { .. })
    )
    .then(|| clients.warming())
    .flatten();
    if one_shot {
        drop(nanocode::durability::drive_on(runtime.handle().clone()));
    }
    let paying = match closing.as_mut().and_then(tempo::Closing::take_start) {
        #[cfg(unix)]
        Some(start) => {
            nanocode_process::prepare_commands(start);
            None
        }
        paying => paying,
    };
    let drawn = matches!(cli.command, None | Some(Command::Resume { .. }));
    let threads = telemetry
        .as_ref()
        .map(nanocode_observability::ObservabilityGuard::exporter_threads);
    if !drawn && let Some(threads) = &threads {
        threads.start();
    }
    if warming.is_some() || paying.is_some() || (drawn && threads.is_some()) {
        nanocode_platform::spawn_on(runtime.handle(), async move {
            if drawn {
                app::FIRST_FRAME.wait().await;
            }
            if let Some(threads) = threads {
                threads.start();
            }
            if let Some(start) = paying {
                tempo::apart(start);
            }
            if let Some(warming) = warming {
                warming.await;
            }
        });
    }
    let outcome = runtime.block_on(async move {
        match cli.command {
            Some(Command::Exec(args)) => {
                on_worker(exec(Box::new(args.or(cli.session)), clients)).await
            }
            _ if EXEC => Err("this executable runs only nanocode exec".into()),
            Some(Command::Hand(args)) => hand(args).await,
            #[cfg(unix)]
            Some(Command::Cua(args)) => cua::cua(args).await,
            Some(Command::Setup(args)) => setup::setup(args).await,
            Some(Command::Login(mut args)) => {
                args.google = args.google.or(cli.session.google);
                setup::login(args).await
            }
            Some(Command::Sessions(mut args)) => {
                args.home = args.home.or(cli.session.home);
                on_worker(list_sessions(args)).await
            }
            Some(Command::Models) => print_models(&clients).await,
            Some(Command::Default { words }) => match host::home_dir(cli.session.home.as_deref()) {
                Ok(home) => defaults::command(words, clients, home).await,
                Err(error) => Err(error),
            },
            Some(Command::Resume { id }) => {
                let mut args = cli.session;
                args.resume = Some(id.unwrap_or_default());
                interactive(args, clients).await
            }
            None => interactive(cli.session, clients).await,
        }
    });
    if let Some(closing) = closing {
        runtime.block_on(tempo::close(closing));
    }
    if one_shot {
        runtime.shutdown_background();
    } else {
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    }
    drop(telemetry);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nanocode: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn on_worker(work: impl Future<Output = Result<()>> + Send + 'static) -> Result<()> {
    nanocode_platform::spawn_task(work).await?
}

struct Prepared {
    host: Host,
    model: ModelSelection,
    route: host::Route,
    thinking: Thinking,
    tier: ServiceTier,
    home: PathBuf,
    runner_events: nanocode_sync::mpsc::UnboundedReceiver<nanocode::RunnerEvent>,
}

async fn prepare(
    args: &SessionArgs,
    clients: ModelClients,
    notify: &mut (dyn FnMut(&str) + Send),
    headless: bool,
) -> Result<Prepared> {
    prepare::prepare(args, clients, notify, headless).await
}

fn working_state_policy(
    args: &SessionArgs,
) -> Result<Option<nanocode::oai::session::working_state::WorkingStatePolicy>> {
    let Some(budget) = args.working_state_budget else {
        return match args.working_state_retain {
            Some(_) => Err("--working-state-retain needs --working-state-budget".into()),
            None => Ok(None),
        };
    };
    let retain = args.working_state_retain.unwrap_or(budget / 2);
    nanocode::oai::session::working_state::WorkingStatePolicy::new(budget, retain)
        .map(Some)
        .map_err(|error| format!("--working-state-budget: {error}").into())
}

fn hedge_policy(args: &SessionArgs) -> Result<nanocode::hedge::HedgePolicy> {
    let default = nanocode::hedge::HedgePolicy::DEFAULT;
    let cap_percent = match args.hedge_cap_percent {
        Some(percent) => u8::try_from(percent)
            .ok()
            .filter(|percent| *percent <= 100)
            .ok_or_else(|| format!("--hedge-cap-percent {percent} is not between 0 and 100"))?,
        None => default.cap_percent,
    };
    let window = match args.hedge_window {
        Some(requests) => usize::try_from(requests)
            .ok()
            .and_then(std::num::NonZeroUsize::new)
            .ok_or_else(|| format!("--hedge-window {requests} is not a positive request count"))?,
        None => default.window,
    };
    Ok(nanocode::hedge::HedgePolicy {
        cap_percent,
        floor: args
            .hedge_floor_ms
            .map_or(default.floor, std::time::Duration::from_millis),
        window,
        ..default
    })
}

fn runner_failure(event: &nanocode::RunnerEvent) -> Option<String> {
    match event {
        nanocode::RunnerEvent::Failed { name, error } => Some(format!("runner {name}: {error}")),
        nanocode::RunnerEvent::StatusChanged { .. } => None,
    }
}

fn family(harness: HarnessArg) -> nanocode::HarnessFamily {
    match harness {
        HarnessArg::Codex => nanocode::HarnessFamily::Codex,
        HarnessArg::Claude => nanocode::HarnessFamily::Claude,
        HarnessArg::Antigravity => nanocode::HarnessFamily::new(nanocode::antigravity::FAMILY),
    }
}

async fn interactive(args: SessionArgs, clients: ModelClients) -> Result<()> {
    interactive::interactive(args, clients).await
}

async fn latest_session(host: &Host) -> Result<String> {
    let store = host
        .config
        .store
        .as_ref()
        .ok_or("--continue requires durability")?;
    store
        .brief_page(
            Some(host.config.workspace.as_path()),
            1,
            sessions::user_facing,
        )
        .await?
        .into_iter()
        .next()
        .map(|session| session.session_id)
        .ok_or_else(|| "no saved session for this workspace".into())
}

async fn exec(args: Box<SessionArgs>, clients: ModelClients) -> Result<()> {
    exec::exec(args, clients).await
}

async fn unanswered(host: &Host, session: &str) -> bool {
    let Some(store) = host.config.store.as_ref() else {
        return false;
    };
    store.load(session).await.is_ok_and(|stored| {
        stored.into_transcript().is_ok_and(|transcript| {
            transcript
                .iter()
                .all(|item| matches!(item, TranscriptItem::User(_)))
        })
    })
}

async fn piped_prompt() -> Result<Option<String>> {
    use std::io::BufRead;
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let stdin = std::io::stdin();
        if nanocode_sys::unix::readable_now(stdin.as_fd())? && stdin.lock().fill_buf()?.is_empty() {
            return Ok(Some(String::new()));
        }
    }
    let (chunks, mut received) = nanocode_sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("nanocode-stdin".to_owned())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            loop {
                let chunk = match stdin.fill_buf() {
                    Ok([]) => return,
                    Ok(bytes) => Ok(bytes.to_vec()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => Err(error),
                };
                let length = chunk.as_ref().map_or(0, Vec::len);
                let failed = chunk.is_err();
                if chunks.send(chunk).is_err() || failed {
                    return;
                }
                stdin.consume(length);
            }
        })?;
    let first = nanocode_sync::select! {
        chunk = received.recv() => chunk,
        () = nanocode_platform::sleep(PIPED_PROMPT_WAIT) => {
            eprintln!(
                "nanocode: no stdin data received in {}s, proceeding without it; redirect stdin from /dev/null to skip the wait",
                PIPED_PROMPT_WAIT.as_secs()
            );
            return Ok(None);
        }
    };
    let mut bytes = Vec::new();
    let mut chunk = first;
    while let Some(next) = chunk {
        bytes.extend_from_slice(&next?);
        if bytes.len() > PIPED_PROMPT_LIMIT {
            return Err(format!(
                "piped stdin input exceeds {} MiB; pass large content as a file path in the prompt instead",
                PIPED_PROMPT_LIMIT / (1024 * 1024)
            )
            .into());
        }
        chunk = received.recv().await;
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

async fn escalation_model(
    host: &Host,
    name: &str,
    route: host::Route,
    session: &ModelSelection,
) -> Result<ModelSelection> {
    if host.config.clients.resolve(name).is_err() {
        host.config.clients.listed().await?;
    }
    let (escalated, model) = host.config.clients.resolve(name)?;
    let model = match host::follows_harness(escalated, &model) {
        true => host::on_harness(&model, &session.family()),
        false => model,
    };
    if escalated != route || model.family() != session.family() {
        return Err(format!(
            "--escalate-model {} must share the route and harness of {}",
            escalated.label(&model),
            route.label(session)
        )
        .into());
    }
    Ok(model)
}

struct Stall {
    after: u64,
    idle: u64,
    cancelled: bool,
}

impl Stall {
    fn cancels(&mut self, event: &nanocode::agent::events::AgentEvent) -> bool {
        use nanocode::agent::events::AgentEventKind;
        match event.kind {
            AgentEventKind::ToolCall => {
                let call = serde_json::from_str::<serde_json::Value>(event.payload.get())
                    .unwrap_or_default();
                if call
                    .get("parent_call_id")
                    .is_some_and(|parent| !parent.is_null())
                {
                    return false;
                }
                let tool = call
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let arguments = call
                    .get("arguments")
                    .map(ToString::to_string)
                    .unwrap_or_default();
                self.idle = match EDIT_TOOLS.contains(&tool)
                    || WRITE_MARKS.iter().any(|mark| arguments.contains(mark))
                {
                    true => 0,
                    false => self.idle + 1,
                };
                false
            }
            AgentEventKind::ModelCallStarted if !self.cancelled && self.idle >= self.after => {
                self.cancelled = true;
                true
            }
            _ => false,
        }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
static MODEL_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn model_call_started() {
    if (MODEL_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1).is_power_of_two() {
        nanocode_sys::entry::release_file_pages();
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
static NESTED_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tool_call_started(event: &nanocode::agent::events::AgentEvent) {
    #[derive(serde::Deserialize)]
    struct Call {
        parent_call_id: Option<serde::de::IgnoredAny>,
    }
    if serde_json::from_str::<Call>(event.payload.get())
        .is_ok_and(|call| call.parent_call_id.is_some())
        && (NESTED_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1).is_power_of_two()
    {
        nanocode_sys::entry::release_file_pages();
    }
}

async fn stream_turn(
    mut turn: nanocode::agent::Turn,
    answers: impl Write,
    terminal: bool,
    stall: Option<u64>,
) -> Result<bool> {
    let mut printer = stream::Printer::new(std::io::BufWriter::new(answers), terminal);
    let control = turn.control();
    let mut stall = stall.map(|after| Stall {
        after,
        idle: 0,
        cancelled: false,
    });
    loop {
        let event = match turn.events.try_recv_timed() {
            Some(event) => event,
            None => {
                printer.flush()?;
                let Some(event) = turn.events.recv_timed().await else {
                    break;
                };
                event
            }
        };
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        match event.event.kind {
            nanocode::agent::events::AgentEventKind::ModelCallStarted => model_call_started(),
            nanocode::agent::events::AgentEventKind::ToolCall => tool_call_started(&event.event),
            _ => {}
        }
        printer.event(
            &event.event,
            event.delta.as_ref().map(SharedTextDelta::borrowed),
        )?;
        if stall
            .as_mut()
            .is_some_and(|stall| stall.cancels(&event.event))
        {
            control.cancel().await?;
        }
    }
    let stalled = stall.is_some_and(|stall| stall.cancelled);
    match turn.await {
        Ok(result) => {
            printer.finish(result.final_message())?;
            Ok(false)
        }
        Err(nanocode::NanocodeError::TurnCancelled) if stalled => {
            printer.flush()?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

async fn list_sessions(args: SessionsArgs) -> Result<()> {
    interactive::list_sessions(args).await
}

fn print_transcript(transcript: &[TranscriptItem]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    for item in transcript {
        match item {
            TranscriptItem::User(text) => writeln!(out, "## user\n{text}\n")?,
            TranscriptItem::Assistant(text) => writeln!(out, "## assistant\n{text}\n")?,
            _ => {}
        }
    }
    out.flush()
}

fn print_sessions(page: &[nanocode::durability::SessionSummary]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    for session in page {
        let record = &session.record;
        writeln!(
            out,
            "{}  {:>8}  {:<20}  {}",
            record.session_id,
            sessions::age(record.updated_at_ms),
            sessions::model(record),
            sessions::title(session)
        )?;
    }
    out.flush()
}

fn unless_closed(printed: std::io::Result<()>) -> std::io::Result<()> {
    match printed {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        printed => printed,
    }
}

async fn print_models(clients: &ModelClients) -> Result<()> {
    if let Some(tempo) = clients.tempo_models() {
        tempo.load().await;
    }
    let listed = clients.listed().await;
    let groups = models::catalog(clients);
    let width = groups
        .iter()
        .flat_map(|group| &group.models)
        .map(|(route, model, _)| route.label(model).chars().count())
        .max()
        .unwrap_or_default();
    let mut out = std::io::stdout().lock();
    let printed = groups.iter().try_for_each(|group| {
        writeln!(out, "{}", group.title)?;
        if let Some(note) = &group.note {
            writeln!(out, "  {note}")?;
        }
        for (route, model, about) in &group.models {
            let capabilities = model.capabilities(nanocode::ModelTransport::Native);
            let thinking = capabilities
                .thinking()
                .map(Thinking::as_str)
                .collect::<Vec<_>>()
                .join("/");
            let features = models::features(model);
            let detail = [features.as_str(), about.as_deref().unwrap_or_default()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            writeln!(
                out,
                "  {:<width$} thinking {:<30} default {:<7} {}",
                route.label(model),
                thinking,
                capabilities.default_thinking().as_str(),
                detail
            )?;
        }
        Ok(())
    });
    unless_closed(printed.and_then(|()| out.flush()))?;
    listed
}

async fn hand(args: HandArgs) -> Result<()> {
    interactive::hand(args).await
}

#[cfg(unix)]
async fn terminated() -> std::io::Result<()> {
    nanocode_executor::signal::unix::signal(
        nanocode_executor::signal::unix::SignalKind::terminate(),
    )?
    .recv()
    .await;
    Ok(())
}

#[cfg(not(unix))]
async fn terminated() -> std::io::Result<()> {
    std::future::pending().await
}

#[cfg(unix)]
fn reader() -> std::io::Result<std::process::Command> {
    let (program, arguments) = nanocode_psh::program()?;
    let mut command = std::process::Command::new(program);
    command.args(arguments);
    Ok(command)
}

#[cfg(unix)]
fn hand_over() -> std::io::Result<()> {
    std::process::exit(nanocode_process::hand_over(reader()?)?.into())
}

#[cfg(not(unix))]
const fn hand_over() -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn leave_reader() -> std::io::Result<()> {
    if nanocode_process::leave_reader(reader()?)? {
        eprintln!(
            "hand: ended; what its commands left running is read by a process of its own until it ends"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
const fn leave_reader() -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn leave_commands() -> std::io::Result<bool> {
    nanocode_process::leave_reader(reader()?)
}

#[cfg(not(unix))]
pub(crate) const fn leave_commands() -> std::io::Result<bool> {
    Ok(false)
}

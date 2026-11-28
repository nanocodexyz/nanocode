use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::downstream::{self, Verdict};
use nanocode_build_opt::{
    check::{self as gate, Finished, Kind, Modules, Outcome, SimJob, Simulator, Unit},
    graph::Graph,
    plan::{self, Rules, Scope},
};

const USAGE: &str = "usage: nc-build check [--full] [--base REF] [-j N] [--seeds N] [--no-sim] [--plan]
  changed scope: the crates the diff since REF (merge-base with the upstream, or HEAD~1 when nothing differs) and untracked files touch,
  with their dependents (cargo check --locked: --lib --bins --examples native, --lib wasm32), the simulator's --tidy and --machines walks, and the pass jobs and canaries of
  bin/nanocode-sim/cfo.sh's table for every scenario that reaches them; --full: the whole workspace, every scenario and canary, and
  cargo test --lib --bins of the crates in FULL_TESTS
  -j N: parallel jobs (default: CARGO_BUILD_JOBS, else the cores this process may run on); --seeds N: seeds per pass job (default 8, 64 with --full; a pass:M job runs at least M)
  a canary passes once a seed catches it, trying seeds 0, then 1..8, then the rest of its budget in units of 16 seeds;
  a unit that gives no result within NANOCODE_CHECK_UNIT_TIMEOUT seconds (default 120) fails; logs: output/check/<check>.log
  --plan: print the crates line, the scenarios line and the simulator jobs, and run nothing
  --full and --downstream (that step alone): with NANOCODE_DOWNSTREAM naming a consumer list, one line per
  consumer built against this worktree, each `name source cargo [dir...]`, `name source run command` or
  `name source pinned reason` (scripts/downstream.example); a pinned consumer is reported and skipped";

const SIM: Simulator = Simulator {
    package: "nanocode-sim",
    dir: "bin/nanocode-sim/",
    src: "bin/nanocode-sim/src/",
    program: "nanocode-sim",
    aliases: &[("runtime", "whole"), ("executor", "tasks")],
};
const TABLE: &str = "bin/nanocode-sim/cfo.sh";
/// Backend/mode combos that must hold the memory invariant (docs/memory-invariant.md), one per line.
const MEMORY_GATE: &str = "bin/nanocode-sim/memory-gate.txt";
const EVERYTHING_PREFIXES: [&str; 5] = [
    "vendor/",
    "scripts/check",
    "scripts/buildcache",
    "scripts/rustc-parallel-frontend",
    "scripts/wild-cc",
];
const WORKSPACE: &str = "nanocode";
const WASM: &str = "wasm32-unknown-unknown";
const WASM_ALWAYS: [&str; 4] = [
    "nanocode-board",
    "nanocode-board-fold",
    "nanocode-partitions",
    "nanocode-turnlog",
];
/// Crates whose unit tests --full runs. Most set `[lib] test = false`, so a bare `cargo test -p`
/// runs nothing for them: the step names the targets, --lib for the libraries and --bins for
/// nc-build, a bin-only crate. rustls and serde_core are the patched crates under crates/patches.
const FULL_TESTS: [&str; 8] = [
    "nc-build",
    "nanocode-tool-host",
    "nanocode-shell",
    "nanocode-workspace-fs",
    "nanocode-backend-claude",
    "serde_derive",
    "rustls",
    "serde_core",
];
const UNIT_TIMEOUT_SECS: u64 = 120;
const HUNG_EXIT: i32 = 124;
const SPAWN_EXIT: i32 = 127;
const SIGNAL_EXIT: i32 = 128;
const PASS_SEEDS: u64 = 8;
const MARKER: &str = ".owner";
const FULL_PASS_SEEDS: u64 = 64;

struct Options {
    full: bool,
    base: Option<String>,
    jobs: usize,
    seeds: u64,
    sim: bool,
    plan: bool,
    steps: Steps,
}

enum Steps {
    Gate,
    Downstream,
}

enum Parsed {
    Run(Options),
    Help,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut options = Options {
        full: false,
        base: None,
        jobs: std::env::var("CARGO_BUILD_JOBS")
            .ok()
            .and_then(|jobs| jobs.parse().ok())
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, usize::from)),
        seeds: 0,
        sim: true,
        plan: false,
        steps: Steps::Gate,
    };
    let mut seeds = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--full" => options.full = true,
            "--base" => options.base = Some(value("--base")?),
            "-j" => {
                options.jobs = value("-j")?
                    .parse()
                    .map_err(|error| format!("-j: {error}"))?;
            }
            "--seeds" => {
                seeds = Some(
                    value("--seeds")?
                        .parse()
                        .map_err(|error| format!("--seeds: {error}"))?,
                );
            }
            "--no-sim" => options.sim = false,
            "--plan" => options.plan = true,
            "--downstream" => options.steps = Steps::Downstream,
            "-h" | "--help" => return Ok(Parsed::Help),
            other => return Err(format!("unknown option {other}")),
        }
    }
    options.seeds = seeds.unwrap_or(if options.full {
        FULL_PASS_SEEDS
    } else {
        PASS_SEEDS
    });
    Ok(Parsed::Run(options))
}

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Fail,
    Skip,
}

struct Ui {
    color: bool,
    logs: PathBuf,
    failed: Vec<String>,
}

impl Ui {
    fn line(&self, status: Status, name: &str, secs: f64, detail: &str) {
        let (word, color) = match status {
            Status::Ok => ("ok", "\x1b[32m"),
            Status::Fail => ("FAIL", "\x1b[31m"),
            Status::Skip => ("skip", "\x1b[33m"),
        };
        let (word, rest) = gate::line(word, name, secs, detail);
        let (on, off) = if self.color {
            (color, "\x1b[0m")
        } else {
            ("", "")
        };
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{on}{word}{off}{rest}");
        let _ = out.flush();
    }

    fn print(lines: &[String]) {
        let mut out = io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }

    fn log(&self, name: &str) -> PathBuf {
        self.logs.join(format!("{name}.log"))
    }

    fn fail(&mut self, name: &str) {
        let _ = File::create(self.logs.join(format!("{name}.failed")));
        self.failed.push(name.to_owned());
    }

    fn downstream(&mut self, report: &downstream::Report) {
        let log = format!("downstream-{}", report.consumer());
        let (name, secs, detail) = ("downstream", report.secs(), report.detail());
        match report.verdict() {
            Verdict::Built => self.line(Status::Ok, name, secs, detail),
            Verdict::Pinned(reason) => {
                self.line(
                    Status::Skip,
                    name,
                    secs,
                    &format!("{detail} pinned: {reason}"),
                );
            }
            Verdict::Failed { errors, exit } => {
                let noun = if *errors == 1 { "error" } else { "errors" };
                self.line(
                    Status::Fail,
                    name,
                    secs,
                    &format!("{detail}: {errors} {noun} (exit {exit}, output/check/{log}.log)"),
                );
                let text = fs::read_to_string(self.log(&log)).unwrap_or_default();
                Self::print(&gate::failure_lines(&text));
                self.fail(&log);
            }
            Verdict::Broken(error) => {
                self.line(Status::Fail, name, secs, &format!("{detail}: {error}"));
                self.fail(&log);
            }
        }
    }

    fn downstreams(&mut self, root: &Path, target: &Path, jobs: usize) {
        let Some(list) = std::env::var_os(downstream::LIST) else {
            let why = format!("{} names no consumer list", downstream::LIST);
            self.line(Status::Skip, "downstream", 0.0, &why);
            return;
        };
        let logs = self.logs.clone();
        downstream::run(Path::new(&list), root, target, &logs, jobs, &mut |report| {
            self.downstream(&report);
        });
    }

    fn verdict(&self, started: Instant) -> u8 {
        let secs = started.elapsed().as_secs_f64();
        let (red, green, off) = if self.color {
            ("\x1b[31m", "\x1b[32m", "\x1b[0m")
        } else {
            ("", "", "")
        };
        if self.failed.is_empty() {
            Self::print(&[format!("{green}GREEN{off} {WORKSPACE} {secs:.1}s")]);
            return 0;
        }
        let mut failed = self.failed.clone();
        failed.sort();
        Self::print(&[format!(
            "{red}RED{off}   {WORKSPACE} {secs:.1}s ({})",
            failed.join(" ")
        )]);
        1
    }

    fn step(&mut self, step: &Step, detail: &str) {
        if step.exit == 0 {
            self.line(Status::Ok, &step.name, step.secs, detail);
            return;
        }
        self.line(
            Status::Fail,
            &step.name,
            step.secs,
            &format!(
                "{detail} (exit {}, output/check/{}.log)",
                step.exit, step.name
            ),
        );
        let log = fs::read_to_string(self.log(&step.name)).unwrap_or_default();
        Self::print(&gate::failure_lines(&log));
        self.fail(&step.name);
    }
}

struct Step {
    name: String,
    exit: i32,
    secs: f64,
}

fn code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT + signal))
        .unwrap_or(SIGNAL_EXIT)
}

fn logged(name: &str, logs: &Path, mut command: Command) -> Step {
    let started = Instant::now();
    let exit = File::create(logs.join(format!("{name}.log")))
        .and_then(|file| Ok((file.try_clone()?, file)))
        .and_then(|(out, err)| {
            command
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .status()
        })
        .map_or(SPAWN_EXIT, code);
    Step {
        name: name.to_owned(),
        exit,
        secs: started.elapsed().as_secs_f64(),
    }
}

fn cargo(jobs: usize) -> Command {
    let mut command = Command::new("cargo");
    command
        .env("CARGO_BUILD_JOBS", jobs.to_string())
        .env("SHIPPY_REMOTE_BUILD", "0")
        .env("CARGO_TERM_COLOR", "never");
    command
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn base(given: Option<String>) -> String {
    if let Some(base) = given {
        return base;
    }
    let upstream = git(&["rev-parse", "-q", "--verify", "@{upstream}"])
        .or_else(|| git(&["rev-parse", "-q", "--verify", "origin/master"]))
        .unwrap_or_default();
    let base = git(&["merge-base", "HEAD", &upstream]).unwrap_or_else(|| "HEAD".to_owned());
    if git(&["rev-parse", "HEAD"]) == git(&["rev-parse", &base])
        && git(&["status", "--porcelain"]).is_some_and(|status| status.is_empty())
    {
        return "HEAD~1".to_owned();
    }
    base
}

fn changed(base: &str) -> Result<Vec<String>, String> {
    let mut changed = BTreeSet::new();
    for args in [
        &["diff", "--name-only", base, "--"][..],
        &["ls-files", "--others", "--exclude-standard"][..],
    ] {
        let listing = git(args).ok_or_else(|| format!("git {} failed", args.join(" ")))?;
        changed.extend(
            listing
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(changed.into_iter().collect())
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<_>>()?;
    entries.sort();
    Ok(entries)
}

fn walk(dir: &Path, files: &mut Vec<(String, String)>) -> io::Result<()> {
    for path in sorted_entries(dir)? {
        if path.is_dir() {
            walk(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = fs::read_to_string(&path)?;
            files.push((path.to_string_lossy().into_owned(), text));
        }
    }
    Ok(())
}

fn modules(src: &str) -> Result<Modules, String> {
    let read = || -> io::Result<Modules> {
        let mut modules = Modules::new();
        for path in sorted_entries(Path::new(src))? {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if let Some(module) = name.strip_suffix(".rs") {
                let text = fs::read_to_string(&path)?;
                modules
                    .entry(module.to_owned())
                    .or_default()
                    .push((path.to_string_lossy().into_owned(), text));
            } else if path.is_dir() {
                walk(&path, modules.entry(name).or_default())?;
            }
        }
        Ok(modules)
    };
    read().map_err(|error| format!("{src}: {error}"))
}

enum Selection {
    Workspace,
    /// Every reached crate, checked natively with --lib --bins --examples so a bin-only crate
    /// (nc-board, nanocode-fuzz, ...) and examples are checked too, and the libraries among them,
    /// checked for wasm32.
    Crates {
        reached: BTreeSet<String>,
        libraries: BTreeSet<String>,
    },
}

struct Planned {
    selection: Selection,
    scenarios: Vec<String>,
    /// The change reaches nanocode-sim, so its memory gate runs even when no scenario does.
    simulator: bool,
}

fn scope_of(full: bool, changed: &[String], graph: &Graph) -> Result<Planned, String> {
    let mut rules = Rules::cargo();
    rules
        .everything_prefixes
        .extend(EVERYTHING_PREFIXES.map(str::to_owned));
    let scope = if full {
        Scope::Everything
    } else {
        plan::affected(graph, &rules, changed)
    };
    match scope {
        Scope::Everything => Ok(Planned {
            selection: Selection::Workspace,
            simulator: true,
            scenarios: gate::all_scenarios(&modules(SIM.src)?)
                .map_err(|error| format!("{error:?}"))?,
        }),
        Scope::Crates(reached) => {
            let scenarios = if reached.contains(SIM.package) {
                gate::scenarios(&SIM, &modules(SIM.src)?, graph, &reached, changed)
                    .map_err(|error| format!("{error:?}"))?
            } else {
                Vec::new()
            };
            Ok(Planned {
                simulator: reached.contains(SIM.package),
                selection: Selection::Crates {
                    libraries: gate::libraries(graph, &reached),
                    reached,
                },
                scenarios,
            })
        }
    }
}

fn run_unit(bin: &Path, job: &SimJob, unit: &Unit, timeout: Duration) -> (i32, String) {
    let (mut reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => return (SPAWN_EXIT, format!("FAIL pipe: {error}")),
    };
    let spawned = writer.try_clone().and_then(|out| {
        Command::new(bin)
            .arg("--seeds")
            .arg(format!("{}..{}", unit.from, unit.upto))
            .args(&job.flags)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(writer)
            .spawn()
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return (
                SPAWN_EXIT,
                format!("FAIL could not start {}: {error}", bin.display()),
            );
        }
    };
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let _ = reader.read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let (hung, output) = match receiver.recv_timeout(timeout) {
        Ok(output) => (false, output),
        Err(_) => {
            let _ = child.kill();
            (true, receiver.recv().unwrap_or_default())
        }
    };
    let exit = child.wait().map_or(SPAWN_EXIT, code);
    let output = String::from_utf8_lossy(&output).into_owned();
    if hung {
        return (
            HUNG_EXIT,
            format!(
                "FAIL hung: no result after {} s\n{output}",
                timeout.as_secs()
            ),
        );
    }
    (exit, output)
}

fn run_units(
    bin: &Path,
    jobs: &[SimJob],
    units: &[Unit],
    workers: usize,
    timeout: Duration,
) -> Vec<Finished> {
    let next = AtomicUsize::new(0);
    let caught: Vec<AtomicBool> = jobs.iter().map(|_| AtomicBool::new(false)).collect();
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..workers.min(units.len()) {
            let (sender, next, caught) = (sender.clone(), &next, &caught);
            scope.spawn(move || {
                while let Some(unit) = units.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let job = &jobs[unit.job];
                    if job.kind == Kind::Canary && caught[unit.job].load(Ordering::Acquire) {
                        continue;
                    }
                    let started = Instant::now();
                    let (exit, output) = run_unit(bin, job, unit, timeout);
                    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let outcome = gate::classify(job.kind, exit, &output);
                    if outcome == Outcome::Caught {
                        caught[unit.job].store(true, Ordering::Release);
                    }
                    let _ = sender.send(Finished {
                        unit: *unit,
                        outcome,
                        ms,
                    });
                }
            });
        }
        drop(sender);
        receiver.iter().collect()
    })
}

enum SimBuild {
    Failed {
        done: Instant,
    },
    Built {
        done: Instant,
        tidy: Step,
        machines: Step,
        canaries: Step,
        memory: Option<(Step, String)>,
        sim: Step,
    },
}

fn simulate(target: &Path, logs: &Path, options: &Options, jobs: &[SimJob]) -> SimBuild {
    let sim_target = target.join("check-sim");
    let profile = if options.full { "release" } else { "fast" };
    let mut build = cargo(options.jobs);
    build.env("CARGO_TARGET_DIR", &sim_target).args([
        "build",
        "--locked",
        "--profile",
        profile,
        "-p",
        SIM.package,
    ]);
    let built = logged("sim-build", logs, build);
    let bin = logs.join(SIM.program);
    let copied =
        built.exit == 0 && fs::copy(sim_target.join(profile).join(SIM.program), &bin).is_ok();
    let done = Instant::now();
    if !copied {
        return SimBuild::Failed { done };
    }
    let times_path = sim_target.join("sim-times.txt");
    let units = gate::units(jobs, &fs::read_to_string(&times_path).unwrap_or_default());
    let _ = fs::write(
        logs.join("sim-units.txt"),
        units
            .iter()
            .map(|unit| format!("{} {}..{}\n", jobs[unit.job].name, unit.from, unit.upto))
            .collect::<String>(),
    );
    let timeout = Duration::from_secs(
        std::env::var("NANOCODE_CHECK_UNIT_TIMEOUT")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(UNIT_TIMEOUT_SECS),
    );
    thread::scope(|scope| {
        let tidy = scope.spawn(|| {
            let mut command = Command::new(&bin);
            command.arg("--tidy");
            logged("tidy", logs, command)
        });
        let machines = scope.spawn(|| {
            let mut command = Command::new(&bin);
            command.args(["--machines", "--deep"]);
            let walk = logged("machines", logs, command);
            let mut command = Command::new(&bin);
            command.args(["--machines", "--canaries"]);
            (walk, logged("machine-canaries", logs, command))
        });
        let memory = scope.spawn(|| {
            let combos = memory_combos();
            (!combos.is_empty()).then(|| {
                let mut command = Command::new(&bin);
                command
                    .args(["--memory", "--memory-gate", "--only"])
                    .arg(combos.join(","));
                let names = combos
                    .iter()
                    .filter_map(|combo| combo.split_whitespace().next())
                    .collect::<Vec<_>>();
                (logged("memory", logs, command), names.join(" "))
            })
        });
        let started = Instant::now();
        let finished = run_units(&bin, jobs, &units, options.jobs, timeout);
        let report = gate::report(&SIM, jobs, &finished, units.len());
        let _ = fs::write(&times_path, report.times.join("\n") + "\n");
        let _ = fs::write(logs.join("sim.log"), report.log.join("\n") + "\n");
        let sim = Step {
            name: "sim".to_owned(),
            exit: i32::from(!report.green),
            secs: started.elapsed().as_secs_f64(),
        };
        let tidy = tidy.join().unwrap_or(Step {
            name: "tidy".to_owned(),
            exit: SPAWN_EXIT,
            secs: 0.0,
        });
        let (machines, canaries) = machines.join().unwrap_or_else(|_| {
            let failed = |name: &str| Step {
                name: name.to_owned(),
                exit: SPAWN_EXIT,
                secs: 0.0,
            };
            (failed("machines"), failed("machine-canaries"))
        });
        let memory = memory.join().unwrap_or_else(|_| {
            Some((
                Step {
                    name: "memory".to_owned(),
                    exit: SPAWN_EXIT,
                    secs: 0.0,
                },
                String::new(),
            ))
        });
        SimBuild::Built {
            done,
            tidy,
            machines,
            canaries,
            memory,
            sim,
        }
    })
}

fn memory_combos() -> Vec<String> {
    fs::read_to_string(MEMORY_GATE)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Pass => "pass",
        Kind::Canary => "canary",
    }
}

fn graph(logs: &Path, jobs: usize) -> Result<(Graph, PathBuf), String> {
    let log = File::create(logs.join("metadata.log")).map_err(|error| error.to_string())?;
    let output = cargo(jobs)
        .args(["metadata", "--format-version", "1", "--locked", "--no-deps"])
        .stderr(log)
        .output()
        .map_err(|error| format!("cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err("cargo metadata failed".to_owned());
    }
    let json = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let target = serde_json::from_str::<serde_json::Value>(&json)
        .map_err(|error| error.to_string())?
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or("cargo metadata has no target_directory")?;
    Ok((
        Graph::from_metadata(&json).map_err(|error| format!("{error:?}"))?,
        target,
    ))
}

fn worktree_target(root: &Path, reported: PathBuf) -> Result<PathBuf, String> {
    let target = if reported.starts_with(root) {
        reported
    } else {
        reported.join(gate::worktree_dir(&root.to_string_lossy()))
    };
    let marker = target.join(MARKER);
    let root = root.to_string_lossy();
    match fs::read_to_string(&marker) {
        Ok(owner) if owner.trim() != root => Err(format!(
            "{} holds the builds of the worktree {}: give every worktree its own target dir",
            target.display(),
            owner.trim()
        )),
        Ok(_) => Ok(target),
        Err(_) => fs::create_dir_all(&target)
            .and_then(|()| fs::write(&marker, format!("{root}\n")))
            .map(|()| target.clone())
            .map_err(|error| format!("{}: {error}", target.display())),
    }
}

/// Runs the gate and returns its exit status: 0 GREEN, 1 RED, 2 when it could not run.
pub fn main(args: &[String]) -> u8 {
    let options = match parse(args) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return 0;
        }
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return 2;
        }
    };
    let started = Instant::now();
    let Some(root) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("nc-build check: not in a git repository");
        return 2;
    };
    if let Err(error) = std::env::set_current_dir(&root) {
        eprintln!("{root}: {error}");
        return 2;
    }
    let root = PathBuf::from(root);
    let logs = root.join("output/check");
    let _ = fs::remove_dir_all(&logs);
    if let Err(error) = fs::create_dir_all(&logs) {
        eprintln!("{}: {error}", logs.display());
        return 2;
    }
    let mut ui = Ui {
        color: io::stdout().is_terminal(),
        logs: logs.clone(),
        failed: Vec::new(),
    };
    if matches!(options.steps, Steps::Downstream) {
        match graph(&logs, options.jobs).and_then(|(_, reported)| worktree_target(&root, reported))
        {
            Ok(target) => ui.downstreams(&root, &target, options.jobs),
            Err(error) => {
                ui.line(Status::Fail, "scope", 0.0, &error);
                ui.fail("scope");
            }
        }
        return ui.verdict(started);
    }
    if !options.plan {
        let scrub = Command::new("cargo-shippy")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if scrub {
            let mut command = Command::new("cargo-shippy");
            command.arg("--self-test");
            let step = logged("shippy-scrub", &logs, command);
            ui.step(&step, "cargo-shippy --self-test: no secret reaches cargo");
        } else {
            ui.line(
                Status::Skip,
                "shippy-scrub",
                0.0,
                "no cargo-shippy on this machine",
            );
        }
    }
    let base = (!options.full).then(|| base(options.base.clone()));
    let planned = graph(&logs, options.jobs).and_then(|(graph, reported)| {
        let changed = match &base {
            Some(base) => changed(base)?,
            None => Vec::new(),
        };
        Ok((scope_of(options.full, &changed, &graph)?, reported))
    });
    let (planned, target) = match planned.and_then(|(planned, reported)| {
        let target = if options.plan {
            reported
        } else {
            worktree_target(&root, reported)?
        };
        Ok((planned, target))
    }) {
        Ok(planned) => planned,
        Err(error) => {
            let _ = fs::OpenOptions::new()
                .append(true)
                .open(logs.join("metadata.log"))
                .and_then(|mut log| writeln!(log, "{error}"));
            ui.line(
                Status::Fail,
                "scope",
                0.0,
                "cargo metadata, scope selection or the target dir failed (output/check/metadata.log)",
            );
            let log = fs::read_to_string(logs.join("metadata.log")).unwrap_or_default();
            let lines: Vec<String> = log.lines().map(str::to_owned).collect();
            Ui::print(&lines[lines.len().saturating_sub(gate::ERROR_CONTEXT_LINES)..]);
            return 1;
        }
    };
    let crates_line = match &planned.selection {
        Selection::Workspace => "all".to_owned(),
        Selection::Crates { reached, .. } => reached.iter().cloned().collect::<Vec<_>>().join(" "),
    };
    let _ = fs::write(
        logs.join("scope.txt"),
        format!("{crates_line}\n{}\n", planned.scenarios.join(" ")),
    );
    let since = base
        .as_deref()
        .and_then(|base| git(&["rev-parse", "--short", base]))
        .map(|short| format!(" since {short}"))
        .unwrap_or_default();
    let packages = |crates: &BTreeSet<String>| -> Vec<String> {
        crates
            .iter()
            .flat_map(|name| ["-p".to_owned(), name.clone()])
            .collect()
    };
    let always_wasm = |libraries: &BTreeSet<String>| -> Vec<String> {
        let mut crates = libraries.clone();
        crates.extend(WASM_ALWAYS.map(str::to_owned));
        let mut wasm = packages(&crates);
        wasm.push("--lib".to_owned());
        wasm
    };
    // Native: --lib --bins --examples (--bins --examples alone when no reached crate has a library,
    // since cargo refuses --lib then); wasm32: the libraries plus the crates nanocode-ts links.
    // Empty means skipped.
    let (native_args, wasm_args, detail): (Vec<String>, Vec<String>, String) =
        match &planned.selection {
            Selection::Workspace => (
                ["--workspace", "--lib", "--bins", "--examples"]
                    .map(str::to_owned)
                    .to_vec(),
                ["--workspace", "--lib"].map(str::to_owned).to_vec(),
                "workspace".to_owned(),
            ),
            Selection::Crates { reached, .. } if reached.is_empty() => (
                Vec::new(),
                always_wasm(&BTreeSet::new()),
                format!("no crates changed{since}"),
            ),
            Selection::Crates { reached, libraries } => {
                let mut native = packages(reached);
                if !libraries.is_empty() {
                    native.push("--lib".to_owned());
                }
                native.push("--bins".to_owned());
                native.push("--examples".to_owned());
                let wasm = always_wasm(libraries);
                (
                    native,
                    wasm,
                    format!(
                        "{} crates changed{since}, with dependents ({} libraries)",
                        reached.len(),
                        libraries.len()
                    ),
                )
            }
        };
    let jobs = if options.sim
        && (!planned.scenarios.is_empty() || (planned.simulator && !memory_combos().is_empty()))
    {
        let jobs = fs::read_to_string(TABLE)
            .map_err(|error| format!("{TABLE}: {error}"))
            .and_then(|script| {
                gate::sim_jobs(
                    &gate::default_table(&script),
                    &planned.scenarios,
                    options.seeds,
                )
                .map_err(|error| format!("{TABLE}: {error:?}"))
            });
        match jobs {
            Ok(jobs) => Some(jobs),
            Err(error) => {
                ui.line(Status::Fail, "scope", 0.0, &error);
                return 1;
            }
        }
    } else {
        None
    };
    if let Some(jobs) = &jobs {
        let _ = fs::write(
            logs.join("sim-jobs.txt"),
            jobs.iter()
                .map(|job| {
                    format!(
                        "{} {} {} {}\n",
                        job.name,
                        kind_name(job.kind),
                        job.seeds,
                        job.flags.join(" ")
                    )
                })
                .collect::<String>(),
        );
    }
    if options.plan {
        let mut lines = vec![crates_line, planned.scenarios.join(" ")];
        lines.extend(
            fs::read_to_string(logs.join("sim-jobs.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned),
        );
        Ui::print(&lines);
        return 0;
    }
    let outcome = thread::scope(|scope| {
        let (target, logs, options) = (&target, &logs, &options);
        let sim = jobs
            .as_deref()
            .map(|jobs| scope.spawn(move || simulate(target, logs, options, jobs)));
        if native_args.is_empty() {
            ui.line(Status::Skip, "check", 0.0, &detail);
        } else {
            let mut native = cargo(options.jobs);
            native
                .env("CARGO_TARGET_DIR", target)
                .args(["check", "--locked"])
                .args(&native_args);
            let step = logged("check", logs, native);
            ui.step(&step, &detail);
        }
        if wasm_args.is_empty() {
            let why = if native_args.is_empty() {
                detail.clone()
            } else {
                "no library among the changed crates".to_owned()
            };
            ui.line(Status::Skip, "check-wasm", 0.0, &why);
        } else {
            let mut wasm = cargo(options.jobs);
            wasm.env("CARGO_TARGET_DIR", target)
                .args(["check", "--locked", "--target", WASM])
                .args(&wasm_args);
            let step = logged("check-wasm", logs, wasm);
            ui.step(&step, &detail);
            if step.exit != 0
                && fs::read_to_string(ui.log("check-wasm")).is_ok_and(|log| {
                    log.contains("__builtin_clzg") || log.contains("__builtin_ctzg")
                })
            {
                Ui::print(&["     this clang can't build python-capi's wasm32 libc++: set NANOCODE_CC to the clang docs/RELEASE-CHAIN.md uses".to_owned()]);
            }
        }
        if options.full {
            let mut test = cargo(options.jobs);
            test.env("CARGO_TARGET_DIR", target)
                .args(["test", "--locked", "--no-fail-fast"])
                .args(FULL_TESTS.iter().flat_map(|name| ["-p", name]))
                .args(["--lib", "--bins"]);
            let step = logged("test", logs, test);
            ui.step(
                &step,
                &format!("cargo test --lib --bins: {}", FULL_TESTS.join(" ")),
            );
            ui.downstreams(&root, target, options.jobs);
        }
        let checked = Instant::now();
        sim.map(|handle| (checked, handle.join()))
    });
    match (outcome, &jobs) {
        (
            Some((
                checked,
                Ok(SimBuild::Built {
                    done,
                    tidy,
                    machines,
                    canaries,
                    memory,
                    sim,
                }),
            )),
            Some(jobs),
        ) => {
            ui.line(
                Status::Ok,
                "sim-build",
                done.saturating_duration_since(checked).as_secs_f64(),
                &format!(
                    "{} nanocode-sim (time left after the checks)",
                    if options.full { "release" } else { "fast" }
                ),
            );
            ui.step(&tidy, "nanocode-sim --tidy");
            ui.step(&machines, "nanocode-sim --machines --deep");
            ui.step(&canaries, "nanocode-sim --machines --canaries");
            match &memory {
                Some((step, combos)) => ui.step(
                    step,
                    &format!("nanocode-sim --memory --memory-gate: {combos}"),
                ),
                None => ui.line(
                    Status::Skip,
                    "memory",
                    0.0,
                    &format!("no combos in {MEMORY_GATE}"),
                ),
            }
            let passes = jobs.iter().filter(|job| job.kind == Kind::Pass).count();
            if jobs.is_empty() {
                ui.line(
                    Status::Skip,
                    "sim",
                    0.0,
                    "no simulator scenario reaches the changed crates",
                );
            } else {
                ui.step(
                    &sim,
                    &format!(
                        "{} scenarios: {passes} pass jobs x {} seeds, {} canaries",
                        planned.scenarios.len(),
                        options.seeds,
                        jobs.len() - passes
                    ),
                );
            }
        }
        (Some((checked, Ok(SimBuild::Failed { done }))), _) => {
            ui.line(
                Status::Fail,
                "sim-build",
                done.saturating_duration_since(checked).as_secs_f64(),
                "(output/check/sim-build.log)",
            );
            let log = fs::read_to_string(ui.log("sim-build")).unwrap_or_default();
            Ui::print(&gate::error_context(&log));
            ui.fail("sim-build");
        }
        (Some(_), _) => {
            ui.line(Status::Fail, "sim", 0.0, "the simulator runner panicked");
            ui.fail("sim");
        }
        (None, _) => ui.line(
            Status::Skip,
            "sim",
            0.0,
            "no simulator scenario reaches the changed crates",
        ),
    }
    ui.verdict(started)
}

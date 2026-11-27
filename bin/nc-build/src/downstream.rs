use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::Instant,
};

use nanocode_build_opt::check::worktree_dir;

pub(crate) const LIST: &str = "NANOCODE_DOWNSTREAM";
const CLONES: &str = "NANOCODE_DOWNSTREAM_DIR";
const UPSTREAM: &str = "https://github.com/nanocodexyz/nanocode";
const SKIPPED: [&str; 4] = ["target", "output", "node_modules", "vendor"];
const SHAPE: &str =
    "name source cargo [dir...], name source run command, name source pinned reason";
const SIGNAL_EXIT: i32 = 128;

enum How {
    Cargo(Vec<String>),
    Run(String),
    Pinned(String),
}

struct Consumer {
    name: String,
    source: String,
    how: How,
}

#[derive(Debug)]
pub(crate) enum Error {
    Io {
        path: PathBuf,
        error: io::Error,
    },
    Line {
        line: u32,
        text: String,
    },
    Duplicate {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    NoUpstream {
        lock: PathBuf,
    },
    NoCheckout {
        path: PathBuf,
    },
    Git {
        args: String,
        exit: i32,
    },
    Spawn {
        program: String,
        error: io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Line { line, text } => write!(f, "line {line} is none of {SHAPE}: {text}"),
            Self::Duplicate {
                name,
                first,
                second,
            } => write!(
                f,
                "two packages named {name}: {} and {}",
                first.display(),
                second.display()
            ),
            Self::NoUpstream { lock } => {
                write!(f, "{} takes no crate from {UPSTREAM}", lock.display())
            }
            Self::NoCheckout { path } => write!(f, "{} is not a directory", path.display()),
            Self::Git { args, exit } => write!(f, "git {args} exited {exit}"),
            Self::Spawn { program, error } => write!(f, "could not start {program}: {error}"),
        }
    }
}

pub(crate) enum Verdict {
    Built,
    Failed { errors: u32, exit: i32 },
    Pinned(String),
    Broken(Error),
}

pub(crate) struct Report {
    consumer: String,
    detail: String,
    secs: f64,
    verdict: Verdict,
}

impl Report {
    pub(crate) fn consumer(&self) -> &str {
        &self.consumer
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) const fn secs(&self) -> f64 {
        self.secs
    }

    pub(crate) const fn verdict(&self) -> &Verdict {
        &self.verdict
    }
}

struct Place<'a> {
    root: &'a Path,
    target: PathBuf,
    logs: &'a Path,
    jobs: usize,
    dir: PathBuf,
    crates: BTreeMap<String, PathBuf>,
}

struct Ran {
    exit: i32,
    what: String,
}

pub(crate) fn run(
    list: &Path,
    root: &Path,
    target: &Path,
    logs: &Path,
    jobs: usize,
    report: &mut dyn FnMut(Report),
) {
    let started = Instant::now();
    let found = read(list)
        .and_then(|text| parse(&text))
        .and_then(|consumers| Ok((consumers, crates(root)?)));
    let (consumers, crates) = match found {
        Ok(found) => found,
        Err(error) => {
            report(Report {
                consumer: "list".to_owned(),
                detail: list.display().to_string(),
                secs: started.elapsed().as_secs_f64(),
                verdict: Verdict::Broken(error),
            });
            return;
        }
    };
    assert!(!crates.is_empty(), "the worktree holds no package");
    let place = Place {
        root,
        target: target.join("downstream"),
        logs,
        jobs,
        dir: std::env::var_os(CLONES).map_or_else(
            || {
                let worktree = worktree_dir(&root.to_string_lossy());
                std::env::temp_dir()
                    .join("nanocode-downstream")
                    .join(worktree)
            },
            PathBuf::from,
        ),
        crates,
    };
    for consumer in &consumers {
        report(build(&place, consumer));
    }
}

fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        error,
    })
}

fn word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    text.split_once(char::is_whitespace)
        .map_or((text, ""), |(word, rest)| (word, rest.trim()))
}

fn parse(text: &str) -> Result<Vec<Consumer>, Error> {
    let mut consumers = Vec::new();
    for (at, line) in (1..).zip(text.lines()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let malformed = || Error::Line {
            line: at,
            text: line.to_owned(),
        };
        let (name, rest) = word(line);
        let (source, rest) = word(rest);
        let (how, rest) = word(rest);
        let named = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !named || name.starts_with('.') || source.is_empty() {
            return Err(malformed());
        }
        let how = match how {
            "cargo" if rest.is_empty() => How::Cargo(vec![".".to_owned()]),
            "cargo" => How::Cargo(rest.split_whitespace().map(str::to_owned).collect()),
            "run" if !rest.is_empty() => How::Run(rest.to_owned()),
            "pinned" if !rest.is_empty() => How::Pinned(rest.to_owned()),
            _ => return Err(malformed()),
        };
        consumers.push(Consumer {
            name: name.to_owned(),
            source: source.to_owned(),
            how,
        });
    }
    Ok(consumers)
}

fn package_name(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "[package]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "name").then(|| value.trim().trim_matches('"').to_owned())
        })
}

fn crates(root: &Path) -> Result<BTreeMap<String, PathBuf>, Error> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let io = |error| Error::Io {
            path: dir.clone(),
            error,
        };
        for entry in fs::read_dir(&dir).map_err(io)? {
            let path = entry.map_err(io)?.path();
            let Some(file) = path.file_name().and_then(|file| file.to_str()) else {
                continue;
            };
            if path.is_dir() {
                if !file.starts_with('.') && !SKIPPED.contains(&file) {
                    pending.push(path);
                }
                continue;
            }
            if file != "Cargo.toml" {
                continue;
            }
            let Some(name) = package_name(&read(&path)?) else {
                continue;
            };
            if let Some(first) = found.get(&name) {
                return Err(Error::Duplicate {
                    name,
                    first: first.clone(),
                    second: dir.clone(),
                });
            }
            found.insert(name, dir.clone());
        }
    }
    Ok(found)
}

fn upstream(lock: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut name = None;
    for line in lock.lines() {
        if line == "[[package]]" {
            name = None;
        } else if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"'));
        } else if let Some(value) = line.strip_prefix("source = ")
            && let Some(name) = name
            && value
                .trim_matches('"')
                .strip_prefix("git+")
                .and_then(|source| source.strip_prefix(UPSTREAM))
                .is_some_and(|rest| rest.starts_with(['?', '#']) || rest.starts_with(".git"))
        {
            names.insert(name.to_owned());
        }
    }
    names
}

fn patches(names: &BTreeSet<String>, crates: &BTreeMap<String, PathBuf>) -> (String, Vec<String>) {
    let entries: String = names
        .iter()
        .filter_map(|name| {
            let dir = crates.get(name)?.display().to_string();
            Some(format!("{name} = {{ path = {dir:?} }}\n"))
        })
        .collect();
    let absent: Vec<String> = names
        .iter()
        .filter(|name| !crates.contains_key(*name))
        .cloned()
        .collect();
    let config = format!("[patch.\"{UPSTREAM}\"]\n{entries}[patch.crates-io]\n{entries}");
    assert!(
        config.len() >= entries.len() * 2,
        "both patch tables hold every entry"
    );
    (config, absent)
}

fn code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT + signal))
        .unwrap_or(SIGNAL_EXIT)
}

fn status(mut command: Command, log: &File) -> Result<i32, Error> {
    let program = command.get_program().to_string_lossy().into_owned();
    let spawn = |error| Error::Spawn {
        program: program.clone(),
        error,
    };
    let (out, err) = (
        log.try_clone().map_err(spawn)?,
        log.try_clone().map_err(spawn)?,
    );
    command
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .status()
        .map(code)
        .map_err(spawn)
}

fn checkout(place: &Place<'_>, consumer: &Consumer, log: &File) -> Result<PathBuf, Error> {
    let source = consumer.source.as_str();
    if !source.contains("://") && !source.starts_with("git@") {
        let path = match (source.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => Path::new(&home).join(rest),
            _ => place.root.join(source),
        };
        return if path.is_dir() {
            Ok(path)
        } else {
            Err(Error::NoCheckout { path })
        };
    }
    let path = place.dir.join(&consumer.name);
    let at = path.to_string_lossy().into_owned();
    let steps: Vec<Vec<&str>> = if path.join(".git").is_dir() {
        vec![
            vec!["-C", &at, "fetch", "-q", "--depth", "1", "origin", "HEAD"],
            vec!["-C", &at, "reset", "-q", "--hard", "FETCH_HEAD"],
        ]
    } else {
        fs::create_dir_all(&place.dir).map_err(|error| Error::Io {
            path: place.dir.clone(),
            error,
        })?;
        vec![vec!["clone", "-q", "--depth", "1", source, &at]]
    };
    for args in steps {
        let mut command = Command::new("git");
        command.args(&args);
        let exit = status(command, log)?;
        if exit != 0 {
            return Err(Error::Git {
                args: args.join(" "),
                exit,
            });
        }
    }
    Ok(path)
}

fn cargo(
    place: &Place<'_>,
    name: &str,
    checkout: &Path,
    dirs: &[String],
    mut log: &File,
) -> Result<Ran, Error> {
    let mut exit = 0;
    let mut patched = 0;
    for (at, dir) in (0_u32..).zip(dirs) {
        let manifest = checkout.join(dir);
        let lock_path = manifest.join("Cargo.lock");
        let lock = read(&lock_path)?;
        let names = upstream(&lock);
        if names.is_empty() {
            return Err(Error::NoUpstream { lock: lock_path });
        }
        let (config, absent) = patches(&names, &place.crates);
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |error| Error::Io { path, error }
        };
        for missing in &absent {
            writeln!(log, "error: {missing} is not a package in this worktree")
                .map_err(io(&lock_path))?;
        }
        let config_path = place.logs.join(format!("downstream-{name}-{at}.toml"));
        fs::write(&config_path, config).map_err(io(&config_path))?;
        let mut command = Command::new("cargo");
        command
            .current_dir(&manifest)
            .args(["check", "--workspace", "--config"])
            .arg(&config_path)
            .env("CARGO_TARGET_DIR", place.target.join(name))
            .env("CARGO_BUILD_JOBS", place.jobs.to_string())
            .env("SHIPPY_REMOTE_BUILD", "0")
            .env("CARGO_TERM_COLOR", "never");
        let ran = status(command, log)?;
        fs::write(&lock_path, &lock).map_err(io(&lock_path))?;
        if exit == 0 {
            exit = ran;
        }
        patched += names.len() - absent.len();
    }
    Ok(Ran {
        exit,
        what: format!(
            "cargo check in {}, {patched} crates from this worktree",
            dirs.join(" ")
        ),
    })
}

fn shell(
    place: &Place<'_>,
    name: &str,
    checkout: &Path,
    script: &str,
    log: &File,
) -> Result<Ran, Error> {
    let mut command = Command::new("sh");
    command
        .current_dir(checkout)
        .args(["-c", script])
        .env("NANOCODE_WORKTREE", place.root)
        .env(CLONES, &place.dir)
        .env("CARGO_TARGET_DIR", place.target.join(name))
        .env("CARGO_BUILD_JOBS", place.jobs.to_string())
        .env("SHIPPY_REMOTE_BUILD", "0")
        .env("CARGO_TERM_COLOR", "never");
    Ok(Ran {
        exit: status(command, log)?,
        what: "its run command".to_owned(),
    })
}

fn errors(log: &str) -> u32 {
    let count = log
        .lines()
        .filter(|line| line.starts_with("error") || line.contains(": error TS"))
        .filter(|line| !line.starts_with("error: could not compile"))
        .filter(|line| !line.starts_with("error: aborting"))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn head(checkout: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "--short", "HEAD"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "(no git head)".to_owned(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
}

fn attempt(
    place: &Place<'_>,
    consumer: &Consumer,
    step: impl FnOnce(&Path, &File) -> Result<Ran, Error>,
) -> (String, Verdict) {
    let log_path = place.logs.join(format!("downstream-{}.log", consumer.name));
    let ran = File::create(&log_path)
        .map_err(|error| Error::Io {
            path: log_path.clone(),
            error,
        })
        .and_then(|log| {
            let checkout = checkout(place, consumer, &log)?;
            let ran = step(&checkout, &log)?;
            Ok((checkout, ran))
        });
    let (checkout, ran) = match ran {
        Ok(ran) => ran,
        Err(error) => return (consumer.name.clone(), Verdict::Broken(error)),
    };
    let detail = format!("{} {}: {}", consumer.name, head(&checkout), ran.what);
    let errors = errors(&fs::read_to_string(&log_path).unwrap_or_default());
    if ran.exit == 0 && errors == 0 {
        return (detail, Verdict::Built);
    }
    (
        detail,
        Verdict::Failed {
            errors,
            exit: ran.exit,
        },
    )
}

fn build(place: &Place<'_>, consumer: &Consumer) -> Report {
    let started = Instant::now();
    let (detail, verdict) = match &consumer.how {
        How::Pinned(reason) => (consumer.name.clone(), Verdict::Pinned(reason.clone())),
        How::Cargo(dirs) => attempt(place, consumer, |checkout, log| {
            cargo(place, &consumer.name, checkout, dirs, log)
        }),
        How::Run(script) => attempt(place, consumer, |checkout, log| {
            shell(place, &consumer.name, checkout, script, log)
        }),
    };
    assert!(!detail.is_empty(), "every report names its consumer");
    Report {
        consumer: consumer.name.clone(),
        detail,
        secs: started.elapsed().as_secs_f64(),
        verdict,
    }
}

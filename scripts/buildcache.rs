use std::{
    collections::BTreeMap,
    hash::Hasher,
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, Read, Write},
    net::TcpStream,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{PermissionsExt, symlink},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::SystemTime,
};

const RUST_KEY: &str = "nanocode-rustc-cache-6";
const WORKSPACE_KEY: &str = "nanocode-workspace-cache-2";
const C_KEY: &str = "nanocode-c-cache-3 -ffile-prefix-map=<out>=. -MD";
const MEMO: &str = ".nanocode-sha256";

type Environment = BTreeMap<OsString, OsString>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Registry,
    Workspace,
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let scripts = PathBuf::from(args.next().unwrap_or_default());
    let args: Vec<OsString> = args.collect();
    let Some(tool) = args.first() else {
        return ExitCode::from(2);
    };
    if let Some(stale) = stale_pointer(&scripts, &args) {
        return stale;
    }
    let cache = cache_dir();
    let environment: Environment = env::vars_os().collect();
    if tool == "publish" {
        return match args.get(1).map(|to| publish(&cache, Path::new(to))) {
            Some(Ok(())) => ExitCode::SUCCESS,
            Some(Err(error)) => {
                eprintln!("buildcache: could not publish: {error}");
                ExitCode::from(1)
            }
            None => ExitCode::from(2),
        };
    }
    if let Some(root) = cache.parent() {
        start_pending(&root.join("remote"), &scripts);
    }
    if tool == "fetch-pending" {
        return fetch_pending(&cache, Path::new(args.get(1).map_or(OsStr::new(""), OsString::as_os_str)), args.get(2).map(Path::new));
    }
    if is_rustc(tool)
        && let Some(answered) = probe(&cache, &args, &environment)
    {
        return answered;
    }
    if is_rustc(tool) {
        rust(&cache, &scripts, args, environment, 1)
    } else if args.get(1).is_some_and(|compiler| is_rustc(compiler)) {
        rust(&cache, &scripts, args, environment, 2)
    } else if environment.contains_key(OsStr::new("OUT_DIR")) {
        c(&cache, &args, environment)
    } else {
        exec(&args, &environment)
    }
}

fn is_rustc(path: &OsStr) -> bool {
    path.as_bytes().ends_with(b"rustc")
}

fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("nanocode/build")
}

fn exec(args: &[OsString], environment: &Environment) -> ExitCode {
    materialize(&cache_dir(), args);
    let error = Command::new(&args[0])
        .args(&args[1..])
        .env_clear()
        .envs(environment)
        .exec();
    eprintln!("buildcache: could not run {}: {error}", args[0].to_string_lossy());
    ExitCode::from(1)
}

fn probe(cache: &Path, args: &[OsString], environment: &Environment) -> Option<ExitCode> {
    let flags = &args[1..];
    let asks = (flags.len() == 1 && flags[0] == "-vV")
        || (flags.first().is_some_and(|first| first == "-")
            && flags.windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == "___"));
    if !asks || flags.iter().any(|flag| flag.as_bytes().windows(5).any(|window| window == b".json")) {
        return None;
    }
    let rustc = match (environment.get(OsStr::new("RUSTUP_HOME")), environment.get(OsStr::new("RUSTUP_TOOLCHAIN"))) {
        (Some(home), Some(name)) if !args[0].as_bytes().contains(&b'/') => {
            Path::new(home).join("toolchains").join(name).join("bin").join(&args[0])
        }
        _ => PathBuf::from(&args[0]),
    };
    if !rustc.as_os_str().as_bytes().windows(12).any(|window| window == b"/toolchains/") {
        return None;
    }
    let version = rustc_version(cache, &rustc)?;
    let mut hasher = Sha256::new();
    hasher.update(version.as_bytes());
    hasher.update(b"\n");
    hasher.update(rustc.as_os_str().as_bytes());
    for flag in flags {
        hasher.update(b"\n");
        hasher.update(flag.as_bytes());
    }
    for (name, value) in environment {
        let name = name.as_bytes();
        if name.starts_with(b"RUST") || name.ends_with(b"_DEPLOYMENT_TARGET") || name == b"SDKROOT" {
            hasher.update(b"\n");
            hasher.update(name);
            hasher.update(b"=");
            hasher.update(value.as_bytes());
        }
    }
    let memo = cache.join("rustc").join(format!("probe-{}", hasher.finish()));
    if let Ok(answer) = fs::read(&memo) {
        let mut stdout = io::stdout();
        return Some(if stdout.write_all(&answer).and_then(|()| stdout.flush()).is_ok() { ExitCode::SUCCESS } else { ExitCode::from(1) });
    }
    let output = Command::new(&args[0]).args(flags).stdin(Stdio::inherit()).output().ok()?;
    let mut stdout = io::stdout();
    let _ = stdout.write_all(&output.stdout).and_then(|()| stdout.flush());
    let _ = io::stderr().write_all(&output.stderr);
    if output.status.success() && output.stderr.is_empty() && fs::create_dir_all(cache.join("rustc")).is_ok() {
        let mut staging = memo.clone().into_os_string();
        staging.push(format!(".{}.{}", std::process::id(), unique()));
        if fs::write(&staging, &output.stdout).is_ok() && fs::rename(&staging, &memo).is_err() {
            let _ = fs::remove_file(&staging);
        }
    }
    Some(exit_code(output.status))
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(status.code().map_or(1, |code| code as u8))
}

fn rust(cache: &Path, scripts: &Path, mut args: Vec<OsString>, mut environment: Environment, mut compiler: usize) -> ExitCode {
    for name in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
        environment.remove(OsStr::new(name));
    }
    if let Some(out_dir) = environment.get(OsStr::new("OUT_DIR")).cloned()
        && Path::new(&out_dir).is_dir()
        && let Some(stable) = stable(cache, Path::new(&out_dir))
    {
        environment.insert("OUT_DIR".into(), stable.into_os_string());
    }
    let cargo_home = PathBuf::from(
        environment
            .get(OsStr::new("CARGO_HOME"))
            .cloned()
            .unwrap_or_else(|| {
                PathBuf::from(environment.get(OsStr::new("HOME")).cloned().unwrap_or_default())
                    .join(".cargo")
                    .into_os_string()
            }),
    );
    let registry = cargo_home.join("registry/src/");
    let Ok(cwd) = env::current_dir() else {
        return exec(&args, &environment);
    };
    let mut workspace_root = cwd.as_os_str().as_bytes().to_vec();
    workspace_root.push(b'/');
    let mut keyed: Vec<Vec<u8>> = Vec::with_capacity(args.len());
    let mut externs: Vec<Vec<u8>> = Vec::new();
    let (mut out_dir, mut suffix, mut kind, mut eligible) = (None, None, None, true);
    for index in compiler..args.len() {
        let previous = args[index - 1].as_bytes().to_vec();
        let arg = args[index].clone();
        let bytes = arg.as_bytes();
        let mut key = bytes.to_vec();
        match previous.as_slice() {
            b"-L" => {
                if let Some(at) = bytes.iter().position(|byte| *byte == b'=') {
                    let (prefix, path) = (&bytes[..at], &bytes[at + 1..]);
                    if prefix == b"dependency" {
                        key = b"dependency=".to_vec();
                    } else if is_build_out(path) && Path::new(OsStr::from_bytes(path)).is_dir()
                        && let Some(stable) = stable(cache, Path::new(OsStr::from_bytes(path)))
                    {
                        let mut rewritten = prefix.to_vec();
                        rewritten.push(b'=');
                        rewritten.extend_from_slice(stable.as_os_str().as_bytes());
                        key.clone_from(&rewritten);
                        args[index] = OsString::from_vec(rewritten);
                    }
                }
            }
            b"--out-dir" => {
                out_dir = Some(PathBuf::from(&arg));
                key.clear();
            }
            b"--extern" => {
                if let Some(at) = bytes.iter().position(|byte| *byte == b'=') {
                    let path = Path::new(OsStr::from_bytes(&bytes[at + 1..]));
                    key = bytes[..at].to_vec();
                    key.push(b'=');
                    key.extend_from_slice(path.file_name().map_or(b"" as &[u8], OsStrExt::as_bytes));
                    match identity(path) {
                        Some(identity) => {
                            let mut line = key.clone();
                            line.push(b' ');
                            line.extend_from_slice(identity.as_bytes());
                            externs.push(line);
                        }
                        None => eligible = false,
                    }
                }
            }
            b"-C" => {
                if let Some(value) = bytes.strip_prefix(b"extra-filename=") {
                    suffix = Some(value.to_vec());
                } else if bytes.starts_with(b"incremental=") {
                    key = b"incremental=".to_vec();
                }
            }
            _ => {}
        }
        if bytes.starts_with(b"@") {
            eligible = false;
        }
        if bytes.ends_with(b".rs") {
            if bytes.starts_with(registry.as_os_str().as_bytes()) {
                kind = Some(Kind::Registry);
            } else if !bytes.starts_with(b"/") || bytes.starts_with(&workspace_root) {
                kind = Some(Kind::Workspace);
            }
        }
        keyed.push(key);
    }
    if compiler == 1 && kind == Some(Kind::Workspace) {
        args.insert(0, scripts.join("rustc-parallel-frontend").into_os_string());
        compiler = 2;
    }
    let suffix = suffix.or_else(|| out_dir.as_ref().filter(|dir| is_build_out(dir.as_os_str().as_bytes())).map(|_| Vec::new()));
    let (Some(out_dir), Some(suffix), Some(kind), true) = (out_dir, suffix, kind, eligible) else {
        return exec(&args, &environment);
    };
    let toolchain = match (environment.get(OsStr::new("RUSTUP_HOME")), environment.get(OsStr::new("RUSTUP_TOOLCHAIN"))) {
        (Some(home), Some(name)) if !args[compiler - 1].as_bytes().contains(&b'/') => {
            Some(Path::new(home).join("toolchains").join(name).join("bin").join(&args[compiler - 1])).filter(|path| path.is_file())
        }
        _ => None,
    };
    let Some(version) = rustc_version(cache, toolchain.as_deref().unwrap_or(Path::new(&args[compiler - 1]))) else {
        return exec(&args, &environment);
    };
    let mut remaps = vec![
        (cache.as_os_str().as_bytes(), b"\x01cache\x01" as &[u8], b"/nanocode-build" as &[u8]),
        (cargo_home.as_os_str().as_bytes(), b"\x01cargo\x01", b"/cargo"),
    ];
    if kind == Kind::Workspace {
        remaps.push((cwd.as_os_str().as_bytes(), b"\x01workspace\x01", b"."));
    }
    let mut roots = Roots(remaps.iter().map(|(path, name, _)| (path.to_vec(), *name)).collect());
    roots.0.push((out_dir.as_os_str().as_bytes().to_vec(), b"\x01out\x01"));
    roots.0.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
    remaps.sort_by_key(|(path, _, _)| path.len());
    let remaps: Vec<OsString> = remaps
        .iter()
        .map(|(path, _, to)| {
            let mut flag = b"--remap-path-prefix=".to_vec();
            flag.extend_from_slice(path);
            flag.push(b'=');
            flag.extend_from_slice(to);
            OsString::from_vec(flag)
        })
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(if kind == Kind::Registry { RUST_KEY } else { WORKSPACE_KEY }.as_bytes());
    hasher.update(b"\n");
    hasher.update(version.as_bytes());
    hasher.update(b"\n");
    let mut clippy = false;
    for wrapper in &args[..compiler - 1] {
        let Some(digest) = tool_digest(cache, Path::new(wrapper)) else {
            return exec(&args, &environment);
        };
        clippy |= Path::new(wrapper)
            .file_name()
            .is_some_and(|name| name.as_bytes().windows(6).any(|window| window == b"clippy"));
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    for key in &keyed {
        hasher.update(&roots.hide(key));
        hasher.update(b"\n");
    }
    if kind == Kind::Registry {
        for line in &externs {
            hasher.update(line);
            hasher.update(b"\n");
        }
    }
    for (name, value) in &environment {
        let name = name.as_bytes();
        let relevant = (name.starts_with(b"CARGO_") && name != b"CARGO_MAKEFLAGS" && (clippy || name != b"CARGO_PRIMARY_PACKAGE"))
            || (clippy && name.starts_with(b"CLIPPY_"))
            || matches!(name, b"OUT_DIR" | b"RUSTC_BOOTSTRAP" | b"MACOSX_DEPLOYMENT_TARGET" | b"SDKROOT");
        let host = [&b"CARGO_HTTP_"[..], b"CARGO_NET_", b"CARGO_TERM_", b"CARGO_REGISTRIES_", b"CARGO_REGISTRY_", b"CARGO_CACHE_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            || matches!(name, b"CARGO_HOME" | b"CARGO_TARGET_DIR" | b"CARGO_BUILD_JOBS" | b"CARGO_LOG");
        if relevant && !host {
            hasher.update(name);
            hasher.update(b"=");
            if name != b"CARGO" {
                hasher.update(&roots.hide(value.as_bytes()));
            }
            hasher.update(b"\n");
        }
    }
    let key = hasher.finish();
    let inputs = match kind {
        Kind::Registry => Vec::new(),
        Kind::Workspace => {
            let manifest = environment
                .get(OsStr::new("CARGO_MANIFEST_PATH"))
                .map(PathBuf::from)
                .or_else(|| environment.get(OsStr::new("CARGO_MANIFEST_DIR")).map(|dir| Path::new(dir).join("Cargo.toml")));
            let mut inputs = Vec::new();
            for line in &externs {
                inputs.extend_from_slice(line);
                inputs.push(b'\n');
            }
            for path in manifest.iter().map(PathBuf::as_path).chain([cwd.join("Cargo.toml").as_path()]) {
                let Ok(digest) = digest_file(path) else {
                    return exec(&args, &environment);
                };
                inputs.extend_from_slice(digest.as_bytes());
                inputs.push(b' ');
                inputs.extend_from_slice(&roots.hide(path.as_os_str().as_bytes()));
                inputs.push(b'\n');
            }
            inputs
        }
    };
    let entry = match kind {
        Kind::Registry => cache.join("units").join(&key),
        Kind::Workspace => cache.join("workspace").join(&key),
    };
    let mut command = cwd.as_os_str().as_bytes().to_vec();
    for arg in args[..compiler].iter().chain(&remaps).chain(&args[compiler..]) {
        command.push(0);
        command.extend_from_slice(arg.as_bytes());
    }
    command.push(0);
    for (name, value) in &environment {
        if !matches!(name.as_bytes(), b"CARGO_MAKEFLAGS" | b"MAKEFLAGS" | b"MFLAGS") {
            command.push(0);
            command.extend_from_slice(name.as_bytes());
            command.push(b'=');
            command.extend_from_slice(value.as_bytes());
        }
    }
    if restore(&entry, &out_dir, &environment, kind, &inputs, &roots, &command).is_ok() {
        return ExitCode::SUCCESS;
    }
    if let Some(remote) = remote(cache, scripts, &environment)
        && fetch(cache, &remote, kind, &key, &entry).is_ok()
        && restore(&entry, &out_dir, &environment, kind, &inputs, &roots, &command).is_ok()
    {
        return ExitCode::SUCCESS;
    }
    for output in outputs(&out_dir, &suffix) {
        if let Some(file) = output.as_os_str().as_bytes().strip_suffix(b".nanocode-lazy") {
            let _ = fs::remove_file(OsStr::from_bytes(file));
            let _ = fs::remove_file(&output);
        }
    }
    materialize(cache, &args[compiler..]);
    args.splice(compiler..compiler, remaps);
    let started = SystemTime::now();
    let mut child = match Command::new(&args[0])
        .args(&args[1..])
        .env_clear()
        .envs(&environment)
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("buildcache: could not run {}: {error}", args[0].to_string_lossy());
            return ExitCode::from(1);
        }
    };
    let mut marker = Marker {
        kind,
        key: &key,
        metadata: None,
    };
    let mut captured = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let mut buffer = [0; 65536];
        let mut pending = Vec::new();
        let mut stderr = io::stderr();
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    pending.extend_from_slice(&buffer[..read]);
                    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        if let Some((path, emit)) = artifact(&line) {
                            marker.mark(&path, &emit);
                        }
                        let _ = stderr.write_all(&line);
                        let _ = stderr.flush();
                        captured.extend_from_slice(&line);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = stderr.write_all(&pending);
        let _ = stderr.flush();
        captured.extend_from_slice(&pending);
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(_) => return ExitCode::from(1),
    };
    if !status.success() {
        return exit_code(status);
    }
    let outputs = outputs(&out_dir, &suffix);
    let mut identities = Vec::new();
    for output in &outputs {
        let name = output.file_name().map_or(b"" as &[u8], OsStrExt::as_bytes);
        if name.ends_with(b".d") {
            continue;
        }
        if fresh_memo(output).is_none() {
            marker.mark(output, if name.ends_with(b".rmeta") { b"metadata" } else { b"link" });
        }
        if let Some(identity) = fresh_memo(output) {
            identities.extend_from_slice(name);
            identities.push(b' ');
            identities.extend_from_slice(identity.as_bytes());
            identities.push(b'\n');
        }
    }
    let Some(depinfo) = outputs
        .iter()
        .find(|path| path.extension() == Some(OsStr::new("d")))
        .and_then(|path| fs::read(path).ok())
    else {
        return ExitCode::SUCCESS;
    };
    let (dependencies, names) = dependencies(&depinfo);
    let stable_root = cache.join("out");
    let inside = |path: &[u8]| {
        path.starts_with(registry.as_os_str().as_bytes()) || path.starts_with(stable_root.as_os_str().as_bytes())
    };
    let (identity, sources) = match kind {
        Kind::Registry => {
            if !dependencies.iter().all(|path| inside(path)) {
                return ExitCode::SUCCESS;
            }
            (key.clone(), Vec::new())
        }
        Kind::Workspace => {
            let mut sources = Vec::new();
            for path in &dependencies {
                let local = !path.starts_with(b"/") || path.starts_with(&workspace_root);
                if !local && !inside(path) {
                    return ExitCode::SUCCESS;
                }
                if unchanged_since(Path::new(OsStr::from_bytes(path)), started).is_none() {
                    return ExitCode::SUCCESS;
                }
                let Ok(digest) = digest_file(Path::new(OsStr::from_bytes(path))) else {
                    return ExitCode::SUCCESS;
                };
                sources.extend_from_slice(digest.as_bytes());
                sources.push(b' ');
                sources.extend_from_slice(&roots.hide(path));
                sources.push(b'\n');
            }
            let mut hasher = Sha256::new();
            hasher.update(key.as_bytes());
            hasher.update(b"\n");
            hasher.update(&inputs);
            hasher.update(b"\n");
            hasher.update(&sources);
            (hasher.finish(), sources)
        }
    };
    let Some(recorded) = recorded_environment(&names, &environment, &roots) else {
        return ExitCode::SUCCESS;
    };
    let pinned = outputs.iter().any(|output| {
        output.extension() != Some(OsStr::new("d")) && fs::read(output).map_or(true, |data| roots.mentioned(&data))
    });
    let unit = Unit {
        roots: &roots,
        pinned,
        outputs: &outputs,
        captured: &captured,
        recorded: &recorded,
        inputs: &inputs,
        sources: &sources,
        identities: &identities,
        identity: &identity,
    };
    let _ = match kind {
        Kind::Registry => store(cache, &entry, &unit),
        Kind::Workspace => store_workspace(cache, &entry, &unit),
    };
    ExitCode::SUCCESS
}

struct Unit<'a> {
    roots: &'a Roots,
    pinned: bool,
    outputs: &'a [PathBuf],
    captured: &'a [u8],
    recorded: &'a [u8],
    inputs: &'a [u8],
    sources: &'a [u8],
    identities: &'a [u8],
    identity: &'a str,
}

struct Marker<'a> {
    kind: Kind,
    key: &'a str,
    metadata: Option<String>,
}

impl Marker<'_> {
    fn mark(&mut self, path: &Path, emit: &[u8]) {
        let name = path.file_name().map_or(b"" as &[u8], OsStrExt::as_bytes);
        let identity = match (self.kind, emit) {
            (_, b"dep-info") => return,
            (Kind::Registry, _) => self.key.to_owned(),
            (Kind::Workspace, b"metadata") => {
                let Ok(digest) = digest_file(path) else {
                    return;
                };
                self.metadata = Some(digest.clone());
                digest
            }
            (Kind::Workspace, _) if name.ends_with(b".rlib") && self.metadata.is_some() => {
                self.metadata.clone().unwrap_or_default()
            }
            (Kind::Workspace, _) if name.starts_with(b"lib") => {
                let Ok(digest) = digest_file(path) else {
                    return;
                };
                digest
            }
            (Kind::Workspace, _) => return,
        };
        write_memo(path, &identity);
    }
}

fn artifact(line: &[u8]) -> Option<(PathBuf, Vec<u8>)> {
    if !line.windows(29).any(|window| window == br#""$message_type":"artifact""#) {
        return None;
    }
    let path = json_string(line, br#""artifact":""#)?;
    let emit = json_string(line, br#""emit":""#)?;
    Some((PathBuf::from(OsString::from_vec(path)), emit))
}

fn json_string(line: &[u8], field: &[u8]) -> Option<Vec<u8>> {
    let start = line.windows(field.len()).position(|window| window == field)? + field.len();
    let mut value = Vec::new();
    let mut bytes = line[start..].iter();
    while let Some(&byte) = bytes.next() {
        match byte {
            b'"' => return Some(value),
            b'\\' => match bytes.next()? {
                escaped @ (b'"' | b'\\' | b'/') => value.push(*escaped),
                _ => return None,
            },
            _ => value.push(byte),
        }
    }
    None
}

fn is_build_out(path: &[u8]) -> bool {
    path.ends_with(b"/out") && path.windows(7).any(|window| window == b"/build/")
}

fn outputs(out_dir: &Path, suffix: &[u8]) -> Vec<PathBuf> {
    let mut outputs: Vec<PathBuf> = fs::read_dir(out_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.as_bytes();
            (suffix.is_empty() || name.windows(suffix.len()).any(|window| window == suffix))
                && !name.windows(MEMO.len()).any(|window| window == MEMO.as_bytes())
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .map(|entry| entry.path())
        .collect();
    outputs.sort();
    outputs
}

fn dependencies(depinfo: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let (mut paths, mut names) = (Vec::new(), Vec::new());
    for line in depinfo.split(|byte| *byte == b'\n') {
        if let Some(name) = line.strip_prefix(b"# env-dep:") {
            let end = name.iter().position(|byte| *byte == b'=').unwrap_or(name.len());
            names.push(name[..end].to_vec());
        } else if !line.starts_with(b"#") && line.ends_with(b":") && !line.ends_with(b"\\:") {
            paths.push(relocate(&line[..line.len() - 1], b"\\ ", b" "));
        }
    }
    paths.sort();
    paths.dedup();
    names.sort();
    names.dedup();
    (paths, names)
}

fn recorded_environment(names: &[Vec<u8>], environment: &Environment, roots: &Roots) -> Option<Vec<u8>> {
    let mut recorded = Vec::new();
    for name in names {
        recorded.extend_from_slice(name);
        if let Some(value) = environment.get(OsStr::from_bytes(name)) {
            if value.as_bytes().contains(&b'\n') {
                return None;
            }
            recorded.push(b'=');
            recorded.extend_from_slice(&roots.hide(value.as_bytes()));
        }
        recorded.push(b'\n');
    }
    Some(recorded)
}

fn memo_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(MEMO);
    PathBuf::from(name)
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

fn unchanged_since(path: &Path, started: SystemTime) -> Option<()> {
    (modified(path)? < started).then_some(())
}

fn fresh_memo(path: &Path) -> Option<String> {
    let memo = memo_path(path);
    let (file, memo_time) = (modified(path)?, modified(&memo)?);
    let text = fs::read_to_string(&memo).ok()?;
    let line = text.lines().next()?;
    (file <= memo_time && line.len() == 64).then(|| line.to_owned())
}

fn identity(path: &Path) -> Option<String> {
    if let Some(identity) = fresh_memo(path) {
        return Some(identity);
    }
    let digest = digest_file(path).ok()?;
    write_memo(path, &digest);
    Some(digest)
}

fn write_memo(path: &Path, digest: &str) {
    let memo = memo_path(path);
    let mut staging = memo.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    if fs::write(&staging, format!("{digest}\n")).is_ok() && fs::rename(&staging, &memo).is_err() {
        let _ = fs::remove_file(&staging);
    }
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(&fs::read(path)?);
    Ok(hasher.finish())
}

fn tool_digest(cache: &Path, tool: &Path) -> Option<String> {
    let mut name: Vec<u8> = tool
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| if matches!(byte, b'/' | b' ') { b'_' } else { *byte })
        .collect();
    name.extend_from_slice(b".sha256");
    let memo = cache.join("rustc").join(OsStr::from_bytes(&name));
    if let (Some(binary), Some(memo_time)) = (modified(tool), modified(&memo))
        && binary <= memo_time
        && let Ok(text) = fs::read_to_string(&memo)
        && let Some(line) = text.lines().next()
        && line.len() == 64
    {
        return Some(line.to_owned());
    }
    let digest = digest_file(tool).ok()?;
    let _ = fs::create_dir_all(cache.join("rustc"));
    let mut staging = memo.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    if fs::write(&staging, format!("{digest}\n")).is_ok() {
        let _ = fs::rename(&staging, &memo);
    }
    Some(digest)
}

fn rustc_version(cache: &Path, rustc: &Path) -> Option<String> {
    let path = rustc.as_os_str().as_bytes();
    let memo = path
        .windows(12)
        .any(|window| window == b"/toolchains/")
        .then(|| {
            let name: Vec<u8> = path
                .iter()
                .map(|byte| if matches!(byte, b'/' | b' ') { b'_' } else { *byte })
                .collect();
            cache.join("rustc").join(OsStr::from_bytes(&name))
        });
    if let Some(memo) = &memo
        && let (Some(binary), Some(memo_time)) = (modified(rustc), modified(memo))
        && binary <= memo_time
        && let Ok(text) = fs::read_to_string(memo)
        && let Some(line) = text.lines().next()
    {
        return Some(line.to_owned());
    }
    let output = Command::new(rustc).arg("-vV").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(&output.stdout);
    let version = hasher.finish();
    if let Some(memo) = memo {
        let _ = fs::create_dir_all(cache.join("rustc"));
        let mut staging = memo.clone().into_os_string();
        staging.push(format!(".{}", std::process::id()));
        if fs::write(&staging, format!("{version}\n")).is_ok() {
            let _ = fs::rename(&staging, &memo);
        }
    }
    Some(version)
}

fn stable(cache: &Path, directory: &Path) -> Option<PathBuf> {
    let mut entries = Vec::new();
    walk(directory, Path::new(""), &mut entries).ok()?;
    let renames = object_renames(directory, &entries);
    let mut canonical = Vec::with_capacity(entries.len());
    for (relative, is_file) in entries {
        let name = renames.iter().find(|(from, _)| *from == relative).map_or_else(|| relative.clone(), |(_, to)| to.clone());
        let content = if is_file && !renames.is_empty() && relative.extension() == Some(OsStr::new("a")) {
            Some(rename_members(&fs::read(directory.join(&relative)).ok()?, &renames)?)
        } else {
            None
        };
        canonical.push((name, relative, is_file, content));
    }
    canonical.sort_by(|left, right| left.0.cmp(&right.0));
    if canonical.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    let mut hasher = Sha256::new();
    for (name, relative, is_file, content) in &canonical {
        hasher.update(name.as_os_str().as_bytes());
        if *is_file {
            let digest = match content {
                Some(content) => {
                    let mut digest = Sha256::new();
                    digest.update(content);
                    digest.finish()
                }
                None => digest_file(&directory.join(relative)).ok()?,
            };
            hasher.update(b" ");
            hasher.update(digest.as_bytes());
        }
        hasher.update(b"\n");
    }
    let stable = cache.join("out").join(hasher.finish());
    if !stable.exists() {
        fs::create_dir_all(cache.join("out")).ok()?;
        let staging = cache.join("out").join(format!(".data.{}.{}", std::process::id(), unique()));
        let copied = (|| -> io::Result<()> {
            fs::create_dir(&staging)?;
            for (name, relative, is_file, content) in &canonical {
                match (is_file, content) {
                    (false, _) => fs::create_dir_all(staging.join(name))?,
                    (true, Some(content)) => fs::write(staging.join(name), content)?,
                    (true, None) => clone(&directory.join(relative), &staging.join(name))?,
                }
            }
            Ok(())
        })();
        if copied.is_err() || staging.file_name().is_none_or(|name| symlink(name, &stable).is_err()) {
            let _ = fs::remove_dir_all(&staging);
        }
    }
    stable.exists().then_some(stable)
}

fn object_renames(directory: &Path, entries: &[(PathBuf, bool)]) -> Vec<(PathBuf, PathBuf)> {
    const EXTENSIONS: [&str; 10] = ["c", "cc", "cpp", "cxx", "C", "s", "S", "asm", "m", "mm"];
    let prefix_of = |dirname: &str, extension: &str| {
        let mut hasher = std::hash::DefaultHasher::new();
        hasher.write(dirname.as_bytes());
        hasher.write(extension.as_bytes());
        format!("{:016x}", hasher.finish())
    };
    let mut renames = Vec::new();
    for (relative, is_file) in entries {
        let Some(name) = relative.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let (prefix, rest) = name.split_at(name.len().min(16));
        if !is_file || !name.ends_with(".o") || !rest.starts_with('-') || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let parent = relative.parent().unwrap_or(Path::new(""));
        let absolute = if parent.as_os_str().is_empty() { directory.to_path_buf() } else { directory.join(parent) };
        let (Some(absolute), Some(local)) = (absolute.to_str(), parent.to_str()) else {
            continue;
        };
        if let Some(extension) = EXTENSIONS.iter().find(|extension| prefix_of(absolute, extension) == prefix) {
            renames.push((relative.clone(), parent.join(format!("{}{rest}", prefix_of(local, extension)))));
        }
    }
    renames
}

fn rename_members(archive: &[u8], renames: &[(PathBuf, PathBuf)]) -> Option<Vec<u8>> {
    let mut archive = archive.to_vec();
    if !archive.starts_with(b"!<arch>\n") {
        return None;
    }
    let mut offset = 8;
    while offset + 60 <= archive.len() {
        let header = &archive[offset..offset + 60];
        let size: usize = std::str::from_utf8(&header[48..58]).ok()?.trim().parse().ok()?;
        let data = offset + 60;
        if data + size > archive.len() {
            return None;
        }
        if let Some(length) = header.strip_prefix(b"#1/") {
            let length: usize = std::str::from_utf8(&length[..13]).ok()?.trim().parse().ok()?;
            let stored = &archive[data..data + length.min(size)];
            let name = stored[..stored.iter().position(|byte| *byte == 0).unwrap_or(stored.len())].to_vec();
            if let Some((_, to)) = renames.iter().find(|(from, _)| from.file_name().map(OsStrExt::as_bytes) == Some(name.as_slice())) {
                let to = to.file_name()?.as_bytes();
                if to.len() != name.len() {
                    return None;
                }
                archive[data..data + name.len()].copy_from_slice(to);
            }
        } else if header.starts_with(b"//") {
            for (from, to) in renames {
                let (from, to) = (from.file_name()?.as_bytes(), to.file_name()?.as_bytes());
                let region = relocate(&archive[data..data + size], from, to);
                archive[data..data + size].copy_from_slice(&region);
            }
        }
        offset = data + size + size % 2;
    }
    Some(archive)
}

fn unique() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

fn walk(root: &Path, relative: &Path, entries: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            entries.push((path.clone(), false));
            walk(root, &path, entries)?;
        } else {
            entries.push((path, true));
        }
    }
    Ok(())
}

fn clone(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        fs::remove_file(to)?;
    }
    fs::copy(from, to)?;
    Ok(())
}

fn touch(path: &Path) -> io::Result<()> {
    fs::File::options().write(true).open(path)?.set_modified(SystemTime::now())
}

struct Roots(Vec<(Vec<u8>, &'static [u8])>);

impl Roots {
    fn at(text: &[u8], from: &[u8]) -> bool {
        text.starts_with(from)
            && text.get(from.len()).is_none_or(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    }

    fn swap(&self, text: &[u8], show: bool) -> Vec<u8> {
        let mut result = Vec::with_capacity(text.len());
        let mut index = 0;
        while index < text.len() {
            let found = self.0.iter().find_map(|(path, name)| {
                let (from, to) = if show { (*name, path.as_slice()) } else { (path.as_slice(), *name) };
                Self::at(&text[index..], from).then_some((from.len(), to))
            });
            match found {
                Some((length, to)) => {
                    result.extend_from_slice(to);
                    index += length;
                }
                None => {
                    result.push(text[index]);
                    index += 1;
                }
            }
        }
        result
    }

    fn hide(&self, text: &[u8]) -> Vec<u8> {
        self.swap(text, false)
    }

    fn show(&self, text: &[u8]) -> Vec<u8> {
        self.swap(text, true)
    }

    fn mentioned(&self, data: &[u8]) -> bool {
        let mut index = 0;
        while let Some(at) = data[index..].iter().position(|byte| *byte == b'/') {
            index += at;
            if self.0.iter().any(|(path, _)| Self::at(&data[index..], path)) {
                return true;
            }
            index += 1;
        }
        false
    }

    fn list(&self) -> Vec<u8> {
        let mut list = Vec::new();
        for (path, name) in &self.0 {
            list.extend_from_slice(name);
            list.extend_from_slice(path);
            list.push(b'\n');
        }
        list
    }
}

fn relocate(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return text.to_vec();
    }
    let mut result = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index..].starts_with(from) {
            result.extend_from_slice(to);
            index += from.len();
        } else {
            result.push(text[index]);
            index += 1;
        }
    }
    result
}

fn same_environment(entry: &Path, environment: &Environment, roots: &Roots) -> bool {
    let Ok(recorded) = fs::read(entry.join("env")) else {
        return true;
    };
    recorded.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).all(|line| {
        match line.iter().position(|byte| *byte == b'=') {
            Some(at) => environment
                .get(OsStr::from_bytes(&line[..at]))
                .is_some_and(|value| roots.hide(value.as_bytes()) == &line[at + 1..]),
            None => !environment.contains_key(OsStr::from_bytes(line)),
        }
    })
}

fn sources_match(sources: &[u8], roots: &Roots) -> bool {
    sources.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).all(|line| {
        line.iter().position(|byte| *byte == b' ').is_some_and(|at| {
            digest_file(Path::new(OsStr::from_bytes(&roots.show(&line[at + 1..]))))
                .is_ok_and(|digest| digest.as_bytes() == &line[..at])
        })
    })
}

fn restore(
    entry: &Path,
    out_dir: &Path,
    environment: &Environment,
    kind: Kind,
    inputs: &[u8],
    roots: &Roots,
    command: &[u8],
) -> io::Result<()> {
    let entry = &fs::canonicalize(entry)?;
    if kind == Kind::Workspace
        && (fs::read(entry.join("inputs"))? != inputs || !sources_match(&fs::read(entry.join("sources"))?, roots))
    {
        return Err(io::Error::other("inputs changed"));
    }
    if fs::read(entry.join("roots")).is_ok_and(|pinned| pinned != roots.list()) {
        return Err(io::Error::other("roots changed"));
    }
    let identities = fs::read(entry.join("identities"))?;
    if !same_environment(entry, environment, roots) {
        return Err(io::Error::other("environment changed"));
    }
    let mut files: Vec<_> = fs::read_dir(entry.join("files"))?.collect::<io::Result<_>>()?;
    files.sort_by_key(fs::DirEntry::file_name);
    let lazy = fs::read(entry.join("lazy")).unwrap_or_default();
    let lazy: Vec<&[u8]> = lazy
        .split(|byte| *byte == b'\n')
        .filter(|name| !name.is_empty() && !entry.join("files").join(OsStr::from_bytes(name)).exists())
        .collect();
    let source = if lazy.is_empty() || entry.parent().and_then(Path::parent).is_some_and(|cache| cache.join("lazy.failed").exists()) {
        None
    } else {
        Some(fs::read(entry.join("source"))?)
    };
    if !lazy.is_empty() && (source.is_none() || !lazy.iter().all(|name| is_library(name))) {
        return Err(io::Error::other("not fetched"));
    }
    for file in &files {
        let name = file.file_name();
        let destination = out_dir.join(&name);
        if name.as_bytes().ends_with(b".d") {
            fs::write(&destination, roots.show(&fs::read(file.path())?))?;
        } else {
            if fs::symlink_metadata(&destination).is_ok() {
                fs::remove_file(&destination)?;
            }
            if fs::hard_link(file.path(), &destination).is_err() {
                fs::copy(file.path(), &destination)?;
            }
            touch(&destination)?;
        }
    }
    for name in &lazy {
        let destination = out_dir.join(OsStr::from_bytes(name));
        if fs::symlink_metadata(&destination).is_ok() {
            fs::remove_file(&destination)?;
        }
        fs::write(&destination, b"")?;
        let mut sidecar = source.clone().unwrap_or_default();
        sidecar.push(b'\n');
        sidecar.extend_from_slice(command);
        fs::write(lazy_path(&destination), sidecar)?;
    }
    for line in identities.split(|byte| *byte == b'\n') {
        if let Some(at) = line.iter().position(|byte| *byte == b' ') {
            write_memo(&out_dir.join(OsStr::from_bytes(&line[..at])), &String::from_utf8_lossy(&line[at + 1..]));
        }
    }
    let stderr = fs::read(entry.join("stderr"))?;
    let mut output = io::stderr();
    output.write_all(&roots.show(&stderr))?;
    output.flush()
}

fn stage(cache: &Path, directory: &str, unit: &Unit<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(cache.join(directory))?;
    let staging = cache.join(directory).join(format!(".data.{}.{}", std::process::id(), unique()));
    let result = (|| -> io::Result<()> {
        fs::create_dir_all(staging.join("files"))?;
        for output in unit.outputs {
            if let Some(name) = output.file_name() {
                if output.extension() == Some(OsStr::new("d")) {
                    fs::write(staging.join("files").join(name), unit.roots.hide(&fs::read(output)?))?;
                } else {
                    clone(output, &staging.join("files").join(name))?;
                }
            }
        }
        if unit.pinned {
            fs::write(staging.join("roots"), unit.roots.list())?;
        }
        fs::write(staging.join("env"), unit.recorded)?;
        fs::write(staging.join("stderr"), unit.roots.hide(unit.captured))?;
        fs::write(staging.join("inputs"), unit.inputs)?;
        fs::write(staging.join("sources"), unit.sources)?;
        fs::write(staging.join("identities"), unit.identities)
    })();
    match result {
        Ok(()) => Ok(staging),
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

fn store(cache: &Path, entry: &Path, unit: &Unit<'_>) -> io::Result<()> {
    let staging = stage(cache, "units", unit)?;
    symlink(staging.file_name().unwrap_or_default(), entry).inspect_err(|_| {
        let _ = fs::remove_dir_all(&staging);
    })
}

fn store_workspace(cache: &Path, entry: &Path, unit: &Unit<'_>) -> io::Result<()> {
    let (Some(directory), Some(slot)) = (entry.parent(), entry.file_name()) else {
        return Ok(());
    };
    let mut prefix = slot.as_bytes().to_vec();
    prefix.push(b'.');
    let mut version = prefix.clone();
    version.extend_from_slice(unit.identity.as_bytes());
    let version = directory.join(OsStr::from_bytes(&version));
    if !version.exists() {
        let staging = stage(cache, "workspace", unit)?;
        if fs::rename(&staging, &version).is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
    }
    point(directory, entry, &version, &prefix)
}

fn point(directory: &Path, entry: &Path, version: &Path, prefix: &[u8]) -> io::Result<()> {
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(directory.join(".point.lock"))?;
    lock.lock()?;
    let link = directory.join(format!(".link.{}.{}", std::process::id(), unique()));
    symlink(version.file_name().unwrap_or_default(), &link)?;
    if let Err(error) = fs::rename(&link, entry) {
        let _ = fs::remove_file(&link);
        return Err(error);
    }
    let current = fs::read_link(entry).ok();
    for sibling in fs::read_dir(directory)?.flatten() {
        let path = sibling.path();
        if sibling.file_name().as_bytes().starts_with(prefix) && current.as_deref() != Some(Path::new(&sibling.file_name())) {
            let _ = fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

fn directory_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Registry => "units",
        Kind::Workspace => "workspace",
    }
}

fn is_library(name: &[u8]) -> bool {
    name.ends_with(b".rlib") || name.ends_with(b".rmeta")
}

fn lazy_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".nanocode-lazy");
    PathBuf::from(name)
}

fn url_of(template: &[u8], directory: &str, key: &str) -> Vec<u8> {
    let template = String::from_utf8_lossy(template);
    if template.contains("{key}") {
        template.replace("{dir}", directory).replace("{key}", key).into_bytes()
    } else {
        let mut url = template.trim_end_matches('/').as_bytes().to_vec();
        url.extend_from_slice(format!("/{directory}/{key}").as_bytes());
        url
    }
}

fn indexed(cache: &Path, directory: &str, key: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let root = cache.parent()?;
    if root.join("build/lazy.failed").exists() {
        return None;
    }
    let source = url_of(fs::read(root.join("remote/url")).ok()?.trim_ascii(), directory, key);
    let index = fs::read(root.join("remote/workspace/index")).ok()?;
    let prefix = format!("{directory}/{key}/");
    let mut rest = index.as_slice();
    let mut packed = Vec::new();
    while !rest.is_empty() {
        let name_end = rest.iter().position(|byte| *byte == b'\n')?;
        let name = &rest[..name_end];
        let header = &rest[name_end + 1..];
        let size_end = header.iter().position(|byte| *byte == b'\n')?;
        let size: usize = std::str::from_utf8(header[..size_end].split(|byte| *byte == b' ').next()?).ok()?.parse().ok()?;
        let end = name_end + 1 + size_end + 1 + size;
        if let Some(relative) = name.strip_prefix(prefix.as_bytes()) {
            packed.extend_from_slice(relative);
            packed.extend_from_slice(&rest[name_end..end]);
        }
        rest = rest.get(end..)?;
    }
    (!packed.is_empty()).then_some((packed, source))
}

fn pack_path(cache: &Path, directory: &str, key: &str) -> Option<PathBuf> {
    Some(cache.parent()?.join("remote").join(directory).join(key))
}

fn eager_pack(cache: &Path, directory: &str, key: &str, source: &[u8]) -> io::Result<Vec<u8>> {
    let Some(path) = pack_path(cache, directory, key).filter(|path| path.parent().is_some_and(Path::is_dir)) else {
        return unpacked(download(source)?);
    };
    let mut name = path.clone().into_os_string();
    name.push(".lock");
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&name)?;
    lock.lock()?;
    if let Ok(data) = fs::read(&path) {
        return unpacked(data);
    }
    let data = unpacked(download(source)?)?;
    let mut staging = path.clone().into_os_string();
    staging.push(format!(".{}.{}", std::process::id(), unique()));
    if fs::write(&staging, &data).and_then(|()| fs::rename(&staging, &path)).is_err() {
        let _ = fs::remove_file(&staging);
    }
    Ok(data)
}

fn stale_pointer(scripts: &Path, args: &[OsString]) -> Option<ExitCode> {
    let helper = env::current_exe().ok()?;
    let checksum: u32 = helper.file_name()?.to_str()?.strip_prefix("buildcache-")?.parse().ok()?;
    let source = fs::read(scripts.join("buildcache.rs")).ok()?;
    let pointer = scripts.join(".buildcache");
    if cksum(&source) == checksum {
        if fs::metadata(&pointer).is_err() {
            let staging = scripts.join(format!(".buildcache.{}.{}", std::process::id(), unique()));
            if symlink(&helper, &staging).is_ok() && fs::rename(&staging, &pointer).is_err() {
                let _ = fs::remove_file(&staging);
            }
        }
        return None;
    }
    if fs::read_link(&pointer).is_ok_and(|target| target == helper) {
        let _ = fs::remove_file(&pointer);
    }
    let error = Command::new(scripts.join("buildcache")).args(args).exec();
    eprintln!("buildcache: could not run {}: {error}", scripts.join("buildcache").display());
    Some(ExitCode::from(1))
}

fn cksum(data: &[u8]) -> u32 {
    let step = |crc: u32, byte: u8| {
        (0..8).fold(crc ^ (u32::from(byte) << 24), |crc, _| if crc & 0x8000_0000 == 0 { crc << 1 } else { (crc << 1) ^ 0x04c1_1db7 })
    };
    let length = data.len().to_le_bytes();
    let significant = length.iter().rposition(|byte| *byte != 0).map_or(0, |at| at + 1);
    !data.iter().chain(&length[..significant]).fold(0, |crc, byte| step(crc, *byte))
}

fn start_pending(directory: &Path, scripts: &Path) {
    let pending = directory.join("pending");
    if !pending.exists() {
        return;
    }
    let claimed = directory.join(format!("pending.{}.{}", std::process::id(), unique()));
    if fs::rename(&pending, &claimed).is_err() {
        return;
    }
    let started = env::current_exe().and_then(|helper| {
        Command::new(helper)
            .arg(scripts)
            .arg("fetch-pending")
            .arg(&claimed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    });
    if started.is_err() {
        let _ = fs::remove_file(&claimed);
    }
}

fn fetch_pending(cache: &Path, list: &Path, bundle: Option<&Path>) -> ExitCode {
    let names = fs::read(list).unwrap_or_default();
    let _ = fs::remove_file(list);
    let Some(template) = cache.parent().and_then(|root| fs::read(root.join("remote/url")).ok()) else {
        return ExitCode::from(1);
    };
    let template = template.trim_ascii();
    let pending: Vec<(&str, &str)> = names
        .split(|byte| *byte == b'\n')
        .filter_map(|name| {
            let at = name.iter().position(|byte| *byte == b'-')?;
            Some((std::str::from_utf8(&name[..at]).ok()?, std::str::from_utf8(&name[at + 1..]).ok()?))
        })
        .collect();
    let mut following = bundle.and_then(|bundle| {
        Some(io::BufReader::new(Following { packs: fs::File::open(bundle.join("packs")).ok()?, done: Some(bundle.join("done")) }))
    });
    if let Some(following) = following.as_mut() {
        bundled(cache, &pending, following);
    }
    std::thread::scope(|scope| {
        for (directory, key) in pending {
            if pack_path(cache, directory, key).is_some_and(|path| path.exists()) {
                continue;
            }
            scope.spawn(move || eager_pack(cache, directory, key, &url_of(template, directory, key)));
        }
    });
    if let Some(done) = following.and_then(|following| following.into_inner().done) {
        let _ = fs::File::open(done);
    }
    if let Some(bundle) = bundle {
        let _ = fs::remove_dir_all(bundle);
    }
    ExitCode::SUCCESS
}

struct Following {
    packs: fs::File,
    done: Option<PathBuf>,
}

impl Read for Following {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.packs.read(buffer)?;
            if read > 0 || buffer.is_empty() {
                return Ok(read);
            }
            let Some(done) = self.done.take() else {
                return Ok(0);
            };
            fs::File::open(done)?;
        }
    }
}

fn bundled(cache: &Path, pending: &[(&str, &str)], bundle: &mut io::BufReader<Following>) {
    let mut held: Vec<(Vec<u8>, PathBuf, fs::File, Option<(OsString, fs::File, u64)>)> = pending
        .iter()
        .filter_map(|(directory, key)| {
            let path = pack_path(cache, directory, key).filter(|path| path.parent().is_some_and(Path::is_dir))?;
            let mut name = path.clone().into_os_string();
            name.push(".lock");
            let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&name).ok()?;
            lock.try_lock().ok()?;
            (!path.exists()).then(|| (format!("{directory}-{key}").into_bytes(), path, lock, None))
        })
        .collect();
    while !held.is_empty() {
        let (mut name, mut size) = (Vec::new(), Vec::new());
        if bundle.read_until(b'\n', &mut name).unwrap_or(0) == 0 || bundle.read_until(b'\n', &mut size).unwrap_or(0) == 0 {
            break;
        }
        let Some((length, total)) = std::str::from_utf8(size.trim_ascii()).ok().and_then(|size| {
            let (length, total) = size.split_once(' ').unwrap_or((size, size));
            Some((length.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
        }) else {
            break;
        };
        let mut entry = (&mut *bundle).take(length);
        let Some(at) = held.iter().position(|(held, ..)| held.as_slice() == name.trim_ascii()) else {
            if io::copy(&mut entry, &mut io::sink()).ok() != Some(length) {
                break;
            }
            continue;
        };
        let (_, path, _, partial) = &mut held[at];
        if partial.is_none() {
            let mut staging = path.clone().into_os_string();
            staging.push(format!(".{}.{}", std::process::id(), unique()));
            let Ok(file) = fs::File::create(&staging) else {
                break;
            };
            *partial = Some((staging, file, 0));
        }
        let Some((staging, file, written)) = partial.as_mut() else {
            break;
        };
        if io::copy(&mut entry, file).ok() != Some(length) {
            break;
        }
        *written += length;
        if *written < total {
            continue;
        }
        if *written > total || fs::rename(&*staging, &*path).is_err() {
            break;
        }
        held.swap_remove(at);
    }
    for (.., partial) in held {
        if let Some((staging, ..)) = partial {
            let _ = fs::remove_file(staging);
        }
    }
}

fn fetch(cache: &Path, remote: &OsStr, kind: Kind, key: &str, entry: &Path) -> io::Result<()> {
    let directory = directory_of(kind);
    let (packed, source) = match download(&url_of(remote.as_bytes(), directory, key)) {
        Ok(data) => (unpacked(data)?, None),
        Err(error) => indexed(cache, directory, key).map(|(packed, source)| (packed, Some(source))).ok_or(error)?,
    };
    fs::create_dir_all(cache.join(directory))?;
    let staging = cache.join(directory).join(format!(".data.{}.{}", std::process::id(), unique()));
    let placed = unpack(&packed, &staging).and_then(|()| {
        let Some(source) = source else {
            return Ok(());
        };
        let lazy = fs::read(staging.join("lazy"))?;
        if lazy.split(|byte| *byte == b'\n').all(|name| name.is_empty() || is_library(name)) {
            return fs::write(staging.join("source"), source);
        }
        unpack(&eager_pack(cache, directory, key, &source)?, &staging)
    });
    let placed = placed.and_then(|()| match kind {
        Kind::Registry => symlink(staging.file_name().unwrap_or_default(), entry),
        Kind::Workspace => {
            let (Some(parent), Some(slot)) = (entry.parent(), entry.file_name()) else {
                return Err(io::Error::other("no slot"));
            };
            let mut hasher = Sha256::new();
            hasher.update(key.as_bytes());
            hasher.update(b"\n");
            hasher.update(&fs::read(staging.join("inputs"))?);
            hasher.update(b"\n");
            hasher.update(&fs::read(staging.join("sources"))?);
            let mut prefix = slot.as_bytes().to_vec();
            prefix.push(b'.');
            let mut version = prefix.clone();
            version.extend_from_slice(hasher.finish().as_bytes());
            let version = parent.join(OsStr::from_bytes(&version));
            if fs::rename(&staging, &version).is_err() && !version.exists() {
                return Err(io::Error::other("could not place"));
            }
            point(parent, entry, &version, &prefix)
        }
    });
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    placed
}

fn remote(cache: &Path, scripts: &Path, environment: &Environment) -> Option<OsString> {
    match environment.get(OsStr::new("NANOCODE_BUILDCACHE_URL")) {
        Some(remote) if remote == "off" => None,
        Some(remote) if !remote.is_empty() => Some(remote.clone()),
        _ => prefetched(cache, scripts, environment),
    }
}

fn prefetched(cache: &Path, scripts: &Path, environment: &Environment) -> Option<OsString> {
    let root = cache.parent()?;
    let directory = root.join("remote");
    let template = || {
        let mut template = OsString::from("file://");
        template.push(&directory);
        template.push("/{dir}/{key}");
        Some(template)
    };
    if directory.is_dir() {
        start_pending(&directory, scripts);
        return template();
    }
    let cold = ["units", "workspace"]
        .iter()
        .all(|name| fs::read_dir(cache.join(name)).map_or(true, |mut entries| entries.next().is_none()));
    if !cold {
        return None;
    }
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join("remote.lock")).ok()?;
    lock.lock().ok()?;
    if directory.is_dir() {
        return template();
    }
    let failed = root.join("remote.failed");
    let build = std::os::unix::process::parent_id().to_string();
    if fs::read_to_string(&failed).is_ok_and(|previous| previous == build) {
        return None;
    }
    let fetched = Command::new(scripts.join("buildcache-prefetch"))
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if fetched && directory.is_dir() {
        start_pending(&directory, scripts);
        return template();
    }
    let _ = fs::write(&failed, build);
    None
}

struct Placeholder {
    file: PathBuf,
    source: Vec<u8>,
    command: Vec<u8>,
}

fn placeholders(directories: &[PathBuf]) -> Vec<Placeholder> {
    let mut found = Vec::new();
    for directory in directories {
        for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(file) = path.as_os_str().as_bytes().strip_suffix(b".nanocode-lazy") else {
                continue;
            };
            if !fs::metadata(OsStr::from_bytes(file)).is_ok_and(|data| data.len() == 0) {
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Ok(sidecar) = fs::read(&path) {
                let at = sidecar.iter().position(|byte| *byte == b'\n').unwrap_or(sidecar.len());
                found.push(Placeholder {
                    file: PathBuf::from(OsStr::from_bytes(file)),
                    source: sidecar[..at].to_vec(),
                    command: sidecar.get(at + 1..).unwrap_or_default().to_vec(),
                });
            }
        }
    }
    found.sort_by(|left, right| left.file.cmp(&right.file));
    found
}

fn library_directories(args: &[OsString]) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    for pair in args.windows(2) {
        let (flag, value) = (pair[0].as_bytes(), pair[1].as_bytes());
        let directory = match flag {
            b"-L" => value.strip_prefix(b"dependency=").map(|path| PathBuf::from(OsStr::from_bytes(path))),
            b"--extern" => value
                .iter()
                .position(|byte| *byte == b'=')
                .and_then(|at| Path::new(OsStr::from_bytes(&value[at + 1..])).parent().map(Path::to_path_buf)),
            _ => None,
        };
        if let Some(directory) = directory
            && !directories.contains(&directory)
        {
            directories.push(directory);
        }
    }
    directories
}

fn materialize(cache: &Path, args: &[OsString]) {
    let directories = library_directories(args);
    if placeholders(&directories).is_empty() {
        return;
    }
    let Ok(lock) = fs::OpenOptions::new().create(true).truncate(false).write(true).open(cache.join("lazy.lock")) else {
        return;
    };
    if lock.lock().is_err() {
        return;
    }
    let found = placeholders(&directories);
    let mut sources: Vec<&[u8]> = found.iter().map(|placeholder| placeholder.source.as_slice()).collect();
    sources.sort();
    sources.dedup();
    let staging = cache.join(format!(".lazy.{}.{}", std::process::id(), unique()));
    let mut config = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        config.extend_from_slice(b"url = \"");
        config.extend_from_slice(source);
        config.extend_from_slice(format!("\"\noutput = \"{}/{index}\"\n", staging.display()).as_bytes());
    }
    if fs::create_dir_all(&staging).is_ok() && fs::write(staging.join("config"), &config).is_ok() {
        let _ = Command::new("curl")
            .args(["-sfL", "-Z", "-K"])
            .arg(staging.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mut missed: Vec<&Placeholder> = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let unit = staging.join(format!("{index}.d"));
        let placed = fs::read(staging.join(index.to_string()))
            .and_then(unpacked)
            .and_then(|packed| unpack(&packed, &unit))
            .is_ok();
        for placeholder in found.iter().filter(|placeholder| placeholder.source.as_slice() == *source) {
            let file = &placeholder.file;
            let Some(name) = file.file_name() else {
                continue;
            };
            let next = unit.join("files").join(name);
            let done = placed
                && modified(file).is_some_and(|time| {
                    fs::File::options().write(true).open(&next).and_then(|data| data.set_modified(time)).is_ok()
                })
                && fs::rename(&next, file).is_ok();
            if done {
                let _ = fs::remove_file(lazy_path(file));
            } else {
                missed.push(placeholder);
            }
        }
    }
    let _ = fs::remove_dir_all(&staging);
    if missed.is_empty() {
        return;
    }
    let _ = fs::write(cache.join("lazy.failed"), b"");
    eprintln!("buildcache: could not fetch {} cached dependencies; compiling them", missed.len());
    let mut commands: Vec<&[u8]> = missed.iter().map(|placeholder| placeholder.command.as_slice()).collect();
    commands.sort();
    commands.dedup();
    while !commands.is_empty() {
        let lazy = placeholders(&directories);
        let (ready, waiting): (Vec<&[u8]>, Vec<&[u8]>) = commands.iter().partition(|command| {
            !command_args(command).1.windows(2).any(|pair| {
                pair[0] == "--extern"
                    && pair[1].as_bytes().iter().position(|byte| *byte == b'=').is_some_and(|at| {
                        let path = Path::new(OsStr::from_bytes(&pair[1].as_bytes()[at + 1..]));
                        lazy.iter().any(|placeholder| placeholder.file == path && placeholder.command != **command)
                    })
            })
        });
        let (ready, stuck) = if ready.is_empty() { (Vec::new(), waiting.clone()) } else { (ready, Vec::new()) };
        let width = std::thread::available_parallelism().map_or(1, usize::from);
        let built: Vec<bool> = ready
            .chunks(width)
            .flat_map(|batch| {
                std::thread::scope(|scope| {
                    let running: Vec<_> = batch.iter().map(|command| scope.spawn(|| rebuild(command, &missed))).collect();
                    running.into_iter().map(|run| run.join().unwrap_or(false)).collect::<Vec<bool>>()
                })
            })
            .collect();
        let settled = ready.iter().zip(built).chain(stuck.iter().map(|command| (command, false)));
        for (command, built) in settled {
            for placeholder in missed.iter().filter(|placeholder| placeholder.command.as_slice() == *command) {
                let _ = fs::remove_file(lazy_path(&placeholder.file));
                if !built {
                    let _ = fs::remove_file(&placeholder.file);
                }
            }
        }
        commands = if stuck.is_empty() { waiting } else { Vec::new() };
    }
}

fn command_args(command: &[u8]) -> (PathBuf, Vec<OsString>, Environment) {
    let mut fields = command.split(|byte| *byte == 0);
    let cwd = PathBuf::from(OsStr::from_bytes(fields.next().unwrap_or_default()));
    let args: Vec<OsString> = fields.by_ref().take_while(|field| !field.is_empty()).map(|field| OsStr::from_bytes(field).to_os_string()).collect();
    let environment = fields
        .filter_map(|field| {
            let at = field.iter().position(|byte| *byte == b'=')?;
            Some((OsStr::from_bytes(&field[..at]).to_os_string(), OsStr::from_bytes(&field[at + 1..]).to_os_string()))
        })
        .collect();
    (cwd, args, environment)
}

fn rebuild(command: &[u8], missed: &[&Placeholder]) -> bool {
    let (cwd, args, environment) = command_args(command);
    let Some(program) = args.first() else {
        return false;
    };
    let outputs: Vec<(&Path, Option<SystemTime>)> = missed
        .iter()
        .filter(|placeholder| placeholder.command == command)
        .map(|placeholder| (placeholder.file.as_path(), modified(&placeholder.file)))
        .collect();
    let built = Command::new(program)
        .args(&args[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(&environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    built
        && outputs.iter().all(|(file, time)| {
            time.is_some_and(|time| fs::File::options().write(true).open(file).and_then(|data| data.set_modified(time)).is_ok())
        })
}

fn filter(command: &mut Command, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
    let output = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(input));
        let output = child.wait_with_output();
        writer.join().map_err(|_| io::Error::other("writer panicked"))??;
        output
    })?;
    if !output.status.success() {
        return Err(io::Error::other("filter failed"));
    }
    Ok(output.stdout)
}

fn unpacked(data: Vec<u8>) -> io::Result<Vec<u8>> {
    let tool = match data.get(..4) {
        Some([0x28, 0xb5, 0x2f, 0xfd]) => "zstd",
        Some([0x1f, 0x8b, _, _]) => "gzip",
        _ => return Ok(data),
    };
    filter(Command::new(tool).arg("-dcq"), &data)
}

fn compressed(packed: Vec<u8>) -> Vec<u8> {
    for (tool, args) in [("zstd", &["-q", "-9", "--long=27", "-T0", "-c"][..]), ("gzip", &["-q", "-9", "-c"][..])] {
        if let Ok(data) = filter(Command::new(tool).args(args), &packed) {
            return data;
        }
    }
    packed
}

fn download(url: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(rest) = url.strip_prefix(b"http://") {
        return http_get(rest);
    }
    if let Some(path) = url.strip_prefix(b"file://") {
        return fs::read(OsStr::from_bytes(path));
    }
    if url.starts_with(b"/") {
        return fs::read(OsStr::from_bytes(url));
    }
    let output = Command::new("curl")
        .args(["-fsSL", "--"])
        .arg(OsStr::from_bytes(url))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("curl failed"));
    }
    Ok(output.stdout)
}

fn http_get(rest: &[u8]) -> io::Result<Vec<u8>> {
    let slash = rest.iter().position(|byte| *byte == b'/').unwrap_or(rest.len());
    let authority = std::str::from_utf8(&rest[..slash]).map_err(io::Error::other)?;
    let path = if slash == rest.len() { b"/" as &[u8] } else { &rest[slash..] };
    let address = if authority.rsplit(':').next().is_some_and(|port| port.parse::<u16>().is_ok()) && !authority.ends_with(']') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let mut stream = TcpStream::connect(address.as_str())?;
    let mut request = b"GET ".to_vec();
    request.extend_from_slice(path);
    request.extend_from_slice(format!(" HTTP/1.1\r\nHost: {authority}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n").as_bytes());
    stream.write_all(&request)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("no header end"))?;
    let head = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
    let mut lines = head.split("\r\n");
    if lines.next().and_then(|status| status.split(' ').nth(1)) != Some("200") {
        return Err(io::Error::other("not found"));
    }
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim() {
                "content-length" => length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?),
                "transfer-encoding" => chunked = value.trim() == "chunked",
                _ => {}
            }
        }
    }
    let body = response.split_off(end + 4);
    if chunked {
        return dechunk(&body);
    }
    match length {
        Some(length) if length != body.len() => Err(io::Error::other("short body")),
        _ => Ok(body),
    }
}

fn dechunk(mut body: &[u8]) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        let line = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| io::Error::other("bad chunk"))?;
        let size = std::str::from_utf8(&body[..line]).map_err(io::Error::other)?;
        let size = usize::from_str_radix(size.split(';').next().unwrap_or_default().trim(), 16).map_err(io::Error::other)?;
        body = &body[line + 2..];
        if size == 0 {
            return Ok(data);
        }
        if body.len() < size + 2 {
            return Err(io::Error::other("short chunk"));
        }
        data.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn pack(root: &Path) -> io::Result<Vec<u8>> {
    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries)?;
    entries.sort();
    let mut packed = Vec::new();
    for (relative, is_file) in entries {
        if !is_file {
            continue;
        }
        let path = root.join(&relative);
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
        let data = fs::read(&path)?;
        packed.extend_from_slice(relative.as_os_str().as_bytes());
        packed.extend_from_slice(format!("\n{} {mode:o}\n", data.len()).as_bytes());
        packed.extend_from_slice(&data);
    }
    Ok(packed)
}

fn unpack(mut packed: &[u8], staging: &Path) -> io::Result<()> {
    let bad = || io::Error::other("bad pack");
    fs::create_dir_all(staging.join("files"))?;
    while !packed.is_empty() {
        let name_end = packed.iter().position(|byte| *byte == b'\n').ok_or_else(bad)?;
        let name = Path::new(OsStr::from_bytes(&packed[..name_end]));
        packed = &packed[name_end + 1..];
        let size_end = packed.iter().position(|byte| *byte == b'\n').ok_or_else(bad)?;
        let (size, mode) = std::str::from_utf8(&packed[..size_end]).map_err(io::Error::other)?.split_once(' ').ok_or_else(bad)?;
        let size: usize = size.parse().map_err(io::Error::other)?;
        let mode = u32::from_str_radix(mode, 8).map_err(io::Error::other)?;
        packed = &packed[size_end + 1..];
        if packed.len() < size || !name.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
            return Err(bad());
        }
        let destination = staging.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&destination, &packed[..size])?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
        packed = &packed[size..];
    }
    Ok(())
}

fn publish(cache: &Path, to: &Path) -> io::Result<()> {
    let mut index = Vec::new();
    let mut eager = b"workspace-index\n".to_vec();
    let mut bundle: Vec<(String, Vec<u8>)> = Vec::new();
    for directory in ["units", "workspace"] {
        let Ok(entries) = fs::read_dir(cache.join(directory)) else {
            continue;
        };
        fs::create_dir_all(to.join(directory))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.len() != 64 || !entry.file_type().is_ok_and(|kind| kind.is_symlink()) || entry.path().join("lazy").exists() {
                continue;
            }
            let Ok(packed) = pack(&entry.path()) else {
                continue;
            };
            let mut lazy = Vec::new();
            let mut rest = packed.as_slice();
            while let Some(name_end) = rest.iter().position(|byte| *byte == b'\n') {
                let (relative, header) = (&rest[..name_end], &rest[name_end + 1..]);
                let Some(size_end) = header.iter().position(|byte| *byte == b'\n') else {
                    break;
                };
                let size: usize = std::str::from_utf8(header[..size_end].split(|byte| *byte == b' ').next().unwrap_or_default())
                    .ok()
                    .and_then(|size| size.parse().ok())
                    .unwrap_or_default();
                let end = (name_end + 1 + size_end + 1 + size).min(rest.len());
                match relative.strip_prefix(b"files/") {
                    Some(file) if !file.ends_with(b".d") => {
                        lazy.extend_from_slice(file);
                        lazy.push(b'\n');
                    }
                    _ => {
                        index.extend_from_slice(format!("{directory}/").as_bytes());
                        index.extend_from_slice(name.as_bytes());
                        index.push(b'/');
                        index.extend_from_slice(&rest[..end]);
                    }
                }
                rest = &rest[end..];
            }
            if !lazy.split(|byte| *byte == b'\n').all(|file| file.is_empty() || is_library(file)) {
                eager.extend_from_slice(format!("{directory}-").as_bytes());
                eager.extend_from_slice(name.as_bytes());
                eager.push(b'\n');
                bundle.push((format!("{directory}-{}", name.to_string_lossy()), packed.clone()));
            }
            index.extend_from_slice(format!("{directory}/").as_bytes());
            index.extend_from_slice(name.as_bytes());
            index.extend_from_slice(format!("/lazy\n{} 644\n", lazy.len()).as_bytes());
            index.extend_from_slice(&lazy);
            let destination = to.join(directory).join(&name);
            let mut staging = destination.clone().into_os_string();
            staging.push(format!(".{}", std::process::id()));
            fs::write(&staging, compressed(packed))?;
            fs::rename(&staging, &destination)?;
        }
    }
    fs::create_dir_all(to.join("workspace"))?;
    let destination = to.join("workspace/index");
    let mut staging = destination.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    fs::write(&staging, compressed(index.clone()))?;
    fs::rename(&staging, &destination)?;
    bundle.sort_by_key(|(_, packed)| packed.len());
    let chunk = 1 << 20;
    let mut blocks = Vec::new();
    let mut offset = 0;
    while offset == 0 || bundle.iter().any(|(_, packed)| packed.len() > offset) {
        for (name, packed) in &bundle {
            if offset > 0 && packed.len() <= offset {
                continue;
            }
            let part = &packed[offset.min(packed.len())..packed.len().min(offset + chunk)];
            blocks.extend_from_slice(format!("{name}\n{} {}\n", part.len(), packed.len()).as_bytes());
            blocks.extend_from_slice(part);
        }
        offset += chunk;
    }
    let packs: Vec<u8> = bundle.into_iter().flat_map(|(name, packed)| format!("{name}\n{}\n", packed.len()).into_bytes().into_iter().chain(packed)).collect();
    let dictionary = format!("--lzma2=dict={}", packs.len());
    for (name, data, args) in [
        ("eager-packs", packs, &["-q", "-c", &dictionary][..]),
        ("eager-blocks", blocks, &["-q", "-c", "-6e", "-T0", "--block-size=16MiB"][..]),
    ] {
        let destination = to.join("workspace").join(name);
        match filter(Command::new("xz").args(args), &data) {
            Ok(packed) => {
                let mut staging = destination.clone().into_os_string();
                staging.push(format!(".{}", std::process::id()));
                fs::write(&staging, packed)?;
                fs::rename(&staging, &destination)?;
            }
            Err(_) => {
                let _ = fs::remove_file(&destination);
            }
        }
    }
    if let Some(helper) = publish_helper(to)? {
        eager.extend_from_slice(format!("workspace-{helper}\n").as_bytes());
        if let Some((name, _)) = helper.strip_prefix("helper-").and_then(|name| name.rsplit_once('-')) {
            publish_head(to, &format!("head-{name}"), &helper, &eager, &index)?;
        }
    }
    let destination = to.join("workspace/eager");
    let mut staging = destination.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    fs::write(&staging, eager)?;
    fs::rename(&staging, &destination)
}

fn publish_head(to: &Path, name: &str, helper: &str, eager: &[u8], index: &[u8]) -> io::Result<()> {
    let directory = to.join("workspace");
    let staging = directory.join(format!(".{name}.{}", std::process::id()));
    fs::create_dir_all(&staging)?;
    let archive = fs::write(staging.join("eager"), eager)
        .and_then(|()| fs::write(staging.join("index"), index))
        .and_then(|()| fs::copy(directory.join(helper), staging.join("helper")))
        .and_then(|_| {
            Command::new("tar")
                .arg("-cf")
                .arg("-")
                .arg("-C")
                .arg(&staging)
                .args(["eager", "index", "helper"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
        });
    fs::remove_dir_all(&staging)?;
    let destination = directory.join(name);
    let archive = archive?;
    if !archive.status.success() {
        let _ = fs::remove_file(&destination);
        return Ok(());
    }
    let mut staging = destination.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    fs::write(&staging, compressed(archive.stdout))?;
    fs::rename(&staging, &destination)
}

fn publish_helper(to: &Path) -> io::Result<Option<String>> {
    let helper = env::current_exe()?;
    let Some(checksum) = helper.file_name().and_then(OsStr::to_str).and_then(|name| name.strip_prefix("buildcache-")) else {
        return Ok(None);
    };
    let machine = Command::new("uname").arg("-sm").stdin(Stdio::null()).stderr(Stdio::null()).output()?;
    if !machine.status.success() {
        return Ok(None);
    }
    let machine = String::from_utf8_lossy(&machine.stdout).trim().replace(' ', "-");
    let data = fs::read(&helper)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let name = format!("helper-{checksum}-{machine}-{}", hasher.finish());
    let destination = to.join("workspace").join(&name);
    fs::create_dir_all(to.join("workspace"))?;
    let mut staging = destination.clone().into_os_string();
    staging.push(format!(".{}", std::process::id()));
    fs::write(&staging, &data)?;
    fs::rename(&staging, &destination)?;
    let alias = to.join("workspace").join(format!("helper-{checksum}-{machine}"));
    fs::write(&staging, data)?;
    fs::rename(&staging, &alias)?;
    Ok(Some(name))
}


fn c(cache: &Path, args: &[OsString], environment: Environment) -> ExitCode {
    let (mut object, mut source, mut compile) = (None, None, false);
    for index in 1..args.len() {
        let arg = args[index].as_bytes();
        match args[index - 1].as_bytes() {
            b"-o" => object = Some(index),
            b"-c" => source = Some(index),
            _ => {}
        }
        if arg == b"-E" || arg.starts_with(b"-M") || (arg.starts_with(b"-o") && arg.len() > 2)
            || arg.starts_with(b"@") || arg == b"-"
        {
            return exec(args, &environment);
        }
        if arg == b"-c" {
            compile = true;
        }
    }
    let (Some(object), Some(source), true) = (object, source, compile) else {
        return exec(args, &environment);
    };
    if args[source].as_bytes().starts_with(b"-") {
        return exec(args, &environment);
    }
    let out_dir = environment.get(OsStr::new("OUT_DIR")).cloned().unwrap_or_default();
    let out_dir = out_dir.as_bytes().to_vec();
    let Ok(source_text) = fs::read(&args[source]) else {
        return exec(args, &environment);
    };
    if volatile(&source_text) {
        return exec(args, &environment);
    }
    let Ok(version) = Command::new(&args[0]).arg("--version").output() else {
        return exec(args, &environment);
    };
    let mut hasher = Sha256::new();
    hasher.update(C_KEY.as_bytes());
    hasher.update(b"\n");
    hasher.update(&version.stdout);
    hasher.update(&version.stderr);
    hasher.update(env::current_dir().unwrap_or_default().as_os_str().as_bytes());
    hasher.update(b"\n");
    for (index, arg) in args.iter().enumerate() {
        if index == object || index + 1 == object {
            continue;
        }
        hasher.update(&relocate(arg.as_bytes(), &out_dir, b"<out>"));
        hasher.update(b"\n");
    }
    for name in ["MACOSX_DEPLOYMENT_TARGET", "SDKROOT", "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "OBJC_INCLUDE_PATH"] {
        if let Some(value) = environment.get(OsStr::new(name)) {
            hasher.update(name.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b"\n");
        }
    }
    let mut source_hasher = Sha256::new();
    source_hasher.update(&source_text);
    hasher.update(source_hasher.finish().as_bytes());
    let entry = cache.join("c").join(hasher.finish());
    let object = PathBuf::from(&args[object]);
    if headers_match(&entry, &out_dir)
        && clone(&entry.join("object"), &object).is_ok()
        && touch(&object).is_ok()
    {
        if let Ok(errors) = fs::read(entry.join("stderr")) {
            let _ = io::stderr().write_all(&errors);
        }
        return ExitCode::SUCCESS;
    }
    let mut depfile = object.clone().into_os_string();
    depfile.push(format!(".{}.buildcache.d", std::process::id()));
    let started = SystemTime::now();
    let mut map = OsString::from("-ffile-prefix-map=");
    map.push(OsStr::from_bytes(&out_dir));
    map.push("=.");
    let output = match Command::new(&args[0])
        .args(&args[1..])
        .arg(&map)
        .arg("-MD")
        .arg("-MF")
        .arg(&depfile)
        .env_clear()
        .envs(&environment)
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            eprintln!("buildcache: could not run {}: {error}", args[0].to_string_lossy());
            return ExitCode::from(1);
        }
    };
    let _ = io::stdout().write_all(&output.stdout);
    let _ = io::stderr().write_all(&output.stderr);
    let dependencies = fs::read(&depfile);
    let _ = fs::remove_file(&depfile);
    if output.status.success()
        && let Ok(dependencies) = dependencies
        && dependency_paths(&dependencies)
            .iter()
            .all(|path| unchanged_since(Path::new(OsStr::from_bytes(path)), started).is_some())
        && let Some(manifest) = manifest(&dependencies, &out_dir)
    {
        let _ = store_object(cache, &entry, &object, &manifest, &output.stderr);
    }
    exit_code(output.status)
}

fn volatile(text: &[u8]) -> bool {
    [b"__DATE__" as &[u8], b"__TIME__", b"__TIMESTAMP__"]
        .iter()
        .any(|token| text.windows(token.len()).any(|window| window == *token))
}

fn dependency_paths(depfile: &[u8]) -> Vec<Vec<u8>> {
    let Some(colon) = depfile.windows(2).position(|window| window == b": ") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = Vec::new();
    let mut bytes = depfile[colon + 2..].iter().peekable();
    while let Some(&byte) = bytes.next() {
        match byte {
            b'\\' if bytes.peek() == Some(&&b' ') => {
                current.push(b' ');
                bytes.next();
            }
            b'\\' if bytes.peek() == Some(&&b'\n') => {
                bytes.next();
            }
            b' ' | b'\n' | b'\t' | b'\r' => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(byte),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    paths
}

fn manifest(depfile: &[u8], out_dir: &[u8]) -> Option<Vec<u8>> {
    let mut manifest = Vec::new();
    for path in dependency_paths(depfile).into_iter().skip(1) {
        let contents = fs::read(OsStr::from_bytes(&path)).ok()?;
        if volatile(&contents) {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(&contents);
        manifest.extend_from_slice(hasher.finish().as_bytes());
        manifest.push(b' ');
        manifest.extend_from_slice(&relocate(&path, out_dir, b"<out>"));
        manifest.push(b'\n');
    }
    Some(manifest)
}

fn headers_match(entry: &Path, out_dir: &[u8]) -> bool {
    let Ok(manifest) = fs::read(entry.join("headers")) else {
        return false;
    };
    manifest.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).all(|line| {
        let Some(at) = line.iter().position(|byte| *byte == b' ') else {
            return false;
        };
        let path = relocate(&line[at + 1..], b"<out>", out_dir);
        digest_file(Path::new(OsStr::from_bytes(&path))).is_ok_and(|digest| digest.as_bytes() == &line[..at])
    })
}

fn store_object(cache: &Path, entry: &Path, object: &Path, manifest: &[u8], errors: &[u8]) -> io::Result<()> {
    fs::create_dir_all(cache.join("c"))?;
    let staging = cache.join("c").join(format!(".data.{}.{}", std::process::id(), unique()));
    let result = (|| -> io::Result<()> {
        fs::create_dir(&staging)?;
        clone(object, &staging.join("object"))?;
        fs::write(staging.join("headers"), manifest)?;
        fs::write(staging.join("stderr"), errors)?;
        if fs::symlink_metadata(entry).is_ok() {
            fs::remove_file(entry)?;
        }
        symlink(staging.file_name().unwrap_or_default(), entry)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    filled: usize,
    length: u64,
}

const ROUND: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    const fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.buffer[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.filled = 0;
        }
        let whole = data.len() - data.len() % 64;
        self.blocks(&data[..whole]);
        let rest = &data[whole..];
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    fn blocks(&mut self, data: &[u8]) {
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("sha2") {
            unsafe { compress_sha2(&mut self.state, data) };
            return;
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("sha") && std::arch::is_x86_feature_detected!("sse4.1") {
            unsafe { compress_sha_ni(&mut self.state, data) };
            return;
        }
        for block in data.chunks_exact(64) {
            self.compress(block.try_into().expect("64-byte block"));
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().expect("word"));
        }
        for index in 16..64 {
            let small0 = words[index - 15].rotate_right(7) ^ words[index - 15].rotate_right(18) ^ (words[index - 15] >> 3);
            let small1 = words[index - 2].rotate_right(17) ^ words[index - 2].rotate_right(19) ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(small0)
                .wrapping_add(words[index - 7])
                .wrapping_add(small1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let big1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let first = h
                .wrapping_add(big1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let big0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = big0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }

    fn finish(mut self) -> String {
        let bits = self.length.wrapping_mul(8);
        let mut padding = vec![0x80u8];
        let used = (self.length % 64) as usize;
        let zeros = if used < 56 { 55 - used } else { 119 - used };
        padding.extend(std::iter::repeat_n(0, zeros));
        padding.extend_from_slice(&bits.to_be_bytes());
        let length = self.length;
        self.update(&padding);
        self.length = length;
        self.state.iter().map(|word| format!("{word:08x}")).collect()
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
unsafe fn compress_sha_ni(state: &mut [u32; 8], data: &[u8]) {
    use std::arch::x86_64::{
        __m128i, _mm_add_epi32, _mm_alignr_epi8, _mm_blend_epi16, _mm_loadu_si128, _mm_set_epi64x, _mm_sha256msg1_epu32,
        _mm_sha256msg2_epu32, _mm_sha256rnds2_epu32, _mm_shuffle_epi8, _mm_shuffle_epi32, _mm_storeu_si128,
    };
    unsafe {
        let order = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
        let dcba = _mm_loadu_si128(state.as_ptr().cast::<__m128i>());
        let hgfe = _mm_loadu_si128(state.as_ptr().add(4).cast::<__m128i>());
        let cdab = _mm_shuffle_epi32(dcba, 0xb1);
        let efgh = _mm_shuffle_epi32(hgfe, 0x1b);
        let mut abef = _mm_alignr_epi8(cdab, efgh, 8);
        let mut cdgh = _mm_blend_epi16(efgh, cdab, 0xf0);
        for block in data.chunks_exact(64) {
            let (saved_abef, saved_cdgh) = (abef, cdgh);
            let mut schedule = [
                _mm_shuffle_epi8(_mm_loadu_si128(block.as_ptr().cast::<__m128i>()), order),
                _mm_shuffle_epi8(_mm_loadu_si128(block.as_ptr().add(16).cast::<__m128i>()), order),
                _mm_shuffle_epi8(_mm_loadu_si128(block.as_ptr().add(32).cast::<__m128i>()), order),
                _mm_shuffle_epi8(_mm_loadu_si128(block.as_ptr().add(48).cast::<__m128i>()), order),
            ];
            for group in 0..16 {
                let words = _mm_add_epi32(schedule[0], _mm_loadu_si128(ROUND.as_ptr().add(group * 4).cast::<__m128i>()));
                cdgh = _mm_sha256rnds2_epu32(cdgh, abef, words);
                abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32(words, 0x0e));
                let next = _mm_sha256msg2_epu32(
                    _mm_add_epi32(_mm_sha256msg1_epu32(schedule[0], schedule[1]), _mm_alignr_epi8(schedule[3], schedule[2], 4)),
                    schedule[3],
                );
                schedule = [schedule[1], schedule[2], schedule[3], next];
            }
            abef = _mm_add_epi32(abef, saved_abef);
            cdgh = _mm_add_epi32(cdgh, saved_cdgh);
        }
        let feba = _mm_shuffle_epi32(abef, 0x1b);
        let dchg = _mm_shuffle_epi32(cdgh, 0xb1);
        _mm_storeu_si128(state.as_mut_ptr().cast::<__m128i>(), _mm_blend_epi16(feba, dchg, 0xf0));
        _mm_storeu_si128(state.as_mut_ptr().add(4).cast::<__m128i>(), _mm_alignr_epi8(dchg, feba, 8));
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "sha2")]
unsafe fn compress_sha2(state: &mut [u32; 8], data: &[u8]) {
    use std::arch::aarch64::{
        vaddq_u32, vld1q_u32, vld1q_u8, vreinterpretq_u32_u8, vrev32q_u8, vsha256h2q_u32, vsha256hq_u32,
        vsha256su0q_u32, vsha256su1q_u32, vst1q_u32,
    };
    unsafe {
        let mut abcd = vld1q_u32(state.as_ptr());
        let mut efgh = vld1q_u32(state.as_ptr().add(4));
        for block in data.chunks_exact(64) {
            let (saved_abcd, saved_efgh) = (abcd, efgh);
            let mut schedule = [
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr()))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(16)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(32)))),
                vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(48)))),
            ];
            for group in 0..16 {
                let words = vaddq_u32(schedule[0], vld1q_u32(ROUND.as_ptr().add(group * 4)));
                let previous = abcd;
                abcd = vsha256hq_u32(abcd, efgh, words);
                efgh = vsha256h2q_u32(efgh, previous, words);
                let next = vsha256su1q_u32(vsha256su0q_u32(schedule[0], schedule[1]), schedule[2], schedule[3]);
                schedule = [schedule[1], schedule[2], schedule[3], next];
            }
            abcd = vaddq_u32(abcd, saved_abcd);
            efgh = vaddq_u32(efgh, saved_efgh);
        }
        vst1q_u32(state.as_mut_ptr(), abcd);
        vst1q_u32(state.as_mut_ptr().add(4), efgh);
    }
}

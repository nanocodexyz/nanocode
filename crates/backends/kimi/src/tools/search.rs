//! Kimi Code's Glob and Grep: both run `rg` from PATH with Kimi's arguments and shape its output the way
//! `globTool.ts` and `grepTool.ts` do.

use super::files::fs_error;
use super::io::{self, Kind};
use super::{Access, Builtins, Outcome, input, normalize, sensitive};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const VCS: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
const VARIANTS: [&str; 10] = [
    ".bak",
    ".backup",
    ".copy",
    ".disabled",
    ".key",
    ".old",
    ".orig",
    ".pem",
    ".save",
    ".tmp",
];
const TIMEOUT_SECS: u64 = 20;
const MAX_OUTPUT_BYTES: usize = 10_485_760;
const MAX_RETAINED_CHARS: usize = 10_000_000;
const NO_MATCHES: &str = "No non-sensitive matches found";

/// `SENSITIVE_GLOBS_TO_EXCLUDE`, already negated.
fn sensitive_globs() -> Vec<String> {
    let mut globs = vec!["!**/.env".to_owned()];
    for name in ["id_rsa", "id_ed25519", "id_ecdsa"] {
        globs.push(format!("!**/{name}"));
        globs.push(format!("!**/{name}[-_]*"));
        globs.extend(VARIANTS.iter().map(|suffix| format!("!**/{name}{suffix}")));
    }
    for dir in [".aws", ".gcp"] {
        globs.push(format!("!**/{dir}/credentials"));
        globs.push(format!("!**/{dir}/credentials/**"));
    }
    globs
}

fn push_globs(rg: &mut Vec<String>, globs: impl IntoIterator<Item = String>) {
    for glob in globs {
        rg.push("--glob".to_owned());
        rg.push(glob);
    }
}

/// JavaScript string length (UTF-16 code units).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `relativizeIfUnder`.
fn relativize(candidate: &str, base: &str) -> String {
    if candidate == base {
        return ".".to_owned();
    }
    let prefix = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    candidate
        .strip_prefix(&prefix)
        .unwrap_or(candidate)
        .to_owned()
}

fn resolve_root(builtins: &Builtins, tool: &str, path: Option<&str>) -> Result<PathBuf, Outcome> {
    path.map_or_else(
        || Ok(builtins.cwd.clone()),
        |path| builtins.resolve(tool, path, Access::Search),
    )
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
    stderr_truncated: bool,
    timed_out: bool,
}

/// `runRgOnce`, retried single-threaded on EAGAIN like `shouldRetryRipgrepEagain`. Errs with Kimi's
/// spawn-failure message.
async fn run_rg(builtins: &Builtins, args: Vec<String>, cwd: &Path) -> Result<Run, String> {
    let fail = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::NotFound => unavailable(builtins),
        _ => error.to_string(),
    };
    let run = spawn(builtins, &args, cwd).await.map_err(fail)?;
    let eagain = run.stderr.contains("os error 11")
        || run.stderr.contains("Resource temporarily unavailable");
    if run.code == 0 || run.code == 1 || run.timed_out || run.truncated || !eagain {
        return Ok(run);
    }
    let single = ["-j".to_owned(), "1".to_owned()]
        .into_iter()
        .chain(args)
        .collect::<Vec<_>>();
    spawn(builtins, &single, cwd).await.map_err(fail)
}

async fn spawn(builtins: &Builtins, args: &[String], cwd: &Path) -> std::io::Result<Run> {
    match builtins.host() {
        Some(host) => host_rg(host, args, cwd).await,
        None => spawn_rg(args, cwd).await,
    }
}

/// rg through a host's shell. The host enforces the timeout and the output cap; a shell without rg
/// (exit 127) reads as rg missing.
async fn host_rg(host: &io::Host, args: &[String], cwd: &Path) -> std::io::Result<Run> {
    let command = std::iter::once("rg".to_owned())
        .chain(args.iter().map(|arg| io::quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    let limit = std::time::Duration::from_secs(TIMEOUT_SECS);
    let started = nanocode_platform::Instant::now();
    let out = io::run(host, cwd, &command, limit, MAX_OUTPUT_BYTES, None).await?;
    if out.exit_code == 127 {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    Ok(Run {
        code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
        truncated: out.truncated,
        stderr_truncated: false,
        timed_out: started.elapsed() >= limit,
    })
}

/// `rgUnavailableMessage` for a local environment. ponytail: Kimi first tries to download rg into
/// `~/.kimi-code/bin`; we only report it missing.
fn unavailable(builtins: &Builtins) -> String {
    let share = builtins
        .home
        .clone()
        .unwrap_or_default()
        .join(".kimi-code/bin/rg");
    format!(
        "ripgrep (rg) is not available and the automatic bootstrap failed.\n\nError: ripgrep (rg) is not available on PATH\n\nFix options:\n  macOS:   brew install ripgrep\n  Ubuntu:  sudo apt-get install ripgrep\n  Other:   https://github.com/BurntSushi/ripgrep#installation\n\nAlternatively, drop a static rg binary at {}",
        share.display()
    )
}

#[cfg(not(target_family = "wasm"))]
async fn spawn_rg(args: &[String], cwd: &Path) -> std::io::Result<Run> {
    use std::process::Stdio;
    let mut child = nanocode_executor::process::Command::new("rg")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut truncated, mut stderr_truncated) = (false, false);
    let read = nanocode_sync::future::join(
        capped(&mut stdout, &mut out, &mut truncated, true),
        capped(&mut stderr, &mut err, &mut stderr_truncated, false),
    );
    let timed_out = nanocode_platform::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), read)
        .await
        .is_err();
    drop((stdout, stderr));
    if timed_out || truncated {
        let _ = child.start_kill();
    }
    let code = child.wait().await?.code().unwrap_or(-1);
    Ok(Run {
        code,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated,
        stderr_truncated,
        timed_out,
    })
}

/// `readStreamWithCap`: keeps the first `MAX_OUTPUT_BYTES`. Past the cap, stdout is closed so rg stops
/// (Kimi kills it); stderr is drained and dropped.
#[cfg(not(target_family = "wasm"))]
async fn capped<R: nanocode_io::AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    buffer: &mut Vec<u8>,
    truncated: &mut bool,
    stop: bool,
) {
    use nanocode_io::AsyncReadExt;
    let mut chunk = vec![0; 64 * 1024];
    while let Some(reader) = pipe.as_mut() {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if *truncated {
            continue;
        }
        let room = MAX_OUTPUT_BYTES - buffer.len();
        buffer.extend_from_slice(&chunk[..n.min(room)]);
        if n > room {
            *truncated = true;
            if stop {
                *pipe = None;
            }
        }
    }
}

#[cfg(target_family = "wasm")]
async fn spawn_rg(_args: &[String], _cwd: &Path) -> std::io::Result<Run> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    head_limit: Option<usize>,
    offset: Option<usize>,
    path: Option<String>,
    #[serde(default)]
    include_ignored: bool,
}

/// Kimi Code's `GlobTool.execution`.
pub async fn glob(builtins: &Builtins, args: &Value) -> Outcome {
    let args: GlobInput = match input("Glob", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let root = match resolve_root(builtins, "Glob", args.path.as_deref()) {
        Ok(root) => root,
        Err(error) => return error,
    };
    let root_text = root.display().to_string();
    match io::kind(builtins.host(), &root).await {
        Ok(Kind::File(_) | Kind::Special) => {
            return Outcome::error(format!("{root_text} is not a directory"));
        }
        Ok(Kind::Dir) => {}
        Ok(Kind::Missing) => {
            return Outcome::error(format!("{root_text} does not exist"));
        }
        Err(error) => return Outcome::error(fs_error(&error, "stat")),
    }
    let mut rg = vec![
        "--files".to_owned(),
        "--hidden".to_owned(),
        "--sortr=modified".to_owned(),
    ];
    push_globs(&mut rg, VCS.iter().map(|dir| format!("!{dir}")));
    push_globs(&mut rg, [args.pattern.clone()]);
    push_globs(&mut rg, sensitive_globs());
    if args.include_ignored {
        rg.push("--no-ignore".to_owned());
    }
    rg.push(".".to_owned());
    let run = match run_rg(builtins, rg, &root).await {
        Ok(run) => run,
        Err(message) => return Outcome::error(message),
    };
    let complete = |cut: bool| {
        let mut text = run.stdout.as_str();
        if cut && !text.ends_with('\n') {
            text = text.rfind('\n').map_or("", |end| &text[..=end]);
        }
        text.split('\n')
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>()
    };
    let stderr = run.stderr.trim();
    let mut warnings = Vec::new();
    if run.timed_out {
        warnings.push(format!(
            "Glob timed out after {TIMEOUT_SECS}s; partial results returned."
        ));
    }
    if run.truncated {
        warnings.push(format!(
            "[stdout truncated at {MAX_OUTPUT_BYTES} bytes; results may be incomplete — use a more specific pattern]"
        ));
    }
    let failed = run.code != 0 && run.code != 1 && !run.timed_out && !run.truncated;
    if failed {
        if complete(true).is_empty() {
            return Outcome::error(
                if stderr.to_lowercase().contains("no such file or directory") {
                    format!("{root_text} does not exist")
                } else if stderr.is_empty() {
                    "Glob failed".to_owned()
                } else {
                    format!("Glob failed: {stderr}")
                },
            );
        }
        warnings.push(if stderr.is_empty() {
            "Glob completed with warnings; some directories could not be read.".to_owned()
        } else {
            format!("Glob completed with warnings; some directories could not be read: {stderr}")
        });
    }
    let mut kept = Vec::new();
    let mut filtered = 0;
    for path in complete(run.truncated || run.timed_out) {
        let absolute = normalize(&root.join(path));
        match sensitive(&absolute) {
            true => filtered += 1,
            false => kept.push(absolute.display().to_string()),
        }
    }
    let offset = args.offset.unwrap_or(0);
    let head_limit = args.head_limit.unwrap_or(100);
    let rest = kept.get(offset..).unwrap_or_default();
    let limited = match head_limit {
        0 => rest,
        _ => &rest[..rest.len().min(head_limit)],
    };
    let partial = run.truncated || run.timed_out || failed;
    let relative = root.starts_with(&builtins.cwd);
    let candidates = limited
        .iter()
        .map(|path| {
            if relative {
                relativize(path, &root_text)
            } else {
                path.clone()
            }
        })
        .collect::<Vec<_>>();
    let notices = |count: usize, char_limited: bool| {
        let mut lines = warnings.clone();
        let mut footer = Vec::new();
        let truncated = char_limited || offset + count < kept.len();
        if count == 0 {
            lines.push(if !kept.is_empty() {
                let set = if partial {
                    "collected partial result set"
                } else {
                    "current result set"
                };
                format!(
                    "No more matches at offset={offset} in the {set} ({} matches).",
                    kept.len()
                )
            } else if partial {
                "No matches collected; search incomplete.".to_owned()
            } else if filtered > 0 {
                format!("No non-sensitive matches found ({filtered} sensitive file(s) filtered).")
            } else {
                "No matches found".to_owned()
            });
        } else if truncated || offset > 0 || partial {
            let total = match partial {
                true => format!("{} collected matches (partial result set)", kept.len()),
                false => kept.len().to_string(),
            };
            lines.push(format!(
                "Showing matches {}–{} of {total}.",
                offset + 1,
                offset + count
            ));
        }
        if char_limited {
            lines.push("Character limit reached; only complete paths are returned.".to_owned());
        }
        if truncated {
            lines.push(format!(
                "Continue with the same search arguments and offset={}.",
                offset + count
            ));
            if !char_limited {
                lines.push(
                    "To remove the match-count limit, omit offset and use head_limit=0.".to_owned(),
                );
            }
        }
        if filtered > 0 && (!kept.is_empty() || partial) {
            footer.push(format!("Filtered {filtered} sensitive file(s)."));
        }
        if !truncated && !partial && offset == 0 && head_limit > 0 && count == head_limit {
            footer.push(format!("Found {count} matches"));
        }
        (lines, footer)
    };
    let notice_chars = [false, true]
        .map(|char_limited| {
            let (lines, footer) = notices(candidates.len(), char_limited);
            js_len(&[lines, footer].concat().join("\n")) + 2
        })
        .into_iter()
        .max()
        .unwrap_or_default();
    let mut remaining = MAX_RETAINED_CHARS as isize - notice_chars as isize;
    let mut shown = Vec::new();
    for path in &candidates {
        let size = js_len(path) as isize + 1;
        if size > remaining {
            break;
        }
        shown.push(path.as_str());
        remaining -= size;
    }
    if !candidates.is_empty() && shown.is_empty() {
        return Outcome::error(
            "Glob cannot fit a complete path and its diagnostics within the output limit. Narrow the search path or pattern.",
        );
    }
    let (lines, footer) = notices(shown.len(), shown.len() < candidates.len());
    let mut output = lines;
    output.extend(shown.into_iter().map(str::to_owned));
    output.extend(footer);
    Outcome::ok(output.join("\n"))
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    r#type: Option<String>,
    output_mode: Option<String>,
    #[serde(rename = "-i", default)]
    ignore_case: bool,
    #[serde(rename = "-n")]
    line_numbers: Option<bool>,
    #[serde(rename = "-A")]
    after: Option<u64>,
    #[serde(rename = "-B")]
    before: Option<u64>,
    #[serde(rename = "-C")]
    context: Option<u64>,
    head_limit: Option<usize>,
    offset: Option<usize>,
    #[serde(default)]
    multiline: bool,
    #[serde(default)]
    include_ignored: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Content,
    Files,
    Count,
}

enum Line {
    Separator,
    Legacy(String),
    Record(String, String),
}

/// Kimi Code's `GrepTool.execution`.
pub async fn grep(builtins: &Builtins, args: &Value) -> Outcome {
    let args: GrepInput = match input("Grep", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let root = match resolve_root(builtins, "Grep", args.path.as_deref()) {
        Ok(root) => root,
        Err(error) => return error,
    };
    let mode = match args.output_mode.as_deref() {
        Some("content") => Mode::Content,
        Some("count_matches") => Mode::Count,
        _ => Mode::Files,
    };
    let numbered = args.line_numbers != Some(false);
    let mut rg = vec!["--hidden".to_owned()];
    if mode != Mode::Content {
        rg.extend(["--max-columns".to_owned(), "500".to_owned()]);
    }
    rg.push("--null".to_owned());
    push_globs(&mut rg, VCS.iter().map(|dir| format!("!{dir}")));
    // A host has no file times to sort on, so rg sorts the files-with-matches list by them instead.
    if mode == Mode::Files && builtins.host().is_some() {
        rg.push("--sortr=modified".to_owned());
    }
    let flags: &[&str] = match mode {
        Mode::Files => &["-l"],
        Mode::Count => &["--count-matches", "--with-filename"],
        Mode::Content => &[],
    };
    rg.extend(flags.iter().map(|flag| (*flag).to_owned()));
    if args.ignore_case {
        rg.push("-i".to_owned());
    }
    if mode == Mode::Content {
        rg.push("--with-filename".to_owned());
        match numbered {
            true => rg.push("-n".to_owned()),
            false => rg.extend(["--field-context-separator".to_owned(), ":".to_owned()]),
        }
        let context = match args.context {
            Some(lines) => vec![("-C", lines)],
            None => [("-A", args.after), ("-B", args.before)]
                .into_iter()
                .filter_map(|(flag, lines)| Some((flag, lines?)))
                .collect(),
        };
        for (flag, lines) in context {
            rg.extend([flag.to_owned(), lines.to_string()]);
        }
    }
    push_globs(&mut rg, args.glob.clone());
    if let Some(kind) = &args.r#type {
        rg.extend(["--type".to_owned(), kind.clone()]);
    }
    if args.multiline {
        rg.extend(["-U".to_owned(), "--multiline-dotall".to_owned()]);
    }
    if args.include_ignored {
        rg.push("--no-ignore".to_owned());
    }
    push_globs(&mut rg, sensitive_globs());
    rg.extend([
        "--".to_owned(),
        args.pattern.clone(),
        root.display().to_string(),
    ]);
    let run = match run_rg(builtins, rg, &builtins.cwd).await {
        Ok(run) => run,
        Err(message) => return Outcome::error(message),
    };
    if run.code != 0 && run.code != 1 && !run.timed_out && !run.truncated {
        return Outcome::error(ripgrep_error(&run));
    }
    let mut stdout = run.stdout.as_str();
    if run.truncated || run.timed_out {
        stdout = &stdout[..complete_end(stdout, mode)];
    }
    if run.timed_out && stdout.trim().is_empty() {
        return Outcome::error(format!(
            "Grep timed out after {TIMEOUT_SECS}s. Try a more specific path or pattern."
        ));
    }
    let workspace = builtins.cwd.display().to_string();
    let mut filtered: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    for line in parse(stdout, mode) {
        if let Some(path) = line_path(&line, mode).filter(|path| sensitive(Path::new(path))) {
            if !filtered.contains(&path) {
                filtered.push(path);
            }
            continue;
        }
        let separator = matches!(line, Line::Separator);
        if mode == Mode::Content
            && separator
            && matches!(lines.last(), None | Some(Line::Separator))
        {
            continue;
        }
        lines.push(line);
    }
    if mode == Mode::Content {
        while matches!(lines.last(), Some(Line::Separator)) {
            lines.pop();
        }
    }
    if mode == Mode::Files && !run.timed_out && builtins.host().is_none() {
        let mut dated = lines
            .into_iter()
            .map(|line| {
                let path = match &line {
                    Line::Record(path, _) | Line::Legacy(path) => Some(path.as_str()),
                    Line::Separator => None,
                };
                (modified(path), line)
            })
            .collect::<Vec<_>>();
        dated.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
        lines = dated.into_iter().map(|(_, line)| line).collect();
    }
    let offset = args.offset.unwrap_or(0);
    let head_limit = args.head_limit.unwrap_or(250);
    let rest = lines.get(offset..).unwrap_or_default();
    let limited = match head_limit {
        0 => rest,
        _ => &rest[..rest.len().min(head_limit)],
    };
    let mut headers = Vec::new();
    let mut messages = Vec::new();
    if !filtered.is_empty() {
        let shown = filtered
            .iter()
            .map(|path| relativize(path, &workspace))
            .collect::<Vec<_>>();
        messages.push(format!(
            "Filtered {} sensitive file(s): {}",
            filtered.len(),
            shown.join(", ")
        ));
    }
    if mode == Mode::Count && !lines.is_empty() {
        headers.push(count_summary(&lines, !filtered.is_empty()));
    }
    if head_limit > 0 && rest.len() > head_limit {
        let total = rest.len() + offset;
        let next = offset + head_limit;
        let set = if run.truncated || run.timed_out {
            " of a partial result set"
        } else {
            ""
        };
        let notice = format!(
            "Results truncated to {head_limit} lines (total: {total}{set}). Use offset={next} to see more."
        );
        match mode {
            Mode::Count => headers.push(notice),
            _ => messages.push(notice),
        }
    }
    if run.truncated {
        messages.push(format!("[Output truncated at {MAX_OUTPUT_BYTES} bytes of rg output — the result set is incomplete. Narrow the pattern, path, or glob filters and re-run to recover complete results.]"));
    }
    if run.timed_out {
        messages.push(format!("Grep timed out after {TIMEOUT_SECS}s; partial results returned. Narrow the path, glob, or pattern and retry for complete results."));
    }
    let body = limited
        .iter()
        .map(|line| display(line, mode, &workspace, mode == Mode::Content && numbered))
        .collect::<Vec<_>>()
        .join("\n");
    let body = match lines.is_empty() && !filtered.is_empty() {
        true => NO_MATCHES.to_owned(),
        false => body,
    };
    let body = match body.is_empty() && headers.is_empty() && messages.is_empty() {
        true => NO_MATCHES.to_owned(),
        false => body,
    };
    headers.push(body);
    headers.extend(messages);
    headers.retain(|part| !part.is_empty());
    Outcome::ok(headers.join("\n"))
}

fn ripgrep_error(run: &Run) -> String {
    let stderr = run.stderr.trim();
    if stderr.is_empty() {
        return format!("Failed to grep: ripgrep exited with code {}", run.code);
    }
    let lines = split_lines(stderr);
    let lines = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let summary = lines
        .iter()
        .rev()
        .find(|line| line.to_lowercase().starts_with("error:"))
        .or(lines.last())
        .map_or("ripgrep error", |line| line);
    let mut text = format!("Failed to grep: {summary}\n\nripgrep stderr:\n{stderr}");
    if run.stderr_truncated {
        text.push_str(&format!("\n[stderr truncated at {MAX_OUTPUT_BYTES} bytes]"));
    }
    text
}

fn modified(path: Option<&str>) -> u64 {
    path.and_then(|path| std::fs::metadata(path).ok()?.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

fn strip_cr(text: &str) -> &str {
    text.strip_suffix('\r').unwrap_or(text)
}

/// `splitRgLines`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = text.split('\n').collect::<Vec<_>>();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines.into_iter().map(strip_cr).collect()
}

/// `parseRipgrepOutput`.
fn parse(text: &str, mode: Mode) -> Vec<Line> {
    if text.is_empty() {
        return Vec::new();
    }
    if !text.contains('\0') {
        return split_lines(text)
            .into_iter()
            .map(|line| match mode == Mode::Content && line == "--" {
                true => Line::Separator,
                false => Line::Legacy(line.to_owned()),
            })
            .collect();
    }
    if mode == Mode::Files {
        return text
            .split('\0')
            .map(strip_cr)
            .filter(|path| !path.is_empty())
            .map(|path| Line::Record(path.to_owned(), String::new()))
            .collect();
    }
    let mut records = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if rest.starts_with('\n') {
            cursor += 1;
            continue;
        }
        if let Some(skip) = ["--\r\n", "--\n"]
            .iter()
            .find(|sep| rest.starts_with(**sep))
        {
            records.push(Line::Separator);
            cursor += skip.len();
            continue;
        }
        let Some(nul) = rest.find('\0') else {
            let tail = strip_cr(rest);
            if !tail.is_empty() {
                records.push(Line::Legacy(tail.to_owned()));
            }
            break;
        };
        let end = rest[nul + 1..].find('\n').map(|at| nul + 1 + at);
        let payload = strip_cr(&rest[nul + 1..end.unwrap_or(rest.len())]);
        records.push(Line::Record(rest[..nul].to_owned(), payload.to_owned()));
        cursor = end.map_or(text.len(), |end| cursor + end + 1);
    }
    records
}

/// `omitIncompleteTrailingRecord`: where the last complete record of a cut-off output ends.
fn complete_end(text: &str, mode: Mode) -> usize {
    if !text.contains('\0') {
        return text.rfind('\n').unwrap_or(0);
    }
    if mode == Mode::Files {
        return text.rfind('\0').map_or(0, |at| at + 1);
    }
    let mut cursor = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if rest.starts_with('\n') {
            cursor += 1;
        } else if rest.starts_with("--\r\n") {
            cursor += 4;
        } else if rest.starts_with("--\n") {
            cursor += 3;
        } else {
            let Some(end) = rest
                .find('\0')
                .and_then(|nul| rest[nul + 1..].find('\n').map(|at| nul + 1 + at))
            else {
                break;
            };
            cursor += end + 1;
        }
    }
    cursor
}

/// `parsedFilePath`. ponytail: Node's `path.normalize` is skipped; rg prints clean absolute paths.
fn line_path(line: &Line, mode: Mode) -> Option<String> {
    match line {
        Line::Separator => None,
        Line::Record(path, _) => Some(path.clone()),
        Line::Legacy(text) => match mode {
            Mode::Files => Some(text.clone()),
            Mode::Count => Some(
                text.rfind(':')
                    .filter(|at| *at > 0)
                    .map_or(text.as_str(), |at| &text[..at])
                    .to_owned(),
            ),
            Mode::Content => content_path(text).map(str::to_owned),
        },
    }
}

/// `extractContentFilePath`: `/^(.*?)([:-])(\d+)\2/`, else everything before the first `:`.
fn content_path(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    for (at, &separator) in bytes.iter().enumerate() {
        if separator != b':' && separator != b'-' {
            continue;
        }
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits > 0 && bytes.get(at + 1 + digits) == Some(&separator) {
            return Some(&line[..at]);
        }
    }
    line.find(':').filter(|at| *at > 0).map(|at| &line[..at])
}

/// `formatDisplayLine`.
fn display(line: &Line, mode: Mode, workspace: &str, numbered: bool) -> String {
    match (line, mode) {
        (Line::Separator, _) => "--".to_owned(),
        (Line::Record(path, _), Mode::Files) => relativize(path, workspace),
        (Line::Record(path, payload), Mode::Count) => {
            format!("{}:{payload}", relativize(path, workspace))
        }
        (Line::Record(path, payload), Mode::Content) => {
            let digits = payload.bytes().take_while(u8::is_ascii_digit).count();
            let context = numbered && digits > 0 && payload.as_bytes().get(digits) == Some(&b'-');
            format!(
                "{}{}{payload}",
                relativize(path, workspace),
                if context { "-" } else { ":" }
            )
        }
        (Line::Legacy(text), Mode::Files) => relativize(text, workspace),
        (Line::Legacy(text), Mode::Count) => match text.rfind(':').filter(|at| *at > 0) {
            Some(at) => format!("{}{}", relativize(&text[..at], workspace), &text[at..]),
            None => text.clone(),
        },
        (Line::Legacy(text), Mode::Content) => match content_path(text) {
            Some(path) => format!("{}{}", relativize(path, workspace), &text[path.len()..]),
            None => text.clone(),
        },
    }
}

/// `formatCountSummary`.
fn count_summary(lines: &[Line], redacted: bool) -> String {
    let (mut matches, mut files) = (0u64, 0u64);
    for line in lines {
        let raw = match line {
            Line::Record(_, payload) => payload.as_str(),
            Line::Legacy(text) => match text.rfind(':').filter(|at| *at > 0) {
                Some(at) => &text[at + 1..],
                None => continue,
            },
            Line::Separator => continue,
        };
        if let Some(count) = js_count(raw) {
            matches += count;
            files += 1;
        }
    }
    format!(
        "Found {matches} {} {} across {files} {}.",
        if redacted {
            "total non-sensitive"
        } else {
            "total"
        },
        if matches == 1 {
            "occurrence"
        } else {
            "occurrences"
        },
        if files == 1 { "file" } else { "files" }
    )
}

/// `Number(raw)` when it is a non-negative safe integer (ripgrep prints plain decimal counts).
fn js_count(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(0);
    }
    trimmed.parse::<u64>().ok().filter(|count| *count < 1 << 53)
}

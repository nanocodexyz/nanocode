use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "tidy_opens.rs"]
mod opens;

const SIMULATED: &[&str] = &[
    "crates/backends/claude/src",
    "crates/core/agent/src",
    "crates/core/durability/src",
    "crates/core/sync/src",
    "crates/core/turnlog/src",
    "crates/core/websocket/src",
    "crates/integrations/store-jsonl/src",
    "crates/integrations/store-sql/src",
    "crates/runtime/agent/src",
    "crates/runtime/code-mode-engine/src",
    "crates/runtime/code-mode/src/code_mode",
    "crates/runtime/durability/src",
    "crates/runtime/subagents/src",
    "crates/runtime/tool-host/src",
];

const BASE_DIR: &str = "crates/patches/djs/crates/djs-prelude/base";

const BYPASSES_HOST: &str = "bypasses nanocode-platform's host";
const WAKE_ORDER: &str = "yields its futures in the order they were woken, which tokio's watch and HashMap iteration draw at random; use nanocode_sync::InOrder";
const REAL_ELAPSED: &str = "reads std's real clock on native, where nanocode_platform::Instant is std::time::Instant; use nanocode_platform::elapsed";
const JOIN_ORDER: &str = "polls in the order its futures were woken (join_all does past 30 futures, through FuturesOrdered); use nanocode_sync::join_in_order or try_join_in_order";

const BANNED: &[(&str, &str)] = &[
    ("SystemTime::now", BYPASSES_HOST),
    ("Instant::now", BYPASSES_HOST),
    (".elapsed()", REAL_ELAPSED),
    ("thread::spawn", BYPASSES_HOST),
    ("thread::Builder", BYPASSES_HOST),
    ("getrandom", BYPASSES_HOST),
    ("nanocode_executor::sleep", BYPASSES_HOST),
    ("nanocode_executor::timeout", BYPASSES_HOST),
    ("nanocode_executor::spawn", BYPASSES_HOST),
    ("nanocode_executor::Builder", BYPASSES_HOST),
    ("nanocode_executor::Handle", BYPASSES_HOST),
    ("FuturesUnordered", WAKE_ORDER),
    ("FuturesOrdered", JOIN_ORDER),
    ("join_all", JOIN_ORDER),
];

const EXECUTOR_GROUP: &str = "use nanocode_executor::{";

struct Allowed {
    path: &'static str,
    call: &'static str,
    uses: usize,
    why: &'static str,
}

const ALLOWED: &[Allowed] = &[
    Allowed {
        path: "crates/core/sync/src/backoff.rs",
        call: "Instant::now",
        uses: 2,
        why: "Backoff::within reads the real clock for its overall deadline; retry delays are caller policy for host I/O outside the simulated crates, never a persisted value",
    },
    Allowed {
        path: "crates/core/sync/src/future.rs",
        call: "FuturesOrdered",
        uses: 3,
        why: "nanocode-sync's own join_all, which keeps futures-util's order: up to 30 futures polled together, past that through FuturesOrdered; simulated crates still use join_in_order",
    },
    Allowed {
        path: "crates/core/sync/src/future.rs",
        call: "join_all",
        uses: 2,
        why: "nanocode-sync's own join_all, which keeps futures-util's order: up to 30 futures polled together, past that through FuturesOrdered; simulated crates still use join_in_order",
    },
    Allowed {
        path: "crates/core/sync/src/stream.rs",
        call: "FuturesOrdered",
        uses: 3,
        why: "nanocode-sync's own FuturesOrdered, which keeps futures-util's order: outputs in push order, futures polled in wake order; simulated crates still join through join_in_order",
    },
    Allowed {
        path: "crates/core/sync/src/stream.rs",
        call: "FuturesUnordered",
        uses: 1,
        why: "nanocode-sync's own FuturesUnordered, which keeps futures-util's order: futures polled in the order they were woken; simulated crates still use InOrder",
    },
    Allowed {
        path: "crates/core/sync/src/unordered.rs",
        call: "FuturesOrdered",
        uses: 8,
        why: "nanocode-sync's own FuturesOrdered, which keeps futures-util's order: outputs in push order, futures polled in wake order; simulated crates still join through join_in_order",
    },
    Allowed {
        path: "crates/core/sync/src/unordered.rs",
        call: "FuturesUnordered",
        uses: 7,
        why: "nanocode-sync's own FuturesUnordered, which keeps futures-util's order: futures polled in the order they were woken; simulated crates still use InOrder",
    },
    Allowed {
        path: "crates/core/turnlog/src/file.rs",
        call: "thread::Builder",
        uses: 1,
        why: "a FileLog's blocking file I/O runs on its owning worker thread behind channels, as AGENTS.md requires; FileLog is native only and no simulated run opens one",
    },
    Allowed {
        path: "crates/integrations/store-jsonl/src/lib.rs",
        call: "Instant::now",
        uses: 5,
        why: "span durations (append, encode, write, queue wait) timed with the real clock, mostly on the writer's own thread where no host runs; they reach tracing fields only",
    },
    Allowed {
        path: "crates/integrations/store-jsonl/src/lib.rs",
        call: ".elapsed()",
        uses: 5,
        why: "span durations measured from the store's own real-clock Instant::now readings, mostly on its writer's thread; they reach tracing fields only",
    },
    Allowed {
        path: "crates/integrations/store-jsonl/src/lib.rs",
        call: "thread::Builder",
        uses: 2,
        why: "blocking file I/O runs on the store's owning writer and syncer threads behind channels, as AGENTS.md requires",
    },
    Allowed {
        path: "crates/runtime/agent/src/runtime.rs",
        call: "thread::Builder",
        uses: 1,
        why: "build_async opens the default store and assembles the runtime on its own thread, off the caller's Tokio worker; a host builds with build",
    },
    Allowed {
        path: "crates/runtime/durability/src/shared_store.rs",
        call: "nanocode_executor::Handle",
        uses: 3,
        why: "without a host, SharedStore drivers run on the runtime a caller gave drive_on, else on one current_thread runtime on their own thread; spawn_driver gives them to an installed host first",
    },
    Allowed {
        path: "crates/runtime/durability/src/shared_store.rs",
        call: "nanocode_executor::Builder",
        uses: 1,
        why: "without a host, SharedStore drivers run on one current_thread runtime on their own thread; spawn_driver gives them to an installed host first",
    },
    Allowed {
        path: "crates/runtime/durability/src/shared_store.rs",
        call: "thread::Builder",
        uses: 1,
        why: "without a host, SharedStore drivers run on one current_thread runtime on their own thread; spawn_driver gives them to an installed host first",
    },
];

fn sources(directory: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, found)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    Ok(())
}

fn grouped(member: &str) -> Option<&'static str> {
    let member = member.trim();
    let name = member.split_whitespace().next().unwrap_or_default();
    if name.starts_with("spawn") {
        return Some("nanocode_executor::spawn");
    }
    if name.starts_with("sleep") {
        return Some("nanocode_executor::sleep");
    }
    if name.starts_with("timeout") {
        return Some("nanocode_executor::timeout");
    }
    if name == "Builder" {
        return Some("nanocode_executor::Builder");
    }
    if name == "Handle" {
        return Some("nanocode_executor::Handle");
    }
    None
}

#[derive(Default)]
struct Group {
    depth: usize,
}

impl Group {
    fn calls(&mut self, line: &str) -> Vec<&'static str> {
        let inside = if self.depth > 0 {
            line
        } else if let Some(at) = line.find(EXECUTOR_GROUP) {
            self.depth = 1;
            &line[at + EXECUTOR_GROUP.len()..]
        } else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        let mut member = String::new();
        let mut at_depth = self.depth;
        for character in inside.chars() {
            match character {
                '{' => {
                    if at_depth == 1 {
                        calls.extend(grouped(&member));
                    }
                    member.clear();
                    self.depth += 1;
                }
                '}' => {
                    if self.depth == 1 {
                        calls.extend(grouped(&member));
                    }
                    member.clear();
                    self.depth -= 1;
                    if self.depth == 0 {
                        break;
                    }
                }
                ',' => {
                    if self.depth == 1 {
                        calls.extend(grouped(&member));
                    }
                    member.clear();
                }
                _ => member.push(character),
            }
            at_depth = self.depth;
        }
        if self.depth == 1 {
            calls.extend(grouped(&member));
        }
        calls
    }
}

fn found(line: &str, group: &mut Group) -> Vec<&'static str> {
    let mut calls: Vec<&'static str> = BANNED
        .iter()
        .map(|(call, _)| *call)
        .filter(|call| line.contains(*call))
        .collect();
    for call in group.calls(line) {
        if !calls.contains(&call) {
            calls.push(call);
        }
    }
    calls
}

fn problem(call: &str) -> &'static str {
    BANNED
        .iter()
        .find(|(banned, _)| *banned == call)
        .map(|(_, problem)| *problem)
        .expect("every reported call is banned")
}

pub fn check(root: &Path) -> std::io::Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut used = vec![0_usize; ALLOWED.len()];
    for crate_sources in SIMULATED {
        let mut files = Vec::new();
        sources(&root.join(crate_sources), &mut files)?;
        for file in files {
            let relative = file
                .strip_prefix(root)
                .expect("a scanned file lies under the workspace root")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&file)?;
            let mut group = Group::default();
            for (number, line) in text.lines().enumerate() {
                for call in found(line, &mut group) {
                    let allowed = ALLOWED
                        .iter()
                        .position(|allowed| allowed.path == relative && allowed.call == call);
                    match allowed {
                        Some(index) => used[index] += 1,
                        None => problems.push(format!(
                            "{relative}:{}: {call} {}",
                            number + 1,
                            problem(call)
                        )),
                    }
                }
            }
        }
    }
    for (allowed, used) in ALLOWED.iter().zip(used) {
        assert!(!allowed.why.is_empty(), "every allowance says why");
        assert!(allowed.uses > 0, "every allowance counts what it allows");
        if used != allowed.uses {
            problems.push(format!(
                "{}: {used} uses of {} where the allowance counts {}",
                allowed.path, allowed.call, allowed.uses
            ));
        }
    }
    problems.extend(locks(root)?);
    problems.extend(held(root)?);
    problems.extend(waits(root)?);
    problems.extend(mutexes(root)?);
    problems.extend(opens::problems(root)?);
    problems.extend(crate::tidy_switches::check(root)?);
    problems.extend(crate::tidy_meter::check(root)?);
    problems.extend(crate::tidy_formats::check(root)?);
    problems.extend(crate::tidy_stamps::check(root)?);
    let base = djs_base::make();
    let base_dir = root.join(BASE_DIR);
    for file in djs_base::stale(&base_dir, &base) {
        problems.push(format!(
            "{BASE_DIR}/{file}: not what this djs-core makes; run crates/patches/djs/scripts/base.sh"
        ));
    }
    Ok(problems)
}

const LOCK_FREE_SQL: &[&str] = &[
    "crates/core/turnlog/src/parted/sql.rs",
    "crates/core/durability/src/sql.rs",
    "crates/core/durability/src/row.rs",
];
const ROW_LOCKS: &[&str] = &[
    "FOR UPDATE",
    "FOR NO KEY UPDATE",
    "FOR SHARE",
    "FOR KEY SHARE",
    "pg_advisory",
    "LOCK TABLE",
    "SET epoch = epoch WHERE",
];

pub fn locks(root: &Path) -> std::io::Result<Vec<String>> {
    let mut problems = Vec::new();
    for path in LOCK_FREE_SQL {
        let text = std::fs::read_to_string(root.join(path))?;
        for (number, line) in text.lines().enumerate() {
            for lock in ROW_LOCKS.iter().filter(|lock| line.contains(**lock)) {
                problems.push(format!(
                    "{path}:{}: {lock} takes a lock beyond the write's own compare-and-set",
                    number + 1
                ));
            }
        }
    }
    Ok(problems)
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const HELD_ROOTS: &[&str] = &["crates", "bin"];
const HELD_SKIPPED: &[&str] = &["crates/patches", "bin/nanocode-sim", "bin/nanocode-fuzz"];
const HELD_SKIPPED_DIRS: &[&str] = &["target", "tests", "benches", "examples", "output"];
const AWAIT: &[u8] = b".await";
const TURN: &str = "Sequencer turn";
const OWNED: &str = "Owned guard";
const PERMIT: &str = "Semaphore::new(1) permit";
const WAITS_ALLOWED: &str = "bin/nanocode-sim/waits-unbounded.txt";
const WAITS_SCANNED: &[&str] = &[
    "crates/integrations/store-sql/src",
    "crates/core/turnlog/src/parted",
];
const WAITS: &[&str] = &[
    "acquire",
    "acquire_owned",
    "acquire_many",
    "acquire_many_owned",
    "connect",
    "connect_reusing",
];
const ACQUIRES: &[&str] = &[
    "acquire_owned",
    "try_acquire_owned",
    "try_acquire",
    "acquire",
];
const MUTEX_ROOTS: &[&str] = &["crates", "bin", "examples"];
const MUTEX_SKIPPED: &[&str] = &["crates/patches"];
const MUTEX_SKIPPED_DIRS: &[&str] = &["target", "output"];
const MUTEX: &[u8] = b"Semaphore::new(";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldGuard {
    pub line: usize,
    pub name: String,
    pub kind: &'static str,
    pub awaited: usize,
}

const fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn literal_end(rest: &[u8]) -> usize {
    let mut at = 1;
    while at < rest.len() {
        match rest[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    rest.len()
}

fn raw_end(rest: &[u8], before: Option<u8>) -> Option<usize> {
    if before.is_some_and(word_byte) || rest.first() != Some(&b'r') {
        return None;
    }
    let hashes = rest[1..].iter().take_while(|byte| **byte == b'#').count();
    if rest.get(1 + hashes) != Some(&b'"') {
        return None;
    }
    let open = 2 + hashes;
    let mut close = vec![b'"'];
    close.extend(std::iter::repeat_n(b'#', hashes));
    Some(find(&rest[open..], &close).map_or(rest.len(), |end| open + end + close.len()))
}

fn char_end(rest: &[u8]) -> usize {
    match rest {
        [_, b'\\', ..] => rest
            .get(3..)
            .and_then(|after| after.iter().position(|byte| *byte == b'\''))
            .map_or(rest.len(), |end| end + 4),
        [_, _, b'\'', ..] => 3,
        _ => 0,
    }
}

pub fn code_only(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut code = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let before = at.checked_sub(1).map(|before| bytes[before]);
        let end = if rest.starts_with(b"//") {
            rest.iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(rest.len())
        } else if rest.starts_with(b"/*") {
            find(&rest[2..], b"*/").map_or(rest.len(), |end| end + 4)
        } else if let Some(end) = raw_end(rest, before) {
            end
        } else if rest[0] == b'"' {
            literal_end(rest)
        } else if rest[0] == b'\'' {
            char_end(rest)
        } else {
            0
        };
        if end == 0 {
            at += 1;
            continue;
        }
        for byte in &mut code[at..at + end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        at += end;
    }
    code
}

fn word_before(code: &[u8], end: usize) -> &[u8] {
    let start = code[..end]
        .iter()
        .rposition(|byte| !word_byte(*byte))
        .map_or(0, |at| at + 1);
    &code[start..end]
}

fn exclusive_kind(line: &[u8]) -> Option<(&[u8], &'static str)> {
    let declared = [
        (&b": Sequencer"[..], TURN),
        (b": Arc<Sequencer>", TURN),
        (b": Owned<", OWNED),
        (b": Arc<Owned<", OWNED),
        (b": Semaphore::new(1)", PERMIT),
        (b": Arc::new(Semaphore::new(1))", PERMIT),
        (b" = Semaphore::new(1)", PERMIT),
        (b" = Arc::new(Semaphore::new(1))", PERMIT),
    ];
    declared.iter().find_map(|(pattern, kind)| {
        let at = find(line, pattern)?;
        let name = word_before(line, at);
        (!name.is_empty()).then_some((name, *kind))
    })
}

pub fn exclusive_names<'a>(
    texts: impl Iterator<Item = &'a [u8]>,
) -> BTreeMap<String, &'static str> {
    let mut names = BTreeMap::new();
    for code in texts {
        for line in code.split(|byte| *byte == b'\n') {
            if let Some((name, kind)) = exclusive_kind(line) {
                names.insert(String::from_utf8_lossy(name).into_owned(), kind);
            }
        }
    }
    names
}

fn acquired(rhs: &[u8], exclusive: &BTreeMap<String, &'static str>) -> Option<&'static str> {
    if let Some(at) = find(rhs, b".enter()") {
        let after = &rhs[at + b".enter()".len()..];
        if after.trim_ascii_start().starts_with(AWAIT) {
            return Some(TURN);
        }
    }
    if find(rhs, b".try_enter()").is_some() {
        return Some(TURN);
    }
    for (at, _) in rhs.iter().enumerate().filter(|(_, byte)| **byte == b'.') {
        let method = word_before(
            rhs,
            at + 1
                + rhs[at + 1..]
                    .iter()
                    .take_while(|byte| word_byte(**byte))
                    .count(),
        );
        if !ACQUIRES.iter().any(|name| name.as_bytes() == method)
            || rhs.get(at + 1 + method.len()) != Some(&b'(')
        {
            continue;
        }
        let mut receiver_end = at;
        if rhs[..receiver_end].ends_with(b".clone()") {
            receiver_end -= b".clone()".len();
        }
        let receiver = String::from_utf8_lossy(word_before(rhs, receiver_end)).into_owned();
        if let Some(kind) = exclusive.get(&receiver) {
            return Some(*kind);
        }
    }
    None
}

fn binding(pattern: &[u8]) -> Option<&[u8]> {
    let mut rest = pattern.trim_ascii_start();
    for prefix in [&b"Some("[..], b"Ok("] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest).trim_ascii_start();
    }
    rest = rest
        .strip_prefix(b"mut ")
        .unwrap_or(rest)
        .trim_ascii_start();
    let length = rest.iter().take_while(|byte| word_byte(**byte)).count();
    let name = &rest[..length];
    (!name.is_empty() && name != b"_").then_some(name)
}

fn statement_end(code: &[u8], from: usize, block: bool) -> Option<usize> {
    let mut depth = 0_usize;
    for (at, byte) in code.iter().enumerate().skip(from) {
        match byte {
            b'{' if depth == 0 && block => return Some(at),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.checked_sub(1)?,
            b';' if depth == 0 => return Some(at),
            _ => {}
        }
    }
    None
}

fn awaited_while_live(code: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut dropped = b"drop(".to_vec();
    dropped.extend_from_slice(name);
    dropped.push(b')');
    let mut depth = 0_usize;
    for at in from..code.len() {
        match code[at] {
            b'{' => depth += 1,
            b'}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        let rest = &code[at..];
        let starts_word = at == 0 || !word_byte(code[at - 1]);
        if rest.starts_with(AWAIT) && !rest.get(AWAIT.len()).copied().is_some_and(word_byte) {
            return Some(at);
        }
        if starts_word && rest.starts_with(&dropped) {
            return None;
        }
    }
    None
}

fn line_of(code: &[u8], at: usize) -> usize {
    code[..at].iter().filter(|byte| **byte == b'\n').count() + 1
}

pub fn held_across_await(
    code: &[u8],
    exclusive: &BTreeMap<String, &'static str>,
) -> Vec<HeldGuard> {
    let mut held = Vec::new();
    let mut from = 0;
    while let Some(found) = find(&code[from..], b"let ") {
        let at = from + found;
        from = at + b"let ".len();
        if at > 0 && word_byte(code[at - 1]) {
            continue;
        }
        let lead = code[..at].trim_ascii_end();
        let block = [&b"if"[..], b"while", b"&&"]
            .iter()
            .any(|word| lead.ends_with(word));
        let Some(end) = statement_end(code, from, block) else {
            continue;
        };
        let statement = &code[from..end];
        let Some(equals) = statement.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (Some(name), Some(kind)) = (
            binding(&statement[..equals]),
            acquired(&statement[equals..], exclusive),
        ) else {
            continue;
        };
        if let Some(awaited) = awaited_while_live(code, end + 1, name) {
            held.push(HeldGuard {
                line: line_of(code, at),
                name: String::from_utf8_lossy(name).into_owned(),
                kind,
                awaited: line_of(code, awaited),
            });
        }
    }
    held
}

fn crate_of(path: &str) -> &str {
    path.find("/src/").map_or(path, |at| &path[..at])
}

pub fn guards_held(sources: &[(String, String)]) -> Vec<(String, HeldGuard)> {
    let code: Vec<(&str, Vec<u8>)> = sources
        .iter()
        .map(|(path, text)| (path.as_str(), code_only(text)))
        .collect();
    let mut by_crate: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, (path, _)) in code.iter().enumerate() {
        by_crate.entry(crate_of(path)).or_default().push(index);
    }
    let mut held = Vec::new();
    for files in by_crate.values() {
        let exclusive = exclusive_names(files.iter().map(|index| code[*index].1.as_slice()));
        for index in files {
            let (path, code) = &code[*index];
            held.extend(
                held_across_await(code, &exclusive)
                    .into_iter()
                    .map(|guard| ((*path).to_owned(), guard)),
            );
        }
    }
    held
}

fn counted(
    listed: &str,
    allowed: &str,
    found: &BTreeMap<String, Vec<String>>,
    what: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in allowed.lines().filter(|line| !line.trim().is_empty()) {
        match line
            .rsplit_once(' ')
            .map(|(path, count)| (path, count.parse::<usize>()))
        {
            Some((path, Ok(count))) if count > 0 => {
                counts.insert(path.to_owned(), count);
            }
            _ => problems.push(format!(
                "{listed}: '{line}' is not '<path> <count above zero>'"
            )),
        }
    }
    for (path, found) in found {
        let allowed = counts.get(path).copied().unwrap_or(0);
        if found.len() > allowed {
            problems.extend(found.iter().map(|problem| {
                format!(
                    "{problem} ({} of {allowed} allowed in {listed})",
                    found.len()
                )
            }));
        }
    }
    for (path, allowed) in &counts {
        let count = found.get(path).map_or(0, Vec::len);
        if count < *allowed {
            problems.push(format!(
                "{listed}: {path} has {count} {what} where it allows {allowed}; lower it to {count}"
            ));
        }
    }
    problems
}

pub fn held_problems(sources: &[(String, String)]) -> Vec<String> {
    guards_held(sources)
        .into_iter()
        .map(|(path, guard)| {
            format!(
                "{path}:{}: {} {} lives across the .await on line {}; release it first or move the state to its owner's task",
                guard.line, guard.kind, guard.name, guard.awaited
            )
        })
        .collect()
}

pub fn bare_waits(code: &[u8]) -> Vec<(usize, String)> {
    let mut waits = Vec::new();
    for (open, _) in code.iter().enumerate().filter(|(_, byte)| **byte == b'(') {
        let call = word_before(code, open);
        let before = &code[..open - call.len()];
        if !WAITS.iter().any(|wait| wait.as_bytes() == call)
            || !(before.ends_with(b".") || before.ends_with(b"::"))
        {
            continue;
        }
        let mut depth = 0_usize;
        let Some(close) = code[open..].iter().position(|byte| {
            match byte {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            depth == 0
        }) else {
            continue;
        };
        let after = code[open + close + 1..].trim_ascii_start();
        if after.starts_with(AWAIT) && !after.get(AWAIT.len()).copied().is_some_and(word_byte) {
            waits.push((
                line_of(code, open),
                String::from_utf8_lossy(call).into_owned(),
            ));
        }
    }
    waits
}

pub fn wait_problems(sources: &[(String, String)], allowed: &str) -> Vec<String> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in sources {
        for (line, call) in bare_waits(&code_only(text)) {
            found.entry(path.clone()).or_default().push(format!(
                "{path}:{line}: {call} is awaited with no deadline; race it against one (nanocode_platform::timeout or a deadline future) and answer Busy"
            ));
        }
    }
    counted(WAITS_ALLOWED, allowed, &found, "waits with no deadline")
}

pub fn waits(root: &Path) -> std::io::Result<Vec<String>> {
    let mut sources = Vec::new();
    for directory in WAITS_SCANNED {
        held_sources(root, &root.join(directory), &mut sources)?;
    }
    Ok(wait_problems(
        &sources,
        &std::fs::read_to_string(root.join(WAITS_ALLOWED))?,
    ))
}

fn held_sources(
    root: &Path,
    directory: &Path,
    found: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    rust_sources(root, directory, HELD_SKIPPED, HELD_SKIPPED_DIRS, found)
}

pub(crate) fn rust_sources(
    root: &Path,
    directory: &Path,
    skipped_paths: &[&str],
    skipped_dirs: &[&str],
    found: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let relative = path
            .strip_prefix(root)
            .expect("a scanned file lies under the workspace root")
            .to_string_lossy()
            .into_owned();
        let skipped = skipped_paths.contains(&relative.as_str())
            || path
                .file_name()
                .is_some_and(|name| skipped_dirs.iter().any(|skipped| name == *skipped));
        if skipped {
            continue;
        }
        if path.is_dir() {
            rust_sources(root, &path, skipped_paths, skipped_dirs, found)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push((relative, std::fs::read_to_string(&path)?));
        }
    }
    Ok(())
}

pub fn held(root: &Path) -> std::io::Result<Vec<String>> {
    let mut sources = Vec::new();
    for directory in HELD_ROOTS {
        held_sources(root, &root.join(directory), &mut sources)?;
    }
    Ok(held_problems(&sources))
}

pub fn mutex_semaphores(code: &[u8]) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut from = 0;
    while let Some(at) = find(&code[from..], MUTEX) {
        let start = from + at;
        from = start + MUTEX.len();
        let Some(rest) = code[from..].trim_ascii_start().strip_prefix(b"1") else {
            continue;
        };
        let rest = rest
            .strip_prefix(b"usize")
            .or_else(|| rest.strip_prefix(b"_usize"))
            .unwrap_or(rest);
        if rest.trim_ascii_start().starts_with(b")") {
            lines.push(1 + code[..start].iter().filter(|byte| **byte == b'\n').count());
        }
    }
    lines
}

pub fn mutex_problems(sources: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, text) in sources {
        for line in mutex_semaphores(&code_only(text)) {
            problems.push(format!(
                "{path}:{line}: Semaphore::new(1) serializes access like a mutex: give the state one owner (an Actor or Mailbox) or a compare-and-set, and size a capacity Semaphore from what it bounds"
            ));
        }
    }
    problems
}

pub fn mutexes(root: &Path) -> std::io::Result<Vec<String>> {
    let mut sources = Vec::new();
    for directory in MUTEX_ROOTS {
        rust_sources(
            root,
            &root.join(directory),
            MUTEX_SKIPPED,
            MUTEX_SKIPPED_DIRS,
            &mut sources,
        )?;
    }
    Ok(mutex_problems(&sources))
}

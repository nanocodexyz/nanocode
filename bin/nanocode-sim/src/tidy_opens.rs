use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{code_only, counted, find, line_of, rust_sources, word_byte};

const OPENS_ROOTS: &[&str] = &["crates", "bin", "examples"];
const OPENS_SKIPPED: &[&str] = &[
    "crates/patches",
    "crates/runtime/scope",
    "crates/core/turnlog/src/adopt.rs",
    "crates/core/turnlog/src/legacy.rs",
    "crates/core/turnlog/src/sim.rs",
    "crates/runtime/agent/src/one_log/legacy.rs",
    "crates/core/turnlog/src/parted",
    "crates/integrations/store-sql/examples/plog.rs",
    "bin/nanocode-sim",
];
const SKIPPED_DIRS: &[&str] = &["target", "output", "node_modules"];
const OPENS: &[&str] = &[
    "TurnLog::open(",
    "TurnLog::follow(",
    "OneLog::open(",
    "OneLog::claim(",
    "legacy::open(",
    "legacy::claim(",
    "PartedLog::claim(",
    ".claim(part",
    ".claim(Partition(",
    ".gain(part",
    ".gain(Partition(",
];
const SIMS_ALLOWED: &str = "bin/nanocode-sim/sims-outside-simulator.txt";
const SIMS_SKIPPED: &[&str] = &["crates/patches", "bin/nanocode-sim"];
const SIM: &[u8] = b"sim";
const SIM_SUFFIX: &[u8] = b"_sim";
const PATH: &[u8] = b"::";
const USE: &[u8] = b"use ";
const ALIAS: &[u8] = b"as";
const LOCAL_ROOTS: &[&[u8]] = &[b"crate", b"super", b"self"];

pub fn problems(root: &Path) -> std::io::Result<Vec<String>> {
    let mut problems = opens(root)?;
    problems.extend(sims(root)?);
    Ok(problems)
}

pub fn per_request_opens(code: &[u8]) -> Vec<(usize, String)> {
    let mut opens = Vec::new();
    for open in OPENS {
        let needle = open.as_bytes();
        let mut from = 0;
        while let Some(at) = find(&code[from..], needle) {
            let start = from + at;
            let bounded = needle[0] == b'.' || start == 0 || !word_byte(code[start - 1]);
            if bounded {
                opens.push((line_of(code, start), (*open).to_owned()));
            }
            from = start + needle.len();
        }
    }
    opens.sort();
    opens
}

pub fn open_problems(sources: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, text) in sources {
        for (line, call) in per_request_opens(&code_only(text)) {
            problems.push(format!(
                "{path}:{line}: {call}..) opens, claims or follows a log outside the scope runtime; reach the scope through nanocode_scope::Scopes::get, ThreadLogs::get or OneLog::over"
            ));
        }
    }
    problems
}

fn opens(root: &Path) -> std::io::Result<Vec<String>> {
    let mut sources = Vec::new();
    for directory in OPENS_ROOTS {
        rust_sources(
            root,
            &root.join(directory),
            OPENS_SKIPPED,
            SKIPPED_DIRS,
            &mut sources,
        )?;
    }
    Ok(open_problems(&sources))
}

fn words(code: &[u8]) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut at = 0;
    while at < code.len() {
        if word_byte(code[at]) {
            let start = at;
            while at < code.len() && word_byte(code[at]) {
                at += 1;
            }
            words.push((start, at));
        } else {
            at += 1;
        }
    }
    words
}

fn names_sim(word: &[u8]) -> bool {
    word == SIM || word.ends_with(SIM_SUFFIX)
}

fn path_root(code: &[u8], start: usize) -> &[u8] {
    let mut root = start;
    let mut at = start;
    while code[..at].ends_with(PATH) {
        let end = at - PATH.len();
        let begin = code[..end]
            .iter()
            .rposition(|byte| !word_byte(*byte))
            .map_or(0, |at| at + 1);
        if begin == end {
            break;
        }
        root = begin;
        at = begin;
    }
    let end = code[root..]
        .iter()
        .position(|byte| !word_byte(*byte))
        .map_or(code.len(), |end| root + end);
    &code[root..end]
}

fn use_root(code: &[u8], start: usize) -> Option<&[u8]> {
    let mut from = 0;
    while let Some(at) = find(&code[from..start], USE) {
        let begin = from + at;
        from = begin + USE.len();
        if begin > 0 && word_byte(code[begin - 1]) {
            continue;
        }
        if code[from..start].contains(&b';') {
            continue;
        }
        let rest = code[from..].trim_ascii_start();
        let rest = rest.strip_prefix(PATH).unwrap_or(rest);
        let end = rest
            .iter()
            .position(|byte| !word_byte(*byte))
            .unwrap_or(rest.len());
        return Some(&rest[..end]);
    }
    None
}

pub fn sim_paths(code: &[u8], crates: &BTreeSet<String>) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let words = words(code);
    for (index, (start, end)) in words.iter().copied().enumerate() {
        let word = &code[start..end];
        if !names_sim(word) {
            continue;
        }
        if index > 0 && &code[words[index - 1].0..words[index - 1].1] == ALIAS {
            continue;
        }
        let root = if code[..start].ends_with(PATH) {
            path_root(code, start)
        } else if let Some(root) = use_root(code, start) {
            root
        } else {
            continue;
        };
        if root == word || LOCAL_ROOTS.contains(&root) {
            continue;
        }
        let root = String::from_utf8_lossy(root).into_owned();
        if crates.contains(&root) {
            let name = String::from_utf8_lossy(word);
            found.push((line_of(code, start), format!("{root}::..::{name}")));
        }
    }
    found
}

pub fn sim_problems(
    sources: &[(String, String)],
    crates: &BTreeSet<String>,
    allowed: &str,
) -> Vec<String> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in sources {
        for (line, named) in sim_paths(&code_only(text), crates) {
            found.entry(path.clone()).or_default().push(format!(
                "{path}:{line}: {named} names another crate's sim module outside bin/nanocode-sim; each crate's sim module is for the simulator only"
            ));
        }
    }
    counted(
        SIMS_ALLOWED,
        allowed,
        &found,
        "sim module paths outside the simulator",
    )
}

fn manifests(directory: &Path, found: &mut BTreeSet<String>) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let skipped = path
            .file_name()
            .is_some_and(|name| SKIPPED_DIRS.iter().any(|skipped| name == *skipped));
        if skipped {
            continue;
        }
        if path.is_dir() {
            manifests(&path, found)?;
        } else if path.file_name().is_some_and(|name| name == "Cargo.toml") {
            for line in std::fs::read_to_string(&path)?.lines() {
                let Some(value) = line.trim().strip_prefix("name = \"") else {
                    continue;
                };
                if let Some(name) = value.strip_suffix('"') {
                    found.insert(name.replace('-', "_"));
                }
            }
        }
    }
    Ok(())
}

fn sims(root: &Path) -> std::io::Result<Vec<String>> {
    let mut crates = BTreeSet::new();
    let mut sources = Vec::new();
    for directory in OPENS_ROOTS {
        manifests(&root.join(directory), &mut crates)?;
        rust_sources(
            root,
            &root.join(directory),
            SIMS_SKIPPED,
            SKIPPED_DIRS,
            &mut sources,
        )?;
    }
    assert!(
        crates.contains("nanocode"),
        "the workspace names its facade crate"
    );
    Ok(sim_problems(
        &sources,
        &crates,
        &std::fs::read_to_string(root.join(SIMS_ALLOWED))?,
    ))
}

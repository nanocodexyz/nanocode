use std::{collections::BTreeMap, path::Path};

use crate::tidy::{code_only, rust_sources};

const ROOT: &str = "bin/nanocode-sim/src";
const SKIPPED: &[&str] = &[];
const SKIPPED_DIRS: &[&str] = &["target", "output"];
const TIDY: &str = "tidy";
const SPAWNS: &[&str] = &["set_child_scope(", "child_scopes()", "start_agent("];
const FACADE: &str = "OneLog::over(";
const ROUTED: &str = ".states()";
const SERVED: &[&str] = &["served_store()", ".sink()"];
const ROUTED_ONLY_ALLOWED: &[(&str, usize)] = &[];

#[derive(Default)]
struct Scenario {
    spawns: bool,
    facade: bool,
    served: bool,
    routed: usize,
}

fn scenario(path: &str) -> Option<String> {
    let inner = path.strip_prefix(ROOT)?.trim_start_matches('/');
    let first = inner.split('/').next()?;
    let name = first.strip_suffix(".rs").unwrap_or(first);
    (!name.starts_with(TIDY)).then(|| name.to_owned())
}

fn scenarios(root: &Path) -> std::io::Result<BTreeMap<String, Scenario>> {
    let mut found = Vec::new();
    rust_sources(root, &root.join(ROOT), SKIPPED, SKIPPED_DIRS, &mut found)?;
    let mut scenarios: BTreeMap<String, Scenario> = BTreeMap::new();
    for (path, text) in found {
        let Some(name) = scenario(&path) else {
            continue;
        };
        let code = String::from_utf8_lossy(&code_only(&text)).into_owned();
        let seen = scenarios.entry(name).or_default();
        seen.spawns |= SPAWNS.iter().any(|spawn| code.contains(spawn));
        seen.facade |= code.contains(FACADE);
        seen.served |= SERVED.iter().any(|served| code.contains(served));
        seen.routed += code.matches(ROUTED).count();
    }
    Ok(scenarios)
}

fn allowed(name: &str) -> usize {
    ROUTED_ONLY_ALLOWED
        .iter()
        .find(|(allowed, _)| *allowed == name)
        .map_or(0, |(_, opens)| *opens)
}

pub fn check(root: &Path) -> std::io::Result<Vec<String>> {
    let scenarios = scenarios(root)?;
    let mut problems = Vec::new();
    for (name, seen) in &scenarios {
        let routed_only = seen.spawns && seen.facade && !seen.served && seen.routed > 0;
        let opens = if routed_only { seen.routed } else { 0 };
        let allowance = allowed(name);
        if opens == allowance {
            continue;
        }
        problems.push(if allowance == 0 {
            format!(
                "{ROOT}/{name}: spawns children or sets child scope while every facade store it opens is over.states() ({opens} opens) and none is served_store() or sink(); a host wires its session store from served_store() and its journal sink from sink(), so make that the default and keep over.states() a minority variant"
            )
        } else {
            format!(
                "{ROOT}/{name}: {opens} routed-only over.states() opens where tidy_wiring.rs allows {allowance}; the allowance only shrinks"
            )
        });
    }
    for (name, _) in ROUTED_ONLY_ALLOWED {
        assert!(!name.is_empty(), "an allowance names its scenario");
        if !scenarios.contains_key(*name) {
            problems.push(format!(
                "{ROOT}/{name}: tidy_wiring.rs allows routed-only opens in a scenario that no longer exists"
            ));
        }
    }
    Ok(problems)
}

use std::{ops::Range, sync::Arc};

use nanocode::turnlog::{OneLog, Policy, ThreadStore};
use nanocode_durability_core::{StoreError, StoreRecord};
use nanocode_turnlog::{LogHost, MemoryLog};

use crate::rng::Rng;
use crate::threadscope::{ThreadScopeStats, World, driven, flag, publish};

const THREAD: &str = "imports";
const OPS: Range<u64> = 20..80;
const STATES: u64 = 3;
const REVISIONS: u64 = 3;
const PAYLOADS: u64 = 2;
const KEYS: u64 = 3;
const VALUES: u64 = 2;

fn shown<T: std::fmt::Debug>(answer: &Result<T, StoreError>) -> String {
    match answer {
        Err(StoreError::NotCommitted(_)) => "Err(NotCommitted)".to_owned(),
        answer => crate::threadturns::normalized(format!("{answer:?}")),
    }
}

fn records(rng: &mut Rng) -> Vec<StoreRecord> {
    let mut records = Vec::new();
    for key in 0..KEYS {
        if rng.chance(600_000) {
            records.push(StoreRecord {
                key: format!("k{key}"),
                value: format!("v{}", rng.below(VALUES)).into(),
            });
        }
    }
    records
}

fn open(world: &World) -> Option<(Arc<OneLog>, ThreadStore)> {
    let host: Arc<dyn LogHost> = Arc::new(MemoryLog::new());
    let one = match driven(
        world,
        "a turn log open",
        nanocode::turnlog::thread_scope_sim::open(host, THREAD, Policy::default()),
    )? {
        Ok(one) => one,
        Err(error) => {
            flag(
                &world.wrong,
                format!("imports: the turn log opens: {error}"),
            );
            return None;
        }
    };
    let node = world.node("imports0");
    publish(world, std::slice::from_ref(&node));
    match driven(world, "a thread get", node.logs.get(THREAD))? {
        Ok(log) => Some((one, ThreadStore::new(Arc::new(log)))),
        Err(error) => {
            flag(&world.wrong, format!("imports: the thread claims: {error}"));
            None
        }
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World, stats: &mut ThreadScopeStats) {
    let Some((one, store)) = open(world) else {
        return;
    };
    for step in 0..rng.range(OPS.start, OPS.end) {
        stats.import_ops += 1;
        let state = format!("s{}", rng.below(STATES));
        let (what, ours, theirs) = if rng.chance(650_000) {
            let revision = rng.below(REVISIONS);
            let payload = format!("p{}", rng.below(PAYLOADS));
            let records = records(rng);
            let Some(ours) = driven(
                world,
                "an import",
                one.import_state(&state, revision, &payload, &records),
            ) else {
                return;
            };
            let Some(theirs) = driven(
                world,
                "an import",
                store.import_state(&state, revision, &payload, &records),
            ) else {
                return;
            };
            ("import", shown(&ours), shown(&theirs))
        } else {
            let Some(ours) = driven(world, "an export", one.export_state(&state)) else {
                return;
            };
            let Some(theirs) = driven(world, "an export", store.export_state(&state)) else {
                return;
            };
            ("export", shown(&ours), shown(&theirs))
        };
        if ours != theirs {
            flag(
                &world.wrong,
                format!(
                    "imports: step {step} {what} of {state} answers {ours} on the turn log but {theirs} on the thread"
                ),
            );
            return;
        }
    }
}

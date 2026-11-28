use std::{collections::BTreeMap, fmt::Debug, ops::Range, sync::Arc};

use nanocode::turnlog::{OneLog, Policy, ThreadHolds, ThreadJournals};
use nanocode_code_mode::code_mode::{
    CELL_JOURNAL_VERSION, CellJournalHeader, CellJournalSink, CellJournalUpdate, JournalEntry,
    JournaledCall, JournaledCallState,
};
use nanocode_durability_core::StateStore;
use nanocode_turnlog::{Effects, LogHost, MemoryLog};
use serde_json::json;

use crate::rng::Rng;
use crate::threadscope::{Node, PARTS, ThreadScopeStats, World, driven, flag, publish};

const THREAD: &str = "journals";
const OPS: Range<u64> = 40..160;
const SESSIONS: u64 = 2;
const CELLS: u64 = 3;
const CATALOGS: u64 = 2;
const ENTRIES_MAX: u64 = 3;
const FINISH_PPM: u64 = 200_000;
const REOPEN_PPM: u64 = 60_000;

struct Sides {
    one: Arc<OneLog>,
    ours: Arc<dyn CellJournalSink>,
    theirs: Arc<dyn CellJournalSink>,
    node: Node,
}

#[derive(Default)]
struct Writer {
    seq: u64,
    entries: usize,
    open: bool,
    next_id: u64,
}

fn shown<T: Debug>(answer: &Option<T>) -> String {
    format!("{answer:?}")
}

fn open(world: &World, host: &Arc<dyn LogHost>, generation: u64) -> Option<Sides> {
    let one = driven(
        world,
        "a turn log open",
        nanocode::turnlog::thread_scope_sim::open(Arc::clone(host), THREAD, Policy::default()),
    );
    let one = match one? {
        Ok(one) => one,
        Err(error) => {
            flag(
                &world.wrong,
                format!("journals: the turn log opens: {error}"),
            );
            return None;
        }
    };
    let node = world.node(&format!("journals{generation}"));
    publish(world, std::slice::from_ref(&node));
    let log = match driven(world, "a thread get", node.logs.get(THREAD))? {
        Ok(log) => log,
        Err(error) => {
            flag(
                &world.wrong,
                format!("journals: the thread claims: {error}"),
            );
            return None;
        }
    };
    let log = Arc::new(log);
    let holds = match ThreadHolds::new(Arc::clone(&log), Effects::default()) {
        Ok(holds) => holds,
        Err(error) => {
            flag(
                &world.wrong,
                format!("journals: the thread's holds open: {error}"),
            );
            return None;
        }
    };
    Some(Sides {
        ours: one.sink(),
        theirs: ThreadJournals::new(log, holds.effects()).sink(),
        one,
        node,
    })
}

fn reopened(
    world: &World,
    host: &Arc<dyn LogHost>,
    sides: Sides,
    generation: u64,
) -> Option<Sides> {
    let flushed = driven(world, "a flush", sides.one.store().flush());
    if let Some(Err(error)) = flushed {
        flag(
            &world.wrong,
            format!("journals: the turn log flushes before reopen {generation}: {error}"),
        );
        return None;
    }
    for part in 0..PARTS {
        let released = driven(
            world,
            "a release",
            sides
                .node
                .logs
                .scopes()
                .release(nanocode_partitions::Partition(part)),
        );
        if let Some(Err(error)) = released {
            flag(
                &world.wrong,
                format!("journals: the thread's node releases partition {part}: {error}"),
            );
            return None;
        }
    }
    drop(sides);
    open(world, host, generation)
}

fn header(rng: &mut Rng, cell: u64) -> CellJournalHeader {
    let catalog = rng.below(CATALOGS);
    CellJournalHeader {
        version: CELL_JOURNAL_VERSION,
        origin_call_id: format!("origin{cell}"),
        script: format!("script {cell}"),
        tools: [json!({ "name": format!("tool{catalog}") })].into(),
        stored: BTreeMap::new(),
        started_at_ms: None,
        prelude: false,
        yield_time_ms: 0,
        max_output_tokens: None,
    }
}

fn entries(rng: &mut Rng, writer: &mut Writer) -> (Vec<JournalEntry>, Vec<JournaledCall>) {
    let mut entries = Vec::new();
    let mut calls = Vec::new();
    for _ in 0..rng.below(ENTRIES_MAX + 1) {
        let id = writer.next_id;
        match rng.below(3) {
            0 => {
                writer.next_id += 1;
                entries.push(JournalEntry::Call {
                    id,
                    name: "deploy".to_owned(),
                    input: json!({ "id": id }),
                    host: false,
                });
                calls.push(JournaledCall {
                    id,
                    call_id: format!("call{id}"),
                    name: "deploy".to_owned(),
                    input: json!({ "id": id }),
                    host: false,
                    state: JournaledCallState::Running,
                });
            }
            1 if id > 0 => entries.push(JournalEntry::Result {
                id: id - 1,
                encoded: format!("result{id}"),
                success: true,
            }),
            _ => entries.push(JournalEntry::Timeout {
                id,
                delay_ms: 10,
                at_ms: id,
            }),
        }
    }
    (entries, calls)
}

fn update(rng: &mut Rng, writers: &mut BTreeMap<(u64, u64), Writer>) -> CellJournalUpdate {
    let (session, cell) = (rng.below(SESSIONS), rng.below(CELLS));
    let writer = writers.entry((session, cell)).or_default();
    let header = (!writer.open).then(|| header(rng, cell));
    if header.is_some() {
        *writer = Writer::default();
        writer.open = true;
    }
    let (entries, calls) = entries(rng, writer);
    let finished = rng.chance(FINISH_PPM);
    let update = CellJournalUpdate {
        session_id: format!("s{session}"),
        cell_id: cell,
        seq: writer.seq,
        header,
        entries_from: writer.entries,
        entries,
        calls: calls.into_iter().map(Arc::new).collect(),
        observer_call_id: None,
        observer_yield_ms: None,
        observer_since_ms: None,
        content_returned: 0,
        finished,
    };
    writer.seq += 1;
    writer.entries += update.entries.len();
    writer.open = !finished;
    update
}

fn op(
    rng: &mut Rng,
    world: &World,
    sides: &Sides,
    writers: &mut BTreeMap<(u64, u64), Writer>,
    id: u64,
) -> (&'static str, String, String) {
    let session = format!("s{}", rng.below(SESSIONS));
    let cell = rng.below(CELLS);
    let (ours, theirs) = (&sides.ours, &sides.theirs);
    match rng.below(6) {
        0..=2 => {
            let update = update(rng, writers);
            (
                "save",
                shown(&driven(world, "a save", ours.save(update.clone()))),
                shown(&driven(world, "a save", theirs.save(update))),
            )
        }
        3 => {
            writers.remove(&(session[1..].parse().expect("a session index"), cell));
            driven(world, "a forget", ours.forget(&session, cell));
            driven(world, "a forget", theirs.forget(&session, cell));
            ("forget", String::new(), String::new())
        }
        4 => {
            let stored = format!("stored{id}");
            (
                "save_stored",
                shown(&driven(
                    world,
                    "a save",
                    ours.save_stored(&session, stored.clone()),
                )),
                shown(&driven(
                    world,
                    "a save",
                    theirs.save_stored(&session, stored),
                )),
            )
        }
        _ => (
            "load_cell",
            shown(&driven(world, "a load", ours.load_cell(&session, cell))),
            shown(&driven(world, "a load", theirs.load_cell(&session, cell))),
        ),
    }
}

fn compared(world: &World, sides: &Sides, step: u64) {
    for session in 0..SESSIONS {
        let session = format!("s{session}");
        let reads = [
            (
                "journals",
                shown(&driven(world, "a load", sides.ours.load(&session))),
                shown(&driven(world, "a load", sides.theirs.load(&session))),
            ),
            (
                "stored",
                shown(&driven(world, "a load", sides.ours.load_stored(&session))),
                shown(&driven(world, "a load", sides.theirs.load_stored(&session))),
            ),
        ];
        for (what, ours, theirs) in reads {
            if ours != theirs {
                flag(
                    &world.wrong,
                    format!(
                        "journals: after step {step} the turn log's {what} of {session} are {ours} but the thread's {theirs}"
                    ),
                );
                return;
            }
        }
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World, stats: &mut ThreadScopeStats) {
    let host: Arc<dyn LogHost> = Arc::new(MemoryLog::new());
    let mut generation = 0;
    let mut writers = BTreeMap::new();
    let Some(mut sides) = open(world, &host, generation) else {
        return;
    };
    for step in 0..rng.range(OPS.start, OPS.end) {
        if rng.chance(REOPEN_PPM) {
            generation += 1;
            stats.journal_reopens += 1;
            let Some(next) = reopened(world, &host, sides, generation) else {
                return;
            };
            sides = next;
            continue;
        }
        stats.journal_ops += 1;
        let (what, ours, theirs) = op(rng, world, &sides, &mut writers, step);
        if ours != theirs {
            flag(
                &world.wrong,
                format!(
                    "journals: step {step} {what} answers {ours} on the turn log but {theirs} on the thread"
                ),
            );
            return;
        }
        compared(world, &sides, step);
        if world.wrong.read(Option::is_some) {
            return;
        }
    }
}

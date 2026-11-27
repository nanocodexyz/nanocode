use std::{
    collections::BTreeMap,
    future::Future,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nanocode::{
    backend::Prompt,
    session_host::{LogEntry, Started, Submission, Submitted, TurnRecord, TurnState},
    turnlog::{
        OneLog, OneLogOver, Policy, ThreadCommand, ThreadLogs, ThreadPath, ThreadPaths,
        ThreadState,
        thread_scope_sim::{self, Mutation},
    },
};
use nanocode_durability_core::{
    Fact, FactLog, FactLogs, Folded, ModelOut, StoreError, commit_fact, fold_facts,
};
use nanocode_scope::Backfilled;
use nanocode_scope::{
    Resident, ScopeError, sim as scope_sim, sim::Mutation as ScopeMutation, switches,
};
use nanocode_sync::future::{join, join_all};
use nanocode_turnlog::{
    Format, Formats, LogHost, SCAN_END,
    adopt::{self, Reading},
    sim as log_sim,
};
use serde_json::{Map, json};

use crate::{
    admitturns::{self, Slow},
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
    threadcompat, threadlegacy,
    threadscope::{Node, PARTS, World, driven, flag, publish, raw, reach},
    threadsessions::{self, Sessions, Transcript},
};

const THREAD: &str = "flips";
const ROUNDS: Range<u64> = 3..9;
const OPS: Range<u64> = 2..10;
const CRASH_PPM: u64 = 250_000;
const HANDOFF_PPM: u64 = 250_000;
const LEGACY_PPM: u64 = 500_000;
const START_LEGACY_PPM: u64 = 500_000;
const START_STAMPED_PPM: u64 = 500_000;
const SESSIONS_PPM: u64 = 500_000;
const RECENT_ROWS: usize = 1 << 16;
const DRIVEN: [&str; 2] = ["thread_path", "row_format"];
const RESTARTS: u64 = 2 * ROUNDS.end;
const QUIET_PASSES: u64 = 2;
const RACE_PPM: u64 = 500_000;
const RACE_READS: Range<u64> = 0..24;
const RACE_START_PPM: u64 = 500_000;
const SWITCHES: u64 = 2;
const ADOPTION_PASSES: u64 = 4;
const SNAPSHOT_PPM: u64 = 750_000;
const SNAPSHOT_ROWS: Range<u64> = 2..8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipsMutation {
    None,
    FlipOffUnreadable,
    LegacyOpenClaims,
    ResidentDropsBlob,
    ClaimSkipsRow,
    AdoptedServesLegacy,
    BackfillRereadsHistory,
    SwitchMissesTail,
    HandedOverWrites,
    ServedHandlesPinned,
}

impl FlipsMutation {
    const ALL: [Self; 10] = [
        Self::None,
        Self::FlipOffUnreadable,
        Self::LegacyOpenClaims,
        Self::ResidentDropsBlob,
        Self::ClaimSkipsRow,
        Self::AdoptedServesLegacy,
        Self::BackfillRereadsHistory,
        Self::SwitchMissesTail,
        Self::HandedOverWrites,
        Self::ServedHandlesPinned,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::FlipOffUnreadable => "flip_off_unreadable",
            Self::LegacyOpenClaims => "legacy_open_claims",
            Self::ResidentDropsBlob => "resident_drops_blob",
            Self::ClaimSkipsRow => "claim_skips_row",
            Self::AdoptedServesLegacy => "adopted_serves_legacy",
            Self::BackfillRereadsHistory => "backfill_rereads_history",
            Self::SwitchMissesTail => "switch_misses_tail",
            Self::HandedOverWrites => "handed_over_writes",
            Self::ServedHandlesPinned => "served_handles_pinned",
        }
    }

    const fn mutation(self) -> Mutation {
        match self {
            Self::None => Mutation::None,
            Self::FlipOffUnreadable => Mutation::FlipOffUnreadable,
            Self::LegacyOpenClaims => Mutation::LegacyOpenClaims,
            Self::ResidentDropsBlob => Mutation::ResidentDropsBlob,
            Self::ClaimSkipsRow => Mutation::ClaimSkipsRow,
            Self::AdoptedServesLegacy => Mutation::AdoptedServesLegacy,
            Self::BackfillRereadsHistory => Mutation::None,
            Self::SwitchMissesTail => Mutation::SwitchMissesTail,
            Self::HandedOverWrites => Mutation::HandedOverWrites,
            Self::ServedHandlesPinned => Mutation::ServedHandlesPinned,
        }
    }

    const fn scope(self) -> ScopeMutation {
        match self {
            Self::BackfillRereadsHistory => ScopeMutation::BackfillRereadsHistory,
            _ => ScopeMutation::None,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .filter(|mutation| **mutation != Self::None)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FlipsStats {
    pub seeds: u64,
    pub flips: u64,
    pub ons: u64,
    pub offs: u64,
    pub format_flips: u64,
    pub crashes: u64,
    pub handoffs: u64,
    pub acked: u64,
    pub checked: u64,
    pub sessions: u64,
    pub held: u64,
    pub raced: u64,
    pub resumed: u64,
    pub bounded: u64,
    pub missed: u64,
}

impl FlipsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.flips += other.flips;
        self.ons += other.ons;
        self.offs += other.offs;
        self.format_flips += other.format_flips;
        self.crashes += other.crashes;
        self.handoffs += other.handoffs;
        self.acked += other.acked;
        self.checked += other.checked;
        self.sessions += other.sessions;
        self.held += other.held;
        self.raced += other.raced;
        self.resumed += other.resumed;
        self.bounded += other.bounded;
        self.missed += other.missed;
    }
}

struct Switched(AtomicBool);

impl ThreadPaths for Switched {
    fn path(&self, _thread: &str) -> ThreadPath {
        if self.0.load(Ordering::Relaxed) {
            ThreadPath::Legacy
        } else {
            ThreadPath::Resident
        }
    }
}

impl Switched {
    fn flip(&self) -> ThreadPath {
        let legacy = !self.0.load(Ordering::Relaxed);
        self.0.store(legacy, Ordering::Relaxed);
        self.path(THREAD)
    }
}

struct Run {
    paths: Arc<Switched>,
    policy: Policy,
    node: Node,
    slow: Arc<Slow>,
    over: Arc<OneLogOver>,
    generation: u64,
    stamped: bool,
    turns: u64,
    ids: Vec<String>,
    acked: BTreeMap<String, Acked>,
    sessions: Option<Sessions>,
    adopted: Option<Adoptions>,
}

#[derive(Clone, Copy, Debug)]
struct Adoptions {
    first: u64,
    reads: u64,
    opens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Acked {
    state: TurnState,
    durable: bool,
}

fn registered(world: &World) -> bool {
    for switch in switches::ON_DISK {
        if switch.flipped().scenario() != "flips" {
            flag(
                &world.wrong,
                format!(
                    "flips: on-disk switch {} names scenario {} as its proof, not flips",
                    switch.name(),
                    switch.flipped().scenario()
                ),
            );
            return false;
        }
        if !DRIVEN.contains(&switch.name()) {
            flag(
                &world.wrong,
                format!("flips: on-disk switch {} has no driver", switch.name()),
            );
            return false;
        }
    }
    let versions: Vec<String> = Formats::all()
        .iter()
        .map(|(format, _)| format.version().to_string())
        .collect();
    if switches::ROW_FORMAT.settings() != versions.as_slice() {
        flag(
            &world.wrong,
            format!(
                "flips: the row format switch's settings {:?} are not the registered formats {versions:?}",
                switches::ROW_FORMAT.settings()
            ),
        );
        return false;
    }
    for path in ThreadPath::ALL {
        if ThreadPath::named(path.setting()) != Some(path) {
            flag(
                &world.wrong,
                format!("flips: thread path {path:?} is not a setting of its switch"),
            );
            return false;
        }
    }
    true
}

fn opened(
    world: &World,
    node: &Node,
    paths: &Arc<Switched>,
    policy: &Policy,
) -> Option<Arc<OneLogOver>> {
    let logs = Arc::new(
        node.logs
            .clone()
            .with_paths(Arc::clone(paths) as Arc<dyn ThreadPaths>),
    );
    let legacy = paths.path(THREAD) == ThreadPath::Legacy;
    let loads = node.logs.scopes().stats().loads;
    match driven(world, "a flips open", OneLog::over(logs, THREAD, policy))? {
        Ok(_) if legacy && node.logs.scopes().stats().loads != loads => {
            flag(
                &world.wrong,
                "flips: a thread on the legacy path loads its scope when the facade opens"
                    .to_owned(),
            );
            None
        }
        Ok(over) => Some(over),
        Err(error) => {
            flag(&world.wrong, format!("flips: the thread opens: {error}"));
            None
        }
    }
}

fn acked(run: &mut Run, record: &TurnRecord, durable: bool) {
    let state = record.state;
    run.acked
        .insert(record.request_id.clone(), Acked { state, durable });
}

fn failed(world: &World, run: &Run, what: &str, error: &StoreError) {
    flag(
        &world.wrong,
        format!(
            "flips: {what} on the {} path answers {error}",
            run.paths.path(THREAD).setting()
        ),
    );
}

fn submit(world: &World, run: &mut Run) -> Option<()> {
    let id = format!("f{}", run.turns);
    run.turns += 1;
    let submission = Submission {
        request_id: id.clone(),
        prompt: Prompt::new(format!("flip turn {id}")),
        hand: None,
        metadata: raw(&json!(null)),
    };
    match driven(world, "a flips submit", run.over.submit(&submission))? {
        Ok(Submitted::Accepted { record, .. }) => {
            run.ids.push(id);
            acked(run, &record, true);
            Some(())
        }
        Ok(answer) => {
            flag(
                &world.wrong,
                format!("flips: a new turn {id} is answered {answer:?}"),
            );
            None
        }
        Err(error) => {
            failed(world, run, "a submit", &error);
            None
        }
    }
}

fn live(run: &Run, rng: &mut Rng, state: TurnState) -> Option<String> {
    let live: Vec<&String> = run
        .acked
        .iter()
        .filter(|(_, held)| held.state == state)
        .map(|(id, _)| id)
        .collect();
    if live.is_empty() {
        return None;
    }
    live.get(rng.pick(live.len())).map(|id| (*id).clone())
}

fn claimed(world: &World, run: &Run) -> Option<()> {
    if run.over.path() != ThreadPath::Legacy {
        return Some(());
    }
    let stale = thread_scope_sim::open(run.node.logs.scopes().legacy(), THREAD, Policy::default());
    let stale = match driven(world, "a stale old writer", stale)? {
        Ok(stale) => stale,
        Err(error) => {
            failed(world, run, "a stale open", &error);
            return None;
        }
    };
    if let Err(error) = driven(world, "a flips claim", run.over.claim())? {
        failed(world, run, "a claim", &error);
        return None;
    }
    let submission = Submission {
        request_id: format!("stale{}", run.turns),
        prompt: Prompt::new("a stale writer's turn"),
        hand: None,
        metadata: raw(&json!(null)),
    };
    match driven(world, "a stale submit", stale.submit(&submission))? {
        Err(StoreError::Fenced) => Some(()),
        answer => {
            flag(
                &world.wrong,
                format!("flips: an old writer opened before a claim writes after it: {answer:?}"),
            );
            None
        }
    }
}

fn op(rng: &mut Rng, world: &World, run: &mut Run) -> Option<()> {
    let fields = Map::new();
    match rng.below(5) {
        0 => submit(world, run),
        4 => claimed(world, run),
        1 => {
            let Some(id) = live(run, rng, TurnState::Queued) else {
                return submit(world, run);
            };
            match driven(world, "a flips start", run.over.start(&id))? {
                Ok(Some(started)) => acked(run, &started.record, false),
                Ok(None) => {}
                Err(error) => failed(world, run, "a start", &error),
            }
            Some(())
        }
        2 => {
            let Some(id) = live(run, rng, TurnState::Running) else {
                return submit(world, run);
            };
            let entry = LogEntry {
                request_id: Some(id.clone()),
                event: raw(&json!({ "type": "flip", "turn": id })),
            };
            if let Err(error) = driven(world, "a flips append", run.over.append(&[entry]))? {
                failed(world, run, "an append", &error);
                return None;
            }
            match driven(
                world,
                "a flips finish",
                run.over.finish(&id, TurnState::Completed, &fields),
            )? {
                Ok(Some((record, _))) => acked(run, &record, true),
                Ok(None) => {}
                Err(error) => failed(world, run, "a finish", &error),
            }
            Some(())
        }
        _ => {
            let Some(id) = live(run, rng, TurnState::Queued) else {
                return submit(world, run);
            };
            match driven(world, "a flips cancel", run.over.cancel(&id, &fields))? {
                Ok(Some((record, _))) => acked(run, &record, true),
                Ok(None) => {}
                Err(error) => failed(world, run, "a cancel", &error),
            }
            Some(())
        }
    }
}

fn decoded(world: &World) -> Option<ThreadState> {
    let host = world.host();
    let fold = world.fold();
    match driven(world, "a flips adoption", fold.adopt(&host, THREAD))? {
        Ok(Some(adopted)) => return Some(adopted.state),
        Ok(None) => {}
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: a resident claim would not adopt the thread: {error}"),
            );
            return None;
        }
    }
    let mut state = fold.empty();
    for (seq, row) in world.rows(THREAD) {
        if let Err(error) = fold.apply(&mut state, seq, &row) {
            flag(
                &world.wrong,
                format!("flips: the resident fold refuses row {seq}: {error}"),
            );
            return None;
        }
    }
    Some(state)
}

fn held(world: &World, run: &mut Run, lossy: bool, when: &str) -> Option<()> {
    let acked: Vec<(String, Acked)> = run
        .acked
        .iter()
        .map(|(id, acked)| (id.clone(), *acked))
        .collect();
    for (id, acked) in acked {
        let answer = driven(world, "a flips turn read", run.over.turn(&id))?;
        let read = match &answer {
            Ok(Some(record)) => Some(record.state),
            Ok(None) | Err(_) => None,
        };
        let staged = lossy
            && !acked.durable
            && acked.state == TurnState::Running
            && read == Some(TurnState::Queued);
        if staged {
            run.acked.insert(
                id,
                Acked {
                    state: TurnState::Queued,
                    durable: true,
                },
            );
            continue;
        }
        if read != Some(acked.state) {
            flag(
                &world.wrong,
                format!(
                    "flips: {when}, turn {id} was answered {} ({}) and the {} path reads {answer:?}",
                    acked.state.as_str(),
                    if acked.durable { "durable" } else { "staged" },
                    run.over.path().setting()
                ),
            );
            return None;
        }
    }
    Some(())
}

fn agree(
    world: &World,
    run: &mut Run,
    lossy: bool,
    when: &str,
    stats: &mut FlipsStats,
) -> Option<()> {
    formats_agree(world, when)?;
    held(world, run, lossy, when)?;
    let host: Arc<dyn LogHost> = world.host();
    let state = decoded(world)?;
    threadcompat::resident(world, THREAD, &host, &state, &run.ids, when)?;
    if run.over.path() == ThreadPath::Resident {
        let log = match reach(world, &run.node, THREAD)? {
            Ok(log) => log,
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("flips: {when}, the resident thread is not reached: {error}"),
                );
                return None;
            }
        };
        let Some(state) = log.state() else {
            flag(
                &world.wrong,
                format!("flips: {when}, the resident thread is not local"),
            );
            return None;
        };
        threadcompat::resident(world, THREAD, &host, &state, &run.ids, when)?;
    }
    stats.checked += 1;
    Some(())
}

fn formats_agree(world: &World, when: &str) -> Option<()> {
    let rows = world.rows(THREAD);
    let mut inner = Vec::with_capacity(rows.len());
    for (seq, row) in &rows {
        match nanocode_turnlog::tagged_row(row) {
            Ok(tagged) => inner.push((*seq, tagged.to_owned())),
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("flips: {when}, row {seq} opens to no format 1 row: {error}"),
                );
                return None;
            }
        }
    }
    let both = nanocode_turnlog::rows(&rows)
        .and_then(|stamped| nanocode_turnlog::rows(&inner).map(|tagged| (stamped, tagged)));
    match both {
        Ok((stamped, tagged)) => {
            let differ = stamped.iter().zip(&tagged).find(|(stamped, tagged)| {
                stamped.seq != tagged.seq || stamped.plain != tagged.plain
            });
            if let Some((row, _)) = differ {
                flag(
                    &world.wrong,
                    format!(
                        "flips: {when}, row {} decodes to other records with its stamp than without",
                        row.seq
                    ),
                );
                return None;
            }
            if stamped.len() != tagged.len() {
                flag(
                    &world.wrong,
                    format!(
                        "flips: {when}, the log decodes to {} rows with stamps and {} without",
                        stamped.len(),
                        tagged.len()
                    ),
                );
                return None;
            }
        }
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: {when}, the registry refuses the log: {error}"),
            );
            return None;
        }
    }
    threadcompat::previous_rows(world, &inner, when)
}

fn formatted(world: &World, run: &mut Run, stats: &mut FlipsStats) -> Option<()> {
    run.stamped = !run.stamped;
    log_sim::force(Some(row_format(run.stamped)));
    stats.format_flips += 1;
    agree(
        world,
        run,
        false,
        &format!(
            "after a flip to row format {}",
            row_format(run.stamped).version()
        ),
        stats,
    )
}

const fn row_format(stamped: bool) -> Format {
    if stamped {
        Format::Stamped
    } else {
        Format::Tagged
    }
}

fn missed(world: &World, node: &Node) -> Option<()> {
    let log = match reach(world, node, THREAD)? {
        Ok(log) => log,
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: a node after a crash reaches the thread: {error}"),
            );
            return None;
        }
    };
    if log.state().is_some() {
        return Some(());
    }
    match driven(world, "a flips forward", log.send(ThreadCommand::Pending))? {
        Err(ScopeError::Unreachable { .. }) => Some(()),
        answer => {
            flag(
                &world.wrong,
                format!("flips: a forward to a crashed holder answers {answer:?}"),
            );
            None
        }
    }
}

fn moved(rng: &mut Rng, world: &World, run: &mut Run, stats: &mut FlipsStats) -> Option<()> {
    let crash = rng.chance(CRASH_PPM);
    let legacy = run.over.path() == ThreadPath::Legacy;
    if !crash && !rng.chance(HANDOFF_PPM) {
        return Some(());
    }
    if crash {
        stats.crashes += 1;
        world.expire();
    } else {
        stats.handoffs += 1;
        if run.over.path() == ThreadPath::Resident {
            driven(world, "a flips drain", run.node.logs.scopes().drain(THREAD))?.ok()?;
        }
        for part in 0..PARTS {
            let released = driven(
                world,
                "a flips release",
                run.node
                    .logs
                    .scopes()
                    .release(nanocode_partitions::Partition(part)),
            )?;
            if let Err(error) = released {
                flag(
                    &world.wrong,
                    format!("flips: a handoff releases partition {part}: {error}"),
                );
                return None;
            }
        }
    }
    run.generation += 1;
    let (node, slow) = paced(world, &format!("f{}", run.generation));
    publish(world, std::slice::from_ref(&node));
    if crash {
        missed(world, &node)?;
    }
    run.over = opened(world, &node, &run.paths, &run.policy)?;
    run.node = node;
    run.slow = slow;
    let lossy = crash || legacy;
    agree(world, run, lossy, "after a node moved", stats)
}

fn round(rng: &mut Rng, world: &World, run: &mut Run, stats: &mut FlipsStats) -> Option<()> {
    submit(world, run)?;
    let ops = rng.range(OPS.start, OPS.end);
    let flip_at = rng.below(ops);
    for at in 0..ops {
        if at == flip_at {
            formatted(world, run, stats)?;
        }
        op(rng, world, run)?;
    }
    moved(rng, world, run, stats)?;
    let to = run.paths.flip();
    match to {
        ThreadPath::Resident => stats.ons += 1,
        ThreadPath::Legacy => stats.offs += 1,
    }
    stats.flips += 1;
    if let Err(error) = driven(world, "a flips pending read", run.over.pending())? {
        failed(world, run, "the read after a flip", &error);
        return None;
    }
    if run.over.path() != to {
        backfilled(rng, world, run, stats)?;
    }
    if run.over.path() != to {
        flag(
            &world.wrong,
            format!(
                "flips: the host flipped the thread to {} and the facade serves {}",
                to.setting(),
                run.over.path().setting()
            ),
        );
        return None;
    }
    agree(
        world,
        run,
        false,
        &format!("after a flip to {}", to.setting()),
        stats,
    )?;
    sessions(rng, world, run, stats)
}

fn paced(world: &World, name: &str) -> (Node, Arc<Slow>) {
    let mut kept = None;
    let node = world.node_wrapped(name, |log| {
        let capacity = log.read_capacity();
        let slow = Arc::new(Slow::new(log, capacity));
        kept = Some(Arc::clone(&slow));
        (slow as Arc<dyn LogHost>, None)
    });
    let slow = kept.expect("the node wraps its host once");
    (node, slow)
}

fn head(world: &World) -> u64 {
    world.rows(THREAD).last().map_or(0, |(seq, _)| seq + 1)
}

fn opened_rows(world: &World) -> Option<u64> {
    let host = world.host();
    let opened = driven(
        world,
        "an old writer's open",
        adopt::read_all(&host, THREAD, Reading::Below(SCAN_END)),
    )?;
    match opened {
        Ok(opened) => Some(opened.usage().rows),
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: the old writer opens the thread: {error}"),
            );
            None
        }
    }
}

fn bounded(
    world: &World,
    run: &mut Run,
    read: u64,
    from: u64,
    stats: &mut FlipsStats,
) -> Option<()> {
    let Some(adoptions) = run.adopted.as_mut() else {
        run.adopted = Some(Adoptions {
            first: from,
            reads: 0,
            opens: 0,
        });
        return Some(());
    };
    adoptions.reads += read;
    adoptions.opens += opened_rows(world)?;
    let written = head(world).saturating_sub(adoptions.first);
    let bound = ADOPTION_PASSES * (adoptions.opens + written);
    if adoptions.reads > bound {
        flag(
            &world.wrong,
            format!(
                "flips: the thread's adoptions after its first read {} rows, past {ADOPTION_PASSES} passes over one old-writer open each ({} rows) and the {written} rows written since the first",
                adoptions.reads, adoptions.opens
            ),
        );
        return None;
    }
    stats.bounded += 1;
    Some(())
}

enum Raced {
    Submitted(String, Result<Submitted, StoreError>),
    Started(Result<Option<Started>, StoreError>),
}

fn raced(rng: &mut Rng, world: &World, run: &mut Run) -> Option<Backfilled> {
    let late_ms = rng.range(RACE_READS.start, RACE_READS.end) * admitturns::READ_MS;
    let queued = live(run, rng, TurnState::Queued).filter(|_| rng.chance(RACE_START_PPM));
    let id = format!("r{}", run.turns);
    run.turns += 1;
    let submission = Submission {
        request_id: id.clone(),
        prompt: Prompt::new(format!("raced turn {id}")),
        hand: None,
        metadata: raw(&json!(null)),
    };
    let scopes = run.node.logs.scopes();
    let over = Arc::clone(&run.over);
    run.slow.paced(true);
    let raced = driven(
        world,
        "a backfill raced by the old writer",
        join(scopes.backfill(), async {
            nanocode_platform::sleep(Duration::from_millis(late_ms)).await;
            match &queued {
                Some(queued) => Raced::Started(over.start(queued).await),
                None => Raced::Submitted(id, over.submit(&submission).await),
            }
        }),
    );
    run.slow.paced(false);
    let (passed, wrote) = raced?;
    match wrote {
        Raced::Submitted(id, Ok(Submitted::Accepted { record, .. })) => {
            run.ids.push(id);
            acked(run, &record, true);
        }
        Raced::Submitted(id, Ok(answer)) => {
            flag(
                &world.wrong,
                format!("flips: a raced turn {id} is answered {answer:?}"),
            );
            return None;
        }
        Raced::Started(Ok(Some(started))) => acked(run, &started.record, false),
        Raced::Started(Ok(None)) => {}
        Raced::Submitted(_, Err(error)) | Raced::Started(Err(error)) => {
            failed(world, run, "a raced write", &error);
            return None;
        }
    }
    Some(passed)
}

const STORM_PPM: u64 = 750_000;
const STORM_TURNS: Range<u64> = 1..4;
const STORM_BESIDE: Range<u64> = 0..4;
const STORM_STEP_READS: Range<u64> = 0..8;

type Stormed = Result<Vec<(TurnRecord, bool)>, String>;

struct Storm {
    over: Arc<OneLogOver>,
    facts: Arc<dyn FactLogs>,
    id: String,
    late_ms: u64,
    step_ms: u64,
}

async fn storm_fact(
    scoped: &dyn FactLog,
    fold: Folded<Transcript>,
    id: &str,
) -> Result<Folded<Transcript>, String> {
    let fact = Fact::ModelOut(ModelOut {
        turn: id.to_owned(),
        response: raw(&json!(format!("storm {id}"))),
    });
    commit_fact(scoped, fold, fact)
        .await
        .map(|(fold, _)| fold)
        .map_err(|error| format!("a session fact of {id} answers {error}"))
}

async fn storm_turn(storm: Storm) -> Stormed {
    let Storm {
        over,
        facts,
        id,
        late_ms,
        step_ms,
    } = storm;
    let step = Duration::from_millis(step_ms);
    nanocode_platform::sleep(Duration::from_millis(late_ms)).await;
    let scoped = facts.scoped(&format!("storm.{id}"), false);
    let mut fold = fold_facts::<Transcript>(&*scoped)
        .await
        .map_err(|error| format!("the session fold of {id} answers {error}"))?;
    let submission = Submission {
        request_id: id.clone(),
        prompt: Prompt::new(format!("storm turn {id}")),
        hand: None,
        metadata: raw(&json!(null)),
    };
    let mut acked = Vec::new();
    match over.submit(&submission).await {
        Ok(Submitted::Accepted { record, .. }) => acked.push((record, true)),
        answer => return Err(format!("a submit of {id} answers {answer:?}")),
    }
    nanocode_platform::sleep(step).await;
    match over.start(&id).await {
        Ok(Some(started)) => acked.push((started.record, false)),
        answer => return Err(format!("a start of {id} answers {answer:?}")),
    }
    fold = storm_fact(&*scoped, fold, &id).await?;
    nanocode_platform::sleep(step).await;
    let entry = LogEntry {
        request_id: Some(id.clone()),
        event: raw(&json!({ "type": "storm", "turn": id })),
    };
    over.append(&[entry])
        .await
        .map_err(|error| format!("an append to {id} answers {error}"))?;
    drop(storm_fact(&*scoped, fold, &id).await?);
    nanocode_platform::sleep(step).await;
    match over.finish(&id, TurnState::Completed, &Map::new()).await {
        Ok(Some((record, _))) => acked.push((record, true)),
        answer => return Err(format!("a finish of {id} answers {answer:?}")),
    }
    Ok(acked)
}

async fn storm_stale(old: Arc<OneLog>, id: String, late_ms: u64) -> Stormed {
    nanocode_platform::sleep(Duration::from_millis(late_ms)).await;
    let submission = Submission {
        request_id: id.clone(),
        prompt: Prompt::new(format!("stale turn {id}")),
        hand: None,
        metadata: raw(&json!(null)),
    };
    match old.submit(&submission).await {
        Ok(Submitted::Accepted { record, .. }) => Ok(vec![(record, true)]),
        Err(StoreError::Fenced) => Ok(Vec::new()),
        answer => Err(format!("a submit of {id} through a held old writer answers {answer:?}")),
    }
}

async fn storm_beside(logs: Arc<ThreadLogs>, policy: Policy, late_ms: u64) -> Stormed {
    nanocode_platform::sleep(Duration::from_millis(late_ms)).await;
    let over = OneLog::over(logs, THREAD, &policy)
        .await
        .map_err(|error| format!("a beside open answers {error}"))?;
    over.pending()
        .await
        .map_err(|error| format!("a beside pending read answers {error}"))?;
    Ok(Vec::new())
}

fn stormed(rng: &mut Rng, world: &World, run: &mut Run) -> Option<Backfilled> {
    let logs = Arc::new(
        run.node
            .logs
            .clone()
            .with_paths(Arc::clone(&run.paths) as Arc<dyn ThreadPaths>),
    );
    let mut tasks: Vec<std::pin::Pin<Box<dyn Future<Output = Stormed> + '_>>> = Vec::new();
    for _ in 0..rng.range(STORM_TURNS.start, STORM_TURNS.end) {
        let states = match driven(world, "a storm state store", run.over.states())? {
            Ok(states) => states,
            Err(error) => {
                failed(world, run, "a storm state store", &error);
                return None;
            }
        };
        let Some(facts) = states.facts() else {
            flag(&world.wrong, "flips: the state store offers no fact log".to_owned());
            return None;
        };
        let id = format!("s{}", run.turns);
        run.turns += 1;
        run.ids.push(id.clone());
        let late_ms = rng.range(RACE_READS.start, RACE_READS.end) * admitturns::READ_MS;
        let step_ms =
            rng.range(STORM_STEP_READS.start, STORM_STEP_READS.end) * admitturns::READ_MS;
        tasks.push(Box::pin(storm_turn(Storm {
            over: Arc::clone(&run.over),
            facts,
            id,
            late_ms,
            step_ms,
        })));
    }
    if let Some(old) = run.over.legacy() {
        let id = format!("h{}", run.turns);
        run.turns += 1;
        let late_reads = RACE_READS.end + 3 * STORM_STEP_READS.end;
        let late_ms = rng.range(RACE_READS.end, late_reads) * admitturns::READ_MS;
        tasks.push(Box::pin(storm_stale(old, id, late_ms)));
    }
    for _ in 0..rng.range(STORM_BESIDE.start, STORM_BESIDE.end) {
        let late_ms = rng.range(RACE_READS.start, RACE_READS.end) * admitturns::READ_MS;
        tasks.push(Box::pin(storm_beside(
            Arc::clone(&logs),
            run.policy.clone(),
            late_ms,
        )));
    }
    let scopes = run.node.logs.scopes();
    run.slow.paced(true);
    let raced = driven(
        world,
        "a backfill raced by concurrent commands",
        join(scopes.backfill(), join_all(tasks)),
    );
    run.slow.paced(false);
    let (passed, answers) = raced?;
    for answer in answers {
        match answer {
            Ok(records) => {
                for (record, durable) in records {
                    if !run.ids.contains(&record.request_id) {
                        run.ids.push(record.request_id.clone());
                    }
                    acked(run, &record, durable);
                }
            }
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("flips: a command racing a backfill on one thread: {error}"),
                );
                return None;
            }
        }
    }
    Some(passed)
}

fn passes(
    rng: &mut Rng,
    world: &World,
    run: &mut Run,
    race: bool,
    storm: bool,
    stats: &mut FlipsStats,
) -> Option<()> {
    let mut passes = QUIET_PASSES + u64::from(race);
    let mut pass = 0;
    while pass < passes {
        pass += 1;
        if !run.node.logs.scopes().adopting(THREAD) {
            flag(
                &world.wrong,
                "flips: the facade serves legacy on the resident path with no backfill queued"
                    .to_owned(),
            );
            return None;
        }
        let before = run.node.logs.scopes().stats().usage.adopt.rows;
        let from = head(world);
        let passed = if storm && pass == 1 {
            stormed(rng, world, run)?
        } else if race && pass == 1 {
            raced(rng, world, run)?
        } else {
            driven(world, "a flips backfill", run.node.logs.scopes().backfill())?
        };
        if let Some((scope, error)) = passed.failed.first() {
            flag(
                &world.wrong,
                format!("flips: the backfill of {scope} fails: {error}"),
            );
            return None;
        }
        let read = run.node.logs.scopes().stats().usage.adopt.rows - before;
        if passed.adopted + passed.resumed > 0 {
            bounded(world, run, read, from, stats)?;
        }
        if passed.resumed > 0 {
            stats.resumed += 1;
            stats.raced += u64::from(race && pass == 1);
            passes = pass + QUIET_PASSES;
        }
        if passed.adopted > 0 {
            break;
        }
    }
    Some(())
}

fn backfilled(rng: &mut Rng, world: &World, run: &mut Run, stats: &mut FlipsStats) -> Option<()> {
    stats.held += 1;
    let race = rng.chance(RACE_PPM);
    let storm = race && rng.chance(STORM_PPM);
    for round in 0..SWITCHES {
        passes(rng, world, run, race && round == 0, storm && round == 0, stats)?;
        if let Err(error) = driven(world, "a flips read after its backfill", run.over.pending())? {
            failed(world, run, "the read after a backfill", &error);
            return None;
        }
        if run.over.path() == ThreadPath::Resident {
            break;
        }
        stats.missed += 1;
    }
    Some(())
}

fn sessions(rng: &mut Rng, world: &World, run: &mut Run, stats: &mut FlipsStats) -> Option<()> {
    let Some(mut sessions) = run.sessions.take() else {
        return Some(());
    };
    if run.over.path() == ThreadPath::Resident
        && let Some(Ok(log)) = reach(world, &run.node, THREAD)
        && let Some(state) = log.state()
        && let Err(error) = threadsessions::held(&state, THREAD, &sessions)
            .and_then(|_| threadsessions::kept(&state, THREAD, &world.rows(THREAD)))
    {
        flag(&world.wrong, format!("flips: {error}"));
        return None;
    }
    let states = match driven(world, "a flips state store", run.over.states())? {
        Ok(states) => states,
        Err(error) => {
            failed(world, run, "the state store", &error);
            return None;
        }
    };
    let Some(logs) = states.facts() else {
        flag(
            &world.wrong,
            "flips: the state store offers no fact log".to_owned(),
        );
        return None;
    };
    let checked = driven(
        world,
        "a flips session fold",
        threadsessions::agrees(rng, &*logs, THREAD, &mut sessions),
    )?;
    match checked {
        Ok(checked) => stats.sessions += checked,
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: on the {} path, {error}", run.over.path().setting()),
            );
            return None;
        }
    }
    run.sessions = Some(sessions);
    Some(())
}

fn started(rng: &mut Rng, world: &World) -> Option<Run> {
    let mut ids = Vec::new();
    let mut sessions = None;
    if rng.chance(LEGACY_PPM) && rng.chance(SESSIONS_PPM) {
        match driven(
            world,
            "an old writer's sessions",
            threadsessions::write(rng, world.host(), THREAD),
        )? {
            Ok(written) => {
                ids.extend(written.ids.iter().cloned());
                sessions = Some(written);
            }
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("flips: the old writer's sessions: {error}"),
                );
                return None;
            }
        }
    } else if rng.chance(LEGACY_PPM) {
        let written = driven(
            world,
            "an old path history",
            threadlegacy::write_named(rng, world.host(), THREAD, "old"),
        )?;
        match written {
            Ok(legacy) => ids.extend(legacy.ids().iter().cloned()),
            Err(error) => {
                flag(&world.wrong, format!("flips: the old path writes: {error}"));
                return None;
            }
        }
    }
    let paths = Arc::new(Switched(AtomicBool::new(rng.chance(START_LEGACY_PPM))));
    let stamped = rng.chance(START_STAMPED_PPM);
    log_sim::force(Some(row_format(stamped)));
    let (node, slow) = paced(world, "f0");
    publish(world, std::slice::from_ref(&node));
    let snapshot_rows = rng
        .chance(SNAPSHOT_PPM)
        .then(|| rng.range(SNAPSHOT_ROWS.start, SNAPSHOT_ROWS.end) as usize);
    let policy = Policy {
        restarts: RESTARTS,
        snapshot_rows,
        ..Policy::default()
    };
    let over = opened(world, &node, &paths, &policy)?;
    Some(Run {
        paths,
        policy,
        node,
        slow,
        over,
        generation: 0,
        stamped,
        turns: 0,
        ids,
        acked: BTreeMap::new(),
        sessions,
        adopted: None,
    })
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: FlipsStats,
    shape: String,
}

fn seeded(seed: u64, mutation: FlipsMutation, time: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "flips");
    let mut stats = FlipsStats {
        seeds: 1,
        ..FlipsStats::default()
    };
    let world = crate::threadscope::world(&mut rng, time).keeping(RECENT_ROWS);
    let rounds = rng.range(ROUNDS.start, ROUNDS.end);
    let shape = format!("rounds={rounds}");
    thread_scope_sim::mutate(mutation.mutation());
    scope_sim::mutate(mutation.scope());
    if registered(&world)
        && let Some(mut run) = started(&mut rng, &world)
    {
        for _ in 0..rounds {
            if round(&mut rng, &world, &mut run, &mut stats).is_none() {
                break;
            }
        }
        stats.acked += run.acked.len() as u64;
    }
    thread_scope_sim::mutate(Mutation::None);
    scope_sim::mutate(ScopeMutation::None);
    log_sim::force(None);
    publish(&world, &[]);
    let mut digest = fnv(FNV_OFFSET, &seed.to_le_bytes());
    for (seq, row) in world.rows(THREAD) {
        digest = fnv(fnv(digest, &seq.to_le_bytes()), row.as_bytes());
    }
    Outcome {
        digest,
        wrong: world.wrong.read(Clone::clone),
        stats,
        shape,
    }
}

fn run_seed(seed: u64, mutation: FlipsMutation) -> Outcome {
    let time = Clock::new();
    crate::host::entered(seed, &time, || seeded(seed, mutation, &time))
}

pub fn judge(
    seed: u64,
    mutation: FlipsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut FlipsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: FlipsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario flips --seed {seed}");
    if mutation != FlipsMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: FlipsMutation,
    totals: &FlipsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=flips mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        FlipsMutation::None if failed == 0 => ExitCode::SUCCESS,
        FlipsMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}

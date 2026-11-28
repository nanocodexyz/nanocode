use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted, TurnState},
    turnlog::{EffectCall, LogHost, OneLog, Policy, facts_sim},
};
use nanocode_durability_core::{
    BlockRef, Decision, DurableFold, EffectResult, Fact, FactLog, FoldError, Folded, Input,
    InputKind, Intent, IntentTarget, Marker, ModelOut, Outcome as Finished, OwnedFact, Seen,
    StateStore, TurnStatus, VALUE_INLINE_BYTES, commit_fact, fold_facts,
};
use nanocode_sync::block_on;
use nanocode_tool_runtime::ToolInput;
use nanocode_turnlog::MemoryLog;
use nanocode_turnlog::{CachedAt, EffectClass, Record};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const INTENDED_INPUT: &str = "{\"tool_use\"";
const SCOPE: &str = "one_log_facts";
const TOOL: &str = "charge";
const CHILD: &str = "one_log_facts:child";
const LATE_CHILD: &str = "one_log_facts:late";
const CHILD_PPM: u64 = 150_000;
const SUBMIT_PPM: u64 = 100_000;
const FINISH_PPM: u64 = 500_000;
const STEPS: Range<u64> = 40..240;
const TURNS: u64 = 6;
const BLOCKS: u64 = 4;
const VALUES: u64 = 5;
const DECISIONS: u64 = 1 << 20;
const SNAPSHOT_RATIO: usize = 2;
const SNAPSHOT_ROWS_MIN: u64 = 2;
const SNAPSHOT_ROWS_MAX: u64 = 8;
const SWAP_PPM: u64 = 300_000;
const HOST_PPM: u64 = 100_000;
const CANCEL_PPM: u64 = 100_000;
const STRAY_PPM: u64 = 50_000;
const ABANDON_PPM: u64 = 50_000;
const HOST_KEYS: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneLogFactsMutation {
    None,
    FoldReadsUnlogged,
    CommitBuffered,
    UnseenSkipped,
    IntentReplayed,
    IntentCopied,
    ScopeLeaks,
    AdmissionCopied,
    CacheGap,
    CacheNeverDue,
    HostCopied,
    CancelLeavesIntent,
    ScopedInputLowered,
    IntentOptional,
    AbandonUnsettled,
    CacheInline,
    RememberedSkipsTail,
    FactsScanOnWriter,
    FoldStopsAtOwnFact,
}

impl OneLogFactsMutation {
    const ALL: [Self; 19] = [
        Self::None,
        Self::FoldReadsUnlogged,
        Self::CommitBuffered,
        Self::UnseenSkipped,
        Self::IntentReplayed,
        Self::IntentCopied,
        Self::ScopeLeaks,
        Self::AdmissionCopied,
        Self::CacheGap,
        Self::CacheNeverDue,
        Self::HostCopied,
        Self::CancelLeavesIntent,
        Self::ScopedInputLowered,
        Self::IntentOptional,
        Self::AbandonUnsettled,
        Self::CacheInline,
        Self::RememberedSkipsTail,
        Self::FactsScanOnWriter,
        Self::FoldStopsAtOwnFact,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::FoldReadsUnlogged => "fold_reads_unlogged",
            Self::CommitBuffered => "commit_buffered",
            Self::UnseenSkipped => "unseen_skipped",
            Self::IntentReplayed => "intent_replayed",
            Self::IntentCopied => "intent_copied",
            Self::ScopeLeaks => "scope_leaks",
            Self::AdmissionCopied => "admission_copied",
            Self::CacheGap => "cache_gap",
            Self::CacheNeverDue => "cache_never_due",
            Self::HostCopied => "host_copied",
            Self::CancelLeavesIntent => "cancel_leaves_intent",
            Self::ScopedInputLowered => "scoped_input_lowered",
            Self::IntentOptional => "intent_optional",
            Self::AbandonUnsettled => "abandon_unsettled",
            Self::CacheInline => "cache_inline",
            Self::RememberedSkipsTail => "remembered_skips_tail",
            Self::FactsScanOnWriter => "facts_scan_on_writer",
            Self::FoldStopsAtOwnFact => "fold_stops_at_own_fact",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn log_rule(self) -> facts_sim::Mutation {
        match self {
            Self::CommitBuffered => facts_sim::Mutation::CommitBuffered,
            Self::UnseenSkipped => facts_sim::Mutation::UnseenSkipped,
            Self::IntentReplayed => facts_sim::Mutation::IntentReplayed,
            Self::IntentCopied => facts_sim::Mutation::IntentCopied,
            Self::ScopeLeaks => facts_sim::Mutation::ScopeLeaks,
            Self::AdmissionCopied => facts_sim::Mutation::AdmissionCopied,
            Self::CacheGap => facts_sim::Mutation::CacheGap,
            Self::HostCopied => facts_sim::Mutation::HostCopied,
            Self::CancelLeavesIntent => facts_sim::Mutation::CancelLeavesIntent,
            Self::IntentOptional => facts_sim::Mutation::IntentOptional,
            Self::AbandonUnsettled => facts_sim::Mutation::AbandonUnsettled,
            Self::RememberedSkipsTail => facts_sim::Mutation::RememberedSkipsTail,
            Self::None
            | Self::FoldReadsUnlogged
            | Self::FactsScanOnWriter
            | Self::FoldStopsAtOwnFact
            | Self::CacheNeverDue
            | Self::ScopedInputLowered
            | Self::CacheInline => facts_sim::Mutation::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OneLogFactsStats {
    pub seeds: u64,
    pub facts: u64,
    pub blobs: u64,
    pub restarts: u64,
    pub refolds: u64,
    pub remembered: u64,
    pub swaps: u64,
    pub unknown: u64,
    pub gated: u64,
    pub children: u64,
    pub submitted: u64,
    pub crashes: u64,
    pub cached: u64,
    pub skipped: u64,
    pub replayed: u64,
    pub hosts: u64,
    pub cancelled: u64,
    pub refused: u64,
    pub abandoned: u64,
    pub late_children: u64,
    pub sealed_unheld: u64,
}

impl OneLogFactsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.facts += other.facts;
        self.blobs += other.blobs;
        self.restarts += other.restarts;
        self.refolds += other.refolds;
        self.remembered += other.remembered;
        self.swaps += other.swaps;
        self.unknown += other.unknown;
        self.gated += other.gated;
        self.children += other.children;
        self.submitted += other.submitted;
        self.crashes += other.crashes;
        self.cached += other.cached;
        self.skipped += other.skipped;
        self.replayed += other.replayed;
        self.hosts += other.hosts;
        self.cancelled += other.cancelled;
        self.refused += other.refused;
        self.abandoned += other.abandoned;
        self.late_children += other.late_children;
        self.sealed_unheld += other.sealed_unheld;
    }
}

static UNLOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MUTATION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn reads_unlogged() -> bool {
    MUTATION.load(std::sync::atomic::Ordering::Relaxed)
        == OneLogFactsMutation::FoldReadsUnlogged as u8
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Ledger {
    digest: u64,
    turns: BTreeMap<String, u64>,
    finished: BTreeMap<String, String>,
    blocks: BTreeMap<u64, u32>,
    calls: BTreeMap<String, Option<u64>>,
    decisions: Vec<u64>,
    unlogged: u64,
    applied: u64,
}

impl Ledger {
    fn mix(&mut self, bytes: &[u8]) {
        self.digest = fnv(self.digest ^ FNV_OFFSET, bytes);
    }
}

impl DurableFold for Ledger {
    const FAMILY: &'static str = "sim.ledger";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Input(input) => {
                *self.turns.entry(input.turn).or_default() += 1;
                self.mix(input.value.get().as_bytes());
            }
            Fact::ModelOut(out) => {
                let blocks = u32::try_from(BLOCKS).expect("the block count fits in u32");
                self.blocks.insert(seq, blocks);
                self.mix(out.response.get().as_bytes());
            }
            Fact::Intent(intent) => {
                let IntentTarget::ToolUse(block) = intent.target else {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: "the ledger takes only tool_use intents".to_owned(),
                    });
                };
                let held = self.blocks.get(&block.model_out).copied().unwrap_or(0);
                if block.block >= held {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: format!(
                            "intent {} names block {} of model output {}, which has {held}",
                            intent.call_id, block.block, block.model_out
                        ),
                    });
                }
                self.calls.insert(intent.call_id, None);
            }
            Fact::EffectResult(result) => {
                let Some(slot) = self.calls.get_mut(&result.call_id) else {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: format!("result of call {} without its intent", result.call_id),
                    });
                };
                *slot = Some(seq);
                self.mix(result.output.get().as_bytes());
            }
            Fact::Decision(Decision::Backend(chosen)) => self.decisions.push(*chosen),
            Fact::Marker(Marker::Status { turn, status }) => {
                self.finished.insert(turn, format!("{status:?}"));
            }
            Fact::Base(_) | Fact::Decision(_) | Fact::Marker(_) => {}
        }
        self.applied += 1;
        if reads_unlogged() {
            self.unlogged = UNLOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        serde_json::to_vec(self).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

struct World {
    host: Arc<MemoryLog>,
    hosts: BTreeMap<String, String>,
    log: Option<Arc<OneLog>>,
    facts: Option<Arc<dyn FactLog>>,
    child: Option<Arc<dyn FactLog>>,
    child_live: Option<Folded<Ledger>>,
    expected: u64,
    child_expected: u64,
    submitted: Vec<(String, TurnState)>,
    durable: u64,
    live: Option<Folded<Ledger>>,
    stale: Option<Folded<Ledger>>,
    policy: Policy,
    outputs: Vec<u64>,
    open: Vec<String>,
    calls: u64,
    turns: u64,
    running: Vec<String>,
    stats: OneLogFactsStats,
    shape: String,
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: OneLogFactsStats,
    shape: String,
}

fn value(rng: &mut Rng) -> Box<RawValue> {
    let text = if rng.chance(200_000) {
        let pick = rng.below(VALUES);
        let bytes =
            usize::try_from(pick + 1).expect("a value index fits in usize") * VALUE_INLINE_BYTES;
        format!("{pick}:{}", "x".repeat(bytes))
    } else {
        format!("v{}", rng.below(DECISIONS))
    };
    to_raw_value(&text).expect("a string encodes")
}

impl World {
    fn facts(&self) -> &dyn FactLog {
        &**self.facts.as_ref().expect("the fact log is open")
    }

    fn open(&mut self) -> Result<(), String> {
        let host: Arc<dyn LogHost> = Arc::clone(&self.host) as Arc<dyn LogHost>;
        let log = block_on(nanocode::turnlog::thread_scope_sim::open(
            host,
            SCOPE,
            self.policy.clone(),
        ))
        .map_err(|error| format!("the facts log opens: {error}"))?;
        let logs = StateStore::facts(&log.store())
            .ok_or_else(|| "a OneLog store offers no fact log".to_owned())?;
        let facts = logs.scoped(SCOPE, true);
        let child = logs.scoped(CHILD, false);
        let child_live = block_on(fold_facts::<Ledger>(&*child))
            .map_err(|error| format!("the child's facts fold on open: {error}"))?;
        self.child = Some(child);
        self.child_live = Some(child_live);
        let live = block_on(fold_facts::<Ledger>(&*facts))
            .map_err(|error| format!("the facts log folds on open: {error}"))?;
        let stale = block_on(fold_facts::<Ledger>(&*facts))
            .map_err(|error| format!("the facts log folds on open: {error}"))?;
        self.stale = Some(stale);
        self.log = Some(log);
        self.facts = Some(facts);
        self.live = Some(live);
        Ok(())
    }

    fn fact(&mut self, rng: &mut Rng) -> OwnedFact<u64> {
        let turn = format!("turn-{}", rng.below(TURNS));
        match rng.below(100) {
            0..15 => {
                self.turns += 1;
                let turn = format!("admitted-{}", self.turns);
                self.running.push(turn.clone());
                Fact::Input(Input {
                    turn,
                    kind: InputKind::Prompt,
                    value: value(rng),
                    metadata: to_raw_value(&()).expect("null encodes"),
                    hand: None,
                    first: None,
                })
            }
            15..40 => Fact::ModelOut(ModelOut {
                turn,
                response: value(rng),
            }),
            40..60 if !self.outputs.is_empty() => {
                self.calls += 1;
                let call_id = format!("call-{}", self.calls);
                self.open.push(call_id.clone());
                let model_out = self.outputs[rng.pick(self.outputs.len())];
                let block = u32::try_from(rng.below(BLOCKS)).expect("a block index fits in u32");
                Fact::Intent(Intent {
                    turn: Some(turn),
                    call_id,
                    tool: TOOL.to_owned(),
                    target: IntentTarget::ToolUse(BlockRef { model_out, block }),
                })
            }
            60..80 if !self.open.is_empty() => {
                let call_id = self.open.swap_remove(rng.pick(self.open.len()));
                Fact::EffectResult(EffectResult {
                    call_id,
                    output: value(rng),
                })
            }
            92.. if !self.running.is_empty() => Fact::Marker(Marker::Status {
                turn: self.running.swap_remove(rng.pick(self.running.len())),
                status: TurnStatus::Finished {
                    last: self.calls,
                    outcome: Finished::Completed,
                    result: to_raw_value(&()).expect("null encodes"),
                },
            }),
            _ => Fact::Decision(Decision::Backend(rng.below(DECISIONS))),
        }
    }

    fn late_child(&mut self, rng: &mut Rng) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let logs = StateStore::facts(&log.store())
            .ok_or_else(|| "a OneLog store offers no fact log".to_owned())?;
        let late = logs.scoped(LATE_CHILD, false);
        let opening = block_on(log.stats());
        let folded = block_on(fold_facts::<Ledger>(&*late))
            .map_err(|error| format!("a new scope's facts fold on open: {error}"))?;
        let opened = block_on(log.stats());
        let scanned = opened.pages - opening.pages;
        let unheld = opened.pages_unheld - opening.pages_unheld;
        if scanned != unheld {
            return Err(format!(
                "opening a new scope's facts scanned {} sealed fact segments on the log's writer",
                scanned - unheld
            ));
        }
        self.stats.sealed_unheld += unheld;
        let fact = Fact::ModelOut(ModelOut {
            turn: "late".to_owned(),
            response: value(rng),
        });
        let (folded, _) = block_on(commit_fact(&*late, folded, fact))
            .map_err(|error| format!("commit a new scope's first fact: {error}"))?;
        let committed = block_on(log.stats());
        if committed.pages != opened.pages {
            return Err(format!(
                "a new scope's first fact rescanned {} sealed fact segments its open had read",
                committed.pages - opened.pages
            ));
        }
        if folded.state().applied != 1 {
            return Err(format!(
                "a new scope folded {} facts after its first commit",
                folded.state().applied
            ));
        }
        self.stats.late_children += 1;
        Ok(())
    }

    fn commit_child(&mut self, rng: &mut Rng) -> Result<(), String> {
        let fact = Fact::ModelOut(ModelOut {
            turn: "child".to_owned(),
            response: value(rng),
        });
        let child = Arc::clone(self.child.as_ref().expect("the child's fact log is open"));
        let live = self.child_live.take().expect("the child's fold is live");
        let (live, _) = block_on(commit_fact(&*child, live, fact))
            .map_err(|error| format!("commit a child fact: {error}"))?;
        self.child_expected += 1;
        if live.state().applied != self.child_expected {
            return Err(format!(
                "the child's fold applied {} facts, but {} were committed to it",
                live.state().applied,
                self.child_expected
            ));
        }
        self.child_live = Some(live);
        self.stats.children += 1;
        Ok(())
    }

    fn commit(&mut self, rng: &mut Rng) -> Result<(), String> {
        if rng.chance(CHILD_PPM) {
            self.commit_child(rng)?;
        }
        let fact = self.fact(rng);
        self.expected += 1;
        let model_out = matches!(fact, Fact::ModelOut(_));
        let intent = match &fact {
            Fact::Intent(intent) => Some(intent.call_id.clone()),
            _ => None,
        };
        if rng.chance(SWAP_PPM) {
            self.stats.swaps += 1;
            std::mem::swap(&mut self.live, &mut self.stale);
        }
        let live = self.live.take().expect("the facts fold is live");
        let before = live.values().len();
        let (live, seq) = block_on(commit_fact(self.facts(), live, fact))
            .map_err(|error| format!("commit a fact: {error}"))?;
        self.durable = self.expected;
        self.stats.facts += 1;
        self.stats.blobs += live.values().len().saturating_sub(before) as u64;
        if model_out {
            self.outputs.push(seq);
        }
        if let Some(call_id) = intent {
            self.gate(&call_id)?;
        }
        self.shape.push('f');
        self.live = Some(live);
        self.check()
    }

    fn host_field(&mut self, rng: &mut Rng) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let key = format!("field-{}", rng.below(HOST_KEYS));
        let value = value(rng);
        block_on(log.effect(&key, value.clone()))
            .map_err(|error| format!("write host field {key}: {error}"))?;
        self.hosts.insert(key.clone(), value.get().to_owned());
        self.stats.hosts += 1;
        let stored = block_on(self.host.load(SCOPE))
            .map_err(|error| format!("read the log's rows: {error}"))?;
        let rows = nanocode_turnlog::rows(&stored).map_err(|error| error.to_string())?;
        for row in rows.iter().filter(|row| !row.snapshot) {
            let records = row.records().map_err(|error| error.to_string())?;
            if records
                .iter()
                .any(|record| matches!(record, Record::Host { key: held, .. } if *held == key))
            {
                return Err(format!(
                    "host field {key} was written as a host record beside its decision"
                ));
            }
        }
        Ok(())
    }

    fn check_hosts(&self) -> Result<(), String> {
        let log = self.log.as_ref().expect("the facts log is open");
        for (key, value) in &self.hosts {
            let wanted = key.clone();
            let held = block_on(
                log.read(move |fold| fold.host(&wanted).map(|held| held.get().to_owned())),
            );
            if held.as_deref() != Some(value.as_str()) {
                return Err(format!(
                    "host field {key} reads back as {held:?} after a restart"
                ));
            }
        }
        Ok(())
    }

    fn check_cached(&mut self) -> Result<(), String> {
        let replay = block_on(self.facts().replay_cached(Ledger::FAMILY, Ledger::FORMAT))
            .map_err(|error| format!("replay the session's facts from its cache: {error}"))?;
        let Some(snapshot) = replay.snapshot else {
            return Err(format!(
                "the session committed {} facts but replays them without a cached fold",
                self.expected
            ));
        };
        let log = self.log.as_ref().expect("the facts log is open");
        let inline = block_on(log.read(|fold| {
            fold.host(&nanocode_turnlog::cache_key(SCOPE))
                .filter(|value| serde_json::from_str::<CachedAt>(value.get()).is_err())
                .map(|value| value.get().len())
        }));
        if let Some(bytes) = inline {
            return Err(format!(
                "the cached fold of {SCOPE} is held inline in the turn log ({bytes} bytes), so every snapshot copies it"
            ));
        }
        self.stats.cached += 1;
        self.stats.skipped += snapshot.applied;
        self.stats.replayed += replay.facts.len() as u64;
        Ok(())
    }

    fn check_scoped_inputs(&self) -> Result<(), String> {
        let log = self.log.as_ref().expect("the facts log is open");
        let turns = self.turns;
        let phantom = block_on(log.read(move |fold| {
            (1..=turns)
                .map(|turn| format!("admitted-{turn}"))
                .find(|turn| fold.turn(turn).is_some())
        }));
        match phantom {
            Some(turn) => Err(format!(
                "a session's own Input for {turn} became a turn of the log"
            )),
            None => Ok(()),
        }
    }

    fn check(&mut self) -> Result<(), String> {
        self.stats.refolds += 1;
        self.check_scoped_inputs()?;
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        if block_on(log.read(|fold| fold.logs_facts(SCOPE))) {
            self.check_cached()?;
        }
        let remembering = self.stats.refolds % 3 == 0;
        for held in [self.live.as_ref(), self.stale.as_ref()]
            .into_iter()
            .flatten()
            .filter(|_| remembering)
        {
            let snapshot = held
                .snapshot()
                .map_err(|error| format!("snapshot a fold to remember: {error}"))?;
            self.facts().remember(snapshot);
            self.stats.remembered += 1;
        }
        let folded = block_on(fold_facts::<Ledger>(self.facts()))
            .map_err(|error| format!("refold the log: {error}"))?;
        let live = self.live.as_ref().expect("the facts fold is live");
        if folded.state().applied != self.expected {
            return Err(format!(
                "the session's facts fold {} facts, but {} were committed to it",
                folded.state().applied,
                self.expected
            ));
        }
        if folded.seq() != live.seq() {
            return Err(format!(
                "the log folds to fact {}, the live fold is at fact {}",
                folded.seq(),
                live.seq()
            ));
        }
        let encode = |state: &Ledger| serde_json::to_string(state).expect("a ledger encodes");
        if encode(folded.state()) != encode(live.state()) {
            return Err(format!(
                "the fold of the log differs from the live fold at fact {}",
                live.seq()
            ));
        }
        let requests = (folded.request(), live.request());
        let (Ok(again), Ok(now)) = requests else {
            return Err("a ledger request does not encode".to_owned());
        };
        if again != now {
            return Err(format!(
                "the next request folded from the log differs from the live one at fact {}",
                live.seq()
            ));
        }
        Ok(())
    }

    fn crash(&mut self) -> Result<(), String> {
        self.stats.crashes += 1;
        self.shape.push('x');
        self.live = None;
        self.stale = None;
        self.facts = None;
        self.child = None;
        self.child_live = None;
        self.log = None;
        self.open()?;
        self.check_hosts()?;
        let applied = self
            .live
            .as_ref()
            .expect("the facts fold is live")
            .state()
            .applied;
        if applied < self.durable || applied > self.expected {
            return Err(format!(
                "after a crash the session's facts fold {applied} facts, outside the {} committed durably and the {} written",
                self.durable, self.expected
            ));
        }
        self.expected = applied;
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        for (id, state) in &mut self.submitted {
            let record = block_on(log.turn(id))
                .map_err(|error| format!("read turn {id}: {error}"))?
                .ok_or_else(|| format!("submitted turn {id} is missing after a crash"))?;
            match (*state, record.state) {
                (TurnState::Running, TurnState::Queued) => *state = TurnState::Queued,
                (held, read) if held == read => {}
                (held, read) => {
                    return Err(format!(
                        "after a crash submitted turn {id} reads {read:?}, not {held:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn restart(&mut self) -> Result<(), String> {
        self.stats.restarts += 1;
        self.shape.push('!');
        let mut store = self.log.as_ref().expect("the facts log is open").store();
        block_on(StateStore::flush(&mut store))
            .map_err(|error| format!("flush before a restart: {error}"))?;
        let before = block_on(fold_facts::<Ledger>(self.facts()))
            .map_err(|error| format!("refold before a restart: {error}"))?;
        self.live = None;
        self.facts = None;
        self.child = None;
        self.child_live = None;
        self.log = None;
        self.open()?;
        self.check_hosts()?;
        let after = self.live.as_ref().expect("the facts fold is live");
        let encode = |state: &Ledger| serde_json::to_string(state).expect("a ledger encodes");
        if after.seq() != before.seq() || encode(after.state()) != encode(before.state()) {
            return Err(format!(
                "after a restart the log folds to fact {}, not the fact {} it folded to before",
                after.seq(),
                before.seq()
            ));
        }
        self.durable = self.expected;
        Ok(())
    }

    fn gate(&mut self, call_id: &str) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let input = ToolInput::Function(to_raw_value(&()).expect("null encodes"));
        block_on(log.gate().before(EffectCall {
            call_id,
            effect: EffectClass::External,
            input: &input,
            session_id: SCOPE,
            tool: TOOL,
            turn_id: None,
        }))
        .map_err(|error| format!("the gate refused intended call {call_id}: {error}"))?;
        let wanted = call_id.to_owned();
        let copied = block_on(log.read(move |fold| {
            fold.open_calls()
                .any(|(id, call)| id == wanted && !call.input.get().starts_with(INTENDED_INPUT))
        }));
        if copied {
            return Err(format!(
                "the gate copied intended call {call_id} into a call record beside its intent"
            ));
        }
        self.stats.gated += 1;
        Ok(())
    }

    fn recover(&mut self) -> Result<(), String> {
        self.turns_hold()?;
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let mut settled = Vec::new();
        for turn in 0..TURNS {
            let turn = format!("turn-{turn}");
            settled.extend(
                block_on(log.settle_intents(&turn))
                    .map_err(|error| format!("settle the intents of {turn}: {error}"))?,
            );
        }
        settled.sort();
        let mut open = std::mem::take(&mut self.open);
        open.sort();
        if settled != open {
            return Err(format!(
                "recovery settled {settled:?} as outcome unknown, but {open:?} had no result"
            ));
        }
        self.stats.unknown += settled.len() as u64;
        self.expected += settled.len() as u64;
        let again = block_on(log.settle_intents("turn-0"))
            .map_err(|error| format!("settle again: {error}"))?;
        if !again.is_empty() {
            return Err(format!("recovery settled {again:?} twice"));
        }
        self.live = Some(
            block_on(fold_facts::<Ledger>(self.facts()))
                .map_err(|error| format!("refold after recovery: {error}"))?,
        );
        self.check()
    }

    fn submit(&mut self, rng: &mut Rng) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let id = format!("submitted-{}", self.submitted.len());
        let submission = Submission {
            request_id: id.clone(),
            prompt: Prompt::new(format!("prompt {}", rng.below(DECISIONS))),
            hand: None,
            metadata: to_raw_value(&()).expect("null encodes"),
        };
        match block_on(log.submit(&submission)) {
            Ok(Submitted::Accepted { .. }) => {}
            other => return Err(format!("submit {id}: {other:?}")),
        }
        self.expected += 1;
        self.durable = self.expected;
        match block_on(log.submit(&submission)) {
            Ok(Submitted::Existing(_)) => {}
            other => return Err(format!("resubmit {id}: {other:?}")),
        }
        if block_on(log.start(&id))
            .map_err(|error| format!("start {id}: {error}"))?
            .is_none()
        {
            return Err(format!("submitted turn {id} did not start"));
        }
        self.expected += 1;
        let mut state = TurnState::Running;
        if rng.chance(FINISH_PPM) {
            block_on(log.finish(&id, TurnState::Completed, &serde_json::Map::new()))
                .map_err(|error| format!("finish {id}: {error}"))?
                .ok_or_else(|| format!("submitted turn {id} did not finish"))?;
            self.expected += 1;
            self.durable = self.expected;
            state = TurnState::Completed;
        }
        self.submitted.push((id, state));
        self.stats.submitted += 1;
        self.turns_hold()
    }

    fn abandon(&mut self, rng: &mut Rng) -> Result<(), String> {
        if self.outputs.is_empty() {
            return Ok(());
        }
        self.calls += 1;
        let call_id = format!("abandoned-{}", self.calls);
        let model_out = self.outputs[rng.pick(self.outputs.len())];
        let block = u32::try_from(rng.below(BLOCKS)).expect("a block index fits in u32");
        let intent = Fact::Intent(Intent {
            turn: Some(format!("direct-{}", self.calls)),
            call_id: call_id.clone(),
            tool: TOOL.to_owned(),
            target: IntentTarget::ToolUse(BlockRef { model_out, block }),
        });
        let live = self.live.take().expect("the facts fold is live");
        let (live, _) = block_on(commit_fact(self.facts(), live, intent))
            .map_err(|error| format!("commit the intent of {call_id}: {error}"))?;
        self.live = Some(live);
        self.expected += 1;
        self.durable = self.expected;
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let settled = block_on(log.gate().abandon(&call_id))
            .map_err(|error| format!("abandon {call_id}: {error}"))?;
        if settled.is_none_or(|output| output.success) {
            return Err(format!(
                "abandoning intended call {call_id} returned no unknown outcome"
            ));
        }
        let wanted = call_id.clone();
        let open = block_on(log.read(move |fold| fold.intent(&wanted).is_some()));
        if open {
            return Err(format!("abandoned call {call_id} has no durable result"));
        }
        let again = block_on(log.gate().abandon(&call_id))
            .map_err(|error| format!("abandon {call_id} again: {error}"))?;
        if again.is_some() {
            return Err(format!("abandoned call {call_id} was settled twice"));
        }
        self.expected += 1;
        self.durable = self.expected;
        self.stats.abandoned += 1;
        self.live = Some(
            block_on(fold_facts::<Ledger>(self.facts()))
                .map_err(|error| format!("refold after abandoning: {error}"))?,
        );
        self.check()
    }

    fn stray(&mut self) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        if !block_on(log.read(|fold| fold.logs_facts(SCOPE))) {
            return Ok(());
        }
        self.calls += 1;
        let call_id = format!("stray-{}", self.calls);
        let input = ToolInput::Function(to_raw_value(&()).expect("null encodes"));
        let refused = block_on(log.gate().before(EffectCall {
            call_id: &call_id,
            effect: EffectClass::External,
            input: &input,
            session_id: SCOPE,
            tool: TOOL,
            turn_id: None,
        }))
        .is_err();
        if !refused {
            return Err(format!(
                "the gate let call {call_id} run in a session that logs facts without an intent"
            ));
        }
        self.stats.refused += 1;
        Ok(())
    }

    fn cancel(&mut self, rng: &mut Rng) -> Result<(), String> {
        let running = self
            .submitted
            .iter()
            .filter(|(_, state)| *state == TurnState::Running)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if running.is_empty() || self.outputs.is_empty() {
            return Ok(());
        }
        let id = running[rng.pick(running.len())].clone();
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let cancelling = block_on(log.turn(&id))
            .map_err(|error| format!("read turn {id}: {error}"))?
            .is_some_and(|record| record.cancel_requested);
        if cancelling {
            return Ok(());
        }
        self.calls += 1;
        let call_id = format!("call-{}", self.calls);
        let model_out = self.outputs[rng.pick(self.outputs.len())];
        let block = u32::try_from(rng.below(BLOCKS)).expect("a block index fits in u32");
        let intent = Fact::Intent(Intent {
            turn: Some(id.clone()),
            call_id: call_id.clone(),
            tool: TOOL.to_owned(),
            target: IntentTarget::ToolUse(BlockRef { model_out, block }),
        });
        let live = self.live.take().expect("the facts fold is live");
        let (live, _) = block_on(commit_fact(self.facts(), live, intent))
            .map_err(|error| format!("commit the intent of {call_id}: {error}"))?;
        self.live = Some(live);
        self.expected += 1;
        block_on(log.cancel(&id, &serde_json::Map::new()))
            .map_err(|error| format!("cancel {id}: {error}"))?;
        let wanted = call_id.clone();
        let open = block_on(log.read(move |fold| fold.intent(&wanted).is_some()));
        if open {
            return Err(format!(
                "cancelling {id} left its dispatched call {call_id} without a result"
            ));
        }
        self.expected += 2;
        self.durable = self.expected;
        self.stats.cancelled += 1;
        self.live = Some(
            block_on(fold_facts::<Ledger>(self.facts()))
                .map_err(|error| format!("refold after a cancel: {error}"))?,
        );
        self.check()
    }

    fn turns_hold(&self) -> Result<(), String> {
        let log = self.log.as_ref().expect("the facts log is open");
        for (id, state) in &self.submitted {
            let record = block_on(log.turn(id))
                .map_err(|error| format!("read turn {id}: {error}"))?
                .ok_or_else(|| format!("submitted turn {id} is missing"))?;
            if record.state != *state {
                return Err(format!(
                    "submitted turn {id} reads back {:?}, not {state:?}",
                    record.state
                ));
            }
        }
        Ok(())
    }

    fn step(&mut self, rng: &mut Rng) -> Result<(), String> {
        if rng.chance(SUBMIT_PPM) {
            self.submit(rng)?;
        }
        if rng.chance(HOST_PPM) {
            self.host_field(rng)?;
        }
        if rng.chance(CANCEL_PPM) {
            self.cancel(rng)?;
        }
        if rng.chance(STRAY_PPM) {
            self.stray()?;
        }
        if rng.chance(ABANDON_PPM) {
            self.abandon(rng)?;
        }
        match rng.below(100) {
            0..88 => self.commit(rng),
            88..94 => {
                self.crash()?;
                self.recover()
            }
            _ => {
                self.restart()?;
                self.recover()
            }
        }
    }
}

fn run_seed(seed: u64, mutation: OneLogFactsMutation) -> Outcome {
    facts_sim::mutate(mutation.log_rule());
    nanocode_turnlog::sim::mutate(match mutation {
        OneLogFactsMutation::CacheNeverDue => nanocode_turnlog::sim::Mutation::CacheNeverDue,
        OneLogFactsMutation::CacheInline => nanocode_turnlog::sim::Mutation::CacheInline,
        OneLogFactsMutation::ScopedInputLowered => {
            nanocode_turnlog::sim::Mutation::ScopedInputLowered
        }
        OneLogFactsMutation::FactsScanOnWriter => {
            nanocode_turnlog::sim::Mutation::FactsScanOnWriter
        }
        _ => nanocode_turnlog::sim::Mutation::None,
    });
    nanocode_durability_core::sim::mutate(match mutation {
        OneLogFactsMutation::FoldStopsAtOwnFact => {
            nanocode_durability_core::sim::Mutation::FoldStopsAtOwnFact
        }
        _ => nanocode_durability_core::sim::Mutation::None,
    });
    MUTATION.store(mutation as u8, std::sync::atomic::Ordering::SeqCst);
    crate::host::enter(seed, &Clock::new());
    let mut rng = Rng::derive(seed, "one_log_facts");
    let rows = usize::try_from(rng.range(SNAPSHOT_ROWS_MIN, SNAPSHOT_ROWS_MAX))
        .expect("a snapshot row count fits in usize");
    let mut world = World {
        host: Arc::new(MemoryLog::new()),
        hosts: BTreeMap::new(),
        log: None,
        facts: None,
        child: None,
        child_live: None,
        expected: 0,
        child_expected: 0,
        submitted: Vec::new(),
        durable: 0,
        live: None,
        stale: None,
        policy: Policy {
            snapshot_ratio: SNAPSHOT_RATIO,
            snapshot_rows: Some(rows),
            ..Policy::default()
        },
        outputs: Vec::new(),
        turns: 0,
        running: Vec::new(),
        open: Vec::new(),
        calls: 0,
        stats: OneLogFactsStats {
            seeds: 1,
            ..OneLogFactsStats::default()
        },
        shape: String::new(),
    };
    let steps = rng.range(STEPS.start, STEPS.end);
    let mut wrong = world.open().err();
    for step in 0..steps {
        if wrong.is_some() {
            break;
        }
        wrong = world
            .step(&mut rng)
            .err()
            .map(|message| format!("step {step}: {message}"));
    }
    if wrong.is_none() {
        wrong = world.late_child(&mut rng).err();
    }
    facts_sim::mutate(facts_sim::Mutation::None);
    nanocode_durability_core::sim::mutate(nanocode_durability_core::sim::Mutation::None);
    nanocode_turnlog::sim::mutate(nanocode_turnlog::sim::Mutation::None);
    MUTATION.store(
        OneLogFactsMutation::None as u8,
        std::sync::atomic::Ordering::SeqCst,
    );
    Outcome {
        digest: fnv(FNV_OFFSET, world.shape.as_bytes()),
        wrong,
        stats: world.stats,
        shape: format!("steps={steps} snapshot_rows={rows} ops={}", world.shape),
    }
}

pub fn judge(
    seed: u64,
    mutation: OneLogFactsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut OneLogFactsStats,
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

pub fn replay(seed: u64, mutation: OneLogFactsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario one_log_facts --seed {seed}");
    if mutation != OneLogFactsMutation::None {
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
    mutation: OneLogFactsMutation,
    totals: &OneLogFactsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=one_log_facts mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        OneLogFactsMutation::None if failed == 0 => ExitCode::SUCCESS,
        OneLogFactsMutation::None => ExitCode::FAILURE,
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

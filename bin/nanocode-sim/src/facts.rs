use std::{
    cell::Cell,
    collections::BTreeMap,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use nanocode_durability_core::{
    Base, BlockRef, Decision, Digest, DurableFold, EffectResult, Fact, FoldError, Folded, Input,
    InputKind, Intent, IntentTarget, Marker, ModelOut, Outcome, OwnedFact, Seen, Snapshot,
    StoredFact, TurnStatus, VALUE_INLINE_BYTES, referenced,
};
use nanocode_sync::FutureExt;
use nanocode_turnlog::{Lease, LogHost, MemoryLog, Phase, Point, Record, TurnLog};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const SCOPE: &str = "facts";
const LEGACY_STATE: &str = "session";
const CHAT_FAMILY: &str = "sim-chat";
const CHAT_FORMAT: u32 = 2;
const OPS_MIN: u64 = 30;
const OPS_MAX: u64 = 120;
const BLOCKS_MAX: u64 = 3;
const LEGACY_CHECKPOINTS_MAX: u64 = 4;
const LEGACY_PPM: u64 = 500_000;
const CRASH_PPM: u64 = 60_000;
const UNCOMMITTED_PPM: u64 = 250_000;
const BLOB_PPM: u64 = 100_000;
const STALE_PPM: u64 = 400_000;
const TICK_MS_MAX: u64 = 90_000;
const WORDS_MAX: u64 = 12;
const UNDECIDED_EVERY: u64 = 3;
const ROUTES: [&str; 2] = ["small", "large"];
const FINISH_PPM: u64 = 400_000;
const SNAPSHOT_ROWS_MIN: u64 = 2;
const SNAPSHOT_ROWS_MAX: u64 = 8;
const RETAIN_MAX: u64 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FactsMutation {
    None,
    UndecidedCompaction,
    DispatchBeforeIntent,
    ReplaysIntent,
    StaleSnapshotTrusted,
    BaseIgnored,
    SealedRelowered,
    FactsResident,
    BlobsResident,
}

impl FactsMutation {
    pub const ALL: [Self; 8] = [
        Self::UndecidedCompaction,
        Self::DispatchBeforeIntent,
        Self::ReplaysIntent,
        Self::StaleSnapshotTrusted,
        Self::BaseIgnored,
        Self::SealedRelowered,
        Self::FactsResident,
        Self::BlobsResident,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UndecidedCompaction => "undecided_compaction",
            Self::DispatchBeforeIntent => "dispatch_before_intent",
            Self::ReplaysIntent => "replays_intent",
            Self::StaleSnapshotTrusted => "stale_snapshot_trusted",
            Self::BaseIgnored => "base_ignored",
            Self::SealedRelowered => "sealed_relowered",
            Self::FactsResident => "facts_resident",
            Self::BlobsResident => "blobs_resident",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        std::iter::once(Self::None)
            .chain(Self::ALL)
            .find(|mutation| mutation.name() == name)
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(FactsMutation::None as u8);

fn mutated(mutation: FactsMutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

thread_local! {
    static APPLIED: Cell<u64> = const { Cell::new(0) };
}

pub fn names() -> String {
    FactsMutation::ALL
        .iter()
        .map(|mutation| mutation.name())
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Default, Deserialize, Serialize)]
struct Chat {
    base: Option<u64>,
    messages: Vec<Box<RawValue>>,
    cut: usize,
    route: String,
    clock: u64,
    open: BTreeMap<String, BlockRef>,
}

#[derive(Deserialize, Serialize)]
enum ChatDecision {
    Compact { keep: u64 },
}

#[derive(Deserialize, Serialize)]
struct Legacy {
    messages: Vec<Box<RawValue>>,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    at: u64,
    messages: &'a [Box<RawValue>],
}

impl Chat {
    fn kept(&self, keep: u64, seq: u64) -> Result<usize, FoldError> {
        usize::try_from(keep)
            .ok()
            .and_then(|keep| self.messages.len().checked_sub(keep))
            .ok_or_else(|| FoldError::Rejected {
                seq,
                reason: format!("it keeps {keep} of {} messages", self.messages.len()),
            })
    }
}

impl DurableFold for Chat {
    const FAMILY: &'static str = CHAT_FAMILY;
    const FORMAT: u32 = CHAT_FORMAT;
    type Decision = ChatDecision;

    fn apply(&mut self, seq: u64, fact: Seen<'_, ChatDecision>) -> Result<(), FoldError> {
        let rejected = |reason: &str| FoldError::Rejected {
            seq,
            reason: reason.to_owned(),
        };
        APPLIED.with(|applied| applied.set(applied.get() + 1));
        match fact {
            Fact::Base(base) => {
                if !mutated(FactsMutation::BaseIgnored) {
                    let legacy: Legacy = serde_json::from_str(base.checkpoint.get())
                        .map_err(|error| rejected(&format!("legacy checkpoint: {error}")))?;
                    self.messages = legacy.messages;
                }
                self.base = Some(base.revision);
            }
            Fact::Input(input) => self.messages.push(input.value.to_owned()),
            Fact::ModelOut(out) => {
                self.messages.push(out.response.to_owned());
                if mutated(FactsMutation::UndecidedCompaction)
                    && APPLIED.with(Cell::get).is_multiple_of(UNDECIDED_EVERY)
                {
                    self.cut = self.messages.len();
                }
            }
            Fact::Intent(intent) => {
                let IntentTarget::ToolUse(block) = intent.target else {
                    return Err(rejected("a nested intent has no cell here"));
                };
                if self.open.insert(intent.call_id, block).is_some() {
                    return Err(rejected("an intent repeats an open call"));
                }
            }
            Fact::EffectResult(result) => {
                if self.open.remove(&result.call_id).is_none() {
                    return Err(rejected("a result names no open intent"));
                }
                self.messages.push(result.output.to_owned());
            }
            Fact::Decision(Decision::Backend(ChatDecision::Compact { keep })) => {
                self.cut = self.kept(*keep, seq)?;
            }
            Fact::Decision(Decision::Time { unix_millis }) => self.clock = unix_millis,
            Fact::Decision(Decision::Route { model }) => self.route = model,
            Fact::Decision(Decision::Id { .. } | Decision::Host { .. }) | Fact::Marker(_) => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        serde_json::to_vec(&Request {
            model: &self.route,
            at: self.clock,
            messages: &self.messages[self.cut..],
        })
        .map_err(|error| FoldError::Encode(error.to_string()))
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
struct Durable {
    state: String,
    request: Vec<u8>,
    seq: u64,
}

impl Durable {
    fn of(folded: &Folded<Chat>) -> Result<Self, String> {
        Ok(Self {
            state: serde_json::to_string(folded.state()).map_err(|error| error.to_string())?,
            request: folded.request().map_err(|error| error.to_string())?,
            seq: folded.seq(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FactsStats {
    facts: u64,
    crashes: u64,
    lost: u64,
    unknown: u64,
    blobs: u64,
    sealed: u64,
    resident: u64,
    written: u64,
    cached: u64,
    stale: u64,
    imported: u64,
}

struct World {
    rng: Rng,
    host: Arc<MemoryLog>,
    log: TurnLog,
    folded: Option<Folded<Chat>>,
    durable: Durable,
    dispatched: BTreeMap<String, u64>,
    legacy: Option<Vec<String>>,
    calls: u64,
    turns: u64,
    clock: u64,
    digest: u64,
    shape: Shape,
    finished: BTreeMap<String, bool>,
    snapshots: u64,
    stats: FactsStats,
}

#[derive(Clone, Copy)]
struct Shape {
    snapshot_rows: usize,
    retain: usize,
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    future
        .now_or_never()
        .expect("the simulated turn log host answers every call at once")
}

fn raw(json: String) -> Box<RawValue> {
    RawValue::from_string(json).expect("the simulator writes valid JSON")
}

fn failed(what: &str) -> impl Fn(FoldError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

pub fn logged(lease: &mut Lease<'_>) -> Result<Vec<(u64, StoredFact)>, String> {
    let mut facts = Vec::new();
    ready(lease.facts_from(0, |seq, _, fact| facts.push((seq, fact.clone()))))
        .map_err(|error| format!("read the log's facts: {error}"))?;
    if let Some(gap) = facts
        .iter()
        .zip(1..)
        .find(|((seq, _), expected)| seq != expected)
    {
        return Err(format!(
            "the log reads fact {} where fact {} belongs",
            (gap.0).0,
            gap.1
        ));
    }
    Ok(facts)
}

pub fn logged_blobs(
    lease: &mut Lease<'_>,
    facts: &[(u64, StoredFact)],
) -> Result<BTreeMap<Digest, Box<RawValue>>, String> {
    let digests = referenced(facts.iter().map(|(_, fact)| fact));
    ready(lease.blobs(&digests)).map_err(|error| format!("read the log's blobs: {error}"))
}

pub fn blob<'a>(
    blobs: &'a BTreeMap<Digest, Box<RawValue>>,
    digest: &Digest,
) -> Result<&'a RawValue, FoldError> {
    blobs
        .get(digest)
        .map(|json| &**json)
        .ok_or(FoldError::MissingBlob { digest: *digest })
}

fn open(host: &Arc<MemoryLog>, shape: Shape) -> Result<TurnLog, String> {
    let host: Arc<dyn LogHost> = host.clone();
    ready(TurnLog::open(host, SCOPE))
        .map(|log| {
            log.snapshot_rows(Some(shape.snapshot_rows))
                .retain_finished(Some(shape.retain))
        })
        .map_err(|error| format!("open the log: {error}"))
}

fn legacy_log(rng: &mut Rng, host: &Arc<MemoryLog>, shape: Shape) -> Result<Vec<String>, String> {
    let log = open(host, shape)?;
    let mut messages = Vec::new();
    for revision in 1..=rng.range(1, LEGACY_CHECKPOINTS_MAX) {
        messages.push(format!("\"legacy {revision} {}\"", rng.draw()));
        let payload = format!("{{\"messages\":[{}]}}", messages.join(","));
        let mut lease = ready(log.lease());
        lease
            .push(Record::Checkpoint {
                payload,
                revision,
                state: LEGACY_STATE.to_owned(),
            })
            .map_err(|error| format!("write a legacy checkpoint: {error}"))?;
        ready(lease.commit(Point::Progress))
            .map_err(|error| format!("commit a legacy checkpoint: {error}"))?;
    }
    Ok(messages)
}

impl World {
    fn new(seed: u64) -> Result<Self, String> {
        let mut rng = Rng::derive(seed, SCOPE);
        let host = Arc::new(MemoryLog::new());
        let shape = Shape {
            snapshot_rows: usize::try_from(rng.range(SNAPSHOT_ROWS_MIN, SNAPSHOT_ROWS_MAX))
                .expect("a row count fits"),
            retain: usize::try_from(rng.range(1, RETAIN_MAX)).expect("a turn count fits"),
        };
        let legacy = if rng.chance(LEGACY_PPM) {
            Some(legacy_log(&mut rng, &host, shape)?)
        } else {
            None
        };
        let log = open(&host, shape)?;
        let mut world = Self {
            rng,
            host,
            log,
            folded: Some(Folded::default()),
            durable: Durable::default(),
            dispatched: BTreeMap::new(),
            legacy,
            calls: 0,
            turns: 0,
            clock: 0,
            digest: FNV_OFFSET,
            shape,
            finished: BTreeMap::new(),
            snapshots: 0,
            stats: FactsStats::default(),
        };
        world.durable = Durable::of(world.fold())?;
        world.reopen()?;
        Ok(world)
    }

    const fn fold(&self) -> &Folded<Chat> {
        self.folded
            .as_ref()
            .expect("the world holds a fold between steps")
    }

    fn commit(
        &mut self,
        fact: OwnedFact<ChatDecision>,
        point: Point,
    ) -> Result<Option<u64>, String> {
        let mut lease = ready(self.log.lease());
        let folded = self
            .folded
            .take()
            .expect("the world holds a fold between steps");
        let blobs = folded.values().len();
        let unsealed = lease.fold().next_fact();
        let (folded, seq) = folded
            .commit(&mut lease, fact)
            .map_err(failed("commit a fact"))?;
        self.stats.facts += 1;
        self.stats.blobs += (folded.values().len() - blobs) as u64;
        let facts = logged(&mut lease)?;
        let blobs = crate::facts::logged_blobs(&mut lease, &facts)?;
        if let Some(divergence) = folded
            .diverges(facts, |digest| crate::facts::blob(&blobs, digest))
            .map_err(failed("refold the log"))?
        {
            return Err(format!("{divergence} after a commit"));
        }
        if self.rng.chance(UNCOMMITTED_PPM) && self.rng.chance(CRASH_PPM) {
            drop(lease);
            self.stats.lost += 1;
            self.folded = Some(folded);
            self.reopen()?;
            return Ok(None);
        }
        lease.retire_finished();
        ready(lease.commit(point)).map_err(|error| format!("commit the log: {error}"))?;
        drop(lease);
        self.check_sealed(unsealed)?;
        self.durable = Durable::of(&folded)?;
        self.digest = fnv(self.digest, self.durable.request.as_slice());
        self.folded = Some(folded);
        Ok(Some(seq))
    }

    fn check_sealed(&mut self, unsealed: u64) -> Result<(), String> {
        let snapshots = ready(self.log.stats()).snapshots;
        if snapshots == self.snapshots {
            return Ok(());
        }
        self.snapshots = snapshots;
        let (resident, next, bodies, markers) = ready(self.log.read(|fold| {
            let markers = fold
                .facts()
                .filter(|(_, fact)| matches!(fact, Fact::Marker(Marker::Blob { .. })))
                .count();
            (
                fold.facts().count() as u64,
                fold.next_fact(),
                fold.unowned_stored(),
                markers,
            )
        }));
        if bodies > markers {
            return Err(format!(
                "after a snapshot the log keeps {bodies} blob bodies resident for {markers} resident blob facts"
            ));
        }
        let written = next - unsealed;
        if resident > written {
            return Err(format!(
                "after a snapshot the log keeps {resident} facts resident, more than the {written} its commit wrote"
            ));
        }
        self.stats.sealed = self.stats.sealed.max(next - 1 - resident);
        Ok(())
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.snapshots = 0;
        self.folded = None;
        self.log = open(&self.host, self.shape)?;
        let mut lease = ready(self.log.lease());
        if self.legacy.is_some() {
            ready(lease.load_owner(LEGACY_STATE))
                .map_err(|error| format!("load the legacy owner: {error}"))?;
        }
        let facts = logged(&mut lease)?;
        let blobs = crate::facts::logged_blobs(&mut lease, &facts)?;
        let fold = lease.fold();
        let cached = fold
            .cached()
            .map_err(|error| format!("read the cached fold: {error}"))?;
        let (folded, _) = Folded::<Chat>::open(cached, facts.clone(), |digest| {
            crate::facts::blob(&blobs, digest)
        })
        .map_err(failed("reopen the fold"))?;
        if Durable::of(&folded)? != self.durable {
            return Err(format!(
                "after a restart at fact {} the fold differs from the live state at fact {}",
                folded.seq(),
                self.durable.seq
            ));
        }
        if let Some(divergence) = folded
            .diverges(facts.clone(), |digest| crate::facts::blob(&blobs, digest))
            .map_err(failed("refold after a restart"))?
        {
            return Err(format!("{divergence} after a restart"));
        }
        for (call, times) in &self.dispatched {
            let held = facts
                .iter()
                .any(|(_, fact)| matches!(fact, Fact::Intent(intent) if intent.call_id == *call));
            if !held {
                return Err(format!(
                    "call {call} was dispatched without a durable intent"
                ));
            }
            if *times > 1 {
                return Err(format!("call {call} was dispatched {times} times"));
            }
        }
        self.check_views(fold, &folded)?;
        let checkpoint = fold.checkpoint(LEGACY_STATE).cloned();
        let based = facts
            .iter()
            .filter(|(_, fact)| matches!(fact, Fact::Base(_)))
            .count();
        drop(lease);
        self.folded = Some(folded);
        self.stats.crashes += 1;
        self.import(checkpoint, based)?;
        let unknown: Vec<String> = self.fold().state().open.keys().cloned().collect();
        for call in unknown {
            self.stats.unknown += 1;
            if mutated(FactsMutation::ReplaysIntent) {
                self.dispatch(&call);
            }
            let output = raw(format!("{{\"call\":\"{call}\",\"outcome\":\"unknown\"}}"));
            let result = Fact::EffectResult(EffectResult {
                call_id: call,
                output,
            });
            self.commit(result, Point::Recovery)?;
        }
        Ok(())
    }

    fn import(
        &mut self,
        checkpoint: Option<nanocode_turnlog::Checkpoint>,
        based: usize,
    ) -> Result<(), String> {
        let Some(legacy) = self.legacy.as_ref().map(Vec::len) else {
            return Ok(());
        };
        let messages = self.legacy.clone().unwrap_or_default();
        let checkpoint = checkpoint.ok_or("the legacy checkpoint is gone after the import")?;
        let expected = format!("{{\"messages\":[{}]}}", messages.join(","));
        if checkpoint.payload != expected {
            return Err("the legacy checkpoint reads back changed".to_owned());
        }
        match based {
            0 if self.fold().seq() == 0 => {}
            0 => return Err("facts precede the legacy base".to_owned()),
            1 => return self.check_base(legacy),
            more => return Err(format!("the legacy base was imported {more} times")),
        }
        let base = Fact::Base(Base {
            state: LEGACY_STATE.to_owned(),
            revision: checkpoint.revision,
            checkpoint: raw(checkpoint.payload),
        });
        self.commit(base, Point::Progress)?;
        self.stats.imported += 1;
        self.check_base(legacy)
    }

    fn check_base(&self, legacy: usize) -> Result<(), String> {
        let state = self.fold().state();
        if state.base.is_none() || state.messages.len() < legacy {
            return Err(format!(
                "the fold holds {} messages after importing {legacy} legacy ones",
                state.messages.len()
            ));
        }
        Ok(())
    }

    fn check_views(
        &self,
        fold: &nanocode_turnlog::Fold,
        folded: &Folded<Chat>,
    ) -> Result<(), String> {
        let open: Vec<&str> = fold.open_calls().map(|(call, _)| call).collect();
        let held: Vec<&str> = folded.state().open.keys().map(String::as_str).collect();
        if open != held {
            return Err(format!(
                "the log's open calls {open:?} differ from the fold's {held:?}"
            ));
        }
        for (turn, finished) in &self.finished {
            match fold.turn(turn).map(|turn| &turn.phase) {
                Some(Phase::Finished { .. }) if *finished => {}
                Some(Phase::Running { .. }) if !*finished => {}
                None if fold.retired(turn).is_some() && *finished => {}
                phase => {
                    return Err(format!(
                        "turn {turn} reads back as {phase:?} after a restart, finished={finished}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, call: &str) {
        *self.dispatched.entry(call.to_owned()).or_default() += 1;
    }

    fn prompt(&mut self) -> Result<(), String> {
        self.turns += 1;
        let words: Vec<String> = (0..self.rng.range(1, WORDS_MAX))
            .map(|_| format!("{:x}", self.rng.draw()))
            .collect();
        let mut text = words.join(" ");
        if self.rng.chance(BLOB_PPM) {
            text.push_str(&"x".repeat(VALUE_INLINE_BYTES));
        }
        let value = to_raw_value(&text).map_err(|error| error.to_string())?;
        let turn = format!("turn-{}", self.turns);
        let fact = Fact::Input(Input {
            turn: turn.clone(),
            kind: InputKind::Prompt,
            value,
            metadata: raw("{}".to_owned()),
            hand: None,
            first: None,
        });
        if self.commit(fact, Point::Admission)?.is_none() {
            return Ok(());
        }
        let running = Fact::Marker(Marker::Status {
            turn: turn.clone(),
            status: TurnStatus::Running,
        });
        if self.commit(running, Point::Progress)?.is_some() {
            self.finished.insert(turn, false);
        }
        Ok(())
    }

    fn finish(&mut self, turn: String, last: u64) -> Result<(), String> {
        if self.finished.get(&turn) != Some(&false) {
            return Ok(());
        }
        let finished = Fact::Marker(Marker::Status {
            turn: turn.clone(),
            status: TurnStatus::Finished {
                last,
                outcome: Outcome::Completed,
                result: raw("{}".to_owned()),
            },
        });
        if self.commit(finished, Point::Finish)?.is_some() {
            self.finished.insert(turn, true);
        }
        Ok(())
    }

    fn model(&mut self) -> Result<(), String> {
        let blocks = self.rng.below(BLOCKS_MAX + 1);
        let calls: Vec<String> = (0..blocks)
            .map(|_| {
                self.calls += 1;
                format!("call-{}", self.calls)
            })
            .collect();
        let content: Vec<String> = std::iter::once("{\"type\":\"text\"}".to_owned())
            .chain(
                calls
                    .iter()
                    .map(|call| format!("{{\"type\":\"tool_use\",\"id\":\"{call}\"}}")),
            )
            .collect();
        let response = raw(format!("{{\"content\":[{}]}}", content.join(",")));
        let turn = format!("turn-{}", self.turns);
        let out = Fact::ModelOut(ModelOut {
            turn: turn.clone(),
            response,
        });
        let Some(model_out) = self.commit(out, Point::Progress)? else {
            return Ok(());
        };
        for (index, call) in calls.into_iter().enumerate() {
            let block = u32::try_from(index + 1).expect("a block index fits");
            let crashes = self.stats.crashes;
            self.call(&turn, call, BlockRef { model_out, block })?;
            if self.stats.crashes != crashes {
                return Ok(());
            }
        }
        if self.rng.chance(FINISH_PPM) {
            return self.finish(turn, model_out);
        }
        Ok(())
    }

    fn call(&mut self, turn: &str, call: String, block: BlockRef) -> Result<(), String> {
        let intent = Fact::Intent(Intent {
            turn: Some(turn.to_owned()),
            call_id: call.clone(),
            tool: "Bash".to_owned(),
            target: IntentTarget::ToolUse(block),
        });
        if mutated(FactsMutation::DispatchBeforeIntent) {
            self.dispatch(&call);
            if self.rng.chance(UNCOMMITTED_PPM) {
                return self.crash();
            }
        }
        if self.commit(intent, Point::Call)?.is_none() {
            return Ok(());
        }
        if !mutated(FactsMutation::DispatchBeforeIntent) {
            self.dispatch(&call);
        }
        if self.rng.chance(CRASH_PPM) {
            return self.crash();
        }
        let output = raw(format!("{{\"call\":\"{call}\",\"exit\":0}}"));
        let result = Fact::EffectResult(EffectResult {
            call_id: call,
            output,
        });
        self.commit(result, Point::Progress).map(drop)
    }

    fn decide(&mut self) -> Result<(), String> {
        let decision = match self.rng.below(3) {
            0 => {
                self.clock += self.rng.range(1, TICK_MS_MAX);
                Decision::Time {
                    unix_millis: self.clock,
                }
            }
            1 => Decision::Route {
                model: ROUTES[self.rng.pick(ROUTES.len())].to_owned(),
            },
            _ => {
                let messages = self.fold().state().messages.len() as u64;
                Decision::Backend(ChatDecision::Compact {
                    keep: self.rng.below(messages + 1),
                })
            }
        };
        self.commit(Fact::Decision(decision), Point::Progress)
            .map(drop)
    }

    fn cache(&mut self) -> Result<(), String> {
        let mut snapshot = self
            .fold()
            .snapshot()
            .map_err(failed("snapshot the fold"))?;
        if self.rng.chance(STALE_PPM) {
            self.stats.stale += 1;
            let mut stale: Chat = serde_json::from_str(snapshot.state.get())
                .map_err(|error| format!("decode a snapshot: {error}"))?;
            stale.route = "retired".to_owned();
            stale.messages.clear();
            snapshot = Snapshot {
                format: if mutated(FactsMutation::StaleSnapshotTrusted) {
                    CHAT_FORMAT
                } else {
                    CHAT_FORMAT - 1
                },
                state: to_raw_value(&stale).map_err(|error| error.to_string())?,
                ..snapshot
            };
        }
        let mut lease = ready(self.log.lease());
        lease
            .cache(&snapshot)
            .map_err(|error| format!("cache the fold: {error}"))?;
        ready(lease.commit(Point::Progress)).map_err(|error| format!("commit a cache: {error}"))?;
        self.stats.cached += 1;
        Ok(())
    }

    fn crash(&mut self) -> Result<(), String> {
        self.reopen()
    }

    fn step(&mut self) -> Result<(), String> {
        match self.rng.below(6) {
            0 | 1 => self.prompt(),
            2 | 3 => self.model(),
            4 => self.decide(),
            _ if self.rng.chance(CRASH_PPM * 4) => self.crash(),
            _ => self.cache(),
        }
    }
}

fn run(seed: u64, verbose: bool) -> Result<u64, String> {
    APPLIED.with(|applied| applied.set(0));
    let mut world = World::new(seed)?;
    let ops = world.rng.range(OPS_MIN, OPS_MAX);
    for _ in 0..ops {
        world.step()?;
    }
    world.crash()?;
    world.stats.written = world.host.writes().bytes;
    world.stats.resident = ready(world.log.read(|fold| fold.facts().count() as u64));
    if verbose {
        println!("seed {seed} ops={ops} stats={:?}", world.stats);
    }
    Ok(world.digest)
}

fn run_seed(seed: u64, mutation: FactsMutation, verbose: bool) -> Result<u64, String> {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
    match mutation {
        FactsMutation::SealedRelowered => {
            nanocode_turnlog::sim::mutate(nanocode_turnlog::sim::Mutation::SealedRelowered);
        }
        FactsMutation::FactsResident => {
            nanocode_turnlog::sim::mutate(nanocode_turnlog::sim::Mutation::FactsResident);
        }
        FactsMutation::BlobsResident => {
            nanocode_turnlog::sim::mutate(nanocode_turnlog::sim::Mutation::BlobsResident);
        }
        _ => {}
    }
    let outcome = std::panic::catch_unwind(|| run(seed, verbose));
    nanocode_turnlog::sim::mutate(nanocode_turnlog::sim::Mutation::None);
    MUTATION.store(FactsMutation::None as u8, Ordering::SeqCst);
    outcome.unwrap_or_else(|panic| {
        Err(format!(
            "panic: {}",
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                .unwrap_or_default()
        ))
    })
}

pub fn judge(seed: u64, mutation: FactsMutation, twice: bool, verbose: bool) -> Option<String> {
    let first = match run_seed(seed, mutation, verbose) {
        Ok(digest) => digest,
        Err(wrong) => return Some(wrong),
    };
    if !twice {
        return None;
    }
    match run_seed(seed, mutation, false) {
        Ok(second) if second == first => None,
        Ok(second) => Some(format!(
            "nondeterminism: digest {first:016x} then {second:016x}"
        )),
        Err(wrong) => Some(wrong),
    }
}

pub fn replay(seed: u64, mutation: FactsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario facts --seed {seed}");
    if mutation != FactsMutation::None {
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
    mutation: FactsMutation,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=facts mutation={} passed={passed} failed={failed} twice={twice}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        FactsMutation::None if failed == 0 => ExitCode::SUCCESS,
        FactsMutation::None => ExitCode::FAILURE,
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

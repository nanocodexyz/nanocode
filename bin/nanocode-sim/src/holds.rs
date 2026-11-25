use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{ImportedTurn, TurnState},
    turnlog::{LogHost, OneLog, Policy},
};
use nanocode_sync::{Atom, block_on};
use nanocode_tool_runtime::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, Tools,
    embedded::HostFuture,
    hold::{Answer, HoldCall, HoldError, HoldPolicy, Holds, Judgment, LedgerError, Resumption},
    runtime::{DynamicToolProvider, ToolRuntime},
    sim::{self as host_sim, Mutation as HostRule},
};
use nanocode_turnlog::{
    Hold, MemoryLog,
    sim::{self as log_sim, Mutation as Rule},
};
use serde_json::{Value, json, value::to_raw_value};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const SESSION: &str = "holds";
const SCOPE: &str = "holds";
const SEND: &str = "send";
const RELAY: &str = "relay";
const SENT: &str = "sent";
const HELD: &str = "held for an answer";
const STEPS: Range<u64> = 40..400;
const KEYS: u64 = 12;
const UNKNOWN_KEYS: u64 = 3;
const SNAPSHOT_RATIO: usize = 2;
const SNAPSHOT_ROWS_MIN: u64 = 2;
const SNAPSHOT_ROWS_MAX: u64 = 8;
const RETAIN_MAX: u64 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldsMutation {
    None,
    ForgetsAnswer,
    ResumeIgnoresAnswer,
    PrunesUnsettled,
}

impl HoldsMutation {
    const ALL: [Self; 4] = [
        Self::None,
        Self::ForgetsAnswer,
        Self::ResumeIgnoresAnswer,
        Self::PrunesUnsettled,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ForgetsAnswer => "forgets_answer",
            Self::ResumeIgnoresAnswer => "resume_ignores_answer",
            Self::PrunesUnsettled => "prunes_unsettled",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::ForgetsAnswer => Rule::HoldForgetsAnswer,
            Self::PrunesUnsettled => Rule::HoldPrunesUnsettled,
            Self::None | Self::ResumeIgnoresAnswer => Rule::None,
        }
    }

    const fn host_rule(self) -> HostRule {
        match self {
            Self::ResumeIgnoresAnswer => HostRule::ResumeIgnoresAnswer,
            Self::None | Self::ForgetsAnswer | Self::PrunesUnsettled => HostRule::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HoldsStats {
    pub seeds: u64,
    pub calls: u64,
    pub holds: u64,
    pub resolves: u64,
    pub refused: u64,
    pub resumed: u64,
    pub replays: u64,
    pub restarts: u64,
    pub uncertain: u64,
    pub relayed: u64,
    pub pruned: u64,
}

impl HoldsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.calls += other.calls;
        self.holds += other.holds;
        self.resolves += other.resolves;
        self.refused += other.refused;
        self.resumed += other.resumed;
        self.replays += other.replays;
        self.restarts += other.restarts;
        self.uncertain += other.uncertain;
        self.relayed += other.relayed;
        self.pruned += other.pruned;
    }
}

type Runs = Arc<Atom<BTreeMap<String, Vec<String>>>>;

#[derive(Clone)]
struct Mailer {
    runs: Runs,
}

impl Tool for Mailer {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            SEND,
            "Send a message",
            json!({
                "type": "object",
                "properties": {"key": {"type": "string"}, "text": {"type": "string"}},
                "required": ["text"]
            }),
        )
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 self,
        input: ToolInput,
        context: ToolContext<'life1>,
    ) -> HostFuture<'async_trait, ToolResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let call_id = context.call_id().to_owned();
        let input = match &input {
            ToolInput::Function(raw) => raw.get().to_owned(),
            ToolInput::Freeform(text) => text.clone(),
        };
        self.runs.update(|runs| {
            runs.entry(call_id.clone()).or_default().push(input.clone());
        });
        Box::pin(async { Ok(ToolOutput::text(SENT)) })
    }
}

struct Courier {
    runs: Runs,
}

impl Courier {
    fn definition() -> ToolDefinition {
        ToolDefinition::function(
            RELAY,
            "Relay a message through a provider",
            json!({
                "type": "object",
                "properties": {"key": {"type": "string"}, "text": {"type": "string"}},
                "required": ["text"]
            }),
        )
    }
}

impl DynamicToolProvider for Courier {
    fn available_definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn execute<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        name: &'life1 str,
        input: Value,
        context: ToolContext<'life2>,
    ) -> HostFuture<'async_trait, Option<ToolOutput>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let relays = name == RELAY;
        let call_id = context.call_id().to_owned();
        if relays {
            let input = input.to_string();
            self.runs.update(|runs| {
                runs.entry(call_id.clone()).or_default().push(input.clone());
            });
        }
        Box::pin(async move { relays.then(|| ToolOutput::text(SENT)) })
    }
}

struct Approvals;

impl HoldPolicy for Approvals {
    fn judge<'a>(&'a self, call: HoldCall<'a>) -> HostFuture<'a, Judgment> {
        let key = serde_json::from_str::<Value>(call.input.get())
            .ok()
            .and_then(|input| input.get("key").and_then(Value::as_str).map(str::to_owned));
        let card =
            to_raw_value(&json!({"tool": call.tool})).expect("a card of one string field encodes");
        Box::pin(async move {
            key.map_or(Judgment::Run, |key| Judgment::Hold {
                card,
                key,
                message: HELD.to_owned(),
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Waiting,
    Running { input: String, live: bool },
    Settled,
}

#[derive(Clone, Debug)]
struct Held {
    call_id: String,
    input: Value,
    phase: Phase,
    tool: &'static str,
    turn: String,
}

struct World {
    host: Arc<MemoryLog>,
    log: Option<Arc<OneLog>>,
    runtime: Option<ToolRuntime>,
    runs: Runs,
    expected: BTreeMap<String, Value>,
    model: BTreeMap<String, Held>,
    policy: Policy,
    stats: HoldsStats,
    shape: String,
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: HoldsStats,
    shape: String,
}

fn text_of(output: &ToolOutput) -> String {
    serde_json::to_string(&output.output).unwrap_or_default()
}

fn says(output: &ToolOutput, text: &str) -> bool {
    output.success && text_of(output).contains(text)
}

impl World {
    fn open(&mut self) {
        let host: Arc<dyn LogHost> = Arc::clone(&self.host) as Arc<dyn LogHost>;
        let log = block_on(OneLog::open(host, SCOPE, self.policy.clone()))
            .unwrap_or_else(|error| panic!("the holds log opens: {error}"));
        let tools = Tools::builder()
            .without_defaults()
            .tool(Mailer {
                runs: Arc::clone(&self.runs),
            })
            .shared_provider(Arc::new(Courier {
                runs: Arc::clone(&self.runs),
            }))
            .holds(Holds::new(Arc::new(Approvals), log.ledger()))
            .build()
            .expect("the holds tools build");
        self.runtime = Some(ToolRuntime::new_with_tools(
            std::env::temp_dir(),
            Vec::new(),
            &tools,
        ));
        self.log = Some(log);
    }

    const fn runtime(&self) -> &ToolRuntime {
        self.runtime
            .as_ref()
            .expect("the runtime is open between restarts")
    }

    const fn log(&self) -> &Arc<OneLog> {
        self.log.as_ref().expect("the log is open between restarts")
    }

    fn execute(&self, held: &Held, nested: bool) -> ToolOutput {
        let context = ToolContext::new("", SESSION, &held.call_id, &[], usize::MAX)
            .with_turn_id(Some(&held.turn));
        if nested {
            return block_on(self.runtime().execute_nested_tool(
                held.tool,
                held.input.clone(),
                context,
            ));
        }
        let raw = to_raw_value(&held.input).expect("a call's input encodes");
        block_on(
            self.runtime()
                .execute_tool(held.tool, ToolInput::Function(raw), context),
        )
        .unwrap_or_else(|error| ToolOutput::error(error.to_string()))
    }

    fn execute_raw(&self, held: &Held, raw: &str) -> ToolOutput {
        let context = ToolContext::new("", SESSION, &held.call_id, &[], usize::MAX)
            .with_turn_id(Some(&held.turn));
        let raw = serde_json::value::RawValue::from_string(raw.to_owned())
            .expect("a journaled input is JSON");
        block_on(
            self.runtime()
                .execute_tool(held.tool, ToolInput::Function(raw), context),
        )
        .unwrap_or_else(|error| ToolOutput::error(error.to_string()))
    }

    fn finish_turn(&self, turn: &str) -> Result<(), String> {
        let imported = ImportedTurn {
            request_id: turn.to_owned(),
            state: TurnState::Completed,
            prompt: Prompt::new(turn),
            hand: None,
            metadata: to_raw_value(&json!({})).expect("empty metadata encodes"),
            cancel_requested: false,
            note: None,
            first_seq: None,
            last_seq: None,
            result: None,
        };
        block_on(self.log().import(&[imported]))
            .map(drop)
            .map_err(|error| format!("finishing turn {turn}: {error}"))
    }

    fn pruned(&mut self) -> Result<(), String> {
        let held = block_on(self.log().read(|fold| {
            self.model
                .iter()
                .map(|(key, held)| {
                    (
                        key.clone(),
                        fold.hold(key).is_some(),
                        fold.turn(&held.turn).is_some(),
                    )
                })
                .collect::<Vec<_>>()
        }));
        for (key, present, retained) in held {
            if present {
                continue;
            }
            let phase = self.model[&key].phase.clone();
            if phase != Phase::Settled || retained {
                return Err(format!(
                    "hold {key} ({phase:?}, turn retained {retained}) left the log"
                ));
            }
            self.model.remove(&key);
            self.stats.pruned += 1;
            self.shape.push('-');
        }
        Ok(())
    }

    fn check_runs(&self) -> Result<(), String> {
        let runs = self.runs.get();
        for (call_id, inputs) in runs.iter() {
            if inputs.len() != 1 {
                return Err(format!("call {call_id} ran {} times", inputs.len()));
            }
            let Some(expected) = self.expected.get(call_id) else {
                return Err(format!("call {call_id} ran without being approved"));
            };
            let ran = serde_json::from_str::<Value>(&inputs[0]).unwrap_or(Value::Null);
            if &ran != expected {
                return Err(format!(
                    "call {call_id} ran with {ran}, not the answered {expected}"
                ));
            }
        }
        if let Some(missing) = self
            .expected
            .keys()
            .find(|call_id| !runs.contains_key(*call_id))
        {
            return Err(format!("call {missing} was answered to run but never ran"));
        }
        Ok(())
    }

    fn call(&mut self, rng: &mut Rng, step: u64) -> Result<(), String> {
        self.stats.calls += 1;
        let call_id = format!("c{step}");
        let key = rng.chance(600_000).then(|| format!("k{}", rng.below(KEYS)));
        let mut input = json!({"text": format!("t{step}")});
        if let Some(key) = &key {
            input["key"] = json!(key);
        }
        let nested = rng.chance(500_000);
        let tool = if rng.chance(400_000) { RELAY } else { SEND };
        self.stats.relayed += u64::from(tool == RELAY);
        let call = Held {
            call_id: call_id.clone(),
            input: input.clone(),
            phase: Phase::Waiting,
            tool,
            turn: format!("t{step}"),
        };
        self.finish_turn(&call.turn)?;
        self.pruned()?;
        let output = self.execute(&call, nested);
        let Some(key) = key else {
            self.shape.push('r');
            if !says(&output, SENT) {
                return Err(format!(
                    "unheld call {call_id} answered {}",
                    text_of(&output)
                ));
            }
            self.expected.insert(call_id, input);
            return Ok(());
        };
        if self.model.contains_key(&key) {
            self.stats.refused += 1;
            self.shape.push('x');
            if output.success {
                return Err(format!(
                    "call {call_id} reused key {key} and was answered {}",
                    text_of(&output)
                ));
            }
            return Ok(());
        }
        self.stats.holds += 1;
        self.shape.push('h');
        if !says(&output, HELD) {
            return Err(format!("held call {call_id} answered {}", text_of(&output)));
        }
        self.model.insert(key, call);
        Ok(())
    }

    fn resolve(&mut self, rng: &mut Rng, step: u64) -> Result<(), String> {
        self.stats.resolves += 1;
        self.shape.push('a');
        let key = format!("k{}", rng.below(KEYS + UNKNOWN_KEYS));
        let edited = json!({"key": key, "text": format!("edited-{step}")});
        let answer = match rng.below(5) {
            0 => Answer::Approved,
            1 => Answer::Edited {
                input: to_raw_value(&edited).expect("an edited input encodes"),
            },
            2 => Answer::Denied { reason: None },
            3 => Answer::Replied {
                message: format!("reply {step}"),
            },
            _ => Answer::Expired,
        };
        let runs = answer.answered().runs();
        let approved = match &answer {
            Answer::Edited { .. } => Some(edited),
            _ => None,
        };
        let result = block_on(self.log().ledger().resolve(&key, answer));
        let Some(held) = self.model.get_mut(&key) else {
            return match result {
                Err(LedgerError::Hold(HoldError::Unknown { .. })) => Ok(()),
                other => Err(format!("unknown key {key} resolved as {other:?}")),
            };
        };
        if held.phase != Phase::Waiting {
            return match result {
                Err(LedgerError::Hold(HoldError::Answered { .. })) => Ok(()),
                other => Err(format!(
                    "key {key} was answered twice: the second answer gave {other:?}"
                )),
            };
        }
        match result {
            Ok(Resumption::Run {
                call_id,
                input,
                tool,
            }) if runs && call_id == held.call_id && tool == held.tool => {
                let wanted = approved.unwrap_or_else(|| held.input.clone());
                let given = serde_json::from_str::<Value>(input.get()).unwrap_or(Value::Null);
                if given != wanted {
                    return Err(format!("key {key} resumes with {given}, not {wanted}"));
                }
                held.phase = Phase::Running {
                    input: input.get().to_owned(),
                    live: true,
                };
                Ok(())
            }
            Ok(Resumption::Answered { call_id, .. }) if !runs && call_id == held.call_id => {
                held.phase = Phase::Settled;
                Ok(())
            }
            other => Err(format!("waiting key {key} resolved as {other:?}")),
        }
    }

    fn resume(&mut self, rng: &mut Rng) -> Result<(), String> {
        let live = self
            .model
            .iter()
            .filter(|(_, held)| matches!(held.phase, Phase::Running { live: true, .. }))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if live.is_empty() {
            return Ok(());
        }
        let key = live[rng.pick(live.len())].clone();
        let held = self
            .model
            .get(&key)
            .expect("a live key is in the model")
            .clone();
        let Phase::Running { input, .. } = &held.phase else {
            unreachable!("live keys are running");
        };
        if rng.chance(200_000) {
            self.shape.push('t');
            let output = self.execute_raw(&held, r#"{"text":"tampered"}"#);
            if output.success {
                return Err(format!(
                    "key {key} ran a tampered input: {}",
                    text_of(&output)
                ));
            }
            return Ok(());
        }
        self.stats.resumed += 1;
        self.shape.push('e');
        let output = self.execute_raw(&held, input);
        if !says(&output, SENT) {
            return Err(format!("resumed key {key} answered {}", text_of(&output)));
        }
        let ran = serde_json::from_str::<Value>(input).expect("a resumed input is JSON");
        self.expected.insert(held.call_id.clone(), ran);
        self.model
            .get_mut(&key)
            .expect("a live key is in the model")
            .phase = Phase::Settled;
        Ok(())
    }

    fn replay(&mut self, rng: &mut Rng) -> Result<(), String> {
        if self.model.is_empty() {
            return Ok(());
        }
        self.stats.replays += 1;
        self.shape.push('p');
        let keys = self.model.keys().cloned().collect::<Vec<_>>();
        let key = keys[rng.pick(keys.len())].clone();
        let held = self.model[&key].clone();
        let live = match &held.phase {
            Phase::Running { input, live: true } => Some(input.clone()),
            _ => None,
        };
        let nested = live.is_none() && rng.chance(500_000);
        let output = self.execute(&held, nested);
        match (&held.phase, live) {
            (Phase::Waiting, _) if says(&output, HELD) => Ok(()),
            (Phase::Settled | Phase::Running { live: false, .. }, _) if !output.success => Ok(()),
            (Phase::Running { .. }, Some(input)) => {
                let original = serde_json::to_string(&held.input).expect("an input encodes");
                if original != input {
                    return if output.success {
                        Err(format!("key {key} ran its original input after an edit"))
                    } else {
                        Ok(())
                    };
                }
                if !says(&output, SENT) {
                    return Err(format!(
                        "replaying approved {key} answered {}",
                        text_of(&output)
                    ));
                }
                self.expected
                    .insert(held.call_id.clone(), held.input.clone());
                self.model
                    .get_mut(&key)
                    .expect("a replayed key is in the model")
                    .phase = Phase::Settled;
                Ok(())
            }
            (phase, _) => Err(format!(
                "replaying {key} ({phase:?}) answered {} success={}",
                text_of(&output),
                output.success
            )),
        }
    }

    fn restart(&mut self) -> Result<(), String> {
        self.stats.restarts += 1;
        self.shape.push('!');
        self.runtime = None;
        self.log = None;
        self.open();
        for held in self.model.values_mut() {
            if let Phase::Running { live, .. } = &mut held.phase {
                *live = false;
            }
        }
        let expected = self
            .model
            .iter()
            .filter(|(_, held)| matches!(held.phase, Phase::Running { live: false, .. }))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let uncertain = block_on(self.log().uncertain_holds());
        if uncertain != expected {
            return Err(format!(
                "after a restart the uncertain holds are {uncertain:?}, not {expected:?}"
            ));
        }
        let phases = block_on(self.log().read(|fold| {
            fold.holds()
                .map(|(key, hold)| {
                    (
                        key.to_owned(),
                        hold.phase(),
                        hold.call_id().to_owned(),
                        hold.turn().map(str::to_owned),
                    )
                })
                .collect::<Vec<_>>()
        }));
        for (key, phase, call_id, turn) in &phases {
            if self.model.contains_key(key) {
                continue;
            }
            if *phase != "settled" {
                return Err(format!(
                    "the reopened log holds {key} ({phase}), which no call made"
                ));
            }
            self.model.insert(
                key.clone(),
                Held {
                    call_id: call_id.clone(),
                    input: json!({"key": key, "text": "settled before a restart"}),
                    phase: Phase::Settled,
                    tool: SEND,
                    turn: turn.clone().unwrap_or_default(),
                },
            );
        }
        let phases = phases
            .into_iter()
            .map(|(key, phase, call_id, _)| (key, phase, call_id))
            .collect::<Vec<_>>();
        let wanted = self
            .model
            .iter()
            .map(|(key, held)| {
                let phase = match held.phase {
                    Phase::Waiting => "waiting",
                    Phase::Running { .. } => "running",
                    Phase::Settled => "settled",
                };
                (key.clone(), phase, held.call_id.clone())
            })
            .collect::<Vec<_>>();
        if phases != wanted {
            return Err(format!("the reopened log holds {phases:?}, not {wanted:?}"));
        }
        Ok(())
    }

    fn settle_uncertain(&mut self, rng: &mut Rng) -> Result<(), String> {
        let uncertain = block_on(self.log().uncertain_holds());
        if uncertain.is_empty() {
            return Ok(());
        }
        self.stats.uncertain += 1;
        self.shape.push('u');
        let key = uncertain[rng.pick(uncertain.len())].clone();
        block_on(self.log().ledger().release(&key))
            .map_err(|error| format!("settling uncertain {key}: {error}"))?;
        self.model
            .get_mut(&key)
            .expect("an uncertain key is in the model")
            .phase = Phase::Settled;
        let turn = self.model[&key].turn.clone();
        let (hold, retained) = block_on(
            self.log()
                .read(|fold| (fold.hold(&key).cloned(), fold.turn(&turn).is_some())),
        );
        match hold {
            Some(Hold::Settled { .. }) => {}
            None if !retained => {}
            _ => return Err(format!("uncertain {key} did not settle")),
        }
        Ok(())
    }

    fn step(&mut self, rng: &mut Rng, step: u64) -> Result<(), String> {
        match rng.below(100) {
            0..35 => self.call(rng, step)?,
            35..60 => self.resolve(rng, step)?,
            60..80 => self.resume(rng)?,
            80..90 => self.replay(rng)?,
            90..95 => self.restart()?,
            _ => self.settle_uncertain(rng)?,
        }
        self.pruned()?;
        self.check_runs()
    }
}

fn run_seed(seed: u64, mutation: HoldsMutation) -> Outcome {
    log_sim::mutate(mutation.rule());
    host_sim::mutate(mutation.host_rule());
    crate::host::enter(seed, &Clock::new());
    let mut rng = Rng::derive(seed, "holds");
    let rows = usize::try_from(rng.range(SNAPSHOT_ROWS_MIN, SNAPSHOT_ROWS_MAX))
        .expect("a snapshot row count fits in usize");
    let retain = rng.chance(500_000).then(|| {
        usize::try_from(rng.range(1, RETAIN_MAX)).expect("a retained turn count fits in usize")
    });
    let mut world = World {
        host: Arc::new(MemoryLog::new()),
        log: None,
        runtime: None,
        runs: Arc::default(),
        expected: BTreeMap::new(),
        model: BTreeMap::new(),
        policy: Policy {
            snapshot_ratio: SNAPSHOT_RATIO,
            snapshot_rows: Some(rows),
            retain_finished: retain,
            ..Policy::default()
        },
        stats: HoldsStats {
            seeds: 1,
            ..HoldsStats::default()
        },
        shape: String::new(),
    };
    world.open();
    let steps = rng.range(STEPS.start, STEPS.end);
    let mut wrong = None;
    for step in 0..steps {
        if let Err(message) = world.step(&mut rng, step) {
            wrong = Some(format!("step {step}: {message}"));
            break;
        }
    }
    log_sim::mutate(Rule::None);
    host_sim::mutate(HostRule::None);
    Outcome {
        digest: fnv(FNV_OFFSET, world.shape.as_bytes()),
        wrong,
        stats: world.stats,
        shape: format!(
            "steps={steps} snapshot_rows={rows} retain={retain:?} ops={}",
            world.shape
        ),
    }
}

pub fn judge(
    seed: u64,
    mutation: HoldsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut HoldsStats,
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

pub fn replay(seed: u64, mutation: HoldsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario holds --seed {seed}");
    if mutation != HoldsMutation::None {
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
    mutation: HoldsMutation,
    totals: &HoldsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=holds mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        HoldsMutation::None if failed == 0 => ExitCode::SUCCESS,
        HoldsMutation::None => ExitCode::FAILURE,
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

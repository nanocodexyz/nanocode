use std::{
    collections::BTreeMap,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use nanocode::claude::durability::{ClaudeDecision, ClaudeFold};
use nanocode_backend_codex::durability::{CodexDecision, CodexFold};
use nanocode_code_mode::contract::{ToolOutput, ToolOutputContent};
use nanocode_durability_core::{
    Base, BlockRef, Decision, DurableFold, EffectResult, Fact, FoldError, Folded, Input, InputKind,
    Intent, IntentTarget, ModelOut, OwnedFact, StoredFact, VALUE_INLINE_BYTES,
};
use nanocode_sync::FutureExt;
use nanocode_turnlog::{LogHost, MemoryLog, Point, Record, TurnLog};
use serde_json::{
    Value, json,
    value::{RawValue, to_raw_value},
};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const SCOPE: &str = "backend-facts";
const LEGACY_STATE: &str = "session";
const OPS_MIN: u64 = 20;
const OPS_MAX: u64 = 80;
const CALLS_MAX: u64 = 3;
const WORDS_MAX: u64 = 12;
const LEGACY_PPM: u64 = 500_000;
const CLAUDE_PPM: u64 = 500_000;
const CRASH_PPM: u64 = 60_000;
const UNCOMMITTED_PPM: u64 = 250_000;
const BLOB_PPM: u64 = 100_000;
const MEDIA_PPM: u64 = 250_000;
const CACHE_PPM: u64 = 100_000;
const TOOL_RESULT_TOKENS: u64 = 12_000;
const UNKNOWN_OUTCOME: &str = "outcome unknown: the call was dispatched before a restart";
const MEDIA_URL: &str = "data:image/png;base64,iVBORw0KGgo=";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BackendFactsMutation {
    None,
    ResultWithoutIntent,
    MediaUnrendered,
    UndecidedNotice,
}

impl BackendFactsMutation {
    pub const ALL: [Self; 3] = [
        Self::ResultWithoutIntent,
        Self::MediaUnrendered,
        Self::UndecidedNotice,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ResultWithoutIntent => "result_without_intent",
            Self::MediaUnrendered => "media_unrendered",
            Self::UndecidedNotice => "undecided_notice",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        std::iter::once(Self::None)
            .chain(Self::ALL)
            .find(|mutation| mutation.name() == name)
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(BackendFactsMutation::None as u8);

fn mutated(mutation: BackendFactsMutation) -> bool {
    MUTATION.load(Ordering::SeqCst) == mutation as u8
}

pub fn names() -> String {
    BackendFactsMutation::ALL
        .iter()
        .map(|mutation| mutation.name())
        .collect::<Vec<_>>()
        .join("|")
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    future
        .now_or_never()
        .expect("the simulated turn log host answers every call at once")
}

fn raw(value: &impl serde::Serialize) -> Result<Box<RawValue>, String> {
    to_raw_value(value).map_err(|error| error.to_string())
}

fn failed(what: &str) -> impl Fn(FoldError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn open(host: &Arc<MemoryLog>) -> Result<TurnLog, String> {
    let host: Arc<dyn LogHost> = host.clone();
    ready(TurnLog::open(host, SCOPE)).map_err(|error| format!("open the log: {error}"))
}

trait Family: DurableFold + Clone {
    const NAME: &'static str;
    fn template() -> Result<Self::Decision, String>;
    fn legacy(texts: &[String]) -> Value;
    fn response(seq: u64, text: &str, calls: &[String]) -> Value;
    fn notice(text: &str) -> Result<Self::Decision, String>;
    fn rendered(call: &str) -> Result<Option<Self::Decision>, String>;
    fn admitted() -> Option<Self::Decision>;
    fn messages(request: &[u8]) -> Result<Vec<String>, String>;
    fn expect_response(text: &str, calls: &[String], oracle: &mut Vec<Vec<String>>);
    fn expect_result(calls: &[String], index: usize, oracle: &mut Vec<Vec<String>>);
}

impl Family for ClaudeFold {
    const NAME: &'static str = "claude";

    fn template() -> Result<ClaudeDecision, String> {
        let request = serde_json::from_value(
            json!({"model": "claude-sim", "max_tokens": 1024, "messages": []}),
        )
        .map_err(|error| error.to_string())?;
        Ok(ClaudeDecision::Template {
            request,
            tool_result_tokens: TOOL_RESULT_TOKENS,
        })
    }

    fn legacy(texts: &[String]) -> Value {
        let messages = texts
            .iter()
            .map(|text| json!({"role": "user", "content": [{"type": "text", "text": text}]}))
            .collect::<Vec<_>>();
        json!({"provider": "claude", "version": 1, "conversation": {"messages": messages, "summary": ""}})
    }

    fn response(seq: u64, text: &str, calls: &[String]) -> Value {
        let content =
            std::iter::once(json!({"type": "text", "text": text}))
                .chain(calls.iter().map(
                    |call| json!({"type": "tool_use", "id": call, "name": "exec", "input": {}}),
                ))
                .collect::<Vec<_>>();
        json!({
            "id": format!("msg_{seq}"), "role": "assistant", "model": "claude-sim", "content": content,
            "stop_reason": if calls.is_empty() { "end_turn" } else { "tool_use" },
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })
    }

    fn notice(text: &str) -> Result<ClaudeDecision, String> {
        serde_json::from_value(json!({"role": "user", "content": [{"type": "text", "text": text}]}))
            .map(ClaudeDecision::Message)
            .map_err(|error| error.to_string())
    }

    fn admitted() -> Option<ClaudeDecision> {
        Some(ClaudeDecision::Admitted { input: 0 })
    }

    fn rendered(call: &str) -> Result<Option<ClaudeDecision>, String> {
        serde_json::from_value(json!({"type": "tool_result", "tool_use_id": call, "content": [{"type": "text", "text": "[image]"}]}))
            .map(|block| Some(ClaudeDecision::Rendered(block)))
            .map_err(|error| error.to_string())
    }

    fn messages(request: &[u8]) -> Result<Vec<String>, String> {
        let request: Value = serde_json::from_slice(request).map_err(|error| error.to_string())?;
        Ok(request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(Value::to_string)
            .collect())
    }

    fn expect_response(text: &str, _calls: &[String], oracle: &mut Vec<Vec<String>>) {
        oracle.push(vec![text.to_owned()]);
    }

    fn expect_result(calls: &[String], index: usize, oracle: &mut Vec<Vec<String>>) {
        if index + 1 == calls.len() {
            oracle.push(calls.to_vec());
        }
    }
}

impl Family for CodexFold {
    const NAME: &'static str = "codex";

    fn template() -> Result<CodexDecision, String> {
        Ok(CodexDecision::Template {
            request: raw(&json!({"model": "gpt-sim", "tool_choice": "auto"}))?,
            tool_output_tokens: usize::try_from(TOOL_RESULT_TOKENS)
                .map_err(|error| error.to_string())?,
        })
    }

    fn legacy(texts: &[String]) -> Value {
        let history = texts
            .iter()
            .map(|text| json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}))
            .collect::<Vec<_>>();
        json!({"conversation": {"history": history}})
    }

    fn response(seq: u64, text: &str, calls: &[String]) -> Value {
        let output = std::iter::once(json!({"type": "message", "role": "assistant", "id": format!("msg_{seq}"), "content": [{"type": "output_text", "text": text}]}))
            .chain(calls.iter().map(|call| json!({"type": "function_call", "id": format!("fc_{call}"), "call_id": call, "name": "exec", "arguments": "{}"})))
            .collect::<Vec<_>>();
        json!({"id": format!("resp_{seq}"), "output": output})
    }

    fn notice(text: &str) -> Result<CodexDecision, String> {
        serde_json::from_value(json!([{"type": "message", "role": "developer", "content": [{"type": "input_text", "text": text}]}]))
            .map(CodexDecision::Items)
            .map_err(|error| error.to_string())
    }

    fn admitted() -> Option<CodexDecision> {
        None
    }

    fn rendered(_call: &str) -> Result<Option<CodexDecision>, String> {
        Ok(None)
    }

    fn messages(request: &[u8]) -> Result<Vec<String>, String> {
        let request: Value = serde_json::from_slice(request).map_err(|error| error.to_string())?;
        Ok(request["input"]
            .as_array()
            .into_iter()
            .flatten()
            .map(Value::to_string)
            .collect())
    }

    fn expect_response(text: &str, calls: &[String], oracle: &mut Vec<Vec<String>>) {
        oracle.push(vec![text.to_owned()]);
        oracle.extend(calls.iter().map(|call| vec![call.clone()]));
    }

    fn expect_result(calls: &[String], index: usize, oracle: &mut Vec<Vec<String>>) {
        oracle.push(vec![calls[index].clone()]);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CallState {
    id: String,
    media: bool,
    intended: bool,
    resulted: bool,
    rendered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Round {
    turn: String,
    model_out: u64,
    calls: Vec<CallState>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Durable {
    state: String,
    request: Option<Vec<u8>>,
    seq: u64,
    oracle: Vec<Vec<String>>,
    round: Option<Round>,
    queued: Vec<String>,
}

struct World<F: Family> {
    rng: Rng,
    host: Arc<MemoryLog>,
    log: TurnLog,
    folded: Option<Folded<F>>,
    durable: Durable,
    oracle: Vec<Vec<String>>,
    round: Option<Round>,
    queued: Vec<String>,
    dispatched: BTreeMap<String, u64>,
    legacy: Option<Vec<String>>,
    turns: u64,
    calls: u64,
    digest: u64,
}

impl<F: Family> World<F> {
    fn new(seed: u64) -> Result<Self, String> {
        let mut rng = Rng::derive(seed, F::NAME);
        let host = Arc::new(MemoryLog::new());
        let legacy = if rng.chance(LEGACY_PPM) {
            Some(Self::legacy_log(&mut rng, &host)?)
        } else {
            None
        };
        let log = open(&host)?;
        let mut world = Self {
            rng,
            host,
            log,
            folded: Some(Folded::default()),
            durable: Durable::default(),
            oracle: Vec::new(),
            round: None,
            queued: Vec::new(),
            dispatched: BTreeMap::new(),
            legacy,
            turns: 0,
            calls: 0,
            digest: FNV_OFFSET,
        };
        world.durable = world.durable(world.fold())?;
        world.reopen()?;
        Ok(world)
    }

    fn legacy_log(rng: &mut Rng, host: &Arc<MemoryLog>) -> Result<Vec<String>, String> {
        let log = open(host)?;
        let mut texts = Vec::new();
        for revision in 1..=rng.range(1, 4) {
            texts.push(format!("legacy-{revision}-{:x}", rng.draw()));
            let mut lease = ready(log.lease());
            lease
                .push(Record::Checkpoint {
                    payload: F::legacy(&texts).to_string(),
                    revision,
                    state: LEGACY_STATE.to_owned(),
                })
                .map_err(|error| format!("write a legacy checkpoint: {error}"))?;
            ready(lease.commit(Point::Progress))
                .map_err(|error| format!("commit a legacy checkpoint: {error}"))?;
        }
        Ok(texts)
    }

    const fn fold(&self) -> &Folded<F> {
        self.folded
            .as_ref()
            .expect("the world holds a fold between steps")
    }

    fn durable(&self, folded: &Folded<F>) -> Result<Durable, String> {
        Ok(Durable {
            state: serde_json::to_string(folded.state()).map_err(|error| error.to_string())?,
            request: folded.request().ok(),
            seq: folded.seq(),
            oracle: self.oracle.clone(),
            round: self.round.clone(),
            queued: self.queued.clone(),
        })
    }

    fn check_oracle(&self, durable: &Durable) -> Result<(), String> {
        let Some(request) = &durable.request else {
            return Ok(());
        };
        let messages = F::messages(request)?;
        if messages.len() != durable.oracle.len() {
            return Err(format!(
                "the {} request at fact {} holds {} messages where the session holds {}",
                F::NAME,
                durable.seq,
                messages.len(),
                durable.oracle.len()
            ));
        }
        for (index, (message, markers)) in messages.iter().zip(&durable.oracle).enumerate() {
            if let Some(marker) = markers
                .iter()
                .find(|marker| !message.contains(marker.as_str()))
            {
                return Err(format!(
                    "the {} request at fact {} lacks {marker} in message {index}: {message}",
                    F::NAME,
                    durable.seq
                ));
            }
        }
        Ok(())
    }

    fn commit(
        &mut self,
        fact: OwnedFact<F::Decision>,
        point: Point,
    ) -> Result<Option<u64>, String> {
        let mut lease = ready(self.log.lease());
        let folded = self
            .folded
            .take()
            .expect("the world holds a fold between steps");
        let (folded, seq) = folded
            .commit(&mut lease, fact)
            .map_err(failed("commit a fact"))?;
        let facts = crate::facts::logged(&mut lease)?;
        let blobs = crate::facts::logged_blobs(&mut lease, &facts)?;
        if let Some(divergence) = folded
            .diverges(facts, |digest| crate::facts::blob(&blobs, digest))
            .map_err(failed("refold the log"))?
        {
            return Err(format!("{divergence} after a commit"));
        }
        if self.rng.chance(UNCOMMITTED_PPM) && self.rng.chance(CRASH_PPM) {
            drop(lease);
            self.folded = Some(folded);
            self.reopen()?;
            return Ok(None);
        }
        ready(lease.commit(point)).map_err(|error| format!("commit the log: {error}"))?;
        drop(lease);
        let durable = self.durable(&folded)?;
        self.check_oracle(&durable)?;
        if let Some(request) = &durable.request {
            self.digest = fnv(self.digest, request);
        }
        self.durable = durable;
        self.folded = Some(folded);
        Ok(Some(seq))
    }

    fn set_round(&mut self, round: Option<Round>) {
        self.durable.round.clone_from(&round);
        self.round = round;
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.folded = None;
        self.log = open(&self.host)?;
        let mut lease = ready(self.log.lease());
        if self.legacy.is_some() {
            ready(lease.load_owner(LEGACY_STATE))
                .map_err(|error| format!("load the legacy owner: {error}"))?;
        }
        let facts = crate::facts::logged(&mut lease)?;
        let blobs = crate::facts::logged_blobs(&mut lease, &facts)?;
        let fold = lease.fold();
        let cached = fold
            .cached()
            .map_err(|error| format!("read the cached fold: {error}"))?;
        let (folded, _) = Folded::<F>::open(cached, facts.clone(), |digest| {
            crate::facts::blob(&blobs, digest)
        })
        .map_err(failed("reopen the fold"))?;
        self.oracle = self.durable.oracle.clone();
        self.round = self.durable.round.clone();
        self.queued = self.durable.queued.clone();
        let reopened = self.durable(&folded)?;
        if reopened != self.durable {
            return Err(format!(
                "after a restart at fact {} the {} fold differs from the live state at fact {}",
                folded.seq(),
                F::NAME,
                self.durable.seq
            ));
        }
        self.check_dispatched(&facts)?;
        let checkpoint = fold.checkpoint(LEGACY_STATE).cloned();
        let based = facts.iter().any(|(_, fact)| matches!(fact, Fact::Base(_)));
        drop(lease);
        self.folded = Some(folded);
        if let (Some(texts), false) = (self.legacy.clone(), based) {
            self.import(checkpoint, &texts)?;
        }
        if self.durable.request.is_none()
            && self
                .commit(
                    Fact::Decision(Decision::Backend(F::template()?)),
                    Point::Progress,
                )?
                .is_none()
        {
            return Ok(());
        }
        self.settle_unknown()
    }

    fn check_dispatched(&self, facts: &[(u64, StoredFact)]) -> Result<(), String> {
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
        Ok(())
    }

    fn import(
        &mut self,
        checkpoint: Option<nanocode_turnlog::Checkpoint>,
        texts: &[String],
    ) -> Result<(), String> {
        if self.fold().seq() != 0 {
            return Err(format!("facts precede the {} legacy base", F::NAME));
        }
        let checkpoint = checkpoint.ok_or("the legacy checkpoint is gone before its import")?;
        let base = Fact::Base(Base {
            state: LEGACY_STATE.to_owned(),
            revision: checkpoint.revision,
            checkpoint: RawValue::from_string(checkpoint.payload)
                .map_err(|error| error.to_string())?,
        });
        self.oracle = texts.iter().map(|text| vec![text.clone()]).collect();
        self.commit(base, Point::Progress).map(drop)
    }

    fn needs_render(&self, call: &CallState) -> Result<bool, String> {
        Ok(call.media && F::rendered(&call.id)?.is_some())
    }

    fn intend(&mut self, index: usize) -> Result<bool, String> {
        let mut round = self.round.clone().expect("a call belongs to an open round");
        round.calls[index].intended = true;
        let call = round.calls[index].id.clone();
        let block = u32::try_from(index + 1).map_err(|error| error.to_string())?;
        let intent = Fact::Intent(Intent {
            turn: Some(round.turn.clone()),
            call_id: call.clone(),
            tool: "exec".to_owned(),
            target: IntentTarget::ToolUse(BlockRef {
                model_out: round.model_out,
                block,
            }),
        });
        self.round = Some(round);
        if !mutated(BackendFactsMutation::ResultWithoutIntent)
            && self.commit(intent, Point::Call)?.is_none()
        {
            return Ok(false);
        }
        *self.dispatched.entry(call).or_default() += 1;
        if self.rng.chance(CRASH_PPM) {
            self.reopen()?;
            return Ok(false);
        }
        Ok(true)
    }

    fn result(&mut self, index: usize, output: ToolOutput) -> Result<bool, String> {
        let mut round = self
            .round
            .clone()
            .expect("a result belongs to an open round");
        round.calls[index].resulted = true;
        let call = round.calls[index].clone();
        let ids = round
            .calls
            .iter()
            .map(|call| call.id.clone())
            .collect::<Vec<_>>();
        let needs = self.needs_render(&call)? && output.success;
        if !needs {
            F::expect_result(&ids, index, &mut self.oracle);
            round.calls[index].rendered = true;
        }
        self.round = Some(round);
        let wire = output.into_wire().map_err(|error| error.to_string())?;
        let result = Fact::EffectResult(EffectResult {
            call_id: call.id,
            output: raw(&wire)?,
        });
        Ok(self.commit(result, Point::Progress)?.is_some())
    }

    fn render(&mut self, index: usize) -> Result<bool, String> {
        let mut round = self
            .round
            .clone()
            .expect("a render belongs to an open round");
        round.calls[index].rendered = true;
        let ids = round
            .calls
            .iter()
            .map(|call| call.id.clone())
            .collect::<Vec<_>>();
        let decision =
            F::rendered(&ids[index])?.expect("only a family that renders media is asked to");
        F::expect_result(&ids, index, &mut self.oracle);
        self.round = Some(round);
        if mutated(BackendFactsMutation::MediaUnrendered) {
            return Ok(true);
        }
        let fact = Fact::Decision(Decision::Backend(decision));
        Ok(self.commit(fact, Point::Progress)?.is_some())
    }

    fn output(&self, call: &CallState) -> Result<ToolOutput, String> {
        if !call.media {
            return Ok(ToolOutput::text(format!("{} done", call.id)));
        }
        let image: ToolOutputContent = serde_json::from_value(
            json!({"type": "input_image", "image_url": MEDIA_URL, "detail": "auto"}),
        )
        .map_err(|error| error.to_string())?;
        Ok(ToolOutput::content(vec![image]))
    }

    fn finish_round(&mut self) -> Result<(), String> {
        let calls = self.round.as_ref().map_or(0, |round| round.calls.len());
        for index in 0..calls {
            let Some(call) = self.round.as_ref().map(|round| round.calls[index].clone()) else {
                return Ok(());
            };
            if !call.intended && !self.intend(index)? {
                return Ok(());
            }
            if !call.resulted {
                let output = self.output(&call)?;
                if !self.result(index, output)? {
                    return Ok(());
                }
            }
            let call = self.round.as_ref().map(|round| round.calls[index].clone());
            if call.is_some_and(|call| !call.rendered) && !self.render(index)? {
                return Ok(());
            }
        }
        self.set_round(None);
        Ok(())
    }

    fn settle_unknown(&mut self) -> Result<(), String> {
        let calls = self.round.as_ref().map_or(0, |round| round.calls.len());
        for index in 0..calls {
            let Some(call) = self.round.as_ref().map(|round| round.calls[index].clone()) else {
                return Ok(());
            };
            if call.intended
                && !call.resulted
                && !self.result(index, ToolOutput::error(UNKNOWN_OUTCOME))?
            {
                return Ok(());
            }
        }
        Ok(())
    }

    fn words(&mut self, label: &str) -> String {
        let mut text = format!("{label}-{:x}", self.rng.draw());
        for _ in 0..self.rng.below(WORDS_MAX) {
            text.push_str(&format!(" {:x}", self.rng.draw()));
        }
        if self.rng.chance(BLOB_PPM) {
            text.push_str(&"x".repeat(VALUE_INLINE_BYTES));
        }
        text
    }

    fn prompt(&mut self) -> Result<(), String> {
        self.turns += 1;
        let text = self.words("prompt");
        if F::admitted().is_some() {
            self.queued.push(text.clone());
        } else {
            self.oracle.push(vec![text.clone()]);
        }
        let fact = Fact::Input(Input {
            turn: format!("turn-{}", self.turns),
            kind: InputKind::Prompt,
            value: raw(&nanocode::backend::Prompt::new(text))?,
            metadata: raw(&json!({}))?,
            hand: None,
            first: None,
        });
        if self.commit(fact, Point::Admission)?.is_none() {
            return Ok(());
        }
        self.admit()
    }

    fn admit(&mut self) -> Result<(), String> {
        while let Some(decision) = F::admitted()
            && !self.queued.is_empty()
        {
            let text = self.queued.remove(0);
            self.oracle.push(vec![text]);
            if self
                .commit(Fact::Decision(Decision::Backend(decision)), Point::Progress)?
                .is_none()
            {
                return Ok(());
            }
        }
        Ok(())
    }

    fn model(&mut self) -> Result<(), String> {
        let text = self.words("answer");
        let calls = (0..self.rng.below(CALLS_MAX + 1))
            .map(|_| {
                self.calls += 1;
                format!("call-{}", self.calls)
            })
            .collect::<Vec<_>>();
        let media = calls
            .iter()
            .map(|_| self.rng.chance(MEDIA_PPM))
            .collect::<Vec<_>>();
        let turn = format!("turn-{}", self.turns);
        F::expect_response(&text, &calls, &mut self.oracle);
        let response = raw(&F::response(self.calls, &text, &calls))?;
        let Some(model_out) = self.commit(
            Fact::ModelOut(ModelOut {
                turn: turn.clone(),
                response,
            }),
            Point::Progress,
        )?
        else {
            return Ok(());
        };
        if calls.is_empty() {
            return Ok(());
        }
        let calls = calls
            .into_iter()
            .zip(media)
            .map(|(id, media)| CallState {
                id,
                media,
                ..CallState::default()
            })
            .collect();
        self.set_round(Some(Round {
            turn,
            model_out,
            calls,
        }));
        self.finish_round()
    }

    fn notice(&mut self) -> Result<(), String> {
        let text = self.words("notice");
        self.oracle.push(vec![text.clone()]);
        if mutated(BackendFactsMutation::UndecidedNotice) {
            return Ok(());
        }
        let decision = F::notice(&text)?;
        self.commit(Fact::Decision(Decision::Backend(decision)), Point::Progress)
            .map(drop)
    }

    fn cache(&mut self) -> Result<(), String> {
        let snapshot = self
            .fold()
            .snapshot()
            .map_err(failed("snapshot the fold"))?;
        let mut lease = ready(self.log.lease());
        lease
            .cache(&snapshot)
            .map_err(|error| format!("cache the fold: {error}"))?;
        ready(lease.commit(Point::Progress))
            .map(drop)
            .map_err(|error| format!("commit a cache: {error}"))
    }

    fn step(&mut self) -> Result<(), String> {
        if self.round.is_some() {
            return self.finish_round();
        }
        if !self.queued.is_empty() {
            return self.admit();
        }
        match self.rng.below(6) {
            0 | 1 => self.prompt(),
            2 | 3 => self.model(),
            4 => self.notice(),
            _ if self.rng.chance(CACHE_PPM) => self.cache(),
            _ => self.reopen(),
        }
    }

    fn run(seed: u64) -> Result<u64, String> {
        let mut world = Self::new(seed)?;
        let ops = world.rng.range(OPS_MIN, OPS_MAX);
        for _ in 0..ops {
            world.step()?;
        }
        world.reopen()?;
        Ok(world.digest)
    }
}

fn run_seed(seed: u64, mutation: BackendFactsMutation) -> Result<u64, String> {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
    let claude = mutation == BackendFactsMutation::MediaUnrendered
        || Rng::derive(seed, SCOPE).chance(CLAUDE_PPM);
    let outcome = std::panic::catch_unwind(|| {
        if claude {
            World::<ClaudeFold>::run(seed)
        } else {
            World::<CodexFold>::run(seed)
        }
    });
    MUTATION.store(BackendFactsMutation::None as u8, Ordering::SeqCst);
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

pub fn judge(seed: u64, mutation: BackendFactsMutation, twice: bool) -> Option<String> {
    let first = match run_seed(seed, mutation) {
        Ok(digest) => digest,
        Err(wrong) => return Some(wrong),
    };
    if !twice {
        return None;
    }
    match run_seed(seed, mutation) {
        Ok(second) if second == first => None,
        Ok(second) => Some(format!(
            "nondeterminism: digest {first:016x} then {second:016x}"
        )),
        Err(wrong) => Some(wrong),
    }
}

pub fn replay(seed: u64, mutation: BackendFactsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario backend_facts --seed {seed}");
    if mutation != BackendFactsMutation::None {
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
    mutation: BackendFactsMutation,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=backend_facts mutation={} passed={passed} failed={failed} twice={twice}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        BackendFactsMutation::None if failed == 0 => ExitCode::SUCCESS,
        BackendFactsMutation::None => ExitCode::FAILURE,
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

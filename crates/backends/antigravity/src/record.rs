use crate::driver::Inner;
use crate::facts::AntigravityFacts;
use crate::fold::AntigravityDecision;
use nanocode_agent_core::{NanocodeError, Result};
use nanocode_backend_antigravity_client::{Content, Part};
use nanocode_durability::facts::{
    Base, Decision, EffectResult, Fact, Intent, IntentTarget, ModelOut, OwnedFact,
};
use nanocode_tool_host::ToolOutput;
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};

fn policy(error: impl std::fmt::Display) -> NanocodeError {
    NanocodeError::InvalidExecutionPolicy(error.to_string())
}

fn raw(value: &impl Serialize) -> Result<Box<RawValue>> {
    serde_json::value::to_raw_value(value)
        .map_err(|error| policy(format!("an antigravity fact does not encode: {error}")))
}

const fn decided(decision: AntigravityDecision) -> OwnedFact<AntigravityDecision> {
    Fact::Decision(Decision::Backend(decision))
}

fn index(at: usize) -> Result<u32> {
    u32::try_from(at).map_err(|_| policy("antigravity contents outgrow u32 entries"))
}

fn decision(
    contents: &[Content],
    sent: &[Content],
    ready: bool,
) -> Result<Option<AntigravityDecision>> {
    let from = contents
        .iter()
        .zip(sent)
        .take_while(|(left, right)| left == right)
        .count();
    if from == contents.len() && ready {
        return Ok(
            (from < sent.len()).then(|| AntigravityDecision::Contents(sent[from..].to_vec()))
        );
    }
    let kept = contents[from..]
        .iter()
        .rev()
        .zip(sent[from..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    Ok(Some(AntigravityDecision::Splice {
        from: index(from)?,
        to: index(contents.len() - kept)?,
        contents: sent[from..sent.len() - kept].to_vec(),
    }))
}

pub(crate) fn call_ids(turn: &str, call_index: u64, parts: &[Part]) -> Vec<String> {
    parts
        .iter()
        .filter_map(|part| part.function_call.as_ref())
        .enumerate()
        .map(|(position, call)| {
            call.id
                .clone()
                .unwrap_or_else(|| format!("{turn}-{call_index}-{position}"))
        })
        .collect()
}

pub(crate) async fn import_base(
    facts: &AntigravityFacts,
    state: &str,
    payload: &Value,
) -> Result<()> {
    if facts.seq().await? != 0 {
        return Ok(());
    }
    let base = Fact::Base(Base {
        state: state.to_owned(),
        revision: 0,
        checkpoint: raw(payload)?,
    });
    facts.commit(base).await.map(drop)
}

fn answer(part: &Part) -> Option<(&'static str, String)> {
    let response = part.function_response.as_ref()?;
    let (key, body) = response.response.as_object()?.iter().next()?;
    let body = body
        .as_str()
        .map_or_else(|| body.to_string(), str::to_owned);
    Some((if key == "error" { "error" } else { "output" }, body))
}

impl Inner {
    pub(crate) async fn recorded(&self, turn: &str, call: u32) -> Result<bool> {
        let Some(facts) = &self.config.facts else {
            return Ok(false);
        };
        let turn = turn.to_owned();
        facts.read(move |fold| fold.answered(&turn, call)).await
    }

    pub(crate) async fn sync_facts(&self, contents: &[Content]) -> Result<()> {
        let Some(facts) = &self.config.facts else {
            return Ok(());
        };
        if crate::sim::mutated(crate::sim::Mutation::FactSyncSkipped) {
            return Ok(());
        }
        let sent = contents.to_vec();
        let decided_now = facts
            .read(move |fold| decision(fold.contents(), &sent, fold.ready()))
            .await??;
        match decided_now {
            Some(decision) => facts.commit(decided(decision)).await.map(drop),
            None => Ok(()),
        }
    }

    pub(crate) async fn fact_model(
        &self,
        turn: &str,
        call: u32,
        parts: &[Part],
        ids: &[String],
    ) -> Result<()> {
        let Some(facts) = &self.config.facts else {
            return Ok(());
        };
        let fact = Fact::ModelOut(ModelOut {
            turn: turn.to_owned(),
            response: raw(&json!({"call": call, "parts": parts, "ids": ids}))?,
        });
        facts.commit(fact).await.map(drop)
    }

    pub(crate) async fn intended(&self, call_id: &str) -> Result<Option<bool>> {
        let Some(facts) = &self.config.facts else {
            return Ok(None);
        };
        let id = call_id.to_owned();
        facts
            .read(move |fold| fold.open_call(&id).map(|call| call.intended))
            .await
    }

    pub(crate) async fn fact_intent(&self, turn: &str, call_id: &str, tool: &str) -> Result<()> {
        let Some(facts) = &self.config.facts else {
            return Ok(());
        };
        let id = call_id.to_owned();
        let open = facts
            .read(move |fold| {
                fold.open_call(&id)
                    .filter(|call| !call.intended)
                    .map(|call| call.block)
            })
            .await?;
        let Some(block) = open else {
            return Err(policy(
                "the antigravity fact log holds no open call for a tool about to run",
            ));
        };
        let fact = Fact::Intent(Intent {
            turn: Some(turn.to_owned()),
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            target: IntentTarget::ToolUse(block),
        });
        facts.commit(fact).await.map(drop)
    }

    pub(crate) async fn fact_result(&self, call_id: &str, key: &str, body: &str) -> Result<()> {
        let Some(facts) = &self.config.facts else {
            return Ok(());
        };
        let id = call_id.to_owned();
        let open = facts
            .read(move |fold| {
                fold.open_call(&id)
                    .is_some_and(|call| call.intended && !call.answered)
            })
            .await?;
        if !open {
            return Ok(());
        }
        let output = match key {
            "error" => ToolOutput::error(body),
            _ => ToolOutput::text(body),
        };
        let wire = output.into_wire().map_err(policy)?;
        let fact = Fact::EffectResult(EffectResult {
            call_id: call_id.to_owned(),
            output: raw(&wire)?,
        });
        facts.commit(fact).await.map(drop)
    }

    pub(crate) async fn settled_result(
        &self,
        call_id: &str,
    ) -> Result<Option<(&'static str, String)>> {
        let Some(facts) = &self.config.facts else {
            return Ok(None);
        };
        let id = call_id.to_owned();
        facts
            .read(move |fold| fold.settled(&id).and_then(answer))
            .await
    }

    pub(crate) async fn abandon_calls(&self, ids: &[String], answer: &str) -> Result<()> {
        let Some(facts) = &self.config.facts else {
            return Ok(());
        };
        for id in ids {
            let call = id.clone();
            let open = facts
                .read(move |fold| {
                    fold.open_call(&call)
                        .is_some_and(|call| call.intended && !call.answered)
                })
                .await?;
            if !open {
                continue;
            }
            let abandoned = self.config.nano.abandon(id).await.map_err(policy)?;
            if abandoned.is_some() || !crate::sim::mutated(crate::sim::Mutation::AbandonKeepsFold) {
                facts.refresh().await?;
            }
            self.fact_result(id, "error", answer).await?;
        }
        Ok(())
    }
}

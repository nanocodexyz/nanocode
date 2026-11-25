use super::*;
use crate::durability::{ClaudeDecision, ClaudeFacts};
use nanocode_durability::facts::{
    Decision, EffectResult, Fact, Intent, IntentTarget, ModelOut, OwnedFact,
};

fn raw(value: &impl Serialize) -> Result<Box<serde_json::value::RawValue>> {
    serde_json::value::to_raw_value(value).map_err(provider_error)
}

const fn decided(decision: ClaudeDecision) -> OwnedFact<ClaudeDecision> {
    Fact::Decision(Decision::Backend(decision))
}

fn tool_results(message: &Message) -> impl Iterator<Item = (&str, &ContentBlock)> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolResult { tool_use_id, .. } => Some((tool_use_id.as_str(), block)),
        _ => None,
    })
}

fn folded_template(template: &MessagesRequest) -> MessagesRequest {
    let mut template = template.clone();
    template.messages.clear();
    template.container = None;
    template.tool_choice = None;
    if template.diagnostics.is_some() {
        template.diagnostics = Some(Value::Object(serde_json::Map::new()));
    }
    template
}

fn common_prefix(left: &[Message], right: &[Message]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(left: &[Message], right: &[Message]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

impl State {
    pub(super) fn facts(&self) -> Option<&ClaudeFacts> {
        self.policy.as_deref()?.facts()
    }

    fn tool_result_tokens_bound(&self) -> u64 {
        self.tool_result_tokens
            .map_or(TOOL_RESULT_TOKEN_LIMIT, std::num::NonZeroU64::get)
    }

    pub(super) async fn sync_facts(
        &self,
        template: &MessagesRequest,
        carried: (Option<&str>, Option<&str>),
        messages: &[HistoryMessage],
    ) -> Result<()> {
        let Some(facts) = self.facts() else {
            return Ok(());
        };
        let tokens = self.tool_result_tokens_bound();
        let template = folded_template(template);
        let (container, previous_message_id) = carried;
        let diagnosed = template.diagnostics.is_some();
        let (stale, uncarried) = facts
            .read(|fold| {
                (
                    fold.template() != Some((&template, tokens)),
                    fold.container() != container
                        || (diagnosed && fold.previous_message_id() != previous_message_id),
                )
            })
            .await?;
        if stale {
            let decision = ClaudeDecision::Template {
                request: template,
                tool_result_tokens: tokens,
            };
            facts.commit(decided(decision)).await?;
        }
        if uncarried {
            let decision = ClaudeDecision::Carried {
                container: container.map(str::to_owned),
                previous_message_id: previous_message_id.map(str::to_owned),
            };
            facts.commit(decided(decision)).await?;
        }
        self.render_facts(facts, messages).await?;
        let wires = messages
            .iter()
            .map(HistoryMessage::wire)
            .collect::<Vec<_>>();
        let synced = facts.take_synced();
        let known = synced
            .iter()
            .zip(&wires)
            .take_while(|(synced, wire)| synced == wire)
            .count();
        let live = messages[known..]
            .iter()
            .map(|message| message.clone().into_message())
            .collect::<Vec<_>>();
        self.splice_facts(facts, known, &live).await?;
        facts.put_synced(wires);
        Ok(())
    }

    async fn render_facts(&self, facts: &ClaudeFacts, messages: &[HistoryMessage]) -> Result<()> {
        let (unrendered, at) = facts
            .read(|fold| (fold.unrendered(), fold.messages().len()))
            .await?;
        let Some(results) = messages
            .get(at)
            .map(|message| message.clone().into_message())
        else {
            return Ok(());
        };
        for id in unrendered {
            if let Some((_, block)) = tool_results(&results).find(|(result, _)| *result == id) {
                facts
                    .commit(decided(ClaudeDecision::Rendered(block.clone())))
                    .await?;
            }
        }
        Ok(())
    }

    async fn splice_facts(
        &self,
        facts: &ClaudeFacts,
        known: usize,
        live: &[Message],
    ) -> Result<()> {
        let (prefix, suffix, folded, open) = facts
            .read(|fold| {
                let folded = fold.messages().get(known..).unwrap_or_default();
                let prefix = common_prefix(folded, live);
                let suffix = common_suffix(&folded[prefix..], &live[prefix..]);
                (prefix, suffix, folded.len(), !fold.unrendered().is_empty())
            })
            .await?;
        if prefix == folded && !open {
            for message in &live[prefix..] {
                let queued = facts
                    .read(|fold| fold.queued().position(|queued| queued == message))
                    .await?;
                let decision = match queued.map(u32::try_from).transpose() {
                    Ok(Some(input)) => ClaudeDecision::Admitted { input },
                    Ok(None) => ClaudeDecision::Message(message.clone()),
                    Err(_) => return Err(provider_error("queued inputs outgrow u32")),
                };
                facts.commit(decided(decision)).await?;
            }
            return Ok(());
        }
        let index = |offset: usize| {
            u32::try_from(known + offset).map_err(|_| provider_error("a conversation outgrows u32"))
        };
        let decision = ClaudeDecision::Splice {
            from: index(prefix)?,
            to: index(folded - suffix)?,
            messages: live[prefix..live.len() - suffix].to_vec(),
        };
        facts.commit(decided(decision)).await.map(drop)
    }

    pub(super) async fn fact_model(
        &self,
        turn: &str,
        response: &crate::MessageResponse,
    ) -> Result<()> {
        let Some(facts) = self.facts() else {
            return Ok(());
        };
        if facts
            .read(|fold| fold.previous_message_id() == Some(response.id.as_str()))
            .await?
        {
            return Ok(());
        }
        let fact = Fact::ModelOut(ModelOut {
            turn: turn.to_owned(),
            response: raw(response)?,
        });
        facts.commit(fact).await.map(drop)
    }

    pub(super) async fn fact_compaction(
        &self,
        turn: &str,
        retained: usize,
        response: &crate::MessageResponse,
    ) -> Result<()> {
        let Some(facts) = self.facts() else {
            return Ok(());
        };
        let retained =
            u32::try_from(retained).map_err(|_| provider_error("a compaction outgrows u32"))?;
        facts
            .commit(decided(ClaudeDecision::Compacting { retained }))
            .await?;
        let fact = Fact::ModelOut(ModelOut {
            turn: turn.to_owned(),
            response: raw(response)?,
        });
        facts.commit(fact).await.map(drop)
    }

    pub(super) async fn abandoned(&self, id: &str) -> Result<Option<ContentBlock>> {
        let Some(facts) = self.facts() else {
            return Ok(None);
        };
        let open = facts
            .read(|fold| {
                fold.open_call(id)
                    .is_some_and(|call| call.intended && !call.output)
            })
            .await?;
        if !open {
            return facts.read(|fold| fold.result(id).cloned()).await;
        }
        let settled = match &self.abandon {
            Some(abandon) => abandon(id.to_owned())
                .await
                .map_err(durable::recovery_error)?
                .is_some(),
            None => false,
        };
        facts.refresh().await?;
        let result = facts.read(|fold| fold.result(id).cloned()).await?;
        if settled && result.is_none() {
            return Err(durable::recovery_error(
                "a Claude call the runtime abandoned has no result in its fact log",
            ));
        }
        Ok(result)
    }

    pub(super) async fn settled_fact(&self, id: &str) -> Result<Option<ContentBlock>> {
        let Some(facts) = self.facts() else {
            return Ok(None);
        };
        facts.read(|fold| fold.result(id).cloned()).await
    }

    pub(super) async fn fact_intent(&self, turn: Option<&str>, id: &str) -> Result<()> {
        let Some(facts) = self.facts() else {
            return Ok(());
        };
        let open = facts
            .read(|fold| {
                fold.open_call(id)
                    .filter(|call| !call.intended)
                    .map(|call| (call.block, call.name.to_owned()))
            })
            .await?;
        let Some((block, tool)) = open else {
            return Err(durable::recovery_error(
                "the Claude fact log holds no open call for a tool about to run",
            ));
        };
        let fact = Fact::Intent(Intent {
            turn: turn.map(str::to_owned),
            call_id: id.to_owned(),
            tool,
            target: IntentTarget::ToolUse(block),
        });
        facts.commit(fact).await.map(drop)
    }

    pub(super) async fn fact_result(
        &self,
        id: &str,
        content: &ToolResultContent,
        is_error: bool,
        metadata: Option<&Value>,
    ) -> Result<()> {
        let Some(facts) = self.facts() else {
            return Ok(());
        };
        let open = facts
            .read(|fold| {
                fold.open_call(id)
                    .is_some_and(|call| call.intended && !call.output)
            })
            .await?;
        if !open {
            return Ok(());
        }
        let output =
            crate::durability::tool_output(content, is_error, metadata).map_err(provider_error)?;
        let fact = Fact::EffectResult(EffectResult {
            call_id: id.to_owned(),
            output: raw(&output)?,
        });
        facts.commit(fact).await.map(drop)
    }
}

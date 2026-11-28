use crate::harness::{FAMILY, KimiHarness};
use nanocode_agent_core::{
    HarnessFamily, SessionCheckpoint, Thinking,
    session::{ToolOutcome, TranscriptItem},
};
use nanocode_backend_kimi_client::{Message, Origin};
use nanocode_durability::{AdmittedTurn, CheckpointFormat, Error, Result, SessionRecord};
use serde_json::Value;

pub const FORMAT: &str = "kimi.v1";

pub struct KimiCheckpointFormat;

impl CheckpointFormat for KimiCheckpointFormat {
    fn family(&self) -> HarnessFamily {
        HarnessFamily::new(FAMILY)
    }

    fn owns(&self, checkpoint: &Value) -> bool {
        checkpoint.get("format").and_then(Value::as_str) == Some(FORMAT)
    }

    fn session_checkpoint(
        &self,
        record: &SessionRecord,
        checkpoint: Value,
    ) -> Result<SessionCheckpoint> {
        let model = checkpoint
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map_or_else(|| record.model.clone(), KimiHarness::selection);
        let thinking = checkpoint_thinking(&checkpoint)?;
        Ok(SessionCheckpoint::native(
            record.session_id.clone(),
            model,
            thinking,
            record.lineage.clone(),
            record.session_id.clone(),
            None,
            true,
            checkpoint,
        ))
    }

    fn thinking(&self, _record: &SessionRecord, checkpoint: &Value) -> Result<Thinking> {
        checkpoint_thinking(checkpoint)
    }

    fn transcript(&self, checkpoint: &Value, _turns: &[AdmittedTurn<'_>]) -> Vec<TranscriptItem> {
        let messages = checkpoint
            .get("messages")
            .cloned()
            .and_then(|messages| serde_json::from_value::<Vec<Message>>(messages).ok())
            .unwrap_or_default();
        let mut items = Vec::new();
        for message in messages {
            match message {
                Message::User {
                    content,
                    origin: Origin::Prompt,
                } => items.push(TranscriptItem::User(content)),
                Message::User { .. } => {}
                Message::Assistant {
                    content,
                    reasoning,
                    tool_calls,
                    ..
                } => {
                    if !reasoning.is_empty() {
                        items.push(TranscriptItem::Reasoning(reasoning));
                    }
                    if !content.is_empty() {
                        items.push(TranscriptItem::Assistant(content));
                    }
                    items.extend(tool_calls.into_iter().map(|call| TranscriptItem::Tool {
                        call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        parent_call_id: None,
                    }));
                }
                Message::Tool {
                    tool_call_id,
                    content,
                    failed,
                    ..
                } => items.push(TranscriptItem::ToolResult {
                    call_id: tool_call_id,
                    output: content,
                    outcome: match failed {
                        true => ToolOutcome::Failed,
                        false => ToolOutcome::Completed,
                    },
                }),
            }
        }
        items
    }
}

fn checkpoint_thinking(checkpoint: &Value) -> Result<Thinking> {
    checkpoint
        .get("thinking")
        .cloned()
        .map_or(Ok(Thinking::default()), serde_json::from_value)
        .map_err(|error| Error::InvalidState(format!("kimi checkpoint thinking: {error}")))
}

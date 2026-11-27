use super::model_max_tokens;
use super::{ContentBlock, Message, MessagesRequest, Result, Role, State, StopReason};
use crate::{OutputConfig, collect_stream};
use nanocode_sync::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;

const TITLE_MODEL: &str = "claude-haiku-4-5-20251001";
const TITLE_MIN_CHARS: usize = 10;
const TITLE_SYSTEM: &str = include_str!("../../prompts/session_title.md");
const TITLE_LANGUAGE: &str = "Write the title in the predominant language of the session \u{2014} a stray word or code token in another language doesn't change it, and neither does the English of these instructions.";

impl State {
    pub(super) async fn generate_title(&self, prompt: String) -> Result<Option<String>> {
        let prompt = prompt.trim();
        if prompt.chars().count() < TITLE_MIN_CHARS {
            return Ok(None);
        }
        let mut request = MessagesRequest {
            model: TITLE_MODEL.into(),
            max_tokens: model_max_tokens(TITLE_MODEL).expect("known model"),
            cache_control: None,
            output_config: Some(OutputConfig {
                effort: None,
                format: Some(json!({
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {"title": {"type": "string"}},
                        "required": ["title"],
                        "additionalProperties": false,
                    },
                })),
            }),
            effort_updates: false,
            speed: None,
            tool_choice: None,
            thinking: Some(json!({"type": "disabled"})),
            context_management: None,
            compaction: None,
            diagnostics: None,
            system: Some(Arc::new(TITLE_SYSTEM.into())),
            messages: vec![Message::text(
                Role::User,
                format!("<session>\n{prompt}\n</session>\n\n{TITLE_LANGUAGE}"),
            )],
            container: None,
            tools: Vec::new(),
        };
        self.client.prepare_request(&mut request);
        let failed = |stage: &str| super::provider_error(format!("session title {stage} failed"));
        let mut stream = self
            .client
            .stream(&request)
            .await
            .map_err(super::provider_error)?;
        let first = stream
            .next()
            .await
            .ok_or_else(|| failed("stream"))?
            .map_err(super::provider_error)?;
        let response = collect_stream(first, stream)
            .await
            .map_err(super::provider_error)?;
        super::spend_aside(&self.session_id, TITLE_MODEL, &response.usage);
        if response.stop_reason != Some(StopReason::EndTurn) {
            return Err(failed("response"));
        }
        let text: String = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        Ok(serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("title")?
                    .as_str()
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .filter(|title| !title.is_empty()))
    }
}

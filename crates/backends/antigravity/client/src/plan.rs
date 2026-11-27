mod auth;
mod storage;

pub use auth::{
    PLAN_CALLBACK_PATH, PLAN_CLIENT_ID_VAR, PLAN_CLIENT_SECRET_VAR, PLAN_LOGIN_COMMAND, PlanAuth,
    PlanClient, PlanCredentials, PlanLogin,
};
pub use storage::{PLAN_STATE_FILE, PlanStorage, PlanStore};

use crate::http::ClientError;
use crate::wire::{Content, FunctionCall, Part, Usage};
use nanocode_sys::base64::STANDARD;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};

pub(crate) const PLAN_PATH: &str = "/v1beta/interactions:createPerUserQuota";
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn invalid(detail: impl Into<String>) -> ClientError {
    ClientError::InvalidRequest(detail.into())
}

fn decode(detail: impl Into<String>) -> ClientError {
    ClientError::Decode(detail.into())
}

pub(crate) fn request(model: &str, body: &str) -> Result<String, ClientError> {
    let request: Map<String, Value> =
        serde_json::from_str(body).map_err(|error| invalid(error.to_string()))?;
    let mut out = Map::new();
    out.insert("model".to_owned(), Value::String(model.to_owned()));
    out.insert("store".to_owned(), Value::Bool(false));
    out.insert("stream".to_owned(), Value::Bool(true));
    for (key, value) in request {
        match key.as_str() {
            "contents" => {
                out.insert("input".to_owned(), Value::Array(steps(value)?));
            }
            "systemInstruction" => {
                out.insert("system_instruction".to_owned(), system(value)?);
            }
            "tools" => {
                let tools = tools(value)?;
                if !tools.is_empty() {
                    out.insert("tools".to_owned(), Value::Array(tools));
                }
            }
            "generationConfig" => {
                let config = generation(value)?;
                if !config.is_empty() {
                    out.insert("generation_config".to_owned(), Value::Object(config));
                }
            }
            "labels" => {}
            other => {
                return Err(invalid(format!(
                    "the Google AI plan route does not support the {other} request field"
                )));
            }
        }
    }
    Ok(Value::Object(out).to_string())
}

fn system(value: Value) -> Result<Value, ClientError> {
    let content: Content =
        serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
    let texts = content
        .parts
        .iter()
        .map(
            |part| match (&part.text, &part.function_call, &part.function_response) {
                (Some(text), None, None) => Ok(text.as_str()),
                _ => Err(invalid(
                    "the system instruction holds a part that is not text",
                )),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::String(texts.join("\n")))
}

fn tools(value: Value) -> Result<Vec<Value>, ClientError> {
    let Value::Array(entries) = value else {
        return Err(invalid("tools is not an array"));
    };
    let mut out = Vec::new();
    for entry in entries {
        let Value::Object(entry) = entry else {
            return Err(invalid("a tools entry is not an object"));
        };
        for (kind, value) in entry {
            if kind != "functionDeclarations" {
                return Err(invalid(format!(
                    "the Google AI plan route does not support the {kind} tool"
                )));
            }
            let Value::Array(declarations) = value else {
                return Err(invalid("functionDeclarations is not an array"));
            };
            for declaration in declarations {
                out.push(function(declaration)?);
            }
        }
    }
    Ok(out)
}

fn function(declaration: Value) -> Result<Value, ClientError> {
    let Value::Object(declaration) = declaration else {
        return Err(invalid("a function declaration is not an object"));
    };
    let mut out = Map::new();
    out.insert("type".to_owned(), Value::String("function".to_owned()));
    for (key, value) in declaration {
        match key.as_str() {
            "name" | "description" => {
                out.insert(key, value);
            }
            "parametersJsonSchema" => {
                out.insert("parameters".to_owned(), value);
            }
            "parameters" => {
                out.insert("parameters".to_owned(), json_schema(value));
            }
            other => {
                return Err(invalid(format!(
                    "the Google AI plan route does not support the function declaration field {other}"
                )));
            }
        }
    }
    Ok(Value::Object(out))
}

fn json_schema(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| match (key.as_str(), value) {
                    ("type", Value::String(kind)) => {
                        (key, Value::String(kind.to_ascii_lowercase()))
                    }
                    (_, value) => (key, json_schema(value)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(json_schema).collect()),
        value => value,
    }
}

fn generation(value: Value) -> Result<Map<String, Value>, ClientError> {
    let Value::Object(config) = value else {
        return Err(invalid("generationConfig is not an object"));
    };
    let mut out = Map::new();
    for (key, value) in config {
        match key.as_str() {
            "maxOutputTokens" => {
                out.insert("max_output_tokens".to_owned(), value);
            }
            "candidateCount" if value == json!(1) => {}
            "thinkingConfig" => {
                let Value::Object(thinking) = value else {
                    return Err(invalid("thinkingConfig is not an object"));
                };
                for (key, value) in thinking {
                    match (key.as_str(), value) {
                        ("thinkingLevel", Value::String(level)) => {
                            out.insert(
                                "thinking_level".to_owned(),
                                Value::String(level.to_ascii_lowercase()),
                            );
                        }
                        ("includeThoughts", Value::Bool(include)) => {
                            out.insert(
                                "thinking_summaries".to_owned(),
                                Value::String(if include { "auto" } else { "none" }.to_owned()),
                            );
                        }
                        (other, _) => {
                            return Err(invalid(format!(
                                "the Google AI plan route does not support thinkingConfig.{other}; its models take a thinkingLevel"
                            )));
                        }
                    }
                }
            }
            other => {
                return Err(invalid(format!(
                    "the Google AI plan route does not support generationConfig.{other}"
                )));
            }
        }
    }
    Ok(out)
}

fn signature(signature: &str) -> String {
    match STANDARD.decode(signature) {
        Ok(bytes) if bytes == SKIP_SIGNATURE.as_bytes() => SKIP_SIGNATURE.to_owned(),
        _ => signature.to_owned(),
    }
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn steps(value: Value) -> Result<Vec<Value>, ClientError> {
    let contents: Vec<Content> =
        serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
    let mut steps = Vec::new();
    let mut pending = VecDeque::new();
    for (index, content) in contents.iter().enumerate() {
        match content.role.as_str() {
            "user" => user(&content.parts, &mut pending, &mut steps)?,
            "model" => model(index, &content.parts, &mut pending, &mut steps)?,
            role => return Err(invalid(format!("contents has an unknown role {role:?}"))),
        }
    }
    Ok(steps)
}

fn user(
    parts: &[Part],
    pending: &mut VecDeque<(String, String)>,
    steps: &mut Vec<Value>,
) -> Result<(), ClientError> {
    let mut texts = Vec::new();
    let flush = |texts: &mut Vec<Value>, steps: &mut Vec<Value>| {
        if !texts.is_empty() {
            steps.push(json!({"type": "user_input", "content": std::mem::take(texts)}));
        }
    };
    for part in parts {
        match (&part.function_response, &part.text) {
            (Some(response), _) => {
                flush(&mut texts, steps);
                let position = pending.iter().position(|(id, name)| match &response.id {
                    Some(wanted) => id == wanted,
                    None => *name == response.name,
                });
                let (call_id, _) = position
                    .and_then(|position| pending.remove(position))
                    .ok_or_else(|| {
                        invalid(format!(
                            "the function response for {} answers no call",
                            response.name
                        ))
                    })?;
                steps.push(json!({
                    "type": "function_result",
                    "call_id": call_id,
                    "name": response.name,
                    "result": response.response,
                }));
            }
            (None, Some(value)) => texts.push(text(value)),
            (None, None) => {
                return Err(invalid(
                    "a user part is neither text nor a function response",
                ));
            }
        }
    }
    flush(&mut texts, steps);
    Ok(())
}

fn model(
    index: usize,
    parts: &[Part],
    pending: &mut VecDeque<(String, String)>,
    steps: &mut Vec<Value>,
) -> Result<(), ClientError> {
    let start = steps.len();
    let mut signed = false;
    for (position, part) in parts.iter().enumerate() {
        if part.is_thought() {
            let mut step = json!({"type": "thought"});
            if let Some(summary) = part.text.as_deref().filter(|summary| !summary.is_empty()) {
                step["summary"] = json!([text(summary)]);
            }
            if let Some(value) = &part.thought_signature {
                step["signature"] = Value::String(signature(value));
                signed = true;
            }
            steps.push(step);
            continue;
        }
        if let Some(value) = &part.thought_signature {
            let value = Value::String(signature(value));
            let ours = steps.len() > start;
            match steps.last_mut() {
                Some(last)
                    if ours && last["type"] == "thought" && last.get("signature").is_none() =>
                {
                    last["signature"] = value;
                }
                _ => steps.push(json!({"type": "thought", "signature": value})),
            }
            signed = true;
        }
        match (&part.function_call, &part.text) {
            (Some(call), _) => {
                if !signed {
                    steps.push(json!({"type": "thought", "signature": SKIP_SIGNATURE}));
                    signed = true;
                }
                let id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{index}_{position}"));
                pending.push_back((id.clone(), call.name.clone()));
                let arguments = match &call.args {
                    Value::Null => json!({}),
                    args => args.clone(),
                };
                steps.push(json!({
                    "type": "function_call",
                    "id": id,
                    "name": call.name,
                    "arguments": arguments,
                }));
            }
            (None, Some(value)) if value.is_empty() => {}
            (None, Some(value)) => match (steps.len() > start, steps.last_mut()) {
                (true, Some(last)) if last["type"] == "model_output" => {
                    if let Some(Value::Array(content)) = last.get_mut("content") {
                        content.push(text(value));
                    }
                }
                (_, _) => steps.push(json!({"type": "model_output", "content": [text(value)]})),
            },
            (None, None) => {
                return Err(invalid(
                    "a model part is neither text, a thought nor a function call",
                ));
            }
        }
    }
    Ok(())
}

enum Open {
    Thought {
        signature: String,
    },
    Output,
    Call {
        id: String,
        name: String,
        arguments: Value,
        streamed: String,
    },
}

#[derive(Default)]
pub(crate) struct Stream {
    open: HashMap<u64, Open>,
    status: Option<String>,
    usage: Usage,
}

fn thought(text: &str) -> Part {
    Part {
        text: Some(text.to_owned()),
        thought: Some(true),
        ..Part::default()
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn summary_texts(items: Option<&Value>) -> Vec<Part> {
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| str_of(item, "type") == Some("text"))
        .filter_map(|item| str_of(item, "text"))
        .filter(|text| !text.is_empty())
        .map(thought)
        .collect()
}

impl Stream {
    pub(crate) fn event(&mut self, data: &str) -> Result<Vec<Part>, ClientError> {
        let event: Value =
            serde_json::from_str(data).map_err(|error| decode(format!("{error}: {data}")))?;
        let index = || {
            event
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| decode(format!("an Interactions step event has no index: {data}")))
        };
        match str_of(&event, "event_type") {
            Some("step.start") => self.start(index()?, event.get("step").unwrap_or(&Value::Null)),
            Some("step.delta") => self.delta(index()?, event.get("delta").unwrap_or(&Value::Null)),
            Some("step.stop") => self.stop(index()?),
            Some("interaction.completed") => {
                self.completed(event.get("interaction").unwrap_or(&Value::Null))?;
                Ok(Vec::new())
            }
            Some("error") => Err(decode(format!(
                "the Interactions stream reported an error: {}",
                event
                    .get("error")
                    .map_or_else(String::new, Value::to_string)
            ))),
            Some(_) => Ok(Vec::new()),
            None => Err(decode(format!(
                "an Interactions event has no event_type: {data}"
            ))),
        }
    }

    fn start(&mut self, index: u64, step: &Value) -> Result<Vec<Part>, ClientError> {
        let (open, parts) = match str_of(step, "type") {
            Some("thought") => (
                Open::Thought {
                    signature: str_of(step, "signature").unwrap_or_default().to_owned(),
                },
                summary_texts(step.get("summary")),
            ),
            Some("model_output") => (
                Open::Output,
                step.get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| str_of(item, "text"))
                    .filter(|text| !text.is_empty())
                    .map(Part::text)
                    .collect(),
            ),
            Some("function_call") => (
                Open::Call {
                    id: str_of(step, "id")
                        .ok_or_else(|| decode("an Interactions function call has no id"))?
                        .to_owned(),
                    name: str_of(step, "name")
                        .ok_or_else(|| decode("an Interactions function call has no name"))?
                        .to_owned(),
                    arguments: step.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    streamed: String::new(),
                },
                Vec::new(),
            ),
            other => {
                return Err(decode(format!(
                    "the Interactions stream started a step nanocode did not request: {other:?}"
                )));
            }
        };
        self.open.insert(index, open);
        Ok(parts)
    }

    fn delta(&mut self, index: u64, delta: &Value) -> Result<Vec<Part>, ClientError> {
        let open = self.open.get_mut(&index).ok_or_else(|| {
            decode(format!(
                "an Interactions delta names step {index}, which is not open"
            ))
        })?;
        match (str_of(delta, "type"), open) {
            (Some("text"), Open::Output) => {
                Ok(str_of(delta, "text").map(Part::text).into_iter().collect())
            }
            (Some("text"), Open::Thought { .. }) => {
                Ok(str_of(delta, "text").map(thought).into_iter().collect())
            }
            (Some("thought_summary"), Open::Thought { .. }) => Ok(summary_texts(
                delta
                    .get("content")
                    .map(|content| json!([content]))
                    .as_ref(),
            )),
            (Some("thought_signature"), Open::Thought { signature }) => {
                signature.push_str(str_of(delta, "signature").unwrap_or_default());
                Ok(Vec::new())
            }
            (Some("arguments_delta"), Open::Call { streamed, .. }) => {
                streamed.push_str(str_of(delta, "arguments").unwrap_or_default());
                Ok(Vec::new())
            }
            (kind, _) => Err(decode(format!(
                "the Interactions stream sent a {kind:?} delta for step {index}, which cannot take it"
            ))),
        }
    }

    fn stop(&mut self, index: u64) -> Result<Vec<Part>, ClientError> {
        let open = self.open.remove(&index).ok_or_else(|| {
            decode(format!(
                "an Interactions stop names step {index}, which is not open"
            ))
        })?;
        Ok(match open {
            Open::Thought { signature } if !signature.is_empty() => vec![Part {
                text: Some(String::new()),
                thought: Some(true),
                thought_signature: Some(signature),
                ..Part::default()
            }],
            Open::Thought { .. } | Open::Output => Vec::new(),
            Open::Call {
                id,
                name,
                arguments,
                streamed,
            } => {
                let args = match streamed.is_empty() {
                    true => arguments,
                    false => serde_json::from_str(&streamed).map_err(|error| {
                        decode(format!(
                            "the arguments of the {name} call are not JSON: {error}"
                        ))
                    })?,
                };
                vec![Part {
                    function_call: Some(FunctionCall {
                        id: Some(id),
                        name,
                        args,
                    }),
                    ..Part::default()
                }]
            }
        })
    }

    fn completed(&mut self, interaction: &Value) -> Result<(), ClientError> {
        let status = str_of(interaction, "status")
            .ok_or_else(|| decode("interaction.completed has no status"))?;
        if status == "failed" {
            return Err(decode(format!(
                "the interaction failed: {}",
                interaction
                    .get("errors")
                    .map_or_else(String::new, Value::to_string)
            )));
        }
        let usage = interaction.get("usage").unwrap_or(&Value::Null);
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
        self.usage = Usage {
            prompt_token_count: count("total_input_tokens"),
            candidates_token_count: count("total_output_tokens"),
            cached_content_token_count: count("total_cached_tokens"),
            thoughts_token_count: count("total_thought_tokens"),
            total_token_count: count("total_tokens"),
        };
        self.status = Some(status.to_owned());
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<Option<(String, Usage)>, ClientError> {
        let Some(status) = &self.status else {
            return Ok(None);
        };
        let reason = match status.as_str() {
            "completed" | "requires_action" => "STOP",
            "incomplete" => "MAX_TOKENS",
            other => {
                return Err(decode(format!("the interaction ended with status {other}")));
            }
        };
        Ok(Some((reason.to_owned(), self.usage)))
    }
}

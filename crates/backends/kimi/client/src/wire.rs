use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    Prompt,
    Reminder,
    Summary,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let usage = value.as_object()?;
        let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            prompt_tokens: number(usage.get("prompt_tokens")),
            completion_tokens: number(usage.get("completion_tokens")),
            cached_tokens: number(
                usage
                    .get("cached_tokens")
                    .or_else(|| value.pointer("/prompt_tokens_details/cached_tokens")),
            ),
            reasoning_tokens: number(value.pointer("/completion_tokens_details/reasoning_tokens")),
        })
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
        origin: Origin,
    },
    Assistant {
        content: String,
        reasoning: String,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        failed: bool,
        #[serde(default)]
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
        /// The parts after `content` when the output carries media, as Kimi Code stores them: `text`,
        /// `image_url` (a data URL, or the saved file's path for a format the provider does not accept) and
        /// `video_url` (the saved file's path, empty when unsaved); `lower_media` turns them into what K3 is sent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

/// Kimi Code's per-part media estimate (`MEDIA_TOKEN_ESTIMATE`).
const MEDIA_TOKENS: u64 = 2000;
const VIDEO_UNSUPPORTED: &str = "[video omitted: the current model does not support video input; use a model with video input capability or convert the file to text]";
const IMAGE_UNAVAILABLE: &str = "[image omitted: the uploaded file is no longer available]";

fn text_part(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// A tool result's stored output parts: `content`, then `media`, with the repeated-call note appended to the
/// last text part (`appendReminder`).
fn output_parts(content: &str, note: &str, media: &[Value]) -> Vec<Value> {
    let mut parts: Vec<Value> = std::iter::once(text_part(stored(content)))
        .chain(media.iter().cloned())
        .collect();
    match parts.last_mut().and_then(|last| last.get_mut("text")) {
        Some(Value::String(text)) => text.push_str(note),
        _ if note.is_empty() => {}
        _ => parts.push(text_part(note)),
    }
    parts
}

/// `buildMediaPathTag`.
fn path_tag(kind: &str, path: &str) -> String {
    let path = path
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<{kind} path=\"{path}\"></{kind}>")
}

/// The key an inline image's upload is remembered by: its data URL, hashed (Kimi Code keys uploads by the
/// media-store id, a hash of the same bytes and type).
#[must_use]
pub fn media_key(url: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    url.hash(&mut hasher);
    hasher.finish()
}

/// The inline images in the messages' tool results, each with the file name Kimi Code uploads it under:
/// the name of the file ReadMediaFile read, from the result's `<image path="...">` part. Kimi Code stores
/// media by content and keeps the latest name, so every copy of the same image goes up under the name of
/// its last read.
#[must_use]
pub fn inline_images<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> Vec<(&'a str, String)> {
    let mut images: Vec<(&str, String)> = Vec::new();
    for message in messages {
        let Message::Tool { content, media, .. } = message else {
            continue;
        };
        let name = content
            .strip_prefix("<image path=\"")
            .and_then(|rest| rest.strip_suffix("\">"))
            .map(|path| path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned());
        for part in media {
            let url = part["image_url"]["url"].as_str().unwrap_or_default();
            if part["type"] == "image_url" && url.starts_with("data:") {
                let fallback = || format!("upload.{}", extension(url));
                images.push((url, name.clone().unwrap_or_else(fallback)));
            }
        }
    }
    let latest: BTreeMap<&str, String> = images.iter().cloned().collect();
    for (url, name) in &mut images {
        name.clone_from(&latest[url]);
    }
    images
}

/// `uploadFilename`'s extension for a data URL's type, `bin` when unknown.
fn extension(url: &str) -> &'static str {
    let mime = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split([';', ',']).next())
        .unwrap_or_default();
    [
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("gif", "image/gif"),
        ("webp", "image/webp"),
        ("bmp", "image/bmp"),
        ("heic", "image/heic"),
        ("heif", "image/heif"),
        ("tiff", "image/tiff"),
        ("svg", "image/svg+xml"),
    ]
    .into_iter()
    .find(|(_, known)| *known == mime)
    .map_or("bin", |(ext, _)| ext)
}

/// Kimi Code's `AgentMediaResolverService` for K3 (`image_in`, no `video_in`): an inline image goes as the
/// `ms://<id>` of its upload to the provider's `/files` endpoint, or as its data URL when that upload
/// failed; an image the provider does not accept becomes its path tag (`degradedImage`), and a video the
/// `unsupportedMedia` notice.
fn lower_media(part: &Value, uploads: &BTreeMap<u64, String>) -> Value {
    let url = |key: &str| part[key]["url"].as_str().unwrap_or_default();
    match part["type"].as_str() {
        Some("image_url") if url("image_url").starts_with("data:") => {
            match uploads.get(&media_key(url("image_url"))) {
                Some(id) => {
                    json!({"type": "image_url", "image_url": {"url": format!("ms://{id}"), "id": id}})
                }
                None => part.clone(),
            }
        }
        Some("image_url") if url("image_url").is_empty() => text_part(IMAGE_UNAVAILABLE),
        Some("image_url") => text_part(&path_tag("image", url("image_url"))),
        Some("video_url") if url("video_url").is_empty() => text_part(VIDEO_UNSUPPORTED),
        Some("video_url") => text_part(&format!(
            "{VIDEO_UNSUPPORTED}\n{}",
            path_tag("video", url("video_url"))
        )),
        _ => part.clone(),
    }
}

const TOOL_ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";
const TOOL_EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";
const TOOL_EMPTY_ERROR_STATUS: &str =
    "<system>ERROR: Tool execution failed. Tool output is empty.</system>";
const WALL_TIME_TOOLS: [&str; 7] = [
    "Agent",
    "AgentSwarm",
    "Bash",
    "FetchURL",
    "Glob",
    "Grep",
    "WebSearch",
];
pub const TOOL_OUTPUT_EMPTY: &str = "Tool output is empty.";

/// Kimi Code's `normalizeLegacyResult`: an empty tool output is stored as "Tool output is empty.".
const fn stored(content: &str) -> &str {
    if content.is_empty() {
        TOOL_OUTPUT_EMPTY
    } else {
        content
    }
}

#[must_use]
pub fn render_tool_output(output: &str, failed: bool) -> String {
    match failed {
        true if output.is_empty() => TOOL_EMPTY_ERROR_STATUS.to_owned(),
        true => format!("{TOOL_ERROR_STATUS}\n{output}"),
        false if output.trim().is_empty() || output.trim() == TOOL_OUTPUT_EMPTY => {
            TOOL_EMPTY_STATUS.to_owned()
        }
        false => output.to_owned(),
    }
}

impl Message {
    #[must_use]
    pub fn user(content: impl Into<String>, origin: Origin) -> Self {
        Self::User {
            content: content.into(),
            origin,
        }
    }

    /// The message as sent; `wall` is the `Wall time` header Kimi Code puts before a tool result's first
    /// text part when it projects history for compaction.
    fn wire(&self, wall: Option<&str>, uploads: &BTreeMap<u64, String>) -> Value {
        let timed = |text: String| match wall {
            Some(header) => format!("{header}\n{text}"),
            None => text,
        };
        match self {
            Self::User { content, .. } => json!({"role": "user", "content": content}),
            Self::Tool {
                tool_call_id,
                content,
                failed,
                note,
                notice,
                media,
                ..
            } => {
                let content = match (notice, media.is_empty()) {
                    (None, true) => json!(timed(render_tool_output(content, *failed) + note)),
                    // With a note or media, Kimi Code sends the stored parts as they are (the repeated-call
                    // note appended to the last text part), behind an error status part when the tool failed,
                    // then the note as its own part.
                    _ => {
                        let mut parts: Vec<Value> = failed
                            .then(|| text_part(TOOL_ERROR_STATUS))
                            .into_iter()
                            .chain(output_parts(content, note, media))
                            .collect();
                        parts.extend(notice.as_deref().map(text_part));
                        if let Some(Value::String(text)) = parts[0].get_mut("text") {
                            *text = timed(std::mem::take(text));
                        }
                        json!(
                            parts
                                .iter()
                                .map(|part| lower_media(part, uploads))
                                .collect::<Vec<_>>()
                        )
                    }
                };
                json!({"role": "tool", "tool_call_id": tool_call_id, "content": content})
            }
            Self::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut message = Map::new();
                message.insert("role".to_owned(), json!("assistant"));
                if tool_calls.is_empty() || !content.trim().is_empty() {
                    message.insert("content".to_owned(), json!(content));
                }
                if !tool_calls.is_empty() {
                    let calls = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {"name": call.name, "arguments": call.arguments},
                            })
                        })
                        .collect::<Vec<_>>();
                    message.insert("tool_calls".to_owned(), Value::Array(calls));
                }
                message.insert("reasoning_content".to_owned(), json!(reasoning));
                Value::Object(message)
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub extra: Option<&'a Message>,
    pub tools: &'a [Value],
    pub max_completion_tokens: u64,
    pub prompt_cache_key: &'a str,
    pub effort: &'a str,
    /// The `/files` ids of the inline images uploaded so far, by `media_key`.
    pub uploads: &'a BTreeMap<u64, String>,
}

impl Request<'_> {
    #[must_use]
    pub fn encode(&self) -> String {
        let mut messages = vec![json!({"role": "system", "content": self.system})];
        let names = self
            .messages
            .iter()
            .flat_map(|message| match message {
                Message::Assistant { tool_calls, .. } => tool_calls
                    .iter()
                    .map(|call| (call.id.as_str(), call.name.as_str()))
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let wall = |message: &Message| match message {
            Message::Tool {
                tool_call_id,
                duration_ms: Some(duration),
                ..
            } if self.extra.is_some()
                && names.get(tool_call_id.as_str()).is_some_and(|name| {
                    WALL_TIME_TOOLS.contains(name) || name.starts_with("mcp__")
                }) =>
            {
                Some(format!(
                    "Wall time: {:.3} seconds",
                    *duration as f64 / 1000.0
                ))
            }
            _ => None,
        };
        messages.extend(
            self.messages
                .iter()
                .chain(self.extra)
                .map(|message| message.wire(wall(message).as_deref(), self.uploads)),
        );
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        body.insert("messages".to_owned(), Value::Array(messages));
        if !self.tools.is_empty() {
            body.insert("tools".to_owned(), Value::Array(self.tools.to_vec()));
        }
        body.insert(
            "max_completion_tokens".to_owned(),
            json!(self.max_completion_tokens),
        );
        body.insert("prompt_cache_key".to_owned(), json!(self.prompt_cache_key));
        body.insert(
            "thinking".to_owned(),
            json!({"type": "enabled", "effort": self.effort, "keep": "all"}),
        );
        body.insert("stream".to_owned(), json!(true));
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
        Value::Object(body).to_string()
    }
}

#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    let (ascii, other) = text.chars().fold((0_u64, 0_u64), |(ascii, other), char| {
        if char.is_ascii() {
            (ascii + 1, other)
        } else {
            (ascii, other + 1)
        }
    });
    ascii.div_ceil(4) + other
}

#[must_use]
pub fn estimate_message(message: &Message) -> u64 {
    estimate(message, |arguments| {
        estimate_text(&Value::String(arguments.to_owned()).to_string())
    })
}

#[must_use]
pub fn estimate_request_message(message: &Message) -> u64 {
    estimate(message, estimate_text)
}

fn estimate(message: &Message, arguments: impl Fn(&str) -> u64) -> u64 {
    match message {
        Message::User { content, .. } => estimate_text("user") + estimate_text(content),
        Message::Tool {
            content,
            note,
            notice,
            media,
            ..
        } => {
            let output = output_parts(content, note, media)
                .iter()
                .map(|part| part["text"].as_str().map_or(MEDIA_TOKENS, estimate_text))
                .sum::<u64>();
            estimate_text("tool") + output + notice.as_deref().map_or(0, estimate_text)
        }
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
            ..
        } => {
            let calls = tool_calls
                .iter()
                .map(|call| estimate_text(&call.name) + arguments(&call.arguments))
                .sum::<u64>();
            estimate_text("assistant") + estimate_text(reasoning) + estimate_text(content) + calls
        }
    }
}

#[must_use]
pub fn sanitize_call_id(id: &str) -> String {
    let sanitized = id
        .chars()
        .map(|char| match char {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => char,
            _ => '_',
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "tool_call".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod upload_tests {
    use super::{Message, inline_images};
    use serde_json::json;

    fn read(path: &str, url: &str) -> Message {
        Message::Tool {
            tool_call_id: String::new(),
            content: format!("<image path=\"{path}\">"),
            failed: false,
            note: String::new(),
            notice: None,
            media: vec![json!({"type": "image_url", "image_url": {"url": url}})],
            duration_ms: None,
        }
    }

    #[test]
    fn copies_of_an_image_take_its_last_name() {
        let messages = [
            read("/w/a.png", "data:image/png;base64,AA"),
            read("/w/b.png", "data:image/png;base64,BB"),
            read("/w/c.dat", "data:image/png;base64,AA"),
        ];
        let names: Vec<_> = inline_images(&messages)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(names, ["c.dat", "b.png", "c.dat"]);
    }
}

use crate::sse::SseDecoder;
use crate::wire::{Reply, ToolCall, Usage, sanitize_call_id};
use nanocode_http::Method;
use nanocode_http::header::{HeaderMap, HeaderName, HeaderValue};
use nanocode_http::layer::{Request, Sender};
use nanocode_sync::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding/v1";

const TRANSIENT: [u16; 8] = [408, 409, 429, 500, 502, 503, 504, 529];
const QUOTA_CODES: [&str; 2] = ["exceeded_current_quota_error", "insufficient_quota"];

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: Option<String>,
    pub user_agent: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial: Duration,
    pub multiplier: u32,
    pub max_delay: Duration,
    pub jitter: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial: Duration::from_millis(500),
            multiplier: 2,
            max_delay: Duration::from_secs(32),
            jitter: 0.25,
        }
    }
}

impl RetryPolicy {
    fn delay(&self, attempt: u32) -> Duration {
        let base = self
            .initial
            .saturating_mul(self.multiplier.saturating_pow(attempt))
            .min(self.max_delay);
        let random = nanocode_platform::uuid_v4().as_bytes()[0];
        base.mul_f64(1.0 + self.jitter * f64::from(random) / 255.0)
    }
}

#[derive(Clone, Debug)]
pub enum ClientError {
    Status {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    Transport(String),
    Stream(String),
    Decode(String),
}

impl ClientError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Status { status, body, .. } => {
                TRANSIENT.contains(status) && !QUOTA_CODES.iter().any(|code| body.contains(code))
            }
            Self::Transport(_) | Self::Stream(_) => true,
            Self::Decode(_) => false,
        }
    }

    const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status { status, body, .. } => {
                write!(formatter, "Kimi answered HTTP {status}: {}", body.trim())
            }
            Self::Transport(error) => write!(formatter, "Kimi request failed: {error}"),
            Self::Stream(error) => write!(formatter, "Kimi stream failed: {error}"),
            Self::Decode(error) => write!(formatter, "Kimi answered something unreadable: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    sender: Arc<dyn Sender>,
    retry: RetryPolicy,
}

#[cfg(not(target_family = "wasm"))]
struct NativeSender(nanocode_http::Client);

#[cfg(not(target_family = "wasm"))]
impl Sender for NativeSender {
    fn send(
        &self,
        request: Request,
    ) -> nanocode_http::layer::BoxFuture<
        '_,
        Result<nanocode_http::layer::Response, nanocode_http::layer::Error>,
    > {
        Box::pin(async move {
            nanocode_http::ClientSender::new(&self.0)
                .send(request)
                .await
        })
    }
}

#[derive(Default)]
struct Fragment {
    id: String,
    name: String,
    arguments: String,
}

impl Client {
    pub fn new(endpoint: Endpoint, sender: Arc<dyn Sender>) -> Self {
        Self {
            endpoint,
            sender,
            retry: RetryPolicy::default(),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn native_sender() -> Result<Arc<dyn Sender>, ClientError> {
        nanocode_tls::install_default_rustls_crypto_provider();
        let http = nanocode_http::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(600))
            .build()
            .map_err(|error| ClientError::Transport(error.to_string()))?;
        Ok(Arc::new(NativeSender(http)))
    }

    #[must_use]
    pub const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn headers(&self) -> Result<HeaderMap, ClientError> {
        let mut headers = HeaderMap::new();
        let mut set = |name: &'static str, value: &str| -> Result<(), ClientError> {
            let value = HeaderValue::from_str(value)
                .map_err(|_| ClientError::Transport(format!("invalid {name} header")))?;
            headers.insert(HeaderName::from_static(name), value);
            Ok(())
        };
        set("content-type", "application/json")?;
        set("accept", "application/json")?;
        set("user-agent", &self.endpoint.user_agent)?;
        if let Some(key) = &self.endpoint.api_key {
            set("authorization", &format!("Bearer {}", key.trim()))?;
        }
        Ok(headers)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.endpoint.base_url.trim_end_matches('/'))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: &str,
    ) -> Result<nanocode_http::layer::Response, ClientError> {
        self.send_with(method, path, self.headers()?, body.as_bytes().to_vec())
            .await
    }

    async fn send_with(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<nanocode_http::layer::Response, ClientError> {
        let response = self
            .sender
            .send(Request {
                method,
                url: self.url(path),
                headers,
                body: body.into(),
            })
            .await
            .map_err(|error| ClientError::Transport(error.detail().to_owned()))?;
        if response.status.is_success() {
            return Ok(response);
        }
        let status = response.status.as_u16();
        let retry_after = retry_after(&response.headers);
        let body = collect(response.body).await.unwrap_or_default();
        Err(ClientError::Status {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
            retry_after,
        })
    }

    async fn retrying<T, F>(&self, mut attempt: impl FnMut() -> F) -> Result<T, ClientError>
    where
        F: std::future::Future<Output = Result<T, ClientError>>,
    {
        let mut tries = 0;
        loop {
            match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) if error.retryable() && tries + 1 < self.retry.max_attempts => {
                    let delay = error
                        .retry_after()
                        .unwrap_or_else(|| self.retry.delay(tries));
                    nanocode_platform::sleep(delay).await;
                    tries += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Kimi Code's `createKimiFiles().upload`: one multipart `POST {base}/files` of the file under `name`
    /// with its `purpose`, no retries; the file id once Kimi reports it ready.
    pub async fn upload(
        &self,
        bytes: &[u8],
        name: &str,
        mime: &str,
        purpose: &str,
    ) -> Result<String, ClientError> {
        let boundary = format!("----nanocode{}", nanocode_platform::uuid_v4().simple());
        let escape = |text: &str| {
            text.replace('"', "%22")
                .replace('\r', "%0D")
                .replace('\n', "%0A")
        };
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {mime}\r\n\r\n",
            escape(name)
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(
            format!(
                "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--{boundary}--\r\n"
            )
            .as_bytes(),
        );
        let mut headers = self.headers()?;
        let content_type =
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}"))
                .map_err(|_| ClientError::Transport("invalid content-type header".to_owned()))?;
        headers.insert(HeaderName::from_static("content-type"), content_type);
        let response = self.send_with(Method::POST, "files", headers, body).await?;
        let bytes = collect(response.body)
            .await
            .map_err(|error| ClientError::Transport(error.detail().to_owned()))?;
        let uploaded = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
            ClientError::Decode("Kimi file upload returned an invalid response payload".to_owned())
        })?;
        uploaded_file(&uploaded).map_err(ClientError::Decode)
    }

    pub async fn context_length(&self, model: &str) -> Result<Option<u64>, ClientError> {
        let models = self
            .retrying(|| async {
                let response = self.send(Method::GET, "models", "").await?;
                let bytes = collect(response.body)
                    .await
                    .map_err(|error| ClientError::Transport(error.detail().to_owned()))?;
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|error| ClientError::Decode(error.to_string()))
            })
            .await?;
        Ok(models
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
            .and_then(|entry| entry.get("context_length"))
            .and_then(Value::as_u64))
    }

    pub async fn stream(
        &self,
        body: &str,
        on_delta: &mut (dyn FnMut(Delta<'_>) + Send),
    ) -> Result<Reply, ClientError> {
        let mut tries = 0;
        loop {
            match self.stream_once(body, on_delta).await {
                Ok(reply) => return Ok(reply),
                Err(error) if error.retryable() && tries + 1 < self.retry.max_attempts => {
                    let delay = error
                        .retry_after()
                        .unwrap_or_else(|| self.retry.delay(tries));
                    nanocode_platform::sleep(delay).await;
                    tries += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn stream_once(
        &self,
        body: &str,
        on_delta: &mut (dyn FnMut(Delta<'_>) + Send),
    ) -> Result<Reply, ClientError> {
        let response = self.send(Method::POST, "chat/completions", body).await?;
        let mut body = response.body;
        let mut decoder = SseDecoder::default();
        let mut reply = Reply::default();
        let mut fragments: BTreeMap<u64, Fragment> = BTreeMap::new();
        loop {
            let (events, done) = match body.next().await {
                Some(Ok(bytes)) => (decoder.push(&bytes), false),
                Some(Err(error)) => return Err(ClientError::Stream(error.detail().to_owned())),
                None => (decoder.finish(), true),
            };
            for event in events {
                let event = event.trim();
                if event.is_empty() || event == "[DONE]" {
                    continue;
                }
                let chunk: Value = serde_json::from_str(event)
                    .map_err(|error| ClientError::Decode(format!("{error}: {event}")))?;
                absorb(&chunk, &mut reply, &mut fragments, on_delta)?;
            }
            if done {
                break;
            }
        }
        if reply.finish_reason.is_none() {
            return Err(ClientError::Stream(
                "the stream ended before a finish_reason".to_owned(),
            ));
        }
        reply.tool_calls = fragments
            .into_values()
            .map(|fragment| ToolCall {
                id: sanitize_call_id(&fragment.id),
                name: fragment.name,
                arguments: fragment.arguments,
            })
            .collect();
        Ok(reply)
    }
}

fn absorb(
    chunk: &Value,
    reply: &mut Reply,
    fragments: &mut BTreeMap<u64, Fragment>,
    on_delta: &mut (dyn FnMut(Delta<'_>) + Send),
) -> Result<(), ClientError> {
    if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
        return Err(ClientError::Stream(error.to_string()));
    }
    let choice = chunk.pointer("/choices/0");
    if let Some(usage) = chunk
        .get("usage")
        .filter(|usage| !usage.is_null())
        .or_else(|| choice.and_then(|choice| choice.get("usage")))
        .and_then(Usage::parse)
    {
        reply.usage = Some(usage);
    }
    let Some(choice) = choice else {
        return Ok(());
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str)
        && !text.is_empty()
    {
        reply.reasoning.push_str(text);
        on_delta(Delta::Reasoning(text));
    }
    if let Some(text) = delta.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        reply.content.push_str(text);
        on_delta(Delta::Text(text));
    }
    for call in delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let fragment = fragments.entry(index).or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            fragment.id.push_str(id);
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            fragment.name.push_str(name);
        }
        if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
            fragment.arguments.push_str(arguments);
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        reply.finish_reason = Some(reason.to_owned());
    }
    Ok(())
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let read = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.0)
    };
    read("retry-after-ms")
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .or_else(|| read("retry-after").map(Duration::from_secs_f64))
}

async fn collect(
    mut body: nanocode_http::layer::Body,
) -> Result<Vec<u8>, nanocode_http::layer::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    Ok(bytes)
}

/// Kimi Code's `parseUploadedKimiFile`: the id of a file whose extraction and status say it is ready.
fn uploaded_file(uploaded: &Value) -> Result<String, String> {
    let Value::Object(record) = uploaded else {
        return Err("Kimi file upload returned an invalid response payload".to_owned());
    };
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or("Kimi file upload returned an empty file id")?;
    let not_ready = |field: &str, value: &Value| {
        let shown = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        match value.as_str() {
            Some("error") => format!("Kimi file upload failed for file \"{id}\" ({field}={shown})"),
            _ => format!("Kimi file upload for file \"{id}\" is not ready ({field}={shown})"),
        }
    };
    let extract = record
        .get("extract_status")
        .filter(|value| !value.is_null());
    if let Some(value) = extract.filter(|value| value.as_str() != Some("ready")) {
        return Err(not_ready("extract_status", value));
    }
    if let Some(status) = record.get("status").filter(|value| !value.is_null()) {
        let ready = matches!(status.as_str(), Some("ready" | "ok" | "processed"))
            || (extract.is_some() && status.as_str() == Some("uploaded"));
        if !ready {
            return Err(not_ready("status", status));
        }
    }
    Ok(id.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn uploads_are_ready_files() {
        let parse = |value| super::uploaded_file(&value);
        assert_eq!(parse(json!({"id": "f1", "status": "ok"})), Ok("f1".to_owned()));
        assert_eq!(
            parse(json!({"id": "f1", "extract_status": "ready", "status": "uploaded"})),
            Ok("f1".to_owned())
        );
        assert!(parse(json!({"id": " "})).is_err());
        assert!(parse(json!({"id": "f1", "status": "uploaded"})).is_err());
        assert_eq!(
            parse(json!({"id": "f1", "extract_status": "error"})),
            Err("Kimi file upload failed for file \"f1\" (extract_status=error)".to_owned())
        );
    }
}

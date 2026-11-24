use super::{TOKEN_URL, now};
use crate::http::{SUBSCRIPTION_BASE_URL, USER_AGENT_SUBSCRIPTION};
use nanocode_http::Method;
use nanocode_http::header::{HeaderMap, HeaderValue};
use nanocode_http::layer::{Request, Sender};
use nanocode_sync::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ONBOARD_ATTEMPTS: usize = 10;
const ONBOARD_INTERVAL: Duration = Duration::from_secs(5);
const HUB_VERSION: &str = "2.24.0";

#[derive(Deserialize)]
pub(super) struct Token {
    pub access_token: String,
    pub refresh_token: Option<String>,
    expires_in: u64,
    token_type: Option<String>,
}

impl Token {
    pub fn expiry(&self, started: u64) -> Result<u64, String> {
        if self.access_token.trim().is_empty()
            || self.expires_in == 0
            || self
                .token_type
                .as_deref()
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err("Google returned an invalid Antigravity access token".into());
        }
        started
            .checked_add(self.expires_in)
            .filter(|expiry| *expiry > now())
            .ok_or_else(|| "Google returned an invalid Antigravity token expiry".into())
    }
}

async fn call(sender: &dyn Sender, request: Request) -> Result<Value, String> {
    let operation = async {
        let response = sender.send(request).await
            .map_err(|_| "Antigravity authentication request failed; its outcome may be unknown, sign in again".to_owned())?;
        let status = response.status.as_u16();
        let mut stream = response.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.map_err(|_| {
                "Antigravity authentication response was interrupted; sign in again".to_owned()
            })?);
        }
        if !(200..300).contains(&status) {
            let revoked = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .is_some_and(|value| {
                    value.get("error").and_then(Value::as_str) == Some("invalid_grant")
                });
            return Err(if revoked {
                "Google revoked the Antigravity refresh token (invalid_grant); sign in again".into()
            } else {
                format!(
                    "Antigravity authentication returned HTTP {status}; verify account eligibility and sign in again"
                )
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| "Antigravity authentication returned invalid JSON".to_owned())
    };
    nanocode_platform::timeout(REQUEST_TIMEOUT, operation)
        .await
        .map_err(|_| {
            "Antigravity authentication request timed out; its outcome is unknown, sign in again"
                .to_owned()
        })?
}

pub(super) async fn token(sender: &dyn Sender, pairs: &[(&str, &str)]) -> Result<Token, String> {
    let mut body = nanocode_url::form::Serializer::new(String::new());
    body.extend_pairs(pairs.iter().copied());
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let value = call(
        sender,
        Request {
            method: Method::POST,
            url: TOKEN_URL.to_owned(),
            headers,
            body: body.finish().into_bytes().into(),
        },
    )
    .await?;
    serde_json::from_value(value)
        .map_err(|_| "Google returned an invalid Antigravity token response".into())
}

async fn api(sender: &dyn Sender, token: &str, action: &str, body: Value) -> Result<Value, String> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Google returned a token that cannot be used as a bearer credential")?;
    bearer.set_sensitive(true);
    headers.insert("authorization", bearer);
    let user_agent = if action == "onboardUser" {
        headers.insert(
            "x-goog-api-client",
            HeaderValue::from_static("gl-node/22.21.1"),
        );
        format!("{USER_AGENT_SUBSCRIPTION} google-api-nodejs-client/10.3.0")
    } else {
        USER_AGENT_SUBSCRIPTION.to_owned()
    };
    headers.insert(
        "user-agent",
        HeaderValue::from_str(&user_agent).map_err(|_| "invalid Hub User-Agent")?,
    );
    call(
        sender,
        Request {
            method: Method::POST,
            url: format!("{SUBSCRIPTION_BASE_URL}/v1internal:{action}"),
            headers,
            body: body.to_string().into_bytes().into(),
        },
    )
    .await
}

fn project(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) async fn discover_project(sender: &dyn Sender, token: &str) -> Result<String, String> {
    let loaded = api(
        sender,
        token,
        "loadCodeAssist",
        json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )
    .await?;
    if let Some(project) = loaded.get("cloudaicompanionProject").and_then(project) {
        return Ok(project);
    }
    let allowed = loaded.get("allowedTiers").and_then(Value::as_array);
    let tier = allowed
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| allowed.and_then(|tiers| tiers.first()))
        .and_then(|tier| tier.get("id"))
        .and_then(Value::as_str)
        .or_else(|| loaded.pointer("/currentTier/id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .unwrap_or("free-tier");
    for attempt in 0..ONBOARD_ATTEMPTS {
        if attempt != 0 {
            nanocode_platform::sleep(ONBOARD_INTERVAL).await;
        }
        let onboarded = api(sender, token, "onboardUser", json!({
            "tier_id":tier,
            "metadata":{"ide_type":"ANTIGRAVITY","ide_version":HUB_VERSION,"ide_name":"antigravity"}
        })).await?;
        if onboarded.get("done").and_then(Value::as_bool) == Some(true) {
            return onboarded.pointer("/response/cloudaicompanionProject").and_then(project)
                .ok_or_else(|| "Antigravity onboarding completed without a managed project; open Antigravity to finish account setup".into());
        }
    }
    Err("Antigravity onboarding is still pending; finish account setup in Antigravity and sign in again".into())
}

pub(super) async fn available_models(
    sender: &dyn Sender,
    token: &str,
    project: &str,
) -> Result<Value, String> {
    api(
        sender,
        token,
        "fetchAvailableModels",
        json!({"project":project}),
    )
    .await
}

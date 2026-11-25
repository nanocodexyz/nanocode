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
const ONBOARD_INTERVAL: Duration = Duration::from_secs(1);

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
    let mut pairs = pairs.to_vec();
    pairs.sort_by_key(|(key, _)| *key);
    let mut body = nanocode_url::form::Serializer::new(String::new());
    body.extend_pairs(pairs);
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    headers.insert("user-agent", HeaderValue::from_static("Go-http-client/1.1"));
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

async fn api_request(
    sender: &dyn Sender,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let mut headers = HeaderMap::new();
    if body.is_some() {
        headers.insert("content-type", HeaderValue::from_static("application/json"));
    }
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Google returned a token that cannot be used as a bearer credential")?;
    bearer.set_sensitive(true);
    headers.insert("authorization", bearer);
    headers.insert(
        "user-agent",
        HeaderValue::from_static(USER_AGENT_SUBSCRIPTION),
    );
    call(
        sender,
        Request {
            method,
            url: format!("{SUBSCRIPTION_BASE_URL}/{path}"),
            headers,
            body: body
                .map(|body| body.to_string().into_bytes())
                .unwrap_or_default()
                .into(),
        },
    )
    .await
}

async fn api(sender: &dyn Sender, token: &str, action: &str, body: Value) -> Result<Value, String> {
    api_request(
        sender,
        token,
        Method::POST,
        &format!("v1internal:{action}"),
        Some(body),
    )
    .await
}

fn project(value: &Value) -> Result<String, String> {
    match value.get("cloudaicompanionProject") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(project)) => Ok(project.clone()),
        _ => Err("Antigravity returned an invalid managed project".into()),
    }
}

async fn load(sender: &dyn Sender, token: &str) -> Result<Value, String> {
    api(
        sender,
        token,
        "loadCodeAssist",
        json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )
    .await
}

async fn onboard(sender: &dyn Sender, token: &str) -> Result<(), String> {
    let operation = async {
        let mut response = api(
            sender,
            token,
            "onboardUser",
            json!({
                "tierId":"free-tier",
                "metadata":{"ideType":"ANTIGRAVITY"}
            }),
        )
        .await?;
        loop {
            if response.get("error").is_some_and(|error| !error.is_null()) {
                return Err("Antigravity onboarding failed; finish account setup in Antigravity and sign in again".into());
            }
            if response.get("done").and_then(Value::as_bool) == Some(true) {
                let valid = response
                    .pointer("/response/@type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        kind.rsplit('/').next()
                            == Some("google.internal.cloud.code.v1internal.OnboardUserResponse")
                    });
                return if valid {
                    Ok(())
                } else {
                    Err("Antigravity returned an invalid onboarding response; sign in again".into())
                };
            }
            let name = response
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && name.split('/').all(|part| {
                            !part.is_empty()
                                && part != "."
                                && part != ".."
                                && part.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'-' | b'_' | b'.')
                                })
                        })
                })
                .ok_or_else(|| "Antigravity returned an invalid onboarding operation".to_owned())?;
            let path = format!("v1internal/{name}");
            nanocode_platform::sleep(ONBOARD_INTERVAL).await;
            response = api_request(sender, token, Method::GET, &path, None).await?;
        }
    };
    nanocode_platform::timeout(REQUEST_TIMEOUT, operation).await
        .map_err(|_| "Antigravity onboarding timed out; its outcome is unknown, finish account setup in Antigravity and sign in again".to_owned())?
}

pub(super) async fn discover_project(sender: &dyn Sender, token: &str) -> Result<String, String> {
    let mut loaded = load(sender, token).await?;
    let free_allowed = loaded
        .get("allowedTiers")
        .and_then(Value::as_array)
        .is_some_and(|tiers| {
            tiers
                .iter()
                .any(|tier| tier.get("id").and_then(Value::as_str) == Some("free-tier"))
        });
    let free_ineligible = loaded
        .get("ineligibleTiers")
        .and_then(Value::as_array)
        .is_some_and(|tiers| {
            tiers
                .iter()
                .any(|tier| tier.get("tierId").and_then(Value::as_str) == Some("free-tier"))
        });
    if !free_allowed && free_ineligible {
        return Err("This Google account is not eligible for Antigravity; finish account validation in Antigravity and sign in again".into());
    }
    if loaded.get("currentTier").is_none_or(Value::is_null) {
        onboard(sender, token).await?;
        loaded = load(sender, token).await?;
    }
    project(&loaded)
}

pub(super) async fn available_models(
    sender: &dyn Sender,
    token: &str,
    project: &str,
) -> Result<Value, String> {
    let mut body = json!({});
    if !project.is_empty() {
        body["project"] = Value::String(project.to_owned());
    }
    api(sender, token, "fetchAvailableModels", body).await
}

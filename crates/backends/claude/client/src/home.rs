use std::{
    collections::BTreeMap,
    io,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use nanocode_executor::{TcpListener, TcpStream};
use nanocode_io::{AsyncReadExt as _, AsyncWriteExt as _};
use nanocode_platform::Revisioned;
use serde::{Deserialize, Serialize};

use crate::{
    ClaudeAuthFuture, ClaudeAuthProvider, ClaudeAuthUnavailable,
    subscription::{
        ClaudeLoginMode, ClaudeSubscription, ClaudeSubscriptionCommit, ClaudeSubscriptionConfig,
        ClaudeSubscriptionError, ClaudeSubscriptionHost, ClaudeSubscriptionHostError,
        ClaudeSubscriptionHttpRequest, ClaudeSubscriptionHttpResponse, ClaudeSubscriptionStatus,
        ClaudeSubscriptionStoreValue, now,
    },
};

mod login;

const FILE: &str = "claude.json";
const KEY: &str = "claude";
const CALLBACK_PATH: &str = "/callback";
const CALLBACK_BYTES_MAX: usize = 16 * 1024;
const SIGNED_IN: &str = "<!doctype html><title>nanocode</title><p>Signed in to Claude. You can close this tab and return to nanocode.</p>";
const FAILED: &str = "<!doctype html><title>nanocode</title><p>Claude sign-in failed. Return to nanocode for details.</p>";

#[derive(Default, Deserialize, Serialize)]
struct Entry {
    revision: u64,
    payload: Option<String>,
}

struct HomeHost {
    path: PathBuf,
    http: nanocode_http::Client,
}

impl HomeHost {
    fn read(path: &Path) -> io::Result<BTreeMap<String, Entry>> {
        Self::parse(Revisioned::new(path).read()?.bytes.as_deref())
    }

    fn parse(bytes: Option<&[u8]>) -> io::Result<BTreeMap<String, Entry>> {
        bytes.map_or_else(
            || Ok(BTreeMap::new()),
            |bytes| serde_json::from_slice(bytes).map_err(io::Error::other),
        )
    }

    fn swap(
        path: &Path,
        key: &str,
        expected: u64,
        payload: &str,
    ) -> io::Result<ClaudeSubscriptionCommit> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Revisioned::new(path).update(|bytes| {
            let mut entries = Self::parse(bytes)?;
            let current = entries.get(key).map_or(0, |entry| entry.revision);
            if current != expected {
                return Ok((None, ClaudeSubscriptionCommit::Conflict(current)));
            }
            let revision = expected.saturating_add(1);
            entries.insert(
                key.to_owned(),
                Entry {
                    revision,
                    payload: Some(payload.to_owned()),
                },
            );
            let bytes = serde_json::to_vec(&entries).map_err(io::Error::other)?;
            Ok((Some(bytes), ClaudeSubscriptionCommit::Committed(revision)))
        })
    }
}

impl ClaudeSubscriptionHost for HomeHost {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> ClaudeAuthFuture<'a, Result<ClaudeSubscriptionStoreValue, ClaudeSubscriptionHostError>>
    {
        let path = self.path.clone();
        Box::pin(async move {
            let mut entries = nanocode_platform::spawn_blocking(move || Self::read(&path))
                .await
                .map_err(|_| ClaudeSubscriptionHostError)?
                .map_err(|_| ClaudeSubscriptionHostError)?;
            Ok(entries
                .remove(key)
                .map_or_else(ClaudeSubscriptionStoreValue::default, |entry| {
                    ClaudeSubscriptionStoreValue {
                        revision: entry.revision,
                        payload: entry.payload,
                    }
                }))
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_revision: u64,
        payload: &'a str,
    ) -> ClaudeAuthFuture<'a, Result<ClaudeSubscriptionCommit, ClaudeSubscriptionHostError>> {
        let (path, key, payload) = (self.path.clone(), key.to_owned(), payload.to_owned());
        Box::pin(async move {
            nanocode_platform::spawn_blocking(move || {
                Self::swap(&path, &key, expected_revision, &payload)
            })
            .await
            .map_err(|_| ClaudeSubscriptionHostError)?
            .map_err(|_| ClaudeSubscriptionHostError)
        })
    }

    fn request(
        &self,
        request: ClaudeSubscriptionHttpRequest,
    ) -> ClaudeAuthFuture<'_, Result<ClaudeSubscriptionHttpResponse, ClaudeSubscriptionHostError>>
    {
        Box::pin(async move {
            let method = nanocode_http::Method::from_bytes(request.method().as_bytes())
                .map_err(|_| ClaudeSubscriptionHostError)?;
            let mut builder = self
                .http
                .request(method, request.url())
                .headers(request.headers().clone())
                .timeout(Duration::from_millis(request.timeout_millis()));
            if !request.body().is_empty() {
                builder = builder
                    .header(nanocode_http::header::CONTENT_TYPE, request.content_type())
                    .body(request.body().to_owned());
            }
            let response = builder
                .send()
                .await
                .map(nanocode_http::decode_flate_response)
                .map_err(|_| ClaudeSubscriptionHostError)?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|_| ClaudeSubscriptionHostError)?;
            if body.len() > request.max_response_bytes() {
                return Err(ClaudeSubscriptionHostError);
            }
            Ok(ClaudeSubscriptionHttpResponse {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            })
        })
    }
}

impl ClaudeSubscription {
    pub fn home() -> Result<Self, ClaudeSubscriptionError> {
        let home =
            nanocode_home::AgentHome::from_env().map_err(|_| ClaudeSubscriptionError::Host)?;
        nanocode_tls::install_default_rustls_crypto_provider();
        Self::new(
            Arc::new(HomeHost {
                path: home.nanocode_home().join(FILE),
                http: nanocode_http::client(),
            }),
            KEY,
            ClaudeSubscriptionConfig::default(),
        )
    }
}

pub struct ClaudeHomeLogin {
    subscription: ClaudeSubscription,
    listener: TcpListener,
    origin: String,
    authorization_url: String,
    expires_at: u64,
}

impl ClaudeHomeLogin {
    pub async fn start() -> Result<Self, ClaudeSubscriptionError> {
        login::start().await
    }

    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub async fn complete(self) -> Result<ClaudeSubscriptionStatus, ClaudeSubscriptionError> {
        login::complete(self).await
    }
}

async fn callback(listener: &TcpListener) -> io::Result<(TcpStream, String)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut buffer = [0; 2048];
        while !request.windows(4).any(|window| window == b"\r\n\r\n")
            && request.len() < CALLBACK_BYTES_MAX
        {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let target = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default()
            .to_owned();
        if target.split('?').next() == Some(CALLBACK_PATH) {
            return Ok((stream, target));
        }
        reply(&mut stream, 404, "").await;
    }
}

async fn reply(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    drop(stream.write_all(response.as_bytes()).await);
    drop(stream.shutdown().await);
}

pub(crate) struct HomeFirst {
    home: Option<ClaudeSubscription>,
    fallback: Arc<dyn ClaudeAuthProvider>,
}

impl HomeFirst {
    pub(crate) fn new(fallback: Arc<dyn ClaudeAuthProvider>) -> Self {
        Self {
            home: ClaudeSubscription::home().ok(),
            fallback,
        }
    }

    async fn signed_in(&self) -> Option<&ClaudeSubscription> {
        let home = self.home.as_ref()?;
        matches!(
            home.status().await,
            Ok(ClaudeSubscriptionStatus::Authenticated { .. })
        )
        .then_some(home)
    }
}

impl ClaudeAuthProvider for HomeFirst {
    fn headers(
        &self,
    ) -> ClaudeAuthFuture<'_, Result<nanocode_http::types::header::HeaderMap, ClaudeAuthUnavailable>>
    {
        Box::pin(async move {
            if let Some(home) = &self.home
                && let Some(headers) = home.ready_headers().await
            {
                return headers.map_err(|error| ClaudeAuthUnavailable::new(error.to_string()));
            }
            self.fallback.headers().await.map_err(|error| {
                ClaudeAuthUnavailable::new(format!(
                    "{}; sign in with /login claude or set ANTHROPIC_API_KEY",
                    error.reason()
                ))
            })
        })
    }

    fn recover_unauthorized<'a>(
        &'a self,
        rejected: &'a nanocode_http::types::header::HeaderMap,
    ) -> ClaudeAuthFuture<'a, Result<bool, ClaudeAuthUnavailable>> {
        Box::pin(async move {
            match self.signed_in().await {
                Some(home) => home.recover_unauthorized(rejected).await,
                None => self.fallback.recover_unauthorized(rejected).await,
            }
        })
    }
}

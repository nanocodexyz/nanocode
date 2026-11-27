use super::storage::PlanStore;
use crate::http::PLAN_USER_AGENT;
use nanocode_digest::Sha256;
use nanocode_http::Method;
use nanocode_http::header::{HeaderMap, HeaderName, HeaderValue};
use nanocode_http::layer::{Request, Sender};
use nanocode_sync::{Atom, SingleFlight, StreamExt};
use nanocode_sys::base64::URL_SAFE_NO_PAD;
use nanocode_tempo::SigningKey;
use nanocode_url::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const PLAN_CLIENT_ID_VAR: &str = "NANOCODE_GOOGLE_AI_PLAN_CLIENT_ID";
pub const PLAN_CLIENT_SECRET_VAR: &str = "NANOCODE_GOOGLE_AI_PLAN_CLIENT_SECRET";
pub const PLAN_CALLBACK_PATH: &str = "/oauth2callback";
pub const PLAN_LOGIN_COMMAND: &str = "nanocode login google-ai-plan";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const QUOTA_SCOPE: &str = "https://www.googleapis.com/auth/generative-language.peruserquota";
const SCOPES: &str =
    "openid email profile https://www.googleapis.com/auth/generative-language.peruserquota";
const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
const REFRESH_MARGIN_SECONDS: u64 = 300;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SECRET_BYTES: usize = 32;
const JTI_BYTES: usize = 24;
const PROOF_RETRIES: usize = 2;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanClient {
    pub client_id: String,
    pub client_secret: String,
}

impl fmt::Debug for PlanClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanClient")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl PlanClient {
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        match (get(PLAN_CLIENT_ID_VAR), get(PLAN_CLIENT_SECRET_VAR)) {
            (Some(client_id), Some(client_secret)) => Ok(Self {
                client_id,
                client_secret,
            }),
            _ => Err(format!(
                "the Google AI plan sign-in needs a Google OAuth Desktop app client that Google approved for the plan: set {PLAN_CLIENT_ID_VAR} and {PLAN_CLIENT_SECRET_VAR}"
            )),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCredentials {
    pub client: PlanClient,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub dpop_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpop_nonce: Option<String>,
}

impl fmt::Debug for PlanCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanCredentials")
            .field("client", &self.client)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl PlanCredentials {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.client.client_id.trim().is_empty()
            || self.access_token.trim().is_empty()
            || self.refresh_token.trim().is_empty()
            || self.expires_at == 0
        {
            return Err(sign_in_again("its saved credentials are incomplete"));
        }
        Dpop::load(&self.dpop_key).map(drop)
    }

    fn fresh(&self) -> bool {
        self.expires_at.saturating_sub(now()) > REFRESH_MARGIN_SECONDS
    }
}

struct Dpop {
    key: SigningKey,
}

impl Dpop {
    fn generate() -> Result<(Self, String), String> {
        loop {
            let secret = random_bytes::<SECRET_BYTES>()?;
            if let Ok(key) = SigningKey::p256(&secret) {
                return Ok((Self { key }, URL_SAFE_NO_PAD.encode(secret)));
            }
        }
    }

    fn load(encoded: &str) -> Result<Self, String> {
        let secret = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| sign_in_again("its saved DPoP key is invalid"))?;
        SigningKey::p256(&secret)
            .map(|key| Self { key })
            .map_err(|_| sign_in_again("its saved DPoP key is invalid"))
    }

    fn proof(&self, jti: &str, nonce: Option<&str>, iat: u64) -> Result<String, String> {
        let (x, y) = self.key.public_key();
        let header = json!({
            "alg": "ES256",
            "jwk": {
                "crv": "P-256",
                "kty": "EC",
                "x": URL_SAFE_NO_PAD.encode(x),
                "y": URL_SAFE_NO_PAD.encode(y),
            },
            "typ": "dpop+jwt",
        });
        let mut payload = json!({"htm": "POST", "htu": TOKEN_URL, "iat": iat, "jti": jti});
        if let Some(nonce) = nonce {
            payload["nonce"] = Value::String(nonce.to_owned());
        }
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(payload.to_string())
        );
        let signature = self
            .key
            .sign_prehash(&Sha256::digest(input.as_bytes()))
            .map_err(|_| "could not sign the Google AI plan DPoP proof".to_owned())?;
        let mut raw = Vec::with_capacity(signature.r.len() + signature.s.len());
        raw.extend_from_slice(&signature.r);
        raw.extend_from_slice(&signature.s);
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(raw)))
    }
}

#[derive(Default, Deserialize)]
struct TokenBody {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl TokenBody {
    fn describe(&self, status: u16) -> String {
        [
            Some(status.to_string()),
            self.error.clone(),
            self.error_description.clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
    }

    fn expiry(&self, started: u64) -> Result<u64, String> {
        self.expires_in
            .filter(|seconds| *seconds > 0)
            .and_then(|seconds| started.checked_add(seconds))
            .ok_or_else(|| "Google returned an invalid Google AI plan token expiry".to_owned())
    }
}

struct TokenReply {
    status: u16,
    body: TokenBody,
    nonce: Option<String>,
    server_time: Option<u64>,
}

async fn post_token(
    sender: &dyn Sender,
    client: &PlanClient,
    pairs: &[(&str, &str)],
    proof: String,
) -> Result<TokenReply, String> {
    let mut body = nanocode_url::form::Serializer::new(String::new());
    body.extend_pairs(pairs.iter().copied().chain([
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
    ]));
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert("user-agent", HeaderValue::from_static(PLAN_USER_AGENT));
    let mut proof = HeaderValue::from_str(&proof)
        .map_err(|_| "the Google AI plan DPoP proof is not a valid header".to_owned())?;
    proof.set_sensitive(true);
    headers.insert(HeaderName::from_static("dpop"), proof);
    let operation = async {
        let response = sender
            .send(Request {
                method: Method::POST,
                url: TOKEN_URL.to_owned(),
                headers,
                body: body.finish().into_bytes().into(),
            })
            .await
            .map_err(|_| {
                "the Google AI plan token request failed; its outcome is unknown, sign in again"
                    .to_owned()
            })?;
        let header = |name: &str| {
            response
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let nonce = header("dpop-nonce");
        let server_time = header("date")
            .and_then(|date| nanocode_platform::parse_http_date(&date).ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs());
        let status = response.status.as_u16();
        let mut stream = response.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.map_err(|_| {
                "the Google AI plan token response was interrupted; sign in again".to_owned()
            })?);
        }
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| TokenBody {
            error_description: Some(String::from_utf8_lossy(&bytes).trim().to_owned()),
            ..TokenBody::default()
        });
        Ok(TokenReply {
            status,
            body,
            nonce,
            server_time,
        })
    };
    nanocode_platform::timeout(REQUEST_TIMEOUT, operation)
        .await
        .map_err(|_| {
            "the Google AI plan token request timed out; its outcome is unknown, sign in again"
                .to_owned()
        })?
}

pub struct PlanLogin {
    client: PlanClient,
    redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    dpop: Dpop,
    dpop_key: String,
    exchanged: AtomicBool,
}

impl fmt::Debug for PlanLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanLogin")
            .field("client", &self.client)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl PlanLogin {
    pub fn new(client: PlanClient, redirect_uri: String) -> Result<Self, String> {
        let url = Url::parse(&redirect_uri)
            .map_err(|_| "the Google AI plan redirect URI is invalid".to_owned())?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.path() != PLAN_CALLBACK_PATH
        {
            return Err(format!(
                "the Google AI plan sign-in redirects to http://127.0.0.1:PORT{PLAN_CALLBACK_PATH}, not {redirect_uri}"
            ));
        }
        let (dpop, dpop_key) = Dpop::generate()?;
        Ok(Self {
            client,
            redirect_uri,
            state: random_secret::<SECRET_BYTES>()?,
            nonce: random_secret::<SECRET_BYTES>()?,
            verifier: random_secret::<SECRET_BYTES>()?,
            dpop,
            dpop_key,
            exchanged: AtomicBool::new(false),
        })
    }

    pub fn authorization_url(&self) -> Result<String, String> {
        let mut url = Url::parse(AUTHORIZE_URL).map_err(|_| "invalid OAuth authorization URL")?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("client_id", self.client.client_id.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("nonce", self.nonce.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", self.state.as_str()),
        ]);
        Ok(url.into())
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    fn callback_code(&self, callback: &str) -> Result<String, String> {
        let callback = Url::parse(callback.trim())
            .map_err(|_| "paste the full URL the browser ended on after signing in".to_owned())?;
        let redirect = Url::parse(&self.redirect_uri).map_err(|_| "invalid redirect URI")?;
        if callback.scheme() != redirect.scheme()
            || callback.host_str() != redirect.host_str()
            || callback.port_or_known_default() != redirect.port_or_known_default()
            || callback.path() != redirect.path()
        {
            return Err("the Google AI plan callback URL does not match this sign-in".into());
        }
        let mut state = None;
        let mut code = None;
        let mut error = None;
        for (key, value) in callback.query_pairs() {
            let field = match key.as_ref() {
                "state" => &mut state,
                "code" => &mut code,
                "error" => &mut error,
                _ => continue,
            };
            if field.replace(value.into_owned()).is_some() {
                return Err("the Google AI plan callback repeats an OAuth parameter".into());
            }
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return Err("Google AI plan sign-in state mismatch; start a new sign-in".into());
        }
        match error.as_deref() {
            Some("access_denied") => return Err("the Google AI plan sign-in was cancelled".into()),
            Some(error) => return Err(format!("the Google AI plan sign-in failed: {error}")),
            None => {}
        }
        code.filter(|code| !code.trim().is_empty())
            .ok_or_else(|| "the Google AI plan callback has no authorization code".into())
    }

    pub async fn exchange(
        &self,
        callback: &str,
        sender: Arc<dyn Sender>,
    ) -> Result<PlanCredentials, String> {
        let code = self.callback_code(callback)?;
        if self.exchanged.swap(true, Ordering::AcqRel) {
            return Err("this Google AI plan sign-in was already exchanged or has an uncertain outcome; start a new sign-in".into());
        }
        let started = now();
        let jti = URL_SAFE_NO_PAD.encode(Sha256::digest(code.as_bytes()));
        let proof = self.dpop.proof(&jti, None, started)?;
        let reply = post_token(
            sender.as_ref(),
            &self.client,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect_uri),
            ],
            proof,
        )
        .await?;
        let body = &reply.body;
        let (Some(access_token), Some(refresh_token), 200) = (
            body.access_token.clone().filter(|token| !token.is_empty()),
            body.refresh_token.clone().filter(|token| !token.is_empty()),
            reply.status,
        ) else {
            return Err(format!(
                "the Google AI plan sign-in failed: {}",
                body.describe(reply.status)
            ));
        };
        if !body
            .scope
            .as_deref()
            .unwrap_or_default()
            .split(' ')
            .any(|scope| scope == QUOTA_SCOPE)
        {
            return Err(
                "Google did not grant access to the Google AI plan; sign in again and allow it"
                    .into(),
            );
        }
        check_id_token(
            body.id_token.as_deref(),
            &self.nonce,
            &self.client.client_id,
        )?;
        let credentials = PlanCredentials {
            client: self.client.clone(),
            access_token,
            refresh_token,
            expires_at: body.expiry(started)?,
            dpop_key: self.dpop_key.clone(),
            dpop_nonce: None,
        };
        credentials.validate()?;
        Ok(credentials)
    }
}

fn check_id_token(token: Option<&str>, nonce: &str, client_id: &str) -> Result<(), String> {
    let invalid = || "Google returned an ID token that failed validation".to_owned();
    let payload = token
        .and_then(|token| token.split('.').nth(1))
        .ok_or_else(|| "Google returned no ID token".to_owned())?;
    let claims: Value = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(invalid)?;
    let issuer = claims.get("iss").and_then(Value::as_str);
    let audience = claims.get("aud").and_then(Value::as_str);
    let expires = claims
        .get("exp")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let matches = issuer.is_some_and(|issuer| ISSUERS.contains(&issuer))
        && audience == Some(client_id)
        && claims.get("nonce").and_then(Value::as_str) == Some(nonce)
        && expires > now();
    matches.then_some(()).ok_or_else(invalid)
}

#[derive(Clone)]
enum TokenState {
    Ready(PlanCredentials),
    Rejected(PlanCredentials),
    Refreshing,
    Unsaved {
        previous: PlanCredentials,
        credentials: PlanCredentials,
    },
    Failed(String),
}

struct Inner {
    state: Atom<TokenState>,
    sender: Arc<dyn Sender>,
    flight: SingleFlight<Result<String, String>>,
    store: Atom<Option<PlanStore>>,
}

#[derive(Clone)]
pub struct PlanAuth {
    inner: Arc<Inner>,
}

impl PartialEq for PlanAuth {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for PlanAuth {}

impl fmt::Debug for PlanAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanAuth").finish_non_exhaustive()
    }
}

impl PlanAuth {
    pub fn new(credentials: PlanCredentials, sender: Arc<dyn Sender>) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Atom::new(TokenState::Ready(credentials)),
                sender,
                flight: SingleFlight::default(),
                store: Atom::new(None),
            }),
        }
    }

    #[must_use]
    pub fn with_store(self, store: PlanStore) -> Self {
        self.inner.store.set(Some(store));
        self
    }

    pub async fn discover(
        lookup: &(dyn Fn(&str) -> Option<String> + Sync),
        sender: Arc<dyn Sender>,
    ) -> Result<Self, String> {
        PlanStore::locate(None, lookup)?.load_auth(sender).await
    }

    pub async fn token(&self) -> Result<String, String> {
        self.inner.flight.run(|| self.resolve()).await
    }

    async fn resolve(&self) -> Result<String, String> {
        let credentials = match self.inner.state.read(Clone::clone) {
            TokenState::Ready(credentials) => {
                credentials.validate()?;
                if credentials.fresh() {
                    return Ok(credentials.access_token);
                }
                credentials
            }
            TokenState::Rejected(credentials) => credentials,
            TokenState::Unsaved {
                previous,
                credentials,
            } => return self.persist(previous, credentials).await,
            TokenState::Refreshing => {
                return Err(sign_in_again(
                    "a token refresh was interrupted and its outcome is unknown",
                ));
            }
            TokenState::Failed(error) => return Err(error),
        };
        self.inner.state.set(TokenState::Refreshing);
        match self.refresh(&credentials).await {
            Ok(refreshed) => {
                self.inner.state.set(TokenState::Unsaved {
                    previous: credentials.clone(),
                    credentials: refreshed.clone(),
                });
                self.persist(credentials, refreshed).await
            }
            Err(error) => {
                self.inner.state.set(TokenState::Failed(error.clone()));
                Err(error)
            }
        }
    }

    async fn refresh(&self, credentials: &PlanCredentials) -> Result<PlanCredentials, String> {
        let dpop = Dpop::load(&credentials.dpop_key)?;
        let mut nonce = credentials.dpop_nonce.clone();
        let mut iat = None;
        let mut started = now();
        let mut reply = self
            .post_refresh(credentials, &dpop, nonce.as_deref(), iat)
            .await?;
        for _ in 0..PROOF_RETRIES {
            match (reply.body.error.as_deref(), &reply.nonce, reply.server_time) {
                (Some("use_dpop_nonce"), Some(fresh), _) if nonce.as_ref() != Some(fresh) => {
                    nonce = Some(fresh.clone());
                }
                (Some("invalid_dpop_proof"), _, Some(server)) if iat.is_none() => {
                    iat = Some(server);
                }
                _ => break,
            }
            started = now();
            reply = self
                .post_refresh(credentials, &dpop, nonce.as_deref(), iat)
                .await?;
        }
        let body = &reply.body;
        let Some(access_token) = body
            .access_token
            .clone()
            .filter(|token| reply.status == 200 && !token.is_empty())
        else {
            return Err(match body.error.as_deref() {
                Some(error @ ("invalid_grant" | "unauthorized_client")) => {
                    sign_in_again(&format!("Google ended it with {error}"))
                }
                _ => format!(
                    "the Google AI plan token refresh failed: {}",
                    body.describe(reply.status)
                ),
            });
        };
        let refreshed = PlanCredentials {
            client: credentials.client.clone(),
            access_token,
            refresh_token: body
                .refresh_token
                .clone()
                .filter(|token| !token.is_empty())
                .unwrap_or_else(|| credentials.refresh_token.clone()),
            expires_at: body.expiry(started)?,
            dpop_key: credentials.dpop_key.clone(),
            dpop_nonce: reply.nonce.clone().or(nonce),
        };
        refreshed.validate()?;
        Ok(refreshed)
    }

    async fn post_refresh(
        &self,
        credentials: &PlanCredentials,
        dpop: &Dpop,
        nonce: Option<&str>,
        iat: Option<u64>,
    ) -> Result<TokenReply, String> {
        let jti = random_secret::<JTI_BYTES>()?;
        let proof = dpop.proof(&jti, nonce, iat.unwrap_or_else(now))?;
        post_token(
            self.inner.sender.as_ref(),
            &credentials.client,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &credentials.refresh_token),
            ],
            proof,
        )
        .await
    }

    async fn persist(
        &self,
        previous: PlanCredentials,
        credentials: PlanCredentials,
    ) -> Result<String, String> {
        if let Some(store) = self.inner.store.read(Clone::clone) {
            store.replace(&previous, &credentials).await?;
        }
        self.inner.state.set(TokenState::Ready(credentials.clone()));
        Ok(credentials.access_token)
    }

    pub fn invalidate(&self, rejected: &str) {
        self.inner.state.update(|state| {
            if let TokenState::Ready(credentials) = state
                && credentials.access_token == rejected
            {
                *state = TokenState::Rejected(credentials.clone());
            }
        });
    }
}

fn sign_in_again(why: &str) -> String {
    format!(
        "the Google AI plan sign-in is no longer valid ({why}; invalid_grant): run `{PLAN_LOGIN_COMMAND}`"
    )
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    nanocode_platform::fill_random(&mut bytes)
        .map_err(|_| "could not generate secure Google AI plan OAuth randomness".to_owned())?;
    Ok(bytes)
}

fn random_secret<const N: usize>() -> Result<String, String> {
    random_bytes::<N>().map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
}

fn now() -> u64 {
    nanocode_platform::unix_millis() / 1000
}

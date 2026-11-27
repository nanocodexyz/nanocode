pub(crate) mod storage;
mod wire;

pub use storage::{SubscriptionStorage, SubscriptionStore};

use nanocode_digest::Sha256;
use nanocode_http::layer::Sender;
use nanocode_sync::{Atom, SingleFlight};
use nanocode_sys::base64::URL_SAFE_NO_PAD;
use nanocode_url::Url;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const SUBSCRIPTION_REDIRECT_URI: &str = "https://antigravity.google/oauth-callback";
pub const SUBSCRIPTION_STATE_FILE: &str = "antigravity-subscription.json";
pub const SUBSCRIPTION_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs https://www.googleapis.com/auth/aicode openid";
const REFRESH_MARGIN_SECONDS: u64 = 60;
const STATE_RANDOM_BYTES: usize = 16;
const PKCE_RANDOM_BYTES: usize = 32;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub project: String,
}

impl fmt::Debug for SubscriptionCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionCredentials")
            .field("project", &self.project)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl SubscriptionCredentials {
    fn validate(&self) -> Result<(), String> {
        if self.access_token.trim().is_empty()
            || self.refresh_token.trim().is_empty()
            || self.expires_at == 0
        {
            return Err(
                "Antigravity subscription credentials are incomplete; sign in again".into(),
            );
        }
        Ok(())
    }

    fn fresh(&self) -> bool {
        self.expires_at.saturating_sub(now()) > REFRESH_MARGIN_SECONDS
    }
}

pub struct SubscriptionLogin {
    redirect_uri: String,
    state: String,
    verifier: String,
    exchanged: AtomicBool,
}

impl fmt::Debug for SubscriptionLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionLogin")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl SubscriptionLogin {
    pub fn new(redirect_uri: String) -> Result<Self, String> {
        if redirect_uri != SUBSCRIPTION_REDIRECT_URI {
            return Err(format!(
                "Antigravity OAuth requires redirect URI {SUBSCRIPTION_REDIRECT_URI}"
            ));
        }
        Ok(Self {
            redirect_uri,
            state: random_secret::<STATE_RANDOM_BYTES>()?,
            verifier: random_secret::<PKCE_RANDOM_BYTES>()?,
            exchanged: AtomicBool::new(false),
        })
    }

    pub fn authorization_url(&self) -> Result<String, String> {
        let mut url = Url::parse(AUTHORIZE_URL).map_err(|_| "invalid OAuth authorization URL")?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("access_type", "offline"),
            ("client_id", SUBSCRIPTION_CLIENT_ID),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("prompt", "consent"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", self.state.as_str()),
        ]);
        Ok(url.into())
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    fn callback_code(&self, callback: &str) -> Result<String, String> {
        let callback = callback.trim();
        if callback.is_empty()
            || callback
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err("paste the Antigravity authorization code or full callback URL".into());
        }
        if !callback.contains([':', '?', '#', '&', '=']) {
            return Ok(callback.to_owned());
        }
        let callback = Url::parse(callback)
            .map_err(|_| "paste the Antigravity authorization code or full callback URL")?;
        let redirect = Url::parse(&self.redirect_uri).map_err(|_| "invalid redirect URI")?;
        if callback.scheme() != redirect.scheme()
            || callback.host_str() != redirect.host_str()
            || callback.port_or_known_default() != redirect.port_or_known_default()
            || callback.path() != redirect.path()
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
        {
            return Err("the Antigravity callback URL does not match this login".into());
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
                return Err("the Antigravity callback contains duplicate OAuth parameters".into());
            }
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return Err("Antigravity OAuth state mismatch; start a new login".into());
        }
        if error.is_some() {
            return Err("Google declined or cancelled the Antigravity OAuth login".into());
        }
        code.filter(|code| !code.trim().is_empty())
            .ok_or_else(|| "the Antigravity callback has no authorization code".into())
    }

    pub async fn exchange(
        &self,
        authorization_response: &str,
        sender: Arc<dyn Sender>,
    ) -> Result<SubscriptionCredentials, String> {
        let code = self.callback_code(authorization_response)?;
        if self.exchanged.swap(true, Ordering::AcqRel) {
            return Err("this Antigravity login was already exchanged or has an uncertain outcome; start a new login".into());
        }
        let started = now();
        let token = wire::token(
            sender.as_ref(),
            &[
                ("client_id", SUBSCRIPTION_CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &self.redirect_uri),
                ("code_verifier", &self.verifier),
            ],
        )
        .await?;
        let refresh_token = token
            .refresh_token
            .clone()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                "Google returned no refresh token; start a new login with consent".to_owned()
            })?;
        let expires_at = token.expiry(started)?;
        let project = wire::discover_project(sender.as_ref(), &token.access_token).await?;
        let credentials = SubscriptionCredentials {
            access_token: token.access_token,
            refresh_token,
            expires_at,
            project,
        };
        credentials.validate()?;
        Ok(credentials)
    }
}

#[derive(Clone)]
enum TokenState {
    Ready(SubscriptionCredentials),
    Rejected(SubscriptionCredentials),
    Refreshing,
    Unsaved {
        previous: SubscriptionCredentials,
        credentials: SubscriptionCredentials,
    },
    Failed(String),
}

struct Inner {
    project: String,
    state: Atom<TokenState>,
    sender: Arc<dyn Sender>,
    flight: SingleFlight<Result<String, String>>,
    store: Atom<Option<SubscriptionStore>>,
}

#[derive(Clone)]
pub struct SubscriptionAuth {
    inner: Arc<Inner>,
}

impl PartialEq for SubscriptionAuth {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for SubscriptionAuth {}

impl fmt::Debug for SubscriptionAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionAuth")
            .field("project", &self.inner.project)
            .finish_non_exhaustive()
    }
}

impl SubscriptionAuth {
    pub fn new(credentials: SubscriptionCredentials, sender: Arc<dyn Sender>) -> Self {
        Self {
            inner: Arc::new(Inner {
                project: credentials.project.clone(),
                state: Atom::new(TokenState::Ready(credentials)),
                sender,
                flight: SingleFlight::default(),
                store: Atom::new(None),
            }),
        }
    }

    #[must_use]
    pub fn with_store(self, store: SubscriptionStore) -> Self {
        self.inner.store.set(Some(store));
        self
    }

    pub async fn discover(
        lookup: &(dyn Fn(&str) -> Option<String> + Sync),
        sender: Arc<dyn Sender>,
    ) -> Result<Self, String> {
        SubscriptionStore::locate(None, lookup)?
            .load_auth(sender)
            .await
    }

    pub fn project(&self) -> &str {
        &self.inner.project
    }

    pub async fn available_models(&self) -> Result<serde_json::Value, String> {
        let token = self.token().await?;
        wire::available_models(self.inner.sender.as_ref(), &token, self.project()).await
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
            TokenState::Unsaved { previous, credentials } => return self.persist(previous, credentials).await,
            TokenState::Refreshing => return Err("Antigravity token refresh was interrupted and its outcome is unknown; sign in again".into()),
            TokenState::Failed(error) => return Err(error),
        };
        self.inner.state.set(TokenState::Refreshing);
        let started = now();
        let result = wire::token(
            self.inner.sender.as_ref(),
            &[
                ("client_id", SUBSCRIPTION_CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("grant_type", "refresh_token"),
                ("refresh_token", &credentials.refresh_token),
            ],
        )
        .await;
        let refreshed = result.and_then(|token| {
            let expires_at = token.expiry(started)?;
            let refreshed = SubscriptionCredentials {
                access_token: token.access_token,
                refresh_token: token
                    .refresh_token
                    .unwrap_or_else(|| credentials.refresh_token.clone()),
                expires_at,
                project: credentials.project.clone(),
            };
            refreshed.validate()?;
            Ok(refreshed)
        });
        match refreshed {
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

    async fn persist(
        &self,
        previous: SubscriptionCredentials,
        credentials: SubscriptionCredentials,
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

fn random_secret<const N: usize>() -> Result<String, String> {
    let mut bytes = [0; N];
    nanocode_platform::fill_random(&mut bytes)
        .map_err(|_| "could not generate secure Antigravity OAuth randomness".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn now() -> u64 {
    nanocode_platform::unix_millis() / 1000
}

use crate::plan::{self, PLAN_LOGIN_COMMAND, PlanAuth};
use crate::project::{Projects, Saved, Store};
use crate::sse::SseDecoder;
use crate::subscription::SubscriptionAuth;
use crate::wire::{GenerateResponse, Part, Reply, merge_parts};
use nanocode_google_auth::GoogleAuth;
use nanocode_http::Method;
use nanocode_http::header::{HeaderMap, HeaderName, HeaderValue};
use nanocode_http::layer::{Request, Sender};
use nanocode_sync::StreamExt;
use serde_json::Value;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const USER_AGENT_GATEWAY: &str = "antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=gateway)";
pub const USER_AGENT_API_KEY: &str = "antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=api_key)";
pub const GATEWAY_TIER: &str = "CiphbnRpZ3Jhdml0eS5jbGlfZ2F0ZXdheS5lbnZfcHJvZC50aWVyX3BhaWQ=";
pub const USER_AGENT_VERTEX: &str = "antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=vertex_ai)";
pub const USER_AGENT_VERTEX_EXPRESS: &str = "antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=vertex_express)";
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const VERTEX_GLOBAL_BASE_URL: &str = "https://aiplatform.googleapis.com";
pub const VERTEX_GLOBAL_LOCATION: &str = "global";
pub const SETUP_COMMAND: &str = "nanocode setup google";
pub const ADC_LOGIN_COMMAND: &str = "gcloud auth application-default login";
pub const SERVICE_DISABLED_BACKOFF: [Duration; 2] =
    [Duration::from_secs(5), Duration::from_secs(15)];
pub const SUBSCRIPTION_LOGIN_COMMAND: &str = "nanocode login google";
pub const SUBSCRIPTION_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const USER_AGENT_SUBSCRIPTION: &str = "antigravity/cli/1.5.13 (aidev_client; os_type=linux; arch=amd64; cl=1025641825; auth_method=consumer)";
pub const PLAN_USER_AGENT: &str = concat!("nanocode/", env!("CARGO_PKG_VERSION"));
const LISTED_FAILURES: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    ApiKey(String),
    Bearer(String),
    Google(GoogleAuth),
    Subscription(SubscriptionAuth),
    Plan(PlanAuth),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    GeminiApi,
    Gateway,
    Vertex { project: String, location: String },
    VertexExpress,
    Antigravity { project: String },
    GoogleAiPlan,
}

impl Route {
    pub const fn is_vertex(&self) -> bool {
        matches!(self, Self::Vertex { .. } | Self::VertexExpress)
    }

    pub const fn method(&self) -> AuthMethod {
        match self {
            Self::GeminiApi => AuthMethod::ApiKey,
            Self::Gateway => AuthMethod::Gateway,
            Self::Vertex { .. } => AuthMethod::Vertex,
            Self::VertexExpress => AuthMethod::VertexExpress,
            Self::Antigravity { .. } => AuthMethod::Antigravity,
            Self::GoogleAiPlan => AuthMethod::GoogleAiPlan,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    ApiKey,
    Vertex,
    VertexExpress,
    Gateway,
    Antigravity,
    GoogleAiPlan,
}

impl AuthMethod {
    pub const ALL: [Self; 6] = [
        Self::ApiKey,
        Self::Vertex,
        Self::VertexExpress,
        Self::Gateway,
        Self::Antigravity,
        Self::GoogleAiPlan,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::Vertex => "vertex",
            Self::VertexExpress => "vertex-express",
            Self::Gateway => "gateway",
            Self::Antigravity => "antigravity",
            Self::GoogleAiPlan => "google-ai-plan",
        }
    }

    pub fn parse(name: &str) -> Result<Option<Self>, String> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        if name.is_empty() || name == "auto" {
            return Ok(None);
        }
        Self::ALL
            .into_iter()
            .find(|method| method.name() == name)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "unknown Google auth method {name:?}; expected auto, api-key, vertex, vertex-express, gateway, antigravity or google-ai-plan"
                )
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoogleConfig {
    pub method: Option<AuthMethod>,
    pub project: Option<String>,
    pub location: Option<String>,
    pub state: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub base_url: String,
    pub auth: Auth,
    pub user_agent: String,
    pub route: Route,
    pub notice: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial: Duration,
    pub multiplier: u32,
    pub service_disabled: &'static [Duration],
}

impl RetryPolicy {
    pub const NONE: Self = Self {
        max_retries: 0,
        initial: Duration::ZERO,
        multiplier: 1,
        service_disabled: &[],
    };
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            initial: Duration::from_secs(1),
            multiplier: 2,
            service_disabled: &SERVICE_DISABLED_BACKOFF,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    ServiceDisabled,
    BillingDisabled,
    PermissionDenied,
    ModelNotFound,
    FreeTier,
    RateLimited,
    Reauth,
    Other,
}

fn mentions_reauth(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["invalid_grant", "invalid_rapt", "reauth"]
        .iter()
        .any(|needle| lower.contains(needle))
}

pub fn classify(status: u16, body: &str) -> Failure {
    let value = serde_json::from_str::<Value>(body).ok();
    let error = value.as_ref().and_then(|value| value.get("error"));
    let reason = error
        .and_then(|error| error.get("details"))
        .and_then(Value::as_array)
        .and_then(|details| {
            details
                .iter()
                .find_map(|detail| detail.get("reason").and_then(Value::as_str))
        });
    let lower = body.to_ascii_lowercase();
    match (status, reason) {
        (_, Some("SERVICE_DISABLED")) => Failure::ServiceDisabled,
        (_, Some("BILLING_DISABLED")) => Failure::BillingDisabled,
        _ if lower.contains("free_tier") || lower.contains("free tier") => Failure::FreeTier,
        (403, _)
            if lower.contains("has not been used in project")
                || lower.contains("it is disabled") =>
        {
            Failure::ServiceDisabled
        }
        (403, _) if lower.contains("billing") => Failure::BillingDisabled,
        (403, _) => Failure::PermissionDenied,
        (404, _) => Failure::ModelNotFound,
        (429, _) => Failure::RateLimited,
        (400 | 401, _) if mentions_reauth(body) => Failure::Reauth,
        _ => Failure::Other,
    }
}

pub fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_owned())
}

pub fn reauth_hint() -> String {
    format!("your Google sign-in expired or needs reauthentication: run `{ADC_LOGIN_COMMAND}`")
}

fn plan_hint(failure: Failure, model: Option<&str>) -> Option<String> {
    match failure {
        Failure::Reauth => Some(format!(
            "your Google AI plan sign-in expired or was rejected: run `{PLAN_LOGIN_COMMAND}`"
        )),
        Failure::PermissionDenied | Failure::ServiceDisabled => Some(
            "Google refused this account or nanocode's OAuth client for the Google AI plan: check that the account has an eligible plan and that the client in the saved sign-in is one Google approved for it".to_owned(),
        ),
        Failure::BillingDisabled | Failure::FreeTier => Some(
            "this Google account has no eligible Google AI plan: upgrade it, or use a Gemini API key (GEMINI_API_KEY)".to_owned(),
        ),
        Failure::RateLimited => Some(
            "the Google AI plan's quota is used up for now: wait for its reset, or use a Gemini API key".to_owned(),
        ),
        Failure::ModelNotFound => Some(format!(
            "{} is not on the Google AI plan: choose another model",
            model.unwrap_or("this model")
        )),
        Failure::Other => None,
    }
}

fn daily_limit(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("perday") || lower.contains("per day")
}

fn subscription_hint(failure: Failure, model: Option<&str>) -> Option<String> {
    match failure {
        Failure::Reauth => Some(format!(
            "your Antigravity sign-in expired or was rejected: run `{SUBSCRIPTION_LOGIN_COMMAND}`"
        )),
        Failure::PermissionDenied | Failure::ServiceDisabled => Some(format!(
            "this Google account cannot use Antigravity: check its Antigravity access and sign in with `{SUBSCRIPTION_LOGIN_COMMAND}`; Application Default Credentials do not grant subscription access"
        )),
        Failure::BillingDisabled | Failure::FreeTier => Some(
            "check this account's Antigravity subscription eligibility; Cloud project billing and Gemini API keys do not provide Antigravity subscription quota".to_owned(),
        ),
        Failure::RateLimited => Some(
            "Antigravity subscription quota is exhausted or rate limited: wait for its reset or check your account's plan; Google Cloud project quota is separate".to_owned(),
        ),
        Failure::ModelNotFound => Some(format!(
            "{} is not available through this Antigravity subscription: check the model name and your account's model access",
            model.unwrap_or("this model")
        )),
        Failure::Other => None,
    }
}

#[derive(Debug)]
pub enum ClientError {
    Transport(String),
    Status {
        status: u16,
        body: String,
        hint: Option<String>,
    },
    Decode(String),
    InvalidRequest(String),
    Blocked(String),
    Auth(String),
    SubscriptionAuth(String),
    PlanAuth(String),
}

impl ClientError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, body, .. } => {
                classify(*status, body) != Failure::FreeTier
                    && matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
            }
            Self::Decode(_)
            | Self::InvalidRequest(_)
            | Self::Blocked(_)
            | Self::Auth(_)
            | Self::SubscriptionAuth(_)
            | Self::PlanAuth(_) => false,
        }
    }

    pub fn failure(&self) -> Failure {
        match self {
            Self::Status { status, body, .. } => classify(*status, body),
            Self::Auth(detail) | Self::SubscriptionAuth(detail) | Self::PlanAuth(detail)
                if mentions_reauth(detail) =>
            {
                Failure::Reauth
            }
            Self::Transport(_)
            | Self::Decode(_)
            | Self::InvalidRequest(_)
            | Self::Blocked(_)
            | Self::Auth(_)
            | Self::SubscriptionAuth(_)
            | Self::PlanAuth(_) => Failure::Other,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "Gemini transport error: {detail}"),
            Self::Status { status, body, hint } => {
                write!(
                    formatter,
                    "Gemini API returned {status}: {}",
                    error_message(body)
                )?;
                match hint {
                    Some(hint) => write!(formatter, "\nhint: {hint}"),
                    None => Ok(()),
                }
            }
            Self::Decode(detail) => {
                write!(formatter, "Gemini response could not be decoded: {detail}")
            }
            Self::InvalidRequest(detail) => {
                write!(formatter, "Gemini request is invalid: {detail}")
            }
            Self::Blocked(detail) => write!(formatter, "Gemini blocked the request: {detail}"),
            Self::SubscriptionAuth(detail) => write!(
                formatter,
                "Antigravity subscription authentication failed: {detail}\nhint: run `{SUBSCRIPTION_LOGIN_COMMAND}` to sign in again"
            ),
            Self::PlanAuth(detail) => {
                write!(formatter, "Google AI plan authentication failed: {detail}")
            }
            Self::Auth(detail) => {
                write!(formatter, "Google authentication failed: {detail}")?;
                match mentions_reauth(detail) {
                    true => write!(formatter, "\nhint: {}", reauth_hint()),
                    false => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for ClientError {}

fn present(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn enabled(value: Option<String>) -> bool {
    present(value)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn vertex_base_url(location: &str) -> String {
    match location {
        VERTEX_GLOBAL_LOCATION => VERTEX_GLOBAL_BASE_URL.to_owned(),
        location => format!("https://{location}-aiplatform.googleapis.com"),
    }
}

impl Endpoint {
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let get = |name: &str| present(lookup(name));
        Self::gateway(&get).or_else(|| Self::api_key(&get))
    }

    fn gateway(get: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let url = get("AGY_LLM_GATEWAY_URL")?;
        Some(Self {
            base_url: url.trim_end_matches('/').to_owned(),
            auth: get("AGY_LLM_GATEWAY_API_KEY").map_or(Auth::None, Auth::Bearer),
            user_agent: USER_AGENT_GATEWAY.to_owned(),
            route: Route::Gateway,
            notice: None,
        })
    }

    fn api_key(get: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let key = get("GEMINI_API_KEY").or_else(|| get("GOOGLE_API_KEY"))?;
        let base = get("GEMINI_BASE_URL")
            .or_else(|| get("GOOGLE_GEMINI_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Some(Self {
            base_url: base.trim_end_matches('/').to_owned(),
            auth: Auth::ApiKey(key),
            user_agent: USER_AGENT_API_KEY.to_owned(),
            route: Route::GeminiApi,
            notice: None,
        })
    }

    fn vertex_express(get: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let key = get("GOOGLE_API_KEY").or_else(|| get("GEMINI_API_KEY"))?;
        let base =
            get("GOOGLE_VERTEX_BASE_URL").unwrap_or_else(|| VERTEX_GLOBAL_BASE_URL.to_owned());
        Some(Self {
            base_url: base.trim_end_matches('/').to_owned(),
            auth: Auth::ApiKey(key),
            user_agent: USER_AGENT_VERTEX_EXPRESS.to_owned(),
            route: Route::VertexExpress,
            notice: None,
        })
    }

    pub fn for_subscription(auth: &SubscriptionAuth, base: Option<&str>) -> Self {
        Self {
            base_url: base
                .unwrap_or(SUBSCRIPTION_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
            auth: Auth::Subscription(auth.clone()),
            user_agent: USER_AGENT_SUBSCRIPTION.to_owned(),
            route: Route::Antigravity {
                project: auth.project().to_owned(),
            },
            notice: None,
        }
    }

    pub fn for_plan(auth: &PlanAuth, base: Option<&str>) -> Self {
        Self {
            base_url: base
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
            auth: Auth::Plan(auth.clone()),
            user_agent: PLAN_USER_AGENT.to_owned(),
            route: Route::GoogleAiPlan,
            notice: None,
        }
    }

    pub fn for_vertex(
        auth: &GoogleAuth,
        base: Option<&str>,
        project: &str,
        location: &str,
    ) -> Self {
        let base = base.map_or_else(|| vertex_base_url(location), str::to_owned);
        Self {
            base_url: base.trim_end_matches('/').to_owned(),
            auth: Auth::Google(auth.clone().with_user_project(project.to_owned())),
            user_agent: USER_AGENT_VERTEX.to_owned(),
            route: Route::Vertex {
                project: project.to_owned(),
                location: location.to_owned(),
            },
            notice: None,
        }
    }

    async fn vertex(
        config: &GoogleConfig,
        get: &(dyn Fn(&str) -> Option<String> + Sync),
        sender: Arc<dyn Sender>,
        model: &str,
    ) -> Result<Self, String> {
        let auth = GoogleAuth::discover(get, Arc::clone(&sender))
            .await
            .map_err(|error| error.to_string())?;
        let projects = Projects::new(auth.clone(), sender, get);
        let configured_location =
            present(config.location.clone()).or_else(|| get("GOOGLE_CLOUD_LOCATION"));
        let found = auth.project_source(config.project.as_deref(), get);
        if let Some((project, source)) = &found
            && source.is_explicit()
        {
            let location = configured_location.unwrap_or_else(|| VERTEX_GLOBAL_LOCATION.to_owned());
            return Ok(projects.endpoint(project, &location));
        }
        let store = Store::locate(config.state.clone(), get);
        let identity = auth.identity();
        if let Some(saved) = store.load(&identity).await {
            let location = configured_location.unwrap_or(saved.location);
            return Ok(projects.endpoint(&saved.project, &location));
        }
        let location = configured_location.unwrap_or_else(|| VERTEX_GLOBAL_LOCATION.to_owned());
        let saved = |project: &str| Saved {
            project: project.to_owned(),
            location: location.clone(),
            model: model.to_owned(),
        };
        let mut ruled_out = None;
        if let Some((project, source)) = found {
            let probe = projects.probe(&project, &location, model).await;
            if !probe.rules_out() {
                if probe.is_ready() {
                    let _ = store.save(&identity, saved(&project)).await;
                }
                return Ok(projects.endpoint(&project, &location));
            }
            ruled_out = Some((project, source, probe));
        }
        let skip = ruled_out.as_ref().map(|(project, _, _)| project.as_str());
        let why = match &ruled_out {
            Some((project, source, probe)) => format!(
                "{project} (from {}) answered: {}",
                source.name(),
                probe.describe()
            ),
            None => format!(
                "the {} credentials from {} name no project",
                auth.kind().name(),
                auth.origin()
            ),
        };
        let discovery = projects
            .discover(&location, model, skip)
            .await
            .map_err(|error| {
                format!(
                    "Vertex AI needs a Google Cloud project and {why}; listing your projects to find one failed: {error}\nhint: run `{SETUP_COMMAND}`, pass --google-project, or set GOOGLE_CLOUD_PROJECT"
                )
            })?;
        let Some(chosen) = discovery.chosen else {
            let failures = discovery
                .probed
                .iter()
                .take(LISTED_FAILURES)
                .map(|(project, probe)| format!("{}: {}", project.id, probe.describe()))
                .collect::<Vec<_>>();
            let more = discovery.probed.len().saturating_sub(LISTED_FAILURES);
            let listed = match (failures.is_empty(), more) {
                (true, _) => "you can list no active projects".to_owned(),
                (false, 0) => failures.join("; "),
                (false, more) => format!("{}; and {more} more", failures.join("; ")),
            };
            return Err(format!(
                "Vertex AI needs a Google Cloud project where {model} answers in {location}, and none worked: {why}; {listed}\nhint: run `{SETUP_COMMAND}` to enable Vertex AI (aiplatform.googleapis.com) in one of them, pass --google-project, or use a Gemini API key (GEMINI_API_KEY)"
            ));
        };
        let kept = match store.save(&identity, saved(&chosen.id)).await {
            Ok(path) => format!("saved in {path}"),
            Err(error) => format!("not saved: {error}"),
        };
        let mut endpoint = projects.endpoint(&chosen.id, &location);
        endpoint.notice = Some(format!(
            "using Google Cloud project {} for Vertex AI ({why}; {kept}); override with --google-project or GOOGLE_CLOUD_PROJECT, or rerun `{SETUP_COMMAND}`",
            chosen.id
        ));
        Ok(endpoint)
    }

    pub async fn discover(
        config: &GoogleConfig,
        lookup: &(dyn Fn(&str) -> Option<String> + Sync),
        sender: Arc<dyn Sender>,
        model: &str,
    ) -> Result<Self, String> {
        let get = |name: &str| present(lookup(name));
        let missing = |method: AuthMethod, needs: &str| {
            format!("--google-auth {} needs {needs}", method.name())
        };
        match config.method {
            Some(AuthMethod::Antigravity) => {
                let auth = SubscriptionAuth::discover(&get, sender).await
                    .map_err(|error| format!("--google-auth antigravity: {error}\nhint: run `{SUBSCRIPTION_LOGIN_COMMAND}`; Application Default Credentials are not Antigravity subscription credentials"))?;
                Ok(Self::for_subscription(&auth, None))
            }
            Some(AuthMethod::GoogleAiPlan) => {
                let auth = PlanAuth::discover(&get, sender).await.map_err(|error| {
                    format!(
                        "--google-auth google-ai-plan: {error}\nhint: run `{PLAN_LOGIN_COMMAND}`"
                    )
                })?;
                Ok(Self::for_plan(&auth, None))
            }
            Some(AuthMethod::Gateway) => Self::gateway(&get)
                .ok_or_else(|| missing(AuthMethod::Gateway, "AGY_LLM_GATEWAY_URL")),
            Some(AuthMethod::ApiKey) => Self::api_key(&get)
                .ok_or_else(|| missing(AuthMethod::ApiKey, "GEMINI_API_KEY or GOOGLE_API_KEY")),
            Some(AuthMethod::VertexExpress) => Self::vertex_express(&get).ok_or_else(|| {
                missing(
                    AuthMethod::VertexExpress,
                    "GOOGLE_API_KEY (a Vertex AI express mode key)",
                )
            }),
            Some(AuthMethod::Vertex) => Self::vertex(config, &get, sender, model)
                .await
                .map_err(|error| format!("--google-auth vertex: {error}")),
            None => {
                if let Some(endpoint) = Self::gateway(&get) {
                    return Ok(endpoint);
                }
                if enabled(get("GOOGLE_GENAI_USE_VERTEXAI")) {
                    let project =
                        present(config.project.clone()).or_else(|| get("GOOGLE_CLOUD_PROJECT"));
                    if project.is_none()
                        && let Some(endpoint) = Self::vertex_express(&get)
                    {
                        return Ok(endpoint);
                    }
                    return Self::vertex(config, &get, sender, model).await.map_err(|error| {
                        format!(
                            "GOOGLE_GENAI_USE_VERTEXAI is set but Vertex AI is unavailable: {error}"
                        )
                    });
                }
                if let Some(endpoint) = Self::api_key(&get) {
                    return Ok(endpoint);
                }
                Self::vertex(config, &get, sender, model).await.map_err(|error| {
                    format!(
                        "the antigravity harness found no Google credentials; tried AGY_LLM_GATEWAY_URL (unset), GOOGLE_GENAI_USE_VERTEXAI (unset), GEMINI_API_KEY and GOOGLE_API_KEY (unset), then Application Default Credentials for Vertex AI: {error}"
                    )
                })
            }
        }
    }

    pub fn hint(&self, failure: Failure, model: Option<&str>) -> Option<String> {
        match self.route {
            Route::Antigravity { .. } => return subscription_hint(failure, model),
            Route::GoogleAiPlan => return plan_hint(failure, model),
            Route::GeminiApi | Route::Gateway | Route::Vertex { .. } | Route::VertexExpress => {}
        }
        let model = model.unwrap_or("this model");
        let project = match &self.route {
            Route::Vertex { project, .. } => Some(project.as_str()),
            Route::GeminiApi
            | Route::Gateway
            | Route::VertexExpress
            | Route::Antigravity { .. }
            | Route::GoogleAiPlan => None,
        };
        let location = match &self.route {
            Route::Vertex { location, .. } => Some(location.as_str()),
            Route::GeminiApi
            | Route::Gateway
            | Route::VertexExpress
            | Route::Antigravity { .. }
            | Route::GoogleAiPlan => None,
        };
        let elsewhere = format!(
            "run `{SETUP_COMMAND}` to find or set up a project that works, or pass --google-project"
        );
        match (failure, project) {
            (Failure::ServiceDisabled, Some(project)) => Some(format!(
                "Vertex AI (aiplatform.googleapis.com) is not enabled in project {project}: run `{SETUP_COMMAND}`, or enable it with `gcloud services enable aiplatform.googleapis.com --project {project}` (just after enabling, Google can take a few minutes to accept calls)"
            )),
            (Failure::ServiceDisabled, None) => Some(
                "the Gemini API (generativelanguage.googleapis.com) is not enabled for this key's project: enable it in Google AI Studio or the Cloud console".to_owned(),
            ),
            (Failure::BillingDisabled, Some(project)) => Some(format!(
                "project {project} has no active billing account: link one in the Cloud console, or {elsewhere}"
            )),
            (Failure::BillingDisabled, None) => Some(
                "this key's project has no active billing account: enable billing in Google AI Studio".to_owned(),
            ),
            (Failure::PermissionDenied, Some(project)) => Some(format!(
                "your Google credentials cannot call Vertex AI in project {project} (they need roles/aiplatform.user): {elsewhere}"
            )),
            (Failure::ModelNotFound, Some(_)) if location != Some(VERTEX_GLOBAL_LOCATION) => Some(format!(
                "{model} is not served in {}: try --google-location global",
                location.unwrap_or_default()
            )),
            (Failure::ModelNotFound, _) => Some(format!(
                "{model} is not available here: check the name with --model"
            )),
            (Failure::FreeTier, None) => Some(format!(
                "{model} needs a paid-tier Gemini API key: enable billing for the key's project in Google AI Studio, or use Vertex AI with `{SETUP_COMMAND}`"
            )),
            (Failure::RateLimited, _) => Some(
                "Gemini kept rate limiting after nanocode's retries: wait a minute and try again, or raise the project's quota".to_owned(),
            ),
            (Failure::Reauth, _) => Some(reauth_hint()),
            (Failure::FreeTier | Failure::PermissionDenied | Failure::Other, _) => None,
        }
    }

    fn request_hint(&self, status: u16, body: &str, path: &str, request: &str) -> Option<String> {
        let failure = classify(status, body);
        if matches!(self.route, Route::Antigravity { .. } | Route::GoogleAiPlan) {
            let request = serde_json::from_str::<Value>(request).ok();
            let model = request
                .as_ref()
                .and_then(|value| value.get("model"))
                .and_then(Value::as_str);
            return self.hint(
                if status == 401 {
                    Failure::Reauth
                } else {
                    failure
                },
                model,
            );
        }
        let model = path
            .split("/models/")
            .nth(1)
            .and_then(|rest| rest.split(':').next());
        self.hint(failure, model)
    }

    pub fn model_path(&self, model: &str, method: &str) -> String {
        match &self.route {
            Route::GeminiApi | Route::Gateway => format!("/v1beta/models/{model}:{method}"),
            Route::Vertex { project, location } => format!(
                "/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:{method}"
            ),
            Route::VertexExpress => format!("/v1/publishers/google/models/{model}:{method}"),
            Route::Antigravity { .. } => format!("/v1internal:{method}"),
            Route::GoogleAiPlan => plan::PLAN_PATH.to_owned(),
        }
    }

    pub fn cache_collection(&self) -> Option<String> {
        match &self.route {
            Route::GeminiApi | Route::Gateway => Some("/v1beta/cachedContents".to_owned()),
            Route::Vertex { project, location } => Some(format!(
                "/v1/projects/{project}/locations/{location}/cachedContents"
            )),
            Route::VertexExpress | Route::Antigravity { .. } | Route::GoogleAiPlan => None,
        }
    }

    pub fn cache_model(&self, model: &str) -> String {
        match &self.route {
            Route::Vertex { project, location } => {
                format!("projects/{project}/locations/{location}/publishers/google/models/{model}")
            }
            Route::GeminiApi
            | Route::Gateway
            | Route::VertexExpress
            | Route::Antigravity { .. }
            | Route::GoogleAiPlan => {
                format!("models/{model}")
            }
        }
    }

    pub fn resource_path(&self, name: &str) -> String {
        match self.route.is_vertex() {
            true => format!("/v1/{name}"),
            false => format!("/v1beta/{name}"),
        }
    }

    async fn authorized_headers(&self) -> Result<(HeaderMap, Option<String>), ClientError> {
        let mut headers = self.headers();
        let token = match &self.auth {
            Auth::Subscription(auth) => {
                Some(auth.token().await.map_err(ClientError::SubscriptionAuth)?)
            }
            Auth::Plan(auth) => Some(auth.token().await.map_err(ClientError::PlanAuth)?),
            Auth::ApiKey(_) | Auth::Bearer(_) | Auth::Google(_) | Auth::None => None,
        };
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                ClientError::SubscriptionAuth("invalid access token header".to_owned())
            })?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("authorization"), value);
            return Ok((headers, Some(token)));
        }
        let Auth::Google(auth) = &self.auth else {
            return Ok((headers, None));
        };
        let token = auth
            .token()
            .await
            .map_err(|error| ClientError::Auth(error.to_string()))?;
        let mut put = |name: &'static str, value: &str| {
            if let Ok(mut value) = HeaderValue::from_str(value) {
                value.set_sensitive(true);
                headers.insert(HeaderName::from_static(name), value);
            }
        };
        put("authorization", &format!("Bearer {token}"));
        if let Some(project) = auth.user_project() {
            put("x-goog-user-project", project);
        }
        Ok((headers, Some(token)))
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let mut put = |name: &'static str, value: &str| {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        };
        put("user-agent", &self.user_agent);
        put("content-type", "application/json");
        if self.route == Route::Gateway {
            put("x-goog-ext-525006001-bin", GATEWAY_TIER);
        }
        put("accept-encoding", "gzip");
        if self.route == Route::GoogleAiPlan {
            put("accept", "text/event-stream");
        }
        match &self.auth {
            Auth::ApiKey(key) => put("x-goog-api-key", key),
            Auth::Bearer(token) => put("authorization", &format!("Bearer {token}")),
            Auth::Google(_) | Auth::Subscription(_) | Auth::Plan(_) | Auth::None => {}
        }
        headers
    }
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    sender: Arc<dyn Sender>,
    retry: RetryPolicy,
    subscription_session: Option<String>,
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

impl Client {
    pub fn new(endpoint: Endpoint, sender: Arc<dyn Sender>) -> Self {
        let subscription_session = matches!(endpoint.route, Route::Antigravity { .. }).then(|| {
            let session_identity = "";
            let hash = session_identity
                .bytes()
                .fold(0xcbf29ce484222325_u64, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
                });
            (hash as i64).to_string()
        });
        Self {
            endpoint,
            sender,
            retry: RetryPolicy::default(),
            subscription_session,
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

    #[cfg(not(target_family = "wasm"))]
    pub fn native(endpoint: Endpoint) -> Result<Self, ClientError> {
        Ok(Self::new(endpoint, Self::native_sender()?))
    }

    #[must_use]
    pub const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint.base_url)
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        body: &str,
    ) -> Result<nanocode_http::layer::Response, Attempt> {
        let (headers, token) = self
            .endpoint
            .authorized_headers()
            .await
            .map_err(Attempt::Fatal)?;
        let response = self
            .sender
            .send(Request {
                method,
                url: self.url(path),
                headers,
                body: body.as_bytes().to_vec().into(),
            })
            .await
            .map_err(|error| Attempt::Retry(ClientError::Transport(error.detail().to_owned())))?;
        let status = response.status;
        if status.is_success() {
            return Ok(response);
        }
        let refreshed = match (&self.endpoint.auth, &token) {
            (Auth::Google(auth), Some(token)) if status.as_u16() == 401 => {
                auth.invalidate(token);
                true
            }
            (Auth::Subscription(auth), Some(token)) if status.as_u16() == 401 => {
                auth.invalidate(token);
                true
            }
            (Auth::Plan(auth), Some(token)) if status.as_u16() == 401 => {
                auth.invalidate(token);
                true
            }
            _ => false,
        };
        let body_request = body;
        let body = collect(response.body).await.unwrap_or_default();
        let body = String::from_utf8_lossy(&body).into_owned();
        let failure = classify(status.as_u16(), &body);
        let final_limit = self.endpoint.route == Route::GoogleAiPlan && daily_limit(&body);
        let retry = refreshed
            || (failure != Failure::FreeTier
                && !final_limit
                && matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504));
        let hint = self
            .endpoint
            .request_hint(status.as_u16(), &body, path, body_request);
        let error = ClientError::Status {
            status: status.as_u16(),
            body,
            hint,
        };
        Err(match retry {
            true => Attempt::Retry(error),
            false => Attempt::Fatal(error),
        })
    }

    fn service_disabled_backoff(&self) -> &'static [Duration] {
        match self.endpoint.route {
            Route::Antigravity { .. } | Route::GoogleAiPlan => &[],
            _ => self.retry.service_disabled,
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: &str,
    ) -> Result<nanocode_http::layer::Response, ClientError> {
        let mut delay = self.retry.initial;
        let mut attempt = 0;
        let mut disabled = self.service_disabled_backoff().iter();
        loop {
            match self.send_once(method.clone(), path, body).await {
                Ok(response) => return Ok(response),
                Err(Attempt::Fatal(error)) => match disabled.next() {
                    Some(wait) if error.failure() == Failure::ServiceDisabled => {
                        nanocode_platform::sleep(*wait).await;
                    }
                    _ => return Err(error),
                },
                Err(Attempt::Retry(error)) if attempt >= self.retry.max_retries => {
                    return Err(error);
                }
                Err(Attempt::Retry(_)) => {
                    nanocode_platform::sleep(delay).await;
                    delay = delay.saturating_mul(self.retry.multiplier);
                    attempt += 1;
                }
            }
        }
    }

    pub async fn json(
        &self,
        method: Method,
        path: &str,
        body: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let response = self.send(method, path, body).await?;
        let bytes = collect(response.body)
            .await
            .map_err(|error| ClientError::Transport(error.detail().to_owned()))?;
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Decode(error.to_string()))
    }

    fn request_body<'a>(&self, model: &str, body: &'a str) -> Result<Cow<'a, str>, ClientError> {
        let project = match &self.endpoint.route {
            Route::Antigravity { project } => project,
            Route::GoogleAiPlan => return plan::request(model, body).map(Cow::Owned),
            Route::GeminiApi | Route::Gateway | Route::Vertex { .. } | Route::VertexExpress => {
                return Ok(Cow::Borrowed(body));
            }
        };
        if !model.starts_with("gemini-") {
            return Err(ClientError::InvalidRequest(format!(
                "the subscription route currently supports Gemini models only, not {model}; choose an exact Gemini model ID available to your Antigravity account"
            )));
        }
        let mut request: serde_json::Map<String, Value> = serde_json::from_str(body)
            .map_err(|error| ClientError::InvalidRequest(error.to_string()))?;
        request.remove("serviceAccount");
        if model.starts_with("gemini-2")
            && request
                .get("generationConfig")
                .and_then(|config| config.get("thinkingConfig"))
                .is_some_and(|config| config.get("thinkingLevel").is_some())
        {
            return Err(ClientError::InvalidRequest(
                "Gemini 2 models require a numeric thinkingBudget, not thinkingLevel; supply a compatible generationConfig or choose a Gemini 3 or later model".to_owned(),
            ));
        }
        if request.contains_key("cachedContent") {
            return Err(ClientError::InvalidRequest(
                "Antigravity subscription requests do not support explicit cachedContent"
                    .to_owned(),
            ));
        }
        let session = self
            .subscription_session
            .as_ref()
            .expect("subscription session");
        request
            .entry("sessionId")
            .or_insert_with(|| Value::String(session.clone()));
        let labels = request.get("labels");
        let trajectory = labels
            .and_then(|labels| labels.get("trajectory_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let request_id = if let Some(trajectory) = trajectory {
            let step = labels
                .and_then(|labels| labels.get("last_step_index"))
                .and_then(Value::as_str)
                .and_then(|index| index.parse::<i32>().ok())
                .and_then(|index| index.checked_add(1))
                .filter(|index| *index >= 0)
                .ok_or_else(|| {
                    ClientError::InvalidRequest(
                        "Antigravity trajectory labels require a valid last_step_index".to_owned(),
                    )
                })?;
            let millis = nanocode_platform::unix_millis();
            match labels
                .and_then(|labels| labels.get("cascade_id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                Some(conversation) => format!("agent/{conversation}/{millis}/{trajectory}/{step}"),
                None => format!("agent/{millis}/{trajectory}/{step}"),
            }
        } else {
            format!("agent/{}", nanocode_platform::uuid_v4())
        };
        let mut body = serde_json::json!({
            "model": model,
            "request": request,
            "userAgent": "antigravity",
            "requestType": "agent",
            "requestId": request_id,
        });
        if !project.is_empty() {
            body["project"] = Value::String(project.clone());
        }
        Ok(Cow::Owned(body.to_string()))
    }

    pub async fn generate(&self, model: &str, body: &str) -> Result<Reply, ClientError> {
        if self.endpoint.route == Route::GoogleAiPlan {
            return self.stream(model, body, &mut |_| {}).await;
        }
        let body = self.request_body(model, body)?;
        let value = self
            .json(
                Method::POST,
                &self.endpoint.model_path(model, "generateContent"),
                &body,
            )
            .await?;
        let response = match self.endpoint.route {
            Route::Antigravity { .. } => serde_json::from_value::<SubscriptionResponse>(value)
                .map(|wrapped| wrapped.response),
            _ => serde_json::from_value::<GenerateResponse>(value),
        }
        .map_err(|error| ClientError::Decode(error.to_string()))?;
        let mut reply = Reply::default();
        let mut chunks = Vec::new();
        absorb(&mut reply, response, &mut chunks);
        reply.parts = merge_parts(chunks);
        Ok(reply)
    }

    pub async fn stream(
        &self,
        model: &str,
        body: &str,
        on_part: &mut (dyn FnMut(&Part) + Send),
    ) -> Result<Reply, ClientError> {
        let body = self.request_body(model, body)?;
        let path = self
            .endpoint
            .model_path(model, "streamGenerateContent?alt=sse");
        let mut delay = self.retry.initial;
        let mut attempt = 0;
        let mut disabled = self.service_disabled_backoff().iter();
        loop {
            match self.stream_once(&path, &body, on_part).await {
                Ok(reply) => return Ok(reply),
                Err((false, error)) if error.failure() == Failure::ServiceDisabled => {
                    match disabled.next() {
                        Some(wait) => nanocode_platform::sleep(*wait).await,
                        None => return Err(error),
                    }
                }
                Err((false, error)) | Err((true, error)) if attempt >= self.retry.max_retries => {
                    return Err(error);
                }
                Err((false, error)) => return Err(error),
                Err((true, _)) => {
                    nanocode_platform::sleep(delay).await;
                    delay = delay.saturating_mul(self.retry.multiplier);
                    attempt += 1;
                }
            }
        }
    }

    async fn stream_once(
        &self,
        path: &str,
        body: &str,
        on_part: &mut (dyn FnMut(&Part) + Send),
    ) -> Result<Reply, (bool, ClientError)> {
        let response = match self.send_once(Method::POST, path, body).await {
            Ok(response) => response,
            Err(Attempt::Retry(error)) => return Err((true, error)),
            Err(Attempt::Fatal(error)) => return Err((false, error)),
        };
        if self.endpoint.route == Route::GoogleAiPlan {
            return plan_events(response.body, on_part).await;
        }
        let mut body = response.body;
        let mut decoder = SseDecoder::default();
        let mut reply = Reply::default();
        let mut chunks = Vec::new();
        let mut delivered = false;
        loop {
            let (events, done) = match body.next().await {
                Some(Ok(bytes)) => (decoder.push(&bytes), false),
                Some(Err(error)) => {
                    return Err((
                        !delivered,
                        ClientError::Transport(error.detail().to_owned()),
                    ));
                }
                None => (decoder.finish(), true),
            };
            for event in events {
                if event.trim().is_empty() || event.trim() == "[DONE]" {
                    continue;
                }
                let response = match self.endpoint.route {
                    Route::Antigravity { .. } => {
                        serde_json::from_str::<SubscriptionResponse>(&event)
                            .map(|wrapped| wrapped.response)
                    }
                    _ => serde_json::from_str::<GenerateResponse>(&event),
                }
                .map_err(|error| (false, ClientError::Decode(format!("{error}: {event}"))))?;
                let start = chunks.len();
                absorb(&mut reply, response, &mut chunks);
                delivered |= matches!(self.endpoint.route, Route::Antigravity { .. })
                    && reply.finish_reason.is_some();
                for part in &chunks[start..] {
                    delivered = true;
                    on_part(part);
                }
            }
            if done {
                break;
            }
        }
        reply.parts = merge_parts(chunks);
        if reply.parts.is_empty() && reply.finish_reason.is_none() {
            return Err((
                true,
                ClientError::Decode("stream ended without a candidate".to_owned()),
            ));
        }
        Ok(reply)
    }
}

async fn plan_events(
    mut body: nanocode_http::layer::Body,
    on_part: &mut (dyn FnMut(&Part) + Send),
) -> Result<Reply, (bool, ClientError)> {
    let mut decoder = SseDecoder::default();
    let mut stream = plan::Stream::default();
    let mut chunks = Vec::new();
    loop {
        let (events, done) = match body.next().await {
            Some(Ok(bytes)) => (decoder.push(&bytes), false),
            Some(Err(error)) => {
                return Err((
                    chunks.is_empty(),
                    ClientError::Transport(error.detail().to_owned()),
                ));
            }
            None => (decoder.finish(), true),
        };
        for event in events {
            if event.trim().is_empty() || event.trim() == "[DONE]" {
                continue;
            }
            for part in stream.event(&event).map_err(|error| (false, error))? {
                on_part(&part);
                chunks.push(part);
            }
        }
        if done {
            break;
        }
    }
    let Some((finish_reason, usage)) = stream.finish().map_err(|error| (false, error))? else {
        return Err((
            chunks.is_empty(),
            ClientError::Decode(
                "the Interactions stream ended before interaction.completed".to_owned(),
            ),
        ));
    };
    Ok(Reply {
        parts: merge_parts(chunks),
        usage,
        finish_reason: Some(finish_reason),
        model_version: None,
        grounding: None,
    })
}

#[derive(serde::Deserialize)]
struct SubscriptionResponse {
    response: GenerateResponse,
}

enum Attempt {
    Retry(ClientError),
    Fatal(ClientError),
}

fn absorb(reply: &mut Reply, response: GenerateResponse, chunks: &mut Vec<Part>) {
    if let Some(usage) = response.usage_metadata {
        reply.usage = usage;
    }
    if response.model_version.is_some() {
        reply.model_version = response.model_version;
    }
    for candidate in response.candidates.into_iter().take(1) {
        if let Some(content) = candidate.content {
            chunks.extend(content.parts);
        }
        if candidate.finish_reason.is_some() {
            reply.finish_reason = candidate.finish_reason;
        }
        if candidate.grounding_metadata.is_some() {
            reply.grounding = candidate.grounding_metadata;
        }
    }
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

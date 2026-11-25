use crate::http::{Auth, Client, ClientError, Endpoint, Failure, RetryPolicy, Route};
use nanocode_google_auth::GoogleAuth;
use nanocode_http::Method;
use nanocode_http::layer::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const VERTEX_SERVICE: &str = "aiplatform.googleapis.com";
pub const RESOURCE_MANAGER_BASE_URL: &str = "https://cloudresourcemanager.googleapis.com";
pub const SERVICE_USAGE_BASE_URL: &str = "https://serviceusage.googleapis.com";
pub const RESOURCE_MANAGER_OVERRIDE_ENV: &str =
    "CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDRESOURCEMANAGER";
pub const SERVICE_USAGE_OVERRIDE_ENV: &str = "CLOUDSDK_API_ENDPOINT_OVERRIDES_SERVICEUSAGE";
pub const VERTEX_BASE_URL_ENV: &str = "GOOGLE_VERTEX_BASE_URL";
pub const STATE_FILE: &str = "antigravity.json";
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_ATTEMPTS: usize = 3;
const PROBE_GAP: Duration = Duration::from_secs(2);
const PROBED_PROJECTS_MAX: usize = 64;
const PROJECT_PAGE_SIZE: u32 = 500;
const PROJECT_PAGES_MAX: usize = 20;
const ENABLE_POLL: Duration = Duration::from_secs(3);
const ENABLE_DEADLINE: Duration = Duration::from_secs(300);
const PROBE_BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"ping"}]}]}"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Ready,
    Disabled,
    NoBilling,
    Denied,
    ModelMissing,
    Reauth,
    Failed(String),
}

impl Probe {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn rules_out(&self) -> bool {
        matches!(self, Self::Disabled | Self::NoBilling | Self::Denied)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Ready => "Vertex AI answers".to_owned(),
            Self::Disabled => format!("{VERTEX_SERVICE} is not enabled"),
            Self::NoBilling => "no billing account".to_owned(),
            Self::Denied => "no permission to call Vertex AI".to_owned(),
            Self::ModelMissing => "the model is not served in this location".to_owned(),
            Self::Reauth => "the Google sign-in needs to be renewed".to_owned(),
            Self::Failed(detail) => format!("probe failed: {detail}"),
        }
    }

    pub fn from_error(error: &ClientError) -> Self {
        match error.failure() {
            Failure::ServiceDisabled => Self::Disabled,
            Failure::BillingDisabled => Self::NoBilling,
            Failure::PermissionDenied => Self::Denied,
            Failure::ModelNotFound => Self::ModelMissing,
            Failure::Reauth => Self::Reauth,
            Failure::FreeTier | Failure::RateLimited | Failure::Other => Self::Failed(
                error
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub chosen: Option<Project>,
    pub probed: Vec<(Project, Probe)>,
}

#[derive(Clone)]
pub struct Projects {
    auth: GoogleAuth,
    sender: Arc<dyn Sender>,
    vertex_base: Option<String>,
    resource_manager: String,
    service_usage: String,
}

fn base(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: &str) -> String {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned()
}

impl Projects {
    pub fn new(
        auth: GoogleAuth,
        sender: Arc<dyn Sender>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        Self {
            vertex_base: lookup(VERTEX_BASE_URL_ENV)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            resource_manager: base(
                lookup,
                RESOURCE_MANAGER_OVERRIDE_ENV,
                RESOURCE_MANAGER_BASE_URL,
            ),
            service_usage: base(lookup, SERVICE_USAGE_OVERRIDE_ENV, SERVICE_USAGE_BASE_URL),
            auth,
            sender,
        }
    }

    pub const fn auth(&self) -> &GoogleAuth {
        &self.auth
    }

    pub fn endpoint(&self, project: &str, location: &str) -> Endpoint {
        Endpoint::for_vertex(&self.auth, self.vertex_base.as_deref(), project, location)
    }

    fn api(&self, base: &str, auth: GoogleAuth) -> Client {
        let mut endpoint = self.endpoint("", crate::http::VERTEX_GLOBAL_LOCATION);
        base.clone_into(&mut endpoint.base_url);
        endpoint.auth = Auth::Google(auth);
        endpoint.route = Route::VertexExpress;
        Client::new(endpoint, Arc::clone(&self.sender)).with_retry(RetryPolicy::NONE)
    }

    pub async fn list(&self) -> Result<Vec<Project>, ClientError> {
        let client = self.api(&self.resource_manager, self.auth.clone());
        let mut projects = Vec::new();
        let mut token = String::new();
        for _ in 0..PROJECT_PAGES_MAX {
            let mut path =
                format!("/v1/projects?filter=lifecycleState%3AACTIVE&pageSize={PROJECT_PAGE_SIZE}");
            if !token.is_empty() {
                path.push_str("&pageToken=");
                path.push_str(&token);
            }
            let page = client.json(Method::GET, &path, "").await?;
            projects.extend(
                page.get("projects")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|project| {
                        project
                            .get("lifecycleState")
                            .and_then(Value::as_str)
                            .is_none_or(|state| state == "ACTIVE")
                    })
                    .filter_map(|project| {
                        let id = project.get("projectId").and_then(Value::as_str)?;
                        Some(Project {
                            id: id.to_owned(),
                            name: project
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(id)
                                .to_owned(),
                        })
                    }),
            );
            match page.get("nextPageToken").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => next.clone_into(&mut token),
                _ => break,
            }
        }
        Ok(projects)
    }

    pub async fn call(
        &self,
        project: &str,
        location: &str,
        model: &str,
    ) -> Result<(), ClientError> {
        let client = Client::new(self.endpoint(project, location), Arc::clone(&self.sender))
            .with_retry(RetryPolicy::NONE);
        let path = client.endpoint().model_path(model, "countTokens");
        match nanocode_platform::timeout(
            PROBE_TIMEOUT,
            client.json(Method::POST, &path, PROBE_BODY),
        )
        .await
        {
            Ok(result) => result.map(drop),
            Err(_) => Err(ClientError::Transport(format!(
                "no answer within {PROBE_TIMEOUT:?}"
            ))),
        }
    }

    pub async fn probe(&self, project: &str, location: &str, model: &str) -> Probe {
        let mut last = Probe::Failed("not probed".to_owned());
        for attempt in 0..PROBE_ATTEMPTS {
            if attempt > 0 {
                nanocode_platform::sleep(PROBE_GAP).await;
            }
            last = match self.call(project, location, model).await {
                Ok(()) => return Probe::Ready,
                Err(error) => Probe::from_error(&error),
            };
            if last != Probe::Disabled {
                return last;
            }
        }
        last
    }

    pub async fn probe_all(&self, projects: &[Project], location: &str, model: &str) -> Vec<Probe> {
        nanocode_sync::join_in_order(
            projects
                .iter()
                .map(|project| self.probe(&project.id, location, model)),
        )
        .await
    }

    pub async fn discover(
        &self,
        location: &str,
        model: &str,
        skip: Option<&str>,
    ) -> Result<Discovery, ClientError> {
        let mut projects = self.list().await?;
        projects.retain(|project| Some(project.id.as_str()) != skip);
        projects.truncate(PROBED_PROJECTS_MAX);
        let probes = self.probe_all(&projects, location, model).await;
        let probed = projects.into_iter().zip(probes).collect::<Vec<_>>();
        let chosen = probed
            .iter()
            .find(|(_, probe)| probe.is_ready())
            .map(|(project, _)| project.clone());
        Ok(Discovery { chosen, probed })
    }

    pub async fn enable(&self, project: &str) -> Result<(), ClientError> {
        let client = self.api(
            &self.service_usage,
            self.auth.clone().with_user_project(project.to_owned()),
        );
        let mut operation = client
            .json(
                Method::POST,
                &format!("/v1/projects/{project}/services/{VERTEX_SERVICE}:enable"),
                "{}",
            )
            .await?;
        let started = nanocode_platform::now();
        loop {
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                return match operation.get("error") {
                    Some(error) => Err(ClientError::Status {
                        status: error
                            .get("code")
                            .and_then(Value::as_u64)
                            .and_then(|code| u16::try_from(code).ok())
                            .unwrap_or_default(),
                        body: serde_json::json!({ "error": error }).to_string(),
                        hint: None,
                    }),
                    None => Ok(()),
                };
            }
            let Some(name) = operation.get("name").and_then(Value::as_str) else {
                return Ok(());
            };
            if started.elapsed() > ENABLE_DEADLINE {
                return Err(ClientError::Transport(format!(
                    "enabling {VERTEX_SERVICE} in {project} did not finish within {ENABLE_DEADLINE:?} (operation {name})"
                )));
            }
            let path = format!("/v1/{name}");
            nanocode_platform::sleep(ENABLE_POLL).await;
            operation = client.json(Method::GET, &path, "").await?;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub project: String,
    pub location: String,
    pub model: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    vertex: BTreeMap<String, Saved>,
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub const fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    pub fn locate(configured: Option<PathBuf>, lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let path = configured.or_else(|| {
            nanocode_home::AgentHome::from_vars(|name| lookup(name).map(Into::into))
                .ok()
                .map(|home| home.nanocode_home().join(STATE_FILE))
        });
        Self { path }
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    async fn read(&self) -> StateFile {
        let Some(path) = self.path.clone() else {
            return StateFile::default();
        };
        read_bytes(path)
            .await
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub async fn load(&self, identity: &str) -> Option<Saved> {
        self.read()
            .await
            .vertex
            .remove(identity)
            .filter(|saved| !saved.project.is_empty())
    }

    pub async fn save(&self, identity: &str, saved: Saved) -> Result<String, String> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| "nanocode has no home directory to save it in".to_owned())?;
        let mut state = self.read().await;
        state.vertex.insert(identity.to_owned(), saved);
        let raw = serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?;
        write_bytes(path.clone(), raw).await?;
        Ok(path.display().to_string())
    }
}

#[cfg(not(target_family = "wasm"))]
async fn read_bytes(path: PathBuf) -> Result<Vec<u8>, String> {
    nanocode_platform::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[cfg(target_family = "wasm")]
async fn read_bytes(_path: PathBuf) -> Result<Vec<u8>, String> {
    Err("files are not readable in WASM".to_owned())
}

#[cfg(not(target_family = "wasm"))]
async fn write_bytes(path: PathBuf, raw: Vec<u8>) -> Result<(), String> {
    nanocode_platform::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let staged = path.with_extension("json.tmp");
        std::fs::write(&staged, raw)?;
        std::fs::rename(&staged, &path)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[cfg(target_family = "wasm")]
async fn write_bytes(_path: PathBuf, _raw: Vec<u8>) -> Result<(), String> {
    Err("files are not writable in WASM".to_owned())
}

use crate::prompts;
use nanocode_agent_config::configuration::{AgentResult, RuntimeBackend, RuntimeRequest};
use nanocode_agent_core::{
    BackendId, HarnessFamily, ModelSelection, NanocodeError, Result, Thinking,
    backend::BackendFuture,
};
use nanocode_backend_kimi_client::{Client, DEFAULT_BASE_URL, Endpoint, RetryPolicy, Sender};
use nanocode_durability::CheckpointFormat;
use std::sync::Arc;

pub const FAMILY: &str = "kimi";
pub const DEFAULT_MODEL: &str = "k3";

pub type Clock = Arc<dyn Fn() -> String + Send + Sync>;

#[derive(Clone, Default)]
pub struct Environment {
    pub os: Option<String>,
    pub shell: Option<(String, String)>,
    pub cwd: Option<String>,
    pub cwd_listing: Option<String>,
    pub agents_md: Option<String>,
    pub timezone: Option<String>,
    pub today: Option<Clock>,
}

#[derive(Clone, Default)]
pub struct KimiHarness {
    endpoint: Option<Endpoint>,
    sender: Option<Arc<dyn Sender>>,
    retry: Option<RetryPolicy>,
    model: Option<String>,
    context_window: Option<u64>,
    environment: Environment,
    direct_tools: bool,
    host: Option<crate::tools::Host>,
}

impl KimiHarness {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    #[must_use]
    pub fn sender(mut self, sender: Arc<dyn Sender>) -> Self {
        self.sender = Some(sender);
        self
    }

    #[must_use]
    pub const fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }

    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub const fn context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    #[must_use]
    pub const fn direct_tools(mut self, direct: bool) -> Self {
        self.direct_tools = direct;
        self
    }

    /// Runs Kimi Code's built-in tools over a host's files and shell instead of the local machine, which is
    /// how a wasm agent gets them (Codex's and Claude's `host_tools` serve the same purpose). Both must see
    /// the runtime workspace; the shell is POSIX and finds `rg` for Glob and Grep. FetchURL stays native.
    #[must_use]
    pub fn host(
        mut self,
        files: Arc<dyn nanocode_process_contracts::FileHost>,
        shell: Arc<dyn nanocode_process_contracts::ForegroundExecutor>,
    ) -> Self {
        self.host = Some(crate::tools::Host { files, shell });
        self
    }

    #[must_use]
    pub fn environment(mut self, environment: Environment) -> Self {
        self.environment = environment;
        self
    }

    pub fn selection(model: impl Into<String>) -> ModelSelection {
        ModelSelection::new(HarnessFamily::new(FAMILY), model)
    }
}

pub fn effort(thinking: Thinking) -> Result<&'static str> {
    match thinking {
        Thinking::Low => Ok("low"),
        Thinking::High => Ok("high"),
        Thinking::Max => Ok("max"),
        other => Err(NanocodeError::InvalidRequest(format!(
            "the kimi harness runs K3, which always reasons at effort low, high or max, not {other:?}"
        ))),
    }
}

impl RuntimeBackend for KimiHarness {
    fn id(&self) -> BackendId {
        BackendId::new(FAMILY)
    }

    fn is_harness(&self) -> bool {
        true
    }

    fn default_model(&self) -> ModelSelection {
        Self::selection(
            self.model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        )
    }

    fn default_thinking(&self) -> Option<Thinking> {
        Some(Thinking::High)
    }

    fn validate_model(&self, model: &str) -> Result<()> {
        match model.trim().is_empty() {
            true => Err(NanocodeError::InvalidRequest(
                "the kimi harness needs a Kimi model ID, such as k3".to_owned(),
            )),
            false => Ok(()),
        }
    }

    fn checkpoint_format(&self) -> Option<Arc<dyn CheckpointFormat>> {
        Some(Arc::new(crate::durability::KimiCheckpointFormat))
    }

    fn build(&self, request: RuntimeRequest) -> BackendFuture<AgentResult> {
        let harness = self.clone();
        Box::pin(async move { harness.assemble(request).await })
    }
}

impl KimiHarness {
    async fn assemble(&self, request: RuntimeRequest) -> AgentResult {
        use crate::driver::{Config, Driver};
        use crate::nanotools::NanoTools;
        use nanocode_agent_config::configuration::DelegationAuthority;
        use nanocode_agent_core::{AgentHandle, backend::BackendRuntime};
        use nanocode_tool_contracts::ToolCollection as _;
        use nanocode_tool_host::{Tools, runtime::ToolRuntime};
        let invalid = |error: String| NanocodeError::InvalidRequest(error);
        let RuntimeRequest {
            request: spec,
            settings,
            defaults,
            workspace,
            subagents,
            services,
        } = request;
        let settings = settings.or(&defaults);
        let model = spec.model.clone();
        let effort = effort(spec.thinking)?;
        let session = spec
            .options
            .session_id
            .clone()
            .or_else(|| {
                spec.durable_state
                    .as_ref()
                    .map(|state| state.state_id().to_owned())
            })
            .unwrap_or_else(|| nanocode_platform::uuid_v4().to_string());
        let sender = match self.sender.clone() {
            Some(sender) => sender,
            None => native_sender()?,
        };
        let endpoint = self.endpoint.clone().unwrap_or_else(|| Endpoint {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: std::env::var("KIMI_API_KEY").ok(),
            user_agent: concat!("nanocode/", env!("CARGO_PKG_VERSION")).to_owned(),
        });
        let mut client = Client::new(endpoint, sender);
        if let Some(retry) = self.retry {
            client = client.with_retry(retry);
        }
        let window = match self.context_window {
            Some(window) => window,
            None => client
                .context_length(model.as_str())
                .await
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| {
                    invalid(format!(
                        "Kimi does not list model {:?} in its /models catalog",
                        model.as_str()
                    ))
                })?,
        };
        let root = spec.parent.as_ref().map_or_else(
            || session.clone(),
            |parent| parent.root_session_id().to_owned(),
        );
        let handle = AgentHandle::new(session.clone(), model.clone(), spec.spawn_factory.clone())
            .with_root_session_id(root)
            .with_spawn_factory(spec.spawn_factory.clone());
        // As in Codex, no selection means `Tools::default()`: the workspace tools (here Kimi Code's own) and
        // web access are on. An explicit workspace source or a code-mode host brings its own workspace tools.
        let selected = match &settings.tools {
            Some(tools) => tools
                .materialize(handle.clone())
                .map_err(|error| invalid(error.to_string()))?,
            None => Tools::default(),
        };
        let local = self.host.is_none();
        let kimi = kimi_workspace(&selected, !local);
        // Kimi Code's session stores (spilled results, media) sit where its tools can Read them back: on
        // the host when they run there.
        let spill = match kimi && !local {
            true => Some(
                std::path::PathBuf::from("/tmp/nanocode-kimi")
                    .join(&session)
                    .join("tool-results"),
            ),
            false => spill_directory(&session),
        };
        let builtins = kimi.then(|| {
            Arc::new(crate::tools::Builtins::new(
                match local {
                    true => std::fs::canonicalize(&workspace)
                        .unwrap_or_else(|_| workspace.to_path_buf()),
                    false => workspace.to_path_buf(),
                },
                home(),
                spill.as_ref().map(|spill| spill.with_file_name("media")),
                selected.web_search_enabled() && !cfg!(target_family = "wasm"),
                self.host.clone(),
            ))
        });
        let selected = match &builtins {
            Some(builtins) => selected
                .with_builtin_names(builtins.names())
                .map_err(|error| invalid(error.to_string()))?,
            None => selected,
        };
        let selected = services
            .prepare_tools(&handle, selected, None)
            .map_err(|error| invalid(error.to_string()))?;
        let mut additive = services.shared_tools(false).await?;
        if let Some(subagents) = &subagents {
            additive.extend(match self.direct_tools {
                true => subagents.direct_tools(handle.clone(), DelegationAuthority::Turn),
                false => subagents.tools(handle.clone(), DelegationAuthority::Turn),
            });
        }
        let selected = selected.add_shared_tools(additive)?;
        let selected = match self.direct_tools {
            true => selected
                .into_builder()
                .exposure(nanocode_tool_host::ToolExposure::DirectOnly)
                .web_search(false)
                .image_generation(false)
                .build()
                .map_err(|error| invalid(error.to_string()))?,
            false => selected,
        };
        let runtime = ToolRuntime::new_with_tools(workspace.to_path_buf(), Vec::new(), &selected);
        let mut tools = NanoTools::new(runtime, &session).map_err(invalid)?;
        let mut entries = std::mem::take(&mut tools.entries);
        if let Some(builtins) = &builtins {
            entries.extend(crate::nanotools::builtin_entries(builtins));
        }
        let catalog = crate::nanotools::Catalog::new(entries);
        let given = &self.environment;
        let environment = prompts::Environment {
            os: given.os.clone().unwrap_or_else(|| prompts::os_name(OS)),
            shell: given.shell.clone().or_else(default_shell),
            cwd: given
                .cwd
                .clone()
                .unwrap_or_else(|| workspace.display().to_string()),
            cwd_listing: given
                .cwd_listing
                .clone()
                .unwrap_or_else(|| prompts::listing(&workspace)),
            agents_md: given
                .agents_md
                .clone()
                .unwrap_or_else(|| prompts::agents_md(home().as_deref(), &workspace)),
            role_additional: settings
                .instructions
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_default(),
        };
        let zone = given
            .timezone
            .clone()
            .or_else(nanocode_platform::time_zone_name)
            .unwrap_or_else(|| "UTC".to_owned());
        let today = given.today.clone().unwrap_or_else(|| {
            Arc::new(|| crate::driver::local_date(nanocode_platform::system_time()))
        });
        let (owner, stored) = match &spec.durable_state {
            Some(state) => {
                let (owner, stored) = state
                    .acquire_agent_with_initial(nanocode_durability::InitialSettings {
                        thinking: spec.thinking,
                        service_tier: nanocode_agent_core::ServiceTier::Standard,
                    })
                    .await
                    .map_err(nanocode_durability::agent_error)?;
                (Some(owner), stored)
            }
            None => (None, None),
        };
        let checkpoint = spec.checkpoint.clone();
        let root = spec.parent.is_none();
        let (runtime, events) = BackendRuntime::new(session.clone());
        let driver = Driver::new(Config {
            client,
            model,
            thinking: spec.thinking,
            effort,
            window,
            system: prompts::system(&environment, builtins.is_some()),
            tools,
            catalog,
            builtins,
            root,
            spill,
            zone,
            today,
            session,
            owner,
        });
        let driver = match (checkpoint, stored) {
            (Some(checkpoint), _) => driver.restore(&checkpoint)?,
            (None, Some(stored)) => driver.restore_payload(
                &stored
                    .decode::<serde_json::Value>()
                    .map_err(nanocode_durability::agent_error)?,
            )?,
            (None, None) => driver,
        };
        Ok((runtime.bind(driver), events))
    }
}

#[cfg(not(target_family = "wasm"))]
const OS: &str = std::env::consts::OS;

#[cfg(target_family = "wasm")]
const OS: &str = "linux";

#[cfg(not(target_family = "wasm"))]
fn native_sender() -> Result<Arc<dyn Sender>> {
    Client::native_sender().map_err(|error| NanocodeError::InvalidRequest(error.to_string()))
}

#[cfg(target_family = "wasm")]
fn native_sender() -> Result<Arc<dyn Sender>> {
    Err(NanocodeError::InvalidRequest(
        "the kimi harness on wasm needs a host sender: KimiHarness::sender".into(),
    ))
}

#[cfg(not(target_family = "wasm"))]
fn default_shell() -> Option<(String, String)> {
    std::path::Path::new("/bin/bash")
        .exists()
        .then(|| ("bash".to_owned(), "/bin/bash".to_owned()))
}

#[cfg(target_family = "wasm")]
const fn default_shell() -> Option<(String, String)> {
    None
}

/// Where results over Kimi Code's 50 000-character limit are saved for the model to Read back; none on
/// wasm, where Kimi Code's could-not-save preview applies.
#[cfg(not(target_family = "wasm"))]
fn spill_directory(session: &str) -> Option<std::path::PathBuf> {
    Some(
        std::env::temp_dir()
            .join("nanocode-kimi")
            .join(session)
            .join("tool-results"),
    )
}

#[cfg(target_family = "wasm")]
const fn spill_directory(_session: &str) -> Option<std::path::PathBuf> {
    None
}

/// Whether Kimi Code's built-in tools serve this selection's workspace, by Codex's rule for its own: the
/// workspace switch is on and no Code Mode host or explicit workspace source brings other workspace tools.
/// They run locally, or over a host (`KimiHarness::host`), the only way on wasm.
fn kimi_workspace(tools: &nanocode_tool_host::Tools, hosted: bool) -> bool {
    tools.workspace_enabled()
        && !tools.has_code_mode_host()
        && no_workspace_source(tools)
        && (hosted || !cfg!(target_family = "wasm"))
}

#[cfg(not(target_family = "wasm"))]
const fn no_workspace_source(tools: &nanocode_tool_host::Tools) -> bool {
    tools.workspace_source().is_none()
}

#[cfg(target_family = "wasm")]
const fn no_workspace_source(_: &nanocode_tool_host::Tools) -> bool {
    true
}

#[cfg(not(target_family = "wasm"))]
fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(target_family = "wasm")]
const fn home() -> Option<std::path::PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use nanocode_tool_host::Tools;

    #[test]
    fn selection_drives_kimi_builtins() {
        let builtins = |web| crate::tools::Builtins::new("/w".into(), None, None, web, None);
        let none = Tools::builder().without_defaults().build().unwrap();
        let workspace = Tools::builder()
            .without_defaults()
            .workspace(true)
            .build()
            .unwrap();
        assert!(super::kimi_workspace(&Tools::default(), false));
        assert!(super::kimi_workspace(&Tools::default(), true));
        assert!(!super::kimi_workspace(&none, true));
        assert!(super::kimi_workspace(&workspace, false));
        assert!(builtins(true).handles("FetchURL") && builtins(true).handles("Bash"));
        assert!(!builtins(false).handles("FetchURL") && builtins(false).handles("Write"));
        assert_eq!(builtins(false).names().count(), 12);
        struct Noop;
        impl nanocode_tool_host::FunctionHandler for Noop {
            fn call<'a>(
                &'a self,
                _: &'a str,
                _: serde_json::Value,
                _: nanocode_tool_host::ToolContext<'a>,
            ) -> nanocode_tool_host::embedded::HostFuture<'a, Result<serde_json::Value, String>>
            {
                Box::pin(async { Ok(serde_json::Value::Null) })
            }
        }
        let read = nanocode_tool_host::FunctionTool::new(
            "Read",
            "",
            serde_json::json!({"type": "object"}),
            std::sync::Arc::new(Noop),
        );
        let reserved = Tools::default()
            .with_builtin_names(builtins(true).names())
            .unwrap();
        assert!(reserved.into_builder().tool(read).build().is_err());
    }
}

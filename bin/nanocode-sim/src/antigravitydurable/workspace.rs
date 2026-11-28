use super::*;
use nanocode::{Agent, ModelSelection, agent::SpawnOptions};
use nanocode_process_contracts::{
    ExecCommand, ExecCommandResult, ProcessFuture, ShellExecutor, WriteStdin,
};
use std::path::Path;

const RUNTIME: &str = "/";
const OVERRIDE: &str = "/nanocode-sim/antigravity-workspace";
const FIXTURE: &str = "workspace.txt";
const COMMAND: &str = "cat workspace.txt";
const FALLBACK_TEXT: &str = "runtime fixture contents";
const OVERRIDE_TEXT: &str = "antigravity fixture contents";
const READ: &str = "Read workspace.txt";

struct ProbeShell;

impl ShellExecutor for ProbeShell {
    fn execute<'a>(
        &'a self,
        command: ExecCommand,
        workspace: &'a Path,
    ) -> ProcessFuture<'a, ExecCommandResult> {
        Box::pin(async move {
            if command.script != COMMAND || command.workdir.as_deref() != workspace.to_str() {
                return refused();
            }
            let target = workspace.join(FIXTURE);
            let runtime = Path::new(RUNTIME).join(FIXTURE);
            let antigravity = Path::new(OVERRIDE).join(FIXTURE);
            let output = match target {
                path if path == runtime => FALLBACK_TEXT,
                path if path == antigravity => OVERRIDE_TEXT,
                _ => return refused(),
            };
            ExecCommandResult {
                error: None,
                exit_code: Some(0),
                output: output.to_owned(),
                chunk_id: None,
                wall_time_seconds: 0.0,
                session_id: None,
                original_token_count: None,
                time_left_seconds: None,
            }
        })
    }

    fn write_stdin(&self, request: WriteStdin) -> ProcessFuture<'_, ExecCommandResult> {
        NoShell.write_stdin(request)
    }

    fn terminate_all(&self) -> ProcessFuture<'_, ()> {
        NoShell.terminate_all()
    }
}

#[derive(Default)]
struct ProbeModel(AtomicU64);

impl nanocode_http::layer::Sender for ProbeModel {
    fn send(
        &self,
        request: nanocode_http::layer::Request,
    ) -> nanocode_http::layer::BoxFuture<
        '_,
        Result<nanocode_http::layer::Response, nanocode_http::layer::Error>,
    > {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::AcqRel);
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|error| nanocode_http::layer::Error::new(error.to_string()))?;
            let last = body["contents"].as_array().and_then(|parts| parts.last());
            let last = last.ok_or_else(|| nanocode_http::layer::Error::new("no content"))?;
            let content = last.to_string();
            let advertised = body["systemInstruction"]["parts"][0]["text"]
                .as_str()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("Command Working Directory: "))
                });
            let parts = if content.contains(READ) {
                vec![json!({"functionCall": {"name": "run_command", "args": {
                    "CommandLine": COMMAND,
                    "toolSummary": "Read workspace fixture",
                    "WaitMsBeforeAsync": 500,
                }}})]
            } else {
                let text = [(RUNTIME, FALLBACK_TEXT), (OVERRIDE, OVERRIDE_TEXT)]
                    .into_iter()
                    .find(|(path, text)| advertised == Some(*path) && content.contains(text))
                    .map(|(_, text)| text)
                    .ok_or_else(|| {
                        nanocode_http::layer::Error::new(
                            "command fixture output does not match the advertised directory",
                        )
                    })?;
                vec![json!({"text": text})]
            };
            Ok(model_response(parts, true))
        })
    }
}

struct NoCodexInference;

impl nanocode::http::Layer for NoCodexInference {
    fn send<'a>(
        &'a self,
        _request: nanocode::http::Request,
        _next: nanocode::http::Next<'a>,
    ) -> nanocode::http::BoxFuture<'a, Result<nanocode::http::Response, nanocode::http::Error>>
    {
        Box::pin(async { Err(nanocode::http::Error::new("unexpected Codex model request")) })
    }
}

fn runtime_for(
    model: Arc<ProbeModel>,
    store: SessionStore,
    override_path: bool,
) -> Result<Nanocode, String> {
    let codex = nanocode::OpenAi::builder(nanocode::codex::OpenAiAuth::api_key("sk-sim"))
        .api_base_url("http://model.sim")
        .transport(nanocode::codex::transport::ResponsesTransport::Https)
        .http_layer(NoCodexInference)
        .build()
        .map_err(|error| error.to_string())?;
    let antigravity = harness(model).shell(Arc::new(ProbeShell));
    let antigravity = if override_path {
        antigravity.workspace(OVERRIDE)
    } else {
        antigravity
    };
    Nanocode::builder()
        .tools(
            Tools::builder()
                .without_defaults()
                .build()
                .map_err(|error| error.to_string())?,
        )
        .runner_management(false)
        .workspace(RUNTIME)
        .durability(store)
        .openai(nanocode::CodexHarness::new(codex).headless(true))
        .google(antigravity)
        .for_backend::<Gemini>()
        .model(MODEL)
        .build()
        .map_err(|error| error.to_string())
}

async fn check(agent: &Agent, path: &str, expected: &str) -> Result<(), String> {
    let result = agent
        .prompt(PromptRequest::new(READ))
        .await
        .map_err(|error| format!("{path} admission: {error}"))?
        .result()
        .await
        .map_err(|error| format!("{path} turn: {error}"))?;
    if result.final_message != expected {
        return Err(format!(
            "{path} read {:?}, expected {expected:?}",
            result.final_message
        ));
    }
    Ok(())
}

async fn run(model: Arc<ProbeModel>, media: &Media) -> Result<(), String> {
    let (store, _) = media.open(Store::Kv).await?;
    let original = runtime_for(Arc::clone(&model), store.clone(), false)?;
    let (original, _) = original
        .spawn()
        .session_id(SESSION)
        .build()
        .await
        .map_err(|error| error.to_string())?;
    check(&original, "runtime fallback", FALLBACK_TEXT).await?;
    original
        .disconnect()
        .await
        .map_err(|error| error.to_string())?;

    let runtime = runtime_for(model, store, true)?;
    let (restored, _) = runtime
        .open_session(SESSION)
        .await
        .map_err(|error| error.to_string())?;
    check(&restored, "restored", OVERRIDE_TEXT).await?;
    let (fresh, _) = runtime
        .spawn()
        .build()
        .await
        .map_err(|error| error.to_string())?;
    check(&fresh, "fresh", OVERRIDE_TEXT).await?;
    let (native_child, _) = fresh.spawn().await.map_err(|error| error.to_string())?;
    check(&native_child, "same-harness child", OVERRIDE_TEXT).await?;

    let (codex, _) = runtime
        .spawn()
        .harness_model(ModelSelection::Codex(nanocode::codex::Model::Luna))
        .build()
        .await
        .map_err(|error| error.to_string())?;
    let (routed_child, _) = codex
        .spawn_with(SpawnOptions::new().harness_model(AntigravityHarness::selection(MODEL)))
        .await
        .map_err(|error| error.to_string())?;
    check(&routed_child, "cross-harness child", OVERRIDE_TEXT).await
}

pub(super) fn simulate(seed: u64) -> (u64, Vec<String>) {
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let profile = Profile {
        before_ppm: 0,
        after_ppm: 0,
        torn_ppm: 0,
        read_ppm: 0,
        durability: Durability::OnAck,
        ..Profile::of(seed)
    };
    let media = Media {
        kv: SimKv::new(
            seed,
            profile,
            StoreMutation::None,
            false,
            Arc::clone(&clock),
        ),
        disk: Disk::new(),
    };
    let model = Arc::new(ProbeModel::default());
    let (ended, result) = epoch(
        seed,
        &clock,
        "antigravity workspace",
        run(Arc::clone(&model), &media),
    );
    let wrong = match (ended, result) {
        (Ended::Finished, Some(Ok(()))) => Vec::new(),
        (Ended::Finished, Some(Err(error))) => vec![error],
        (ended, _) => vec![format!("workspace ended {ended:?} without a result")],
    };
    (model.0.load(Ordering::Acquire), wrong)
}

mod plan;
mod subscription;

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use nanocode::antigravity::{
    AuthMethod, Client, DEFAULT_MODEL, Endpoint, GoogleAuth, GoogleConfig, Probe, Project,
    Projects, RetryPolicy, Route, Saved, Store, VERTEX_GLOBAL_LOCATION, VERTEX_SERVICE,
};
use nanocode_http::Method;

use nanocode_sync::mpsc::{UnboundedReceiver, UnboundedSender};
use nanocode_tui::transcript::NoticeLevel;

use crate::{
    app::AppEvent,
    cli::{LoginArgs, SetupArgs, SetupCommand},
    host::Result,
};

const GCLOUD: &str = "gcloud";
const ADC_LOGIN: [&str; 3] = ["auth", "application-default", "login"];
const SETTLED_SUCCESSES: usize = 3;
const SETTLE_ATTEMPTS: usize = 60;
const SETTLE_GAP: Duration = Duration::from_secs(5);
const COUNT_TOKENS_BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"ping"}]}]}"#;
const GENERATE_BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"ping"}]}],"generationConfig":{"maxOutputTokens":1}}"#;
const SIGN_IN_PAGE: &str = "Google Cloud sign-in";
const API_KEY_HELP: &str = "To use a Gemini API key instead, create one at https://aistudio.google.com/apikey and run GEMINI_API_KEY=... nanocode -m gemini-4-neon (gemini-4-neon needs a paid-tier key: enable billing for the key's project in AI Studio).";

pub(crate) enum Report {
    Terminal,
    Notices {
        events: UnboundedSender<AppEvent>,
        codes: Option<(UnboundedSender<String>, UnboundedReceiver<String>)>,
    },
}

impl Report {
    fn say(&self, text: impl Into<String>) {
        match self {
            Self::Terminal => eprintln!("{}", text.into()),
            Self::Notices { events, .. } => {
                drop(events.send(AppEvent::Notice(NoticeLevel::Info, text.into())));
            }
        }
    }

    fn release_codes(&mut self) {
        if let Self::Notices { codes, .. } = self {
            codes.take();
        }
    }
}

fn lookup(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub(crate) fn login_config(mut config: GoogleConfig) -> GoogleConfig {
    if config.method.is_none() {
        let key = lookup("GEMINI_API_KEY").or_else(|| lookup("GOOGLE_API_KEY"));
        config.method = Some(if lookup("AGY_LLM_GATEWAY_URL").is_some() {
            AuthMethod::Gateway
        } else if lookup("GOOGLE_GENAI_USE_VERTEXAI").is_some_and(|value| {
            matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }) {
            if config.project.is_none() && lookup("GOOGLE_CLOUD_PROJECT").is_none() && key.is_some()
            {
                AuthMethod::VertexExpress
            } else {
                AuthMethod::Vertex
            }
        } else if key.is_some() {
            AuthMethod::ApiKey
        } else if config.project.is_some()
            || config.location.is_some()
            || [
                "GOOGLE_CLOUD_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GOOGLE_OAUTH_ACCESS_TOKEN",
            ]
            .iter()
            .any(|name| lookup(name).is_some())
        {
            AuthMethod::Vertex
        } else {
            AuthMethod::Antigravity
        });
    }
    config
}

pub(crate) async fn login(args: LoginArgs) -> Result<()> {
    let config = login_config(args.google.config());
    if config.method == Some(AuthMethod::GoogleAiPlan) {
        plan::login(&mut Report::Terminal, !args.no_browser).await?;
        eprintln!(
            "Signed in to the Google AI plan. Run: nanocode -m {DEFAULT_MODEL} --google-auth google-ai-plan"
        );
        return Ok(());
    }
    if args.no_browser && config.method != Some(AuthMethod::Antigravity) {
        return Err(
            "--no-browser requires subscription login; use nanocode login antigravity --no-browser"
                .into(),
        );
    }
    if config.method == Some(AuthMethod::Antigravity) {
        subscription::login(&mut Report::Terminal, !args.no_browser).await?;
        eprintln!(
            "Signed in to Antigravity. Run: nanocode -m {DEFAULT_MODEL}\nUse --google-auth antigravity to override configured Google API keys, gateways or Vertex settings."
        );
    } else {
        google(&config, DEFAULT_MODEL, &mut Report::Terminal).await?;
        eprintln!("Done. Run: nanocode -m {DEFAULT_MODEL} --google-auth vertex");
    }
    Ok(())
}

pub async fn setup(args: SetupArgs) -> Result<()> {
    match args.command {
        SetupCommand::Google {
            google,
            model,
            check,
            no_browser,
        } => {
            let config = google.config();
            let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_owned());
            if no_browser
                && !matches!(
                    config.method,
                    Some(AuthMethod::Antigravity | AuthMethod::GoogleAiPlan)
                )
            {
                return Err(
                    "--no-browser requires --google-auth antigravity or google-ai-plan".into(),
                );
            }
            if !check && config.method == Some(AuthMethod::GoogleAiPlan) {
                plan::login(&mut Report::Terminal, !no_browser).await?;
                eprintln!(
                    "Done. Run: nanocode -m {model} --google-auth google-ai-plan\nCheck with nanocode setup google --google-auth google-ai-plan --check."
                );
                return Ok(());
            }
            match check {
                true => self::check(&config, &model).await,
                false => {
                    if no_browser {
                        subscription::login(&mut Report::Terminal, false).await?;
                    } else {
                        self::google(&config, &model, &mut Report::Terminal).await?;
                    }
                    if config.method == Some(AuthMethod::Antigravity) {
                        eprintln!(
                            "Done. Run: nanocode -m {model}\nUse --google-auth antigravity to override configured Google API keys, gateways or Vertex settings.\nCheck with nanocode setup google --google-auth antigravity --check. Subscription access is separate from Vertex AI billing."
                        );
                    } else {
                        eprintln!(
                            "Done. Run: nanocode -m {model}\nOverride with --google-project or GOOGLE_CLOUD_PROJECT; check with nanocode setup google --check."
                        );
                    }
                    Ok(())
                }
            }
        }
    }
}

fn on_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

async fn ask(question: String) -> Option<String> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    nanocode_platform::spawn_blocking(move || {
        eprint!("{question} ");
        drop(std::io::stderr().flush());
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        Some(line.trim().to_owned())
    })
    .await
    .ok()
    .flatten()
}

async fn confirm(question: String) -> bool {
    ask(format!("{question} [y/N]"))
        .await
        .is_some_and(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

async fn adc_login(gcloud: Option<&std::path::Path>, report: &mut Report) -> Result<()> {
    let Some(gcloud) = gcloud.map(std::path::Path::to_path_buf) else {
        return Err(format!(
            "gcloud is not installed: install the Google Cloud CLI (https://cloud.google.com/sdk/docs/install), then run {GCLOUD} {}, or use a Gemini API key. {API_KEY_HELP}",
            ADC_LOGIN.join(" ")
        )
        .into());
    };
    report.say(format!("Running {GCLOUD} {} ...", ADC_LOGIN.join(" ")));
    if let Report::Notices { events, codes } = report {
        let Some(codes) = codes.take() else {
            return Err("the Google sign-in has no way to take a verification code".into());
        };
        let events = events.clone();
        return nanocode_platform::spawn_blocking(move || piped_adc_login(&gcloud, &events, codes))
            .await?;
    }
    let status = nanocode_platform::spawn_blocking(move || {
        std::process::Command::new(gcloud).args(ADC_LOGIN).status()
    })
    .await??;
    match status.success() {
        true => Ok(()),
        false => Err(format!("{GCLOUD} {} failed ({status})", ADC_LOGIN.join(" ")).into()),
    }
}

fn piped_adc_login(
    gcloud: &std::path::Path,
    events: &UnboundedSender<AppEvent>,
    (code_holder, mut codes): (UnboundedSender<String>, UnboundedReceiver<String>),
) -> Result<()> {
    use std::io::BufRead;
    use std::process::Stdio;
    let mut child = std::process::Command::new(gcloud)
        .args(ADC_LOGIN)
        .arg("--launch-browser")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(format!("{GCLOUD} started without its pipes").into());
    };
    let read = |pipe: Box<dyn std::io::Read + Send>| {
        let mut console = false;
        let mut errors = Vec::new();
        for line in std::io::BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.starts_with("Go to the following link") {
                console = true;
            } else if line.starts_with("Your browser has been opened") {
                console = false;
            } else if line.starts_with("https://") {
                let text = match console {
                    true => format!(
                        "{}, then paste the verification code it shows with /login vertex CODE",
                        crate::login::browse(SIGN_IN_PAGE, line)
                    ),
                    false => format!(
                        "opened {SIGN_IN_PAGE} in your browser; if it did not appear, visit {line}"
                    ),
                };
                drop(events.send(AppEvent::Notice(NoticeLevel::Info, text)));
            } else if line.starts_with("ERROR") {
                errors.push(line.to_owned());
            }
        }
        errors
    };
    let (status, errors) = std::thread::scope(move |scope| {
        scope.spawn(move || {
            while let Some(code) = codes.blocking_recv() {
                if writeln!(stdin, "{}", code.trim())
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let out = scope.spawn(move || read(Box::new(stdout)));
        let err = scope.spawn(move || read(Box::new(stderr)));
        let status = child.wait();
        drop(code_holder);
        let mut errors = out.join().unwrap_or_default();
        errors.extend(err.join().unwrap_or_default());
        (status, errors)
    });
    let status = status?;
    match status.success() {
        true => Ok(()),
        false => Err(format!(
            "{GCLOUD} {} failed ({status}){}{}; to sign in outside nanocode, run it in a terminal, then /login vertex again",
            ADC_LOGIN.join(" "),
            match errors.is_empty() {
                true => "",
                false => ": ",
            },
            errors.join("; ")
        )
        .into()),
    }
}

async fn credentials(
    sender: &Arc<dyn nanocode_http::layer::Sender>,
    gcloud: Option<&std::path::Path>,
    report: &mut Report,
) -> Result<GoogleAuth> {
    let found = GoogleAuth::discover(&lookup, Arc::clone(sender)).await;
    let failure = match &found {
        Ok(auth) => match auth.token().await {
            Ok(_) => None,
            Err(error) => Some(format!(
                "the {} credentials from {} could not get a token: {error}",
                auth.kind().name(),
                auth.origin()
            )),
        },
        Err(error) => Some(format!("no Application Default Credentials: {error}")),
    };
    let Some(failure) = failure else {
        return found.map_err(|error| error.to_string().into());
    };
    report.say(failure.clone());
    let relogin = match &found {
        Ok(auth) => auth.kind().is_user(),
        Err(_) => true,
    };
    if !relogin {
        return Err(failure.into());
    }
    adc_login(gcloud, report).await?;
    let auth = GoogleAuth::discover(&lookup, Arc::clone(sender))
        .await
        .map_err(|error| error.to_string())?;
    auth.token().await.map_err(|error| error.to_string())?;
    Ok(auth)
}

async fn remember(
    store: &Store,
    auth: &GoogleAuth,
    project: &str,
    location: &str,
    model: &str,
    report: &Report,
) {
    let saved = Saved {
        project: project.to_owned(),
        location: location.to_owned(),
        model: model.to_owned(),
    };
    match store.save(&auth.identity(), saved).await {
        Ok(path) => report.say(format!("Saved {project} ({location}) in {path}.")),
        Err(error) => report.say(format!("Could not save the choice: {error}")),
    }
}

async fn settle(projects: &Projects, project: &str, location: &str, model: &str) -> bool {
    let mut streak = 0;
    for attempt in 0..SETTLE_ATTEMPTS {
        if attempt > 0 {
            nanocode_platform::sleep(SETTLE_GAP).await;
        }
        match projects.call(project, location, model).await {
            Ok(()) => streak += 1,
            Err(error) => {
                streak = 0;
                eprintln!(
                    "  not ready yet: {}",
                    error.to_string().lines().next().unwrap_or_default()
                );
            }
        }
        if streak >= SETTLED_SUCCESSES {
            return true;
        }
    }
    false
}

pub(crate) async fn google(
    config: &GoogleConfig,
    model: &str,
    report: &mut Report,
) -> Result<String> {
    if config.method == Some(AuthMethod::Antigravity) {
        return subscription::login(report, true).await;
    }
    if config.method == Some(AuthMethod::GoogleAiPlan) {
        return plan::login(report, true).await;
    }
    if let Some(method @ (AuthMethod::ApiKey | AuthMethod::VertexExpress | AuthMethod::Gateway)) =
        config.method
    {
        return Err(format!(
            "Google {} is explicitly configured and does not use browser sign-in. Keep that configuration to use it, or explicitly run nanocode login antigravity (or /login antigravity) for subscription access; no billing route was changed.",
            method.name()
        ).into());
    }
    let gcloud = on_path(GCLOUD);
    match &gcloud {
        Some(path) => report.say(format!("gcloud: {}", path.display())),
        None => report.say("gcloud: not found on PATH"),
    }
    let sender = Client::native_sender()?;
    let auth = credentials(&sender, gcloud.as_deref(), report).await;
    report.release_codes();
    let auth = auth?;
    report.say(format!(
        "Credentials: {} from {}",
        auth.kind().name(),
        auth.origin()
    ));
    let location = config
        .location
        .clone()
        .or_else(|| lookup("GOOGLE_CLOUD_LOCATION"))
        .unwrap_or_else(|| VERTEX_GLOBAL_LOCATION.to_owned());
    let projects = Projects::new(auth.clone(), Arc::clone(&sender), &lookup);
    let store = Store::locate(config.state.clone(), &lookup);
    let preferred = auth.project_source(config.project.as_deref(), &lookup);
    let mut preferred_probe = None;
    if let Some((project, source)) = &preferred {
        report.say(format!(
            "Probing {project} (from {}) with {model} in {location} ...",
            source.name()
        ));
        let probe = projects.probe(project, &location, model).await;
        report.say(format!("  {project}: {}", probe.describe()));
        if probe.is_ready() {
            remember(&store, &auth, project, &location, model, report).await;
            return Ok(project.clone());
        }
        preferred_probe = Some(Project {
            id: project.clone(),
            name: project.clone(),
        })
        .zip(Some(probe));
    }
    report.say("Listing your active Google Cloud projects and probing each ...");
    let skip = preferred.as_ref().map(|(project, _)| project.as_str());
    let discovery = projects.discover(&location, model, skip).await?;
    let mut probed = preferred_probe.into_iter().collect::<Vec<_>>();
    probed.extend(discovery.probed);
    report.say(
        probed
            .iter()
            .map(|(project, probe)| format!("  {:<32} {}", project.id, probe.describe()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if let Some(chosen) = discovery.chosen {
        report.say(format!("Picked {}.", chosen.id));
        remember(&store, &auth, &chosen.id, &location, model, report).await;
        return Ok(chosen.id);
    }
    let disabled: Vec<String> = probed
        .iter()
        .filter(|(_, probe)| *probe == Probe::Disabled)
        .map(|(project, _)| project.id.clone())
        .collect();
    if disabled.is_empty() {
        return Err(format!(
            "No project you can reach has Vertex AI ready, and none only needs {VERTEX_SERVICE} enabled (see the list above: fix billing or permissions in one, or pass --google-project). {API_KEY_HELP}"
        )
        .into());
    }
    if let Report::Notices { .. } = report {
        return Err(format!(
            "Vertex AI ({VERTEX_SERVICE}) is disabled in {}: enable it with gcloud services enable {VERTEX_SERVICE} --project PROJECT (Vertex AI calls bill to that project), or run nanocode setup google in a terminal to pick one, then /login vertex again. {API_KEY_HELP}",
            disabled.join(", ")
        )
        .into());
    }
    eprintln!("Vertex AI ({VERTEX_SERVICE}) is disabled in:");
    for (index, project) in disabled.iter().enumerate() {
        eprintln!("  {}. {project}", index + 1);
    }
    let choice = ask(format!(
        "Enable it in which project? [1-{}, Enter to skip]",
        disabled.len()
    ))
    .await
    .and_then(|answer| answer.parse::<usize>().ok())
    .and_then(|index| index.checked_sub(1))
    .and_then(|index| disabled.get(index).cloned());
    let Some(project) = choice else {
        return Err(format!(
            "Nothing enabled. Enable it yourself with: gcloud services enable {VERTEX_SERVICE} --project PROJECT, then rerun nanocode setup google. {API_KEY_HELP}"
        )
        .into());
    };
    if !confirm(format!(
        "Enable {VERTEX_SERVICE} in {project}? Vertex AI calls will bill to {project}'s billing account."
    ))
    .await
    {
        return Err(format!("Left {project} unchanged. {API_KEY_HELP}").into());
    }
    eprintln!("Enabling {VERTEX_SERVICE} in {project} ...");
    projects.enable(&project).await?;
    eprintln!(
        "Enabled. Waiting until {SETTLED_SUCCESSES} calls in a row succeed (Google can take a few minutes) ..."
    );
    if !settle(&projects, &project, &location, model).await {
        return Err(format!(
            "{VERTEX_SERVICE} is enabled in {project} but {model} still does not answer reliably; wait a few minutes and rerun nanocode setup google"
        )
        .into());
    }
    remember(&store, &auth, &project, &location, model, report).await;
    Ok(project)
}

async fn check(config: &GoogleConfig, model: &str) -> Result<()> {
    let sender = Client::native_sender()?;
    let endpoint = Endpoint::discover(config, &lookup, Arc::clone(&sender), model).await?;
    let method = endpoint.route.method();
    println!("auth:     {}", method.name());
    if let nanocode::antigravity::Auth::Google(auth) = &endpoint.auth {
        println!("creds:    {} from {}", auth.kind().name(), auth.origin());
    }
    match &endpoint.route {
        Route::Vertex { project, location } => {
            println!("project:  {project}");
            println!("location: {location}");
        }
        Route::Antigravity { project } => {
            println!("project:  {project} (subscription-managed, not a Vertex billing project)");
        }
        Route::GoogleAiPlan => {
            println!("plan:     Google AI plan per-user quota (Interactions API)");
        }
        Route::GeminiApi | Route::Gateway | Route::VertexExpress => {}
    }
    println!("base:     {}", endpoint.base_url);
    println!("model:    {model}");
    if let Some(notice) = &endpoint.notice {
        println!("note:     {notice}");
    }
    let client = Client::new(endpoint, sender).with_retry(RetryPolicy::NONE);
    let started = nanocode_platform::now();
    let (operation, result) =
        if matches!(method, AuthMethod::Antigravity | AuthMethod::GoogleAiPlan) {
            (
                "generateContent",
                client.generate(model, GENERATE_BODY).await.map(|_| ()),
            )
        } else {
            let path = client.endpoint().model_path(model, "countTokens");
            (
                "countTokens",
                client
                    .json(Method::POST, &path, COUNT_TOKENS_BODY)
                    .await
                    .map(|_| ()),
            )
        };
    match result {
        Ok(_) => {
            println!("probe:    ok ({operation} in {:?})", started.elapsed());
            Ok(())
        }
        Err(error) => {
            println!("probe:    failed");
            Err(error.to_string().into())
        }
    }
}

use nanocode::{
    claude::{ClaudeHomeLogin, subscription::ClaudeSubscriptionStatus},
    codex::auth::{ChatGptLogin, chatgpt_auth_file},
};
use nanocode_mpp::tempo_id::{self, DeviceLogin};
use nanocode_sync::mpsc::{UnboundedSender, unbounded_channel};
use nanocode_tui::transcript::NoticeLevel;

use crate::app::{App, AppEvent};

impl App {
    pub(crate) fn login(&mut self, argument: &str) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "login", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        let tx = self.tx.clone();
        let (provider, rest) = argument
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((argument.trim(), ""));
        match provider {
            "codex" | "chatgpt" | "openai" => {
                nanocode_platform::spawn_task(codex(tx));
            }
            "claude" | "anthropic" => {
                nanocode_platform::spawn_task(claude(tx));
            }
            "tempo" => {
                nanocode_platform::spawn_task(tempo(tx));
            }
            "antigravity" => self.login_google(
                rest.trim(),
                Some(nanocode::antigravity::AuthMethod::Antigravity),
            ),
            "google-ai-plan" => self.login_google(
                rest.trim(),
                Some(nanocode::antigravity::AuthMethod::GoogleAiPlan),
            ),
            "vertex" => {
                self.login_google(rest.trim(), Some(nanocode::antigravity::AuthMethod::Vertex))
            }
            "google" | "gemini" => {
                let (method, callback) = rest
                    .trim()
                    .split_once(char::is_whitespace)
                    .unwrap_or((rest.trim(), ""));
                match method {
                    "antigravity" => self.login_google(
                        callback.trim(),
                        Some(nanocode::antigravity::AuthMethod::Antigravity),
                    ),
                    "plan" | "google-ai-plan" => self.login_google(
                        callback.trim(),
                        Some(nanocode::antigravity::AuthMethod::GoogleAiPlan),
                    ),
                    "vertex" => self.login_google(
                        callback.trim(),
                        Some(nanocode::antigravity::AuthMethod::Vertex),
                    ),
                    _ => self.login_google(rest.trim(), None),
                }
            }
            _ => self.notice(
                NoticeLevel::Info,
                "usage: /login codex|claude|google|antigravity|google-ai-plan|vertex|tempo",
            ),
        }
    }

    fn login_google(&mut self, code: &str, method: Option<nanocode::antigravity::AuthMethod>) {
        let waiting = self
            .google_codes
            .as_ref()
            .and_then(nanocode_sync::mpsc::WeakUnboundedSender::upgrade);
        match (waiting, code.is_empty()) {
            (Some(codes), false) => {
                drop(codes.send(code.to_owned()));
                self.notice(NoticeLevel::Info, "sent the authorization response to the pending Google sign-in");
            }
            (Some(_), true) => self.notice(
                NoticeLevel::Info,
                "a Google sign-in is already running: finish it in the browser, or use the paste instructions shown when it started",
            ),
            (None, false) => self.notice(
                NoticeLevel::Info,
                "no Google sign-in is waiting for an authorization response; /login google or /login antigravity starts one",
            ),
            (None, true) => {
                let (codes, waiting) = unbounded_channel();
                self.google_codes = Some(codes.downgrade());
                let antigravity = self.model.family().as_str() == nanocode::antigravity::FAMILY;
                let model = match antigravity {
                    true => self.model.as_str().to_owned(),
                    false => nanocode::antigravity::DEFAULT_MODEL.to_owned(),
                };
                let next = match (antigravity, self.agent.is_none()) {
                    (true, true) => "/new starts a session that uses it".to_owned(),
                    (true, false) => "restart nanocode to use it in a new session".to_owned(),
                    (false, _) => format!("restart with nanocode -m {model} to use it"),
                };
                let mut config = self.host.config.google.clone();
                config.method = method.or(config.method);
                let config = crate::setup::login_config(config);
                let subscription = config.method == Some(nanocode::antigravity::AuthMethod::Antigravity);
                let next = if config.method == Some(nanocode::antigravity::AuthMethod::GoogleAiPlan) {
                    format!("restart with nanocode -m {model} --google-auth google-ai-plan to use it")
                } else if subscription {
                    format!("restart with nanocode -m {model} to use it; --google-auth antigravity overrides configured Google API keys, gateways or Vertex settings")
                } else if method == Some(nanocode::antigravity::AuthMethod::Vertex) {
                    format!("restart with nanocode -m {model} --google-auth vertex to use it")
                } else {
                    next
                };
                nanocode_platform::spawn_task(google(
                    self.tx.clone(),
                    config,
                    model,
                    next,
                    (codes, waiting),
                ));
            }
        }
    }

    pub(crate) fn fund(&mut self, argument: &str) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "fund", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        nanocode_platform::spawn_task(fund(
            self.tx.clone(),
            argument.trim().trim_start_matches('$').to_owned(),
        ));
    }

    pub(crate) fn pass(&mut self, argument: &str) {
        match self.host.config.clients.answering() {
            Some(answer) => {
                if let Some(reply) = answer(argument) {
                    self.notice(NoticeLevel::Info, format!("pass: {reply}"));
                }
            }
            None => self.notice(
                NoticeLevel::Info,
                "pass: no MPP service can ask for an email pass in this session",
            ),
        }
    }

    pub(crate) fn save_defaults(&mut self, argument: &str) {
        let words = argument
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let clients = self.host.config.clients.clone();
        let home = self.home.clone();
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let notice = match crate::defaults::run(&words, &clients, &home).await {
                Ok(report) => AppEvent::Notice(
                    NoticeLevel::Info,
                    format!("defaults for new sessions:\n{}", report.trim_end()),
                ),
                Err(error) => AppEvent::Notice(NoticeLevel::Error, format!("default: {error}")),
            };
            drop(tx.send(notice));
        });
    }

    pub(crate) fn account(&mut self) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "account", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        nanocode_platform::spawn_task(account(self.tx.clone()));
    }
}

async fn codex(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let path = match chatgpt_auth_file() {
        Ok(path) => path,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    let login = match ChatGptLogin::start(path).await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("ChatGPT sign-in", login.authorization_url()),
    );
    match login.complete().await {
        Ok(status) => notice(
            NoticeLevel::Info,
            format!(
                "signed in to ChatGPT as {}",
                status.email.unwrap_or(status.account_id)
            ),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login codex: {error}")),
    }
}

async fn claude(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let login = match ClaudeHomeLogin::start().await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login claude: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("Claude sign-in", login.authorization_url()),
    );
    match login.complete().await {
        Ok(ClaudeSubscriptionStatus::Authenticated { .. }) => {
            notice(NoticeLevel::Info, "signed in to Claude".to_owned());
        }
        Ok(status) => notice(
            NoticeLevel::Error,
            format!("login claude ended as {status:?}"),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login claude: {error}")),
    }
}

async fn google(
    tx: UnboundedSender<AppEvent>,
    config: nanocode::antigravity::GoogleConfig,
    model: String,
    next: String,
    codes: (
        UnboundedSender<String>,
        nanocode_sync::mpsc::UnboundedReceiver<String>,
    ),
) {
    let mut report = crate::setup::Report::Notices {
        events: tx.clone(),
        codes: Some(codes),
    };
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    match crate::setup::google(&config, &model, &mut report).await {
        Ok(account) => {
            let text = if config.method == Some(nanocode::antigravity::AuthMethod::GoogleAiPlan) {
                format!("signed in to the Google AI plan; {next}")
            } else if config.method == Some(nanocode::antigravity::AuthMethod::Antigravity) {
                format!("signed in to Antigravity; {next}")
            } else {
                format!(
                    "signed in to Google Cloud: Vertex AI project {account} serves {model}; {next}"
                )
            };
            notice(NoticeLevel::Info, text);
        }
        Err(error) => notice(NoticeLevel::Error, format!("login google: {error}")),
    }
}

const TEMPO_PAYS: &str = "it pays for the Tempo gateway's and Mercator's tools and the Tempo services shell commands reach, and --gateway tempo pays for models with it; /fund buys MACH by card";

async fn tempo(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let login = match DeviceLogin::start(nanocode_mpp::tx::CHAIN_ID).await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login tempo: {error}")),
    };
    notice(
        NoticeLevel::Info,
        format!(
            "{}; check that it shows {}",
            browse("Tempo ID sign-in", login.verification_url()),
            login.user_code()
        ),
    );
    match login.complete().await {
        Ok(account) => notice(
            NoticeLevel::Info,
            format!("signed in with Tempo ID as {account}; {TEMPO_PAYS}"),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login tempo: {error}")),
    }
}

async fn fund(tx: UnboundedSender<AppEvent>, amount: String) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let account = match nanocode_mpp::accounts::default_active_account() {
        Ok(account) => account,
        Err(error) => return notice(NoticeLevel::Error, format!("fund: {error}")),
    };
    let url = tempo_id::deposit_url(account, nanocode_mpp::tx::CHAIN_ID, Some(&amount));
    notice(
        NoticeLevel::Info,
        format!(
            "{}; MACH bought there goes to the Tempo account {account}, and nanocode pays with it once it arrives",
            browse("the Tempo ID deposit page", &url)
        ),
    );
}

async fn account(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let held = match nanocode_mpp::accounts::default_active_account() {
        Ok(account) => {
            format!("nanocode pays from the Tempo account {account}; /fund buys MACH for it")
        }
        Err(_) => "/login tempo signs nanocode in with Tempo ID".to_owned(),
    };
    notice(
        NoticeLevel::Info,
        format!("{}; {held}", browse("Tempo ID", tempo_id::TEMPO_ID_URL)),
    );
}

pub(crate) fn browse(page: &str, url: &str) -> String {
    if crate::extras::open_url(url) {
        format!("opened {page} in your browser; if it did not appear, visit {url}")
    } else {
        format!("visit {url} for {page}")
    }
}

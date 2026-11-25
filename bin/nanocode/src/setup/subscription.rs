use std::sync::Arc;
use std::time::Duration;

use nanocode::antigravity::{
    Client, SUBSCRIPTION_REDIRECT_URI, SubscriptionLogin, SubscriptionStore,
};

use super::{Report, ask, lookup};
use crate::host::Result;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WARNING: &str = "Warning: Google consumer subscription access through third-party clients is unsupported and can restrict Antigravity or Gemini CLI access for your account. Continue only if you accept that risk. This is not Vertex AI billing setup.";

pub(super) async fn login(report: &mut Report, open_browser: bool) -> Result<String> {
    report.say(WARNING);
    let login = SubscriptionLogin::new(SUBSCRIPTION_REDIRECT_URI.to_owned())?;
    let url = login.authorization_url()?;
    let opened = open_browser && crate::extras::open_url(&url);
    report.say(if opened {
        format!("opened Antigravity subscription sign-in in your browser; if it did not appear, visit {url}")
    } else {
        format!("visit {url} for Antigravity subscription sign-in")
    });
    let callback = receive(report).await;
    report.release_codes();
    let callback = callback?;
    let sender = Client::native_sender()?;
    let credentials = login.exchange(&callback, Arc::clone(&sender)).await?;
    let project = credentials.project.clone();
    let store = SubscriptionStore::locate(None, &lookup)?;
    store.save(&credentials).await?;
    report.say("Saved Antigravity subscription credentials separately from Google Cloud Application Default Credentials.");
    Ok(project)
}

async fn receive(report: &mut Report) -> Result<String> {
    match report {
        Report::Notices { codes, events } => {
            let Some((_, waiting)) = codes.as_mut() else {
                return Err("the subscription sign-in has no authorization code input channel".into());
            };
            let instruction = "Finish signing in in the browser, then paste the authorization code or full callback URL with /login google CODE (or /login antigravity CODE).";
            drop(events.send(crate::app::AppEvent::Notice(
                nanocode_tui::transcript::NoticeLevel::Info,
                instruction.to_owned(),
            )));
            nanocode_sync::select! {
                pasted = waiting.recv() => pasted.ok_or_else(|| "subscription sign-in authorization code input closed".into()),
                () = nanocode_platform::sleep(LOGIN_TIMEOUT) => Err("subscription sign-in timed out; run /login google again".into()),
            }
        }
        Report::Terminal => {
            ask("Paste the authorization code or full callback URL:".to_owned())
                .await
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "no subscription authorization code provided; run nanocode login google in a terminal or use /login google".into())
        }
    }
}

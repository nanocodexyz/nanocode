use std::sync::Arc;
use std::time::Duration;

use nanocode::antigravity::{
    Client, SUBSCRIPTION_REDIRECT_URI, SubscriptionLogin, SubscriptionStore,
};
use nanocode_executor::{TcpListener, TcpStream};
use nanocode_http::Url;
use nanocode_io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{Report, ask, lookup};
use crate::host::Result;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CALLBACK_BYTES_MAX: usize = 16 * 1024;
const WARNING: &str = "Warning: Google consumer subscription access through third-party clients is unsupported and can restrict Antigravity or Gemini CLI access for your account. Continue only if you accept that risk. This is not Vertex AI billing setup.";

pub(super) async fn login(report: &mut Report, open_browser: bool) -> Result<String> {
    report.say(WARNING);
    let login = SubscriptionLogin::new(SUBSCRIPTION_REDIRECT_URI.to_owned())?;
    let redirect = Url::parse(SUBSCRIPTION_REDIRECT_URI)?;
    let port = redirect.port().ok_or("subscription redirect has no port")?;
    let listener = if open_browser {
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                report.say(format!("Could not listen for the Google callback: {error}. Use the full callback URL paste fallback."));
                None
            }
        }
    } else {
        None
    };
    let url = login.authorization_url()?;
    let opened = open_browser && crate::extras::open_url(&url);
    report.say(if opened {
        format!("opened Antigravity subscription sign-in in your browser; if it did not appear, visit {url}")
    } else {
        format!("visit {url} for Antigravity subscription sign-in")
    });
    let listener = if !opened && matches!(report, Report::Terminal) {
        None
    } else {
        listener
    };
    let callback = receive(listener, &redirect, report).await;
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

async fn receive(
    listener: Option<TcpListener>,
    redirect: &Url,
    report: &mut Report,
) -> Result<String> {
    match report {
        Report::Notices { codes, events } => {
            let Some((_, waiting)) = codes.as_mut() else {
                return Err("the subscription sign-in has no callback input channel".into());
            };
            let callback = async {
                match listener {
                    Some(listener) => callback(&listener, redirect).await,
                    None => std::future::pending().await,
                }
            };
            let manual = waiting.recv();
            let instruction = "Finish in the browser, or paste the entire redirected URL (including code and state) with /login antigravity URL. Never paste only the code.";
            drop(events.send(crate::app::AppEvent::Notice(
                nanocode_tui::transcript::NoticeLevel::Info,
                instruction.to_owned(),
            )));
            nanocode_sync::select! {
                result = callback => result,
                pasted = manual => pasted.ok_or_else(|| "subscription sign-in callback input closed".into()),
                () = nanocode_platform::sleep(LOGIN_TIMEOUT) => Err("subscription sign-in timed out; run /login antigravity again".into()),
            }
        }
        Report::Terminal => {
            if let Some(listener) = listener {
                report.say("Waiting up to five minutes for the browser callback. For a remote browser, restart with nanocode setup google --google-auth antigravity --no-browser to paste the URL immediately.");
                match nanocode_platform::timeout(LOGIN_TIMEOUT, callback(&listener, redirect)).await {
                    Ok(result) => return result,
                    Err(_) => report.say("No callback received. Copy the full redirected URL from the browser address bar."),
                }
            }
            ask("Paste the full redirected URL (including code and state):".to_owned())
                .await
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "no subscription callback URL provided; run setup in a terminal or use /login antigravity".into())
        }
    }
}

async fn callback(listener: &TcpListener, redirect: &Url) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let target = request_target(&mut stream).await?;
        if !target.starts_with('/') || target.starts_with("//") {
            reply(&mut stream, "400 Bad Request", "Invalid callback target.").await?;
            return Err("subscription callback must use a local request target".into());
        }
        let callback = redirect.join(&target)?;
        if callback.path() != redirect.path() {
            reply(&mut stream, "404 Not Found", "Not found.").await?;
            continue;
        }
        reply(
            &mut stream,
            "200 OK",
            "Callback received. Return to nanocode to check whether sign-in succeeded.",
        )
        .await?;
        return Ok(callback.to_string());
    }
}

async fn request_target(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err("subscription callback ended before its headers".into());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > CALLBACK_BYTES_MAX {
            return Err("subscription callback headers are too large".into());
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    std::str::from_utf8(&bytes)?
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split_once(' '))
        .map(|(target, _)| target.to_owned())
        .ok_or_else(|| "subscription callback is not an HTTP GET request".into())
}

async fn reply(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

use std::net::Ipv4Addr;
use std::time::Duration;

use nanocode::antigravity::{
    Client, PLAN_CALLBACK_PATH, PLAN_LOGIN_COMMAND, PlanClient, PlanLogin, PlanStore,
};
use nanocode_executor::{TcpListener, TcpStream};
use nanocode_io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{Report, ask, lookup};
use crate::host::Result;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const SIGNED_IN: &str = "<!doctype html><title>nanocode</title><p>Signed in to the Google AI plan. You can close this tab and return to nanocode.</p>";
const FAILED: &str = "<!doctype html><title>nanocode</title><p>The Google AI plan sign-in failed. Return to nanocode for the reason.</p>";

enum Received {
    Browser(TcpStream, String),
    Pasted(String),
}

pub(super) async fn login(report: &mut Report, open_browser: bool) -> Result<String> {
    let client = PlanClient::from_env(&lookup)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("could not listen for the Google sign-in callback: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not listen for the Google sign-in callback: {error}"))?
        .port();
    let origin = format!("http://127.0.0.1:{port}");
    let login = PlanLogin::new(client, format!("{origin}{PLAN_CALLBACK_PATH}"))?;
    let url = login.authorization_url()?;
    let opened = open_browser && crate::extras::open_url(&url);
    report.say(if opened {
        format!(
            "opened the Google AI plan sign-in in your browser; if it did not appear, visit {url}"
        )
    } else {
        format!("visit {url} to sign in to the Google AI plan")
    });
    let received = receive(report, &listener, open_browser).await;
    report.release_codes();
    let (stream, callback) = match received? {
        Received::Browser(stream, target) => (Some(stream), format!("{origin}{target}")),
        Received::Pasted(callback) => (None, callback),
    };
    let result = login.exchange(&callback, Client::native_sender()?).await;
    if let Some(mut stream) = stream {
        reply(
            &mut stream,
            200,
            if result.is_ok() { SIGNED_IN } else { FAILED },
        )
        .await;
    }
    let credentials = result?;
    PlanStore::locate(None, &lookup)?.save(&credentials).await?;
    report.say("Saved the Google AI plan credentials separately from the Antigravity subscription and Google Cloud credentials.");
    Ok(credentials.client.client_id)
}

async fn receive(
    report: &mut Report,
    listener: &TcpListener,
    open_browser: bool,
) -> Result<Received> {
    let browser = async {
        callback(listener)
            .await
            .map(|(stream, target)| Received::Browser(stream, target))
            .map_err(|error| format!("the Google sign-in callback failed: {error}"))
    };
    let timeout = format!("the Google AI plan sign-in timed out; run {PLAN_LOGIN_COMMAND} again");
    match report {
        Report::Notices { codes, events } => {
            let Some((_, waiting)) = codes.as_mut() else {
                return Err("the Google AI plan sign-in has no callback input channel".into());
            };
            drop(events.send(crate::app::AppEvent::Notice(
                nanocode_tui::transcript::NoticeLevel::Info,
                "Finish signing in in the browser. If it runs on another machine, paste the URL it ends on with /login google-ai-plan URL.".to_owned(),
            )));
            nanocode_sync::select! {
                received = browser => Ok(received?),
                pasted = waiting.recv() => pasted
                    .map(Received::Pasted)
                    .ok_or_else(|| "the Google AI plan sign-in input closed".into()),
                () = nanocode_platform::sleep(LOGIN_TIMEOUT) => Err(timeout.into()),
            }
        }
        Report::Terminal if open_browser => nanocode_sync::select! {
            received = browser => Ok(received?),
            () = nanocode_platform::sleep(LOGIN_TIMEOUT) => Err(timeout.into()),
        },
        Report::Terminal => ask("Paste the URL the browser ended on after signing in:".to_owned())
            .await
            .filter(|value| !value.is_empty())
            .map(Received::Pasted)
            .ok_or_else(|| "no callback URL provided; run nanocode login google-ai-plan in a terminal or use /login google-ai-plan".into()),
    }
}

async fn callback(listener: &TcpListener) -> std::io::Result<(TcpStream, String)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut buffer = [0; 2048];
        while !request.windows(4).any(|window| window == b"\r\n\r\n")
            && request.len() < MAX_CALLBACK_BYTES
        {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let target = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default()
            .to_owned();
        if target.split('?').next() == Some(PLAN_CALLBACK_PATH) {
            return Ok((stream, target));
        }
        reply(&mut stream, 404, "").await;
    }
}

async fn reply(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    drop(stream.write_all(response.as_bytes()).await);
    drop(stream.shutdown().await);
}

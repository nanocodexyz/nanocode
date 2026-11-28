//! Kimi Code's FetchURL (`fetchUrlTool.ts` over `LocalFetchURLProvider`, the fetcher Kimi Code uses when
//! no Moonshot fetch service is configured and there is no Kimi OAuth login, as with an API key).
//! Like Kimi Code's local fetcher, it refuses only non-http(s) URLs.

use super::{Builtins, Outcome, input};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct FetchInput {
    url: String,
}

pub async fn fetch(_builtins: &Builtins, args: &Value) -> Outcome {
    let url = match input::<FetchInput>("FetchURL", args) {
        Ok(input) => input.url,
        Err(outcome) => return outcome,
    };
    match get(&url).await {
        Ok((content, _)) if content.is_empty() => Outcome::ok("The response body is empty."),
        Ok((content, passthrough)) => {
            let note = if passthrough {
                "The returned content is the full response body, returned verbatim."
            } else {
                "The returned content is the main text extracted from the page."
            };
            Outcome::ok(format!(
                "{note} If you use it in your answer, cite this page as a markdown link, e.g. [title](url).\n\n{content}"
            ))
        }
        Err(Failure::Status(status, message)) => {
            Outcome::error(format!("Failed to fetch URL. Status: {status}. {message}"))
        }
        Err(Failure::Other(message)) => Outcome::error(format!(
            "Failed to fetch URL due to network error: {url}. {message}"
        )),
    }
}

#[cfg_attr(target_family = "wasm", allow(dead_code))]
enum Failure {
    Status(u16, String),
    Other(String),
}

#[cfg(target_family = "wasm")]
async fn get(_url: &str) -> Result<(String, bool), Failure> {
    Err(Failure::Other("fetch failed".to_owned()))
}

#[cfg(not(target_family = "wasm"))]
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";
#[cfg(not(target_family = "wasm"))]
const MAX_BYTES: usize = 10_485_760;
#[cfg(not(target_family = "wasm"))]
const MAX_REDIRECT_HOPS: usize = 10;

/// `LocalFetchURLProvider.fetch`: the body and whether it is passed through verbatim.
#[cfg(not(target_family = "wasm"))]
async fn get(url: &str) -> Result<(String, bool), Failure> {
    use nanocode_http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION};
    use std::time::Duration;
    // Node's fetch rejects every transport failure with `TypeError: fetch failed`.
    let failed = |_| Failure::Other("fetch failed".to_owned());
    // Header values as `Headers.get` returns them (isomorphic decode).
    let text = |value: &HeaderValue| {
        value
            .as_bytes()
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>()
    };
    nanocode_tls::install_default_rustls_crypto_provider();
    // undici's defaults: 10 s to connect, 300 s for the headers and between body chunks.
    let client = nanocode_http::Client::builder()
        .redirect(nanocode_http::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .head_timeout(Duration::from_secs(300))
        .read_timeout(Duration::from_secs(300))
        .build()
        .map_err(failed)?;
    let mut current = url.to_owned();
    let mut redirects = 0;
    let response = loop {
        let target = validate(&current).map_err(Failure::Other)?;
        let response = client
            .get(target.clone())
            .header("accept-language", "*")
            .header("sec-fetch-mode", "cors")
            .send()
            .await
            .map_err(failed)?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            break response;
        }
        let Some(location) = response.headers().get(LOCATION).map(text) else {
            break response;
        };
        if redirects >= MAX_REDIRECT_HOPS {
            return Err(Failure::Other(format!(
                "Too many redirects while fetching \"{url}\" (limit {MAX_REDIRECT_HOPS})."
            )));
        }
        redirects += 1;
        current = target
            .join(&location)
            .map_err(|_| Failure::Other("Invalid URL".to_owned()))?
            .to_string();
    };
    let status = response.status();
    if status.as_u16() >= 400 {
        let reason = response.reason().unwrap_or_default();
        return Err(Failure::Status(
            status.as_u16(),
            format!("HTTP {} {reason}", status.as_u16()),
        ));
    }
    let too_large = |bytes: String| {
        Failure::Other(format!(
            "Response body too large: {bytes} bytes exceeds maxBytes ({MAX_BYTES})."
        ))
    };
    let declared = response.headers().get(CONTENT_LENGTH).map(text);
    if let Some(length) = declared.and_then(|length| length.trim().parse::<f64>().ok())
        && length.is_finite()
        && length > MAX_BYTES as f64
    {
        return Err(too_large(length.to_string()));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(text)
        .unwrap_or_default()
        .to_lowercase();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| Failure::Other("terminated".to_owned()))?;
    let decoded = String::from_utf8_lossy(&bytes);
    let body = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);
    if body.len() > MAX_BYTES {
        return Err(too_large(body.len().to_string()));
    }
    if content_type.starts_with("text/plain") || content_type.starts_with("text/markdown") {
        return Ok((body.to_owned(), true));
    }
    extract(body)
        .map(|content| (content, false))
        .map_err(Failure::Other)
}

/// `validateFetchUrl`.
#[cfg(not(target_family = "wasm"))]
fn validate(url: &str) -> Result<nanocode_http::Url, String> {
    let parsed = nanocode_http::Url::parse(url).map_err(|_| format!("Invalid URL: \"{url}\""))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        scheme => Err(format!(
            "Unsupported URL scheme \"{scheme}:\" — only http(s) allowed."
        )),
    }
}

/// `extractMainContent`: Readability 0.6.0's text (`dom_smoothie` is its port; Kimi's `charThreshold: 0`
/// falls back to the default 500) under a `# title`, else the first article, main or body's text.
#[cfg(not(target_family = "wasm"))]
fn extract(source: &str) -> Result<String, String> {
    let shape = shape(source);
    // html5ever turns CRLF into LF where htmlparser2 keeps it; restored when every line break is one.
    let crlf = source.contains('\n')
        && source
            .match_indices('\n')
            .all(|(index, _)| source[..index].ends_with('\r'));
    let text = |text: &str| match crlf {
        true => js_trim(text).replace('\n', "\r\n"),
        false => js_trim(text).to_owned(),
    };
    if shape.readable
        && let Ok(article) = dom_smoothie::Readability::new(shape.html.as_str(), None, None)
            .and_then(|mut page| page.parse())
    {
        let content = text(&article.text_content);
        if !content.is_empty() {
            return Ok(titled(js_trim(&article.title), &content));
        }
    }
    let document = dom_query::Document::from(shape.html.as_str());
    let first =
        |selector: &str| Some(document.select(selector).first()).filter(|found| found.exists());
    let main = first("article")
        .or_else(|| first("main"))
        .or_else(|| first("body").filter(|_| shape.body));
    let content = text(&main.map(|found| found.text()).unwrap_or_default());
    if content.is_empty() {
        return Err("Failed to extract meaningful content from the page. The page may require JavaScript to render.".to_owned());
    }
    let title = first("title").map(|found| found.text()).unwrap_or_default();
    Ok(titled(&text(&title), &content))
}

#[cfg(not(target_family = "wasm"))]
fn titled(title: &str, text: &str) -> String {
    if title.is_empty() {
        text.to_owned()
    } else {
        format!("# {title}\n\n{text}")
    }
}

/// `String.prototype.trim`.
#[cfg(not(target_family = "wasm"))]
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// What Kimi's DOM (linkedom over htmlparser2: the tags as written, no implied html, head or body)
/// decides. Readability reads `document.body`, which linkedom finds only as the first element's first
/// child element, or second after a `<head>`; otherwise it inserts an empty one and Readability finds
/// nothing. The fallback's `body` is a literal one.
#[cfg(not(target_family = "wasm"))]
struct Shape {
    readable: bool,
    body: bool,
    /// The page with a line feed doubled after `<pre>`, `<listing>` and `<textarea>`, which html5ever
    /// drops and htmlparser2 keeps.
    html: String,
}

/// Replays htmlparser2's element nesting: comments, declarations and raw text skipped, void elements,
/// end tags closing up to their match, and `<body>` closing an open head.
// ponytail: htmlparser2's other implied closes (p, li, td...) never change the first element's first two
// children in real pages; add them if a page proves otherwise.
#[cfg(not(target_family = "wasm"))]
fn shape(source: &str) -> Shape {
    const VOID: [&str; 19] = [
        "area", "base", "basefont", "br", "col", "command", "embed", "frame", "hr", "img", "input",
        "isindex", "keygen", "link", "meta", "param", "source", "track", "wbr",
    ];
    const RAW: [&str; 5] = ["script", "style", "title", "textarea", "xmp"];
    let html = source.to_ascii_lowercase();
    let mut breaks = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let (mut children, mut documents, mut body) = (Vec::new(), 0, false);
    let mut at = 0;
    while let Some(open) = html[at..].find('<').map(|offset| at + offset + 1) {
        let rest = &html[open..];
        at = open;
        if let Some(comment) = rest.strip_prefix("!--") {
            at = comment
                .find("-->")
                .map_or(html.len(), |end| open + 3 + end + 3);
            continue;
        }
        if rest.starts_with(['!', '?']) {
            at = rest.find('>').map_or(html.len(), |end| open + end + 1);
            continue;
        }
        let closing = rest.starts_with('/');
        let start = open + usize::from(closing);
        if !html[start..].starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        let end = html[start..]
            .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
            .map_or(html.len(), |length| start + length);
        let name = &html[start..end];
        at = tag_end(&html, end);
        if closing {
            if let Some(index) = stack.iter().rposition(|open| *open == name) {
                stack.truncate(index);
            }
            continue;
        }
        if name == "body" {
            body = true;
            while stack
                .last()
                .is_some_and(|top| ["head", "link", "script"].contains(top))
            {
                stack.pop();
            }
        }
        match stack.len() {
            0 => documents += 1,
            1 if documents == 1 => children.push(name),
            _ => {}
        }
        if ["pre", "listing", "textarea"].contains(&name)
            && (html[at..].starts_with('\n') || html[at..].starts_with("\r\n"))
        {
            breaks.push(at);
        }
        if RAW.contains(&name) {
            let close = format!("</{name}");
            at = html[at..].find(&close).map_or(html.len(), |end| at + end);
        }
        if !VOID.contains(&name) {
            stack.push(name);
        }
    }
    let readable = matches!(children.as_slice(), ["body", ..] | ["head", "body", ..]);
    let mut html = String::with_capacity(source.len() + breaks.len());
    let mut copied = 0;
    for at in breaks {
        html.push_str(&source[copied..at]);
        html.push('\n');
        copied = at;
    }
    html.push_str(&source[copied..]);
    Shape {
        readable,
        body,
        html,
    }
}

/// Past the `>` ending a tag, skipping quoted attribute values.
#[cfg(not(target_family = "wasm"))]
fn tag_end(html: &str, from: usize) -> usize {
    let (mut quote, mut after_equals) = (None, false);
    for (index, byte) in html.bytes().enumerate().skip(from) {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => continue,
            None if byte == b'>' => return index + 1,
            None if after_equals && (byte == b'"' || byte == b'\'') => {
                quote = Some(byte);
                continue;
            }
            None => {}
        }
        if !byte.is_ascii_whitespace() {
            after_equals = byte == b'=';
        }
    }
    html.len()
}

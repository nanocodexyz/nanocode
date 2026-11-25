use std::collections::BTreeMap;

use nanocode_board::{EventId, NewTask, TaskId, mirror, tasks::Terms};
use serde_json::Value;

use super::{Author, Event, Inbound, Mode, PlatformError, Settle, Target, seconds, throttled};

pub const SYNC: &str = "github";

const OWN: [&str; 2] = ["tag.", "assignee."];
const KEY_MAX: usize = 64;
const MARKER_OPEN: &str = "<!-- board:";
const MARKER_CLOSE: &str = " -->";
const SECONDARY_WAIT_MS: u64 = 60_000;

pub(crate) type Parts = Vec<(&'static str, Event)>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Close {
    #[default]
    Cancel,
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub bot: Option<String>,
    pub close: Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pull,
    Issue,
}

impl Kind {
    const fn word(self) -> &'static str {
        match self {
            Self::Pull => "pr",
            Self::Issue => "issue",
        }
    }

    const fn field(self) -> &'static str {
        match self {
            Self::Pull => "pull_request",
            Self::Issue => "issue",
        }
    }
}

pub(crate) fn text<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer)?.as_str()
}

pub(crate) fn number(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer)?.as_u64()
}

pub(crate) fn keyed(delivery: &str, parts: Parts) -> Vec<Inbound> {
    assert!(!delivery.is_empty(), "a delivery has an id");
    let inbound = parts
        .into_iter()
        .map(|(name, event)| Inbound {
            key: format!("{delivery}/{name}"),
            event,
        })
        .collect::<Vec<_>>();
    debug_assert!(
        inbound
            .iter()
            .enumerate()
            .all(|(at, one)| inbound[..at].iter().all(|other| other.key != one.key))
    );
    inbound
}

pub(crate) fn marker(body: &str) -> (String, Option<EventId>) {
    let Some(start) = body.find(MARKER_OPEN) else {
        return (body.to_owned(), None);
    };
    let from = start + MARKER_OPEN.len();
    let Some(len) = body[from..].find(MARKER_CLOSE) else {
        return (body.to_owned(), None);
    };
    let id = body[from..from + len].trim();
    let rest = format!(
        "{}{}",
        &body[..start],
        &body[from + len + MARKER_CLOSE.len()..]
    );
    (
        rest.trim().to_owned(),
        (!id.is_empty()).then(|| EventId(id.to_owned())),
    )
}

fn offset(zone: &str) -> Option<i64> {
    let sign = match zone.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let (hours, minutes) = zone[1..].split_once(':')?;
    Some(sign * (hours.parse::<i64>().ok()? * 60 + minutes.parse::<i64>().ok()?))
}

pub(crate) fn millis(iso: &str) -> Option<u64> {
    let (date, time) = iso.split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, zone) = match time.strip_suffix('Z') {
        Some(clock) => (clock, 0),
        None => {
            let at = time.rfind(['+', '-'])?;
            (&time[..at], offset(&time[at..])?)
        }
    };
    let mut fields = clock.splitn(3, ':');
    let hour = fields.next()?.parse::<i64>().ok()?;
    let minute = fields.next()?.parse::<i64>().ok()?;
    let last = fields.next()?;
    let (second, fraction) = last.split_once('.').unwrap_or((last, ""));
    let second = second.parse::<i64>().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let milli = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(3)
        .collect::<String>()
        .parse::<i64>()
        .ok()?;
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let minutes = days * 1440 + hour * 60 + minute - zone;
    u64::try_from((minutes * 60 + second) * 1000 + milli).ok()
}

fn slug(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut gap = false;
    for c in name.chars().filter(char::is_ascii) {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            if gap {
                out.push('-');
                gap = false;
            }
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    if gap {
        out.push('-');
    }
    out.chars()
        .any(|c| c.is_ascii_alphanumeric())
        .then_some(out)
}

pub(crate) fn tagged<'a>(
    set: &mut BTreeMap<String, String>,
    prefix: &str,
    names: impl Iterator<Item = &'a str>,
) {
    for name in names {
        if let Some(slug) = slug(name) {
            let mut key = format!("{prefix}{slug}");
            debug_assert!(key.is_ascii());
            key.truncate(KEY_MAX);
            set.insert(key, name.to_owned());
        }
    }
}

pub(crate) fn names<'a>(
    item: &'a Value,
    list: &str,
    field: &'a str,
) -> impl Iterator<Item = &'a str> {
    item.pointer(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(move |entry| text(entry, field))
}

fn own() -> Vec<String> {
    OWN.iter().map(|prefix| (*prefix).to_owned()).collect()
}

fn reconcile(item: &Value) -> BTreeMap<String, String> {
    let mut set = BTreeMap::new();
    tagged(&mut set, OWN[0], names(item, "/labels", "/name"));
    tagged(&mut set, OWN[1], names(item, "/assignees", "/login"));
    set
}

fn thread(repo: &str, number: u64) -> Target {
    Target::new(SYNC, format!("{repo}#{number}"))
}

fn task(kind: Kind, repo: &str, number: u64) -> TaskId {
    TaskId(format!("{SYNC}/{}/{repo}/{number}", kind.word()))
}

fn author(config: &Config, user: &Value) -> Option<Author> {
    let login = text(user, "/login")?;
    let bot = text(user, "/type") == Some("Bot") || config.bot.as_deref() == Some(login);
    Some(Author {
        user: login.to_owned(),
        name: login.to_owned(),
        bot,
    })
}

fn state(kind: Kind, item: &Value) -> &'static str {
    if kind == Kind::Pull && item.get("draft").and_then(Value::as_bool) == Some(true) {
        "draft"
    } else {
        "open"
    }
}

fn closing(config: &Config, kind: Kind, item: &Value) -> (Settle, &'static str) {
    if kind == Kind::Pull && item.get("merged").and_then(Value::as_bool) == Some(true) {
        return (Settle::Done("merged".to_owned()), "merged");
    }
    let outcome = match config.close {
        Close::Cancel => Settle::Cancelled("closed".to_owned()),
        Close::Done => Settle::Done("closed".to_owned()),
    };
    (outcome, "closed")
}

fn opened(
    kind: Kind,
    repo: &str,
    num: u64,
    item: &Value,
    task: &TaskId,
    head: &str,
) -> Option<Event> {
    let spec = mirror::spec(text(item, "/title")?, text(item, "/body"));
    let mut set = reconcile(item);
    set.insert(kind.word().to_owned(), num.to_string());
    set.insert("state".to_owned(), state(kind, item).to_owned());
    set.insert("repo".to_owned(), repo.to_owned());
    if kind == Kind::Pull && !head.is_empty() {
        set.insert("head".to_owned(), head.to_owned());
    }
    let new = NewTask {
        id: task.clone(),
        parent: None,
        spec,
        labels: set,
        needs: Vec::new(),
        terms: Terms::default(),
    };
    Some(Event::Opened {
        task: new,
        thread: thread(repo, num),
        message: format!("{}:{num}", kind.word()),
        mode: Mode::Always,
    })
}

fn reworded(
    config: &Config,
    repo: &str,
    num: u64,
    item: &Value,
    payload: &Value,
    stamp: u64,
) -> Option<Event> {
    let title = text(item, "/title")?.to_owned();
    let body = text(item, "/body").map(str::to_owned);
    let author = author(config, payload.get("sender")?)?;
    Some(Event::Reworded {
        thread: thread(repo, num),
        author,
        title,
        body,
        version: stamp,
    })
}

fn subject(
    config: &Config,
    kind: Kind,
    repo: &str,
    action: &str,
    payload: &Value,
) -> Option<Parts> {
    let item = payload.get(kind.field())?;
    let num = number(item, "/number")?;
    let stamp = text(item, "/updated_at").and_then(millis)?;
    let task = task(kind, repo, num);
    let labels = |extra: &[(&str, &str)]| {
        let mut set = reconcile(item);
        set.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        Event::Labels {
            task: task.clone(),
            set,
            unset: Vec::new(),
            own: own(),
            version: stamp,
        }
    };
    let head = text(item, "/head/sha").unwrap_or_default();
    Some(match action {
        "opened" => vec![("opened", opened(kind, repo, num, item, &task, head)?)],
        "edited"
            if payload.pointer("/changes/title").is_some()
                || payload.pointer("/changes/body").is_some() =>
        {
            vec![(
                "reworded",
                reworded(config, repo, num, item, payload, stamp)?,
            )]
        }
        "labeled" | "unlabeled" | "assigned" | "unassigned" => vec![("labels", labels(&[]))],
        "synchronize" if kind == Kind::Pull => vec![("labels", labels(&[("head", head)]))],
        "reopened" | "ready_for_review" | "converted_to_draft" => {
            vec![("labels", labels(&[("state", state(kind, item))]))]
        }
        "closed" => {
            let (outcome, label) = closing(config, kind, item);
            vec![
                (
                    "closed",
                    Event::Closed {
                        task: task.clone(),
                        outcome,
                    },
                ),
                ("labels", labels(&[("state", label)])),
            ]
        }
        _ => return None,
    })
}

fn remark(
    config: &Config,
    thread: Target,
    tag: &str,
    action: &str,
    comment: &Value,
    extra: (Option<String>, Option<String>),
) -> Option<Parts> {
    let message = format!("{tag}:{}", number(comment, "/id")?);
    let event = match action {
        "created" => {
            let (body, marker) = marker(text(comment, "/body")?);
            let author = author(config, comment.get("user")?)?;
            Event::Message {
                thread,
                message,
                author,
                body,
                context: extra.0,
                sub: extra.1,
                marker,
            }
        }
        "edited" => {
            let (body, _) = marker(text(comment, "/body")?);
            let version = text(comment, "/updated_at").and_then(millis)?;
            Event::Edited {
                thread,
                message,
                body,
                version,
            }
        }
        "deleted" => Event::Deleted { thread, message },
        _ => return None,
    };
    Some(vec![("comment", event)])
}

fn issue_comment(config: &Config, repo: &str, action: &str, payload: &Value) -> Option<Parts> {
    let thread = thread(repo, number(payload, "/issue/number")?);
    remark(
        config,
        thread,
        "c",
        action,
        payload.get("comment")?,
        (None, None),
    )
}

fn review_comment(config: &Config, repo: &str, action: &str, payload: &Value) -> Option<Parts> {
    let thread = thread(repo, number(payload, "/pull_request/number")?);
    let comment = payload.get("comment")?;
    let id = number(comment, "/id")?;
    let line = number(comment, "/line").or_else(|| number(comment, "/original_line"));
    let context = text(comment, "/path")
        .map(|path| line.map_or_else(|| path.to_owned(), |line| format!("{path}:{line}")));
    let top = number(comment, "/in_reply_to_id").unwrap_or(id);
    remark(
        config,
        thread,
        "rc",
        action,
        comment,
        (context, Some(format!("rc:{top}"))),
    )
}

fn review(config: &Config, repo: &str, action: &str, payload: &Value) -> Option<Parts> {
    let review = payload.get("review")?;
    let pull = payload.get("pull_request")?;
    let num = number(pull, "/number")?;
    let task = task(Kind::Pull, repo, num);
    let labels = |set: BTreeMap<String, String>, unset: Vec<String>, version| Event::Labels {
        task: task.clone(),
        set,
        unset,
        own: Vec::new(),
        version,
    };
    match action {
        "submitted" => {
            let state = text(review, "/state")?.to_ascii_lowercase();
            let (body, marker) = marker(text(review, "/body").unwrap_or_default());
            let summary = match state.as_str() {
                "approved" => "approved",
                "changes_requested" => "requested changes",
                _ => "commented",
            };
            let mut parts = Parts::new();
            if state != "commented" || !body.is_empty() {
                let body = if body.is_empty() {
                    summary.to_owned()
                } else {
                    format!("{summary}\n\n{body}")
                };
                let message = format!("r:{}", number(review, "/id")?);
                let event = Event::Message {
                    thread: thread(repo, num),
                    message,
                    author: author(config, review.get("user")?)?,
                    body,
                    context: None,
                    sub: None,
                    marker,
                };
                parts.push(("message", event));
            }
            if matches!(state.as_str(), "approved" | "changes_requested") {
                let version = text(review, "/submitted_at").and_then(millis)?;
                parts.push((
                    "labels",
                    labels(
                        BTreeMap::from([("review".to_owned(), state)]),
                        Vec::new(),
                        version,
                    ),
                ));
            }
            Some(parts)
        }
        "dismissed" => {
            let version = text(pull, "/updated_at").and_then(millis)?;
            Some(vec![(
                "labels",
                labels(BTreeMap::new(), vec!["review".to_owned()], version),
            )])
        }
        _ => None,
    }
}

pub fn parse(delivery: &str, payload: &Value) -> Vec<Inbound> {
    parse_with(&Config::default(), delivery, payload)
}

pub fn parse_with(config: &Config, delivery: &str, payload: &Value) -> Vec<Inbound> {
    let (Some(repo), Some(action)) = (
        text(payload, "/repository/full_name"),
        text(payload, "/action"),
    ) else {
        return Vec::new();
    };
    let has = |field: &str| payload.get(field).is_some();
    let parts = if has("review") {
        review(config, repo, action, payload)
    } else if has("comment") && has("issue") {
        issue_comment(config, repo, action, payload)
    } else if has("comment") {
        review_comment(config, repo, action, payload)
    } else if has("pull_request") {
        subject(config, Kind::Pull, repo, action, payload)
    } else if has("issue") {
        subject(config, Kind::Issue, repo, action, payload)
    } else {
        None
    };
    keyed(delivery, parts.unwrap_or_default())
}

pub fn limited(
    status: u16,
    headers: &BTreeMap<String, String>,
    message: &str,
    now_ms: u64,
) -> Option<PlatformError> {
    let spent = headers
        .get("x-ratelimit-remaining")
        .is_some_and(|remaining| remaining.trim() == "0");
    if status != 429 && !(status == 403 && (spent || message.contains("rate limit"))) {
        return None;
    }
    let retry_after_ms = match headers.get("retry-after") {
        Some(header) => seconds(header),
        None if spent => headers
            .get("x-ratelimit-reset")
            .and_then(|header| seconds(header))
            .map(|reset| reset.saturating_sub(now_ms)),
        None => Some(SECONDARY_WAIT_MS),
    };
    Some(throttled(retry_after_ms))
}

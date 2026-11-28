use crate::prompts;
use nanocode_backend_kimi_client::{
    Message, Origin, estimate_message, estimate_request_message, estimate_text,
};

const TRIGGER_RATIO: f64 = 0.85;
const RESERVED_TOKENS: u64 = 50_000;
const USER_MESSAGE_TOKENS: u64 = 20_000;
const USER_HEAD_TOKENS: u64 = 2_000;
pub const MAX_OUTPUT_TOKENS: u64 = 128 * 1024;
pub const MARGIN_RATIO: f64 = 0.02;
const SHRINK_RATIO: f64 = 0.85;
pub const ATTEMPTS: u32 = 5;

pub fn should_compact(used: u64, window: u64) -> bool {
    window > 0
        && (used as f64 >= window as f64 * TRIGGER_RATIO
            || (RESERVED_TOKENS < window && used + RESERVED_TOKENS >= window))
}

fn anchor(messages: &[Message]) -> Option<(usize, u64)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            Message::Assistant {
                usage: Some(usage), ..
            } if usage.total() > 0 => Some((index, usage.total())),
            _ => None,
        })
}

fn sum(messages: &[Message], estimate: fn(&Message) -> u64) -> u64 {
    messages.iter().map(estimate).sum()
}

pub fn request_used(messages: &[Message], system: u64, listing: u64) -> u64 {
    match anchor(messages) {
        Some((index, usage)) => usage + sum(&messages[index + 1..], estimate_request_message),
        None => sum(messages, estimate_request_message) + system + listing,
    }
}

pub fn counted(messages: &[Message], rebase: Option<(usize, u64)>) -> u64 {
    match (anchor(messages), rebase) {
        (Some((index, usage)), _) => usage + sum(&messages[index + 1..], estimate_message),
        (None, Some((length, tokens))) => {
            tokens + sum(&messages[length.min(messages.len())..], estimate_message)
        }
        (None, None) => sum(messages, estimate_message),
    }
}

pub fn history_tokens(overhead: u64, messages: &[Message]) -> u64 {
    let estimated = overhead + sum(messages, estimate_message);
    anchor(messages).map_or(estimated, |(index, usage)| {
        estimated.max(usage + sum(&messages[index + 1..], estimate_message))
    })
}

pub fn weights(history: &[Message], overhead: u64) -> Vec<u64> {
    let mut weights = history.iter().map(estimate_message).collect::<Vec<_>>();
    let mut anchors = history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            Message::Assistant {
                usage: Some(usage), ..
            } if usage.total() > 0 => Some((index, usage.total())),
            _ => None,
        })
        .collect::<Vec<_>>();
    for k in (0..anchors.len().saturating_sub(1)).rev() {
        anchors[k].1 = anchors[k].1.min(anchors[k + 1].1);
    }
    let (mut previous, mut previous_tokens) = (0, overhead);
    for (index, tokens) in anchors {
        let span = weights[previous..=index].iter().sum::<u64>();
        let excess = tokens.saturating_sub(previous_tokens).saturating_sub(span);
        if excess > 0 && span == 0 {
            weights[index] += excess;
        } else if excess > 0 {
            let base = weights[previous..=index].to_vec();
            let mut given = 0;
            for (offset, weight) in base.iter().enumerate() {
                let add = excess * weight / span;
                weights[previous + offset] += add;
                given += add;
            }
            weights[index] += excess - given;
        }
        previous = index + 1;
        previous_tokens = tokens;
    }
    weights
}

pub fn pre_shrink(
    history: &[Message],
    weights: &[u64],
    overhead: u64,
    instruction: u64,
    window: u64,
) -> usize {
    let reserve = window.min(MAX_OUTPUT_TOKENS).min(window / 8);
    let limit = window - reserve;
    if history_tokens(overhead, history) + instruction <= limit {
        return 0;
    }
    let budget = (limit as f64 * SHRINK_RATIO).floor() as u64;
    let Some(budget) = budget.checked_sub(overhead).filter(|budget| *budget > 0) else {
        return 0;
    };
    let mut start = history.len();
    let mut tokens = 0;
    for index in (0..history.len()).rev() {
        if tokens + weights[index] > budget {
            break;
        }
        tokens += weights[index];
        start = index;
    }
    let start = skip_tool_results(history, start.max(1));
    if start >= history.len() { 0 } else { start }
}

pub fn without_oldest(history: &[Message], start: usize) -> usize {
    skip_tool_results(history, start + 1)
}

fn skip_tool_results(history: &[Message], mut start: usize) -> usize {
    while matches!(history.get(start), Some(Message::Tool { .. })) {
        start += 1;
    }
    start
}

pub fn compacted(history: &[Message], summary: &str) -> Vec<Message> {
    let users = history
        .iter()
        .filter_map(|message| match message {
            Message::User {
                content,
                origin: Origin::Prompt,
            } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut kept = select(&users)
        .into_iter()
        .map(Message::from)
        .collect::<Vec<_>>();
    kept.push(Message::user(prompts::summary(summary), Origin::Summary));
    kept.push(Message::user(prompts::continuation(), Origin::Reminder));
    kept
}

enum Kept {
    Text(String),
    Elision(u64),
}

fn tokens(text: &str) -> u64 {
    estimate_text("user") + estimate_text(text)
}

fn select(users: &[String]) -> Vec<Kept> {
    let total = users.iter().map(|text| tokens(text)).sum::<u64>();
    if total <= USER_MESSAGE_TOKENS {
        return users.iter().cloned().map(Kept::Text).collect();
    }
    let mut tail = Vec::new();
    let mut tail_left = USER_MESSAGE_TOKENS - USER_HEAD_TOKENS;
    let mut head_end = users.len();
    let mut dropped_prefix = None;
    for (index, text) in users.iter().enumerate().rev() {
        if tail_left == 0 {
            break;
        }
        let size = tokens(text);
        head_end = index;
        if size <= tail_left {
            tail.push(text.clone());
            tail_left -= size;
            continue;
        }
        let suffix = suffix_within(text, tail_left);
        let prefix = &text[..text.len() - suffix.len()];
        if !prefix.is_empty() {
            dropped_prefix = Some(prefix.to_owned());
        }
        tail.push(suffix.to_owned());
        break;
    }
    tail.reverse();
    let mut candidates = users[..head_end].to_vec();
    candidates.extend(dropped_prefix);
    let mut head = Vec::new();
    let mut head_left = USER_HEAD_TOKENS;
    for text in candidates {
        if head_left == 0 {
            break;
        }
        let size = tokens(&text);
        if size <= head_left {
            head.push(text);
            head_left -= size;
            continue;
        }
        head.push(prefix_within(&text, head_left).to_owned());
        break;
    }
    let kept = head
        .iter()
        .chain(&tail)
        .map(|text| tokens(text))
        .sum::<u64>();
    let mut selection = head.into_iter().map(Kept::Text).collect::<Vec<_>>();
    selection.push(Kept::Elision(total.saturating_sub(kept)));
    selection.extend(tail.into_iter().map(Kept::Text));
    selection
}

const fn fits(ascii: u64, other: u64, budget: u64) -> bool {
    ascii.div_ceil(4) + other <= budget
}

fn prefix_within(text: &str, budget: u64) -> &str {
    let (mut ascii, mut other, mut end) = (0, 0, 0);
    for char in text.chars() {
        if char.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
        if !fits(ascii, other, budget) {
            break;
        }
        end += char.len_utf8();
    }
    &text[..end]
}

fn suffix_within(text: &str, budget: u64) -> &str {
    let (mut ascii, mut other, mut start) = (0, 0, text.len());
    for (index, char) in text.char_indices().rev() {
        if char.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
        if !fits(ascii, other, budget) {
            break;
        }
        start = index;
    }
    &text[start..]
}

impl From<Kept> for Message {
    fn from(kept: Kept) -> Self {
        match kept {
            Kept::Elision(omitted) => Self::user(prompts::elision(omitted), Origin::Reminder),
            Kept::Text(text) => Self::user(text, Origin::Prompt),
        }
    }
}

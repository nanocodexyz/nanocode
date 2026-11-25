use crate::{ContentBlock, ToolResultContent};

use super::history::HistoryMessage;

pub(super) const CLEARED: &str = "[Old tool result content cleared]";
const CLEAR_AT_LEAST_TOKENS: u64 = 20_000;
const KEEP_TOOL_RESULTS: usize = 12;
const CLEARABLE_CHARS_MIN: usize = 64;

pub(super) fn result_tokens(block: &ContentBlock) -> Option<u64> {
    let ContentBlock::ToolResult { content, .. } = block else {
        return None;
    };
    let (chars, tokens) = match content {
        ToolResultContent::Text(text) => (text.chars().count(), super::estimate_text_tokens(text)),
        ToolResultContent::Blocks(blocks) => {
            if blocks.iter().any(|block| block["type"] != "text") {
                return None;
            }
            blocks.iter().fold((0, 0), |(chars, tokens), block| {
                let text = block["text"].as_str().unwrap_or_default();
                (
                    chars + text.chars().count(),
                    tokens + super::estimate_text_tokens(text),
                )
            })
        }
    };
    (chars >= CLEARABLE_CHARS_MIN).then_some(tokens)
}

pub(super) fn clear_tool_results(
    messages: &mut [HistoryMessage],
    policy: Option<(usize, u64)>,
) -> [Option<usize>; 2] {
    let (keep, at_least_tokens) = policy.unwrap_or((KEEP_TOOL_RESULTS, CLEAR_AT_LEAST_TOKENS));
    let results: Vec<(usize, usize, u64)> = messages
        .iter()
        .enumerate()
        .flat_map(|(message, entry)| {
            entry
                .result_tokens(result_tokens)
                .iter()
                .map(move |&(block, tokens)| (message, block, tokens))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut cleared = 0;
    let mut previous = None;
    let mut pending = 0;
    for seen in keep + 1..=results.len() {
        pending += results[seen - keep - 1].2;
        if pending >= at_least_tokens {
            previous = Some(cleared);
            cleared = seen - keep;
            pending = 0;
        }
    }
    let mut at = 0;
    while at < cleared {
        let message = results[at].0;
        let end = at
            + results[at..cleared]
                .iter()
                .take_while(|result| result.0 == message)
                .count();
        let blocks = &results[at..end];
        if end - at == messages[message].result_tokens(result_tokens).len() {
            messages[message] = messages[message].cleared();
        } else {
            clear(&mut messages[message].make_mut().content, blocks);
        }
        at = end;
    }
    [
        previous.map(|at| results[at].0),
        results.get(cleared).map(|result| result.0),
    ]
}

fn clear(content: &mut [ContentBlock], blocks: &[(usize, usize, u64)]) {
    for &(_, block, _) in blocks {
        if let ContentBlock::ToolResult { content, .. } = &mut content[block] {
            *content = ToolResultContent::Text(CLEARED.to_owned());
        }
    }
}

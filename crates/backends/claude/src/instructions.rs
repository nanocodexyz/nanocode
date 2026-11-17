const CODE_MODE: &str = include_str!("../prompts/code_mode.md");

const OPUS_5_5_PARAGRAPH: &str = "\n\nDo build original games,";
const SURFACES: &str = " - Claude Code is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).";
const FAST_MODE: &str = "\n - Fast mode for Claude Code gives faster output on this session's model if it supports fast mode, and otherwise switches the session to Claude Opus. It can be toggled with /fast.";
const HEADLESS_SYSTEM_LINES: [&str; 4] = [
    "\n - Text you output outside of tool use is displayed to the user",
    "\n - Tools run behind a user-selected permission mode;",
    "\n - Reference code as `file_path:line_number`",
    "\n - Claude Code is available as a CLI",
];
const HEADLESS_SYSTEM_SENTENCES: [&str; 2] = [
    " Hooks may intercept tool calls; treat hook output as user feedback.",
    "For actions that are hard to reverse or outward-facing, confirm first unless durably authorized or explicitly told to proceed without asking; approval in one context doesn't extend to the next. ",
];
const HEADLESS_SYSTEM_PARAGRAPH: &str = "\n\nWhen you use a pronoun for someone";
const HEADLESS_BASH_LINES: [&str; 3] = [
    "\n- Use the `gh` CLI for GitHub operations",
    "\n- GitHub notifies the owner of any",
    "\n- End git commit messages and PR bodies",
];
const HEADLESS_EXEC_LINES: [&str; 2] = [
    "\n- Nanocode extension: a call to a name missing from `ALL_TOOLS`",
    "\n- Nanocode extension: an image file the cell's calls create or change",
];
const HEADLESS_EXEC_GLOBALS: [&str; 4] = [
    "`audio(",
    "`generatedImage(",
    "`notify(",
    "`yield_control()`",
];

fn remove_line(text: &mut String, start: &str) {
    if let Some(at) = text.find(start) {
        let end = text[at + 1..]
            .find('\n')
            .map_or(text.len(), |end| at + 1 + end);
        text.replace_range(at..end, "");
    }
}

fn headless_system(text: &mut String) {
    for line in HEADLESS_SYSTEM_LINES {
        remove_line(text, line);
    }
    for sentence in HEADLESS_SYSTEM_SENTENCES {
        *text = text.replacen(sentence, "", 1);
    }
    if let Some(start) = text.find(HEADLESS_SYSTEM_PARAGRAPH) {
        let end = text[start + 2..]
            .find("\n\n")
            .map_or(text.len(), |end| start + 2 + end);
        text.replace_range(start..end, "");
    }
}

fn headless_bash_parameter() -> Option<&'static str> {
    static RENDERED: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| {
        let schema = nanocode_backend_claude_tools::upstream::tool("Bash").input_schema();
        let text = schema
            .as_value()
            .pointer("/properties/description/description")?
            .as_str()?
            .to_owned();
        let (_, rest) = text.split_once("\n\n")?;
        Some(
            rest.lines()
                .filter(|line| !line.is_empty())
                .map(|line| format!("  // {line}\n"))
                .collect(),
        )
    });
    RENDERED.as_deref()
}

fn headless_exec(description: &mut String) {
    for line in HEADLESS_EXEC_LINES {
        remove_line(description, line);
    }
    let Some(at) = description.find("\n- Globals: ") else {
        return;
    };
    let end = description[at + 1..]
        .find('\n')
        .map_or(description.len(), |end| at + 1 + end);
    let kept = description[at..end]
        .split("; ")
        .filter(|item| {
            !HEADLESS_EXEC_GLOBALS
                .iter()
                .any(|global| item.starts_with(global))
        })
        .collect::<Vec<_>>()
        .join("; ");
    description.replace_range(at..end, &kept);
}

fn headless_tool(definition: &mut crate::ToolDefinition) {
    let mut description = definition.description.to_string();
    if definition.name == "exec" {
        headless_exec(&mut description);
    }
    for line in HEADLESS_BASH_LINES {
        remove_line(&mut description, line);
    }
    if let Some(rendered) = headless_bash_parameter() {
        description = description.replacen(rendered, "", 1);
    }
    if description != *definition.description {
        definition.description = description.into();
    }
}

pub(crate) fn upstream_tool(
    model: &str,
    codesign: bool,
    headless: bool,
    definition: &mut crate::ToolDefinition,
) {
    if headless {
        headless_tool(definition);
    }
    if crate::environment::bypass_note(model, codesign).is_none() {
        return;
    }
    let line = format!("\n{}", crate::upstream::bash_first().dedicated_tools);
    if definition.description.contains(&line) {
        definition.description = definition.description.replacen(&line, "", 1).into();
    }
}

pub(crate) fn default_system_blocks(
    code_only: bool,
    model: &str,
    codesign: bool,
    upstream_context: bool,
    headless: bool,
) -> std::sync::Arc<Vec<serde_json::Value>> {
    let mut blocks = crate::upstream::system_blocks();
    if let Some(serde_json::Value::String(text)) =
        blocks.last_mut().and_then(|block| block.get_mut("text"))
    {
        if !crate::environment::opus_5_5_prompt(model, codesign)
            && let Some(start) = text.find(OPUS_5_5_PARAGRAPH)
        {
            let end = text[start + 2..]
                .find("\n\n")
                .map_or(text.len(), |end| start + 2 + end);
            text.replace_range(start..end, "");
        }
        if headless {
            headless_system(text);
        } else if upstream_context && let Some(at) = text.find(SURFACES) {
            text.insert_str(at + SURFACES.len(), FAST_MODE);
        }
        if crate::environment::total_tokens_reminder(model, codesign) {
            text.push_str(&format!(
                "\n\n<total_tokens>{} tokens left</total_tokens>",
                crate::environment::TOKEN_BUDGET
            ));
        }
    }
    if code_only {
        let marker = blocks
            .last_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|block| block.remove("cache_control"));
        blocks.push(serde_json::json!({"type": "text", "text": CODE_MODE}));
        if let Some(note) = crate::environment::bypass_note(model, codesign) {
            blocks.push(serde_json::json!({"type": "text", "text": note}));
        }
        if let (Some(marker), Some(block)) = (marker, blocks.last_mut()) {
            block["cache_control"] = marker;
        }
    }
    std::sync::Arc::new(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(blocks: &[serde_json::Value]) -> Vec<(String, bool)> {
        blocks
            .iter()
            .map(|block| {
                (
                    block["text"].as_str().unwrap().chars().take(60).collect(),
                    block.get("cache_control").is_some(),
                )
            })
            .collect()
    }

    #[test]
    fn code_mode_marks_the_last_static_system_block() {
        let upstream = layout(&crate::upstream::system_blocks());
        assert_eq!(upstream.iter().filter(|block| block.1).count(), 2);
        for (model, codesign, headless) in [
            ("claude-opus-5-5", true, true),
            ("claude-opus-5-5", false, true),
            ("claude-haiku-5-5", false, true),
            ("claude-opus-5-5", true, false),
        ] {
            let plain = layout(&default_system_blocks(
                false, model, codesign, false, headless,
            ));
            let marks =
                |blocks: &[(String, bool)]| blocks.iter().map(|block| block.1).collect::<Vec<_>>();
            assert_eq!(marks(&plain), marks(&upstream));
            let blocks = layout(&default_system_blocks(
                true, model, codesign, false, headless,
            ));
            let bypass = crate::environment::bypass_note(model, codesign);
            let mut expected = vec![true, false, false];
            expected.extend(bypass.as_ref().map(|_| false));
            *expected.last_mut().unwrap() = true;
            assert_eq!(marks(&blocks), expected, "{blocks:?}");
            assert_eq!(
                blocks[..2].iter().map(|block| &block.0).collect::<Vec<_>>(),
                plain.iter().map(|block| &block.0).collect::<Vec<_>>()
            );
            let last: String = bypass
                .as_deref()
                .unwrap_or(CODE_MODE)
                .chars()
                .take(60)
                .collect();
            assert_eq!(blocks.last().unwrap().0, last);
        }
    }

    #[test]
    fn code_mode_request_stays_within_four_breakpoints() {
        let system = default_system_blocks(true, "claude-opus-5-5", true, false, true);
        let mut system = Vec::clone(&system);
        system.push(serde_json::json!({"type": "text", "text": "# Environment"}));
        let mut request: crate::MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1,
            "cache_control": {"type": "ephemeral"},
            "system": system,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "task"}]}],
            "tools": [{"name": "exec", "description": "run", "input_schema": {"type": "object"}}],
        }))
        .unwrap();
        let messages = crate::MessageLayout::of(&request.messages);
        request.cache_prefix_with(messages, false).unwrap();
        let wire = serde_json::to_value(&request).unwrap();
        let system = wire["system"].as_array().unwrap();
        let marks: Vec<bool> = layout(system).into_iter().map(|block| block.1).collect();
        assert_eq!(marks, [true, false, false, true, false]);
        assert!(wire["tools"][0].get("cache_control").is_some());
        assert!(
            wire["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        let automatic = usize::from(wire.get("cache_control").is_some());
        assert_eq!(
            marks.iter().filter(|mark| **mark).count() + 1 + automatic,
            4
        );
        request.validate_cache_control().unwrap();
    }
}

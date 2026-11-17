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
        blocks.push(serde_json::json!({"type": "text", "text": CODE_MODE}));
        if let Some(note) = crate::environment::bypass_note(model, codesign) {
            blocks.push(serde_json::json!({"type": "text", "text": note}));
        }
    }
    std::sync::Arc::new(blocks)
}

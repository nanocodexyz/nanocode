const CODE_MODE: &str = include_str!("../prompts/code_mode.md");

const OPUS_5_5_PARAGRAPH: &str = "\n\nDo build original games,";
const SURFACES: &str = " - Claude Code is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).";
const FAST_MODE: &str = "\n - Fast mode for Claude Code gives faster output on this session's model if it supports fast mode, and otherwise switches the session to Claude Opus. It can be toggled with /fast.";

pub(crate) fn upstream_tool(model: &str, codesign: bool, definition: &mut crate::ToolDefinition) {
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
        if upstream_context && let Some(at) = text.find(SURFACES) {
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

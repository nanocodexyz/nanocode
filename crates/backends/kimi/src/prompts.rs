use std::path::Path;

pub const SYSTEM: &str = include_str!("../prompts/system.md");
pub const CODING: &str = include_str!("../prompts/coding.md");
pub const DELIVERING_WORK: &str = include_str!("../prompts/delivering-work.md");
pub const PROJECT_INFORMATION: &str = include_str!("../prompts/project-information.md");
pub const COMPACTION_INSTRUCTION: &str = include_str!("../prompts/compaction-instruction.md");
pub const SUMMARY_PREFIX: &str = include_str!("../prompts/compaction-summary-prefix.md");
pub const AUTO_MODE: &str = include_str!("../prompts/permission-mode-auto-enter-reminder.md");

const PRODUCT_NAME: &str = "Kimi Code CLI";
const REPLY_STYLE_GUIDE: &str = "Your text replies render as Markdown in the user's terminal. Keep structure light and shallow — deep nesting, large tables, and heavy headings read poorly there. Cite code locations as `path/to/file.ts:42` so the user can navigate to them. Do not use emoji unless the user does first or asks for it.";
const AGENTS_MD_UPDATE: &str =
    " If you change anything an `AGENTS.md` documents, update that `AGENTS.md` to match.";
const WINDOWS_NOTES: &str = "IMPORTANT: You are on Windows. The Bash tool runs through Git Bash, so use Unix shell syntax inside Bash commands — `/dev/null` not `NUL`, and forward slashes in paths. For file operations, always prefer the built-in tools (Read, Write, Edit, Glob, Grep) over Bash commands — they work reliably across all platforms.";
const ROOT_WIDTH: usize = 30;
const CHILD_WIDTH: usize = 10;

const REPEAT_FIRST: &str = "The same tool call has been repeated several times in a row. Before making your next call, write one sentence stating what new information you expect it to produce. Then act on that sentence: if it names something this result does not already give you, choose the action that best provides it; otherwise, continue with the evidence you already have.";
const REPEAT_FINAL: &str = "Write your final response now, without any further tool calls. Cover: the current blocker, each approach you have tried and what it established, and the specific information or decision you need from the user to unblock progress. Text only.";

#[derive(Clone, Debug, Default)]
pub struct Environment {
    pub os: String,
    pub shell: Option<(String, String)>,
    pub cwd: String,
    pub cwd_listing: String,
    pub agents_md: String,
    pub role_additional: String,
}

pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name_end = after
            .find(|char: char| !(char.is_ascii_alphanumeric() || char == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_end];
        let closed = after[name_end..].starts_with('}');
        let valid = closed
            && name
                .chars()
                .next()
                .is_some_and(|char| char.is_ascii_alphabetic() || char == '_');
        match vars.iter().find(|(key, _)| *key == name).filter(|_| valid) {
            Some((_, value)) => {
                out.push_str(value);
                rest = &after[name_end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

const TODO_LIST_GUIDANCE: &str = "When you track work in `TodoList`, keep it in step with what you have actually done: mark an item done as soon as it is finished, before you start the next one, and make sure the list matches what you did before you write your final message.\n\n";

/// The system prompt; `todo_list` adds Kimi Code's TodoList guidance, present whenever the tool is.
pub fn system(environment: &Environment, todo_list: bool) -> String {
    let todo_list_guidance = if todo_list { TODO_LIST_GUIDANCE } else { "" };
    let delivering_work = render(
        DELIVERING_WORK,
        &[
            ("plan_mode_guidance", ""),
            ("todo_list_guidance", todo_list_guidance),
        ],
    );
    let agents_section = render(
        PROJECT_INFORMATION,
        &[
            ("agents_md", &environment.agents_md),
            ("agents_md_update", AGENTS_MD_UPDATE),
        ],
    );
    let shell = environment
        .shell
        .as_ref()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, path)| format!("{name} (`{path}`)"))
        .unwrap_or_default();
    let windows_notes = match environment.os.as_str() {
        "Windows" => format!("\n\n{WINDOWS_NOTES}\n\n"),
        _ => String::new(),
    };
    render(
        SYSTEM,
        &[
            ("role_additional", &environment.role_additional),
            ("product_name", PRODUCT_NAME),
            ("reply_style_guide", REPLY_STYLE_GUIDE),
            ("notify_user_guidance", ""),
            ("todo_list_guidance", todo_list_guidance),
            ("coding_section", &format!("{CODING}\n")),
            ("delivering_work_section", &format!("{delivering_work}\n")),
            ("os", &environment.os),
            ("windows_notes", &windows_notes),
            ("shell", &shell),
            ("cwd", &environment.cwd),
            ("cwd_listing", &environment.cwd_listing),
            ("agents_md", &environment.agents_md),
            ("agents_section", &agents_section),
            ("additional_dirs_info", ""),
            ("additional_dirs_section", ""),
            ("skills", ""),
            ("skills_section", ""),
            ("plugin_sections", ""),
        ],
    )
}

pub fn reminder(text: &str) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}

pub fn date(local_date: &str, zone: &str) -> String {
    reminder(&format!(
        "Today's date is {local_date}, in the local client machine's timezone ({zone}). The current date is restated in a reminder whenever it changes; rely on the latest such reminder for the current date. DO NOT mention this to the user explicitly."
    ))
}

pub fn date_changed(local_date: &str, zone: &str) -> String {
    reminder(&format!(
        "The date has changed. Today's date is now {local_date}, in the local client machine's timezone ({zone}). Rely on this reminder over any earlier date statement for the current date. DO NOT mention this to the user explicitly."
    ))
}

pub fn compaction_instruction() -> String {
    render(COMPACTION_INSTRUCTION, &[("custom_instruction_block", "")])
        .trim_end()
        .to_owned()
}

pub fn summary(text: &str) -> String {
    let text = text.trim();
    format!(
        "{}\n{}",
        SUMMARY_PREFIX.trim_end(),
        if text.is_empty() {
            "(no summary available)"
        } else {
            text
        }
    )
}

pub fn elision(omitted: u64) -> String {
    reminder(&format!(
        "Some of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly {omitted} tokens in between were dropped. The omitted content is covered by the compaction summary at the end of the conversation."
    ))
}

pub fn continuation() -> String {
    reminder(
        "Context compaction is complete — continue the work that was in progress when it began.",
    )
}

pub fn repeated(streak: u32) -> Option<String> {
    let text = match streak {
        0..=2 => return None,
        3..=4 => REPEAT_FIRST.to_owned(),
        5..=7 => format!(
            "The same tool call has now been issued {streak} times in a row. Choose exactly one of the following and state your choice before acting:\n(1) Falsification check: run the cheapest test that could conclusively disprove your current approach, if such a test exists.\n(2) Missing input: tell the user precisely what information or decision you need to proceed, and ask for it.\n(3) Conclude: deliver your best result based on the evidence already gathered, listing anything that remains uncertain."
        ),
        _ => REPEAT_FINAL.to_owned(),
    };
    Some(reminder(&text))
}

pub fn os_name(os: &str) -> String {
    match os {
        "macos" => "macOS".to_owned(),
        "linux" => "Linux".to_owned(),
        "windows" => "Windows".to_owned(),
        other => other.to_owned(),
    }
}

pub fn listing(directory: &Path) -> String {
    let Some((rows, total)) = entries(directory, ROOT_WIDTH) else {
        return "[not readable]".to_owned();
    };
    let remaining = total - rows.len();
    let mut lines = Vec::new();
    for (index, (name, is_dir)) in rows.iter().enumerate() {
        let last = index + 1 == rows.len() && remaining == 0;
        let connector = if last { "└── " } else { "├── " };
        if !is_dir {
            lines.push(format!("{connector}{name}"));
            continue;
        }
        lines.push(format!("{connector}{name}/"));
        if name.starts_with('.') {
            continue;
        }
        let prefix = if last { "    " } else { "│   " };
        let Some((children, child_total)) = entries(&directory.join(name), CHILD_WIDTH) else {
            lines.push(format!("{prefix}└── [not readable]"));
            continue;
        };
        let child_remaining = child_total - children.len();
        for (child_index, (child, child_dir)) in children.iter().enumerate() {
            let child_last = child_index + 1 == children.len() && child_remaining == 0;
            let connector = if child_last {
                "└── "
            } else {
                "├── "
            };
            let suffix = if *child_dir { "/" } else { "" };
            lines.push(format!("{prefix}{connector}{child}{suffix}"));
        }
        if child_remaining > 0 {
            lines.push(format!("{prefix}└── ... and {child_remaining} more"));
        }
    }
    if remaining > 0 {
        lines.push(format!("└── ... and {remaining} more entries"));
    }
    if lines.is_empty() {
        "(empty directory)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn entries(directory: &Path, width: usize) -> Option<(Vec<(String, bool)>, usize)> {
    let mut all = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            (entry.file_name().to_string_lossy().into_owned(), is_dir)
        })
        .collect::<Vec<_>>();
    all.sort_by(|(left, left_dir), (right, right_dir)| {
        right_dir
            .cmp(left_dir)
            .then_with(|| left.to_lowercase().cmp(&right.to_lowercase()))
            .then_with(|| left.cmp(right))
    });
    let total = all.len();
    all.truncate(width);
    Some((all, total))
}

pub fn agents_md(home: Option<&Path>, workspace: &Path) -> String {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut collect = |path: &Path| -> bool {
        let Ok(content) = std::fs::read_to_string(path) else {
            return false;
        };
        let key = path.display().to_string();
        if content.trim().is_empty() || found.iter().any(|(seen, _)| *seen == key) {
            return false;
        }
        found.push((key, content.trim().to_owned()));
        true
    };
    if let Some(home) = home {
        collect(&home.join(".kimi-code").join("AGENTS.md"));
        for name in ["AGENTS.md", "agents.md"] {
            if collect(&home.join(".agents").join(name)) {
                break;
            }
        }
    }
    let root = workspace
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(workspace);
    let mut chain = workspace
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .collect::<Vec<_>>();
    chain.reverse();
    for dir in chain {
        collect(&dir.join(".kimi-code").join("AGENTS.md"));
        for name in ["AGENTS.md", "agents.md"] {
            if collect(&dir.join(name)) {
                break;
            }
        }
    }
    found
        .iter()
        .map(|(path, content)| format!("<!-- From: {path} -->\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

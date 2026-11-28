//! Kimi Code's `ToolResultTruncationService.truncateForModel`: a tool result longer than 50 000 characters
//! (JavaScript string length, so UTF-16 code units) is saved to a file and replaced by a pointer with a
//! head and tail preview, or by the preview alone when it cannot be saved. Read is exempt.

use std::path::{Path, PathBuf};

const MAX_CHARS: usize = 50_000;
const MAX_RETAINED_CHARS: usize = 10_000_000;
const PREVIEW_HEAD_CHARS: usize = 4096;
const PREVIEW_TAIL_CHARS: usize = 1024;
const MAX_LINE_CHARS: usize = 2000;
const TRUNCATION_MARKER: &str = "[...truncated]";
const NEXT_STEP: &str =
    "next_step: Use Read with output_path to page through the saved output, or Grep to search it.";

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `sliceOnCodePointBoundary`: the longest prefix of at most `end` code units that splits no character.
fn head(text: &str, end: usize) -> &str {
    let mut taken = 0;
    for (index, char) in text.char_indices() {
        if taken + char.len_utf16() > end {
            return &text[..index];
        }
        taken += char.len_utf16();
    }
    text
}

/// The suffix of `text` starting at code unit `start`, moved past a character it would split
/// (`codePointBoundaryStart`), with that start in code units.
fn tail(text: &str, start: usize) -> (usize, &str) {
    let mut taken = 0;
    for (index, char) in text.char_indices() {
        if taken >= start {
            return (taken, &text[index..]);
        }
        taken += char.len_utf16();
    }
    (taken, "")
}

/// `shapeStringPerLine`: cuts every line longer than 2000 code units, keeping its line break.
fn shape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let body = rest.find(['\r', '\n']).unwrap_or(rest.len());
        let brk = match &rest[body..] {
            after if after.starts_with("\r\n") => 2,
            after if after.starts_with(['\r', '\n']) => 1,
            _ => 0,
        };
        let line = &rest[..body + brk];
        rest = &rest[body + brk..];
        if units(line) <= MAX_LINE_CHARS {
            out.push_str(line);
            continue;
        }
        let line_break = &line[line.trim_end_matches(['\r', '\n']).len()..];
        let suffix = format!("{TRUNCATION_MARKER}{line_break}");
        let keep = MAX_LINE_CHARS.max(units(&suffix)) - units(&suffix);
        out.push_str(head(line, keep));
        out.push_str(&suffix);
    }
    out
}

fn stem(tool: &str, call: &str) -> String {
    let mut stem = String::new();
    let mut gap = false;
    for char in format!("{tool}-{call}").chars() {
        if char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-') {
            stem.push(char);
            gap = false;
        } else if !gap {
            stem.push('_');
            gap = true;
        }
    }
    let stem = stem.trim_matches('_');
    match stem.chars().take(80).collect::<String>() {
        empty if empty.is_empty() => "tool-result".to_owned(),
        stem => stem,
    }
}

async fn save(
    directory: Option<&Path>,
    host: Option<&super::Host>,
    tool: &str,
    call: &str,
    text: &str,
) -> Option<PathBuf> {
    let directory = directory?;
    let path = directory.join(format!(
        "{}-{}.txt",
        stem(tool, call),
        nanocode_platform::uuid_v4()
    ));
    super::io::store(host, &path, text.as_bytes()).await.ok()?;
    Some(path)
}

fn preview(lines: &mut Vec<String>, text: &str) {
    let first = head(text, PREVIEW_HEAD_CHARS);
    let first_units = units(first);
    let total = units(text);
    let (start, last) = tail(
        text,
        first_units.max(total.saturating_sub(PREVIEW_TAIL_CHARS)),
    );
    lines.extend([
        "".to_owned(),
        format!("[preview: chars [0, {first_units})]"),
        first.to_owned(),
    ]);
    if !last.is_empty() {
        if start > first_units {
            lines.extend([
                "".to_owned(),
                format!("[elided: chars [{first_units}, {start})]"),
            ]);
        }
        lines.extend([
            "".to_owned(),
            format!("[preview: chars [{start}, {total})]"),
            last.to_owned(),
        ]);
    }
}

/// Applies Kimi Code's spill to `output`, saving into `directory`, through `host` when there is one (no
/// directory: the result cannot be saved, as on wasm without a host).
pub async fn truncate(
    tool: &str,
    call: &str,
    output: String,
    directory: Option<&Path>,
    host: Option<&super::Host>,
) -> String {
    let total = units(&output);
    if total <= MAX_CHARS {
        return output;
    }
    let retained = match total <= MAX_RETAINED_CHARS {
        true => output.as_str(),
        false => head(&output, MAX_RETAINED_CHARS),
    };
    let preserved = units(retained);
    let Some(path) = save(directory, host, tool, call, retained).await else {
        let mut lines = vec![
            format!(
                "Tool output exceeded {MAX_CHARS} characters and could not be saved to a file; only this preview is available."
            ),
            format!("tool_name: {tool}"),
            format!("tool_call_id: {call}"),
            format!("output_size_chars: {total}"),
        ];
        preview(&mut lines, retained);
        return lines.join("\n");
    };
    let shaped = shape(&output);
    if units(&shaped) <= MAX_CHARS {
        let pointer = [
            match preserved < total {
                true => format!("[Per-line truncation occurred; only the first {preserved} characters (of {total}) were saved to a file."),
                false => "[Per-line truncation occurred; the complete output was saved to a file.".to_owned(),
            },
            format!("output_path: {}", path.display()),
            format!("{NEXT_STEP}]"),
        ]
        .join("\n");
        return match shaped.ends_with('\n') || shaped.is_empty() {
            true => format!("{shaped}{pointer}"),
            false => format!("{shaped}\n{pointer}"),
        };
    }
    let partial = preserved < total;
    let mut lines = vec![
        match partial {
            true => format!(
                "Tool output exceeded {MAX_CHARS} characters; the first {preserved} characters (of {total}) were saved to a file."
            ),
            false => format!(
                "Tool output exceeded {MAX_CHARS} characters; the full output was saved to a file."
            ),
        },
        format!("tool_name: {tool}"),
        format!("tool_call_id: {call}"),
        match partial {
            true => format!(
                "output_size_chars: {total} (only the first {preserved} characters were preserved)"
            ),
            false => format!("output_size_chars: {total}"),
        },
        format!("output_size_bytes: {}", retained.len()),
        format!("output_path: {}", path.display()),
        NEXT_STEP.to_owned(),
    ];
    preview(&mut lines, retained);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{head, shape, stem, tail, truncate};

    #[test]
    fn slices_on_code_points() {
        assert_eq!(head("a😀b", 2), "a");
        assert_eq!(head("a😀b", 3), "a😀");
        assert_eq!(tail("a😀b", 2), (3, "b"));
        assert_eq!(tail("a😀b", 1), (1, "😀b"));
        assert_eq!(stem("mcp__eth__x", "call:a/1"), "mcp__eth__x-call_a_1");
    }

    #[test]
    fn shapes_long_lines() {
        let line = "x".repeat(2500);
        let shaped = shape(&format!("{line}\nshort\n{line}"));
        assert_eq!(
            shaped,
            format!(
                "{}[...truncated]\nshort\n{}[...truncated]",
                "x".repeat(1985),
                "x".repeat(1986)
            )
        );
    }

    #[test]
    fn previews_unsaved_output() {
        let output = "y".repeat(60_000);
        let text = nanocode_sync::block_on(truncate("Bash", "c1", output, None, None));
        assert!(text.starts_with("Tool output exceeded 50000 characters and could not be saved to a file; only this preview is available.\ntool_name: Bash\ntool_call_id: c1\noutput_size_chars: 60000\n\n[preview: chars [0, 4096)]\n"));
        assert!(
            text.contains("\n\n[elided: chars [4096, 58976)]\n\n[preview: chars [58976, 60000)]\n")
        );
    }
}

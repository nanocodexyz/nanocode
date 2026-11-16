use super::*;

const BASH_FIELDS: &[&str] = &[
    "command",
    "description",
    "timeout",
    "run_in_background",
    "dangerouslyDisableSandbox",
];
const BASH_IGNORED: &[&str] = &[
    "yield_time_ms",
    "max_output_tokens",
    "login",
    "tty",
    "shell",
];

const BASH_DIRECTORY: &[&str] = &["cwd", "workdir", "working_directory", "dir", "directory"];

const BASH_STDIN: &[&str] = &["stdin", "input"];

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn env_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}
pub(super) fn repair_bash(input: Value) -> Value {
    let Value::Object(mut fields) = input else {
        return input;
    };
    let Some(mut command) = fields
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Value::Object(fields);
    };
    let extra = fields
        .keys()
        .filter(|key| !BASH_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if extra.is_empty() {
        return Value::Object(fields);
    }
    let mut prefix = String::new();
    let mut stdin = None;
    for key in extra {
        let value = &fields[&key];
        let repaired = if BASH_IGNORED.contains(&key.as_str()) {
            true
        } else if BASH_DIRECTORY.contains(&key.as_str()) {
            match value.as_str().filter(|dir| !dir.trim().is_empty()) {
                Some(dir) => {
                    prefix.push_str(&format!("cd -- {} && ", quote(dir)));
                    true
                }
                None => false,
            }
        } else if key == "env" {
            let pairs = value.as_object().and_then(|env| {
                env.iter()
                    .map(|(name, value)| {
                        env_name(name)
                            .then(|| env_value(value))
                            .flatten()
                            .map(|value| format!("export {name}={}; ", quote(&value)))
                    })
                    .collect::<Option<Vec<_>>>()
            });
            match pairs {
                Some(pairs) => {
                    prefix.insert_str(0, &pairs.concat());
                    true
                }
                None => false,
            }
        } else if BASH_STDIN.contains(&key.as_str()) && stdin.is_none() {
            match value.as_str() {
                Some(text) => {
                    stdin = Some(text.to_owned());
                    true
                }
                None => false,
            }
        } else if key == "timeout_ms" && !fields.contains_key("timeout") && value.is_number() {
            fields.insert("timeout".into(), value.clone());
            true
        } else {
            false
        };
        if repaired {
            fields.remove(&key);
        }
    }
    command = format!("{prefix}{command}");
    if let Some(text) = stdin {
        let mut delimiter = "NANOCODE_STDIN".to_owned();
        while text.lines().any(|line| line == delimiter) || command.contains(&delimiter) {
            delimiter.push('_');
        }
        let body = text.strip_suffix('\n').unwrap_or(&text);
        command = format!("{{ {command}\n}} <<'{delimiter}'\n{body}\n{delimiter}");
    }
    fields.insert("command".into(), command.into());
    Value::Object(fields)
}

fn position(message: &str) -> Option<(usize, usize)> {
    let start = message.find("<input>:")? + "<input>:".len();
    let mut parts = message[start..].splitn(3, |c: char| !c.is_ascii_digit());
    let line = parts.next()?.parse().ok()?;
    let column = parts.next()?.parse().ok()?;
    Some((line, column))
}

fn excerpt(code: &str, line: usize, column: usize) -> Option<String> {
    let text = code.lines().nth(line.checked_sub(1)?)?;
    const WIDTH: usize = 160;
    let chars = text.chars().collect::<Vec<_>>();
    let at = column.saturating_sub(1).min(chars.len());
    let from = at.saturating_sub(WIDTH / 2);
    let to = (from + WIDTH).min(chars.len());
    let shown = chars[from..to].iter().collect::<String>();
    let caret = " ".repeat(at - from);
    Some(format!("Line {line} of your code:\n  {shown}\n  {caret}^"))
}

fn undefined_name(message: &str) -> Option<&str> {
    let rest = message.split("ReferenceError: '").nth(1)?;
    let end = rest.find("' is not defined")?;
    Some(&rest[..end])
}
pub(super) fn script_hint(code: &str, message: &str) -> Option<String> {
    let mut notes = Vec::new();
    if let Some((line, column)) = position(message)
        && let Some(text) = excerpt(code, line, column)
    {
        notes.push(text);
    }
    if let Some(name) = undefined_name(message) {
        notes.push(format!(
            "`{name}` is not defined in this cell. Each exec cell runs in a fresh context: variables, constants and functions from earlier cells do not carry over. Define `{name}` again in this cell, or save it with store(\"{name}\", value) in the cell that makes it and read it back with load(\"{name}\")."
        ));
    } else if message.contains("SyntaxError") && message.contains("template") {
        notes.push("Inside a JavaScript template literal (backticks), `${` starts a JavaScript expression, so a shell `${VAR}` or `${VAR#...}` breaks the script. Write it as `\\${VAR}`, or build the command with plain string concatenation.".into());
    } else if message.contains("SyntaxError") {
        notes.push("The script did not parse, so nothing ran. Check quotes, backticks and braces near that position; for long file contents, String.raw with backticks avoids most escaping mistakes.".into());
    }
    (!notes.is_empty()).then(|| notes.join("\n\n"))
}

fn script_error(content: &ToolResultContent) -> Option<String> {
    match content {
        ToolResultContent::Text(text) => {
            text.find("Script error:\n").map(|at| text[at..].to_owned())
        }
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .find(|text| text.starts_with("Script error:\n"))
            .map(str::to_owned),
    }
}

pub(super) fn bash_handler(handler: Handler) -> Handler {
    Arc::new(move |input, invocation| handler(repair_bash(input), invocation))
}

pub(super) fn exec_handler(handler: Handler) -> Handler {
    Arc::new(move |input, invocation| {
        let code = input.get("code").and_then(Value::as_str).map(str::to_owned);
        let handler = handler.clone();
        Box::pin(async move {
            let mut reply = handler(input, invocation).await?;
            if let Some(code) = code
                && let Some(message) = script_error(&reply.content)
                && let Some(hint) = script_hint(&code, &message)
            {
                crate::deadline::append(&mut reply.content, &hint);
            }
            Ok(reply)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(input: Value) -> Value {
        repair_bash(input)
    }

    #[test]
    fn known_fields_pass_through() {
        let input = json!({"command": "ls", "timeout": 5000, "description": "list"});
        assert_eq!(command(input.clone()), input);
    }

    #[test]
    fn cwd_env_and_ignored_keys_become_the_command() {
        let out = command(json!({
            "command": "make test",
            "cwd": "/app/it's here",
            "env": {"CC": "clang", "JOBS": 4},
            "yield_time_ms": 30000,
        }));
        assert_eq!(
            out,
            json!({"command": r"export CC='clang'; export JOBS='4'; cd -- '/app/it'\''s here' && make test"})
        );
    }

    #[test]
    fn stdin_becomes_a_heredoc_with_an_unused_delimiter() {
        let out = command(json!({"command": "python3 -", "stdin": "print(1)\nNANOCODE_STDIN\n"}));
        assert_eq!(
            out["command"],
            "{ python3 -\n} <<'NANOCODE_STDIN_'\nprint(1)\nNANOCODE_STDIN\nNANOCODE_STDIN_"
        );
        assert_eq!(out.as_object().unwrap().len(), 1);
    }

    #[test]
    fn timeout_ms_renames_only_without_timeout() {
        assert_eq!(
            command(json!({"command": "x", "timeout_ms": 10})),
            json!({"command": "x", "timeout": 10})
        );
        let both = json!({"command": "x", "timeout": 5, "timeout_ms": 10});
        assert_eq!(command(both.clone()), both);
    }

    #[test]
    fn unrepairable_keys_stay_for_the_tool_to_reject() {
        let bad_env = json!({"command": "x", "env": {"1BAD": "v"}});
        assert_eq!(command(bad_env.clone()), bad_env);
        let unknown = json!({"command": "x", "frobnicate": true, "cwd": "/a"});
        assert_eq!(
            command(unknown),
            json!({"command": "cd -- '/a' && x", "frobnicate": true})
        );
        let no_command = json!({"cwd": "/a"});
        assert_eq!(command(no_command.clone()), no_command);
    }

    #[test]
    fn reference_error_names_the_line_and_the_fresh_context() {
        let code = "const a = 1;\nlet s = src2.replace('x', 'y');";
        let message = "Script error:\nReferenceError: 'src2' is not defined\n    at anonymous (<input>:2:9)\n    at runCell (nanocode:bootstrap:825:28)";
        let hint = script_hint(code, message).unwrap();
        assert!(
            hint.starts_with(
                "Line 2 of your code:\n  let s = src2.replace('x', 'y');\n          ^"
            ),
            "{hint}"
        );
        assert!(hint.contains("fresh context"));
        assert!(hint.contains("store(\"src2\", value)"));
    }

    #[test]
    fn template_syntax_error_explains_shell_expansions() {
        let code = "await tools.Bash({ command: \x60echo ${ref#refs/heads/}\x60 });";
        let message = "Script error:\nSyntaxError: expected '}' after template expression\n    at <input>:1:37\n    at AsyncFunction (native)";
        let hint = script_hint(code, message).unwrap();
        assert!(hint.contains("Line 1 of your code"));
        assert!(hint.contains("template literal"));
    }

    #[test]
    fn other_errors_get_no_hint() {
        assert_eq!(script_hint("x", "Script error:\nError: boom"), None);
    }

    #[test]
    fn script_error_is_found_in_blocks_and_text() {
        let blocks = ToolResultContent::Blocks(vec![
            json!({"type": "text", "text": "Script failed\nWall time 0.0 seconds\nOutput:\n"}),
            json!({"type": "text", "text": "Script error:\nReferenceError: 'a' is not defined"}),
        ]);
        assert!(script_error(&blocks).is_some());
        let ok = ToolResultContent::Text("Script completed".into());
        assert!(script_error(&ok).is_none());
    }
}

//! Functions related to adding and removing indentation from lines of
//! text.

use std::borrow::Cow;
use std::cmp;

use ruff_source_file::UniversalNewlines;

use crate::PythonWhitespace;

/// Indent each line by the given prefix.
///
/// # Examples
///
/// ```
/// # use ruff_python_trivia::textwrap::indent;
///
/// assert_eq!(indent("First line.\nSecond line.\n", "  "),
///            "  First line.\n  Second line.\n");
/// ```
///
/// When indenting, trailing whitespace is stripped from the prefix.
/// This means that empty lines remain empty afterwards:
///
/// ```
/// # use ruff_python_trivia::textwrap::indent;
///
/// assert_eq!(indent("First line.\n\n\nSecond line.\n", "  "),
///            "  First line.\n\n\n  Second line.\n");
/// ```
///
/// Notice how `"\n\n\n"` remained as `"\n\n\n"`.
///
/// This feature is useful when you want to indent text and have a
/// space between your prefix and the text. In this case, you _don't_
/// want a trailing space on empty lines:
///
/// ```
/// # use ruff_python_trivia::textwrap::indent;
///
/// assert_eq!(indent("foo = 123\n\nprint(foo)\n", "# "),
///            "# foo = 123\n#\n# print(foo)\n");
/// ```
///
/// Notice how `"\n\n"` became `"\n#\n"` instead of `"\n# \n"` which
/// would have trailing whitespace.
///
/// Leading and trailing whitespace coming from the text itself is
/// kept unchanged:
///
/// ```
/// # use ruff_python_trivia::textwrap::indent;
///
/// assert_eq!(indent(" \t  Foo   ", "->"), "-> \t  Foo   ");
/// ```
pub fn indent<'a>(text: &'a str, prefix: &str) -> Cow<'a, str> {
    if prefix.is_empty() {
        return Cow::Borrowed(text);
    }

    let mut result = String::with_capacity(text.len() + prefix.len());
    let trimmed_prefix = prefix.trim_whitespace_end();
    for line in text.universal_newlines() {
        if line.trim_whitespace().is_empty() {
            result.push_str(trimmed_prefix);
        } else {
            result.push_str(prefix);
        }
        result.push_str(line.as_full_str());
    }
    Cow::Owned(result)
}

/// Indent only the first line by the given prefix.
///
/// This function is useful when you want to indent the first line of a multi-line
/// expression while preserving the relative indentation of subsequent lines.
///
/// # Examples
///
/// ```
/// # use ruff_python_trivia::textwrap::indent_first_line;
///
/// assert_eq!(indent_first_line("First line.\nSecond line.\n", "  "),
///            "  First line.\nSecond line.\n");
/// ```
///
/// When indenting, trailing whitespace is stripped from the prefix.
/// This means that empty lines remain empty afterwards:
///
/// ```
/// # use ruff_python_trivia::textwrap::indent_first_line;
///
/// assert_eq!(indent_first_line("\n\n\nSecond line.\n", "  "),
///            "\n\n\nSecond line.\n");
/// ```
///
/// Leading and trailing whitespace coming from the text itself is
/// kept unchanged:
///
/// ```
/// # use ruff_python_trivia::textwrap::indent_first_line;
///
/// assert_eq!(indent_first_line(" \t  Foo   ", "->"), "-> \t  Foo   ");
/// ```
pub fn indent_first_line<'a>(text: &'a str, prefix: &str) -> Cow<'a, str> {
    if prefix.is_empty() {
        return Cow::Borrowed(text);
    }

    let mut lines = text.universal_newlines();
    let Some(first_line) = lines.next() else {
        return Cow::Borrowed(text);
    };

    let mut result = String::with_capacity(text.len() + prefix.len());

    // Indent only the first line
    if first_line.trim_whitespace().is_empty() {
        result.push_str(prefix.trim_whitespace_end());
    } else {
        result.push_str(prefix);
    }
    result.push_str(first_line.as_full_str());

    // Add remaining lines without indentation
    for line in lines {
        result.push_str(line.as_full_str());
    }

    Cow::Owned(result)
}

/// Removes common leading whitespace from each line.
///
/// This function will look at each non-empty line and determine the
/// maximum amount of whitespace that can be removed from all lines.
///
/// Lines that consist solely of whitespace are trimmed to a blank line.
///
/// ```
/// # use ruff_python_trivia::textwrap::dedent;
///
/// assert_eq!(dedent("
///     1st line
///       2nd line
///     3rd line
/// "), "
/// 1st line
///   2nd line
/// 3rd line
/// ");
/// ```
pub fn dedent(text: &str) -> Cow<'_, str> {
    // Find the minimum amount of leading whitespace on each line.
    let prefix_len = text
        .universal_newlines()
        .fold(usize::MAX, |prefix_len, line| {
            let leading_whitespace_len = line.len() - line.trim_whitespace_start().len();
            if leading_whitespace_len == line.len() {
                // Skip empty lines.
                prefix_len
            } else {
                cmp::min(prefix_len, leading_whitespace_len)
            }
        });

    // If there is no common prefix, no need to dedent.
    if prefix_len == usize::MAX {
        return Cow::Borrowed(text);
    }

    // Remove the common prefix from each line.
    let mut result = String::with_capacity(text.len());
    for line in text.universal_newlines() {
        if line.trim_whitespace().is_empty() {
            if let Some(line_ending) = line.line_ending() {
                result.push_str(&line_ending);
            }
        } else {
            result.push_str(&line.as_full_str()[prefix_len..]);
        }
    }
    Cow::Owned(result)
}

/// Reduce a block's indentation to match the provided indentation.
///
/// This function looks at the first line in the block to determine the
/// current indentation, then removes whitespace from each line to
/// match the provided indentation.
///
/// Leading comments are ignored unless the block is only composed of comments.
///
/// Lines that are indented by _less_ than the indent of the first line
/// are left unchanged.
///
/// Lines that consist solely of whitespace are trimmed to a blank line.
///
/// Lines that start with formfeeds have the indentation after the formfeeds
/// removed and the formfeeds reinstated
///
/// # Panics
/// If the first line is indented by less than the provided indent.
pub fn dedent_to(text: &str, indent: &str) -> Option<String> {
    // The caller may provide an `indent` from source code by taking
    // a range of text beginning with the start of a line. In Python,
    // while a line may begin with form feeds, these do not contribute
    // to the indentation. So we strip those here.
    let indent = indent.trim_start_matches('\x0C');
    // Look at the indentation of the first non-empty line, to determine the "baseline" indentation.
    let mut first_comment_indent = None;
    let existing_indent_len = text
        .universal_newlines()
        .find_map(|line| {
            // Following Python's lexer, treat form feed character's at the start of a line
            // the same as a line break (reset the indentation)
            let trimmed_start_of_line_formfeed = line.trim_start_matches('\x0C');
            let trimmed = trimmed_start_of_line_formfeed.trim_whitespace_start();

            // A whitespace only line
            if trimmed.is_empty() {
                return None;
            }

            let indent_len = trimmed_start_of_line_formfeed.len() - trimmed.len();

            if trimmed.starts_with('#') && first_comment_indent.is_none() {
                first_comment_indent = Some(indent_len);
                None
            } else {
                Some(indent_len)
            }
        })
        .unwrap_or(first_comment_indent.unwrap_or_default());

    if existing_indent_len < indent.len() {
        return None;
    }

    // Determine the amount of indentation to remove.
    let dedent_len = existing_indent_len - indent.len();

    let mut result = String::with_capacity(text.len() + indent.len());

    for line in text.universal_newlines() {
        let line_content = line.trim_start_matches('\x0C');
        let formfeed_count = line.len() - line_content.len();

        let line_ending = if let Some(line_ending) = line.line_ending() {
            line_ending.as_str()
        } else {
            ""
        };

        let line_without_indent = line.trim_whitespace_start();

        if line_without_indent.is_empty() {
            result.push_str(line_ending);
            continue;
        }

        // Determine the current indentation level.
        let current_indent_len = line_content.len() - line_without_indent.len();

        if current_indent_len < existing_indent_len {
            // If the current indentation level is less than the baseline, keep it as is.
            result.push_str(line.as_full_str());
            continue;
        }
        let dedented_content = &line_content[dedent_len..];

        let formfeeds = &line[..formfeed_count];
        result.push_str(formfeeds);
        result.push_str(dedented_content);
        result.push_str(line_ending);
    }
    Some(result)
}

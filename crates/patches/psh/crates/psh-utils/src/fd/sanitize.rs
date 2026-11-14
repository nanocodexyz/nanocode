// Copied from fd (https://github.com/sharkdp/fd, commit 193d0df), src/sanitize.rs:
// Copyright (c) 2017-present The fd developers. Licensed under the MIT License (see NOTICE).

//! TTY-output sanitization to prevent terminal escape injection via filenames.

use std::fmt::{Display, Formatter, Write};

/// True for any char that is neither printable nor permitted whitespace (only HT).
/// Covers C0/C1/DEL, bidi overrides, zero-width and format chars, and tag chars.
#[inline]
fn needs_escape(c: char) -> bool {
    if c == '\t' {
        return false;
    }
    c.is_control()
        || matches!(c,
            '\u{00AD}'                  // soft hyphen (invisible)
            | '\u{180E}'                // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}'   // zero-width + LRM/RLM
            | '\u{202A}'..='\u{202E}'   // bidi embedding/override
            | '\u{2060}'..='\u{206F}'   // word joiner, invisibles, deprecated formats
            | '\u{FEFF}'                // BOM / zero-width no-break space
            | '\u{FFF9}'..='\u{FFFB}'   // interlinear annotation
            | '\u{E0000}'..='\u{E007F}' // language tags
        )
}

/// Write the sanitized contents of `raw` to `f`.
pub fn write_sanitized(f: &mut std::fmt::Formatter<'_>, raw: &str) -> std::fmt::Result {
    // Would it be faster to do a pass to see if we don't need an escape and write
    // the whole string first? Maybe with a faster check just for non-control ASCII?
    for c in raw.chars() {
        if needs_escape(c) {
            let v = c as u32;
            if v <= 0xFF {
                write!(f, "\\x{v:02X}")?;
            } else {
                write!(f, "\\u{{{v:04X}}}")?;
            }
        } else {
            f.write_char(c)?;
        }
    }
    Ok(())
}

// TODO: add something to sanitize paths directly instead of as strings.

/// A wrapper type that sanitizes the output to Display
pub(crate) struct SanitizedStr<'a> {
    raw: &'a str,
    /// If true, the content is safe to output without sanitizing,
    /// either because it is trusted, or because it isn't going to a terminal.
    is_safe: bool,
}

impl<'a> Display for SanitizedStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.is_safe {
            // We don't need to sanitize anything, just forward
            self.raw.fmt(f)
        } else {
            write_sanitized(f, self.raw)
        }
    }
}

/// Sanitize for terminal output only; raw bytes pass through on pipes/files.
pub fn sanitize_for_term(raw: &str, is_terminal: bool) -> SanitizedStr<'_> {
    SanitizedStr {
        raw,
        is_safe: !is_terminal,
    }
}

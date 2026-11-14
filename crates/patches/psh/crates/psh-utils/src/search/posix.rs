//! POSIX BRE/ERE for the search utilities (D1131): `psh_regex` parses (GNU syntax,
//! errors and bracket rules) and translates to Rust's regex syntax; the ripgrep crates
//! match. Patterns the translation cannot carry (back-references) or whose spans can
//! differ (leftmost-longest) are matched by `psh_regex` itself where spans matter.

pub use psh_regex::{Error, Options, Translation, to_rust_syntax};

/// Translates `pattern` for a line-oriented search (`grep`): newline-sensitive, UTF-8.
pub fn for_lines(pattern: &[u8], extended: bool) -> Result<Translation, Error> {
    to_rust_syntax(
        pattern,
        Options {
            extended,
            icase: false,
            newline: true,
            utf8: true,
        },
    )
}

//! Unicode normalization (`unicodedata.normalize` / `is_normalized`).

// spell-checker:ignore nfkc

use core::str::FromStr;

use rustpython_wtf8::{Wtf8, Wtf8Buf, Wtf8Chunk};
use unicode_normalization::UnicodeNormalization;

/// One of the four Unicode normalization forms.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NormalizeForm {
    Nfc,
    Nfkc,
    Nfd,
    Nfkd,
}

impl FromStr for NormalizeForm {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "NFC" => Ok(Self::Nfc),
            "NFKC" => Ok(Self::Nfkc),
            "NFD" => Ok(Self::Nfd),
            "NFKD" => Ok(Self::Nfkd),
            _ => Err(()),
        }
    }
}

/// Normalize `text` to `form` (`unicodedata.normalize`).
///
/// Lone surrogates are passed through unchanged; only the valid UTF-8 runs are
/// normalized.
#[must_use]
pub fn normalize(form: NormalizeForm, text: &Wtf8) -> Wtf8Buf {
    match form {
        NormalizeForm::Nfc => text.map_utf8(UnicodeNormalization::nfc).collect(),
        NormalizeForm::Nfkc => text.map_utf8(UnicodeNormalization::nfkc).collect(),
        NormalizeForm::Nfd => text.map_utf8(UnicodeNormalization::nfd).collect(),
        NormalizeForm::Nfkd => text.map_utf8(UnicodeNormalization::nfkd).collect(),
    }
}

/// Whether `text` is already in `form` (`unicodedata.is_normalized`).
///
/// Lone surrogates split the text into valid UTF-8 runs; each run is checked
/// independently, matching the run-wise normalization performed by [`normalize`].
#[must_use]
pub fn is_normalized(form: NormalizeForm, text: &Wtf8) -> bool {
    let check: fn(&str) -> bool = match form {
        NormalizeForm::Nfc => |s| s.chars().eq(s.nfc()),
        NormalizeForm::Nfkc => |s| s.chars().eq(s.nfkc()),
        NormalizeForm::Nfd => |s| s.chars().eq(s.nfd()),
        NormalizeForm::Nfkd => |s| s.chars().eq(s.nfkd()),
    };
    text.chunks().all(|chunk| match chunk {
        Wtf8Chunk::Utf8(s) => check(s),
        Wtf8Chunk::Surrogate(_) => true,
    })
}

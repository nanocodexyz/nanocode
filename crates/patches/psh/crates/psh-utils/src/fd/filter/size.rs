// Copied from fd (https://github.com/sharkdp/fd, commit 193d0df), src/filter/size.rs,
// with the psh patches marked `psh:` (D1138):
// Copyright (c) 2017-present The fd developers. Licensed under the MIT License (see NOTICE).

use psh_pattern::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeFilter {
    Max(u64),
    Min(u64),
    Equals(u64),
}

// SI prefixes (powers of 10)
const KILO: u64 = 1000;
const MEGA: u64 = KILO * 1000;
const GIGA: u64 = MEGA * 1000;
const TERA: u64 = GIGA * 1000;

// Binary prefixes (powers of 2)
const KIBI: u64 = 1024;
const MEBI: u64 = KIBI * 1024;
const GIBI: u64 = MEBI * 1024;
const TEBI: u64 = GIBI * 1024;

impl SizeFilter {
    // psh: the message as a `String` (no anyhow).
    pub fn from_string(s: &str) -> Result<Self, String> {
        Self::parse_opt(s)
            .ok_or_else(|| format!("'{}' is not a valid size constraint. See 'fd --help'.", s))
    }

    fn parse_opt(s: &str) -> Option<Self> {
        let pattern = Regex::new(r"(?i)^([+-]?)(\d+)(b|[kmgt]i?b?)$").unwrap();
        if !pattern.is_match(s) {
            return None;
        }

        let captures = pattern.captures(s)?;
        let limit_kind = captures.get(1).map_or("+", |m| m.as_str());
        let quantity = captures
            .get(2)
            .and_then(|v| v.as_str().parse::<u64>().ok())?;

        let multiplier = match &captures.get(3).map_or("b", |m| m.as_str()).to_lowercase()[..] {
            v if v.starts_with("ki") => KIBI,
            v if v.starts_with('k') => KILO,
            v if v.starts_with("mi") => MEBI,
            v if v.starts_with('m') => MEGA,
            v if v.starts_with("gi") => GIBI,
            v if v.starts_with('g') => GIGA,
            v if v.starts_with("ti") => TEBI,
            v if v.starts_with('t') => TERA,
            "b" => 1,
            _ => return None,
        };

        let size = quantity.checked_mul(multiplier)?;

        match limit_kind {
            "+" => Some(Self::Min(size)),
            "-" => Some(Self::Max(size)),
            "" => Some(Self::Equals(size)),
            _ => None,
        }
    }

    pub fn is_within(&self, size: u64) -> bool {
        match *self {
            Self::Max(limit) => size <= limit,
            Self::Min(limit) => size >= limit,
            Self::Equals(limit) => size == limit,
        }
    }
}

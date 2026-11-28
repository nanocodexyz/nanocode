mod collate;
mod data;
mod date;
mod displaynames;
mod segdata;
mod segdict;
mod segmenter;
mod dndata;
mod locale;
mod locdata;
mod locdata_avail;
mod number;
mod numdata;
mod numsys;
mod numtext;
mod plurdata;
mod text;
mod tzdata;

use crate::heap::{Heap, ObjectClass};
use crate::interp::conv::{Fault, type_error};
use crate::value::Value;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;



pub(crate) fn range<T>(msg: &str) -> Result<T, Fault> {
    crate::builtins::typed::range_error(msg)
}

pub(crate) fn type_err<T>(msg: &str) -> Result<T, Fault> {
    type_error(msg)
}

pub(crate) fn parse_f64(s: &str) -> f64 {
    match number::parse_decimal(s) {
        number::Num::Dec(d) => {
            let mut v = 0.0;
            for (i, x) in d.digits.iter().enumerate() {
                v += f64::from(*x) * pow10(d.exp - 1 - i as i32);
            }
            if d.neg { -v } else { v }
        }
        number::Num::Inf(n) => if n { f64::NEG_INFINITY } else { f64::INFINITY },
        number::Num::Nan => f64::NAN,
    }
}

fn pow10(e: i32) -> f64 {
    let mut v = 1.0;
    for _ in 0..e.unsigned_abs() {
        v *= 10.0;
    }
    if e < 0 { 1.0 / v } else { v }
}

#[derive(Default)]
pub(crate) struct Spec {
    pairs: Vec<(String, String)>,
    later: Vec<(String, String)>,
}

impl Spec {
    pub fn parse(s: &str) -> Spec {
        let mut spec = Spec::default();
        for pair in s.split('\u{1}') {
            if let Some((k, v)) = pair.split_once('\u{2}') {
                spec.pairs.push((k.to_string(), v.to_string()));
            }
        }
        spec
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    pub fn int(&self, k: &str, default: u32) -> u32 {
        self.get(k)
            .and_then(|v| v.trim_start_matches('#').parse::<u32>().ok())
            .unwrap_or(default)
    }
    pub fn put(&mut self, k: &str, v: &str) {
        self.pairs.push((k.to_string(), v.to_string()));
    }
    /// Keeps k but hides it from resolvedOptions (its name gains a leading @).
    pub fn hide(&mut self, k: &str) {
        for (n, _) in self.pairs.iter_mut().filter(|(n, _)| n == k) {
            n.insert(0, '@');
        }
    }
    pub fn num(&mut self, k: &str, v: u32) {
        self.put(k, &format!("#{v}"));
    }
    pub fn later(&mut self, k: &str, v: &str) {
        self.later.push((k.to_string(), v.to_string()));
    }
    pub fn flush_later(&mut self) {
        let later = core::mem::take(&mut self.later);
        self.pairs.extend(later);
    }
    pub fn choose(&self, k: &str, values: &[&'static str], default: &'static str, ctor: &str) -> Result<&'static str, Fault> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => match values.iter().find(|x| **x == v) {
                Some(x) => Ok(x),
                None => range(&format!("Value {v} out of range for {ctor} options property {k}")),
            },
        }
    }
    pub fn choose_opt(&self, k: &str, values: &[&'static str], ctor: &str) -> Result<Option<&'static str>, Fault> {
        match self.get(k) {
            None => Ok(None),
            Some(_) => self.choose(k, values, "", ctor).map(Some),
        }
    }
    pub fn encode(&self) -> String {
        let mut s = String::new();
        for (i, (k, v)) in self.pairs.iter().enumerate() {
            if i > 0 {
                s.push('\u{1}');
            }
            s.push_str(k);
            s.push('\u{2}');
            s.push_str(v);
        }
        s
    }
}

#[derive(Default)]
pub(crate) struct Parts {
    pub items: Vec<(&'static str, String)>,
}

impl Parts {
    pub fn push(&mut self, ty: &'static str, v: &str) {
        if v.is_empty() {
            return;
        }
        if ty == "literal"
            && let Some((t, last)) = self.items.last_mut()
            && *t == "literal"
        {
            last.push_str(v);
            return;
        }
        self.items.push((ty, v.to_string()));
    }
    pub fn extend(&mut self, other: &Parts) {
        for (t, v) in &other.items {
            self.push(t, v);
        }
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (_, v) in &self.items {
            s.push_str(v);
        }
        s
    }
    pub fn encode(&self) -> String {
        let mut s = String::new();
        for (i, (t, v)) in self.items.iter().enumerate() {
            if i > 0 {
                s.push('\u{1}');
            }
            s.push_str(t);
            s.push('\u{2}');
            s.push_str(v);
        }
        s
    }
}

/// i, v and f are CLDR's operands, n the whole value as ICU holds it (a double).
pub(crate) fn plural_en_like(lang: &str, i: u64, v: u32, f: u64, n: f64, ordinal: bool) -> &'static str {
    if let Some(c) = text::rule_plural(lang, i, v, f, n, ordinal) {
        return c;
    }
    // n is the whole value: an integer when its visible fraction is zero.
    let n_int = f == 0;
    if ordinal {
        if !n_int {
            return "other";
        }
        return match lang {
            "fr" | "vi" => if i == 1 { "one" } else { "other" },
            "it" => if matches!(i, 8 | 11 | 80 | 800) { "many" } else { "other" },
            "hi" => match i {
                1 => "one",
                2 | 3 => "two",
                4 => "few",
                6 => "many",
                _ => "other",
            },
            "en" if v == 0 => en_ordinal(i),
            _ => "other",
        };
    }
    match lang {
        "ja" | "zh" | "ko" | "id" | "vi" => "other",
        "ar" if n_int => match n {
            0.0 => "zero",
            1.0 => "one",
            2.0 => "two",
            _ if (3.0..=10.0).contains(&(n % 100.0)) => "few",
            _ if (11.0..=99.0).contains(&(n % 100.0)) => "many",
            _ => "other",
        },
        "ar" => "other",
        "hi" => if i == 0 || (i == 1 && n_int) { "one" } else { "other" },
        "tr" => if i == 1 && n_int { "one" } else { "other" },
        "pl" => {
            if v != 0 {
                return "other";
            }
            match (i, i % 10, i % 100) {
                (1, _, _) => "one",
                (_, 2..=4, m) if !(12..=14).contains(&m) => "few",
                _ => "many",
            }
        }
        _ => plural_other_langs(lang, i, v, f),
    }
}

fn en_ordinal(i: u64) -> &'static str {
    {
        match (i % 10, i % 100) {
            (1, m) if m != 11 => "one",
            (2, m) if m != 12 => "two",
            (3, m) if m != 13 => "few",
            _ => "other",
        }
    }
}

fn plural_other_langs(lang: &str, i: u64, v: u32, f: u64) -> &'static str {
    match lang {
        "fr" | "pt" => if i <= 1 { "one" } else if v == 0 && i != 0 && i.is_multiple_of(1_000_000) { "many" } else { "other" },
        "es" | "it" => if i == 1 && (v == 0 || (lang == "es" && f == 0)) { "one" } else if v == 0 && i != 0 && i.is_multiple_of(1_000_000) { "many" } else { "other" },
        "ru" => {
            if v != 0 {
                return "other";
            }
            match (i % 10, i % 100) {
                (1, m) if m != 11 => "one",
                (2..=4, m) if !(12..=14).contains(&m) => "few",
                _ => "many",
            }
        }
        _ => if i == 1 && v == 0 { "one" } else { "other" },
    }
}

/// The categories PluralRules' resolvedOptions lists, in ICU's order (sorted, other last).
pub(crate) fn plural_categories(lang: &str, ordinal: bool) -> &'static str {
    if let Some(c) = text::rule_categories(lang, ordinal) {
        return c;
    }
    if ordinal {
        return match lang {
            "en" => "few,one,two,other",
            "fr" | "vi" => "one,other",
            "it" => "many,other",
            "hi" => "few,many,one,two,other",
            _ => "other",
        };
    }
    match lang {
        "ja" | "zh" | "ko" | "id" | "vi" => "other",
        "fr" | "pt" | "es" | "it" => "many,one,other",
        "ru" | "pl" => "few,many,one,other",
        "ar" => "few,many,one,two,zero,other",
        _ => "one,other",
    }
}

/// The Intl kinds of Locale (its spec is its tag) and of Intl.supportedValuesOf's lists.
const LOCALE: i32 = 16;
const SUPPORTED_VALUES: i32 = 15;
const DISPLAY_NAMES: i32 = 17;
const SEGMENTER: i32 = 18;

pub(crate) const LANGS: [&str; 11] = ["en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru"];

/// The languages NumberFormat, PluralRules, ListFormat and RelativeTimeFormat have data for.
pub(crate) const NUM_LANGS: [&str; 38] = [
    "en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru", "pl", "tr", "ar", "hi", "id", "vi", "hu", "nb", "no", "he", "ta", "sr", "cs", "uk", "sv",
    "da", "ms", "ro", "bn", "el", "sw", "ur", "fil", "ca", "fa", "th", "fi",
];

fn canonical_list(tags: &str) -> Result<Vec<String>, Fault> {
    let mut out: Vec<String> = Vec::new();
    if tags.is_empty() {
        return Ok(out);
    }
    for t in tags.split('\n') {
        let c = locale::canonicalize(t)?;
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

pub(crate) fn resolve_locale(tags: &[String], langs: &[&str]) -> String {
    for t in tags {
        let base: Vec<&str> = t.split('-').take_while(|s| s.len() != 1).collect();
        if langs.contains(&base[0]) {
            let mut keep: Vec<&str> = alloc::vec![base[0]];
            for s in &base[1..] {
                if (s.len() == 4 && keep.len() == 1) || ((s.len() == 2 || s.len() == 3) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())) {
                    keep.push(s);
                }
            }
            return keep.join("-");
        }
    }
    "en-US".to_string()
}

fn str_arg(heap: &Heap, v: Value) -> Option<String> {
    v.as_string().map(|s| heap.str_to_string(s))
}

fn string_value(heap: &mut Heap, s: &str) -> Result<Value, Fault> {
    Ok(Value::string(heap.alloc_str(s)?))
}

pub(crate) fn intrinsic(heap: &mut Heap, k: u16, a: [Value; 5]) -> Result<Option<Value>, Fault> {
    Ok(Some(match k {
        0 => {
            let (Some(proto), Some(kind), Some(spec)) = (a[0].as_object(), a[1].as_int(), a[2].as_string()) else {
                return Ok(None);
            };
            let o = heap.alloc_object(ObjectClass::Intl, Some(proto), 3)?;
            heap.set_object_slot(o, 0, Value::int(kind));
            heap.set_object_slot(o, 1, Value::string(spec));
            heap.set_object_slot(o, 2, Value::UNDEFINED);
            Value::object(o)
        }
        1 => match a[0].as_object() {
            Some(o) if heap.object_class(o) == ObjectClass::Intl && heap.object_slot(o, 0) == a[1] => heap.object_slot(o, 1),
            _ => Value::UNDEFINED,
        },
        2 => {
            let Some(o) = a[0].as_object() else { return Ok(None) };
            if a[1] != Value::UNDEFINED {
                heap.set_object_slot(o, 2, a[1]);
            }
            heap.object_slot(o, 2)
        }
        3 => {
            let (Some(kind), Some(raw)) = (a[0].as_int(), str_arg(heap, a[2])) else {
                return Ok(None);
            };
            if kind == LOCALE {
                return Ok(Some(string_value(heap, &locale::construct(&str_arg(heap, a[1]).unwrap_or_default(), &Spec::parse(&raw))?)?));
            }
            let tags = canonical_list(&str_arg(heap, a[1]).unwrap_or_default())?;
            let collator_langs: Vec<&str> = LANGS.iter().chain(collate::TAILORED.iter()).copied().collect();
            // Segmenter resolves among the languages its supportedLocalesOf accepts.
            let segmenter_langs: Vec<&str> = locdata_avail::AVAILABLE.iter().filter(|e| e.1 & 1 << 7 != 0).map(|e| e.0).collect();
            let locale = resolve_locale(&tags, match kind {
                1 => &date::LANGS[..],
                2 => &collator_langs[..],
                0 | 3 | 4 | 5 => &NUM_LANGS[..],
                DISPLAY_NAMES => &dndata::LANGS[..],
                SEGMENTER => &segmenter_langs[..],
                _ => &LANGS[..],
            });
            let raw = Spec::parse(&raw);
            // Every constructor reads localeMatcher first and accepts only its two values.
            let ctor = match kind {
                0 => "NumberFormat",
                1 => "DateTimeFormat",
                2 => "Collator",
                3 => "PluralRules",
                4 => "ListFormat",
                5 => "RelativeTimeFormat",
                DISPLAY_NAMES => "DisplayNames",
                _ => "Segmenter",
            };
            raw.choose("localeMatcher", &["lookup", "best fit"], "best fit", &format!("Intl.{ctor}"))?;
            let spec = match kind {
                0 => number::resolve(&locale, &tags, &raw)?,
                1 => date::resolve(&locale, &tags, &raw)?,
                2 => collate::resolve(&locale, &tags, &raw)?,
                DISPLAY_NAMES => displaynames::resolve(&locale, &raw)?,
                SEGMENTER => segmenter::resolve(&locale, &raw)?,
                _ => text::resolve(kind, &locale, &tags, &raw)?,
            };
            string_value(heap, &spec.encode())?
        }
        4 => {
            let (Some(kind), Some(spec)) = (a[0].as_int(), str_arg(heap, a[1])) else {
                return Ok(None);
            };
            if kind == LOCALE {
                let what = str_arg(heap, a[2]).unwrap_or_default();
                return Ok(Some(match locale::get(&spec, &what)? {
                    Some(v) => string_value(heap, &v)?,
                    None => Value::UNDEFINED,
                }));
            }
            if kind == SEGMENTER {
                let Some(x) = a[2].as_string() else { return Ok(None) };
                let units: Vec<u16> = heap.str_units(x).collect();
                return Ok(Some(string_value(heap, &segmenter::segment(&Spec::parse(&spec), &units))?));
            }
            if kind == DISPLAY_NAMES {
                let code = str_arg(heap, a[2]).unwrap_or_default();
                return Ok(Some(match displaynames::of(&Spec::parse(&spec), &code)? {
                    Some(v) => string_value(heap, &v)?,
                    None => Value::UNDEFINED,
                }));
            }
            if kind == SUPPORTED_VALUES {
                return Ok(Some(string_value(heap, locale::supported_values(&spec)?)?));
            }
            let spec = Spec::parse(&spec);
            let parts = a[4] == Value::TRUE;
            let out = match kind {
                0 => {
                    let num = |v: Value| match (v.as_string(), v.as_number()) {
                        (Some(s), _) => Some(number::parse_intl_string(&heap.str_to_string(s))),
                        (None, Some(n)) => Some(number::from_f64(n)),
                        _ => None,
                    };
                    let Some(x) = num(a[2]) else { return Ok(None) };
                    if a[3] != Value::UNDEFINED {
                        // formatRange: parts are type \u{3} source.
                        let Some(y) = num(a[3]) else { return Ok(None) };
                        // As V8: the message shows the value as given ("Invalid start : abc").
                        for (n, v, which) in [(&x, a[2], "start"), (&y, a[3], "end")] {
                            if matches!(n, number::Num::Nan) {
                                let shown = match v.as_string() {
                                    Some(s) => heap.str_to_string(s),
                                    None if v.as_object().is_some() => "#<Object>".to_string(),
                                    None => "NaN".to_string(),
                                };
                                return range(&format!("Invalid {which} : {shown}"));
                            }
                        }
                        let items = number::format_range(&spec, x, y);
                        let mut s = String::new();
                        for (i, (t, v, src)) in items.iter().enumerate() {
                            if parts {
                                if i > 0 {
                                    s.push('\u{1}');
                                }
                                s.push_str(t);
                                s.push('\u{3}');
                                s.push_str(src);
                                s.push('\u{2}');
                            }
                            s.push_str(v);
                        }
                        return Ok(Some(string_value(heap, &s)?));
                    }
                    number::format(&spec, x).parts
                }
                1 => {
                    let Some(t) = a[2].as_number() else { return Ok(None) };
                    if a[3] != Value::UNDEFINED {
                        // formatRange: parts are type \u{3} source.
                        let Some(u) = a[3].as_number() else { return Ok(None) };
                        let (items, alone) = date::format_range(&spec, t, u)?;
                        let mut s = String::new();
                        for (i, (ty, v, src)) in items.iter().enumerate() {
                            if parts {
                                if i > 0 {
                                    s.push('\u{1}');
                                }
                                s.push_str(ty);
                                s.push('\u{3}');
                                s.push_str(["startRange", "endRange", "shared"][usize::from(*src)]);
                                s.push('\u{2}');
                            }
                            s.push_str(v);
                        }
                        // The start alone is format's text, with plain spaces for U+202F.
                        if alone && !parts {
                            s = s.replace('\u{202f}', " ");
                        }
                        return Ok(Some(string_value(heap, &s)?));
                    }
                    date::format(&spec, t)?
                }
                2 => {
                    let (Some(x), Some(y)) = (a[2].as_string(), a[3].as_string()) else { return Ok(None) };
                    let (x, y): (Vec<u16>, Vec<u16>) = (heap.str_units(x).collect(), heap.str_units(y).collect());
                    return Ok(Some(Value::int(collate::compare(&spec, &x, &y))));
                }
                _ => {
                    let x = str_arg(heap, a[2]);
                    text::format(kind, &spec, a[2].as_number(), x.as_deref(), str_arg(heap, a[3]).as_deref())?
                }
            };
            let mut s = if parts { out.encode() } else { out.text() };
            // As V8 does: a date's text has plain spaces where its parts have U+202F.
            if kind == 1 && !parts {
                s = s.replace('\u{202f}', " ");
            }
            string_value(heap, &s)?
        }
        5 => {
            let tags = canonical_list(&str_arg(heap, a[0]).unwrap_or_default())?;
            let mut s = String::new();
            for (i, t) in tags.iter().enumerate() {
                if i > 0 {
                    s.push('\n');
                }
                s.push_str(t);
            }
            // supportedLocalesOf: a[1] is the constructor's kind + 1. ICU's data decides by
            // the canonical language alone (see scripts/intl-loc/avail.js).
            if let Some(k) = a[1].as_int().filter(|&k| k > 0) {
                let bit = match k - 1 {
                    k @ 0..=5 => k,
                    DISPLAY_NAMES => 6,
                    _ => 7,
                };
                let available = |t: &&String| {
                    let lang = t.split('-').next().unwrap_or("");
                    locdata_avail::AVAILABLE
                        .binary_search_by(|e| e.0.cmp(lang))
                        .is_ok_and(|i| locdata_avail::AVAILABLE[i].1 & (1 << bit) != 0)
                };
                s = tags.iter().filter(available).map(|t| t.as_str()).collect::<Vec<_>>().join("\n");
            }
            string_value(heap, &s)?
        }
        _ => return Ok(None),
    }))
}

pub(crate) fn locale_compare(a: &[u16], b: &[u16]) -> i32 {
    collate::compare(&Spec::default(), a, b)
}

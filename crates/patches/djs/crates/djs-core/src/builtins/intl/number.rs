use super::data::{CURRENCIES, CURRENCY_SYMBOLS, NUM_LOCALES};
use super::numtext::S;
use super::numdata::{
    COMPACT_PATTERNS, COMPACT_PERCENT_GAP, COMPOUND_PATTERNS, CURRENCY_NAMES, CURRENCY_SYMBOLS_L,
    PER_PATTERNS, PERCENT_PER, PLURAL_RANGES, RANGES, UNIT_PATTERNS,
};
use super::{Parts, Spec, plural_en_like, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub(crate) struct NumLocale {
    pub tag: &'static str,
    pub dec: &'static str,
    pub grp: &'static str,
    pub min2: bool,
    pub indian: bool,
    pub minus: &'static str,
    pub cur: &'static str,
    pub neg_cur: &'static str,
    pub acct: &'static str,
    pub pct: &'static str,
    pub neg_pct: &'static str,
    pub nan: &'static str,
}

pub(crate) struct Currency {
    pub code: &'static str,
    pub symbol: &'static str,
    pub narrow: &'static str,
    pub digits: u8,
}

/// The regional data a locale reads ahead of its language's: the generated tables keep a
/// regional row only where it differs from its language's.
pub(super) fn data_key(locale: &str) -> Option<&'static str> {
    let mut it = locale.split('-');
    let lang = it.next()?;
    let rest: Vec<&str> = it.take_while(|s| s.len() != 1).collect();
    let has = |s: &str| rest.contains(&s);
    Some(match lang {
        "en" if has("GB") => "en-GB",
        "en" if has("IN") => "en-IN",
        "es" if has("MX") => "es-MX",
        "fr" if has("CA") => "fr-CA",
        "sr" if has("Latn") => "sr-Latn",
        "fa" if has("AF") => "fa-AF",
        "zh" if has("TW") || (has("Hant") && !has("HK") && !has("MO")) => "zh-Hant",
        _ => return None,
    })
}

/// The keys a locale's rows are looked up by: its regional data's, then its language's.
pub(super) fn keys(locale: &str) -> [&str; 2] {
    let lang = locale.split('-').next().unwrap_or("en");
    [data_key(locale).unwrap_or(lang), lang]
}

/// A locale's row: its regional one, else its language's.
pub(super) fn find_row<'a, T>(rows: &'a [T], locale: &str, f: impl Fn(&T, &str) -> bool) -> Option<&'a T> {
    keys(locale).iter().find_map(|k| rows.iter().find(|r| f(r, k)))
}

/// The key of a table whose regional rows replace its language's as a whole (a missing row
/// means something there): the locale's regional data where REGIONAL_TABLES names it, else its
/// language's when it has rows, else en.
pub(super) fn table_key<'a>(locale: &'a str, table: &str, has: impl Fn(&str) -> bool) -> &'a str {
    let [k, lang] = keys(locale);
    if super::numdata::REGIONAL_TABLES.iter().any(|r| r.0 == table && r.1 == k) {
        k
    } else if has(lang) || table != "compact" {
        // A language without compound or auto rows composes its own; compact falls to en's.
        lang
    } else {
        "en"
    }
}

/// Whether a locale writes Arabic-Indic digits (ar in most Arab League regions, as ICU).
pub(super) fn arab_digits(locale: &str) -> bool {
    super::date::numbering_system(locale) == "arab"
}

/// A minus or plus sign's leading bidi mark (ar's LRM, ALM), which ICU gives as a literal.
fn sign_mark(loc: &NumLocale) -> (&'static str, &'static str) {
    match loc.minus.char_indices().nth(1) {
        Some((i, _)) => (&loc.minus[..i], &loc.minus[i..]),
        None => ("", loc.minus),
    }
}

/// The exponent separator of a language that writes its own.
fn exp_sep(lang: &str) -> &'static str {
    match lang {
        "uk" => "\u{415}",
        "sv" => "\u{d7}10^",
        "el" => "e",
        _ => "E",
    }
}

/// ta's accounting pattern groups in threes (¤#,##0.00), its others as Indian.
static TA_ACCT: NumLocale = NumLocale { tag: "ta", dec: ".", grp: ",", min2: false, indian: false, minus: "-", cur: "\u{a4}#", neg_cur: "-\u{a4}#", acct: "(\u{a4}#)", pct: "#%", neg_pct: "-#%", nan: "NaN" };

static FA_ARABEXT_SPACED: NumLocale = NumLocale {
    tag: "fa-arabext",
    dec: "\u{66b}",
    grp: "\u{66c}",
    min2: false,
    indian: false,
    minus: "\u{200e}\u{2212}",
    cur: "\u{200e}\u{a4}\u{a0}#",
    neg_cur: "-\u{200e}\u{a4}\u{a0}#",
    acct: "\u{200e}(\u{a4}\u{a0}#)",
    pct: "#%",
    neg_pct: "-#%",
    nan: "\u{646}\u{627}\u{639}\u{62f}\u{62f}",
};

/// bn's percent pattern groups in threes (#,##0%), its others as Indian.
static BN_PERCENT: NumLocale = NumLocale { tag: "bn", dec: ".", grp: ",", min2: false, indian: false, minus: "-", cur: "#\u{a4}", neg_cur: "-#\u{a4}", acct: "(#\u{a4})", pct: "#%", neg_pct: "-#%", nan: "NaN" };

fn pct_sign(loc: &NumLocale) -> &'static str {
    if is_arab(loc) || loc.tag == "fa-arabext" || loc.tag == "fa-AF-arabext" { "\u{66a}" } else { "%" }
}

fn is_arab(loc: &NumLocale) -> bool {
    loc.tag == "ar-arab" || loc.tag == "arab" || loc.tag == "arabext"
}

/// Arabic-Indic symbols and patterns outside ar (en-u-nu-arab), which ICU takes from root but
/// for a few languages' own (zh's currency pattern, ko's and zh-Hant's minus).
const ROOT_ARAB: NumLocale = NumLocale { tag: "arab", dec: "\u{66b}", grp: "\u{66c}", min2: false, indian: false, minus: "\u{61c}-", cur: "#\u{a0}\u{a4}", neg_cur: "-#\u{a0}\u{a4}", acct: "-#\u{a0}\u{a4}", pct: "#%\u{61c}", neg_pct: "-#%\u{61c}", nan: "NaN" };
/// Extended Arabic-Indic (arabext) outside ar: the arab separators with LRM-wrapped signs.
static ROOT_ARABEXT: NumLocale = NumLocale { tag: "arabext", minus: "\u{200e}-\u{200e}", pct: "#\u{a0}%", neg_pct: "-#\u{a0}%", ..ROOT_ARAB };
static ARAB_ROWS: [(&str, NumLocale); 5] = [
    ("min2", NumLocale { min2: true, ..ROOT_ARAB }),
    ("nl", NumLocale { pct: "#\u{a0}%\u{61c}", neg_pct: "-#\u{a0}%\u{61c}", ..ROOT_ARAB }),
    ("ko", NumLocale { minus: "\u{200f}-", ..ROOT_ARAB }),
    ("zh", NumLocale { cur: "\u{a4}#", neg_cur: "-\u{a4}#", acct: "-\u{a4}#", ..ROOT_ARAB }),
    ("zh-Hant", NumLocale { minus: "-", nan: "\u{975e}\u{6578}\u{503c}", ..ROOT_ARAB }),
];

/// The symbols a locale formats with in numbering system nu: Arabic-Indic ones for arab, else
/// the locale's (Latin ones in ar-EG when nu is latn), whose ASCII digits map to nu afterwards.
pub(super) fn num_locale_nu(locale: &str, nu: &str) -> &'static NumLocale {
    let ar = locale.split('-').next() == Some("ar");
    match nu {
        "arab" if ar => num_locale("ar-EG"),
        // fa writes arabext with its own symbols
        "arabext" if locale.split('-').next() == Some("fa") => {
            num_locale(if locale.split('-').any(|s| s == "AF") { "fa-AF-arabext" } else { "fa-arabext" })
        }
        "arabext" if !ar => &ROOT_ARABEXT,
        "arab" if !ar => {
            let own = num_locale(locale);
            let key = if own.tag == "zh-Hant" || own.tag == "ko" || own.tag == "nl" { own.tag } else { own.tag.split('-').next().unwrap_or("") };
            ARAB_ROWS
                .iter()
                .find(|r| r.0 == key)
                .or_else(|| ARAB_ROWS.iter().find(|r| r.0 == "min2" && own.min2))
                .map_or(&ROOT_ARAB, |r| &r.1)
        }
        _ if arab_digits(locale) => num_locale("ar"),
        _ => num_locale(locale),
    }
}

pub(super) fn num_locale(tag: &str) -> &'static NumLocale {
    let lang = tag.split('-').next().unwrap_or("en");
    let tag = if arab_digits(tag) { "ar-arab" } else { data_key(tag).unwrap_or(tag) };
    NUM_LOCALES
        .iter()
        .find(|l| l.tag == tag)
        .or_else(|| {
            let mut it = tag.split('-');
            let l = it.next().unwrap_or("");
            let r = it.find(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase()));
            r.and_then(|r| {
                NUM_LOCALES
                    .iter()
                    .find(|n| n.tag.len() == 5 && n.tag.starts_with(l) && n.tag.ends_with(r))
            })
        })
        .or_else(|| NUM_LOCALES.iter().find(|l| l.tag == lang))
        .unwrap_or(&NUM_LOCALES[0])
}

const SIMPLE_UNITS: [&str; 45] = [
    "acre",
    "bit",
    "byte",
    "celsius",
    "centimeter",
    "day",
    "degree",
    "fahrenheit",
    "fluid-ounce",
    "foot",
    "gallon",
    "gigabit",
    "gigabyte",
    "gram",
    "hectare",
    "hour",
    "inch",
    "kilobit",
    "kilobyte",
    "kilogram",
    "kilometer",
    "liter",
    "megabit",
    "megabyte",
    "meter",
    "microsecond",
    "mile",
    "mile-scandinavian",
    "milliliter",
    "millimeter",
    "millisecond",
    "minute",
    "month",
    "nanosecond",
    "ounce",
    "percent",
    "petabyte",
    "pound",
    "second",
    "stone",
    "terabit",
    "terabyte",
    "week",
    "yard",
    "year",
];

fn is_unit(u: &str) -> bool {
    if SIMPLE_UNITS.contains(&u) {
        return true;
    }
    match u.split_once("-per-") {
        Some((a, b)) => SIMPLE_UNITS.contains(&a) && SIMPLE_UNITS.contains(&b),
        None => false,
    }
}

/// Currencies whose amounts don't have two fraction digits in ICU (node 22's resolvedOptions for
/// every Intl.supportedValuesOf('currency'), with CLF and UYW, which it leaves out).
const CURRENCY_DIGITS: [(&str, u8); 41] = [("AFN", 0), ("ALL", 0), ("BHD", 3), ("BIF", 0), ("CLF", 4), ("CLP", 0), ("COP", 0), ("DJF", 0), ("GNF", 0), ("HUF", 0), ("IDR", 0), ("IQD", 0), ("IRR", 0), ("ISK", 0), ("JOD", 3), ("JPY", 0), ("KMF", 0), ("KPW", 0), ("KRW", 0), ("KWD", 3), ("LAK", 0), ("LBP", 0), ("LYD", 3), ("MGA", 0), ("MMK", 0), ("OMR", 3), ("PKR", 0), ("PYG", 0), ("RWF", 0), ("SLL", 0), ("SOS", 0), ("SYP", 0), ("TND", 3), ("UGX", 0), ("UYW", 4), ("VND", 0), ("VUV", 0), ("XAF", 0), ("XOF", 0), ("XPF", 0), ("YER", 0)];

fn currency_digits(code: &str) -> u32 {
    if let Some(d) = CURRENCY_DIGITS.iter().find(|c| c.0 == code) {
        return u32::from(d.1);
    }
    CURRENCIES
        .iter()
        .find(|c| c.code == code)
        .map_or(2, |c| u32::from(c.digits))
}

const INCREMENTS: [f64; 15] = [
    1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0, 1000.0, 2000.0, 2500.0,
    5000.0,
];

fn digit_option(raw: &Spec, name: &str, min: f64, max: f64) -> Result<Option<u32>, Fault> {
    match raw.get(name) {
        None => Ok(None),
        Some(v) => {
            let n = super::parse_f64(v);
            if !(n >= min && n <= max) {
                return range(&format!("{name} value is out of range."));
            }
            Ok(Some(libm_floor(n) as u32))
        }
    }
}

fn libm_floor(n: f64) -> f64 {
    let t = n as i64 as f64;
    if t > n { t - 1.0 } else { t }
}

pub(super) struct Digits {
    pub min_int: u32,
    pub min_fd: u32,
    pub max_fd: u32,
    pub min_sd: u32,
    pub max_sd: u32,
    pub kind: &'static str,
    pub priority: &'static str,
    /// roundingIncrement: rounding by fraction digits goes to multiples of inc units of the last digit.
    pub inc: u32,
}

pub(super) fn digit_options(
    raw: &Spec,
    out: &mut Spec,
    fd_default: (u32, u32),
    compact: bool,
) -> Result<Digits, Fault> {
    let min_int = digit_option(raw, "minimumIntegerDigits", 1.0, 21.0)?.unwrap_or(1);
    let (mnfd, mxfd) = (
        raw.get("minimumFractionDigits"),
        raw.get("maximumFractionDigits"),
    );
    let (mnsd, mxsd) = (
        raw.get("minimumSignificantDigits"),
        raw.get("maximumSignificantDigits"),
    );
    let inc = match raw.get("roundingIncrement") {
        None => 1.0,
        Some(v) => {
            let n = super::parse_f64(v);
            if !(1.0..=5000.0).contains(&n) {
                return range("roundingIncrement value is out of range.");
            }
            let n = libm_floor(n);
            if !INCREMENTS.contains(&n) {
                return range("roundingIncrement value is out of range.");
            }
            n
        }
    };
    let mode = raw.choose(
        "roundingMode",
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
        "halfExpand",
        "Intl.NumberFormat",
    )?;
    let priority = raw.choose(
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
        "auto",
        "Intl.NumberFormat",
    )?;
    let tzd = raw.choose(
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        "auto",
        "Intl.NumberFormat",
    )?;
    let has_sd = mnsd.is_some() || mxsd.is_some();
    let has_fd = mnfd.is_some() || mxfd.is_some();
    let mut need_sd = true;
    let mut need_fd = true;
    if priority == "auto" {
        need_sd = has_sd;
        if need_sd || (!has_fd && compact) {
            need_fd = false;
        }
    }
    let mut d = Digits {
        min_int,
        min_fd: 0,
        max_fd: 0,
        min_sd: 1,
        max_sd: 21,
        kind: "fractionDigits",
        priority,
        inc: 1,
    };
    if need_sd && has_sd {
        d.min_sd = digit_option(raw, "minimumSignificantDigits", 1.0, 21.0)?.unwrap_or(1);
        d.max_sd =
            digit_option(raw, "maximumSignificantDigits", f64::from(d.min_sd), 21.0)?.unwrap_or(21);
    }
    if need_fd {
        if has_fd {
            let lo = digit_option(raw, "minimumFractionDigits", 0.0, 100.0)?;
            let hi = digit_option(raw, "maximumFractionDigits", 0.0, 100.0)?;
            match (lo, hi) {
                (None, Some(h)) => {
                    d.min_fd = fd_default.0.min(h);
                    d.max_fd = h;
                }
                (Some(l), None) => {
                    d.min_fd = l;
                    d.max_fd = fd_default.1.max(l);
                }
                (Some(l), Some(h)) => {
                    if l > h {
                        return range("maximumFractionDigits value is out of range.");
                    }
                    d.min_fd = l;
                    d.max_fd = h;
                }
                (None, None) => {}
            }
        } else {
            d.min_fd = fd_default.0;
            d.max_fd = fd_default.1;
        }
    }
    if !need_sd && !need_fd {
        d.min_fd = 0;
        d.max_fd = 0;
        d.min_sd = 1;
        d.max_sd = 2;
        d.kind = "morePrecision";
        // As node 22: resolvedOptions says auto once trailingZeroDisplay strips.
        d.priority = if tzd == "stripIfInteger" {
            "auto"
        } else {
            "morePrecision"
        };
    } else if priority == "auto" {
        d.kind = if has_sd {
            "significantDigits"
        } else {
            "fractionDigits"
        };
    } else {
        d.kind = priority;
    }
    if inc != 1.0 {
        if d.kind != "fractionDigits" {
            return type_err("RoundingType is not fractionDigits");
        }
        if d.max_fd != d.min_fd {
            return range("maximumFractionDigits value is out of range.");
        }
    }
    out.num("minimumIntegerDigits", min_int);
    if d.kind != "significantDigits" {
        out.num("minimumFractionDigits", d.min_fd);
        out.num("maximumFractionDigits", d.max_fd);
    }
    if d.kind != "fractionDigits" {
        out.num("minimumSignificantDigits", d.min_sd);
        out.num("maximumSignificantDigits", d.max_sd);
    }
    out.later("roundingIncrement", &format!("#{inc}"));
    out.later("roundingMode", mode);
    out.later("roundingPriority", d.priority);
    // As node 22: resolvedOptions says auto for weeks, which still strip (V8 reads it off the
    // skeleton, where duration-week's /w is the stripIfInteger stem's).
    let tzd = if out.get("unit") == Some("week") { "auto" } else { tzd };
    out.later("trailingZeroDisplay", tzd);
    Ok(d)
}

pub(super) fn resolve(locale: &str, tags: &[String], raw: &Spec) -> Result<Spec, Fault> {
    let mut out = Spec::default();
    let (nu, shown, range_nu) = super::numsys::resolve(locale, tags, raw)?;
    out.put("locale", &shown);
    out.put("numberingSystem", &nu);
    if let Some(r) = range_nu {
        out.put("@rnu", r);
    }
    let n = "Intl.NumberFormat";
    let style = raw.choose(
        "style",
        &["decimal", "percent", "currency", "unit"],
        "decimal",
        n,
    )?;
    let currency = match raw.get("currency") {
        Some(c) => {
            if c.len() != 3 || !c.bytes().all(|b| b.is_ascii_alphabetic()) {
                return range(&format!("Invalid currency code : {c}"));
            }
            Some(c.to_ascii_uppercase())
        }
        None => None,
    };
    let cd = raw.choose(
        "currencyDisplay",
        &["code", "symbol", "narrowSymbol", "name"],
        "symbol",
        n,
    )?;
    let cs = raw.choose("currencySign", &["standard", "accounting"], "standard", n)?;
    let unit = raw.get("unit").map(String::from);
    if let Some(u) = &unit
        && !is_unit(u)
    {
        return range(&format!(
            "Invalid unit argument for Intl.NumberFormat() '{u}'"
        ));
    }
    let ud = raw.choose("unitDisplay", &["short", "narrow", "long"], "short", n)?;
    // A missing currency or unit is refused before notation is read, as V8 does.
    if style == "currency" && currency.is_none() {
        return type_err("Currency code is required with currency style.");
    }
    if style == "unit" && unit.is_none() {
        return type_err("Invalid unit argument for Intl.NumberFormat() ''");
    }
    out.put("style", style);
    let notation = raw.choose(
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        "standard",
        n,
    )?;
    let fd = match style {
        "currency" => {
            let Some(c) = &currency else {
                return type_err("Currency code is required with currency style.");
            };
            out.put("currency", c);
            out.put("currencyDisplay", cd);
            // As V8: a currency's name has no accounting form.
            // As node 22: signDisplay never drops the accounting form.
            let sd_never = raw.get("signDisplay") == Some("never");
            out.put("currencySign", if sd_never { "standard" } else { cs });
            // As node 22 (V8 12): the currency's digits whatever the notation (newer V8 drops
            // them outside standard notation); compact without digit options rounds its own way.
            (currency_digits(c), currency_digits(c))
        }
        "unit" => {
            let Some(u) = &unit else {
                return type_err("Invalid unit argument for Intl.NumberFormat() ''");
            };
            out.put("unit", u);
            out.put("unitDisplay", ud);
            (0, 3)
        }
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let compact = notation == "compact";
    digit_options(raw, &mut out, fd, compact)?;
    let cdisp = raw.choose("compactDisplay", &["short", "long"], "short", n)?;
    let grouping = match raw.get("useGrouping") {
        None => {
            if compact {
                "min2"
            } else {
                "auto"
            }
        }
        Some("false") => "!f",
        Some("true") => "always",
        Some(g) => {
            let g = ["min2", "auto", "always"].into_iter().find(|x| *x == g);
            match g {
                Some(g) => g,
                None => {
                    return range(&format!(
                        "Value {} out of range for {n} options property useGrouping",
                        raw.get("useGrouping").unwrap_or("")
                    ));
                }
            }
        }
    };
    out.put("useGrouping", grouping);
    out.put("notation", notation);
    if compact {
        out.put("compactDisplay", cdisp);
    }
    let sd = raw.choose(
        "signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
        "auto",
        n,
    )?;
    out.put("signDisplay", sd);
    out.flush_later();
    Ok(out)
}

fn same_double(x: &Num, y: &Num) -> bool {
    let f = |d: &Dec| {
        let mut s = String::from(if d.neg { "-0." } else { "0." });
        s.extend(d.digits.iter().map(|x| char::from(b'0' + x)));
        s.push_str(&format!("e{}", d.exp));
        s.parse::<f64>().ok()
    };
    match (x, y) {
        (Num::Dec(a), Num::Dec(b)) => a.digits != b.digits && f(a).is_some_and(|v| Some(v) == f(b) && v != 0.0),
        _ => false,
    }
}

#[derive(Clone)]
pub(super) struct Dec {
    pub neg: bool,
    pub digits: Vec<u8>,
    pub exp: i32,
}

#[derive(Clone)]
pub(super) enum Num {
    Nan,
    Inf(bool),
    Dec(Dec),
}

impl Dec {
    fn trim(&mut self) {
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        let lead = self.digits.iter().take_while(|d| **d == 0).count();
        if lead > 0 {
            self.digits.drain(..lead);
            self.exp -= lead as i32;
        }
        if self.digits.is_empty() {
            self.exp = 0;
        }
    }
    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }
}

pub(super) fn parse_decimal(s: &str) -> Num {
    let t = s.trim_matches(|c: char| c.is_whitespace());
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if body == "Infinity" {
        return Num::Inf(neg);
    }
    let (mant, e) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()),
        None => (body, Some(0)),
    };
    let Some(e) = e else { return Num::Nan };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if (ip.is_empty() && fp.is_empty()) || !ip.bytes().chain(fp.bytes()).all(|b| b.is_ascii_digit())
    {
        return Num::Nan;
    }
    let mut d = Dec {
        neg,
        digits: ip.bytes().chain(fp.bytes()).map(|b| b - b'0').collect(),
        exp: ip.len() as i32 + e,
    };
    d.trim();
    Num::Dec(d)
}

/// JS StrWhiteSpaceChar: WhiteSpace and LineTerminator.
fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// ToIntlMathematicalValue of a string, as V8 reads it: a StringNumericLiteral (whitespace, 0x/0o/0b
/// integers, Infinity) whose decimal value stays exact, except that one past the double range is ±∞.
pub(super) fn parse_intl_string(s: &str) -> Num {
    let t = s.trim_matches(js_space);
    if t.is_empty() {
        return Num::Dec(Dec { neg: false, digits: Vec::new(), exp: 0 });
    }
    let b = t.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            // Exact integer: decimal digits, least significant first.
            let mut dec: Vec<u8> = Vec::new();
            for c in t[2..].chars() {
                let Some(v) = c.to_digit(radix) else { return Num::Nan };
                let mut carry = v;
                for d in dec.iter_mut() {
                    let x = u32::from(*d) * radix + carry;
                    *d = (x % 10) as u8;
                    carry = x / 10;
                }
                while carry > 0 {
                    dec.push((carry % 10) as u8);
                    carry /= 10;
                }
            }
            dec.reverse();
            let mut d = Dec { neg: false, exp: dec.len() as i32, digits: dec };
            d.trim();
            return Num::Dec(d);
        }
    }
    let (neg, body) = match b[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if body == "Infinity" {
        return Num::Inf(neg);
    }
    let (mant, ex) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let e: i64 = match ex {
        None => 0,
        Some(x) => {
            let (en, ed) = match x.as_bytes().first() {
                Some(b'-') => (true, &x[1..]),
                Some(b'+') => (false, &x[1..]),
                _ => (false, x),
            };
            if ed.is_empty() || !ed.bytes().all(|c| c.is_ascii_digit()) {
                return Num::Nan;
            }
            let mut v: i64 = 0;
            for c in ed.bytes() {
                v = (v * 10 + i64::from(c - b'0')).min(1 << 40);
            }
            if en { -v } else { v }
        }
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if (ip.is_empty() && fp.is_empty()) || !ip.bytes().chain(fp.bytes()).all(|c| c.is_ascii_digit()) {
        return Num::Nan;
    }
    let exp = (ip.len() as i64 + e).clamp(-(1 << 30), 1 << 30) as i32;
    let mut d = Dec { neg, digits: ip.bytes().chain(fp.bytes()).map(|c| c - b'0').collect(), exp };
    d.trim();
    if !d.is_zero() && d.exp > 300 {
        // Past the double range, V8 reads ±∞: decide with a correctly rounded parse.
        let mut s = String::from("0.");
        s.extend(d.digits.iter().map(|x| char::from(b'0' + x)));
        s.push_str(&format!("e{}", d.exp));
        if s.parse::<f64>().map_or(true, |v| v.is_infinite()) {
            return Num::Inf(neg);
        }
    }
    Num::Dec(d)
}

pub(super) fn from_f64(x: f64) -> Num {
    if x.is_nan() {
        return Num::Nan;
    }
    if x.is_infinite() {
        return Num::Inf(x < 0.0);
    }
    let s = crate::num::to_string(x.abs());
    match parse_decimal(s.as_str()) {
        Num::Dec(mut d) => {
            d.neg = x.is_sign_negative();
            Num::Dec(d)
        }
        other => other,
    }
}

fn round_up(digits: &[u8], keep: i32, neg: bool, mode: &str) -> bool {
    let keep = keep.max(0) as usize;
    let rest = digits.get(keep..).unwrap_or(&[]);
    if rest.iter().all(|d| *d == 0) {
        return false;
    }
    let first = rest[0];
    let half_more = first > 5 || (first == 5 && rest[1..].iter().any(|d| *d != 0));
    let exact_half = first == 5 && rest[1..].iter().all(|d| *d == 0);
    let last_odd = keep > 0 && digits.get(keep - 1).is_some_and(|d| d % 2 == 1);
    match mode {
        "ceil" => !neg,
        "floor" => neg,
        "expand" => true,
        "trunc" => false,
        _ if half_more => true,
        _ if !exact_half => false,
        "halfCeil" => !neg,
        "halfFloor" => neg,
        "halfTrunc" => false,
        "halfEven" => last_odd,
        _ => true,
    }
}

fn round_at(d: &Dec, keep: i32, mode: &str) -> Dec {
    if keep < 0 {
        let mut digits = alloc::vec![0u8; keep.unsigned_abs() as usize];
        digits.extend_from_slice(&d.digits);
        let padded = Dec {
            neg: d.neg,
            digits,
            exp: d.exp - keep,
        };
        return round_at(&padded, 0, mode);
    }
    let mut r = d.clone();
    if keep >= r.digits.len() as i32 {
        return r;
    }
    let up = round_up(&d.digits, keep, d.neg, mode);
    r.digits.truncate(keep as usize);
    if up {
        let mut i = r.digits.len();
        loop {
            if i == 0 {
                r.digits.insert(0, 1);
                r.exp += 1;
                break;
            }
            i -= 1;
            if r.digits[i] == 9 {
                r.digits[i] = 0;
            } else {
                r.digits[i] += 1;
                break;
            }
        }
    }
    r.trim();
    r
}

/// The plural category of r × 10^e, with ICU's exponent operand: CLDR's many for fr, es, pt, it and ca
/// holds for an exponent outside 0..5, ahead of their other rules.
fn plural_exp(lang: &str, r: &Rounded, e: i32, exp: i32) -> &'static str {
    if matches!(lang, "fr" | "es" | "pt" | "it" | "ca") && !(0..=5).contains(&exp) {
        return "many";
    }
    plural_exp_inner(lang, r, e)
}

fn plural_exp_inner(lang: &str, r: &Rounded, e: i32) -> &'static str {
    let dec = Dec {
        neg: r.dec.neg,
        digits: r.dec.digits.clone(),
        exp: if r.dec.is_zero() {
            r.dec.exp
        } else {
            r.dec.exp + e
        },
    };
    plural_of(
        lang,
        &Rounded {
            dec,
            min_frac: (r.min_frac - e).max(0),
            min_int: r.min_int,
        },
        false,
    )
}

pub(super) struct Rounded {
    pub dec: Dec,
    pub min_frac: i32,
    pub min_int: i32,
}

/// d × k, for a small k.
fn mul_small(d: &Dec, k: u32) -> Dec {
    let mut digits = Vec::with_capacity(d.digits.len() + 4);
    let mut carry = 0u32;
    for &x in d.digits.iter().rev() {
        let v = u32::from(x) * k + carry;
        digits.push((v % 10) as u8);
        carry = v / 10;
    }
    let mut exp = d.exp;
    while carry > 0 {
        digits.push((carry % 10) as u8);
        carry /= 10;
        exp += 1;
    }
    digits.reverse();
    let mut r = Dec {
        neg: d.neg,
        digits,
        exp,
    };
    r.trim();
    r
}

/// d rounded to a multiple of inc × 10^-fd (inc is 1, 2, 5, 10, 20, 25, 50, … 5000).
fn round_increment_mode(d: &Dec, fd: i32, inc: u32, mode: &str) -> Dec {
    if d.is_zero() {
        return d.clone();
    }
    // d / inc = d × (10^z / inc) / 10^z, with 10^z / inc an integer for inc = a × 10^k, a in 1, 2, 5, 25.
    let mut a = inc;
    let mut k = 0;
    while a.is_multiple_of(10) {
        a /= 10;
        k += 1;
    }
    let (mul, z) = match a {
        2 => (5, 1),
        5 => (2, 1),
        25 => (4, 2),
        _ => (1, 0),
    };
    let mut q = mul_small(d, mul);
    q.exp += fd - z - k;
    let q = round_at(&q, q.exp, mode);
    if q.is_zero() {
        return q;
    }
    let mut r = mul_small(&q, inc);
    r.exp -= fd;
    r
}

pub(super) fn round(d: &Dec, g: &Digits, mode: &str, strip: bool) -> Rounded {
    let by_fd = || {
        let r = if g.inc > 1 {
            round_increment_mode(d, g.max_fd as i32, g.inc, mode)
        } else {
            round_at(d, d.exp + g.max_fd as i32, mode)
        };
        (r, g.min_fd as i32)
    };
    let by_sd = || {
        let r = round_at(d, g.max_sd as i32, mode);
        let lead = if r.is_zero() { 1 } else { r.exp };
        (r, g.min_sd as i32 - lead)
    };
    let (dec, mut min_frac) = match g.kind {
        "fractionDigits" => by_fd(),
        "significantDigits" => by_sd(),
        k => {
            let (a, b) = (by_fd(), by_sd());
            let fa = a.0.digits.len() as i32 - a.0.exp;
            let fb = b.0.digits.len() as i32 - b.0.exp;
            let more = fb > fa || (fb == fa && b.1 > a.1);
            if (k == "morePrecision") == more { b } else { a }
        }
    };
    if strip && dec.digits.len() as i32 <= dec.exp.max(0) {
        min_frac = 0;
    }
    Rounded {
        dec,
        min_frac: min_frac.max(0),
        min_int: g.min_int as i32,
    }
}

fn group(int: &str, loc: &NumLocale, mode: &str) -> Vec<String> {
    let n = int.len();
    let min = match mode {
        "!f" => return alloc::vec![int.to_string()],
        "always" => 4,
        "min2" => 5,
        _ => {
            if loc.min2 {
                5
            } else {
                4
            }
        }
    };
    if n < min {
        return alloc::vec![int.to_string()];
    }
    let mut groups = Vec::new();
    let mut end = n;
    let mut size = 3;
    while end > size {
        groups.push(int[end - size..end].to_string());
        end -= size;
        if loc.indian {
            size = 2;
        }
    }
    groups.push(int[..end].to_string());
    groups.reverse();
    groups
}

/// Locales whose currency amounts use other separators than their numbers (decimal, group).
const CURRENCY_SEPARATORS: [(&str, &str, &str); 2] = [("de-AT", ",", "."), ("fr-CH", ".", "'")];

fn digits_parts(r: &Rounded, loc: &NumLocale, grouping: &str, out: &mut Parts) {
    digits_parts_sep(r, loc, grouping, None, out);
}

fn digits_parts_sep(r: &Rounded, loc: &NumLocale, grouping: &str, cur: Option<&str>, out: &mut Parts) {
    let seps = cur.and_then(|_| CURRENCY_SEPARATORS.iter().find(|c| c.0 == loc.tag));
    let (dec, grp) = seps.map_or((loc.dec, loc.grp), |c| (c.1, c.2));
    let d = &r.dec;
    let mut int = String::new();
    let mut frac = String::new();
    for i in 0..d.exp.max(0) {
        int.push((b'0' + d.digits.get(i as usize).copied().unwrap_or(0)) as char);
    }
    while (int.len() as i32) < r.min_int {
        int.insert(0, '0');
    }
    let nfrac = (d.digits.len() as i32 - d.exp).max(r.min_frac);
    for i in 0..nfrac {
        let at = d.exp + i;
        let c = if at < 0 {
            0
        } else {
            d.digits.get(at as usize).copied().unwrap_or(0)
        };
        frac.push((b'0' + c) as char);
    }
    let groups = group(&int, loc, grouping);
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push("group", grp);
        }
        out.push("integer", &digits_of(loc, g));
    }
    if !frac.is_empty() {
        out.push("decimal", dec);
        out.push("fraction", &digits_of(loc, &frac));
    }
}

/// ASCII digits in the locale's numbering system.
fn digits_of(loc: &NumLocale, s: &str) -> String {
    if !is_arab(loc) {
        return s.to_string();
    }
    s.chars()
        .map(|c| c.to_digit(10).and_then(|d| char::from_u32(0x660 + d)).unwrap_or(c))
        .collect()
}

pub(super) fn plural_of(lang: &str, r: &Rounded, ordinal: bool) -> &'static str {
    let d = &r.dec;
    let visible = (d.digits.len() as i32 - d.exp).max(r.min_frac).max(0) as u32;
    let mut int: u64 = 0;
    // As ICU's operand i: the integer's lowest 18 digits.
    for i in (d.exp - 18).max(0)..d.exp.max(0) {
        int = int * 10 + u64::from(d.digits.get(i as usize).copied().unwrap_or(0));
    }
    let mut fraction: u64 = 0;
    for i in 0..visible.min(18) as i32 {
        let at = d.exp + i;
        let c = if at < 0 {
            0
        } else {
            d.digits.get(at as usize).copied().unwrap_or(0)
        };
        fraction = fraction * 10 + u64::from(c);
    }
    // ICU evaluates the rules on doubles: past 2^53 i loses its low digits.
    let int = int as f64 as u64;
    let n: f64 = if d.digits.is_empty() {
        0.0
    } else {
        let ds: String = d.digits.iter().map(|g| (b'0' + g) as char).collect();
        format!("{ds}e{}", d.exp - d.digits.len() as i32).parse().unwrap_or(0.0)
    };
    plural_en_like(lang, int, visible, fraction, n, ordinal)
}

pub(super) struct Formatted {
    pub parts: Parts,
}

fn sign_of(sd: &str, neg: bool, zero: bool) -> Option<bool> {
    match sd {
        "never" => None,
        "always" => Some(neg),
        "exceptZero" => {
            if zero {
                None
            } else {
                Some(neg)
            }
        }
        "negative" => {
            if neg && !zero {
                Some(true)
            } else {
                None
            }
        }
        _ => {
            if neg {
                Some(true)
            } else {
                None
            }
        }
    }
}

fn affix_parts(
    pattern: &str,
    sign: Option<bool>,
    approx: Option<&str>,
    body: &Parts,
    sym: Option<(&'static str, &str)>,
    loc: &NumLocale,
    out: &mut Parts,
) {
    let mut lit = String::new();
    let flush = |lit: &mut String, out: &mut Parts| {
        if !lit.is_empty() {
            out.push("literal", lit);
            lit.clear();
        }
    };
    let chars: Vec<char> = pattern.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '#' => {
                flush(&mut lit, out);
                out.extend(body);
            }
            '-' => {
                let (mark, minus) = sign_mark(loc);
                flush(&mut lit, out);
                if let Some(a) = approx {
                    out.push("approximatelySign", a);
                }
                if sign.is_some() && !mark.is_empty() {
                    out.push("literal", mark);
                }
                // A mark on both sides (arabext's LRM-wrapped minus) closes after the sign too.
                let (minus, after) = match minus.char_indices().nth(1) {
                    Some((i, _)) => (&minus[..i], &minus[i..]),
                    None => (minus, ""),
                };
                match sign {
                    Some(true) => out.push("minusSign", minus),
                    Some(false) => out.push("plusSign", "+"),
                    None => {}
                }
                if sign.is_some() && !after.is_empty() {
                    out.push("literal", after);
                }
            }
            '%' => {
                flush(&mut lit, out);
                out.push("percentSign", pct_sign(loc));
            }
            '\u{66a}' => {
                flush(&mut lit, out);
                out.push("percentSign", "\u{66a}");
            }
            '\u{a4}' => {
                flush(&mut lit, out);
                if let Some((ty, s)) = sym {
                    // ICU's currency spacing: only next to a digit (not NaN or ∞).
                    let text = body.text();
                    let next_digit = chars.get(i + 1) == Some(&'#')
                        && text.chars().next().is_some_and(|c| c.is_numeric());
                    let prev_digit = i > 0
                        && chars[i - 1] == '#'
                        && text.chars().last().is_some_and(|c| c.is_numeric());
                    let letter_end = s.chars().last().is_some_and(|c| c.is_alphabetic());
                    let letter_start = s.chars().next().is_some_and(|c| c.is_alphabetic());
                    if prev_digit && letter_start {
                        out.push("literal", "\u{a0}");
                    }
                    out.push(ty, s);
                    if next_digit && letter_end {
                        out.push("literal", "\u{a0}");
                    }
                }
            }
            c => lit.push(*c),
        }
    }
    flush(&mut lit, out);
}

fn with_sign(p: &str) -> String {
    if p.contains('-') {
        p.to_string()
    } else {
        format!("-{p}")
    }
}

pub(super) fn format(spec: &Spec, x: Num) -> Formatted {
    format_full(spec, x, false, None).0
}

/// x formatted, with the approximately sign at the sign's place when approx, and the plural
/// category of a unit or currency name taken from plural when given; with x's own category.
fn format_full(
    spec: &Spec,
    x: Num,
    approx: bool,
    plural_of_range: Option<&'static str>,
) -> (Formatted, &'static str) {
    let (mut f, pl) = format_full_latn(spec, x, approx, plural_of_range);
    let nu = spec.get("numberingSystem").unwrap_or("latn");
    if nu != "latn" && nu != "arab" {
        for (t, v) in f.parts.items.iter_mut() {
            if matches!(*t, "integer" | "fraction" | "exponentInteger") {
                *v = super::numsys::map_digits(nu, v);
            }
        }
    }
    let items = &mut f.parts.items;
    let first_num = items.iter().position(|p| matches!(p.0, "integer" | "nan" | "infinity"));
    let sign_at = items.iter().position(|p| matches!(p.0, "minusSign" | "plusSign" | "approximatelySign"));
    // sw's compact currency patterns put the sign next to the number (₩ -1M)
    let sw_cur = spec.get("locale").is_some_and(|l| l.split('-').next() == Some("sw"))
        && spec.get("style") == Some("currency")
        && items.iter().any(|p| p.0 == "compact")
        // except its millions and trillions in the other plural form (-₩ 1.2M, ₩ -1M)
        && !(items.iter().any(|p| p.0 == "compact" && matches!(p.1.as_str(), "M" | "T"))
            && (items.iter().any(|p| p.0 == "fraction") || items.iter().filter(|p| p.0 == "integer").map(|p| p.1.as_str()).ne(["1"])));
    if let (Some(n), Some(s)) = (first_num, sign_at)
        && let Some(c) = items.get(s..n).and_then(|r| r.iter().rposition(|p| p.0 == "compact" || (sw_cur && p.0 == "currency")))
    {
        {
            // every sign ahead of the compact prefix follows it, in order
            let c = s + c;
            let signs: Vec<_> = (s..c).filter(|&j| matches!(items[j].0, "minusSign" | "plusSign" | "approximatelySign")).collect();
            let moved: Vec<_> = signs.iter().rev().map(|&j| items.remove(j)).collect();
            let at = n - moved.len();
            for sg in moved {
                items.insert(at, sg);
            }
        }
    }
    // fa's compact symbol is spaced only for a compacted or negative number, or by ICU's currency
    // spacing (a symbol ending in a letter before a digit: $MX ۰)
    let fa_ext = spec.get("locale").is_some_and(|l| l.split('-').next() == Some("fa") && !l.split('-').any(|s| s == "AF")) && nu == "arabext";
    let bare = spec.get("notation") == Some("compact")
        && spec.get("currencySign") != Some("accounting")
        && !items.iter().any(|p| p.0 == "compact");
    if fa_ext
        && bare
        && let Some(c) = items.iter().position(|p| p.0 == "currency")
        && items.get(c + 1).is_some_and(|p| p.0 == "literal" && p.1 == "\u{a0}")
        && !(items[c].1.chars().last().is_some_and(char::is_alphabetic) && items.get(c + 2).is_some_and(|p| p.0 == "integer"))
    {
        items.remove(c + 1);
    }
    if spec.get("locale").is_some_and(|l| l.split('-').next() == Some("he"))
        && let Some(c) = items.iter().position(|p| p.0 == "currency" && p.1 == "CN\u{a5}")
    {
        {
            if c > 0 && items[c - 1].0 == "literal" {
                items[c - 1].1.push('\u{200e}');
            } else {
                items.insert(c, ("literal", "\u{200e}".to_string()));
            }
            let c = items.iter().position(|p| p.0 == "currency").unwrap_or(0);
            items.insert(c + 1, ("literal", "\u{200e}".to_string()));
        }
    }
    // Han digits aren't decimal digits to ICU's currency spacing, so no space joins them to a code.
    // (A space the locale's own pattern has, as de's "# ¤", stays.)
    let loc = num_locale_nu(super::numsys::base_locale(spec.get("locale").unwrap_or("en-US")), nu);
    let own_space = [loc.cur, loc.neg_cur, loc.acct].iter().any(|p| p.contains("\u{a0}\u{a4}") || p.contains("\u{a4}\u{a0}"));
    if nu == "hanidec" && !own_space {
        let items = &mut f.parts.items;
        let mut i = 1;
        while i + 1 < items.len() {
            let pair = |a: &str, b: &str| (a == "currency" && b == "integer") || (a == "integer" && b == "currency");
            if items[i].0 == "literal" && items[i].1 == "\u{a0}" && pair(items[i - 1].0, items[i + 1].0) {
                items.remove(i);
            } else {
                i += 1;
            }
        }
    }
    (f, pl)
}

fn format_full_latn(
    spec: &Spec,
    x: Num,
    approx: bool,
    plural_of_range: Option<&'static str>,
) -> (Formatted, &'static str) {
    let locale = super::numsys::base_locale(spec.get("locale").unwrap_or("en-US"));
    let nu = spec.get("numberingSystem").unwrap_or("latn");
    let lang = locale.split('-').next().unwrap_or("en");
    let style = spec.get("style").unwrap_or("decimal");
    // en-IN's accounting pattern groups in threes ($100,000.00), its others (and names) as Indian.
    let loc = if locale.starts_with("en-IN")
        && style == "currency"
        && spec.get("currencySign") == Some("accounting")
        && spec.get("currencyDisplay") != Some("name")
    {
        num_locale_nu("en", nu)
    } else if lang == "ta" && style == "currency" && spec.get("currencySign") == Some("accounting") && spec.get("currencyDisplay") != Some("name") {
        &TA_ACCT
    } else if lang == "bn"
        && spec.get("notation").unwrap_or("standard") == "standard"
        && (style == "percent" || (style == "unit" && spec.get("unit").is_some_and(|u| u == "percent" || u.starts_with("percent-per-")) && spec.get("unitDisplay") != Some("long")))
    {
        &BN_PERCENT
    } else {
        num_locale_nu(locale, nu)
    };
    let notation = spec.get("notation").unwrap_or("standard");
    // fa in its own digits spaces the symbol in accounting and compact patterns (‎$ ۱)
    let loc = if loc.tag == "fa-arabext" && style == "currency" && (notation == "compact" || spec.get("currencySign") == Some("accounting")) {
        &FA_ARABEXT_SPACED
    } else {
        loc
    };
    let grouping = spec.get("useGrouping").unwrap_or("auto");
    // ICU groups compact numbers as min2 under auto.
    let grouping = if notation == "compact" && grouping == "auto" {
        "min2"
    } else {
        grouping
    };
    let sd = spec.get("signDisplay").unwrap_or("auto");
    let mode = spec.get("roundingMode").unwrap_or("halfExpand");
    let strip = spec.get("trailingZeroDisplay") == Some("stripIfInteger");
    let g = Digits {
        min_int: spec.int("minimumIntegerDigits", 1),
        min_fd: spec.int("minimumFractionDigits", 0),
        max_fd: spec.int("maximumFractionDigits", 3),
        min_sd: spec.int("minimumSignificantDigits", 1),
        max_sd: spec.int("maximumSignificantDigits", 21),
        kind: match (
            spec.get("minimumFractionDigits").is_some(),
            spec.get("minimumSignificantDigits").is_some(),
        ) {
            (true, true) => {
                if spec.get("roundingPriority") == Some("lessPrecision") {
                    "lessPrecision"
                } else {
                    "morePrecision"
                }
            }
            (false, true) => "significantDigits",
            _ => "fractionDigits",
        },
        priority: "auto",
        inc: spec.int("roundingIncrement", 1),
    };
    let mut body = Parts::default();
    let mut neg = false;
    let mut zero = false;
    let mut plural = "other";
    let mut compact_affix: Option<String> = None;
    match x {
        Num::Nan => {
            body.push("nan", loc.nan);
        }
        Num::Inf(n) => {
            neg = n;
            body.push("infinity", "\u{221e}");
        }
        Num::Dec(mut d) => {
            neg = d.neg;
            if style == "percent" && !d.is_zero() {
                d.exp += 2;
            }
            match notation {
                "scientific" | "engineering" => {
                    let mut e = if d.is_zero() { 0 } else { d.exp - 1 };
                    if notation == "engineering" {
                        e -= e.rem_euclid(3);
                    }
                    let mut m = d.clone();
                    m.exp -= e;
                    let mut r = round(&m, &g, mode, strip);
                    if !r.dec.is_zero()
                        && r.dec.exp - 1 > (if notation == "engineering" { 2 } else { 0 })
                    {
                        let step = if notation == "engineering" { 3 } else { 1 };
                        e += step;
                        m.exp -= step;
                        r = round(&m, &g, mode, strip);
                    }
                    zero = r.dec.is_zero();
                    plural = plural_exp(lang, &r, e, e);
                    digits_parts(&r, loc, grouping, &mut body);
                    body.push("exponentSeparator", if loc.tag == "ar-arab" { "\u{623}\u{633}" } else if loc.tag == "arab" { "\u{627}\u{633}" } else if loc.tag == "arabext" || loc.tag == "fa-arabext" || loc.tag == "fa-AF-arabext" { "\u{d7}\u{6f1}\u{6f0}^" } else { exp_sep(lang) });
                    if e < 0 {
                        let (mark, minus) = sign_mark(loc);
                        if !mark.is_empty() {
                            body.push("literal", mark);
                        }
                        body.push("exponentMinusSign", minus);
                    }
                    body.push("exponentInteger", &digits_of(loc, &e.unsigned_abs().to_string()));
                }
                "compact" => {
                    // ICU has no long compact form for a currency's symbol or code.
                    let long = spec.get("compactDisplay") == Some("long")
                        && (style != "currency" || spec.get("currencyDisplay") == Some("name"));
                    let tl = table_key(locale, "compact", |k| COMPACT_PATTERNS.iter().any(|t| t.0 == k));
                    let row = |mag: i32| {
                        COMPACT_PATTERNS
                            .iter()
                            .find(|t| t.0 == tl && t.1 == long && i32::from(t.2) == mag.min(20))
                    };
                    let mut mag = if d.is_zero() { 0 } else { d.exp - 1 };
                    let mut r;
                    let mut pat;
                    let mut scale;
                    loop {
                        pat = None;
                        let mut m = d.clone();
                        if mag >= 3
                            && let Some(t) = row(mag)
                        {
                            m.exp -= i32::from(t.3);
                            pat = Some(t);
                        }
                        r = round(&m, &g, mode, strip);
                        scale = d.exp - m.exp;
                        let new_mag = if r.dec.is_zero() {
                            0
                        } else {
                            r.dec.exp - 1 + scale
                        };
                        if new_mag > mag && mag < 20 {
                            mag = new_mag;
                            continue;
                        }
                        break;
                    }
                    zero = r.dec.is_zero();
                    // ICU picks the compact pattern by the shown number's category, and a
                    // unit's or currency's name by the whole value's, rounded as the unscaled
                    // value would be (pl 1,2 mln funty for 1200001.6, funtów for 1200001.4).
                    let shown = plural_of(lang, &r, false);
                    plural = if scale > 0 {
                        plural_of(lang, &round(&d, &g, mode, strip), false)
                    } else {
                        shown
                    };
                    if let Some(t) = pat {
                        let exactly_one = r.dec.digits == [1] && r.dec.exp == 1 && !d.neg;
                        compact_affix = Some(
                            if exactly_one && !t.5.is_empty() {
                                t.5.get()
                            } else {
                                plural_pick(&t.4, shown)
                            }
                            .to_string(),
                        );
                    }
                    digits_parts(&r, loc, grouping, &mut body);
                }
                _ => {
                    let r = round(&d, &g, mode, strip);
                    zero = r.dec.is_zero();
                    plural = plural_of(lang, &r, false);
                    let cur = (style == "currency" && spec.get("currencyDisplay") != Some("name")).then_some("");
                    digits_parts_sep(&r, loc, grouping, cur, &mut body);
                }
            }
        }
    }
    let compacted = compact_affix.is_some();
    if let Some(p) = compact_affix {
        let (pre, post) = match p.split_once("{0}") {
            Some((a, b)) => (a, b),
            // a pattern without the number (fr mille)
            None => {
                body = Parts::default();
                ("", p.as_str())
            }
        };
        let mut nb = Parts::default();
        // Spaces and bidi marks (he's RLM) around the compact text are literals.
        let lit = |c: char| c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{61c}');
        let affix = |nb: &mut Parts, s: &str| {
            let core = s.trim_matches(lit);
            if core.is_empty() {
                if !s.is_empty() {
                    nb.push("literal", s);
                }
                return;
            }
            let start = s.len() - s.trim_start_matches(lit).len();
            let end = start + core.len();
            if start > 0 {
                nb.push("literal", &s[..start]);
            }
            nb.push("compact", core);
            if end < s.len() {
                nb.push("literal", &s[end..]);
            }
        };
        affix(&mut nb, pre);
        nb.extend(&body);
        affix(&mut nb, post);
        body = nb;
    }
    // NaN is unsigned, but signDisplay always gives it a plus sign.
    let sign = if x_is_nan(&body) {
        if sd == "always" { Some(false) } else { None }
    } else {
        sign_of(sd, neg, zero)
    };
    let ap = if approx && (loc.tag == "arab" || loc.tag == "arabext") {
        Some("~")
    } else if approx {
        find_row(RANGES, locale, |r, k| r.0 == k)
            .or(RANGES.first())
            .map(|r| r.2.get())
    } else {
        None
    };
    let signed = sign.is_some() || ap.is_some();
    let own_plural = plural;
    let plural = plural_of_range.unwrap_or(plural);
    let mut out = Parts::default();
    // ICU lays out a compact percent unit as a compact percent (tr %-0,001, ar 1.2 ألف٪).
    let layout = if style == "unit"
        && spec.get("unit") == Some("percent")
        && notation == "compact"
        && spec.get("unitDisplay") != Some("long")
    {
        "percent"
    } else {
        style
    };
    match layout {
        "currency" => {
            let code = spec.get("currency").unwrap_or("USD");
            let cd = spec.get("currencyDisplay").unwrap_or("symbol");
            let entry = CURRENCIES.iter().find(|c| c.code == code);
            if cd == "name" {
                let row = find_row(CURRENCY_NAMES, locale, |(l, c, _), k| *l == k && *c == code)
                    .or_else(|| {
                        CURRENCY_NAMES
                            .iter()
                            .find(|(l, c, _)| *l == "en" && *c == code)
                    });
                let pattern = row.map_or_else(
                    || format!("{{0}} {code}"),
                    |r| plural_pick(&r.2, plural).to_string(),
                );
                let mut num = Parts::default();
                affix_parts(
                    if signed { "-#" } else { "#" },
                    sign,
                    ap,
                    &body,
                    None,
                    loc,
                    &mut num,
                );
                wrap_pattern(&pattern, &num, "currency", &mut out);
                return (Formatted { parts: out }, own_plural);
            }
            let by_lang = find_row(CURRENCY_SYMBOLS_L, locale, |(l, c, _, _), k| *l == k && *c == code)
                .or_else(|| {
                    CURRENCY_SYMBOLS_L
                        .iter()
                        .find(|(l, c, _, _)| *l == "en" && *c == code)
                });
            let sym: &str = match cd {
                "code" => code,
                "narrowSymbol" => by_lang
                    .map(|t| t.3.get())
                    .or(entry.map(|c| c.narrow))
                    .unwrap_or(code),
                _ => CURRENCY_SYMBOLS
                    .iter()
                    .find(|(l, c, _)| *c == code && *l == locale && l.contains('-'))
                    .map(|t| t.2)
                    .or(by_lang.map(|t| t.2.get()))
                    .or(entry.map(|c| c.symbol))
                    .unwrap_or(code),
            };
            let acct = spec.get("currencySign") == Some("accounting");
            let pat = match sign {
                // ICU's compact patterns have no accounting form, and sign them ahead of
                // everything (nl -US$ 1,2K, where its own negative pattern is US$ -1,2).
                _ if compacted && signed => with_sign(loc.cur),
                Some(true) if acct => loc.acct.to_string(),
                // An accounting pattern in parentheses has its own positive form (ar ؜5₩), which
                // a plus sign leads (en +€1.00, nl +€ 1,00).
                None | Some(false) if acct && !compacted && loc.acct.starts_with('(') => {
                    let pos = &loc.acct[1..loc.acct.len() - 1];
                    if sign.is_some() { format!("-{pos}") } else { pos.to_string() }
                }
                None if !signed => loc.cur.to_string(),
                _ => loc.neg_cur.to_string(),
            };
            let parenthesized = acct && sign == Some(true) && pat.contains('(') && !compacted;
            let pat = if signed && !parenthesized {
                with_sign(&pat)
            } else {
                pat
            };
            let sign = if parenthesized { None } else { sign };
            affix_parts(
                &pat,
                sign,
                ap,
                &body,
                Some(("currency", sym)),
                loc,
                &mut out,
            );
        }
        "percent" => {
            let pat = if notation == "compact" {
                // ICU signs a compact percent next to its number (tr %-50), and ar's has
                // neither its marks nor its own sign (0٪).
                let p = if lang == "ar" { "#\u{66a}" } else { loc.pct };
                if signed { p.replacen('#', "-#", 1) } else { p.to_string() }
            } else if signed {
                with_sign(loc.neg_pct)
            } else {
                loc.pct.to_string()
            };
            affix_parts(&pat, sign, ap, &body, None, loc, &mut out);
            // ICU marks a compact percent's sign as a unit.
            // ICU marks a compact percent's sign as a unit, spaced its own way.
            if notation == "compact" {
                let gap = find_row(COMPACT_PERCENT_GAP, locale, |r, k| r.0 == k).map(|r| r.1.get());
                for i in 0..out.items.len() {
                    if out.items[i].0 == "percentSign" {
                        out.items[i].0 = "unit";
                        let spaced = i > 0 && out.items[i - 1].0 == "literal" && out.items[i - 1].1.trim().is_empty();
                        match lang {
                            "da" => out.items[i].1 = "pct.".to_string(),
                            "sw" => {
                                // sw puts its word for percent ahead of the number (asilimia elfu 1.2)
                                out.items.remove(i);
                                if spaced {
                                    out.items.remove(i - 1);
                                }
                                out.items.insert(0, ("unit", "asilimia".to_string()));
                                out.items.insert(1, ("literal", " ".to_string()));
                                break;
                            }
                            _ => {}
                        }
                        match gap {
                            Some("") if spaced => {
                                out.items.remove(i - 1);
                                break;
                            }
                            Some(gap) if spaced => out.items[i - 1].1 = gap.to_string(),
                            // es-MX spaces it though its percent pattern doesn't (0 %).
                            Some(gap) if !gap.is_empty() && i > 0 && out.items[i - 1].0 != "literal" => {
                                out.items.insert(i, ("literal", gap.to_string()));
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        "unit" => {
            let unit = spec.get("unit").unwrap_or("");
            let disp = match spec.get("unitDisplay") {
                Some("long") => 1,
                Some("narrow") => 2,
                _ => 0,
            };
            let pattern = unit_pattern(locale, unit, disp, plural);
            let mut num = Parts::default();
            // ICU signs a percent unit ahead of its sign (tr -%5), as the percent style.
            let lead = signed && (unit == "percent" || unit.starts_with("percent-per-")) && disp != 1 && notation != "compact" && !pattern.starts_with("{0}");
            if lead {
                affix_parts("-", sign, ap, &body, None, loc, &mut out);
            }
            affix_parts(
                if signed && !lead { "-#" } else { "#" },
                if lead { None } else { sign },
                if lead { None } else { ap },
                &body,
                None,
                loc,
                &mut num,
            );
            wrap_pattern(&pattern, &num, "unit", &mut out);
            // A compact percent unit is spaced as a compact percent.
            if notation == "compact"
                && let Some(gap) = find_row(COMPACT_PERCENT_GAP, locale, |r, k| r.0 == k).map(|r| r.1)
            {
                for i in 1..out.items.len() {
                    if out.items[i].0 == "unit"
                        && out.items[i].1 == pct_sign(loc)
                        && out.items[i - 1].0 == "literal"
                        && out.items[i - 1].1.trim().is_empty()
                    {
                        out.items[i - 1].1 = gap.to_string();
                    }
                }
            }
        }
        _ => {
            affix_parts(
                if signed { "-#" } else { "#" },
                sign,
                ap,
                &body,
                None,
                loc,
                &mut out,
            );
        }
    }
    (Formatted { parts: out }, own_plural)
}

/// A formatted number split as ICU's range formatter sees it: the outer modifier (a unit's or
/// currency name's text), the middle one (sign, symbol, percent sign, compact suffix) on each
/// side of the number, and the number itself with its exponent (the inner modifier).
struct Layers {
    outer_pre: Vec<(&'static str, String)>,
    middle_pre: Vec<(&'static str, String)>,
    number: Vec<(&'static str, String)>,
    middle_post: Vec<(&'static str, String)>,
    outer_post: Vec<(&'static str, String)>,
    inner_len: usize,
}

fn layers(p: &Parts, has_outer: bool) -> Layers {
    const NUMBER: [&str; 9] = [
        "integer",
        "group",
        "decimal",
        "fraction",
        "nan",
        "infinity",
        "exponentSeparator",
        "exponentMinusSign",
        "exponentInteger",
    ];
    let items = &p.items;
    let first = items
        .iter()
        .position(|(t, _)| NUMBER.contains(t))
        .unwrap_or(0);
    let last = items
        .iter()
        .rposition(|(t, _)| NUMBER.contains(t))
        .map_or(0, |i| i + 1);
    let is_outer = |t: &str| has_outer && (t == "unit" || t == "currency");
    let (pre, number, post) = (&items[..first], &items[first..last], &items[last..]);
    let mut split_pre = 0;
    if let Some(k) = pre.iter().rposition(|(t, _)| is_outer(t)) {
        split_pre = k + 1;
        while split_pre < pre.len() && pre[split_pre].0 == "literal" {
            split_pre += 1;
        }
    }
    let mut split_post = post.len();
    if let Some(k) = post.iter().position(|(t, _)| is_outer(t)) {
        split_post = k;
        while split_post > 0 && post[split_post - 1].0 == "literal" {
            split_post -= 1;
        }
    }
    let inner_len = number
        .iter()
        .filter(|(t, _)| t.starts_with("exponent"))
        .map(|(_, v)| v.chars().count())
        .sum();
    Layers {
        outer_pre: pre[..split_pre].to_vec(),
        middle_pre: pre[split_pre..].to_vec(),
        number: number.to_vec(),
        middle_post: post[..split_post].to_vec(),
        outer_post: post[split_post..].to_vec(),
        inner_len,
    }
}

fn text_of(v: &[(&'static str, String)]) -> String {
    v.iter().map(|(_, s)| s.as_str()).collect()
}

/// The length ICU's range formatter sees of a middle modifier: a space ICU's currency spacing
/// put next to the symbol (ja 元 1.00) is not in it, but one in the pattern (de 1,00 €) is.
fn middle_len(m: &str, style: &str, loc: &NumLocale) -> usize {
    let n = m.chars().count();
    if style == "currency" && !loc.cur.contains(|c: char| c.is_whitespace()) {
        return m.chars().filter(|c| !c.is_whitespace()).count();
    }
    n
}

/// The plural category of a range whose ends have categories a and b.
pub(super) fn plural_range(lang: &str, a: &str, b: &'static str) -> &'static str {
    PLURAL_RANGES
        .iter()
        .find(|r| r.0 == lang && r.1 == a && r.2 == b)
        .map_or(b, |r| r.3.get())
}

/// formatRange's parts, as (type, value, source), the way ICU's range formatter with its
/// default collapse (auto) and identity fallback (approximately) makes them.
pub(super) fn format_range(
    spec: &Spec,
    x: Num,
    y: Num,
) -> Vec<(&'static str, String, &'static str)> {
    let mut out = format_range_parts(spec, x, y);
    // As V8: a range in a locale whose own digits the option replaced with latn keeps its own.
    if let Some(nu) = spec.get("@rnu") {
        for (t, v, _) in out.iter_mut() {
            if matches!(*t, "integer" | "fraction") {
                *v = super::numsys::map_digits(nu, v);
            }
        }
    }
    out
}

fn format_range_parts(
    spec: &Spec,
    x: Num,
    y: Num,
) -> Vec<(&'static str, String, &'static str)> {
    let locale = super::numsys::base_locale(spec.get("locale").unwrap_or("en-US"));
    let nu = spec.get("numberingSystem").unwrap_or("latn");
    let lang = locale.split('-').next().unwrap_or("en");
    let style = spec.get("style").unwrap_or("decimal");
    // A unit's or currency name's text is ICU's outer modifier; so is a compact percent's sign,
    // but a percent unit's is a middle one.
    let has_outer = (style == "unit"
        && (!spec.get("unit").unwrap_or("").starts_with("percent")
            || spec.get("unitDisplay") == Some("long")))
        || (style == "currency" && spec.get("currencyDisplay") == Some("name"))
        || ((style == "percent" || spec.get("unit") == Some("percent"))
            && spec.get("notation") == Some("compact"));
    let (a, pa) = format_full(spec, x.clone(), false, None);
    let (b, pb) = format_full(spec, y.clone(), false, None);
    // As V8: two values that are the same double are one value (approximately) even when
    // their exact decimals differ.
    if a.parts.text() == b.parts.text() || same_double(&x, &y) {
        let (p, _) = format_full(spec, x, true, None);
        return p
            .parts
            .items
            .into_iter()
            .map(|(t, v)| (t, v, "shared"))
            .collect();
    }
    // A plural form without the number (he מעלה אחת) shows the number in a range, as other's.
    let numbered = |f: Formatted, v: &Num| {
        if f.parts.items.iter().any(|p| matches!(p.0, "integer" | "nan" | "infinity")) {
            f
        } else {
            format_full(spec, v.clone(), false, Some("other")).0
        }
    };
    let (a, b) = (numbered(a, &x), numbered(b, &y));
    // A start whose plural form leads with its unit (he one: kB‏‎+0.9) takes the end's layout.
    let lead_unit = |f: &Formatted| f.parts.items.first().is_some_and(|p| p.0 == "unit");
    let a = if lead_unit(&a) && !lead_unit(&b) { format_full(spec, x.clone(), false, Some("other")).0 } else { a };
    let lang_r = if RANGES.iter().any(|r| r.0 == lang) {
        lang
    } else {
        "en"
    };
    let range_row = find_row(RANGES, locale, |r, k| r.0 == k).filter(|_| lang_r == lang);
    let pl = plural_range(lang_r, pa, pb);
    let (bo, _) = format_full(spec, y, false, Some(pl));
    let (l1, l2, lo) = (
        layers(&a.parts, has_outer),
        layers(&b.parts, has_outer),
        layers(&bo.parts, has_outer),
    );
    // Outer modifiers name the same unit or currency, so they always collapse (with the
    // range's plural); the middle one collapses when it is the same and longer than one
    // character; the inner one never does.
    let m1 = text_of(&l1.middle_pre) + &text_of(&l1.middle_post);
    let collapse_middle = text_of(&l1.middle_pre) == text_of(&l2.middle_pre)
        && text_of(&l1.middle_post) == text_of(&l2.middle_post)
        && middle_len(&m1, style, num_locale_nu(locale, nu)) > 1;
    let repeat = l1.inner_len > 0 || (!collapse_middle && !m1.is_empty());
    let mut sep = range_row
        .or_else(|| RANGES.iter().find(|r| r.0 == "en"))
        .map_or("–", |r| r.1.get())
        .to_string();
    if repeat {
        if !sep.starts_with(char::is_whitespace) {
            sep.insert(0, ' ');
        }
        if !sep.ends_with(char::is_whitespace) {
            sep.push(' ');
        }
    }
    let mut out: Vec<(&'static str, String, &'static str)> = Vec::new();
    let put = |v: &[(&'static str, String)],
               src: &'static str,
               out: &mut Vec<(&'static str, String, &'static str)>| {
        for (t, s) in v {
            out.push((t, s.clone(), src));
        }
    };
    put(&lo.outer_pre, "shared", &mut out);
    // the outer text's trailing bidi marks are the start's (he B‏‎-5 – 2.5)
    if let (Some(last), Some(("literal", m))) = (out.last_mut(), l1.outer_pre.last().map(|p| (p.0, p.1.as_str())))
        && last.0 == "literal"
        && l1.outer_pre.len() == lo.outer_pre.len()
        && m.chars().all(|c| matches!(c, '\u{200e}' | '\u{200f}' | '\u{61c}'))
    {
        last.1 = m.to_string();
    }
    if collapse_middle {
        put(&l1.middle_pre, "shared", &mut out);
    }
    for (l, src) in [(&l1, "startRange"), (&l2, "endRange")] {
        let mut pre = &l.middle_pre[..];
        if src == "endRange" {
            // ICU's end span starts after a leading right-to-left mark (ar 3.00 € – ‏5.00 €).
            let mut s = sep.clone();
            if !collapse_middle && let Some((("literal", m), rest)) = pre.split_first().map(|(a, r)| ((a.0, a.1.as_str()), r)) && !m.is_empty() && m.chars().all(|c| matches!(c, '\u{200e}' | '\u{200f}' | '\u{61c}')) {
                s.push_str(m);
                pre = rest;
            }
            out.push(("literal", s, "shared"));
        }
        if !collapse_middle {
            put(pre, src, &mut out);
        }
        put(&l.number, src, &mut out);
        if !collapse_middle {
            // ...and the start's ends before a trailing bidi mark (ar 5‎%‎ – 2.5‎%‎), which
            // joins the separator.
            let post = &l.middle_post[..];
            match post.split_last() {
                Some((("literal", m), rest)) if src == "startRange" && matches!(m.as_str(), "\u{200e}" | "\u{61c}") => {
                    put(rest, src, &mut out);
                    sep.insert_str(0, m);
                }
                _ => put(post, src, &mut out),
            }
        }
    }
    if collapse_middle {
        put(&l1.middle_post, "shared", &mut out);
    }
    put(&lo.outer_post, "shared", &mut out);
    // A collapsed currency whose spacing ICU inserts (es-MX EUR 3.00-5.00) leaves its spans one
    // character early, as node reports them: a part belongs to a span it lies wholly in.
    let loc = num_locale_nu(locale, nu);
    if collapse_middle
        && style == "currency"
        && loc.cur.contains("\u{a4}#")
        && l1.middle_pre.last().is_some_and(|(t, v)| *t == "literal" && v == "\u{a0}")
    {
        let mut spans: [(usize, usize); 2] = [(usize::MAX, 0); 2];
        let mut at = 0;
        for (_, v, src) in &out {
            let n = v.chars().count();
            let k = match *src {
                "startRange" => 0,
                "endRange" => 1,
                _ => 2,
            };
            if k < 2 {
                spans[k].0 = spans[k].0.min(at);
                spans[k].1 = at + n;
            }
            at += n;
        }
        let mut at = 0;
        for part in out.iter_mut() {
            let n = part.1.chars().count();
            part.2 = "shared";
            for (k, (a, b)) in spans.iter().enumerate() {
                if *a != usize::MAX && *a >= 1 && at + 1 >= *a && at + n < *b {
                    part.2 = if k == 0 { "startRange" } else { "endRange" };
                }
            }
            at += n;
        }
    }
    out
}

fn x_is_nan(p: &Parts) -> bool {
    p.items.iter().any(|(t, _)| *t == "nan")
}

const PLURALS: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];

/// A generated row's form for a plural category ("" in a row means the same as other).
fn plural_pick(row: &[S; 6], plural: &str) -> &'static str {
    let i = PLURALS.iter().position(|p| *p == plural).unwrap_or(5);
    if row[i].is_empty() { row[5].get() } else { row[i].get() }
}

/// Splits a "{0}" pattern around the formatted number: the text around it is a part of type
/// `ty` with its outer spaces as literals.
fn wrap_pattern(pattern: &str, num: &Parts, ty: &'static str, out: &mut Parts) {
    // ar names one and two without the number (متر, متران).
    let numberless = !pattern.contains("{0}");
    let (pre, post) = pattern.split_once("{0}").unwrap_or(("", pattern));
    for (s, is_pre) in [(pre, true), (post, false)] {
        if !is_pre && !numberless {
            out.extend(num);
        }
        if s.is_empty() {
            continue;
        }
        // ICU gives the spaces and bidi marks around a unit's text as literals (ar ‎%‎).
        let edge = |c: char| c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{61c}');
        let core = s.trim_matches(edge);
        let ws_l = s.len() - s.trim_start_matches(edge).len();
        if ws_l > 0 {
            out.push("literal", &s[..ws_l]);
        }
        if !core.is_empty() {
            out.push(ty, core);
        }
        if ws_l + core.len() < s.len() {
            out.push("literal", &s[ws_l + core.len()..]);
        }
    }
}

fn unit_pattern(locale: &str, unit: &str, disp: usize, plural: &str) -> String {
    let lang = locale.split('-').next().unwrap_or("en");
    let locale = if UNIT_PATTERNS.iter().any(|r| r.0 == lang) {
        locale
    } else {
        "en"
    };
    let d = disp as u8;
    let simple = |u: &str| {
        find_row(UNIT_PATTERNS, locale, |r, k| r.0 == k && r.1 == u && r.2 == d)
            .map(|r| plural_pick(&r.3, plural))
    };
    if let Some(p) = simple(unit) {
        // ar's Arabic-Indic percent unit has its own sign and mark (٥٪؜).
        if unit == "percent" && arab_digits(locale) {
            return p.replace("\u{200e}%\u{200e}", "\u{66a}\u{61c}");
        }
        return p.to_string();
    }
    let ck = table_key(locale, "compound", |k| COMPOUND_PATTERNS.iter().any(|r| r.0 == k));
    if let Some(r) = COMPOUND_PATTERNS
        .iter()
        .find(|r| r.0 == ck && r.1 == unit && r.2 == d)
    {
        return plural_pick(&r.3, plural).to_string();
    }
    if let Some((a, b)) = unit.split_once("-per-") {
        if a == "percent"
            && let Some(r) = PERCENT_PER
                .iter()
                .find(|r| r.0 == table_key(locale, "percent_per", |k| PERCENT_PER.iter().any(|r| r.0 == k)) && r.1 == d)
        {
            let p = plural_pick(&r.2, plural);
            if arab_digits(locale) {
                return p.replace("\u{200e}%\u{200e}", "\u{66a}\u{61c}");
            }
            return p.to_string();
        }
        if let Some(x) = simple(a)
            && let Some(per) = find_row(PER_PATTERNS, locale, |r, k| r.0 == k && r.1 == b && r.2 == d)
        {
            return per.3.replacen("{0}", x, 1);
        }
    }
    "{0}".to_string()
}

use super::number::{self, Num};
use super::numdata::{LISTS, RELATIVE, RELATIVE_AUTO};
use super::numtext::S;
use super::plurdata::PLURAL_RULES;
use super::{Parts, Spec, plural_categories, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;

const UNITS: [&str; 8] = [
    "second", "minute", "hour", "day", "week", "month", "quarter", "year",
];

pub(super) fn resolve(kind: i32, locale: &str, tags: &[alloc::string::String], raw: &Spec) -> Result<Spec, Fault> {
    let mut out = Spec::default();
    // RelativeTimeFormat keeps a -u-nu- keyword it formats in.
    let nus = if kind == 5 { Some(super::numsys::resolve(locale, tags, raw)?) } else { None };
    out.put("locale", nus.as_ref().map_or(locale, |n| n.1.as_str()));
    let lang = locale.split('-').next().unwrap_or("en");
    match kind {
        3 => {
            // ICU's plural data is per language (pt-PT has its own): the region is dropped.
            let mut out2 = Spec::default();
            let supported = super::NUM_LANGS.contains(&lang);
            // The default locale (no requested one has data) stays en-US.
            let default = !tags.iter().any(|t| super::NUM_LANGS.contains(&t.split('-').next().unwrap_or("")));
            out2.put(
                "locale",
                if !supported || locale == "pt-PT" || default {
                    locale
                } else {
                    lang
                },
            );
            let mut out = out2;
            let ty = raw.choose(
                "type",
                &["cardinal", "ordinal"],
                "cardinal",
                "Intl.PluralRules",
            )?;
            out.put("type", ty);
            number::digit_options(raw, &mut out, (0, 3), false)?;
            // As V8: with a roundingPriority, resolvedOptions shows no fraction digits.
            if raw.get("roundingPriority").is_some_and(|p| p != "auto") {
                out.hide("minimumFractionDigits");
                out.hide("maximumFractionDigits");
            }
            out.put(
                "pluralCategories",
                &format!("@{}", plural_categories(lang, ty == "ordinal")),
            );
            out.flush_later();
            return Ok(out);
        }
        4 => {
            let n = "Intl.ListFormat";
            let ty = raw.choose(
                "type",
                &["conjunction", "disjunction", "unit"],
                "conjunction",
                n,
            )?;
            let st = raw.choose("style", &["long", "short", "narrow"], "long", n)?;
            out.put("type", ty);
            out.put("style", st);
        }
        5 => {
            let n = "Intl.RelativeTimeFormat";
            let st = raw.choose("style", &["long", "short", "narrow"], "long", n)?;
            let nu = raw.choose("numeric", &["always", "auto"], "always", n)?;
            out.put("style", st);
            out.put("numeric", nu);
            out.put("numberingSystem", nus.as_ref().map_or("latn", |n| n.0.as_str()));
        }
        _ => {
            let g = raw.choose(
                "granularity",
                &["grapheme", "word", "sentence"],
                "grapheme",
                "Intl.Segmenter",
            )?;
            out.put("granularity", g);
        }
    }
    Ok(out)
}

fn index(values: &[&str], v: Option<&str>) -> u8 {
    values.iter().position(|x| Some(*x) == v).unwrap_or(0) as u8
}

/// Regions whose English inherits en-001 (British usage, such as no serial comma in lists).
const EN_001: &[&str] = &["001", "150", "AG", "AI", "AT", "AU", "BB", "BE", "BM", "BS", "BW", "BZ", "CA", "CC", "CH", "CK", "CM", "CX", "CY", "DE", "DG", "DK", "DM", "ER", "FI", "FJ", "FK", "FM", "GB", "GD", "GG", "GH", "GI", "GM", "GY", "HK", "IE", "IL", "IM", "IN", "IO", "JE", "JM", "KE", "KI", "KN", "KY", "LC", "LR", "LS", "MG", "MO", "MS", "MT", "MU", "MV", "MW", "MY", "NA", "NF", "NG", "NL", "NR", "NU", "NZ", "PG", "PK", "PN", "PW", "RW", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SL", "SS", "SX", "SZ", "TC", "TK", "TO", "TT", "TV", "TZ", "UG", "VC", "VG", "VU", "WS", "ZA", "ZM", "ZW"];

/// The data key of a locale's lists: en-GB's for English that inherits en-001, else its own.
fn list_key(locale: &str) -> &str {
    let mut it = locale.split('-');
    let own = |r: &str| LISTS.iter().any(|row| *row.0 == format!("en-{r}"));
    if it.next() == Some("en") && it.take_while(|s| s.len() != 1).any(|s| EN_001.contains(&s) && !own(s)) {
        "en-GB"
    } else {
        locale
    }
}

/// Whether a Chinese locale takes Hong Kong's RelativeTimeFormat text (zh-HK, zh-MO, zh-Hant-HK).
fn zh_hk(locale: &str) -> bool {
    let mut it = locale.split('-');
    it.next() == Some("zh") && {
        let rest: alloc::vec::Vec<&str> = it.take_while(|s| s.len() != 1).collect();
        (rest.contains(&"HK") || rest.contains(&"MO")) && !rest.contains(&"Hans")
    }
}

fn unit_index(u: &str) -> Option<usize> {
    let u = u.strip_suffix('s').unwrap_or(u);
    UNITS.iter().position(|x| *x == u)
}

pub(super) fn format(
    kind: i32,
    spec: &Spec,
    n: Option<f64>,
    s: Option<&str>,
    y: Option<&str>,
) -> Result<Parts, Fault> {
    let locale = super::numsys::base_locale(spec.get("locale").unwrap_or("en-US"));
    let lang = locale.split('-').next().unwrap_or("en");
    let mut out = Parts::default();
    match kind {
        3 => {
            let x = match n {
                Some(v) => number::from_f64(v),
                None => return Ok(out),
            };
            let cat = match x {
                Num::Dec(d) => {
                    let g = number::Digits {
                        min_int: 1,
                        min_fd: spec.int("minimumFractionDigits", spec.int("@minimumFractionDigits", 0)),
                        max_fd: spec.int("maximumFractionDigits", spec.int("@maximumFractionDigits", 3)),
                        min_sd: spec.int("minimumSignificantDigits", 1),
                        max_sd: spec.int("maximumSignificantDigits", 21),
                        kind: if spec.get("minimumSignificantDigits").is_some() {
                            "significantDigits"
                        } else {
                            "fractionDigits"
                        },
                        priority: "auto",
                        inc: spec.int("roundingIncrement", 1),
                    };
                    let r = number::round(
                        &d,
                        &g,
                        spec.get("roundingMode").unwrap_or("halfExpand"),
                        false,
                    );
                    number::plural_of(lang, &r, spec.get("type") == Some("ordinal"))
                }
                _ => "other",
            };
            // selectRange passes the end's category: CLDR's plural ranges for cardinals, and
            // (as node 22) other for ordinals.
            let cat = match y {
                None => cat,
                // A language with CLDR rules here takes CLDR's range of the ends' ordinal categories.
                Some(b) if spec.get("type") == Some("ordinal") && rule_categories(lang, true).is_some() => {
                    super::plurdata::PLURAL_RANGES_CLDR.iter().find(|r| r.0 == lang && r.1 == cat && r.2 == b).map_or("other", |r| r.3)
                }
                Some(_) if spec.get("type") == Some("ordinal") => "other",
                Some(b) => {
                    let b = ["zero", "one", "two", "few", "many", "other"]
                        .into_iter()
                        .find(|c| *c == b)
                        .unwrap_or("other");
                    number::plural_range(lang, cat, b)
                }
            };
            out.push("literal", cat);
        }
        4 => {
            let items = s.unwrap_or("");
            let list: alloc::vec::Vec<&str> = if items.is_empty() {
                alloc::vec::Vec::new()
            } else {
                items.split('\u{1}').collect()
            };
            let ty = index(&["conjunction", "disjunction", "unit"], spec.get("type"));
            let st = index(&["long", "short", "narrow"], spec.get("style"));
            let row = number::find_row(LISTS, list_key(locale), |r, k| r.0 == k && r.1 == ty && r.2 == st)
                .or_else(|| LISTS.iter().find(|r| r.0 == "en" && r.1 == ty && r.2 == st))
                .map_or([", "; 4], |r| r.3.map(S::get));
            // Spanish: y is e before an i sound (i…, hi… but not hia…, hie…), o is u before an
            // o sound (o…, ho…, 8…, 11), as ICU decides from the spelling.
            let es_fix = |s: &'static str, w: &str| -> &'static str {
                let l = w.to_lowercase();
                let b = l.as_bytes();
                match s {
                    " y "
                        if b.first() == Some(&b'i')
                            || (l.starts_with("hi") && !matches!(b.get(2), Some(b'a' | b'e'))) =>
                    {
                        " e "
                    }
                    " o "
                        if b.first() == Some(&b'o')
                            || l.starts_with("ho")
                            || l.starts_with('8')
                            || l == "11" =>
                    {
                        " u "
                    }
                    _ => s,
                }
            };
            let mut sep = row[3];
            let mut pair = row[0];
            if lang == "es"
                && let Some(w) = list.last()
            {
                sep = es_fix(sep, w);
                pair = es_fix(pair, w);
            }
            let row = [pair, row[1], row[2], row[3]];
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    let lit = if list.len() == 2 {
                        row[0]
                    } else if i == list.len() - 1 {
                        sep
                    } else if i == 1 {
                        row[1]
                    } else {
                        row[2]
                    };
                    out.push("literal", lit);
                }
                out.push("element", item);
            }
        }
        5 => {
            let Some(v) = n else {
                return range("Invalid number value");
            };
            if !v.is_finite() {
                return range("Invalid time value");
            }
            let unit = y.unwrap_or("");
            let Some(ui) = unit_index(unit) else {
                return range(&format!("Invalid unit argument for format() '{unit}'"));
            };
            let st = index(&["long", "short", "narrow"], spec.get("style"));
            let st_l = st;
            let neg = v < 0.0 || (v == 0.0 && v.is_sign_negative());
            let hk = zh_hk(locale);
            let auto_key = if hk { "zh-HK" } else { number::table_key(locale, "relative_auto", |k| RELATIVE_AUTO.iter().any(|r| r.0 == k)) };
            if spec.get("numeric") == Some("auto")
                && v == (v as i64) as f64
                && let Some(r) = RELATIVE_AUTO.iter().find(|r| {
                    r.0 == auto_key && usize::from(r.1) == ui && r.2 == st_l && f64::from(r.3) == v
                })
            {
                out.push("literal", r.4.get());
                return Ok(out);
            }
            let mut nspec = Spec::default();
            nspec.put("locale", locale);
            nspec.put("numberingSystem", spec.get("numberingSystem").unwrap_or("latn"));
            let f = number::format(&nspec, number::from_f64(v.abs()));
            let plural = {
                let d = number::from_f64(v.abs());
                match d {
                    Num::Dec(d) => {
                        let g = number::Digits {
                            min_int: 1,
                            min_fd: 0,
                            max_fd: 3,
                            min_sd: 1,
                            max_sd: 21,
                            kind: "fractionDigits",
                            priority: "auto",
                            inc: 1,
                        };
                        number::plural_of(lang, &number::round(&d, &g, "halfExpand", false), false)
                    }
                    _ => "other",
                }
            };
            let dir = u8::from(neg);
            let pick = |l: &str, cat: &str| {
                let hit = |r: &&(S, u8, u8, u8, S, S), k: &str| {
                    r.0 == k && usize::from(r.1) == ui && r.2 == st_l && r.3 == dir && r.4 == cat
                };
                if hk && l == locale {
                    RELATIVE.iter().find(|r| hit(r, "zh-HK"))
                } else {
                    number::find_row(RELATIVE, l, |r, k| hit(&r, k))
                }
            };
            let pat = pick(locale, plural)
                .or_else(|| pick(locale, "other"))
                .or_else(|| pick("en", plural))
                .or_else(|| pick("en", "other"))
                .map_or("{0}", |r| r.5.get());
            match pat.split_once("{0}") {
                Some((pre, post)) => {
                    out.push("literal", pre);
                    out.extend(&f.parts);
                    out.push("literal", post);
                }
                // ar names one and two without the number (قبل يوم).
                None => out.push("literal", pat),
            }
        }
        _ => return type_err("unsupported Intl kind"),
    }
    Ok(out)
}

/// A value's plural category by its language's CLDR rules (plurdata.rs), for the languages that
/// have them there. i, v and f are CLDR's operands, n the absolute value.
pub(super) fn rule_plural(lang: &str, i: u64, v: u32, f: u64, n: f64, ordinal: bool) -> Option<&'static str> {
    let row = PLURAL_RULES.iter().find(|r| r.0 == lang && r.1 == ordinal)?;
    let (mut t, mut w) = (f, v);
    while w > 0 && t % 10 == 0 {
        t /= 10;
        w -= 1;
    }
    let ops = [n.abs(), i as f64, f64::from(v), f64::from(w), f as f64, t as f64];
    Some(row.3.iter().find(|r| rule_holds(r.1, &ops)).map_or("other", |r| r.0))
}

/// The categories resolvedOptions lists for a language with rules in plurdata.rs.
pub(super) fn rule_categories(lang: &str, ordinal: bool) -> Option<&'static str> {
    PLURAL_RULES.iter().find(|r| r.0 == lang && r.1 == ordinal).map(|r| r.2)
}

/// Whether a CLDR rule ("v = 0 and i % 10 = 1 or n = 0..2") holds for operands n, i, v, w, f, t
/// (c and e, the compact exponent, are 0).
fn rule_holds(rule: &str, ops: &[f64; 6]) -> bool {
    rule.split(" or ").any(|and| and.split(" and ").all(|rel| rule_relation(rel, ops)))
}

fn rule_relation(rel: &str, ops: &[f64; 6]) -> bool {
    let (lhs, rhs, eq) = match rel.split_once("!=") {
        Some((l, r)) => (l, r, false),
        None => match rel.split_once('=') {
            Some((l, r)) => (l, r, true),
            None => return false,
        },
    };
    let (op, m) = match lhs.split_once('%') {
        Some((o, m)) => (o.trim(), m.trim().parse::<f64>().ok()),
        None => (lhs.trim(), None),
    };
    let mut x = match op {
        "n" => ops[0],
        "i" => ops[1],
        "v" => ops[2],
        "w" => ops[3],
        "f" => ops[4],
        "t" => ops[5],
        _ => 0.0,
    };
    if let Some(m) = m {
        x %= m;
    }
    let num = |s: &str| s.trim().parse::<f64>().unwrap_or(f64::NAN);
    let hit = rhs.split(',').any(|r| match r.split_once("..") {
        Some((a, b)) => x == (x as i64) as f64 && num(a) <= x && x <= num(b),
        None => num(r) == x,
    });
    hit == eq
}

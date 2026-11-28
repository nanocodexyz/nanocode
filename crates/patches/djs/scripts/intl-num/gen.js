// Generates crates/djs-core/src/builtins/intl/numdata.rs from node's ICU (node 22):
//   node gen.js > ../../crates/djs-core/src/builtins/intl/numdata.rs
// Unit patterns per language x unit x display x plural category ("{0}" is the number), the
// per-unit pattern of each unit (X-per-Y = per[Y] with {0} replaced by X's pattern), the
// compounds ICU names itself (checked: every X-per-Y that the per rule does not give),
// currency name patterns and symbols. Plural categories: zero one two few many other;
// "" means the same as other.
const LANGS = ["en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru", "pl", "tr", "ar", "hi", "id", "vi",
  "hu", "nb", "no", "he", "ta", "sr", "cs", "uk", "sv", "da", "ms", "ro", "bn", "el", "sw", "ur", "fil", "ca", "fa", "th", "fi",
  "en-GB", "en-IN", "es-MX", "fr-CA", "zh-Hant", "sr-Latn", "fa-AF"];
// Regional data: its rows are kept only where they differ from its language's (djs reads a
// regional row first, then the language's).
const REGIONAL = { "en-GB": "en", "en-IN": "en", "es-MX": "es", "fr-CA": "fr", "zh-Hant": "zh", "sr-Latn": "sr", "fa-AF": "fa" };
const SIMPLE = ["acre", "bit", "byte", "celsius", "centimeter", "day", "degree", "fahrenheit", "fluid-ounce",
  "foot", "gallon", "gigabit", "gigabyte", "gram", "hectare", "hour", "inch", "kilobit",
  "kilobyte", "kilogram", "kilometer", "liter", "megabit", "megabyte", "meter", "microsecond",
  "mile", "mile-scandinavian", "milliliter", "millimeter", "millisecond", "minute", "month",
  "nanosecond", "ounce", "percent", "petabyte", "pound", "second", "stone", "terabit",
  "terabyte", "week", "yard", "year"];
// Regional locales with their own RelativeTimeFormat text in ICU, appended to RELATIVE and
// RELATIVE_AUTO by gen-rtf-region.js (text.rs reads zh-HK's rows for zh-HK, zh-MO and zh-Hant-HK).
const RTF_REGIONS = ["zh-HK"];
// Table lines ([locale, line]) with each RTF_REGIONS locale's rows after its script's (zh-Hant).
const RTF_AFTER = { "zh-HK": "zh-Hant" };
function withRegions(rows, table) {
  const lines = rows.map((r) => r[1]);
  for (const l of RTF_REGIONS) {
    let at = rows.length;
    for (let i = rows.length - 1; i >= 0; i--) if (rows[i][0] === RTF_AFTER[l]) { at = i + 1; break; }
    lines.splice(at, 0, "    // " + l + " (scripts/intl-num/gen-rtf-region.js " + l + ")", ...require("./gen-rtf-region.js").rows(l, str)[table]);
  }
  return lines;
}
const CATS = ["zero", "one", "two", "few", "many", "other"];
const DISP = ["short", "long", "narrow"];
const CUR = process.argv[2] ? process.argv[2].split(",") : null;
const SAMPLES = [1, 2, 0, 3, 5, 11, 21, 22, 100, 1000000, 0.5, 1.5, 1.25, 7, 25, 4, 6, 9];
function samples(lang, opts) {
  const ro = opts ? new Intl.NumberFormat(lang, opts).resolvedOptions() : {};
  const pr = new Intl.PluralRules(lang, opts ? { minimumFractionDigits: ro.minimumFractionDigits, maximumFractionDigits: ro.maximumFractionDigits } : {}), out = {};
  for (const n of SAMPLES) { const c = pr.select(n); if (!(c in out)) out[c] = n; }
  return out;
}
const esc = (s) => '"' + [...s].map((c) => { const u = c.codePointAt(0); return c === '"' ? '\\"' : c === "\\" ? "\\\\" : u < 0x20 || u > 0x7e ? "\\u{" + u.toString(16) + "}" : c; }).join("") + '"';
function pats(lang, opts, sm) {
  const f = new Intl.NumberFormat(lang, opts), ro = f.resolvedOptions();
  const nf = new Intl.NumberFormat(lang, { minimumFractionDigits: ro.minimumFractionDigits, maximumFractionDigits: ro.maximumFractionDigits });
  return CATS.map((c) => {
    if (!(c in sm)) return null;
    const n = sm[c], s = f.format(n), num = nf.format(n);
    // ar names one and two without the number (فدان, فدانان)
    if (!s.includes(num)) { if (c !== "one" && c !== "two") throw new Error("no number in " + s); return s; }
    return s.replace(num, "{0}");
  });
}
// a category the options never reach (other for a currency without fractions in ru) takes many's
const fill = (p) => { const o = p[5] ?? p[4] ?? p.find((x) => x !== null); return p.map((x, i) => (i === 5 ? o : x === null || x === o ? "" : x)); };
const units = [], pers = [], comps = [], names = [], syms = [];
let checked = 0;
for (const lang of LANGS) {
  const sm = samples(lang);
  for (let d = 0; d < 3; d++) {
    const P = {};
    for (const u of SIMPLE) { P[u] = pats(lang, { style: "unit", unit: u, unitDisplay: DISP[d] }, sm); units.push([lang, u, d, fill(P[u])]); }
    const per = {};
    for (const y of SIMPLE) {
      // the per pattern of y, from bit-per-y (or byte-per-y)
      for (const x of ["bit", "byte", "acre"]) {
        if (x === y) continue;
        const c = pats(lang, { style: "unit", unit: x + "-per-" + y, unitDisplay: DISP[d] }, sm);
        const cand = CATS.map((k, i) => (c[i] === null ? null : c[i].includes(P[x][i]) ? c[i].replace(P[x][i], "{0}") : undefined));
        const vals = [...new Set(cand.filter((v) => v !== null))];
        if (vals.length === 1 && vals[0] !== undefined) { per[y] = vals[0]; break; }
      }
      if (per[y] === undefined) throw new Error("no per pattern " + lang + " " + y + " " + d);
      pers.push([lang, y, d, per[y]]);
    }
    for (const x of SIMPLE) for (const y of SIMPLE) {
      if (x === y) continue;
      const c = pats(lang, { style: "unit", unit: x + "-per-" + y, unitDisplay: DISP[d] }, sm);
      checked++;
      const ok = CATS.every((k, i) => c[i] === null || c[i] === per[y].replace("{0}", P[x][i]));
      if (!ok) comps.push([lang, x + "-per-" + y, d, fill(c)]);
    }
  }
  const codes = CUR || Intl.supportedValuesOf("currency");
  for (const code of codes) {
    // Names for every category: fraction digits as the sample needs them (1 Indian rupee).
    const co = { style: "currency", currency: code, currencyDisplay: "name", minimumFractionDigits: 0, maximumFractionDigits: 3 };
    const p = pats(lang, co, samples(lang));
    names.push([lang, code, fill(p)]);
    const sym = (cd) => new Intl.NumberFormat(lang, { style: "currency", currency: code, currencyDisplay: cd }).formatToParts(1).find((x) => x.type === "currency").value;
    syms.push([lang, code, sym("symbol"), sym("narrowSymbol")]);
  }
}
// ICU formats percent-per-Y as the bare percent pattern in some displays: one row per language and display.
const pctper = [];
for (const lang of LANGS) for (let d = 0; d < 3; d++) {
  const rows = comps.filter((c) => c[0] === lang && c[2] === d && c[1].startsWith("percent-per-"));
  if (!rows.length) continue;
  const k = JSON.stringify(rows[0][3]);
  if (rows.length !== SIMPLE.length - 1 || rows.some((r) => JSON.stringify(r[3]) !== k)) throw new Error("percent-per not uniform " + lang + d);
  pctper.push([lang, d, rows[0][3]]);
}
const kept = comps.filter((c) => !c[1].startsWith("percent-per-") || !pctper.some((p) => p[0] === c[0] && p[1] === c[2]));
comps.length = 0; comps.push(...kept);
// Ranges: the separator of formatRange(1, 2), the approximately sign of formatRange(5, 5), and
// PluralRules.selectRange for each pair of categories (cardinal; ordinal ranges are the same in ICU).
const ranges = [], prange = [];
for (const lang of LANGS) {
  const nf = new Intl.NumberFormat(lang);
  const sep = nf.formatRange(1, 2).slice(1, -1), approx = nf.formatRange(5, 5).replace(nf.format(5), "");
  ranges.push([lang, sep, approx]);
  if (lang in REGIONAL) continue;
  const sm = samples(lang), pr = new Intl.PluralRules(lang);
  for (const a of CATS) for (const b of CATS) {
    if (!(a in sm) || !(b in sm)) continue;
    const r = pr.selectRange(sm[a], sm[b]);
    if (r !== b) prange.push([lang, a, b, r]);
  }
}
// Compact patterns: per language, display and magnitude 3..20, the power of ten the number is
// divided by, the pattern ({0} the number) for each plural category of the shown number, and
// the pattern for exactly 1 where CLDR names one (fr mille).
const compact = [];
for (const lang of LANGS) for (const long of [false, true]) {
  const f = new Intl.NumberFormat(lang, { notation: "compact", compactDisplay: long ? "long" : "short", maximumFractionDigits: 2 });
  const plain = new Intl.NumberFormat(lang, { maximumFractionDigits: 2, useGrouping: "min2" });
  const pr = new Intl.PluralRules(lang, { maximumFractionDigits: 2 });
  for (let m = 3; m <= 20; m++) {
    const p2 = f.formatToParts(2 * 10 ** m);
    if (!p2.some((p) => p.type === "compact")) { compact.push([lang, long, m, 0, null, ""]); continue; }
    // the locale's own digits (fa, bn) read as ASCII ones
    const dg = [...Array(10).keys()].map((d) => new Intl.NumberFormat(lang).format(d));
    const shown = Number([...p2.filter((p) => p.type === "integer").map((p) => p.value).join("")].map((c) => (dg.includes(c) ? dg.indexOf(c) : c)).join(""));
    const div = m - Math.round(Math.log10(shown / 2));
    const pats = {};
    // 1.02 to 1.23 reach pl's and ru's few and one, and ar's few and many, at magnitudes that show
    // three digits; 1.55 and 1.555 their other (a fraction) at those showing two and three.
    for (const k of [2, 1.5, 3, 5, 11, 21, 22, 1.25, 7, 25, 1.01, 100, 101, 1.02, 1.03, 1.11, 1.21, 1.22, 1.55, 1.555]) {
      const v = k * 10 ** (m - div) >= 10 ** (m - div + 1) ? null : k;
      if (v === null) continue;
      const n = k * 10 ** (m - div);
      const c = pr.select(n);
      if (c in pats) continue;
      const s = f.format(k * 10 ** m), num = plain.format(n);
      if (!s.includes(num)) continue;
      pats[c] = s.replace(num, "{0}");
    }
    const one = f.format(10 ** m), on = plain.format(10 ** (m - div));
    const c1 = pr.select(10 ** (m - div));
    let exact = "";
    // it's one is itself without the number (mille, -mille); fr's is only exactly 1 (-1 millier).
    if (!one.includes(on) && f.format(-(10 ** m)) === "-" + one) pats[c1] = one;
    else if (!one.includes(on)) exact = one;
    else if (!(c1 in pats)) pats[c1] = one.replace(on, "{0}");
    else if (one.replace(on, "{0}") !== pats[c1]) exact = one.replace(on, "{0}");
    if (!Object.keys(pats).length) throw new Error("no compact pattern " + lang + " " + long + " " + m + " " + div + " " + f.format(2 * 10 ** m));
    // Past the largest pattern (trillions), magnitudes share one and show no fraction: take
    // their missing categories from its own magnitude.
    const same = compact.find((r) => r[0] === lang && r[1] === long && r[3] === div && r[4]);
    if (same) CATS.forEach((c, i) => { if (!(c in pats) && same[4][i] !== null) pats[c] = same[4][i]; });
    compact.push([lang, long, m, div, CATS.map((c) => pats[c] ?? null), exact]);
  }
}
// RelativeTimeFormat: (language, unit, style, past, plural category, pattern) and numeric auto's
// words (language, unit, style, value, text); ListFormat: (language, type, style, [pair, start,
// middle, end]).
const RUNITS = ["second", "minute", "hour", "day", "week", "month", "quarter", "year"];
const rel = [], relAuto = [], lists = [];
for (const lang of LANGS) {
  const sm = samples(lang), nf = new Intl.NumberFormat(lang);
  for (let s = 0; s < 3; s++) {
    const f = new Intl.RelativeTimeFormat(lang, { style: DISP[s === 0 ? 1 : s === 1 ? 0 : 2] });
    const fa = new Intl.RelativeTimeFormat(lang, { style: DISP[s === 0 ? 1 : s === 1 ? 0 : 2], numeric: "auto" });
    RUNITS.forEach((u, ui) => {
      for (const past of [0, 1]) for (const c of CATS) {
        if (!(c in sm)) continue;
        const n = sm[c], t = f.format(past ? -n : n, u), num = nf.format(n);
        if (!t.includes(num) && c !== "one" && c !== "two") throw new Error("rtf " + lang + u + t);
        rel.push([lang, ui, s, past, c, t.replace(num, "{0}")]);
      }
      for (const v of [-3, -2, -1, 0, 1, 2, 3]) {
        const t = fa.format(v, u);
        if (t !== f.format(v, u)) relAuto.push([lang, ui, s, v, t]);
      }
    });
  }
  for (let t = 0; t < 3; t++) for (let s = 0; s < 3; s++) {
    const f = new Intl.ListFormat(lang, { type: ["conjunction", "disjunction", "unit"][t], style: DISP[s === 0 ? 1 : s === 1 ? 0 : 2] });
    const between = (x, a, b) => { const i = x.indexOf(a) + a.length; return x.slice(i, x.indexOf(b, i)); };
    const two = f.format(["A", "B"]), three = f.format(["A", "B", "C"]), four = f.format(["A", "B", "C", "D"]);
    lists.push([lang, t, s, [between(two, "A", "B"), between(three, "A", "B"), between(four, "B", "C"), between(three, "B", "C")]]);
  }
}
const gaps = LANGS.map((lang) => {
  const p = new Intl.NumberFormat(lang, { style: "percent", notation: "compact" }).formatToParts(0.12);
  const i = p.findIndex((x) => x.type === "unit");
  return [lang, i > 0 && p[i - 1].type === "literal" ? p[i - 1].value : ""];
});
// Tables where every language has every key drop a regional row equal to its language's; tables
// where a missing row means something (no compound, no auto word) keep a regional locale's rows
// only when they differ from its language's as a whole.
const J = JSON.stringify;
const perRow = (rows, nkey) => {
  const base = new Map(rows.filter((r) => !(r[0] in REGIONAL)).map((r) => [J(r.slice(0, nkey)), J(r.slice(nkey))]));
  return rows.filter((r) => !(r[0] in REGIONAL) || base.get(J([REGIONAL[r[0]], ...r.slice(1, nkey)])) !== J(r.slice(nkey)));
};
// The tables whose rows a regional locale owns (possibly none: zh-Hant composes km/h).
const owned = [];
const perTable = (rows, name) => {
  const by = (l) => J(rows.filter((r) => r[0] === l).map((r) => r.slice(1)));
  for (const l in REGIONAL) if (by(l) !== by(REGIONAL[l])) owned.push([name, l]);
  return rows.filter((r) => !(r[0] in REGIONAL) || by(r[0]) !== by(REGIONAL[r[0]]));
};
const keep = (a, f) => { const k = f(a); a.length = 0; for (const r of k) a.push(r); };
keep(units, (a) => perRow(a, 3)); keep(pers, (a) => perRow(a, 3)); keep(names, (a) => perRow(a, 2));
keep(syms, (a) => perRow(a, 2)); keep(rel, (a) => perRow(a, 5)); keep(lists, (a) => perRow(a, 3));
keep(ranges, (a) => perRow(a, 1)); keep(gaps, (a) => perRow(a, 1));
keep(comps, (a) => perTable(a, "compound")); keep(pctper, (a) => perTable(a, "percent_per"));
keep(compact, (a) => perTable(a, "compact")); keep(relAuto, (a) => perTable(a, "relative_auto"));
const strings = new Map([["", 0]]);
const str = (s) => { if (!strings.has(s)) strings.set(s, strings.size); return "S(" + strings.get(s) + ")"; };
const arr = (a) => "[" + a.map(str).join(", ") + "]";
const o = [];
o.push("#![allow(clippy::type_complexity)]");
o.push("// Generated by scripts/intl-num/gen.js from node " + process.version + " (ICU " + process.versions.icu + "). Do not edit.");
o.push("// Plural forms: zero, one, two, few, many, other; S(0), \"\", is the same as other.");
o.push("use super::numtext::S;");
o.push("pub(super) static UNIT_PATTERNS: &[(S, S, u8, [S; 6])] = &[");
for (const [l, u, d, p] of units) o.push("    (" + str(l) + ", " + str(u) + ", " + d + ", " + arr(p) + "),");
o.push("];");
o.push("// The per pattern of a unit: X-per-Y is PER_PATTERNS[Y] with {0} replaced by X's pattern.");
o.push("pub(super) static PER_PATTERNS: &[(S, S, u8, S)] = &[");
for (const [l, u, d, p] of pers) o.push("    (" + str(l) + ", " + str(u) + ", " + d + ", " + str(p) + "),");
o.push("];");
o.push("// The X-per-Y compounds that the per rule does not give (" + checked + " checked).");
o.push("pub(super) static COMPOUND_PATTERNS: &[(S, S, u8, [S; 6])] = &[");
for (const [l, u, d, p] of comps) o.push("    (" + str(l) + ", " + str(u) + ", " + d + ", " + arr(p) + "),");
o.push("];");
o.push("// Languages and displays where percent-per-Y is this pattern, whatever Y.");
o.push("pub(super) static PERCENT_PER: &[(S, u8, [S; 6])] = &[");
for (const [l, d, p] of pctper) o.push("    (" + str(l) + ", " + d + ", " + arr(p) + "),");
o.push("];");
o.push("// (language, long, magnitude, power of ten shown numbers are divided by, patterns by plural, pattern of exactly 1)");
o.push("// The tables (compound, percent_per, compact, relative_auto) whose rows a regional locale");
o.push("// has in place of its language's, possibly none.");
o.push("pub(super) static REGIONAL_TABLES: &[(S, S)] = &[");
for (const [t, l] of owned) o.push("    (" + str(t) + ", " + str(l) + "),");
o.push("];");
o.push("pub(super) static COMPACT_PATTERNS: &[(S, bool, u8, u8, [S; 6], S)] = &[");
for (const [l, lg, m, d, p, e] of compact) if (p) o.push("    (" + str(l) + ", " + lg + ", " + m + ", " + d + ", " + arr(fill(p)) + ", " + str(e) + "),");
o.push("];");
o.push("// The text between a compact percent's number and its sign (ICU's own, not the percent pattern's).");
o.push("pub(super) static COMPACT_PERCENT_GAP: &[(S, S)] = &[");
for (const [l, g] of gaps) o.push("    (" + str(l) + ", " + str(g) + "),");
o.push("];");
o.push("// Styles are long, short, narrow; past is 1 for a negative value.");
o.push("pub(super) static RELATIVE: &[(S, u8, u8, u8, S, S)] = &[");
o.push(...withRegions(rel.map(([l, u, s, p, c, t]) => [l, "    (" + str(l) + ", " + u + ", " + s + ", " + p + ", " + str(c) + ", " + str(t) + "),"]), "rel"));
o.push("];");
o.push("pub(super) static RELATIVE_AUTO: &[(S, u8, u8, i8, S)] = &[");
o.push(...withRegions(relAuto.map(([l, u, s, v, t]) => [l, "    (" + str(l) + ", " + u + ", " + s + ", " + v + ", " + str(t) + "),"]), "auto"));
o.push("];");
o.push("// Types are conjunction, disjunction, unit.");
o.push("pub(super) static LISTS: &[(S, u8, u8, [S; 4])] = &[");
for (const [l, t, s, p] of lists) o.push("    (" + str(l) + ", " + t + ", " + s + ", " + arr(p) + "),");
o.push("];");
o.push("// (language, range separator, approximately sign)");
o.push("pub(super) static RANGES: &[(S, S, S)] = &[");
for (const [l, s, a] of ranges) o.push("    (" + str(l) + ", " + str(s) + ", " + str(a) + "),");
o.push("];");
o.push("// The plural category of a range from the categories of its ends, where it is not the end's.");
o.push("pub(super) static PLURAL_RANGES: &[(S, S, S, S)] = &[");
for (const [l, a, b, r] of prange) o.push("    (" + str(l) + ", " + str(a) + ", " + str(b) + ", " + str(r) + "),");
o.push("];");
o.push("// Currency names: the whole pattern, {0} the number.");
o.push("pub(super) static CURRENCY_NAMES: &[(S, S, [S; 6])] = &[");
for (const [l, c, p] of names) o.push("    (" + str(l) + ", " + str(c) + ", " + arr(p) + "),");
o.push("];");
o.push("// (language, code, symbol, narrow symbol)");
o.push("pub(super) static CURRENCY_SYMBOLS_L: &[(S, S, S, S)] = &[");
for (const [l, c, s, n] of syms) o.push("    (" + str(l) + ", " + str(c) + ", " + str(s) + ", " + str(n) + "),");
o.push("];");
if (strings.size > 65536) throw new Error("S is a u16: " + strings.size + " strings");
const ends = [0];
for (const s of strings.keys()) ends.push(ends[ends.length - 1] + Buffer.byteLength(s));
o.push("// The strings the tables name: S(i) is TEXT[ENDS[i]..ENDS[i + 1]].");
o.push("pub(super) static TEXT: &str = concat!(");
for (const s of strings.keys()) if (s) o.push("    " + esc(s) + ",");
o.push(");");
o.push("pub(super) static ENDS: [u32; " + ends.length + "] = [" + ends.join(", ") + "];");
process.stdout.write(o.join("\n") + "\n");
process.stderr.write("units " + units.length + " pers " + pers.length + " compounds " + comps.length + "/" + checked + " names " + names.length + "\n");

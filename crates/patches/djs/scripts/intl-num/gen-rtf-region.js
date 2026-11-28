// Rows of RELATIVE and RELATIVE_AUTO for a regional locale whose RelativeTimeFormat text has its
// own data in ICU (zh-HK: Traditional, Hong Kong wording). gen.js appends them for every locale
// in RTF_REGIONS; run alone with node 22 to print one locale's rows:
//   node gen-rtf-region.js zh-HK
const UNITS = ["second", "minute", "hour", "day", "week", "month", "quarter", "year"];
const STYLES = ["long", "short", "narrow"];
const esc = (s) => '"' + [...s].map((c) => { const u = c.codePointAt(0); return c === '"' ? '\\"' : c === "\\" ? "\\\\" : u < 0x20 || u > 0x7e ? "\\u{" + u.toString(16) + "}" : c; }).join("") + '"';
function rows(L, str = esc) {
  const rel = [], auto = [];
  STYLES.forEach((st, si) => UNITS.forEach((u, ui) => {
    const f = new Intl.RelativeTimeFormat(L, { style: st });
    for (const dir of [0, 1]) {
      const p = f.formatToParts(dir ? -3 : 3, u).map((x) => (x.type === "literal" ? x.value : "{0}")).join("");
      rel.push("    (" + [str(L), ui, si, dir, str("other"), str(p)].join(", ") + "),");
    }
    const a = new Intl.RelativeTimeFormat(L, { style: st, numeric: "auto" });
    for (let v = -3; v <= 3; v++) {
      const s = a.format(v, u);
      if (!/\d/.test(s)) auto.push("    (" + [str(L), ui, si, v, str(s)].join(", ") + "),");
    }
  }));
  return { rel, auto };
}
module.exports = { rows };
if (require.main === module) {
  const { rel, auto } = rows(process.argv[2]);
  console.log(rel.join("\n"));
  console.log("//AUTO");
  console.log(auto.join("\n"));
}

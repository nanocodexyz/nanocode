//! `[[ … ]]` (D1272): the conditional command. Words are expanded without splitting or
//! globbing; the right side of `==`/`!=` is a pattern (extglob always on), of `=~` a
//! POSIX extended regular expression (psh-regex) whose quoted parts match literally.
//! File, string and variable tests are shared with `test` (`builtins::test`); the
//! arithmetic comparisons evaluate both sides as arithmetic expressions.

use std::rc::Rc;

use psh_syntax::{BinaryTest, Command, CondExpr, CondKind, Part, Span, UnaryTest, Word, WordPart};

use crate::builtins;
use crate::expand::{self, Ctx};
use crate::options::{SetOpt, Shopt};
use crate::state::{Code, R, Unwind, Vp};
use crate::vars::Value;

/// Runs `[[ expr ]]`: 0 true, 1 false, 2 for an invalid regular expression.
pub(crate) async fn run(vp: &mut Vp, code: &Rc<Code>, cmd: &Command, e: &CondExpr) -> R {
    vp.line = code.line(cmd.span.start);
    eval(vp, code, e, false).await
}

/// Evaluates to a status, as bash does: `!` turns any failure (also an error) into
/// success, `&&` and `||` look only at zero or not. `neg` is set under `!`, for the
/// trace line.
fn eval<'a>(
    vp: &'a mut Vp,
    code: &'a Rc<Code>,
    e: &'a CondExpr,
    neg: bool,
) -> crate::sched::LocalBoxFuture<'a, R> {
    Box::pin(async move {
        match &e.kind {
            CondKind::Not(inner) => {
                let s = eval(vp, code, inner, true).await?;
                Ok(i32::from(s == 0))
            }
            CondKind::And(a, b) => {
                let s = eval(vp, code, a, false).await?;
                if s != 0 {
                    return Ok(s);
                }
                eval(vp, code, b, false).await
            }
            CondKind::Or(a, b) => {
                let s = eval(vp, code, a, false).await?;
                if s == 0 {
                    return Ok(0);
                }
                eval(vp, code, b, false).await
            }
            CondKind::Group(inner) => eval(vp, code, inner, false).await,
            CondKind::Word(w) => {
                let v = single(vp, code, w).await?;
                trace(vp, neg, &[b"-n", &trace_word(&v)]).await;
                Ok(i32::from(v.is_empty()))
            }
            CondKind::Unary { op, op_span, arg } => {
                let v = single(vp, code, arg).await?;
                let op_text = code.ast.bytes(*op_span).to_vec();
                trace(vp, neg, &[&op_text, &trace_word(&v)]).await;
                unary(vp, *op, &op_text, &v).await
            }
            CondKind::Binary {
                op,
                op_span,
                lhs,
                rhs,
            } => binary(vp, code, *op, code.ast.bytes(*op_span), lhs, rhs, neg).await,
        }
    })
}

async fn single(vp: &mut Vp, code: &Rc<Code>, w: &Word) -> Result<Vec<u8>, Unwind> {
    Ok(Box::pin(expand::expand_single(vp, code, w, Ctx::Word))
        .await?
        .0)
}

async fn unary(vp: &mut Vp, op: UnaryTest, op_text: &[u8], v: &[u8]) -> R {
    let t = match op {
        UnaryTest::StringHasZeroLength => v.is_empty(),
        UnaryTest::StringHasNonZeroLength => !v.is_empty(),
        _ => match builtins::test::unary(vp, op_text, v).await {
            Ok(t) => t,
            // `-t x`: not a file descriptor number.
            Err(msg) => {
                vp.error(&format!("[[: {msg}")).await;
                return Ok(2);
            }
        },
    };
    Ok(i32::from(!t))
}

async fn binary(
    vp: &mut Vp,
    code: &Rc<Code>,
    op: BinaryTest,
    op_text: &[u8],
    lhs: &Word,
    rhs: &Word,
    neg: bool,
) -> R {
    use BinaryTest::*;
    let l = single(vp, code, lhs).await?;
    match op {
        StringExactlyMatchesPattern | StringDoesNotExactlyMatchPattern => {
            let r = rhs_pieces(vp, code, rhs, Mode::Pattern).await?;
            trace(vp, neg, &[&trace_word(&l), op_text, &r.trace]).await;
            let mut opts = vp.match_opts(vp.opts.shopt(Shopt::Nocasematch));
            // bash matches as if extglob were on.
            opts.extglob = true;
            let hit = crate::pattern::Pattern::with(&r.text, opts).matches(&l, vp.utf8());
            Ok(i32::from(hit != (op == StringExactlyMatchesPattern)))
        }
        StringMatchesRegex => {
            let r = rhs_pieces(vp, code, rhs, Mode::Regex).await?;
            trace(vp, neg, &[&trace_word(&l), op_text, &r.trace]).await;
            regex_match(vp, &l, &r.text).await
        }
        _ => {
            let r = single(vp, code, rhs).await?;
            trace(vp, neg, &[&trace_word(&l), op_text, &trace_word(&r)]).await;
            match op {
                ArithmeticEqualTo
                | ArithmeticNotEqualTo
                | ArithmeticLessThan
                | ArithmeticLessThanOrEqualTo
                | ArithmeticGreaterThan
                | ArithmeticGreaterThanOrEqualTo => {
                    let Some(a) = arith(vp, &l).await else {
                        return Ok(1);
                    };
                    let Some(b) = arith(vp, &r).await else {
                        return Ok(1);
                    };
                    let t = match op {
                        ArithmeticEqualTo => a == b,
                        ArithmeticNotEqualTo => a != b,
                        ArithmeticLessThan => a < b,
                        ArithmeticLessThanOrEqualTo => a <= b,
                        ArithmeticGreaterThan => a > b,
                        _ => a >= b,
                    };
                    Ok(i32::from(!t))
                }
                // `<` and `>` (the locale's collation is code point order in C.UTF-8,
                // which is byte order) and the file comparisons.
                _ => Ok(match builtins::test::binary(vp, &l, op_text, &r).await {
                    Ok(t) => i32::from(!t),
                    Err(_) => 2,
                }),
            }
        }
    }
}

async fn arith(vp: &mut Vp, text: &[u8]) -> Option<i64> {
    match crate::arith::eval(vp, text) {
        Ok(v) => Some(v),
        Err(msg) => {
            vp.error(&format!("[[: {msg}")).await;
            None
        }
    }
}

/// `lhs =~ re`: on a match, `BASH_REMATCH` holds the match and its groups (unset groups
/// as empty strings); on no match, it is emptied; an invalid expression is status 2.
async fn regex_match(vp: &mut Vp, text: &[u8], re: &[u8]) -> R {
    let opts = psh_regex::Options {
        extended: true,
        icase: vp.opts.shopt(Shopt::Nocasematch),
        newline: false,
        utf8: vp.utf8(),
    };
    let re_c = match psh_regex::Regex::new(re, opts) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(
                "[[: invalid regular expression `{}': {}",
                String::from_utf8_lossy(re),
                e.message()
            );
            vp.error(&msg).await;
            return Ok(2);
        }
    };
    let caps = re_c.exec(text, psh_regex::ExecOptions::default());
    let elems: std::collections::BTreeMap<i64, Vec<u8>> = caps
        .as_ref()
        .map(|c| {
            c.iter()
                .enumerate()
                .map(|(i, g)| {
                    let v = g.map(|(a, b)| text[a..b].to_vec()).unwrap_or_default();
                    (i as i64, v)
                })
                .collect()
        })
        .unwrap_or_default();
    set_rematch(vp, elems).await;
    Ok(i32::from(caps.is_none()))
}

async fn set_rematch(vp: &mut Vp, elems: std::collections::BTreeMap<i64, Vec<u8>>) {
    let name = "BASH_REMATCH";
    if let Some(v) = vp.var_ref(name) {
        if v.readonly {
            vp.error(&format!("{name}: readonly variable")).await;
            return;
        }
        if v.is_assoc() {
            vp.error(&format!("[[: {name}: not an indexed array")).await;
            return;
        }
    }
    vp.vars.entry(name).value = Value::Indexed(elems);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pattern,
    Regex,
}

/// The right side of `==` or `=~` expanded, with what is quoted escaped so that it
/// matches literally, and as `set -x` shows it.
struct Rhs {
    text: Vec<u8>,
    trace: Vec<u8>,
}

/// Expands the right side part by part, so that the quoted parts can be escaped for
/// the pattern or regex syntax (the engine's pattern words escape only glob chars).
async fn rhs_pieces(vp: &mut Vp, code: &Rc<Code>, w: &Word, mode: Mode) -> Result<Rhs, Unwind> {
    let mut out = Rhs {
        text: Vec::new(),
        trace: Vec::new(),
    };
    for (i, part) in w.parts.iter().enumerate() {
        let one = |p: &WordPart| Word {
            span: p.span,
            parts: vec![p.clone()],
        };
        match &part.kind {
            // Literal text as written; a leading `~` is a tilde expansion, whose result
            // is taken literally.
            Part::Lit => {
                let mut src = code.ast.bytes(part.span);
                if i == 0 && src.first() == Some(&b'~') {
                    // The tilde prefix, up to a slash: when it expands (`~`, `~user`)
                    // the result is literal; else (`~*`) it stays as written.
                    let len = src.iter().position(|&b| b == b'/').unwrap_or(src.len());
                    let prefix = WordPart {
                        span: Span::new(part.span.start, part.span.start + len as u32),
                        kind: Part::Lit,
                    };
                    let v = single(vp, code, &one(&prefix)).await?;
                    if v != src[..len] {
                        push_quoted(&mut out, &v, mode);
                        src = &src[len..];
                    }
                }
                out.text.extend_from_slice(src);
                out.trace.extend_from_slice(src);
            }
            Part::Escaped | Part::Single | Part::AnsiC | Part::Double(_) | Part::Locale(_) => {
                let v = single(vp, code, &one(part)).await?;
                push_quoted(&mut out, &v, mode);
            }
            // Expansions: their results are live pattern or regex text.
            _ => {
                let v = match mode {
                    Mode::Pattern => Box::pin(expand::expand_pattern(vp, code, &one(part))).await?,
                    Mode::Regex => single(vp, code, &one(part)).await?,
                };
                out.text.extend_from_slice(&v);
                out.trace.extend_from_slice(&v);
            }
        }
    }
    if out.trace.is_empty() {
        out.trace.extend_from_slice(b"''");
    }
    Ok(out)
}

fn push_quoted(out: &mut Rhs, v: &[u8], mode: Mode) {
    // The chars escaped to match literally, and those bash's trace line escapes (the
    // same for a regex; for a pattern bash's trace escapes a few more).
    let (special, shown): (&[u8], &[u8]) = match mode {
        Mode::Pattern => (b"*?[]\\@!+()|", b"!#%()*-.:=?[\\]^|"),
        Mode::Regex => (b"$()*+.?[\\^{|", b"$()*+.?[\\^{|"),
    };
    for &b in v {
        // In a pattern, any quoted punctuation is literal, also inside a bracket
        // expression (`[a\-z]`, `[\^]`).
        if special.contains(&b) || (mode == Mode::Pattern && b.is_ascii_punctuation()) {
            out.text.push(b'\\');
        }
        out.text.push(b);
        if shown.contains(&b) {
            out.trace.push(b'\\');
        }
        out.trace.push(b);
    }
}

/// An operand as the trace line shows it: as is, an empty one as `''`.
fn trace_word(v: &[u8]) -> Vec<u8> {
    if v.is_empty() {
        b"''".to_vec()
    } else {
        v.to_vec()
    }
}

/// `set -x`: each test as it is evaluated, `+ [[ a == b ]]`.
async fn trace(vp: &mut Vp, neg: bool, words: &[&[u8]]) {
    if !vp.opts.get(SetOpt::Xtrace) {
        return;
    }
    let mut line = b"[[ ".to_vec();
    if neg {
        line.extend_from_slice(b"! ");
    }
    for w in words {
        line.extend_from_slice(w);
        line.push(b' ');
    }
    line.extend_from_slice(b"]]");
    builtins::set::xtrace(vp, &line).await;
}

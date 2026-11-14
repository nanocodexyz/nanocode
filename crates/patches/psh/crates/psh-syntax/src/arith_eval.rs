//! Evaluation of arithmetic expressions over an environment of variables, as bash does
//! it: 64-bit wrapping math, short-circuit `&&` `||` `?:`, variable values evaluated as
//! arithmetic in turn (up to bash's recursion limit), and bash's runtime errors.
//!
//! The environment owns variable semantics (scalars, indexed and associative arrays,
//! negative indexes, `set -u`, readonly variables); the evaluator only asks it for values
//! and hands it new ones.

use alloc::string::String;
use alloc::vec::Vec;

use crate::arith::{
    ArithError, ArithErrorKind, ArithExpr, ArithKind, BinOp, LValue, UnaryOp, parse_arith,
};

/// Bash refuses to evaluate variable values nested this deep (`a=b b=c …`).
pub const ARITH_RECURSION_LIMIT: u32 = 1024;

/// A subscript, after the evaluator resolved it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithIndex<'a> {
    /// The subscript of an indexed array, evaluated (may be negative: from the end).
    Num(i64),
    /// The key of an associative array: the subscript text as written.
    Key(&'a [u8]),
}

/// A variable's value as the evaluator reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArithValue {
    /// Unset: counts as 0.
    Unset,
    /// A plain decimal number ([`plain_decimal`]).
    Num(i64),
    /// Any other text: evaluated as arithmetic in turn.
    Text(Vec<u8>),
}

/// What the evaluator needs from the shell.
pub trait ArithEnv {
    /// The shell's error type (unbound variable, readonly variable, …).
    type Error;

    /// Whether `name` is an associative array, whose subscripts are keys rather than
    /// arithmetic.
    fn is_assoc(&mut self, name: &[u8]) -> bool;

    /// The value of a variable (`index` `None`) or array element; `Ok(None)` when it is
    /// unset, which counts as 0. Under `set -u` return the "unbound variable" error.
    fn get(
        &mut self,
        name: &[u8],
        index: Option<ArithIndex<'_>>,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// What [`ArithEnv::get`] returns, read: an environment that can look at the value
    /// in place overrides this and copies only text that is not a plain number.
    fn load(
        &mut self,
        name: &[u8],
        index: Option<ArithIndex<'_>>,
    ) -> Result<ArithValue, Self::Error> {
        Ok(match self.get(name, index)? {
            None => ArithValue::Unset,
            Some(v) => match plain_decimal(&v) {
                Some(n) => ArithValue::Num(n),
                None => ArithValue::Text(v),
            },
        })
    }

    /// Assigns a value (the result of `=`, `+=`, `++`, …).
    fn set(
        &mut self,
        name: &[u8],
        index: Option<ArithIndex<'_>>,
        value: i64,
    ) -> Result<(), Self::Error>;
}

/// An evaluation failure: an arithmetic error in some text (the expression itself, a
/// variable's value or a subscript), or the environment's own error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError<E> {
    Arith { error: ArithError, text: Vec<u8> },
    Env(E),
}

impl<E> EvalError<E> {
    /// Bash's message for an arithmetic error (see [`ArithError::bash_text`]), without
    /// the caller's context prefix; `None` for environment errors.
    pub fn bash_text(&self) -> Option<String> {
        match self {
            EvalError::Arith { error, text } => Some(error.bash_text(text)),
            EvalError::Env(_) => None,
        }
    }
}

/// Parses and evaluates `text` (blank text is 0).
pub fn eval_arith<E: ArithEnv>(text: &[u8], env: &mut E) -> Result<i64, EvalError<E::Error>> {
    Evaluator {
        env,
        depth: 0,
        nest: 0,
    }
    .text(text)
}

/// Evaluates an expression parsed from `text` with [`parse_arith`].
pub fn eval_arith_expr<E: ArithEnv>(
    expr: &ArithExpr,
    text: &[u8],
    env: &mut E,
) -> Result<i64, EvalError<E::Error>> {
    Evaluator {
        env,
        depth: 0,
        nest: 0,
    }
    .expr(expr, text)
}

struct Evaluator<'e, E> {
    env: &'e mut E,
    /// Nesting of variable values being evaluated (bash's recursion level).
    depth: u32,
    /// Nesting of `expr` calls across all texts: values nested deep inside nested
    /// values would otherwise overflow the stack before bash's limit is reached.
    nest: u32,
}

/// A bound on the evaluator's own recursion (1024 levels of `x=x` take one each); fits
/// a 1 MiB stack in release builds (tested). Values nested deep inside nested values
/// reach it before bash's level limit and get the same error.
const MAX_NEST: u32 = 2048;

fn err<E>(kind: ArithErrorKind, token: u32, text: &[u8]) -> EvalError<E> {
    EvalError::Arith {
        error: ArithError::new(kind, token, None),
        text: text.to_vec(),
    }
}

impl<E: ArithEnv> Evaluator<'_, E> {
    fn text(&mut self, text: &[u8]) -> Result<i64, EvalError<E::Error>> {
        match parse_arith(text) {
            Ok(Some(e)) => self.expr(&e, text),
            Ok(None) => Ok(0),
            Err(error) => Err(EvalError::Arith {
                error,
                text: text.to_vec(),
            }),
        }
    }

    fn index<'t>(
        &mut self,
        lv: &LValue,
        text: &'t [u8],
    ) -> Result<Option<ArithIndex<'t>>, EvalError<E::Error>> {
        let Some(sub) = lv.index else {
            return Ok(None);
        };
        let sub = &text[sub.range()];
        if self.env.is_assoc(&text[lv.name.range()]) {
            Ok(Some(ArithIndex::Key(sub)))
        } else {
            Ok(Some(ArithIndex::Num(self.text(sub)?)))
        }
    }

    /// The value of a variable: its text, evaluated as arithmetic.
    fn load(&mut self, lv: &LValue, text: &[u8]) -> Result<i64, EvalError<E::Error>> {
        let index = self.index(lv, text)?;
        if self.depth + 1 >= ARITH_RECURSION_LIMIT {
            return Err(err(ArithErrorKind::RecursionLevel, lv.span.start, text));
        }
        let value = self
            .env
            .load(&text[lv.name.range()], index)
            .map_err(EvalError::Env)?;
        let value = match value {
            ArithValue::Unset => return Ok(0),
            // The common case: a plain decimal number needs no parse.
            ArithValue::Num(n) => return Ok(n),
            ArithValue::Text(value) => value,
        };
        self.depth += 1;
        let r = self.text(&value);
        self.depth -= 1;
        r
    }

    fn store(&mut self, lv: &LValue, text: &[u8], value: i64) -> Result<(), EvalError<E::Error>> {
        let index = self.index(lv, text)?;
        self.env
            .set(&text[lv.name.range()], index, value)
            .map_err(EvalError::Env)
    }

    fn expr(&mut self, e: &ArithExpr, text: &[u8]) -> Result<i64, EvalError<E::Error>> {
        if self.nest >= MAX_NEST {
            return Err(err(ArithErrorKind::RecursionLevel, e.span.start, text));
        }
        self.nest += 1;
        let r = self.expr_inner(e, text);
        self.nest -= 1;
        r
    }

    fn expr_inner(&mut self, e: &ArithExpr, text: &[u8]) -> Result<i64, EvalError<E::Error>> {
        Ok(match &e.kind {
            ArithKind::Num(n) => *n,
            ArithKind::Var(lv) => self.load(lv, text)?,
            ArithKind::Unary { op, expr } => {
                let v = self.expr(expr, text)?;
                match op {
                    UnaryOp::Plus => v,
                    UnaryOp::Minus => v.wrapping_neg(),
                    UnaryOp::Not => (v == 0) as i64,
                    UnaryOp::BitNot => !v,
                }
            }
            ArithKind::IncDec { target, inc, post } => {
                let old = self.load(target, text)?;
                let new = if *inc {
                    old.wrapping_add(1)
                } else {
                    old.wrapping_sub(1)
                };
                self.store(target, text, new)?;
                if *post { old } else { new }
            }
            ArithKind::Binary { op, lhs, rhs } => match op {
                BinOp::And => (self.expr(lhs, text)? != 0 && self.expr(rhs, text)? != 0) as i64,
                BinOp::Or => (self.expr(lhs, text)? != 0 || self.expr(rhs, text)? != 0) as i64,
                BinOp::Comma => {
                    self.expr(lhs, text)?;
                    self.expr(rhs, text)?
                }
                _ => {
                    let a = self.expr(lhs, text)?;
                    let b = self.expr(rhs, text)?;
                    binary(*op, a, b, rhs, text)?
                }
            },
            ArithKind::Cond {
                cond,
                then,
                otherwise,
            } => {
                if self.expr(cond, text)? != 0 {
                    self.expr(then, text)?
                } else {
                    self.expr(otherwise, text)?
                }
            }
            ArithKind::Assign { target, op, value } => {
                // `x op= v` reads x before evaluating v, as bash does (`x += x++`).
                let old = match op {
                    Some(_) => Some(self.load(target, text)?),
                    None => None,
                };
                let v = self.expr(value, text)?;
                let new = match (op, old) {
                    (Some(op), Some(old)) => binary(*op, old, v, value, text)?,
                    _ => v,
                };
                self.store(target, text, new)?;
                new
            }
        })
    }
}

/// A binary operation on values. `rhs` locates the error token of runtime errors, as
/// bash reports it: the start of the divisor for `/` and `%`; for `**`, the token after
/// the exponent (its last token at the end of the text).
fn binary<E>(op: BinOp, a: i64, b: i64, rhs: &ArithExpr, text: &[u8]) -> Result<i64, EvalError<E>> {
    Ok(match op {
        BinOp::Pow => {
            if b < 0 {
                let mut next = rhs.span.end as usize;
                while text
                    .get(next)
                    .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n'))
                {
                    next += 1;
                }
                let token = if next < text.len() {
                    next as u32
                } else {
                    rhs.last
                };
                return Err(err(ArithErrorKind::NegativeExponent, token, text));
            }
            let (mut base, mut exp, mut acc) = (a, b as u64, 1i64);
            while exp > 0 {
                if exp & 1 == 1 {
                    acc = acc.wrapping_mul(base);
                }
                base = base.wrapping_mul(base);
                exp >>= 1;
            }
            acc
        }
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::Div | BinOp::Rem => {
            if b == 0 {
                return Err(err(ArithErrorKind::DivisionByZero, rhs.span.start, text));
            }
            if op == BinOp::Div {
                a.wrapping_div(b)
            } else {
                a.wrapping_rem(b)
            }
        }
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        // Shift counts are taken modulo 64.
        BinOp::Shl => a.wrapping_shl(b as u32),
        BinOp::Shr => a.wrapping_shr(b as u32),
        BinOp::Lt => (a < b) as i64,
        BinOp::Le => (a <= b) as i64,
        BinOp::Gt => (a > b) as i64,
        BinOp::Ge => (a >= b) as i64,
        BinOp::Eq => (a == b) as i64,
        BinOp::Ne => (a != b) as i64,
        BinOp::BitAnd => a & b,
        BinOp::BitXor => a ^ b,
        BinOp::BitOr => a | b,
        BinOp::And => (a != 0 && b != 0) as i64,
        BinOp::Or => (a != 0 || b != 0) as i64,
        BinOp::Comma => b,
    })
}

/// `[-]digits` without a leading zero (which would be octal) and short enough not to
/// wrap: parse it directly.
pub fn plain_decimal(v: &[u8]) -> Option<i64> {
    let (neg, digits) = match v.first() {
        Some(b'-') => (true, &v[1..]),
        _ => (false, v),
    };
    if digits.is_empty() || digits.len() > 18 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    let mut n: i64 = 0;
    for &d in digits {
        n = n * 10 + (d - b'0') as i64;
    }
    Some(if neg { -n } else { n })
}

//! Complex math functions matching Python's cmath module behavior.
//!
//! These implementations follow the algorithms from cmathmodule.c
//! to ensure numerical precision and correct handling of edge cases.

mod exponential;
mod misc;
mod trigonometric;

pub use exponential::{exp, log, log10, sqrt};
pub use misc::{abs, isclose, isfinite, isinf, isnan, phase, polar, rect};
pub use trigonometric::{acos, acosh, asin, asinh, atan, atanh, cos, cosh, sin, sinh, tan, tanh};

use nanocode_numeric::Complex64;

// Public constants (matching Python's cmath module)

/// The mathematical constant e = 2.718281...
pub const E: f64 = std::f64::consts::E;

/// The mathematical constant π = 3.141592...
pub const PI: f64 = std::f64::consts::PI;

/// The mathematical constant τ = 6.283185...
pub const TAU: f64 = std::f64::consts::TAU;

/// Positive infinity.
pub const INF: f64 = f64::INFINITY;

/// A floating point "not a number" (NaN) value.
pub const NAN: f64 = f64::NAN;

/// Complex number with zero real part and positive infinity imaginary part.
pub const INFJ: Complex64 = Complex64::new(0.0, f64::INFINITY);

/// Complex number with zero real part and NaN imaginary part.
pub const NANJ: Complex64 = Complex64::new(0.0, f64::NAN);

use crate::m;

// Shared constants

const M_LN2: f64 = core::f64::consts::LN_2;

/// Used to avoid spurious overflow in sqrt, log, inverse trig/hyperbolic functions.
const CM_LARGE_DOUBLE: f64 = f64::MAX / 4.0;
const CM_LOG_LARGE_DOUBLE: f64 = 709.0895657128241; // log(CM_LARGE_DOUBLE)

// Special value table constants
const P: f64 = core::f64::consts::PI;
const P14: f64 = 0.25 * core::f64::consts::PI;
const P12: f64 = 0.5 * core::f64::consts::PI;
const P34: f64 = 0.75 * core::f64::consts::PI;
const N: f64 = f64::NAN;
#[allow(clippy::excessive_precision)]
const U: f64 = -9.5426319407711027e33; // unlikely value, used as placeholder

/// Helper to create Complex64 in const context (for special value tables)
#[inline]
const fn c(re: f64, im: f64) -> nanocode_numeric::Complex64 {
    nanocode_numeric::Complex64::new(re, im)
}

/// Special value types for classifying doubles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum SpecialType {
    NInf = 0,  // negative infinity
    Neg = 1,   // negative finite (nonzero)
    NZero = 2, // -0.
    PZero = 3, // +0.
    Pos = 4,   // positive finite (nonzero)
    PInf = 5,  // positive infinity
    Nan = 6,   // NaN
}

/// Return special value from table if input is non-finite.
macro_rules! special_value {
    ($z:expr, $table:expr) => {
        if !$z.re.is_finite() || !$z.im.is_finite() {
            return Ok($table[special_type($z.re) as usize][special_type($z.im) as usize]);
        }
    };
}
pub(crate) use special_value;

/// Classify a double into one of seven special types.
#[inline]
fn special_type(d: f64) -> SpecialType {
    if d.is_finite() {
        if d != 0.0 {
            if m::copysign(1.0, d) == 1.0 {
                SpecialType::Pos
            } else {
                SpecialType::Neg
            }
        } else if m::copysign(1.0, d) == 1.0 {
            SpecialType::PZero
        } else {
            SpecialType::NZero
        }
    } else if d.is_nan() {
        SpecialType::Nan
    } else if m::copysign(1.0, d) == 1.0 {
        SpecialType::PInf
    } else {
        SpecialType::NInf
    }
}

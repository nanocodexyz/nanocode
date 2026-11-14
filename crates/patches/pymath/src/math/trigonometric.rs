//! Trigonometric and hyperbolic functions.

use crate::Result;

// Trigonometric functions

/// Return the sine of x (in radians).
#[inline]
pub fn sin(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::sin, false)
}

/// Return the cosine of x (in radians).
#[inline]
pub fn cos(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::cos, false)
}

/// Return the tangent of x (in radians).
#[inline]
pub fn tan(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::tan, false)
}

/// Return the arc sine of x, in radians.
/// Result is in the range [-pi/2, pi/2].
#[inline]
pub fn asin(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::asin, false)
}

/// Return the arc cosine of x, in radians.
/// Result is in the range [0, pi].
#[inline]
pub fn acos(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::acos, false)
}

/// Return the arc tangent of x, in radians.
/// Result is in the range [-pi/2, pi/2].
#[inline]
pub fn atan(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::atan, false)
}

/// Return the arc tangent of y/x, in radians.
/// Result is in the range [-pi, pi].
#[inline]
pub fn atan2(y: f64, x: f64) -> Result<f64> {
    super::math_2(y, x, crate::m::atan2)
}

// Hyperbolic functions

/// Hyperbolic sine.
#[inline]
pub fn sinh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::sinh, true)
}

/// Hyperbolic cosine.
#[inline]
pub fn cosh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::cosh, true)
}

/// Hyperbolic tangent.
#[inline]
pub fn tanh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::tanh, false)
}

/// Inverse hyperbolic sine.
#[inline]
pub fn asinh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::asinh, false)
}

/// Inverse hyperbolic cosine.
#[inline]
pub fn acosh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::acosh, false)
}

/// Inverse hyperbolic tangent.
#[inline]
pub fn atanh(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::atanh, false)
}

//! Floating-point manipulation and validation functions.

use crate::{Error, Result, m};

super::libm_simple!(@1 ceil, floor, trunc);

/// Return the next floating-point value after x towards y.
///
/// If steps is provided, move that many steps towards y using O(1) bit
/// manipulation on the IEEE 754 representation. Steps that overshoot y
/// are clamped so the result never passes y.
///
/// CPython's math_nextafter_impl accepts a Python integer for steps,
/// rejects negative values, and saturates overflows to UINT64_MAX. This
/// Rust API takes `Option<u64>`, so negative rejection and big-int
/// saturation are structurally unnecessary. The caller (e.g. RustPython)
/// should handle Python int conversion and negative checks before calling.
///
/// See math_nextafter_impl in mathmodule.c.
#[inline]
pub fn nextafter(x: f64, y: f64, steps: Option<u64>) -> f64 {
    let usteps = match steps {
        None => return crate::m::nextafter(x, y),
        Some(n) => n,
    };

    if usteps == 0 || x.is_nan() {
        return x;
    }
    if y.is_nan() {
        return y;
    }

    let mut ux = x.to_bits();
    let uy = y.to_bits();
    if ux == uy {
        return x;
    }

    const SIGN_BIT: u64 = 1u64 << 63;
    let ax = ux & !SIGN_BIT;
    let ay = uy & !SIGN_BIT;

    if (ux ^ uy) & SIGN_BIT != 0 {
        // opposite signs — may need to cross zero
        // ax + ay can never overflow because bit 63 is cleared in both
        if ax + ay <= usteps {
            y
        } else if ax < usteps {
            // cross zero: remaining steps land on y's side
            f64::from_bits((uy & SIGN_BIT) | (usteps - ax))
        } else {
            ux -= usteps;
            f64::from_bits(ux)
        }
    } else if ax > ay {
        // same sign, moving toward zero
        if ax - ay >= usteps {
            ux -= usteps;
            f64::from_bits(ux)
        } else {
            y
        }
    } else {
        // same sign, moving away from zero
        if ay - ax >= usteps {
            ux += usteps;
            f64::from_bits(ux)
        } else {
            y
        }
    }
}

/// Return the absolute value of x.
#[inline]
pub fn fabs(x: f64) -> Result<f64> {
    super::math_1(x, crate::m::fabs, false)
}

/// Return a float with the magnitude of x but the sign of y.
#[inline]
pub fn copysign(x: f64, y: f64) -> crate::Result<f64> {
    super::math_2(x, y, crate::m::copysign)
}

// Validation functions

/// Return True if x is neither an infinity nor a NaN, False otherwise.
#[inline]
pub fn isfinite(x: f64) -> bool {
    x.is_finite()
}

/// Return True if x is a positive or negative infinity, False otherwise.
#[inline]
pub fn isinf(x: f64) -> bool {
    x.is_infinite()
}

/// Return True if x is a NaN, False otherwise.
#[inline]
pub fn isnan(x: f64) -> bool {
    x.is_nan()
}

/// Return True if a and b are close to each other.
///
/// Whether or not two values are considered close is determined according to
/// given absolute and relative tolerances:
/// `abs(a-b) <= max(rel_tol * max(abs(a), abs(b)), abs_tol)`
///
/// Default tolerances: rel_tol = 1e-09, abs_tol = 0.0
/// Returns Err(EDOM) if rel_tol or abs_tol is negative.
#[inline]
pub fn isclose(a: f64, b: f64, rel_tol: Option<f64>, abs_tol: Option<f64>) -> Result<bool> {
    let rel_tol = rel_tol.unwrap_or(1e-09);
    let abs_tol = abs_tol.unwrap_or(0.0);
    // Tolerances must be non-negative
    if rel_tol < 0.0 || abs_tol < 0.0 {
        return Err(Error::EDOM);
    }
    if a == b {
        return Ok(true);
    }
    if a.is_nan() || b.is_nan() {
        return Ok(false);
    }
    if a.is_infinite() || b.is_infinite() {
        return Ok(false);
    }
    let diff = (a - b).abs();
    Ok(diff <= abs_tol.max(rel_tol * a.abs().max(b.abs())))
}

/// Fused multiply-add operation: (x * y) + z.
///
/// Returns EDOM for invalid operation (NaN result from non-NaN inputs).
/// Returns ERANGE for overflow (infinite result from finite inputs).
#[inline]
pub fn fma(x: f64, y: f64, z: f64) -> Result<f64> {
    let r = x.mul_add(y, z);

    // Fast path: if we got a finite result, we're done.
    if r.is_finite() {
        return Ok(r);
    }

    // Non-finite result. Raise an exception if appropriate, else return r.
    if r.is_nan() {
        if !x.is_nan() && !y.is_nan() && !z.is_nan() {
            // NaN result from non-NaN inputs.
            return Err(Error::EDOM);
        }
    } else if x.is_finite() && y.is_finite() && z.is_finite() {
        // Infinite result from finite inputs.
        return Err(Error::ERANGE);
    }

    Ok(r)
}

/// Return the mantissa and exponent of x as (m, e).
///
/// m is a float and e is an integer such that x == m * 2**e exactly.
#[inline]
pub fn frexp(x: f64) -> (f64, i32) {
    // Handle special cases directly, to sidestep platform differences
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return (x, 0);
    }
    let mut exp: i32 = 0;
    let mantissa = m::frexp(x, &mut exp);
    (mantissa, exp)
}

/// Return x * (2**i).
///
/// Returns ERANGE if the result overflows.
#[inline]
pub fn ldexp(x: f64, i: i32) -> Result<f64> {
    // NaNs, zeros and infinities are returned unchanged
    if x == 0.0 || !x.is_finite() {
        return Ok(x);
    }
    let r = m::ldexp(x, i);
    if r.is_infinite() {
        return Err(Error::ERANGE);
    }
    Ok(r)
}

/// Return the fractional and integer parts of x.
///
/// Returns (fractional_part, integer_part).
#[inline]
pub fn modf(x: f64) -> (f64, f64) {
    // Some platforms don't do the right thing for NaNs and infinities,
    // so we take care of special cases directly.
    if x.is_infinite() {
        return (m::copysign(0.0, x), x);
    }
    if x.is_nan() {
        return (x, x);
    }
    let mut int_part: f64 = 0.0;
    let frac_part = m::modf(x, &mut int_part);
    (frac_part, int_part)
}

/// Return the remainder of x / y.
///
/// Returns EDOM if y is zero or x is infinite.
#[inline]
pub fn fmod(x: f64, y: f64) -> Result<f64> {
    // fmod(x, +/-Inf) returns x for finite x.
    if y.is_infinite() && x.is_finite() {
        return Ok(x);
    }
    let r = m::fmod(x, y);
    if r.is_nan() && !x.is_nan() && !y.is_nan() {
        return Err(Error::EDOM);
    }
    Ok(r)
}

/// Return the IEEE 754-style remainder of x with respect to y.
///
/// CPython implements this from scratch using fmod (m_remainder in
/// mathmodule.c) rather than calling the C library's remainder().
/// We delegate to libm's remainder() which is correct on all platforms
/// where it conforms to IEEE 754. If you find a platform where the
/// results differ from CPython, please file a bug.
#[inline]
pub fn remainder(x: f64, y: f64) -> Result<f64> {
    super::math_2(x, y, crate::m::remainder)
}

/// Return the value of the least significant bit of x.
#[inline]
pub fn ulp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let x = x.abs();
    if x.is_infinite() {
        return x;
    }
    let x2 = crate::m::nextafter(x, f64::INFINITY);
    if x2.is_infinite() {
        // Special case: x is the largest positive representable float
        let x2 = crate::m::nextafter(x, f64::NEG_INFINITY);
        return x - x2;
    }
    x2 - x
}

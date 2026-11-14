//! BigInt helper functions for RustPython.
//!
//! These functions handle cases where Python integers exceed i64 range.
//! They are not part of Python's math.integer module but are needed
//! for RustPython's internal implementation.

use super::integer::perm_comb_small;
#[cfg(feature = "bigint")]
pub(crate) use nanocode_numeric::{BigInt, BigUint};
use nanocode_numeric::{One, Signed, ToPrimitive};

// Perm/Comb for BigInt n

/// Compute perm(n, k) where n is a BigInt and k fits in u64.
/// Uses divide-and-conquer: P(n, k) = P(n, j) * P(n-j, k-j)
///
/// See: perm_comb in CPython mathmodule.c
pub fn perm_bigint(n: &BigInt, k: u64) -> BigUint {
    if k == 0 {
        return BigUint::one();
    }
    if k == 1 {
        return n.magnitude().clone();
    }

    // P(n, k) = P(n, j) * P(n-j, k-j)
    let j = k / 2;
    let a = perm_bigint(n, j);
    let n_minus_j = n - BigInt::from(j);
    let b = perm_bigint(&n_minus_j, k - j);
    a * b
}

/// Compute comb(n, k) where n is a BigInt and k fits in u64.
/// Uses divide-and-conquer: C(n, k) = C(n, j) * C(n-j, k-j) / C(k, j)
///
/// See: perm_comb in CPython mathmodule.c
pub fn comb_bigint(n: &BigInt, k: u64) -> BigUint {
    if k == 0 {
        return BigUint::one();
    }
    if k == 1 {
        return n.magnitude().clone();
    }

    // C(n, k) = C(n, j) * C(n-j, k-j) / C(k, j)
    let j = k / 2;
    let a = comb_bigint(n, j);
    let n_minus_j = n - BigInt::from(j);
    let b = comb_bigint(&n_minus_j, k - j);
    let numerator = a * b;
    // C(k, j) using small version since k fits in u64
    let divisor = perm_comb_small(k, j, true);
    numerator / divisor
}

// Logarithm functions for BigInt

/// Compute frexp-like decomposition for BigInt.
/// Returns (mantissa, exponent) where:
/// - mantissa is in [0.5, 1.0) for positive n
/// - n ~= mantissa * 2^exponent
///
/// `_PyLong_Frexp` extracts digits one-by-one into a fixed-size
/// accumulator and applies a `half_even_correction` lookup table for
/// rounding.  We instead extract the top 55 bits via a single right
/// shift and use a sticky-bit to mark whether any discarded bits were
/// non-zero, then delegate to `BigInt::to_f64()` which performs
/// IEEE 754 round-half-to-even.  The two approaches are equivalent
/// because the sticky bit preserves the same rounding information
/// that the digit-by-digit extraction would.
fn frexp_bigint(n: &BigInt) -> (f64, i64) {
    let bits = n.bits();
    if bits == 0 {
        return (0.0, 0);
    }

    let bits = bits as i64;

    // For small integers that fit in f64 mantissa (53 bits)
    if bits <= 53 {
        let x = n.to_f64().unwrap();
        // frexp returns (m, e) where x = m * 2^e and 0.5 <= |m| < 1
        let mut e: i32 = 0;
        let m = crate::m::frexp(x, &mut e);
        return (m, e as i64);
    }

    // For large integers, extract top DBL_MANT_DIG + 2 = 55 bits for rounding
    let shift = bits - 55;
    let mut mantissa_int = n >> shift as u64;

    // Sticky bit: if any shifted-out bits were non-zero, set the LSB.
    // This ensures correct IEEE round-half-to-even when converting to f64.
    //
    // `_PyLong_Frexp` checks the remainder from `v_rshift` first, then
    // iterates shifted-out digits top-down.  We use `trailing_zeros()`
    // which scans digits bottom-up instead.  The worst-case traversal
    // order differs (e.g. exact powers of two), but for typical inputs
    // both terminate in O(1).  If you observe a performance regression
    // from this, please file a bug report.
    let tz = n.magnitude().trailing_zeros().unwrap(); // n != 0 here
    if tz < shift as u64 {
        mantissa_int |= BigInt::from(1);
    }

    let mut x = mantissa_int.to_f64().unwrap();

    // x is now approximately n / 2^shift, with ~55 bits of precision
    // Scale to [0.5, 1.0) range
    // x is in [2^54, 2^55), so divide by 2^55 to get [0.5, 1.0)
    x /= (1u64 << 55) as f64;

    // Adjust if rounding pushed us to 1.0
    if x == 1.0 {
        x = 0.5;
        return (x, bits + 1);
    }

    (x, bits)
}

/// Return the natural logarithm of a BigInt.
///
/// Returns Err(EDOM) if n is not positive.
pub fn log_bigint(n: &BigInt, base: Option<f64>) -> crate::Result<f64> {
    if !n.is_positive() {
        return Err(crate::Error::EDOM);
    }

    // Try direct conversion first
    if let Some(x) = n.to_f64()
        && x.is_finite()
    {
        return super::log(x, base);
    }

    // Use frexp decomposition for large values
    // n ~= x * 2^e, so log(n) = log(x) + log(2) * e
    let (x, e) = frexp_bigint(n);
    let log_n = crate::mul_add(crate::m::log(2.0), e as f64, crate::m::log(x));

    match base {
        None => Ok(log_n),
        Some(b) => {
            if b <= 0.0 || b == 1.0 {
                return Err(crate::Error::EDOM);
            }
            Ok(log_n / crate::m::log(b))
        }
    }
}

/// Return the base-2 logarithm of a BigInt.
///
/// Returns Err(EDOM) if n is not positive.
pub fn log2_bigint(n: &BigInt) -> crate::Result<f64> {
    if !n.is_positive() {
        return Err(crate::Error::EDOM);
    }

    // Try direct conversion first
    if let Some(x) = n.to_f64()
        && x.is_finite()
    {
        return super::log2(x);
    }

    // Use frexp decomposition for large values
    // n ~= x * 2^e, so log2(n) = log2(x) + e
    let (x, e) = frexp_bigint(n);
    Ok(crate::mul_add(
        crate::m::log2(2.0),
        e as f64,
        crate::m::log2(x),
    ))
}

/// Return the base-10 logarithm of a BigInt.
///
/// Returns Err(EDOM) if n is not positive.
pub fn log10_bigint(n: &BigInt) -> crate::Result<f64> {
    if !n.is_positive() {
        return Err(crate::Error::EDOM);
    }

    // Try direct conversion first
    if let Some(x) = n.to_f64()
        && x.is_finite()
    {
        return super::log10(x);
    }

    // Use frexp decomposition for large values
    // n ~= x * 2^e, so log10(n) = log10(x) + log10(2) * e
    let (x, e) = frexp_bigint(n);
    Ok(crate::mul_add(
        crate::m::log10(2.0),
        e as f64,
        crate::m::log10(x),
    ))
}

/// Compute ldexp(x, exp) where exp is a BigInt.
///
/// Returns x * 2^exp, handling BigInt exponent overflow:
/// - If x is 0, inf, or nan, returns x unchanged
/// - If exp overflows i32 positively, returns ERANGE (overflow)
/// - If exp overflows i32 negatively, returns signed zero (underflow)
pub fn ldexp_bigint(x: f64, exp: &BigInt) -> crate::Result<f64> {
    // Special values are returned unchanged regardless of exponent
    if x == 0.0 || !x.is_finite() {
        return Ok(x);
    }

    // Fast path: try i64 first (like CPython's PyLong_AsLongAndOverflow)
    let exp_clamped: i64 = match exp.try_into() {
        Ok(e) => e,
        Err(_) => {
            // Exponent overflows i64, clamp to i64::MIN/MAX
            if exp.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        }
    };

    // Check against i32 bounds
    if exp_clamped > i32::MAX as i64 {
        // overflow
        Err(crate::Error::ERANGE)
    } else if exp_clamped < i32::MIN as i64 {
        // underflow to signed zero
        Ok(if x.is_sign_negative() { -0.0 } else { 0.0 })
    } else {
        super::ldexp(x, exp_clamped as i32)
    }
}

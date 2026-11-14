//! Complex polar coordinate and utility functions.

use super::{INF, N, U, c, special_type};
use crate::{Error, Result, m};
use nanocode_numeric::Complex64;

#[rustfmt::skip]
static RECT_SPECIAL_VALUES: [[Complex64; 7]; 7] = [
    [c(INF, N),   c(U, U), c(-INF, 0.0), c(-INF, -0.0), c(U, U), c(INF, N),   c(INF, N)],
    [c(N, N),     c(U, U), c(U, U),      c(U, U),       c(U, U), c(N, N),     c(N, N)],
    [c(0.0, 0.0), c(U, U), c(-0.0, 0.0), c(-0.0, -0.0), c(U, U), c(0.0, 0.0), c(0.0, 0.0)],
    [c(0.0, 0.0), c(U, U), c(0.0, -0.0), c(0.0, 0.0),   c(U, U), c(0.0, 0.0), c(0.0, 0.0)],
    [c(N, N),     c(U, U), c(U, U),      c(U, U),       c(U, U), c(N, N),     c(N, N)],
    [c(INF, N),   c(U, U), c(INF, -0.0), c(INF, 0.0),   c(U, U), c(INF, N),   c(INF, N)],
    [c(N, N),     c(N, N), c(N, 0.0),    c(N, 0.0),     c(N, N), c(N, N),     c(N, N)],
];

/// Return the phase angle (argument) of z.
#[inline]
pub fn phase(z: Complex64) -> Result<f64> {
    crate::err::set_errno(0);
    let phi = m::atan2(z.im, z.re);
    match crate::err::get_errno() {
        0 => Ok(phi),
        e if e == Error::EDOM as i32 => Err(Error::EDOM),
        e if e == Error::ERANGE as i32 => Err(Error::ERANGE),
        _ => Err(Error::EDOM), // Unknown errno treated as domain error (like PyErr_SetFromErrno)
    }
}

#[inline]
fn c_abs_raw(z: Complex64) -> f64 {
    if !z.re.is_finite() || !z.im.is_finite() {
        // C99 rules: if either part is infinite, return infinity,
        // even if the other part is NaN.
        if z.re.is_infinite() {
            return m::fabs(z.re);
        }
        if z.im.is_infinite() {
            return m::fabs(z.im);
        }
        return f64::NAN;
    }
    m::hypot(z.re, z.im)
}

#[inline]
fn c_abs_checked(z: Complex64) -> Result<f64> {
    if !z.re.is_finite() || !z.im.is_finite() {
        return Ok(c_abs_raw(z));
    }
    crate::err::set_errno(0);
    let r = m::hypot(z.re, z.im);
    if r.is_infinite() {
        Err(Error::ERANGE)
    } else {
        Ok(r)
    }
}

/// Convert z to polar coordinates (r, phi).
#[inline]
pub fn polar(z: Complex64) -> Result<(f64, f64)> {
    let phi = m::atan2(z.im, z.re);
    let r = c_abs_checked(z)?;
    Ok((r, phi))
}

/// Convert polar coordinates (r, phi) to rectangular form.
#[inline]
pub fn rect(r: f64, phi: f64) -> Result<Complex64> {
    // Handle special values
    if !r.is_finite() || !phi.is_finite() {
        // if r is +/-infinity and phi is finite but nonzero then
        // result is (+-INF +-INF i), but we need to compute cos(phi)
        // and sin(phi) to figure out the signs.
        let z = if r.is_infinite() && phi.is_finite() && phi != 0.0 {
            if r > 0.0 {
                Complex64::new(m::copysign(INF, m::cos(phi)), m::copysign(INF, m::sin(phi)))
            } else {
                Complex64::new(
                    -m::copysign(INF, m::cos(phi)),
                    -m::copysign(INF, m::sin(phi)),
                )
            }
        } else {
            RECT_SPECIAL_VALUES[special_type(r) as usize][special_type(phi) as usize]
        };
        // need to set errno = EDOM if r is a nonzero number and phi is infinite
        if r != 0.0 && !r.is_nan() && phi.is_infinite() {
            return Err(Error::EDOM);
        }
        return Ok(z);
    } else if phi == 0.0 {
        // Workaround for buggy results with phi=-0.0 on OS X 10.8.
        return Ok(Complex64::new(r, r * phi));
    }

    let (sin_phi, cos_phi) = m::sincos(phi);
    Ok(Complex64::new(r * cos_phi, r * sin_phi))
}

/// Return True if both real and imaginary parts are finite.
#[inline]
pub fn isfinite(z: Complex64) -> bool {
    z.re.is_finite() && z.im.is_finite()
}

/// Return True if either real or imaginary part is NaN.
#[inline]
pub fn isnan(z: Complex64) -> bool {
    z.re.is_nan() || z.im.is_nan()
}

/// Return True if either real or imaginary part is infinite.
#[inline]
pub fn isinf(z: Complex64) -> bool {
    z.re.is_infinite() || z.im.is_infinite()
}

/// Complex absolute value (magnitude).
#[inline]
pub fn abs(z: Complex64) -> f64 {
    c_abs_raw(z)
}

/// Determine whether two complex numbers are close in value.
///
/// Default tolerances: rel_tol = 1e-09, abs_tol = 0.0
/// Returns Err(EDOM) if rel_tol or abs_tol is negative.
#[inline]
pub fn isclose(
    a: Complex64,
    b: Complex64,
    rel_tol: Option<f64>,
    abs_tol: Option<f64>,
) -> Result<bool> {
    let rel_tol = rel_tol.unwrap_or(1e-09);
    let abs_tol = abs_tol.unwrap_or(0.0);

    // Tolerances must be non-negative
    if rel_tol < 0.0 || abs_tol < 0.0 {
        return Err(Error::EDOM);
    }

    // short circuit exact equality
    if a.re == b.re && a.im == b.im {
        return Ok(true);
    }

    // This catches the case of two infinities of opposite sign, or
    // one infinity and one finite number.
    if a.re.is_infinite() || a.im.is_infinite() || b.re.is_infinite() || b.im.is_infinite() {
        return Ok(false);
    }

    // now do the regular computation
    let diff = abs(Complex64::new(a.re - b.re, a.im - b.im));
    Ok((diff <= rel_tol * abs(b)) || (diff <= rel_tol * abs(a)) || (diff <= abs_tol))
}

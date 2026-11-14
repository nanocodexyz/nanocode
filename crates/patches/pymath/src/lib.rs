// Public modules
#[cfg(feature = "complex")]
pub mod cmath;
pub mod math;

// Internal modules
mod err;
// Native libm via FFI (unix/windows)
#[cfg(any(unix, windows))]
pub(crate) mod m;
// Pure Rust libm for WASM and other targets
#[cfg(not(any(unix, windows)))]
#[path = "m_rust.rs"]
pub(crate) mod m;
#[cfg(any(unix, windows))]
mod m_sys;
#[cfg(not(any(unix, windows)))]
mod m_libm;

// Re-export error types at root level
pub use err::{Error, Result};

/// Fused multiply-add operation.
/// When `mul_add` feature is enabled, uses hardware FMA instruction.
/// Otherwise, uses separate multiply and add operations.
#[inline(always)]
pub(crate) fn mul_add(a: f64, b: f64, c: f64) -> f64 {
    if cfg!(feature = "mul_add") {
        a.mul_add(b, c)
    } else {
        a * b + c
    }
}

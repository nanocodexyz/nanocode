#![allow(
    clippy::assign_op_pattern,
    clippy::eq_op,
    clippy::excessive_precision,
    clippy::float_cmp,
    clippy::int_plus_one,
    clippy::just_underscores_and_digits,
    clippy::many_single_char_names,
    clippy::mixed_case_hex_literals,
    clippy::needless_late_init,
    clippy::needless_return,
    clippy::unreadable_literal,
    clippy::zero_divided_by_zero
)]

mod copysign;
mod erf;
mod exp2;
mod frexp;
mod modf;
mod nextafter;
mod pow;
mod remainder;
mod remquo;

pub use copysign::copysign;
pub use erf::{erf, erfc};
pub use exp2::exp2;
pub use frexp::frexp;
pub use modf::modf;
pub use nextafter::nextafter;
pub use pow::pow;
pub use remainder::remainder;

use djs_libm::{exp, fabs, scalbn, sqrt};
use remquo::remquo;

const SIGN_MASK: u64 = 1 << (u64::BITS - 1);

#[inline]
fn get_high_word(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

#[inline]
fn with_set_high_word(f: f64, hi: u32) -> f64 {
    let mut tmp = f.to_bits();
    tmp &= 0x00000000_ffffffff;
    tmp |= (hi as u64) << 32;
    f64::from_bits(tmp)
}

#[inline]
fn with_set_low_word(f: f64, lo: u32) -> f64 {
    let mut tmp = f.to_bits();
    tmp &= 0xffffffff_00000000;
    tmp |= lo as u64;
    f64::from_bits(tmp)
}

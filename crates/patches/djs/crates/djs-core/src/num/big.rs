//! A small fixed-capacity unsigned big integer.
//!
//! Only what exact number conversion needs: multiply and divide by small factors (or powers
//! of a small radix), shifts with a sticky bit, and bit access. Every caller bounds its
//! operands, so the capacity is a static property of the algorithms (see `LIMBS`). Overflowing
//! it is a bug and panics through the slice bounds check; nothing here allocates.

/// Capacity in 32-bit limbs (3200 bits). The largest user is the parsing slow path: up to
/// `MAX_SIG_DIGITS` (800) decimal digits (2658 bits), or a quotient pre-shifted to keep 56 bits
/// after dividing by 5^1124 (2666 bits). Radix formatting and the power table generator stay
/// well below that.
pub(crate) const LIMBS: usize = 100;

#[derive(Clone)]
pub(crate) struct Big {
    /// Number of used limbs. Invariant: `limbs[len - 1] != 0`, so zero has `len == 0`.
    len: usize,
    /// Little-endian limbs; entries at `len..` are zero.
    limbs: [u32; LIMBS],
}

impl Big {
    pub(crate) fn zero() -> Big {
        Big {
            len: 0,
            limbs: [0; LIMBS],
        }
    }

    pub(crate) fn from_u64(v: u64) -> Big {
        let mut b = Big::zero();
        b.limbs[0] = v as u32;
        b.limbs[1] = (v >> 32) as u32;
        b.len = 2;
        b.trim();
        b
    }

    fn trim(&mut self) {
        while self.len > 0 && self.limbs[self.len - 1] == 0 {
            self.len -= 1;
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.len == 0
    }

    /// Number of significant bits (0 for zero).
    pub(crate) fn bit_len(&self) -> u32 {
        if self.len == 0 {
            0
        } else {
            self.len as u32 * 32 - self.limbs[self.len - 1].leading_zeros()
        }
    }

    pub(crate) fn bit(&self, i: u32) -> bool {
        let l = (i / 32) as usize;
        l < self.len && (self.limbs[l] >> (i % 32)) & 1 != 0
    }

    /// True if any of the bits below bit `n` are set.
    pub(crate) fn any_below(&self, n: u32) -> bool {
        let full = ((n / 32) as usize).min(self.len);
        if self.limbs[..full].iter().any(|&l| l != 0) {
            return true;
        }
        let rem = n % 32;
        full < self.len && rem != 0 && self.limbs[full] & ((1u32 << rem) - 1) != 0
    }

    /// The low 64 bits.
    pub(crate) fn low_u64(&self) -> u64 {
        self.limbs[0] as u64 | (self.limbs[1] as u64) << 32
    }

    /// True if the value fits in a `u64`.
    pub(crate) fn fits_u64(&self) -> bool {
        self.len <= 2
    }

    /// `self = self * m + a`.
    pub(crate) fn mul_add_small(&mut self, m: u32, a: u32) {
        let mut carry = a as u64;
        for l in &mut self.limbs[..self.len] {
            let t = *l as u64 * m as u64 + carry;
            *l = t as u32;
            carry = t >> 32;
        }
        if carry != 0 {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
        // m == 0 or a non-zero `a` on an empty number.
        if self.len == 0 && a != 0 {
            self.limbs[0] = a;
            self.len = 1;
        }
        self.trim();
    }

    pub(crate) fn add_small(&mut self, a: u32) {
        self.mul_add_small(1, a);
    }

    /// `self *= r^n`, multiplying by the largest power of `r` that fits a limb at a time.
    pub(crate) fn mul_pow(&mut self, r: u32, mut n: u32) {
        if r == 1 || self.is_zero() {
            return;
        }
        let (chunk, chunk_n) = limb_power(r);
        while n >= chunk_n {
            self.mul_add_small(chunk, 0);
            n -= chunk_n;
        }
        if n > 0 {
            self.mul_add_small(r.pow(n), 0);
        }
    }

    /// `self = floor(self / d)`, returning the remainder. `d != 0`.
    pub(crate) fn div_small(&mut self, d: u32) -> u32 {
        let mut rem = 0u64;
        for l in self.limbs[..self.len].iter_mut().rev() {
            let cur = rem << 32 | *l as u64;
            *l = (cur / d as u64) as u32;
            rem = cur % d as u64;
        }
        self.trim();
        rem as u32
    }

    /// `self = floor(self / r^n)`, returning true if the division was inexact.
    ///
    /// Dividing step by step is exact: floor(floor(a / b) / c) == floor(a / (b * c)), and the
    /// total remainder is zero only if every partial remainder is.
    pub(crate) fn div_pow(&mut self, r: u32, mut n: u32) -> bool {
        if r == 1 {
            return false;
        }
        let (chunk, chunk_n) = limb_power(r);
        let mut inexact = false;
        while n >= chunk_n {
            inexact |= self.div_small(chunk) != 0;
            n -= chunk_n;
        }
        if n > 0 {
            inexact |= self.div_small(r.pow(n)) != 0;
        }
        inexact
    }

    pub(crate) fn shl(&mut self, n: u32) {
        if self.is_zero() || n == 0 {
            return;
        }
        let limbs = (n / 32) as usize;
        let bits = n % 32;
        let old = self.len;
        if bits == 0 {
            self.limbs.copy_within(0..old, limbs);
        } else {
            self.limbs[old + limbs] = 0;
            for i in (0..old).rev() {
                let v = self.limbs[i];
                self.limbs[i + limbs + 1] |= v >> (32 - bits);
                self.limbs[i + limbs] = v << bits;
            }
        }
        self.limbs[..limbs].fill(0);
        self.len = old + limbs + 1;
        self.trim();
    }

    /// `self >>= n`, returning true if any 1 bit was shifted out.
    pub(crate) fn shr_sticky(&mut self, n: u32) -> bool {
        if n == 0 {
            return false;
        }
        let sticky = self.any_below(n);
        let limbs = (n / 32) as usize;
        if limbs >= self.len {
            *self = Big::zero();
            return sticky;
        }
        let bits = n % 32;
        let new_len = self.len - limbs;
        for i in 0..new_len {
            let lo = self.limbs[i + limbs];
            let hi = if i + limbs + 1 < self.len {
                self.limbs[i + limbs + 1]
            } else {
                0
            };
            self.limbs[i] = if bits == 0 {
                lo
            } else {
                lo >> bits | hi << (32 - bits)
            };
        }
        self.limbs[new_len..self.len].fill(0);
        self.len = new_len;
        self.trim();
        sticky
    }

}

impl PartialEq for Big {
    fn eq(&self, other: &Big) -> bool {
        self.limbs[..self.len] == other.limbs[..other.len]
    }
}

impl core::fmt::Debug for Big {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Big(0x")?;
        if self.len == 0 {
            write!(f, "0")?;
        }
        for (i, l) in self.limbs[..self.len].iter().rev().enumerate() {
            if i == 0 {
                write!(f, "{l:x}")?;
            } else {
                write!(f, "_{l:08x}")?;
            }
        }
        write!(f, ")")
    }
}

/// The largest power `r^k` that fits in a `u32`, as `(r^k, k)`. `r >= 2`.
pub(crate) fn limb_power(r: u32) -> (u32, u32) {
    let mut p = r;
    let mut k = 1;
    while let Some(next) = p.checked_mul(r) {
        p = next;
        k += 1;
    }
    (p, k)
}

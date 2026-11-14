use nanocode_numeric::{BigInt, BigUint, Sign};
use nanocode_numeric::{One, ToPrimitive, Zero};

#[must_use]
pub fn true_div(numerator: &BigInt, denominator: &BigInt) -> f64 {
    match nanocode_numeric::rational_to_f64(numerator, denominator) {
        // returned value is $t::MAX but still less than the original
        (val, core::cmp::Ordering::Less) if val == f64::MAX => f64::INFINITY,
        // returned value is $t::MIN but still greater than the original
        (val, core::cmp::Ordering::Greater) if val == f64::MIN => f64::NEG_INFINITY,
        (val, _) => val,
    }
}

#[must_use]
pub fn float_to_ratio(value: f64) -> Option<(BigInt, BigInt)> {
    let sign = match core::cmp::PartialOrd::partial_cmp(&value, &0.0)? {
        core::cmp::Ordering::Less => Sign::Minus,
        core::cmp::Ordering::Equal => return Some((BigInt::zero(), BigInt::one())),
        core::cmp::Ordering::Greater => Sign::Plus,
    };
    nanocode_numeric::f64_to_ratio(value)
        .map(|(numer, denom)| (BigInt::from_biguint(sign, numer), denom.into()))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BytesToIntError {
    InvalidLiteral { base: u32 },
    InvalidBase,
    DigitLimit { got: usize, limit: usize },
}

// https://github.com/python/cpython/blob/4e665351082c50018fb31d80db25b4693057393e/Objects/longobject.c#L2977
// https://github.com/python/cpython/blob/4e665351082c50018fb31d80db25b4693057393e/Objects/longobject.c#L2884
pub fn bytes_to_int(
    buf: &[u8],
    mut base: u32,
    digit_limit: usize,
) -> Result<BigInt, BytesToIntError> {
    if base != 0 && !(2..=36).contains(&base) {
        return Err(BytesToIntError::InvalidBase);
    }
    // A rejected literal names the base that was asked for, not the one a
    // base of 0 turned out to mean.
    let requested_base = base;

    let mut buf = buf.trim_ascii();

    // split sign
    let sign = match buf.first() {
        Some(b'+') => Some(Sign::Plus),
        Some(b'-') => Some(Sign::Minus),
        None => {
            return Err(BytesToIntError::InvalidLiteral {
                base: requested_base,
            });
        }
        _ => None,
    };

    if sign.is_some() {
        buf = &buf[1..];
    }

    let mut error_if_nonzero = false;
    if base == 0 {
        match (buf.first(), buf.get(1)) {
            (Some(v), _) if *v != b'0' => base = 10,
            // A sign on its own leaves nothing to read, which is not the
            // old octal form either.
            (None, _) => base = 10,
            (_, Some(b'x' | b'X')) => base = 16,
            (_, Some(b'o' | b'O')) => base = 8,
            (_, Some(b'b' | b'B')) => base = 2,
            (_, _) => {
                // "old" (C-style) octal literal, now invalid. it might still be zero though
                base = 10;
                error_if_nonzero = true;
            }
        }
    }

    if error_if_nonzero {
        if let [_first, others @ .., last] = buf {
            let is_zero = *last == b'0' && others.iter().all(|&c| c == b'0' || c == b'_');
            if !is_zero {
                return Err(BytesToIntError::InvalidLiteral {
                    base: requested_base,
                });
            }
        }
        return Ok(BigInt::zero());
    }

    if buf.first().is_some_and(|&v| v == b'0')
        && buf.get(1).is_some_and(|&v| {
            (base == 16 && (v == b'x' || v == b'X'))
                || (base == 8 && (v == b'o' || v == b'O'))
                || (base == 2 && (v == b'b' || v == b'B'))
        })
    {
        buf = &buf[2..];

        // One underscore allowed here
        if buf.first().is_some_and(|&v| v == b'_') {
            buf = &buf[1..];
        }
    }

    // A pure digit string longer than the limit can be rejected from its
    // length. The check is a wide digit test so it stays cheaper than parsing.
    if digit_limit > 0
        && !base.is_power_of_two()
        && buf.len() > digit_limit
        && let Some(digits) = plain_ascii_digit_len(buf)
    {
        return Err(BytesToIntError::DigitLimit {
            got: digits,
            limit: digit_limit,
        });
    }

    // Reject empty strings
    let mut prev = *buf.first().ok_or(BytesToIntError::InvalidLiteral {
        base: requested_base,
    })?;

    // Leading underscore not allowed
    if prev == b'_' || !prev.is_ascii_alphanumeric() {
        return Err(BytesToIntError::InvalidLiteral {
            base: requested_base,
        });
    }

    // Verify all characters are digits and underscores
    let mut digits = 1;
    for &cur in buf.iter().skip(1) {
        if cur == b'_' {
            // Double underscore not allowed
            if prev == b'_' {
                return Err(BytesToIntError::InvalidLiteral {
                    base: requested_base,
                });
            }
        } else if cur.is_ascii_alphanumeric() {
            digits += 1;
        } else {
            return Err(BytesToIntError::InvalidLiteral {
                base: requested_base,
            });
        }

        prev = cur;
    }

    // Trailing underscore not allowed
    if prev == b'_' {
        return Err(BytesToIntError::InvalidLiteral {
            base: requested_base,
        });
    }

    if digit_limit > 0 && !base.is_power_of_two() && digits > digit_limit {
        return Err(BytesToIntError::DigitLimit {
            got: digits,
            limit: digit_limit,
        });
    }

    let uint = BigUint::parse_bytes(buf, base).ok_or(BytesToIntError::InvalidLiteral {
        base: requested_base,
    })?;
    Ok(BigInt::from_biguint(sign.unwrap_or(Sign::Plus), uint))
}

/// `Some(buf.len())` when every byte is an ASCII digit.
fn plain_ascii_digit_len(buf: &[u8]) -> Option<usize> {
    let mut rest = buf;
    while rest.len() >= 8 {
        let (head, tail) = rest.split_at(8);
        let word = u64::from_le_bytes(head.try_into().unwrap());
        if !word_all_ascii_digits(word) {
            return None;
        }
        rest = tail;
    }
    rest.iter().all(u8::is_ascii_digit).then_some(buf.len())
}

/// True when each byte of `word` is in `b'0'..=b'9'`.
fn word_all_ascii_digits(word: u64) -> bool {
    let below = word.wrapping_sub(0x3030_3030_3030_3030);
    let above = 0x3939_3939_3939_3939u64.wrapping_sub(word);
    (below | above) & 0x8080_8080_8080_8080 == 0
}

// num-bigint now returns Some(inf) for to_f64() in some cases, so just keep that the same for now
#[inline(always)]
#[must_use]
pub fn bigint_to_finite_float(int: &BigInt) -> Option<f64> {
    int.to_f64().filter(|f| f.is_finite())
}

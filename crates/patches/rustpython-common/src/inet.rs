//! The address converters' text half, which is arithmetic over a spelling
//! rather than a call into a socket layer.
//!
//! A target whose C library carries no `inet_pton` still has to answer for
//! these four names, so the conversions are written out here.  They follow
//! musl, which is wasi-libc: the strict parser reads exactly four octets with
//! no redundant leading zero and the IPv6 grammar without a scope suffix, and
//! the writer compresses the longest run of zero groups.
//!
//! Nothing here touches a descriptor or a host, so the module compiles under
//! `cfg(test)` on every target and its corpus runs with the rest of the unit
//! tests, on hosts where the entry points themselves reach libc instead.

/// `inet_aton` — the lenient parser, which reads one to four parts and gives
/// the last one every byte the earlier ones did not claim.  A part is decimal,
/// octal behind a `0`, or hexadecimal behind a `0x`.
#[must_use]
pub fn aton(text: &[u8]) -> Option<[u8; 4]> {
    let mut parts = Vec::new();
    for field in core::str::from_utf8(text).ok()?.split('.') {
        let (digits, radix) = match field.as_bytes() {
            [b'0', b'x' | b'X', rest @ ..] => (rest, 16),
            [b'0', rest @ ..] if !rest.is_empty() => (rest, 8),
            other => (other, 10),
        };
        let digits = core::str::from_utf8(digits).ok()?;
        parts.push(u32::from_str_radix(digits, radix).ok()?);
        if parts.len() > 4 {
            return None;
        }
    }
    // The last part carries the bytes the leading ones left: `127.1` is
    // `127.0.0.1`, and a bare number is the whole address.
    let leading = parts.len().checked_sub(1)?;
    let mut address: u32 = 0;
    for (index, part) in parts.iter().enumerate() {
        let width = if index == leading {
            32 - 8 * leading
        } else {
            8
        };
        if width < 32 && *part >= 1 << width {
            return None;
        }
        address |= part << (32 - 8 * index - width);
    }
    Some(address.to_be_bytes())
}

/// `inet_ntoa` — the dotted-quad spelling of four address bytes.
#[must_use]
pub fn ntoa(packed: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", packed[0], packed[1], packed[2], packed[3])
}

/// One to three decimal digits with no redundant leading zero, which is the
/// only octet spelling the strict parser reads.
fn strict_octet(field: &[u8]) -> Option<u8> {
    if field.is_empty() || field.len() > 3 || (field.len() > 1 && field[0] == b'0') {
        return None;
    }
    let mut value: u32 = 0;
    for digit in field {
        value = value * 10 + u32::from(digit.checked_sub(b'0').filter(|d| *d < 10)?);
    }
    u8::try_from(value).ok()
}

/// `inet_pton(AF_INET, ...)` — exactly four strict octets.
#[must_use]
pub fn pton_v4(text: &[u8]) -> Option<[u8; 4]> {
    let mut address = [0u8; 4];
    let mut fields = text.split(|b| *b == b'.');
    for slot in &mut address {
        *slot = strict_octet(fields.next()?)?;
    }
    fields.next().is_none().then_some(address)
}

/// `inet_pton(AF_INET6, ...)`.
#[must_use]
pub fn pton_v6(text: &[u8]) -> Option<[u8; 16]> {
    // A trailing dotted quad occupies the last two groups, so it is taken off
    // first and the hexadecimal grammar reads what is left.  The colon before
    // it is a separator, except when the quad follows a compressed run: there
    // it is that run's second colon, and cutting it away would leave a `::`
    // the grammar can no longer see.
    let (head, tail) = match text.iter().position(|b| *b == b'.') {
        None => (text, None),
        Some(_) => {
            let colon = text.iter().rposition(|b| *b == b':')?;
            let head_end = colon + usize::from(text[..colon].ends_with(b":"));
            (&text[..head_end], Some(pton_v4(&text[colon + 1..])?))
        }
    };
    let groups_wanted = if tail.is_some() { 6 } else { 8 };

    let (before, after) = match find_double_colon(head)? {
        None => (head, None),
        Some(at) => (&head[..at], Some(&head[at + 2..])),
    };
    let leading = hex_groups(before, groups_wanted)?;
    let trailing = match after {
        None => Vec::new(),
        Some(rest) => hex_groups(rest, groups_wanted - leading.len())?,
    };
    // Without `::` every group must be spelled; with it at least one must not.
    let elided = groups_wanted - leading.len() - trailing.len();
    if (after.is_none() && elided != 0) || (after.is_some() && elided == 0) {
        return None;
    }

    let mut address = [0u8; 16];
    let mut out = address.iter_mut();
    for group in leading
        .iter()
        .chain(core::iter::repeat_n(&0u16, elided))
        .chain(trailing.iter())
    {
        *out.next()? = (group >> 8) as u8;
        *out.next()? = *group as u8;
    }
    if let Some(quad) = tail {
        address[12..].copy_from_slice(&quad);
    }
    Some(address)
}

/// The offset of the one `::` a spelling may carry, or nothing when a second
/// one makes the address ambiguous.
fn find_double_colon(text: &[u8]) -> Option<Option<usize>> {
    let mut found = None;
    let mut index = 0;
    while index + 1 < text.len() {
        if text[index] == b':' && text[index + 1] == b':' {
            if found.is_some() {
                return None;
            }
            found = Some(index);
            index += 1;
        }
        index += 1;
    }
    Some(found)
}

/// Colon-separated groups of one to four hexadecimal digits.  An empty run is
/// no groups at all, which is what either side of a leading or trailing `::`
/// reads as.
fn hex_groups(text: &[u8], limit: usize) -> Option<Vec<u16>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    let mut groups = Vec::new();
    for field in text.split(|b| *b == b':') {
        if field.is_empty() || field.len() > 4 || groups.len() == limit {
            return None;
        }
        let mut value: u16 = 0;
        for digit in field {
            value = value * 16 + u16::from((*digit as char).to_digit(16)? as u8);
        }
        groups.push(value);
    }
    Some(groups)
}

/// `inet_ntop(AF_INET6, ...)`.
///
/// The address is written out group by group and then has its longest run of
/// zero groups replaced by `::`, which is the rewrite musl performs and the
/// reason a run of one group is left spelled out.
#[must_use]
pub fn ntop_v6(packed: &[u8]) -> String {
    let group = |i: usize| (u16::from(packed[2 * i]) << 8) | u16::from(packed[2 * i + 1]);
    // An IPv4-mapped address keeps its last four bytes in dotted-quad form.
    let text = if packed[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff] {
        format!(
            "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{}.{}.{}.{}",
            group(0),
            group(1),
            group(2),
            group(3),
            group(4),
            group(5),
            packed[12],
            packed[13],
            packed[14],
            packed[15]
        )
    } else {
        (0..8)
            .map(|i| format!("{:x}", group(i)))
            .collect::<Vec<_>>()
            .join(":")
    };

    // The longest run of `:` and `0` that starts the string or starts at a
    // colon, taken only when it spans more than one zero group.
    let bytes = text.as_bytes();
    let (mut best, mut longest) = (0, 2);
    for start in 0..bytes.len() {
        if start != 0 && bytes[start] != b':' {
            continue;
        }
        let run = bytes[start..]
            .iter()
            .take_while(|b| **b == b':' || **b == b'0')
            .count();
        // An interior run includes the colon before its first zero while a
        // leading run does not.  Once the leading run is best, make an
        // interior candidate beat it by that extra byte as well; equal-size
        // zero runs therefore keep the first one, as musl's `inet_ntop` does.
        if run > longest + usize::from(best == 0) {
            (best, longest) = (start, run);
        }
    }
    if longest <= 3 {
        return text;
    }
    format!("{}::{}", &text[..best], &text[best + longest..])
}

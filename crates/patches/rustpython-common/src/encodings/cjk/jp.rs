//! Line-by-line Rust port of PyPy cjkcodecs `_codecs_jp.c`.

use super::mappings_jisx0213_pair::{
    JISX0213_PAIR_DECMAP, JISX0213_PAIR_DECMAP_DATA, JISX0213_PAIR_ENCMAP,
};
use super::mappings_jp::{
    CP932EXT_DECMAP, CP932EXT_DECMAP_DATA, CP932EXT_ENCMAP, CP932EXT_ENCMAP_DATA, JISX0208_DECMAP,
    JISX0208_DECMAP_DATA, JISX0212_DECMAP, JISX0212_DECMAP_DATA, JISX0213_1_BMP_DECMAP,
    JISX0213_1_BMP_DECMAP_DATA, JISX0213_1_EMP_DECMAP, JISX0213_1_EMP_DECMAP_DATA,
    JISX0213_2_BMP_DECMAP, JISX0213_2_BMP_DECMAP_DATA, JISX0213_2_EMP_DECMAP,
    JISX0213_2_EMP_DECMAP_DATA, JISX0213_BMP_ENCMAP, JISX0213_BMP_ENCMAP_DATA, JISX0213_EMP_ENCMAP,
    JISX0213_EMP_ENCMAP_DATA, JISXCOMMON_ENCMAP, JISXCOMMON_ENCMAP_DATA, MULTIC, MapIndex, NOCHAR,
    UNIINV,
};
use super::{DecodeOne, EncodeOne};

const EMPBASE: u32 = 0x20000;

pub(super) fn try_map_decode(index: &[MapIndex; 256], data: &[u16], c1: u8, c2: u8) -> Option<u32> {
    let page = index[c1 as usize];
    if !page.present || c2 < page.bottom || c2 > page.top {
        return None;
    }
    let value = data[page.offset + usize::from(c2 - page.bottom)];
    (value != UNIINV).then_some(u32::from(value))
}

pub(super) fn try_map_encode(index: &[MapIndex; 256], data: &[u16], c: u32) -> Option<u16> {
    if c > 0xffff {
        return None;
    }
    let page = index[(c >> 8) as usize];
    let low = c as u8;
    if !page.present || low < page.bottom || low > page.top {
        return None;
    }
    let value = data[page.offset + usize::from(low - page.bottom)];
    (value != NOCHAR).then_some(value)
}

fn try_pair_decode(c1: u8, c2: u8) -> Option<u32> {
    let page = JISX0213_PAIR_DECMAP[c1 as usize];
    if !page.present || c2 < page.bottom || c2 > page.top {
        return None;
    }
    let value = JISX0213_PAIR_DECMAP_DATA[page.offset + usize::from(c2 - page.bottom)];
    (value != u32::from(super::mappings_jisx0213_pair::UNIINV)).then_some(value)
}

pub(super) fn find_pair_encode(first: u32, second: u32) -> Option<u16> {
    if first > 0xffff || second > 0xffff {
        return None;
    }
    let key = first << 16 | second;
    JISX0213_PAIR_ENCMAP
        .binary_search_by_key(&key, |entry| entry.uniseq)
        .ok()
        .map(|index| JISX0213_PAIR_ENCMAP[index].code)
}

fn bytes1(c: u8) -> EncodeOne {
    let mut output = [0; 8];
    output[0] = c;
    EncodeOne::Bytes(output, 1, 1)
}

fn bytes2(c1: u8, c2: u8, consumed: usize) -> EncodeOne {
    let mut output = [0; 8];
    output[0] = c1;
    output[1] = c2;
    EncodeOne::Bytes(output, 2, consumed)
}

fn bytes3(c1: u8, c2: u8, c3: u8, consumed: usize) -> EncodeOne {
    let mut output = [0; 8];
    output[0] = c1;
    output[1] = c2;
    output[2] = c3;
    EncodeOne::Bytes(output, 3, consumed)
}

fn illegal_decode_lead() -> DecodeOne {
    // [3.14-spec] `DecodeOne::Illegal(1)` ↔ PyPy `_codecs_jp.c`'s decoder
    // bodies return the whole candidate width to `multibytecodec_decerror`.
    // The directly observable span from pinned CPython 3.14.2 (and the real
    // pypy3 oracle) ends after the lead byte, so this combined Rust engine /
    // error-boundary carries that one-byte span explicitly.
    DecodeOne::Illegal(1)
}

pub(super) fn jisx0201_k_encode(c: u32) -> Option<u8> {
    if (0xff61..=0xff9f).contains(&c) {
        Some((c - 0xfec0) as u8)
    } else {
        None
    }
}

fn jisx0201_encode(c: u32) -> Option<u8> {
    if c < 0x80 && c != 0x5c && c != 0x7e {
        Some(c as u8)
    } else if c == 0x00a5 {
        Some(0x5c)
    } else if c == 0x203e {
        Some(0x7e)
    } else {
        jisx0201_k_encode(c)
    }
}

pub(super) fn jisx0201_decode(c: u8) -> Option<u32> {
    if c < 0x5c {
        Some(u32::from(c))
    } else if c == 0x5c {
        Some(0x00a5)
    } else if c < 0x7e {
        Some(u32::from(c))
    } else if c == 0x7e {
        Some(0x203e)
    } else if c == 0x7f {
        Some(0x7f)
    } else if (0xa1..=0xdf).contains(&c) {
        Some(0xfec0 + u32::from(c))
    } else {
        None
    }
}

fn jis_to_shift(code: u16) -> (u8, u8) {
    let c1 = (code >> 8) as u8;
    let c2 = code as u8;
    let c2 = (if (c1 - 0x21) & 1 != 0 { 0x5e } else { 0 }) + (c2 - 0x21);
    let c1 = (c1 - 0x21) >> 1;
    (
        if c1 < 0x1f { c1 + 0x81 } else { c1 + 0xc1 },
        if c2 < 0x3f { c2 + 0x40 } else { c2 + 0x41 },
    )
}

fn shift_to_jis(c: u8, c2: u8) -> Option<(u8, u8)> {
    if c2 < 0x40 || c2 == 0x7f || c2 > 0xfc {
        return None;
    }
    let c1 = if c < 0xe0 { c - 0x81 } else { c - 0xc1 };
    let trail = if c2 < 0x80 { c2 - 0x40 } else { c2 - 0x41 };
    Some((
        2 * c1 + if trail < 0x5e { 0 } else { 1 } + 0x21,
        (if trail < 0x5e { trail } else { trail - 0x5e }) + 0x21,
    ))
}

pub(super) fn encode_cp932(c: u32) -> EncodeOne {
    if c <= 0x80 {
        return bytes1(c as u8);
    }
    if let Some(code) = jisx0201_k_encode(c) {
        return bytes1(code);
    }
    if (0xf8f0..=0xf8f3).contains(&c) {
        return bytes1(if c == 0xf8f0 {
            0xa0
        } else {
            (c - 0xf8f1) as u8 + 0xfd
        });
    }

    let code = if let Some(code) = try_map_encode(&CP932EXT_ENCMAP, &CP932EXT_ENCMAP_DATA, c) {
        return bytes2((code >> 8) as u8, code as u8, 1);
    } else if let Some(code) = try_map_encode(&JISXCOMMON_ENCMAP, &JISXCOMMON_ENCMAP_DATA, c) {
        if code & 0x8000 != 0 {
            return EncodeOne::Illegal(1);
        }
        code
    } else if (0xe000..0xe758).contains(&c) {
        let offset = c - 0xe000;
        let c1 = (offset / 188) as u8;
        let c2 = (offset % 188) as u8;
        return bytes2(c1 + 0xf0, if c2 < 0x3f { c2 + 0x40 } else { c2 + 0x41 }, 1);
    } else {
        return EncodeOne::Illegal(1);
    };
    let (c1, c2) = jis_to_shift(code);
    bytes2(c1, c2, 1)
}

pub(super) fn decode_cp932(input: &[u8]) -> DecodeOne {
    let c = input[0];
    if c <= 0x80 {
        return DecodeOne::Char(u32::from(c), 1);
    }
    if (0xa0..=0xdf).contains(&c) {
        return DecodeOne::Char(
            if c == 0xa0 {
                0xf8f0
            } else {
                0xfec0 + u32::from(c)
            },
            1,
        );
    }
    if c >= 0xfd {
        return DecodeOne::Char(0xf8f1 - 0xfd + u32::from(c), 1);
    }
    if input.len() < 2 {
        return DecodeOne::Incomplete;
    }
    let c2 = input[1];
    if let Some(value) = try_map_decode(&CP932EXT_DECMAP, &CP932EXT_DECMAP_DATA, c, c2) {
        return DecodeOne::Char(value, 2);
    }
    if ((0x81..=0x9f).contains(&c) || (0xe0..=0xea).contains(&c))
        && let Some((c1, c2)) = shift_to_jis(c, c2)
        && let Some(value) = try_map_decode(&JISX0208_DECMAP, &JISX0208_DECMAP_DATA, c1, c2)
    {
        return DecodeOne::Char(value, 2);
    }
    if (0xf0..=0xf9).contains(&c) && ((0x40..=0x7e).contains(&c2) || (0x80..=0xfc).contains(&c2)) {
        return DecodeOne::Char(
            0xe000
                + 188 * u32::from(c - 0xf0)
                + u32::from(if c2 < 0x80 { c2 - 0x40 } else { c2 - 0x41 }),
            2,
        );
    }
    illegal_decode_lead()
}

pub(super) fn encode_euc_jp(c: u32) -> EncodeOne {
    if c < 0x80 {
        return bytes1(c as u8);
    }
    let code = if let Some(code) = try_map_encode(&JISXCOMMON_ENCMAP, &JISXCOMMON_ENCMAP_DATA, c) {
        code
    } else if let Some(code) = jisx0201_k_encode(c) {
        return bytes2(0x8e, code, 1);
    } else if c == 0xff3c {
        0x2140
    } else if c == 0x00a5 {
        return bytes1(0x5c);
    } else if c == 0x203e {
        return bytes1(0x7e);
    } else {
        return EncodeOne::Illegal(1);
    };
    if code & 0x8000 != 0 {
        bytes3(0x8f, (code >> 8) as u8, code as u8 | 0x80, 1)
    } else {
        bytes2((code >> 8) as u8 | 0x80, code as u8 | 0x80, 1)
    }
}

pub(super) fn decode_euc_jp(input: &[u8]) -> DecodeOne {
    let c = input[0];
    if c < 0x80 {
        return DecodeOne::Char(u32::from(c), 1);
    }
    if c == 0x8e {
        if input.len() < 2 {
            return DecodeOne::Incomplete;
        }
        let c2 = input[1];
        return if (0xa1..=0xdf).contains(&c2) {
            DecodeOne::Char(0xfec0 + u32::from(c2), 2)
        } else {
            illegal_decode_lead()
        };
    }
    if c == 0x8f {
        if input.len() < 3 {
            return DecodeOne::Incomplete;
        }
        return try_map_decode(
            &JISX0212_DECMAP,
            &JISX0212_DECMAP_DATA,
            input[1] ^ 0x80,
            input[2] ^ 0x80,
        )
        .map_or_else(illegal_decode_lead, |value| DecodeOne::Char(value, 3));
    }
    if input.len() < 2 {
        return DecodeOne::Incomplete;
    }
    let c2 = input[1];
    if c == 0xa1 && c2 == 0xc0 {
        return DecodeOne::Char(0xff3c, 2);
    }
    try_map_decode(&JISX0208_DECMAP, &JISX0208_DECMAP_DATA, c ^ 0x80, c2 ^ 0x80)
        .map_or_else(illegal_decode_lead, |value| DecodeOne::Char(value, 2))
}

pub(super) fn encode_shift_jis(c: u32) -> EncodeOne {
    let code = if c < 0x80 {
        c as u16
    } else if c == 0x00a5 {
        0x5c
    } else if c == 0x203e {
        0x7e
    } else if let Some(code) = jisx0201_k_encode(c) {
        u16::from(code)
    } else {
        NOCHAR
    };
    if code < 0x80 || (0xa1..=0xdf).contains(&code) {
        return bytes1(code as u8);
    }
    let code = if code == NOCHAR {
        if let Some(code) = try_map_encode(&JISXCOMMON_ENCMAP, &JISXCOMMON_ENCMAP_DATA, c) {
            if code & 0x8000 != 0 {
                return EncodeOne::Illegal(1);
            }
            code
        } else if c == 0xff3c {
            0x2140
        } else {
            return EncodeOne::Illegal(1);
        }
    } else {
        code
    };
    let (c1, c2) = jis_to_shift(code);
    bytes2(c1, c2, 1)
}

pub(super) fn decode_shift_jis(input: &[u8]) -> DecodeOne {
    let c = input[0];
    if c < 0x80 {
        return DecodeOne::Char(u32::from(c), 1);
    }
    if (0xa1..=0xdf).contains(&c) {
        return DecodeOne::Char(0xfec0 + u32::from(c), 1);
    }
    if !((0x81..=0x9f).contains(&c) || (0xe0..=0xea).contains(&c)) {
        return illegal_decode_lead();
    }
    if input.len() < 2 {
        return DecodeOne::Incomplete;
    }
    let Some((c1, c2)) = shift_to_jis(c, input[1]) else {
        return illegal_decode_lead();
    };
    if c1 == 0x21 && c2 == 0x40 {
        return DecodeOne::Char(0xff3c, 2);
    }
    try_map_decode(&JISX0208_DECMAP, &JISX0208_DECMAP_DATA, c1, c2)
        .map_or_else(illegal_decode_lead, |value| DecodeOne::Char(value, 2))
}

pub(super) fn emulate_2000_encode_bmp(c: u32) -> Result<Option<u16>, ()> {
    if matches!(
        c,
        0x9b1c | 0x4ff1 | 0x525d | 0x541e | 0x5653 | 0x59f8 | 0x5c5b | 0x5e77 | 0x7626 | 0x7e6b
    ) {
        Err(())
    } else if c == 0x9b1d {
        Ok(Some(0x8000 | 0x7d3b))
    } else {
        Ok(None)
    }
}

fn emulate_2000_plane1(c1: u8, c2: u8) -> bool {
    matches!(
        (c1, c2),
        (0x2e, 0x21)
            | (0x2f, 0x7e)
            | (0x4f, 0x54)
            | (0x4f, 0x7e)
            | (0x74, 0x27)
            | (0x7e, 0x7a..=0x7e)
    )
}

pub(super) fn encode_jisx0213_code(
    input: &[u32],
    final_input: bool,
    config_2000: bool,
    allow_jisx0212_common: bool,
) -> Result<(u16, usize), EncodeOne> {
    let c = input[0];
    if c <= 0xffff {
        let mut code = if config_2000 {
            emulate_2000_encode_bmp(c).map_err(|()| EncodeOne::Illegal(1))?
        } else {
            None
        };
        if code.is_none() {
            code = try_map_encode(&JISX0213_BMP_ENCMAP, &JISX0213_BMP_ENCMAP_DATA, c);
        }
        if let Some(mut code) = code {
            if code == MULTIC {
                if input.len() < 2 {
                    if !final_input {
                        return Err(EncodeOne::Incomplete);
                    }
                    code = find_pair_encode(c, 0).ok_or(EncodeOne::Illegal(1))?;
                // [3.14-spec] PyPy `_codecs_jp.c::jisx0213_encoder` treats
                // the zero-second pair-map entry as an actual pair and eats
                // the caller's trailing NUL.  CPython 3.14's
                // `multibytecodec_support.TestBase.test_null_terminator`
                // requires that observable NUL to remain a separate byte.
                } else if input[1] != 0
                    && let Some(pair) = find_pair_encode(c, input[1])
                {
                    return Ok((pair, 2));
                } else {
                    code = find_pair_encode(c, 0).ok_or(EncodeOne::Illegal(1))?;
                }
            }
            return Ok((code, 1));
        }
        if let Some(code) = try_map_encode(&JISXCOMMON_ENCMAP, &JISXCOMMON_ENCMAP_DATA, c) {
            // PyPy `_codecs_jp.c`'s `euc_jis_2004_encoder` accepts JIS X 0212
            // entries from `jisxcommon`, while `shift_jis_2004_encoder` and
            // `_codecs_iso2022.c`'s `jisx0213_encoder` reject them.  The high
            // bit is a codeset-2 marker only on the EUC path.
            if allow_jisx0212_common || code & 0x8000 == 0 {
                return Ok((code, 1));
            }
        }
        Err(EncodeOne::Illegal(1))
    } else if c >> 16 == EMPBASE >> 16 {
        if config_2000 && c == 0x20b9f {
            return Err(EncodeOne::Illegal(1));
        }
        try_map_encode(&JISX0213_EMP_ENCMAP, &JISX0213_EMP_ENCMAP_DATA, c & 0xffff)
            .map(|code| (code, 1))
            .ok_or(EncodeOne::Illegal(1))
    } else {
        Err(EncodeOne::Illegal(1))
    }
}

pub(super) fn encode_euc_jis_2004(
    input: &[u32],
    final_input: bool,
    config_2000: bool,
) -> EncodeOne {
    let c = input[0];
    if c < 0x80 {
        return bytes1(c as u8);
    }
    let (code, consumed) = match encode_jisx0213_code(input, final_input, config_2000, true) {
        Ok(value) => value,
        Err(EncodeOne::Illegal(_)) if (0xff61..=0xff9f).contains(&c) => {
            return bytes2(0x8e, (c - 0xfec0) as u8, 1);
        }
        Err(EncodeOne::Illegal(_)) if c == 0xff3c => (0x2140, 1),
        Err(EncodeOne::Illegal(_)) if c == 0xff5e => (0x2232, 1),
        Err(error) => return error,
    };
    if code & 0x8000 != 0 {
        bytes3(0x8f, (code >> 8) as u8, code as u8 | 0x80, consumed)
    } else {
        bytes2((code >> 8) as u8 | 0x80, code as u8 | 0x80, consumed)
    }
}

pub(super) fn decode_jisx0213_plane1(
    c1: u8,
    c2: u8,
    config_2000: bool,
    consumed: usize,
    compat_fullwidth_reverse_solidus: bool,
    compat_fullwidth_tilde: bool,
) -> DecodeOne {
    if config_2000 && emulate_2000_plane1(c1, c2) {
        return DecodeOne::Illegal(consumed);
    }
    if compat_fullwidth_reverse_solidus && c1 == 0x21 && c2 == 0x40 {
        return DecodeOne::Char(0xff3c, consumed);
    }
    if compat_fullwidth_tilde && c1 == 0x22 && c2 == 0x32 {
        return DecodeOne::Char(0xff5e, consumed);
    }
    if let Some(value) = try_map_decode(&JISX0208_DECMAP, &JISX0208_DECMAP_DATA, c1, c2)
        .or_else(|| try_map_decode(&JISX0213_1_BMP_DECMAP, &JISX0213_1_BMP_DECMAP_DATA, c1, c2))
    {
        return DecodeOne::Char(value, consumed);
    }
    if let Some(value) = try_map_decode(&JISX0213_1_EMP_DECMAP, &JISX0213_1_EMP_DECMAP_DATA, c1, c2)
    {
        return DecodeOne::Char(EMPBASE | value, consumed);
    }
    if let Some(value) = try_pair_decode(c1, c2) {
        return DecodeOne::Pair(value >> 16, value & 0xffff, consumed);
    }
    illegal_decode_lead()
}

pub(super) fn decode_jisx0213_plane2(
    c1: u8,
    c2: u8,
    config_2000: bool,
    consumed: usize,
) -> DecodeOne {
    if config_2000 && c1 == 0x7d && c2 == 0x3b {
        return DecodeOne::Char(0x9b1d, consumed);
    }
    if let Some(value) = try_map_decode(&JISX0213_2_BMP_DECMAP, &JISX0213_2_BMP_DECMAP_DATA, c1, c2)
    {
        return DecodeOne::Char(value, consumed);
    }
    if let Some(value) = try_map_decode(&JISX0213_2_EMP_DECMAP, &JISX0213_2_EMP_DECMAP_DATA, c1, c2)
    {
        return DecodeOne::Char(EMPBASE | value, consumed);
    }
    illegal_decode_lead()
}

pub(super) fn decode_euc_jis_2004(input: &[u8], config_2000: bool) -> DecodeOne {
    let c = input[0];
    if c < 0x80 {
        return DecodeOne::Char(u32::from(c), 1);
    }
    if c == 0x8e {
        if input.len() < 2 {
            return DecodeOne::Incomplete;
        }
        return if (0xa1..=0xdf).contains(&input[1]) {
            DecodeOne::Char(0xfec0 + u32::from(input[1]), 2)
        } else {
            illegal_decode_lead()
        };
    }
    if c == 0x8f {
        if input.len() < 3 {
            return DecodeOne::Incomplete;
        }
        let c2 = input[1] ^ 0x80;
        let c3 = input[2] ^ 0x80;
        let decoded = decode_jisx0213_plane2(c2, c3, config_2000, 3);
        if !matches!(decoded, DecodeOne::Illegal(_)) {
            return decoded;
        }
        return try_map_decode(&JISX0212_DECMAP, &JISX0212_DECMAP_DATA, c2, c3)
            .map_or_else(illegal_decode_lead, |value| DecodeOne::Char(value, 3));
    }
    if input.len() < 2 {
        return DecodeOne::Incomplete;
    }
    decode_jisx0213_plane1(c ^ 0x80, input[1] ^ 0x80, config_2000, 2, true, true)
}

pub(super) fn encode_shift_jis_2004(
    input: &[u32],
    final_input: bool,
    config_2000: bool,
) -> EncodeOne {
    let c = input[0];
    if let Some(code) = jisx0201_encode(c) {
        return bytes1(code);
    }
    let (code, consumed) = match encode_jisx0213_code(input, final_input, config_2000, false) {
        Ok((code, consumed)) if code & 0x8000 == 0 || c > 0xffff => (code, consumed),
        Ok((code, consumed)) => {
            if (config_2000 && c == 0x9b1d)
                || try_map_encode(&JISX0213_BMP_ENCMAP, &JISX0213_BMP_ENCMAP_DATA, c).is_some()
            {
                (code, consumed)
            } else {
                return EncodeOne::Illegal(1);
            }
        }
        Err(error) => return error,
    };
    let mut c1 = i32::from(code >> 8);
    let mut c2 = i32::from(code & 0xff) - 0x21;
    if c1 & 0x80 != 0 {
        if c1 >= 0xee {
            c1 -= 0x87;
        } else if c1 >= 0xac || c1 == 0xa8 {
            c1 -= 0x49;
        } else {
            c1 -= 0x43;
        }
    } else {
        c1 -= 0x21;
    }
    if c1 & 1 != 0 {
        c2 += 0x5e;
    }
    c1 >>= 1;
    bytes2(
        (c1 + if c1 < 0x1f { 0x81 } else { 0xc1 }) as u8,
        (c2 + if c2 < 0x3f { 0x40 } else { 0x41 }) as u8,
        consumed,
    )
}

pub(super) fn decode_shift_jis_2004(input: &[u8], config_2000: bool) -> DecodeOne {
    let c = input[0];
    if let Some(value) = jisx0201_decode(c) {
        return DecodeOne::Char(value, 1);
    }
    if !((0x81..=0x9f).contains(&c) || (0xe0..=0xfc).contains(&c)) {
        return illegal_decode_lead();
    }
    if input.len() < 2 {
        return DecodeOne::Incomplete;
    }
    let c2 = input[1];
    if c2 < 0x40 || c2 == 0x7f || c2 > 0xfc {
        return illegal_decode_lead();
    }
    let lead = if c < 0xe0 { c - 0x81 } else { c - 0xc1 };
    let trail = if c2 < 0x80 { c2 - 0x40 } else { c2 - 0x41 };
    let mut c1 = 2 * lead + if trail < 0x5e { 0 } else { 1 };
    let c2 = (if trail < 0x5e { trail } else { trail - 0x5e }) + 0x21;
    if c1 < 0x5e {
        c1 += 0x21;
        return decode_jisx0213_plane1(c1, c2, config_2000, 2, false, false);
    }
    c1 = if c1 >= 0x67 {
        c1 + 0x07
    } else if c1 >= 0x63 || c1 == 0x5f {
        c1 - 0x37
    } else {
        c1 - 0x3d
    };
    decode_jisx0213_plane2(c1, c2, config_2000, 2)
}

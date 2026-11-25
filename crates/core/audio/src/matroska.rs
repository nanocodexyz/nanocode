use crate::bytes::Reader;

pub const EBML_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
const SEGMENT: u64 = 0x1853_8067;
const INFO: u64 = 0x1549_a966;
const TRACKS: u64 = 0x1654_ae6b;
const CLUSTER: u64 = 0x1f43_b675;
const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

pub fn duration_seconds(data: &[u8]) -> Option<f64> {
    let mut reader = Reader::new(data);
    let (id, header) = element(&mut reader)?;
    if id != u64::from(u32::from_be_bytes(EBML_MAGIC)) {
        return None;
    }
    let mut header = Reader::new(header);
    let mut doc_type = None;
    while header.remaining() > 0 {
        let (id, body) = element(&mut header)?;
        if id == 0x4282 {
            doc_type = Some(body);
        }
    }
    if !matches!(doc_type?, b"webm" | b"matroska") {
        return None;
    }
    let segment = loop {
        let (id, body) = element(&mut reader)?;
        if id == SEGMENT {
            break body;
        }
    };
    let mut reader = Reader::new(segment);
    let mut timing = None;
    let mut audio = false;
    while reader.remaining() > 0 {
        let (id, body) = element(&mut reader)?;
        match id {
            INFO => timing = Some(info(body)?),
            TRACKS => audio = has_audio(body)?,
            CLUSTER => break,
            _ => validate_master(id, body)?,
        }
    }
    if !audio {
        return None;
    }
    let (ticks, scale) = timing?;
    crate::ticks_to_seconds(ticks, scale, NANOSECONDS_PER_SECOND)
}

fn vint(reader: &mut Reader<'_>, keep_marker: bool) -> Option<(u64, bool)> {
    let first = reader.u8()?;
    let length = usize::try_from(first.leading_zeros()).ok()? + 1;
    if length > 8 || keep_marker && length > 4 {
        return None;
    }
    let marker = 1u8 << (8 - length);
    let mut value = u64::from(if keep_marker { first } else { first & !marker });
    for byte in reader.bytes(length - 1)? {
        value = (value << 8) | u64::from(*byte);
    }
    let unknown = !keep_marker && value == (1u64 << (7 * length)) - 1;
    Some((value, unknown))
}

fn element<'a>(reader: &mut Reader<'a>) -> Option<(u64, &'a [u8])> {
    let (id, _) = vint(reader, true)?;
    let (length, unknown) = vint(reader, false)?;
    let length = if unknown {
        reader.remaining()
    } else {
        usize::try_from(length).ok()?
    };
    Some((id, reader.bytes(length)?))
}

fn uint(data: &[u8]) -> Option<u64> {
    (data.len() <= 8).then(|| {
        data.iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
    })
}

fn float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f64::from(f32::from_be_bytes(data.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

fn info(data: &[u8]) -> Option<(u64, u64)> {
    let mut reader = Reader::new(data);
    let mut scale = DEFAULT_TIMESTAMP_SCALE;
    let mut duration = None;
    while reader.remaining() > 0 {
        let (id, body) = element(&mut reader)?;
        match id {
            0x002a_d7b1 if !body.is_empty() => scale = uint(body)?,
            0x4489 => duration = Some(float(body)?),
            _ => (),
        }
    }
    let duration = duration?;
    if scale == 0 || scale > u64::from(u32::MAX) || !duration.is_finite() || duration < 0.0 {
        return None;
    }
    Some((duration.round() as u64, scale))
}

fn has_audio(data: &[u8]) -> Option<bool> {
    let mut reader = Reader::new(data);
    let mut audio = false;
    while reader.remaining() > 0 {
        let (id, body) = element(&mut reader)?;
        if id != 0xae {
            continue;
        }
        let mut entry = Reader::new(body);
        let mut number = None;
        let mut uid = None;
        let mut codec = None;
        let mut has_audio = false;
        while entry.remaining() > 0 {
            let (id, body) = element(&mut entry)?;
            match id {
                0xd7 => number = Some(uint(body)?),
                0x73c5 => uid = Some(uint(body)?),
                0x86 => codec = Some(std::str::from_utf8(body).ok()?),
                0xe1 => {
                    validate_audio(body)?;
                    has_audio = true;
                }
                _ => (),
            }
        }
        if number? == 0 || uid? == 0 {
            return None;
        }
        let codec = codec?;
        let supported = supported_codec(codec);
        audio |= has_audio && supported;
    }
    Some(audio)
}

fn supported_codec(codec: &str) -> bool {
    matches!(
        codec,
        "A_MPEG/L1"
            | "A_MPEG/L2"
            | "A_MPEG/L3"
            | "A_FLAC"
            | "A_OPUS"
            | "A_VORBIS"
            | "A_MPC"
            | "A_AC3"
            | "A_AC3/BSID9"
            | "A_AC3/BSID10"
            | "A_EAC3"
            | "A_TRUEHD"
            | "A_ALAC"
            | "A_DTS"
            | "A_TTA1"
            | "A_WAVPACK4"
            | "A_ATRAC/AT1"
            | "A_REAL/ATRC"
            | "A_REAL/14_4"
            | "A_REAL/28_8"
            | "A_REAL/COOK"
            | "A_REAL/SIPR"
            | "A_REAL/RALF"
            | "A_PCM/INT/BIG"
            | "A_PCM/INT/LIT"
            | "A_PCM/FLOAT/IEEE"
            | "A_AAC"
            | "A_AAC/MPEG2/MAIN"
            | "A_AAC/MPEG2/LC"
            | "A_AAC/MPEG2/LC/SBR"
            | "A_AAC/MPEG2/SSR"
            | "A_AAC/MPEG4/MAIN"
            | "A_AAC/MPEG4/LC"
            | "A_AAC/MPEG4/LC/SBR"
            | "A_AAC/MPEG4/SSR"
            | "A_AAC/MPEG4/LTP"
    )
}

fn validate_master(id: u64, data: &[u8]) -> Option<()> {
    let mut pending = vec![(id, data)];
    while let Some((id, data)) = pending.pop() {
        if id == 0xbf && data.len() != 4 {
            return None;
        }
        if !matches!(
            id,
            0x114d_9b74 | 0x4dbb | 0x1254_c367 | 0x7373 | 0x63c0 | 0x67c8
        ) {
            continue;
        }
        let mut reader = Reader::new(data);
        while reader.remaining() > 0 {
            pending.push(element(&mut reader)?);
        }
    }
    Some(())
}

fn validate_audio(data: &[u8]) -> Option<()> {
    let mut reader = Reader::new(data);
    while reader.remaining() > 0 {
        let (id, body) = element(&mut reader)?;
        match id {
            0xb5 | 0x78b5 if !body.is_empty() && float(body)? <= 0.0 => return None,
            0x9f | 0x6264 if !body.is_empty() && uint(body)? == 0 => return None,
            _ => (),
        }
    }
    Some(())
}

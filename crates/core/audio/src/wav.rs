use crate::bytes::Reader;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_ADPCM: u16 = 0x0002;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_ALAW: u16 = 0x0006;
const WAVE_FORMAT_MULAW: u16 = 0x0007;
const WAVE_FORMAT_ADPCM_IMA: u16 = 0x0011;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const KSDATAFORMAT_SUBTYPE_SUFFIX: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
const AMBISONIC_B_FORMAT_SUBTYPE_SUFFIX: [u8; 14] = [
    0x00, 0x00, 0x21, 0x07, 0xd3, 0x11, 0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
];
const UNKNOWN_CHUNK_SIZE: u32 = u32::MAX;

struct Format {
    sample_rate: u32,
    block_size: u64,
    frames_per_block: u64,
}

pub fn duration_seconds(data: &[u8]) -> Option<f64> {
    let mut reader = Reader::new(data);
    if reader.array()? != *b"RIFF" {
        return None;
    }
    let riff_len = reader.u32_le()?;
    if riff_len < 4 || reader.array()? != *b"WAVE" {
        return None;
    }
    let riff_body = reader.rest();
    let mut chunks = if riff_len == UNKNOWN_CHUNK_SIZE {
        Reader::new(riff_body)
    } else {
        let bound = usize::try_from(riff_len - 4).ok()?.min(riff_body.len());
        Reader::new(riff_body.get(..bound)?)
    };
    let mut format: Option<Format> = None;
    loop {
        let id: [u8; 4] = chunks.array()?;
        let len = chunks.u32_le()?;
        if &id == b"data" {
            let format = format?;
            if len != UNKNOWN_CHUNK_SIZE
                && len
                    > riff_len.checked_sub(
                        4 + u32::try_from(riff_body.len() - chunks.remaining()).ok()?,
                    )?
            {
                return None;
            }
            let data_len = if len == UNKNOWN_CHUNK_SIZE {
                return None;
            } else {
                usize::try_from(len).ok()?
            };
            return format.duration_seconds(u64::try_from(data_len).ok()?);
        }
        let body = chunks.bytes(usize::try_from(len).ok()?)?;
        if &id == b"fmt " {
            format = Some(Format::parse(body)?);
        }
        if len % 2 == 1 && chunks.remaining() > 0 {
            chunks.skip(1)?;
        }
    }
}

impl Format {
    fn parse(body: &[u8]) -> Option<Self> {
        let mut reader = Reader::new(body);
        let format_tag = reader.u16_le()?;
        let channels = u64::from(reader.u16_le()?);
        let sample_rate = reader.u32_le()?;
        let _avg_bytes_per_sec = reader.u32_le()?;
        let block_align = u64::from(reader.u16_le()?);
        let bits_per_sample = reader.u16_le()?;
        if channels == 0 || sample_rate == 0 {
            return None;
        }
        let pcm = |bytes_per_sample: u64| Self {
            sample_rate,
            block_size: bytes_per_sample * channels,
            frames_per_block: 1,
        };
        let format = match format_tag {
            WAVE_FORMAT_PCM if matches!(bits_per_sample, 8 | 16 | 24 | 32) => {
                pcm(u64::from(bits_per_sample / 8))
            }
            WAVE_FORMAT_IEEE_FLOAT if matches!(bits_per_sample, 32 | 64) => {
                pcm(u64::from(bits_per_sample / 8))
            }
            WAVE_FORMAT_ALAW | WAVE_FORMAT_MULAW => pcm(1),
            WAVE_FORMAT_ADPCM | WAVE_FORMAT_ADPCM_IMA if bits_per_sample == 4 => {
                let (header_bytes_per_channel, header_frames) = if format_tag == WAVE_FORMAT_ADPCM {
                    (7, 2)
                } else {
                    (4, 1)
                };
                let payload = block_align.checked_sub(header_bytes_per_channel * channels)?;
                Self {
                    sample_rate,
                    block_size: block_align,
                    frames_per_block: 8 * payload / (4 * channels) + header_frames,
                }
            }
            WAVE_FORMAT_EXTENSIBLE => {
                if !Self::extensible_supported(&mut reader, bits_per_sample)? {
                    return None;
                }
                pcm(u64::from(bits_per_sample / 8))
            }
            _ => return None,
        };
        (format.block_size != 0 && format.frames_per_block != 0).then_some(format)
    }

    fn extensible_supported(reader: &mut Reader<'_>, bits_per_sample: u16) -> Option<bool> {
        if reader.u16_le()? != 22 {
            return None;
        }
        let valid_bits_per_sample = reader.u16_le()?;
        let _channel_mask = reader.u32_le()?;
        let sub_format: [u8; 16] = reader.array()?;
        let (code, suffix) = sub_format.split_at(2);
        if suffix != KSDATAFORMAT_SUBTYPE_SUFFIX && suffix != AMBISONIC_B_FORMAT_SUBTYPE_SUFFIX {
            return None;
        }
        let supported = match u16::from_le_bytes([code[0], code[1]]) {
            WAVE_FORMAT_PCM => {
                matches!(bits_per_sample, 8 | 16 | 24 | 32)
                    && valid_bits_per_sample <= bits_per_sample
            }
            WAVE_FORMAT_IEEE_FLOAT => {
                matches!(bits_per_sample, 32 | 64) && valid_bits_per_sample == bits_per_sample
            }
            WAVE_FORMAT_ALAW | WAVE_FORMAT_MULAW => {
                suffix == KSDATAFORMAT_SUBTYPE_SUFFIX && bits_per_sample == 8
            }
            _ => false,
        };
        Some(supported)
    }

    fn duration_seconds(&self, data_len: u64) -> Option<f64> {
        assert!(self.block_size != 0, "a parsed format has a block size");
        assert!(
            self.frames_per_block != 0,
            "a parsed format has frames per block"
        );
        let frames = (data_len / self.block_size).checked_mul(self.frames_per_block)?;
        crate::ticks_to_seconds(frames, 1, u64::from(self.sample_rate))
    }
}

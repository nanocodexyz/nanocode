//! What the archive utilities share (nanocode's addition): CRC-32, Adler-32, deflate and
//! inflate from nanocode-compression, and the gzip container (RFC 1952).

use nanocode_compression::inflate::{Inflate, InflateError, InflateFlush, InflateStatus};
use nanocode_compression::miniz::{self, Level};

pub use nanocode_compression::miniz::Format;

pub fn crc32(data: &[u8]) -> u32 {
    nanocode_compression::checksum::crc32(0, data)
}

pub fn compress(data: &[u8], level: u8, format: Format) -> Vec<u8> {
    assert!((Level::FAST.get()..=Level::BEST.get()).contains(&level));
    let level = Level::new(u32::from(level)).expect("levels 1 to 9 are deflate levels");
    miniz::compress_to_vec(data, level, format)
}

pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    compress(data, level, Format::Raw)
}

const WINDOW_BITS: i32 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEnd {
    More,
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Done { consumed: usize },
    NeedsInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Truncated,
    OutputFull,
    Invalid(&'static str),
}

pub struct Inflater {
    window_bits: i32,
    state: Inflate,
}

impl Inflater {
    pub fn new(format: Format) -> Self {
        let window_bits = match format {
            Format::Raw => -WINDOW_BITS,
            Format::Zlib => WINDOW_BITS,
        };
        Inflater {
            window_bits,
            state: Self::start(window_bits),
        }
    }

    fn start(window_bits: i32) -> Inflate {
        Inflate::new(window_bits).expect("a 32 KiB window is a deflate window")
    }

    pub fn reset(&mut self) {
        self.state = Self::start(self.window_bits);
    }

    pub fn inflate(
        &mut self,
        input: &[u8],
        end: InputEnd,
        output: &mut Vec<u8>,
        output_bytes_max: usize,
    ) -> Result<Progress, Failure> {
        assert!(output.len() <= output_bytes_max);
        let first = self.state.total_in();
        loop {
            let rest = &input[self.read_since(first)..];
            let filled = output.len();
            if filled == output_bytes_max {
                return self.at_limit(rest, end, first);
            }
            if output.capacity() == filled {
                output.reserve(1);
            }
            let room = (output.capacity() - filled).min(output_bytes_max - filled);
            output.resize(filled + room, 0);
            let before = self.state.total_out();
            let status = self
                .state
                .inflate(rest, &mut output[filled..], InflateFlush::NoFlush);
            let wrote = self.written_since(before);
            output.truncate(filled + wrote);
            match status {
                Err(error) => return Err(self.invalid(error)),
                Ok(InflateStatus::StreamEnd) => {
                    return Ok(Progress::Done {
                        consumed: self.read_since(first),
                    });
                }
                Ok(InflateStatus::Ok | InflateStatus::BufError) if wrote < room => {
                    return Self::starved(end);
                }
                Ok(InflateStatus::Ok | InflateStatus::BufError) => {}
            }
        }
    }

    fn at_limit(&mut self, rest: &[u8], end: InputEnd, first: u64) -> Result<Progress, Failure> {
        let mut probe = [0u8];
        let before = self.state.total_out();
        let status = self.state.inflate(rest, &mut probe, InflateFlush::NoFlush);
        if self.written_since(before) > 0 {
            return Err(Failure::OutputFull);
        }
        match status {
            Err(error) => Err(self.invalid(error)),
            Ok(InflateStatus::StreamEnd) => Ok(Progress::Done {
                consumed: self.read_since(first),
            }),
            Ok(InflateStatus::Ok | InflateStatus::BufError) => Self::starved(end),
        }
    }

    fn starved(end: InputEnd) -> Result<Progress, Failure> {
        match end {
            InputEnd::More => Ok(Progress::NeedsInput),
            InputEnd::Final => Err(Failure::Truncated),
        }
    }

    fn invalid(&self, error: InflateError) -> Failure {
        Failure::Invalid(self.state.message().unwrap_or(error.as_str()))
    }

    fn read_since(&self, first: u64) -> usize {
        usize::try_from(self.state.total_in() - first).expect("one call reads at most its input")
    }

    fn written_since(&self, before: u64) -> usize {
        usize::try_from(self.state.total_out().wrapping_sub(before))
            .expect("one call writes at most its output")
    }
}

/// Inflates a raw deflate stream at the start of data: what it holds and how many
/// bytes of data the stream took. None: not a deflate stream.
pub fn inflate(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    match Inflater::new(Format::Raw).inflate(data, InputEnd::Final, &mut out, usize::MAX) {
        Ok(Progress::Done { consumed }) => Some((out, consumed)),
        Ok(Progress::NeedsInput) | Err(_) => None,
    }
}

/// What inflates of a gzip file that ends early or is damaged: the bytes before the
/// damage.
pub fn gunzip_partial(data: &[u8]) -> Vec<u8> {
    if data.len() < 10 {
        return Vec::new();
    }
    let flags = data[3];
    let mut pos = 10;
    if flags & 4 != 0 {
        let len = match (data.get(pos), data.get(pos + 1)) {
            (Some(&a), Some(&b)) => usize::from(u16::from_le_bytes([a, b])),
            _ => return Vec::new(),
        };
        pos += 2 + len;
    }
    for bit in [8u8, 16] {
        if flags & bit != 0 {
            match data
                .get(pos..)
                .and_then(|rest| rest.iter().position(|&b| b == 0))
            {
                Some(nul) => pos += nul + 1,
                None => return Vec::new(),
            }
        }
    }
    if flags & 2 != 0 {
        pos += 2;
    }
    let Some(body) = data.get(pos..) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let _ = Inflater::new(Format::Raw).inflate(body, InputEnd::More, &mut out, usize::MAX);
    out
}

/// One member of a gzip file.
pub struct Member {
    pub data: Vec<u8>,
    pub name: Option<Vec<u8>>,
    pub mtime: u32,
}

pub enum GzipError {
    NotGzip,
    Corrupt(&'static str),
}

/// Reads the gzip member at the start of data: the member and how long it is.
pub fn gunzip_member(data: &[u8]) -> Result<(Member, usize), GzipError> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return Err(GzipError::NotGzip);
    }
    if data[2] != 8 {
        return Err(GzipError::Corrupt("unknown method"));
    }
    let flags = data[3];
    let mtime = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let mut pos = 10;
    let short = GzipError::Corrupt("unexpected end of file");
    if flags & 4 != 0 {
        let len = usize::from(u16::from_le_bytes([
            *data
                .get(pos)
                .ok_or(GzipError::Corrupt("unexpected end of file"))?,
            *data
                .get(pos + 1)
                .ok_or(GzipError::Corrupt("unexpected end of file"))?,
        ]));
        pos += 2 + len;
    }
    let mut name = None;
    for (bit, keep) in [(8u8, true), (16, false)] {
        if flags & bit != 0 {
            let Some(rest) = data.get(pos..) else {
                return Err(short);
            };
            let Some(nul) = rest.iter().position(|&b| b == 0) else {
                return Err(short);
            };
            if keep {
                name = Some(rest[..nul].to_vec());
            }
            pos += nul + 1;
        }
    }
    if flags & 2 != 0 {
        pos += 2;
    }
    let Some(body) = data.get(pos..) else {
        return Err(short);
    };
    let Some((out, used)) = inflate(body) else {
        return Err(GzipError::Corrupt(
            "invalid compressed data--format violated",
        ));
    };
    pos += used;
    let Some(trailer) = data.get(pos..pos + 8) else {
        return Err(short);
    };
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crc != crc32(&out) {
        return Err(GzipError::Corrupt("invalid compressed data--crc error"));
    }
    if size != out.len() as u32 {
        return Err(GzipError::Corrupt("invalid compressed data--length error"));
    }
    Ok((
        Member {
            data: out,
            name,
            mtime,
        },
        pos + 8,
    ))
}

/// Every member of a gzip file, one after the other. The flag says that bytes that
/// are no member follow the last one.
pub fn gunzip(data: &[u8]) -> Result<(Vec<u8>, bool), GzipError> {
    let (first, mut pos) = gunzip_member(data)?;
    let mut out = first.data;
    while pos < data.len() {
        match gunzip_member(&data[pos..]) {
            Ok((member, used)) => {
                out.extend_from_slice(&member.data);
                pos += used;
            }
            Err(GzipError::NotGzip) => return Ok((out, true)),
            Err(e) => return Err(e),
        }
    }
    Ok((out, false))
}

/// A gzip file of one member, as GNU gzip writes it.
pub fn gzip(data: &[u8], level: u8, name: Option<&[u8]>, mtime: u32) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, if name.is_some() { 8 } else { 0 }];
    out.extend_from_slice(&mtime.to_le_bytes());
    out.push(match level {
        9 => 2,
        1 => 4,
        _ => 0,
    });
    out.push(3);
    if let Some(name) = name {
        out.extend_from_slice(name);
        out.push(0);
    }
    out.extend_from_slice(&deflate(data, level));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

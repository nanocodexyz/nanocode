use nanocode_compression::{
    deflate::{Deflate, DeflateConfig, DeflateFlush, DeflateStatus, Strategy},
    inflate::{Inflate, InflateFlush, InflateStatus},
};
use nanocode_durability_core::StoreError;
use nanocode_sys::base64::STANDARD_NO_PAD;

pub(crate) const WINDOW: usize = 1 << WINDOW_BITS;
const WINDOW_BITS: i32 = 15;
const DEFAULT_LEVEL: i32 = -1;
const STREAM_LEVEL: i32 = 7;
const DEFAULT_MEM_LEVEL: i32 = 8;
const FINAL_EMPTY_BLOCK: [u8; 2] = [0x03, 0x00];
const EMPTY_STORED_BLOCK: [u8; 5] = [0x00, 0x00, 0x00, 0xff, 0xff];
const BASE64_GROUP: usize = 3;
const BASE64_CHARS: usize = 4;
pub(crate) const SEAL: usize = (FINAL_EMPTY_BLOCK.len() * BASE64_CHARS).div_ceil(BASE64_GROUP);

pub(crate) const ARCHIVE: char = 'a';
pub(crate) const RECORDS: char = 'r';
pub(crate) const SNAPSHOT: char = 's';
pub(crate) const PART: char = '+';
pub(crate) const BOUNDARY: char = 'b';

#[derive(Clone, Debug, Default)]
pub(crate) struct History(Vec<u8>);

impl History {
    pub(crate) fn extend(&mut self, plain: &[u8]) {
        let kept = &plain[plain.len().saturating_sub(WINDOW)..];
        let excess = (self.0.len() + kept.len()).saturating_sub(WINDOW);
        self.0.drain(..excess);
        self.0.extend_from_slice(kept);
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[cold]
fn failed(action: &str, detail: &str) -> StoreError {
    StoreError::Backend(format!("turn log row {action} failed: {detail}"))
}

fn compressor(level: i32, dictionary: &[u8]) -> Result<Deflate, StoreError> {
    let mut deflate = Deflate::new(DeflateConfig {
        level,
        mem_level: DEFAULT_MEM_LEVEL,
        strategy: Strategy::Default,
        window_bits: -WINDOW_BITS,
    })
    .map_err(|error| failed("compression", error.as_str()))?;
    if !dictionary.is_empty() {
        deflate
            .set_dictionary(dictionary)
            .map_err(|error| failed("compression", error.as_str()))?;
    }
    Ok(deflate)
}

fn encode(tag: char, compressed: &[u8]) -> String {
    let mut body = String::with_capacity(1 + compressed.len().div_ceil(3) * 4);
    body.push(tag);
    STANDARD_NO_PAD.encode_string(compressed, &mut body);
    body
}

pub(crate) fn pack(tag: char, plain: &[u8], dictionary: &[u8]) -> Result<String, StoreError> {
    let mut packer = Packer::new(dictionary)?;
    packer.write(plain)?;
    Ok(packer.finish(tag)?.packed)
}

pub(crate) struct Packer {
    chunk: Vec<u8>,
    compressed: Vec<u8>,
    deflate: Deflate,
    plain: usize,
}

pub(crate) struct Packed {
    pub(crate) packed: String,
    pub(crate) plain: usize,
}

impl Packer {
    pub(crate) fn new(dictionary: &[u8]) -> Result<Self, StoreError> {
        Ok(Self {
            chunk: vec![0; WINDOW],
            compressed: Vec::new(),
            deflate: compressor(DEFAULT_LEVEL, dictionary)?,
            plain: 0,
        })
    }

    pub(crate) const fn plain(&self) -> usize {
        self.plain
    }

    pub(crate) fn write(&mut self, plain: &[u8]) -> Result<(), StoreError> {
        let start = self.deflate.total_in();
        loop {
            let consumed = usize::try_from(self.deflate.total_in() - start)
                .expect("consumed input fits in usize");
            if consumed == plain.len() {
                break;
            }
            let before = self.deflate.total_out();
            self.deflate
                .deflate(&plain[consumed..], &mut self.chunk, DeflateFlush::NoFlush)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(self.deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            self.compressed.extend_from_slice(&self.chunk[..produced]);
            let taken = usize::try_from(self.deflate.total_in() - start)
                .expect("consumed input fits in usize");
            if produced == 0 && taken == consumed {
                return Err(failed("compression", "no progress"));
            }
        }
        self.plain += plain.len();
        Ok(())
    }

    pub(crate) fn finish(mut self, tag: char) -> Result<Packed, StoreError> {
        loop {
            let before = self.deflate.total_out();
            let status = self
                .deflate
                .deflate(&[], &mut self.chunk, DeflateFlush::Finish)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(self.deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            self.compressed.extend_from_slice(&self.chunk[..produced]);
            match status {
                DeflateStatus::StreamEnd => break,
                DeflateStatus::Ok => {}
                DeflateStatus::BufError if produced > 0 => {}
                DeflateStatus::BufError => return Err(failed("compression", "no progress")),
            }
        }
        Ok(Packed {
            packed: encode(tag, &self.compressed),
            plain: self.plain,
        })
    }
}

pub(crate) struct Stream(Deflate);

impl Stream {
    pub(crate) fn new(dictionary: &[u8]) -> Result<Self, StoreError> {
        compressor(STREAM_LEVEL, dictionary).map(Self)
    }

    pub(crate) fn pack(&mut self, tag: char, plain: &[u8]) -> Result<String, StoreError> {
        let deflate = &mut self.0;
        let start = deflate.total_in();
        let mut compressed = Vec::new();
        let mut chunk = vec![0; WINDOW];
        loop {
            let consumed =
                usize::try_from(deflate.total_in() - start).expect("consumed input fits in usize");
            let before = deflate.total_out();
            deflate
                .deflate(&plain[consumed..], &mut chunk, DeflateFlush::SyncFlush)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            compressed.extend_from_slice(&chunk[..produced]);
            let taken =
                usize::try_from(deflate.total_in() - start).expect("consumed input fits in usize");
            if taken == plain.len() && produced < chunk.len() {
                break;
            }
            if produced == 0 && taken == consumed {
                return Err(failed("compression", "no progress"));
            }
        }
        while !compressed.len().is_multiple_of(BASE64_GROUP) {
            compressed.extend_from_slice(&EMPTY_STORED_BLOCK);
        }
        compressed.extend_from_slice(&FINAL_EMPTY_BLOCK);
        Ok(encode(tag, &compressed))
    }
}

pub(crate) fn extensible(body: &str) -> bool {
    body.starts_with(RECORDS)
        && body.len() > 1 + SEAL
        && (body.len() - 1 - SEAL).is_multiple_of(BASE64_CHARS)
}

pub(crate) fn unpack(text: &str, dictionary: &[u8]) -> Result<Vec<u8>, StoreError> {
    let compressed = STANDARD_NO_PAD
        .decode(text)
        .map_err(|error| failed("decoding", &error.to_string()))?;
    let mut inflate =
        Inflate::new(-WINDOW_BITS).map_err(|error| failed("decompression", error.as_str()))?;
    if !dictionary.is_empty() {
        inflate
            .set_dictionary(dictionary)
            .map_err(|error| failed("decompression", error.as_str()))?;
    }
    let start = inflate.total_in();
    let mut plain = Vec::new();
    let mut chunk = vec![0; WINDOW];
    loop {
        let consumed =
            usize::try_from(inflate.total_in() - start).expect("consumed input fits in usize");
        let before = inflate.total_out();
        let status = inflate
            .inflate(&compressed[consumed..], &mut chunk, InflateFlush::NoFlush)
            .map_err(|error| failed("decompression", error.as_str()))?;
        let produced = usize::try_from(inflate.total_out() - before)
            .expect("one call produces at most its chunk");
        plain.extend_from_slice(&chunk[..produced]);
        match status {
            InflateStatus::StreamEnd => return Ok(plain),
            InflateStatus::Ok if produced > 0 || inflate.total_in() - start > consumed as u64 => {}
            InflateStatus::Ok | InflateStatus::BufError => {
                return Err(failed("decompression", "the row ends before its stream"));
            }
        }
    }
}

pub(crate) fn parts(body: &str, limit: Option<usize>) -> Result<Vec<&str>, StoreError> {
    let Some(limit) = limit else {
        return Ok(vec![body]);
    };
    if limit < 2 {
        return Err(StoreError::NotCommitted(format!(
            "a turn log row limit of {limit} bytes cannot hold a tag and a byte"
        )));
    }
    let (first, mut rest) = body.split_at(body.len().min(limit));
    let mut parts = vec![first];
    while !rest.is_empty() {
        let (part, next) = rest.split_at(rest.len().min(limit - 1));
        parts.push(part);
        rest = next;
    }
    Ok(parts)
}

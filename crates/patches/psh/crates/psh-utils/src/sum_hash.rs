//! The digests of the checksum utilities and git: MD5 (RFC 1321), written here from the
//! specification, and SHA-1 and the SHA-2 family (FIPS 180-4) from nanocode-digest.
//! Streaming: [`Digest::update`] any number of times, then [`Digest::finish`]. The tests
//! check the specifications' vectors.

use nanocode_digest::{Sha1, Sha224, Sha256, Sha384, Sha512, Sha512_224, Sha512_256};

/// A digest algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
}

impl Algo {
    /// The name in BSD-style lines (`SHA256 (file) = ...`), as GNU `--tag` writes it.
    pub fn tag(self) -> &'static str {
        match self {
            Algo::Md5 => "MD5",
            Algo::Sha1 => "SHA1",
            Algo::Sha224 => "SHA224",
            Algo::Sha256 => "SHA256",
            Algo::Sha384 => "SHA384",
            Algo::Sha512 => "SHA512",
            Algo::Sha512_224 => "SHA512/224",
            Algo::Sha512_256 => "SHA512/256",
        }
    }

    /// The digest's length in bytes.
    pub fn len(self) -> usize {
        match self {
            Algo::Md5 => MD5_OUTPUT_BYTES,
            Algo::Sha1 => Sha1::OUTPUT_BYTES,
            Algo::Sha224 => Sha224::OUTPUT_BYTES,
            Algo::Sha256 => Sha256::OUTPUT_BYTES,
            Algo::Sha384 => Sha384::OUTPUT_BYTES,
            Algo::Sha512 => Sha512::OUTPUT_BYTES,
            Algo::Sha512_224 => Sha512_224::OUTPUT_BYTES,
            Algo::Sha512_256 => Sha512_256::OUTPUT_BYTES,
        }
    }
}

pub struct Digest(State);

enum State {
    Md5(Md5),
    Sha1(Sha1),
    Sha224(Sha224),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
    Sha512_224(Sha512_224),
    Sha512_256(Sha512_256),
}

const MD5_BLOCK_BYTES: usize = 64;
const MD5_LENGTH_BYTES: usize = 8;
const MD5_OUTPUT_BYTES: usize = 16;
const PADDING_MARKER: u8 = 0x80;

const MD5_INITIAL: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

impl Digest {
    pub fn new(algo: Algo) -> Digest {
        Digest(match algo {
            Algo::Md5 => State::Md5(Md5::new()),
            Algo::Sha1 => State::Sha1(Sha1::new()),
            Algo::Sha224 => State::Sha224(Sha224::new()),
            Algo::Sha256 => State::Sha256(Sha256::new()),
            Algo::Sha384 => State::Sha384(Sha384::new()),
            Algo::Sha512 => State::Sha512(Sha512::new()),
            Algo::Sha512_224 => State::Sha512_224(Sha512_224::new()),
            Algo::Sha512_256 => State::Sha512_256(Sha512_256::new()),
        })
    }

    pub fn update(&mut self, data: &[u8]) {
        match &mut self.0 {
            State::Md5(hasher) => hasher.update(data),
            State::Sha1(hasher) => hasher.update(data),
            State::Sha224(hasher) => hasher.update(data),
            State::Sha256(hasher) => hasher.update(data),
            State::Sha384(hasher) => hasher.update(data),
            State::Sha512(hasher) => hasher.update(data),
            State::Sha512_224(hasher) => hasher.update(data),
            State::Sha512_256(hasher) => hasher.update(data),
        }
    }

    pub fn finish(self) -> Vec<u8> {
        match self.0 {
            State::Md5(hasher) => hasher.finish().to_vec(),
            State::Sha1(hasher) => hasher.finalize().to_vec(),
            State::Sha224(hasher) => hasher.finalize().to_vec(),
            State::Sha256(hasher) => hasher.finalize().to_vec(),
            State::Sha384(hasher) => hasher.finalize().to_vec(),
            State::Sha512(hasher) => hasher.finalize().to_vec(),
            State::Sha512_224(hasher) => hasher.finalize().to_vec(),
            State::Sha512_256(hasher) => hasher.finalize().to_vec(),
        }
    }
}

struct Md5 {
    words: [u32; 4],
    block: [u8; MD5_BLOCK_BYTES],
    block_filled: usize,
    length_bytes: u64,
}

impl Md5 {
    fn new() -> Md5 {
        Md5 {
            words: MD5_INITIAL,
            block: [0; MD5_BLOCK_BYTES],
            block_filled: 0,
            length_bytes: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.length_bytes = self.length_bytes.wrapping_add(data.len() as u64);
        let mut rest = data;
        if self.block_filled > 0 {
            let take = (MD5_BLOCK_BYTES - self.block_filled).min(rest.len());
            self.block[self.block_filled..self.block_filled + take].copy_from_slice(&rest[..take]);
            self.block_filled += take;
            rest = &rest[take..];
            if self.block_filled < MD5_BLOCK_BYTES {
                return;
            }
            let block = self.block;
            md5_compress(&mut self.words, &block);
            self.block_filled = 0;
        }
        let (blocks, tail) = rest.as_chunks::<MD5_BLOCK_BYTES>();
        for block in blocks {
            md5_compress(&mut self.words, block);
        }
        self.block[..tail.len()].copy_from_slice(tail);
        self.block_filled = tail.len();
    }

    fn finish(mut self) -> [u8; MD5_OUTPUT_BYTES] {
        let mut tail = [0u8; 2 * MD5_BLOCK_BYTES];
        let filled = self.block_filled;
        tail[..filled].copy_from_slice(&self.block[..filled]);
        tail[filled] = PADDING_MARKER;
        let total = if filled + 1 + MD5_LENGTH_BYTES <= MD5_BLOCK_BYTES {
            MD5_BLOCK_BYTES
        } else {
            2 * MD5_BLOCK_BYTES
        };
        tail[total - MD5_LENGTH_BYTES..total]
            .copy_from_slice(&self.length_bytes.wrapping_mul(8).to_le_bytes());
        for block in tail[..total].as_chunks::<MD5_BLOCK_BYTES>().0 {
            md5_compress(&mut self.words, block);
        }
        let mut out = [0u8; MD5_OUTPUT_BYTES];
        for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.words) {
            *bytes = word.to_le_bytes();
        }
        out
    }
}

fn md5_compress(state: &mut [u32; 4], block: &[u8]) {
    assert_eq!(block.len(), MD5_BLOCK_BYTES);
    let mut message = [0u32; 16];
    for (word, bytes) in message.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for round in 0..64 {
        let (mixed, index) = match round / 16 {
            0 => ((b & c) | (!b & d), round),
            1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
            2 => (b ^ c ^ d, (3 * round + 5) % 16),
            _ => (c ^ (b | !d), (7 * round) % 16),
        };
        let sum = a
            .wrapping_add(mixed)
            .wrapping_add(MD5_SINES[round])
            .wrapping_add(message[index]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(MD5_SHIFTS[round]));
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// The digest of `data`.
pub fn digest(algo: Algo, data: &[u8]) -> Vec<u8> {
    let mut d = Digest::new(algo);
    d.update(data);
    d.finish()
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 15)] as char);
    }
    s
}
#[cfg(test)]
mod tests {
    use super::*;

    fn h(algo: Algo, data: &[u8]) -> String {
        hex(&digest(algo, data))
    }

    /// RFC 1321 Appendix A.5.
    #[test]
    fn md5_vectors() {
        let cases: &[(&str, &str)] = &[
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(h(Algo::Md5, input.as_bytes()), *want, "{input:?}");
        }
    }

    const ABC: &[u8] = b"abc";
    const TWO_BLOCK: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    const SHA512_TWO_BLOCK: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";

    /// FIPS 180-4's examples (NIST CSRC "Examples with intermediate values").
    #[test]
    fn sha_vectors() {
        let cases: &[(Algo, &[u8], &str)] = &[
            (Algo::Sha1, ABC, "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                Algo::Sha1,
                TWO_BLOCK,
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
            (Algo::Sha1, b"", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            (
                Algo::Sha224,
                ABC,
                "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
            ),
            (
                Algo::Sha224,
                TWO_BLOCK,
                "75388b16512776cc5dba5da1fd890150b0c6455cb4f58b1952522525",
            ),
            (
                Algo::Sha256,
                ABC,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                Algo::Sha256,
                TWO_BLOCK,
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                Algo::Sha256,
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                Algo::Sha384,
                ABC,
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
            ),
            (
                Algo::Sha384,
                SHA512_TWO_BLOCK,
                "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712fcc7c71a557e2db966c3e9fa91746039",
            ),
            (
                Algo::Sha512,
                ABC,
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ),
            (
                Algo::Sha512_224,
                ABC,
                "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa",
            ),
            (
                Algo::Sha512_256,
                ABC,
                "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23",
            ),
            (
                Algo::Sha512,
                SHA512_TWO_BLOCK,
                "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909",
            ),
        ];
        for (algo, input, want) in cases {
            assert_eq!(h(*algo, input), *want, "{algo:?} {input:?}");
        }
    }

    /// One million `a`s (FIPS 180-4 / RFC 3174's third test), fed in uneven pieces so
    /// that the block buffering is exercised.
    #[test]
    fn long_inputs() {
        let data = vec![b'a'; 1_000_000];
        let piecewise = |algo| {
            let mut d = Digest::new(algo);
            for chunk in data.chunks(997) {
                d.update(chunk);
            }
            hex(&d.finish())
        };
        assert_eq!(
            piecewise(Algo::Sha1),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        assert_eq!(
            piecewise(Algo::Sha256),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        assert_eq!(
            piecewise(Algo::Sha512),
            "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
        );
        assert_eq!(piecewise(Algo::Md5), "7707d6ae4e027c70eea2a935c2296f21");
        // Lengths around the padding boundary.
        for n in 50..140 {
            let d: Vec<u8> = (0..n).map(|i| i as u8).collect();
            for algo in [Algo::Md5, Algo::Sha1, Algo::Sha256, Algo::Sha512] {
                let mut s = Digest::new(algo);
                let (x, y) = d.split_at(n / 3);
                s.update(x);
                s.update(y);
                assert_eq!(s.finish(), digest(algo, &d));
            }
        }
    }
}

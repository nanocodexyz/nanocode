// spell-checker:ignore blake2b blake2s dklen fanout hmac opad pbkdf2 scrypt wasm32

//! VM-independent hashlib digest engine.
//!
//! The engine owns digest, HMAC, PBKDF2-HMAC, and scrypt state and reports
//! plain Rust results so interpreter and embedding layers can provide their
//! own object and exception adapters.

mod blake2;
mod blake2b;
mod blake2s;
mod block;
mod keccak;
mod md5;
mod scrypt;
mod sha1;
mod sha256;
mod sha512;

use blake2::Parameters;
use blake2b::Blake2b;
use blake2s::Blake2s;
use keccak::{Keccak, SHAKE_SUFFIX};
use md5::Md5;
use crate::lock::PyMutex as Mutex;
use sha1::Sha1;
use sha256::{SHA224_INITIAL_STATE, SHA224_OUTPUT_BYTES, SHA256_INITIAL_STATE, Sha256};
use sha512::{SHA384_INITIAL_STATE, SHA384_OUTPUT_BYTES, SHA512_INITIAL_STATE, Sha512};

const SHA3_224_OUTPUT_BYTES: usize = 28;
const SHA3_256_OUTPUT_BYTES: usize = 32;
const SHA3_384_OUTPUT_BYTES: usize = 48;
const SHA3_512_OUTPUT_BYTES: usize = 64;
const SHAKE_128_RATE_BYTES: usize = 168;
const SHAKE_256_RATE_BYTES: usize = 136;

#[derive(Clone)]
enum HashState {
    Md5(Md5),
    Sha1(Sha1),
    Sha224(Sha256),
    Sha256(Sha256),
    Sha384(Sha512),
    Sha512(Sha512),
    Sha3 { state: Keccak, output_bytes: usize },
    Shake(Keccak),
    Blake2b(Blake2b),
    Blake2s(Blake2s),
}

impl HashState {
    fn new(name: &str) -> Option<Self> {
        Some(match name {
            "md5" => Self::Md5(Md5::new()),
            "sha1" => Self::Sha1(Sha1::new()),
            "sha224" => Self::Sha224(Sha256::new(SHA224_INITIAL_STATE)),
            "sha256" => Self::Sha256(Sha256::new(SHA256_INITIAL_STATE)),
            "sha384" => Self::Sha384(Sha512::new(SHA384_INITIAL_STATE)),
            "sha512" => Self::Sha512(Sha512::new(SHA512_INITIAL_STATE)),
            "sha3_224" => Self::sha3(SHA3_224_OUTPUT_BYTES),
            "sha3_256" => Self::sha3(SHA3_256_OUTPUT_BYTES),
            "sha3_384" => Self::sha3(SHA3_384_OUTPUT_BYTES),
            "sha3_512" => Self::sha3(SHA3_512_OUTPUT_BYTES),
            "shake_128" => Self::Shake(Keccak::new(SHAKE_128_RATE_BYTES, SHAKE_SUFFIX)),
            "shake_256" => Self::Shake(Keccak::new(SHAKE_256_RATE_BYTES, SHAKE_SUFFIX)),
            "blake2b" => Self::Blake2b(Blake2b::new(&Parameters::default_for(
                blake2b::OUTPUT_BYTES,
            ))),
            "blake2s" => Self::Blake2s(Blake2s::new(&Parameters::default_for(
                blake2s::OUTPUT_BYTES,
            ))),
            _ => return None,
        })
    }

    const fn sha3(output_bytes: usize) -> Self {
        Self::Sha3 {
            state: Keccak::sha3(output_bytes),
            output_bytes,
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(state) => state.update(data),
            Self::Sha1(state) => state.update(data),
            Self::Sha224(state) | Self::Sha256(state) => state.update(data),
            Self::Sha384(state) | Self::Sha512(state) => state.update(data),
            Self::Sha3 { state, .. } | Self::Shake(state) => state.update(data),
            Self::Blake2b(state) => state.update(data),
            Self::Blake2s(state) => state.update(data),
        }
    }

    fn digest(&self, length: usize) -> Vec<u8> {
        match self {
            Self::Md5(state) => state.clone().finalize().to_vec(),
            Self::Sha1(state) => state.clone().finalize().to_vec(),
            Self::Sha224(state) => state.clone().finalize()[..SHA224_OUTPUT_BYTES].to_vec(),
            Self::Sha256(state) => state.clone().finalize().to_vec(),
            Self::Sha384(state) => state.clone().finalize()[..SHA384_OUTPUT_BYTES].to_vec(),
            Self::Sha512(state) => state.clone().finalize().to_vec(),
            Self::Sha3 {
                state,
                output_bytes,
            } => squeeze(state, *output_bytes),
            Self::Shake(state) => squeeze(state, length),
            Self::Blake2b(state) => state.clone().finalize(),
            Self::Blake2s(state) => state.clone().finalize(),
        }
    }
}

fn squeeze(state: &Keccak, length: usize) -> Vec<u8> {
    let mut output = vec![0; length];
    state.clone().finalize(&mut output);
    output
}

type LockedHashState = Mutex<HashState>;

/// Size/alignment contract for the opaque, object-owned storage embedded in
/// a caller-owned `_HashState` payload.  The digest implementations are
/// fixed-size state machines; none owns heap memory or needs drop glue.
///
/// The capacity is stated in BYTES because that is what has to hold a state
/// machine whose size does not follow the target's pointer width.  Stating it
/// as a word count gave wasm32 half the room a 64-bit target got, and the
/// runtime check below then aborted the guest on the first `hashlib` digest.
pub const HASH_STATE_STORAGE_BYTES: usize = 512;
pub const HASH_STATE_STORAGE_WORDS: usize =
    HASH_STATE_STORAGE_BYTES / core::mem::size_of::<usize>();
pub const HASH_STATE_STORAGE_ALIGN: usize = 16;

// The state's size is a compile-time fact on every target, so the target that
// cannot hold it fails to build rather than trapping in the field.
const _: () = assert!(core::mem::size_of::<LockedHashState>() <= HASH_STATE_STORAGE_BYTES);
const _: () = assert!(core::mem::align_of::<LockedHashState>() <= HASH_STATE_STORAGE_ALIGN);

fn check_storage(storage: *mut usize, words: usize) {
    assert!(words >= HASH_STATE_STORAGE_WORDS);
    assert!(!storage.is_null());
    assert_eq!((storage as usize) % HASH_STATE_STORAGE_ALIGN, 0);
}

/// Initialize an object-owned opaque state buffer. Returns false for an
/// unsupported canonical digest name.
///
/// # Safety
///
/// `storage` must point to an aligned, writable, uninitialized buffer of at
/// least `words * size_of::<usize>()` bytes. The buffer must remain valid until
/// its successfully initialized state is passed to [`state_drop`].
pub unsafe fn state_init(storage: *mut usize, words: usize, name: &str) -> bool {
    check_storage(storage, words);
    let Some(state) = HashState::new(name) else {
        return false;
    };
    unsafe { storage.cast::<LockedHashState>().write(Mutex::new(state)) };
    true
}

/// Initialize a BLAKE2 state with the complete RFC 7693 parameter block.
/// The Python layer validates the exact argument ranges before calling;
/// the backend APIs then encode the fields in the algorithm-defined little-
/// endian parameter layout.
///
/// # Safety
///
/// `storage` must point to an aligned, writable, uninitialized buffer of at
/// least `words * size_of::<usize>()` bytes. The buffer must remain valid until
/// its successfully initialized state is passed to [`state_drop`].
#[allow(clippy::too_many_arguments)]
pub unsafe fn state_init_blake2(
    storage: *mut usize,
    words: usize,
    name: &str,
    digest_size: usize,
    key: &[u8],
    salt: &[u8],
    person: &[u8],
    fanout: u8,
    depth: u8,
    leaf_size: u32,
    node_offset: u64,
    node_depth: u8,
    inner_size: usize,
    last_node: bool,
) -> bool {
    check_storage(storage, words);
    let parameters = Parameters {
        digest_size,
        key,
        salt,
        person,
        fanout,
        depth,
        leaf_size,
        node_offset,
        node_depth,
        inner_size,
        last_node,
    };
    let state = match name {
        "blake2b" => HashState::Blake2b(Blake2b::new(&parameters)),
        "blake2s" => HashState::Blake2s(Blake2s::new(&parameters)),
        _ => return false,
    };
    unsafe { storage.cast::<LockedHashState>().write(Mutex::new(state)) };
    true
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`state_init`] or
/// [`state_init_blake2`] with the same `words` value, and it must not be
/// dropped while this operation runs.
pub unsafe fn state_update(storage: *mut usize, words: usize, data: &[u8]) {
    check_storage(storage, words);
    let state = unsafe { &*storage.cast::<LockedHashState>() };
    state.lock().update(data);
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`state_init`] or
/// [`state_init_blake2`] with the same `words` value, and it must not be
/// dropped while this operation runs.
#[must_use]
pub unsafe fn state_digest(storage: *const usize, words: usize, length: usize) -> Vec<u8> {
    check_storage(storage.cast_mut(), words);
    let state = unsafe { &*storage.cast::<LockedHashState>() };
    state.lock().digest(length)
}

/// # Safety
///
/// `src` must contain a live initialized state, while `dst` must point to a
/// distinct aligned, writable, uninitialized buffer. Both buffers must hold at
/// least `words * size_of::<usize>()` bytes and remain valid for this call.
pub unsafe fn state_copy(src: *const usize, dst: *mut usize, words: usize) {
    check_storage(src.cast_mut(), words);
    check_storage(dst, words);
    let source = unsafe { &*src.cast::<LockedHashState>() };
    let cloned = source.lock().clone();
    unsafe { dst.cast::<LockedHashState>().write(Mutex::new(cloned)) };
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`state_init`],
/// [`state_init_blake2`], or [`state_copy`] with the same `words` value. The
/// state must not be used again after this call.
pub unsafe fn state_drop(storage: *mut usize, words: usize) {
    check_storage(storage, words);
    unsafe { core::ptr::drop_in_place(storage.cast::<LockedHashState>()) };
}

#[derive(Clone)]
struct HmacState {
    inner: HashState,
    outer: HashState,
}

impl HmacState {
    fn new(name: &str, key: &[u8]) -> Option<Self> {
        let block_size = digest_block_size(name)?;
        let mut key = if key.len() > block_size {
            HashState::new(name)?.tap_update(key).digest(0)
        } else {
            key.to_vec()
        };
        key.resize(block_size, 0);
        let mut inner = HashState::new(name)?;
        let mut outer = HashState::new(name)?;
        let mut ipad = key.clone();
        let mut opad = key;
        for byte in &mut ipad {
            *byte ^= 0x36;
        }
        for byte in &mut opad {
            *byte ^= 0x5c;
        }
        inner.update(&ipad);
        outer.update(&opad);
        Some(Self { inner, outer })
    }

    fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    fn digest(&self) -> Vec<u8> {
        let mut outer = self.outer.clone();
        outer.update(&self.inner.digest(0));
        outer.digest(0)
    }
}

/// PBKDF2-HMAC (RFC 8018), used by `_hashlib.pbkdf2_hmac`.
#[must_use]
pub fn compute_pbkdf2_hmac(
    name: &str,
    password: &[u8],
    salt: &[u8],
    iterations: usize,
    dklen: usize,
) -> Option<Vec<u8>> {
    if iterations == 0 || dklen == 0 {
        return None;
    }
    let digest_size = digest_output_size(name)?;
    let blocks = dklen.checked_add(digest_size - 1)? / digest_size;
    if blocks > u32::MAX as usize {
        return None;
    }
    let mut derived = Vec::with_capacity(blocks * digest_size);
    let mut first_input = Vec::with_capacity(salt.len() + 4);
    first_input.extend_from_slice(salt);
    let keyed = HmacState::new(name, password)?;
    for block in 1..=blocks {
        first_input.truncate(salt.len());
        first_input.extend_from_slice(&(block as u32).to_be_bytes());
        let mut hmac = keyed.clone();
        hmac.update(&first_input);
        let mut u = hmac.digest();
        let mut accumulator = u.clone();
        for _ in 1..iterations {
            let mut hmac = keyed.clone();
            hmac.update(&u);
            u = hmac.digest();
            for (out, byte) in accumulator.iter_mut().zip(&u) {
                *out ^= *byte;
            }
        }
        derived.extend_from_slice(&accumulator);
    }
    derived.truncate(dklen);
    Some(derived)
}

/// RFC 7914 scrypt, used by `_hashlib.scrypt`.
///
/// `_hashlib` performs argument and memory-limit validation before entering
/// this backend.  `Params::new` still enforces the algorithm's overflow
/// constraints; the raw `scrypt` KDF sizes the caller-owned output
/// independently of any password-hash facade length.
#[must_use]
pub fn compute_scrypt(
    password: &[u8],
    salt: &[u8],
    log_n: u8,
    r: u32,
    p: u32,
    dklen: usize,
) -> Option<Vec<u8>> {
    scrypt::scrypt(password, salt, log_n, r, p, dklen)
}

impl HashState {
    fn tap_update(mut self, data: &[u8]) -> Self {
        self.update(data);
        self
    }
}

#[must_use]
pub fn digest_block_size(name: &str) -> Option<usize> {
    Some(match name {
        "md5" => md5::BLOCK_BYTES,
        "sha1" => sha1::BLOCK_BYTES,
        "sha224" | "sha256" => sha256::BLOCK_BYTES,
        "sha384" | "sha512" => sha512::BLOCK_BYTES,
        "blake2s" => blake2s::BLOCK_BYTES,
        "blake2b" => blake2b::BLOCK_BYTES,
        "sha3_224" => 144,
        "sha3_256" => 136,
        "sha3_384" => 104,
        "sha3_512" => 72,
        // RFC 2104 HMAC is not defined for XOF algorithms.
        "shake_128" | "shake_256" => return None,
        _ => return None,
    })
}

#[must_use]
pub fn digest_output_size(name: &str) -> Option<usize> {
    Some(match name {
        "md5" => md5::OUTPUT_BYTES,
        "sha1" => sha1::OUTPUT_BYTES,
        "sha224" => SHA224_OUTPUT_BYTES,
        "sha256" => sha256::OUTPUT_BYTES,
        "sha384" => SHA384_OUTPUT_BYTES,
        "sha512" => sha512::OUTPUT_BYTES,
        "sha3_224" => SHA3_224_OUTPUT_BYTES,
        "sha3_256" => SHA3_256_OUTPUT_BYTES,
        "sha3_384" => SHA3_384_OUTPUT_BYTES,
        "sha3_512" => SHA3_512_OUTPUT_BYTES,
        "blake2s" => blake2s::OUTPUT_BYTES,
        "blake2b" => blake2b::OUTPUT_BYTES,
        "shake_128" | "shake_256" => return None,
        _ => return None,
    })
}

type LockedHmacState = Mutex<HmacState>;
/// [`HASH_STATE_STORAGE_BYTES`]'s counterpart: an HMAC carries two digest
/// states, so it is sized in bytes for the same reason.
pub const HMAC_STATE_STORAGE_BYTES: usize = 1024;
pub const HMAC_STATE_STORAGE_WORDS: usize =
    HMAC_STATE_STORAGE_BYTES / core::mem::size_of::<usize>();
pub const HMAC_STATE_STORAGE_ALIGN: usize = 16;

const _: () = assert!(core::mem::size_of::<LockedHmacState>() <= HMAC_STATE_STORAGE_BYTES);
const _: () = assert!(core::mem::align_of::<LockedHmacState>() <= HMAC_STATE_STORAGE_ALIGN);

fn check_hmac_storage(storage: *mut usize, words: usize) {
    assert!(words >= HMAC_STATE_STORAGE_WORDS);
    assert!(!storage.is_null());
    assert_eq!((storage as usize) % HMAC_STATE_STORAGE_ALIGN, 0);
}

/// # Safety
///
/// `storage` must point to an aligned, writable, uninitialized buffer of at
/// least `words * size_of::<usize>()` bytes. The buffer must remain valid until
/// its successfully initialized state is passed to [`hmac_state_drop`].
pub unsafe fn hmac_state_init(storage: *mut usize, words: usize, name: &str, key: &[u8]) -> bool {
    check_hmac_storage(storage, words);
    let Some(state) = HmacState::new(name, key) else {
        return false;
    };
    unsafe { storage.cast::<LockedHmacState>().write(Mutex::new(state)) };
    true
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`hmac_state_init`] with
/// the same `words` value, and it must not be dropped while this operation
/// runs.
pub unsafe fn hmac_state_update(storage: *mut usize, words: usize, data: &[u8]) {
    check_hmac_storage(storage, words);
    unsafe { &*storage.cast::<LockedHmacState>() }
        .lock()
        .update(data);
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`hmac_state_init`] with
/// the same `words` value, and it must not be dropped while this operation
/// runs.
#[must_use]
pub unsafe fn hmac_state_digest(storage: *const usize, words: usize) -> Vec<u8> {
    check_hmac_storage(storage.cast_mut(), words);
    unsafe { &*storage.cast::<LockedHmacState>() }
        .lock()
        .digest()
}

/// # Safety
///
/// `src` must contain a live initialized HMAC state, while `dst` must point to
/// a distinct aligned, writable, uninitialized buffer. Both buffers must hold
/// at least `words * size_of::<usize>()` bytes and remain valid for this call.
pub unsafe fn hmac_state_copy(src: *const usize, dst: *mut usize, words: usize) {
    check_hmac_storage(src.cast_mut(), words);
    check_hmac_storage(dst, words);
    let cloned = unsafe { &*src.cast::<LockedHmacState>() }.lock().clone();
    unsafe { dst.cast::<LockedHmacState>().write(Mutex::new(cloned)) };
}

/// # Safety
///
/// `storage` must contain a live state initialized by [`hmac_state_init`] or
/// [`hmac_state_copy`] with the same `words` value. The state must not be used
/// again after this call.
pub unsafe fn hmac_state_drop(storage: *mut usize, words: usize) {
    check_hmac_storage(storage, words);
    unsafe { core::ptr::drop_in_place(storage.cast::<LockedHmacState>()) };
}

#[must_use]
#[inline(never)]
pub fn compute_digest(name: &str, data: &[u8], length: usize) -> Option<Vec<u8>> {
    let mut state = HashState::new(name)?;
    state.update(data);
    Some(state.digest(length))
}

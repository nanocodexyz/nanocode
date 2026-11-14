use core::fmt;

use crate::crypto;
use crate::crypto::cipher::{AeadKey, Iv};
use crate::enums::{CipherSuite, SignatureAlgorithm, SignatureScheme};
use crate::tls12::Tls12CipherSuite;
use crate::tls13::Tls13CipherSuite;
use crate::versions::TLS12;
use crate::versions::{SupportedProtocolVersion, TLS13};

/// Common state for cipher suites (both for TLS 1.2 and TLS 1.3)
pub struct CipherSuiteCommon {
    /// The TLS enumeration naming this cipher suite.
    pub suite: CipherSuite,

    /// Which hash function the suite uses.
    pub hash_provider: &'static dyn crypto::hash::Hash,

    /// Number of TCP-TLS messages that can be safely encrypted with a single key of this type
    ///
    /// Once a `MessageEncrypter` produced for this suite has encrypted more than
    /// `confidentiality_limit` messages, an attacker gains an advantage in distinguishing it
    /// from an ideal pseudorandom permutation (PRP).
    ///
    /// This is to be set on the assumption that messages are maximally sized --
    /// each is 2<sup>14</sup> bytes.
    ///
    /// For AES-GCM implementations, this should be set to 2<sup>24</sup> to limit attack
    /// probability to one in 2<sup>60</sup>.  See [AEBounds] (Table 1) and [draft-irtf-aead-limits-08]:
    ///
    /// ```python
    /// >>> p = 2 ** -60
    /// >>> L = (2 ** 14 // 16) + 1
    /// >>> qlim = (math.sqrt(p) * (2 ** (129 // 2)) - 1) / (L + 1)
    /// >>> print(int(qlim).bit_length())
    /// 24
    /// ```
    /// [AEBounds]: https://eprint.iacr.org/2024/051.pdf
    /// [draft-irtf-aead-limits-08]: https://www.ietf.org/archive/id/draft-irtf-cfrg-aead-limits-08.html#section-5.1.1
    ///
    /// For chacha20-poly1305 implementations, this should be set to `u64::MAX`:
    /// see <https://www.ietf.org/archive/id/draft-irtf-cfrg-aead-limits-08.html#section-5.2.1>
    pub confidentiality_limit: u64,
}

/// A cipher suite supported by rustls.
///
/// This type carries both configuration and implementation. Compare with
/// [`CipherSuite`], which carries solely a cipher suite identifier.
#[derive(Clone, Copy, PartialEq)]
pub enum SupportedCipherSuite {
    /// A TLS 1.2 cipher suite
    Tls12(&'static Tls12CipherSuite),
    /// A TLS 1.3 cipher suite
    Tls13(&'static Tls13CipherSuite),
}

impl SupportedCipherSuite {
    /// The cipher suite's identifier
    pub fn suite(&self) -> CipherSuite {
        self.common().suite
    }

    /// The hash function the ciphersuite uses.
    pub(crate) fn hash_provider(&self) -> &'static dyn crypto::hash::Hash {
        self.common().hash_provider
    }

    pub(crate) fn common(&self) -> &CipherSuiteCommon {
        match self {
            Self::Tls12(inner) => &inner.common,
            Self::Tls13(inner) => &inner.common,
        }
    }

    /// Return the inner `Tls13CipherSuite` for this suite, if it is a TLS1.3 suite.
    pub const fn tls13(&self) -> Option<&'static Tls13CipherSuite> {
        match self {
            Self::Tls12(_) => None,
            Self::Tls13(inner) => Some(inner),
        }
    }

    /// Return supported protocol version for the cipher suite.
    pub fn version(&self) -> &'static SupportedProtocolVersion {
        match self {
            Self::Tls12(_) => &TLS12,
            Self::Tls13(_) => &TLS13,
        }
    }

    /// Return true if this suite is usable for a key only offering `sig_alg`
    /// signatures.  This resolves to true for all TLS1.3 suites.
    pub fn usable_for_signature_algorithm(&self, _sig_alg: SignatureAlgorithm) -> bool {
        match self {
            Self::Tls13(_) => true, // no constraint expressed by ciphersuite (e.g., TLS1.3)
            Self::Tls12(inner) => inner
                .sign
                .iter()
                .any(|scheme| scheme.algorithm() == Some(_sig_alg)),
        }
    }

}

impl fmt::Debug for SupportedCipherSuite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.suite().fmt(f)
    }
}

/// Return true if `sigscheme` is usable by any of the given suites.
pub(crate) fn compatible_sigscheme_for_suites(
    sigscheme: SignatureScheme,
    common_suites: &[SupportedCipherSuite],
) -> bool {
    common_suites
        .iter()
        .any(|&suite| match sigscheme.algorithm() {
            Some(sigalg) => suite.usable_for_signature_algorithm(sigalg),
            None => suite.tls13().is_some(),
        })
}

/// Secrets for transmitting/receiving data over a TLS session.
///
/// This rustls never extracts them: providers name them in `extract_keys`
/// so that one provider builds against this rustls and upstream's.
#[non_exhaustive]
pub enum ConnectionTrafficSecrets {
    /// Secrets for the AES_128_GCM AEAD algorithm
    Aes128Gcm {
        /// AEAD Key
        key: AeadKey,
        /// Initialization vector
        iv: Iv,
    },

    /// Secrets for the AES_256_GCM AEAD algorithm
    Aes256Gcm {
        /// AEAD Key
        key: AeadKey,
        /// Initialization vector
        iv: Iv,
    },

    /// Secrets for the CHACHA20_POLY1305 AEAD algorithm
    Chacha20Poly1305 {
        /// AEAD Key
        key: AeadKey,
        /// Initialization vector
        iv: Iv,
    },
}

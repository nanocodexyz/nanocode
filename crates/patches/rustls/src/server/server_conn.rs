use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::fmt::{Debug, Formatter};
use core::marker::PhantomData;

use std::io;

use pki_types::{DnsName, UnixTime};

use super::hs;
use crate::WantsVerifier;
use crate::builder::ConfigBuilder;
use crate::common_state::{CommonState, Side};
use crate::conn::ConnectionCore;
use crate::crypto::CryptoProvider;
use crate::enums::{CertificateType, CipherSuite, ProtocolVersion, SignatureScheme};
use crate::error::Error;
use crate::msgs::base::Payload;
use crate::msgs::handshake::ProtocolName;
use crate::sync::Arc;
use crate::time_provider::DefaultTimeProvider;
use crate::time_provider::TimeProvider;
use crate::vecbuf::ChunkVecBuffer;
use crate::{
    DistinguishedName, KeyLog, NamedGroup, WantsVersions, sign, verify, versions,
};

/// A trait for the ability to store server session data.
///
/// The keys and values are opaque.
///
/// Inserted keys are randomly chosen by the library and have
/// no internal structure (in other words, you may rely on all
/// bits being uniformly random).  Queried keys are untrusted data.
///
/// Both the keys and values should be treated as
/// **highly sensitive data**, containing enough key material
/// to break all security of the corresponding sessions.
///
/// Implementations can be lossy (in other words, forgetting
/// key/value pairs) without any negative security consequences.
///
/// However, note that `take` **must** reliably delete a returned
/// value.  If it does not, there may be security consequences.
///
/// `put` and `take` are mutating operations; this isn't expressed
/// in the type system to allow implementations freedom in
/// how to achieve interior mutability.  `Mutex` is a common
/// choice.
pub trait StoresServerSessions: Debug + Send + Sync {
    /// Store session secrets encoded in `value` against `key`,
    /// overwrites any existing value against `key`.  Returns `true`
    /// if the value was stored.
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool;

    /// Find a value with the given `key`.  Return it, or None
    /// if it doesn't exist.
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;

    /// Find a value with the given `key`.  Return it and delete it;
    /// or None if it doesn't exist.
    fn take(&self, key: &[u8]) -> Option<Vec<u8>>;

    /// Whether the store can cache another session. This is used to indicate to clients
    /// whether their session can be resumed; the implementation is not required to remember
    /// a session even if it returns `true` here.
    fn can_cache(&self) -> bool;
}

/// A trait for the ability to encrypt and decrypt tickets.
pub trait ProducesTickets: Debug + Send + Sync {
    /// Returns true if this implementation will encrypt/decrypt
    /// tickets.  Should return false if this is a dummy
    /// implementation: the server will not send the SessionTicket
    /// extension and will not call the other functions.
    fn enabled(&self) -> bool;

    /// Returns the lifetime in seconds of tickets produced now.
    /// The lifetime is provided as a hint to clients that the
    /// ticket will not be useful after the given time.
    ///
    /// This lifetime must be implemented by key rolling and
    /// erasure, *not* by storing a lifetime in the ticket.
    ///
    /// The objective is to limit damage to forward secrecy caused
    /// by tickets, not just limiting their lifetime.
    fn lifetime(&self) -> u32;

    /// Encrypt and authenticate `plain`, returning the resulting
    /// ticket.  Return None if `plain` cannot be encrypted for
    /// some reason: an empty ticket will be sent and the connection
    /// will continue.
    fn encrypt(&self, plain: &[u8]) -> Option<Vec<u8>>;

    /// Decrypt `cipher`, validating its authenticity protection
    /// and recovering the plaintext.  `cipher` is fully attacker
    /// controlled, so this decryption must be side-channel free,
    /// panic-proof, and otherwise bullet-proof.  If the decryption
    /// fails, return None.
    fn decrypt(&self, cipher: &[u8]) -> Option<Vec<u8>>;
}

/// How to choose a certificate chain and signing key for use
/// in server authentication.
///
/// This is suitable when selecting a certificate does not require
/// I/O or when the application is using blocking I/O anyhow.
pub trait ResolvesServerCert: Debug + Send + Sync {
    /// Choose a certificate chain and matching key given simplified
    /// ClientHello information.
    ///
    /// Return `None` to abort the handshake.
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<sign::CertifiedKey>>;

    /// Return true when the server only supports raw public keys.
    fn only_raw_public_keys(&self) -> bool {
        false
    }
}

/// A struct representing the received Client Hello
#[derive(Debug)]
pub struct ClientHello<'a> {
    pub(super) server_name: &'a Option<DnsName<'a>>,
    pub(super) signature_schemes: &'a [SignatureScheme],
    pub(super) alpn: Option<&'a Vec<ProtocolName>>,
    pub(super) server_cert_types: Option<&'a [CertificateType]>,
    pub(super) client_cert_types: Option<&'a [CertificateType]>,
    pub(super) cipher_suites: &'a [CipherSuite],
    /// The [certificate_authorities] extension, if it was sent by the client.
    ///
    /// [certificate_authorities]: https://datatracker.ietf.org/doc/html/rfc8446#section-4.2.4
    pub(super) certificate_authorities: Option<&'a [DistinguishedName]>,
    pub(super) named_groups: Option<&'a [NamedGroup]>,
}

impl<'a> ClientHello<'a> {
    /// Get the server name indicator.
    ///
    /// Returns `None` if the client did not supply a SNI.
    pub fn server_name(&self) -> Option<&str> {
        self.server_name
            .as_ref()
            .map(<DnsName<'_> as AsRef<str>>::as_ref)
    }

    /// Get the compatible signature schemes.
    ///
    /// Returns standard-specified default if the client omitted this extension.
    pub fn signature_schemes(&self) -> &[SignatureScheme] {
        self.signature_schemes
    }

    /// Get the ALPN protocol identifiers submitted by the client.
    ///
    /// Returns `None` if the client did not include an ALPN extension.
    ///
    /// Application Layer Protocol Negotiation (ALPN) is a TLS extension that lets a client
    /// submit a set of identifiers that each a represent an application-layer protocol.
    /// The server will then pick its preferred protocol from the set submitted by the client.
    /// Each identifier is represented as a byte array, although common values are often ASCII-encoded.
    /// See the official RFC-7301 specifications at <https://datatracker.ietf.org/doc/html/rfc7301>
    /// for more information on ALPN.
    ///
    /// For example, a HTTP client might specify "http/1.1" and/or "h2". Other well-known values
    /// are listed in the at IANA registry at
    /// <https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids>.
    ///
    /// The server can specify supported ALPN protocols by setting [`ServerConfig::alpn_protocols`].
    /// During the handshake, the server will select the first protocol configured that the client supports.
    pub fn alpn(&self) -> Option<impl Iterator<Item = &'a [u8]>> {
        self.alpn.map(|protocols| {
            protocols
                .iter()
                .map(|proto| proto.as_ref())
        })
    }

    /// Get cipher suites.
    pub fn cipher_suites(&self) -> &[CipherSuite] {
        self.cipher_suites
    }

    /// Get the server certificate types offered in the ClientHello.
    ///
    /// Returns `None` if the client did not include a certificate type extension.
    pub fn server_cert_types(&self) -> Option<&'a [CertificateType]> {
        self.server_cert_types
    }

    /// Get the client certificate types offered in the ClientHello.
    ///
    /// Returns `None` if the client did not include a certificate type extension.
    pub fn client_cert_types(&self) -> Option<&'a [CertificateType]> {
        self.client_cert_types
    }

    /// Get the [certificate_authorities] extension sent by the client.
    ///
    /// Returns `None` if the client did not send this extension.
    ///
    /// [certificate_authorities]: https://datatracker.ietf.org/doc/html/rfc8446#section-4.2.4
    pub fn certificate_authorities(&self) -> Option<&'a [DistinguishedName]> {
        self.certificate_authorities
    }

    /// Get the [`named_groups`] extension sent by the client.
    ///
    /// This means different things in different versions of TLS:
    ///
    /// Originally it was introduced as the "[`elliptic_curves`]" extension for TLS1.2.
    /// It described the elliptic curves supported by a client for all purposes: key
    /// exchange, signature verification (for server authentication), and signing (for
    /// client auth).  Later [RFC7919] extended this to include FFDHE "named groups",
    /// but FFDHE groups in this context only relate to key exchange.
    ///
    /// In TLS1.3 it was renamed to "[`named_groups`]" and now describes all types
    /// of key exchange mechanisms, and does not relate at all to elliptic curves
    /// used for signatures.
    ///
    /// [`elliptic_curves`]: https://datatracker.ietf.org/doc/html/rfc4492#section-5.1.1
    /// [RFC7919]: https://datatracker.ietf.org/doc/html/rfc7919#section-2
    /// [`named_groups`]:https://datatracker.ietf.org/doc/html/rfc8446#section-4.2.7
    pub fn named_groups(&self) -> Option<&'a [NamedGroup]> {
        self.named_groups
    }
}

/// Common configuration for a set of server sessions.
///
/// Making one of these is cheap, though one of the inputs may be expensive: gathering trust roots
/// from the operating system to add to the [`RootCertStore`] passed to a `ClientCertVerifier`
/// builder may take on the order of a few hundred milliseconds.
///
/// These must be created via the [`ServerConfig::builder()`] or [`ServerConfig::builder_with_provider()`]
/// function.
///
/// # Defaults
///
/// * [`ServerConfig::max_fragment_size`]: the default is `None` (meaning 16kB).
/// * [`ServerConfig::session_storage`]: if the `std` feature is enabled, the default stores 256
///   sessions in memory. If the `std` feature is not enabled, the default is to not store any
///   sessions. In a no-std context, by enabling the `hashbrown` feature you may provide your
///   own `session_storage` using [`ServerSessionMemoryCache`] and a `crate::lock::MakeMutex`
///   implementation.
/// * [`ServerConfig::alpn_protocols`]: the default is empty -- no ALPN protocol is negotiated.
/// * [`ServerConfig::key_log`]: key material is not logged.
/// * [`ServerConfig::send_tls13_tickets`]: 2 tickets are sent.
///
/// # Sharing resumption storage between `ServerConfig`s
///
/// In a program using many `ServerConfig`s it may improve resumption rates
/// (which has a significant impact on connection performance) if those
/// configs share [`ServerConfig::session_storage`] or [`ServerConfig::ticketer`].
///
/// However, caution is needed: other fields influence the security of a session
/// and resumption between them can be surprising.  If sharing
/// [`ServerConfig::session_storage`] or [`ServerConfig::ticketer`] between two
/// `ServerConfig`s, you should also evaluate the following fields and ensure
/// they are equivalent:
///
/// * `ServerConfig::verifier` -- client authentication requirements,
/// * [`ServerConfig::cert_resolver`] -- server identities.
///
/// To illustrate, imagine two `ServerConfig`s `A` and `B`.  `A` requires
/// client authentication, `B` does not.  If `A` and `B` shared a resumption store,
/// it would be possible for a session originated by `B` (that is, an unauthenticated client)
/// to be inserted into the store, and then resumed by `A`.  This would give a false
/// impression to the user of `A` that the client was authenticated.  This is possible
/// whether the resumption is performed statefully (via [`ServerConfig::session_storage`])
/// or statelessly (via [`ServerConfig::ticketer`]).
///
/// _Unlike_ `ClientConfig`, rustls does not enforce any policy here.
///
/// [`RootCertStore`]: crate::RootCertStore
/// [`ServerSessionMemoryCache`]: crate::server::handy::ServerSessionMemoryCache
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Source of randomness and other crypto.
    pub(super) provider: Arc<CryptoProvider>,

    /// Ignore the client's ciphersuite order. Instead,
    /// choose the top ciphersuite in the server list
    /// which is supported by the client.
    pub ignore_client_order: bool,

    /// The maximum size of plaintext input to be emitted in a single TLS record.
    /// A value of None is equivalent to the [TLS maximum] of 16 kB.
    ///
    /// rustls enforces an arbitrary minimum of 32 bytes for this field.
    /// Out of range values are reported as errors from [ServerConnection::new].
    ///
    /// Setting this value to a little less than the TCP MSS may improve latency
    /// for stream-y workloads.
    ///
    /// [TLS maximum]: https://datatracker.ietf.org/doc/html/rfc8446#section-5.1
    /// [ServerConnection::new]: crate::server::ServerConnection::new
    pub max_fragment_size: Option<usize>,

    /// How to store client sessions.
    ///
    /// See [ServerConfig#sharing-resumption-storage-between-serverconfigs]
    /// for a warning related to this field.
    pub session_storage: Arc<dyn StoresServerSessions>,

    /// How to produce tickets.
    ///
    /// See [ServerConfig#sharing-resumption-storage-between-serverconfigs]
    /// for a warning related to this field.
    pub ticketer: Arc<dyn ProducesTickets>,

    /// How to choose a server cert and key. This is usually set by
    /// [ConfigBuilder::with_single_cert] or [ConfigBuilder::with_cert_resolver].
    pub cert_resolver: Arc<dyn ResolvesServerCert>,

    /// Protocol names we support, most preferred first.
    /// If empty we don't do ALPN at all.
    pub alpn_protocols: Vec<Vec<u8>>,

    /// Supported protocol versions, in no particular order.
    /// The default is all supported versions.
    pub(super) versions: versions::EnabledVersions,

    /// How to verify client certificates.
    pub(super) verifier: Arc<dyn verify::ClientCertVerifier>,

    /// How to output key material for debugging.  The default
    /// does nothing.
    pub key_log: Arc<dyn KeyLog>,

    /// Amount of early data to accept for sessions created by
    /// this config.  Specify 0 to disable early data.  The
    /// default is 0.
    ///
    /// Read the early data via [`ServerConnection::early_data`].
    ///
    /// The units for this are _both_ plaintext bytes, _and_ ciphertext
    /// bytes, depending on whether the server accepts a client's early_data
    /// or not.  It is therefore recommended to include some slop in
    /// this value to account for the unknown amount of ciphertext
    /// expansion in the latter case.
    pub max_early_data_size: u32,

    /// Whether the server should send "0.5RTT" data.  This means the server
    /// sends data after its first flight of handshake messages, without
    /// waiting for the client to complete the handshake.
    ///
    /// This can improve TTFB latency for either server-speaks-first protocols,
    /// or client-speaks-first protocols when paired with "0RTT" data.  This
    /// comes at the cost of a subtle weakening of the normal handshake
    /// integrity guarantees that TLS provides.  Note that the initial
    /// `ClientHello` is indirectly authenticated because it is included
    /// in the transcript used to derive the keys used to encrypt the data.
    ///
    /// This only applies to TLS1.3 connections.  TLS1.2 connections cannot
    /// do this optimisation and this setting is ignored for them.  It is
    /// also ignored for TLS1.3 connections that even attempt client
    /// authentication.
    ///
    /// This defaults to false.  This means the first application data
    /// sent by the server comes after receiving and validating the client's
    /// handshake up to the `Finished` message.  This is the safest option.
    pub send_half_rtt_data: bool,

    /// How many TLS1.3 tickets to send immediately after a successful
    /// handshake.
    ///
    /// Because TLS1.3 tickets are single-use, this allows
    /// a client to perform multiple resumptions.
    ///
    /// The default is 2.
    ///
    /// If this is 0, no tickets are sent and clients will not be able to
    /// do any resumption.
    pub send_tls13_tickets: usize,

    /// Upper bound on the number of TLS 1.3 tickets sent in response to a
    /// client [RFC 9149] `ticket_request` extension.
    ///
    /// A client requesting `n` tickets receives `min(n, max_tls13_tickets)`.
    /// Set to 0 to ignore client requests entirely (clients get
    /// `send_tls13_tickets` regardless).
    ///
    /// The default is 0 (extension is ignored, preserving current behavior).
    ///
    /// [RFC 9149]: https://datatracker.ietf.org/doc/html/rfc9149
    pub max_tls13_tickets: usize,

    /// If set to `true`, requires the client to support the extended
    /// master secret extraction method defined in [RFC 7627].
    ///
    /// The default is `true` if the configured [`CryptoProvider`] is
    /// FIPS-compliant (i.e., [`CryptoProvider::fips()`] returns `true`),
    /// `false` otherwise.
    ///
    /// It must be set to `true` to meet FIPS requirement mentioned in section
    /// **D.Q Transition of the TLS 1.2 KDF to Support the Extended Master
    /// Secret** from [FIPS 140-3 IG.pdf].
    ///
    /// [RFC 7627]: https://datatracker.ietf.org/doc/html/rfc7627
    /// [FIPS 140-3 IG.pdf]: https://csrc.nist.gov/csrc/media/Projects/cryptographic-module-validation-program/documents/fips%20140-3/FIPS%20140-3%20IG.pdf
    pub require_ems: bool,

    /// Provides the current system time
    pub time_provider: Arc<dyn TimeProvider>,
}

impl ServerConfig {
    /// Create a builder for a server configuration with
    /// [the process-default `CryptoProvider`][CryptoProvider#using-the-per-process-default-cryptoprovider]
    /// and safe protocol version defaults.
    ///
    /// For more information, see the [`ConfigBuilder`] documentation.
    pub fn builder() -> ConfigBuilder<Self, WantsVerifier> {
        Self::builder_with_protocol_versions(versions::DEFAULT_VERSIONS)
    }

    /// Create a builder for a server configuration with
    /// [the process-default `CryptoProvider`][CryptoProvider#using-the-per-process-default-cryptoprovider]
    /// and the provided protocol versions.
    ///
    /// Panics if
    /// - the supported versions are not compatible with the provider (eg.
    ///   the combination of ciphersuites supported by the provider and supported
    ///   versions lead to zero cipher suites being usable),
    /// - if a `CryptoProvider` cannot be resolved using a combination of
    ///   the crate features and process default.
    ///
    /// For more information, see the [`ConfigBuilder`] documentation.
    pub fn builder_with_protocol_versions(
        versions: &[&'static versions::SupportedProtocolVersion],
    ) -> ConfigBuilder<Self, WantsVerifier> {
        // Safety assumptions:
        // 1. that the provider has been installed (explicitly or implicitly)
        // 2. that the process-level default provider is usable with the supplied protocol versions.
        Self::builder_with_provider(
            CryptoProvider::get_default_or_install_from_crate_features().clone(),
        )
        .with_protocol_versions(versions)
        .unwrap()
    }

    /// Create a builder for a server configuration with a specific [`CryptoProvider`].
    ///
    /// This will use the provider's configured ciphersuites. You must additionally choose
    /// which protocol versions to enable, using `with_protocol_versions` or
    /// `with_safe_default_protocol_versions` and handling the `Result` in case a protocol
    /// version is not supported by the provider's ciphersuites.
    ///
    /// For more information, see the [`ConfigBuilder`] documentation.
    pub fn builder_with_provider(
        provider: Arc<CryptoProvider>,
    ) -> ConfigBuilder<Self, WantsVersions> {
        ConfigBuilder {
            state: WantsVersions {},
            provider,
            time_provider: Arc::new(DefaultTimeProvider),
            side: PhantomData,
        }
    }

    /// Create a builder for a server configuration with no default implementation details.
    ///
    /// This API must be used by `no_std` users.
    ///
    /// You must provide a specific [`TimeProvider`].
    ///
    /// You must provide a specific [`CryptoProvider`].
    ///
    /// This will use the provider's configured ciphersuites. You must additionally choose
    /// which protocol versions to enable, using `with_protocol_versions` or
    /// `with_safe_default_protocol_versions` and handling the `Result` in case a protocol
    /// version is not supported by the provider's ciphersuites.
    ///
    /// For more information, see the [`ConfigBuilder`] documentation.
    pub fn builder_with_details(
        provider: Arc<CryptoProvider>,
        time_provider: Arc<dyn TimeProvider>,
    ) -> ConfigBuilder<Self, WantsVersions> {
        ConfigBuilder {
            state: WantsVersions {},
            provider,
            time_provider,
            side: PhantomData,
        }
    }

    /// Return the crypto provider used to construct this client configuration.
    pub fn crypto_provider(&self) -> &Arc<CryptoProvider> {
        &self.provider
    }

    /// We support a given TLS version if it's quoted in the configured
    /// versions *and* at least one ciphersuite for this version is
    /// also configured.
    pub(crate) fn supports_version(&self, v: ProtocolVersion) -> bool {
        self.versions.contains(v)
            && self
                .provider
                .cipher_suites
                .iter()
                .any(|cs| cs.version().version == v)
    }

    pub(super) fn current_time(&self) -> Result<UnixTime, Error> {
        self.time_provider
            .current_time()
            .ok_or(Error::FailedToGetCurrentTime)
    }
}

mod connection {
    use core::fmt;
    use core::fmt::{Debug, Formatter};
    use core::ops::{Deref, DerefMut};
    use std::io;

    use super::{EarlyDataState, ServerConfig, ServerConnectionData};
    use crate::conn::{ConnectionCommon, ConnectionCore};
    use crate::error::Error;
    use crate::sync::Arc;

    /// Allows reading of early data in resumed TLS1.3 connections.
    ///
    /// "Early data" is also known as "0-RTT data".
    ///
    /// This structure implements [`std::io::Read`].
    pub struct ReadEarlyData<'a> {
        early_data: &'a mut EarlyDataState,
    }

    impl<'a> ReadEarlyData<'a> {
        fn new(early_data: &'a mut EarlyDataState) -> Self {
            ReadEarlyData { early_data }
        }
    }

    impl io::Read for ReadEarlyData<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.early_data.read(buf)
        }

    }

    /// This represents a single TLS server connection.
    ///
    /// Send TLS-protected data to the peer using the `io::Write` trait implementation.
    /// Read data from the peer using the `io::Read` trait implementation.
    pub struct ServerConnection {
        pub(super) inner: ConnectionCommon<ServerConnectionData>,
    }

    impl ServerConnection {
        /// Make a new ServerConnection.  `config` controls how
        /// we behave in the TLS protocol.
        pub fn new(config: Arc<ServerConfig>) -> Result<Self, Error> {
            Ok(Self {
                inner: ConnectionCommon::from(ConnectionCore::for_server(config)?),
            })
        }

        /// Retrieves the server name, if any, used to select the certificate and
        /// private key.
        ///
        /// This returns `None` until some time after the client's server name indication
        /// (SNI) extension value is processed during the handshake. It will never be
        /// `None` when the connection is ready to send or process application data,
        /// unless the client does not support SNI.
        ///
        /// This is useful for application protocols that need to enforce that the
        /// server name matches an application layer protocol hostname. For
        /// example, HTTP/1.1 servers commonly expect the `Host:` header field of
        /// every request on a connection to match the hostname in the SNI extension
        /// when the client provides the SNI extension.
        ///
        /// The server name is also used to match sessions during session resumption.
        pub fn server_name(&self) -> Option<&str> {
            self.inner.core.get_sni_str()
        }

        /// Application-controlled portion of the resumption ticket supplied by the client, if any.
        ///
        /// Recovered from the prior session's `set_resumption_data`. Integrity is guaranteed by rustls.
        ///
        /// Returns `Some` if and only if a valid resumption ticket has been received from the client.
        pub fn received_resumption_data(&self) -> Option<&[u8]> {
            self.inner
                .core
                .data
                .received_resumption_data
                .as_ref()
                .map(|x| &x[..])
        }

        /// Set the resumption data to embed in future resumption tickets supplied to the client.
        ///
        /// Defaults to the empty byte string. Must be less than 2^15 bytes to allow room for other
        /// data. Should be called while `is_handshaking` returns true to ensure all transmitted
        /// resumption tickets are affected.
        ///
        /// Integrity will be assured by rustls, but the data will be visible to the client. If secrecy
        /// from the client is desired, encrypt the data separately.
        pub fn set_resumption_data(&mut self, data: &[u8]) {
            assert!(data.len() < 2usize.pow(15));
            self.inner.core.data.resumption_data = data.into();
        }

        /// Explicitly discard early data, notifying the client
        ///
        /// Useful if invariants encoded in `received_resumption_data()` cannot be respected.
        ///
        /// Must be called while `is_handshaking` is true.
        pub fn reject_early_data(&mut self) {
            self.inner.core.reject_early_data()
        }

        /// Returns an `io::Read` implementer you can read bytes from that are
        /// received from a client as TLS1.3 0RTT/"early" data, during the handshake.
        ///
        /// This returns `None` in many circumstances, such as :
        ///
        /// - Early data is disabled if [`ServerConfig::max_early_data_size`] is zero (the default).
        /// - The session negotiated with the client is not TLS1.3.
        /// - The client just doesn't support early data.
        /// - The connection doesn't resume an existing session.
        /// - The client hasn't sent a full ClientHello yet.
        pub fn early_data(&mut self) -> Option<ReadEarlyData<'_>> {
            let data = &mut self.inner.core.data;
            if data.early_data.was_accepted() {
                Some(ReadEarlyData::new(&mut data.early_data))
            } else {
                None
            }
        }

    }

    impl Debug for ServerConnection {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            f.debug_struct("ServerConnection")
                .finish()
        }
    }

    impl Deref for ServerConnection {
        type Target = ConnectionCommon<ServerConnectionData>;

        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    impl DerefMut for ServerConnection {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }

    impl From<ServerConnection> for crate::Connection {
        fn from(conn: ServerConnection) -> Self {
            Self::Server(conn)
        }
    }

}

pub use connection::{ReadEarlyData, ServerConnection};

#[derive(Default)]
pub(super) enum EarlyDataState {
    #[default]
    New,
    Accepted {
        received: ChunkVecBuffer,
        left: usize,
    },
    Rejected,
}

impl EarlyDataState {
    pub(super) fn reject(&mut self) {
        *self = Self::Rejected;
    }

    pub(super) fn accept(&mut self, max_size: usize) {
        *self = Self::Accepted {
            received: ChunkVecBuffer::new(Some(max_size)),
            left: max_size,
        };
    }

    fn was_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    pub(super) fn was_rejected(&self) -> bool {
        matches!(self, Self::Rejected)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Accepted { received, .. } => received.read(buf),
            _ => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        }
    }

    pub(super) fn take_received_plaintext(&mut self, bytes: Payload<'_>) -> bool {
        let available = bytes.bytes().len();
        let Self::Accepted { received, left } = self else {
            return false;
        };

        if received.apply_limit(available) != available || available > *left {
            return false;
        }

        received.append(bytes.into_vec());
        *left -= available;
        true
    }
}

impl Debug for EarlyDataState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::New => write!(f, "EarlyDataState::New"),
            Self::Accepted { received, left } => write!(
                f,
                "EarlyDataState::Accepted {{ received: {}, left: {} }}",
                received.len(),
                left
            ),
            Self::Rejected => write!(f, "EarlyDataState::Rejected"),
        }
    }
}

impl ConnectionCore<ServerConnectionData> {
    pub(crate) fn for_server(
        config: Arc<ServerConfig>,
    ) -> Result<Self, Error> {
        let mut common = CommonState::new(Side::Server);
        common.set_max_fragment_size(config.max_fragment_size)?;
        Ok(Self::new(
            Box::new(hs::ExpectClientHello::new(config)),
            ServerConnectionData::default(),
            common,
        ))
    }

    pub(crate) fn reject_early_data(&mut self) {
        assert!(
            self.common_state.is_handshaking(),
            "cannot retroactively reject early data"
        );
        self.data.early_data.reject();
    }

    pub(crate) fn get_sni_str(&self) -> Option<&str> {
        self.data.get_sni_str()
    }
}

/// State associated with a server connection.
#[derive(Default, Debug)]
pub struct ServerConnectionData {
    pub(super) sni: Option<DnsName<'static>>,
    pub(super) received_resumption_data: Option<Vec<u8>>,
    pub(super) resumption_data: Vec<u8>,
    pub(super) early_data: EarlyDataState,
}

impl ServerConnectionData {
    pub(super) fn get_sni_str(&self) -> Option<&str> {
        self.sni.as_ref().map(AsRef::as_ref)
    }
}

impl crate::conn::SideData for ServerConnectionData {}


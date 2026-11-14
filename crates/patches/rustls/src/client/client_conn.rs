use alloc::vec::Vec;
use core::marker::PhantomData;

use core::fmt;

use pki_types::{ServerName, UnixTime};

use super::handy::NoClientSessionStorage;
use super::hs::{self, ClientHelloInput};
use crate::WantsVerifier;
use crate::builder::ConfigBuilder;
use crate::common_state::{CommonState, Side};
use crate::conn::ConnectionCore;
use crate::crypto::{CryptoProvider, SupportedKxGroup};
use crate::enums::{CipherSuite, ProtocolVersion, SignatureScheme};
use crate::error::Error;
use crate::msgs::enums::NamedGroup;
use crate::msgs::handshake::ClientExtensionsInput;
use crate::msgs::persist;
use crate::suites::SupportedCipherSuite;
use crate::sync::Arc;
use crate::time_provider::DefaultTimeProvider;
use crate::time_provider::TimeProvider;
use crate::{KeyLog, WantsVersions, sign, verify, versions};

/// A trait for the ability to store client session data, so that sessions
/// can be resumed in future connections.
///
/// Generally all data in this interface should be treated as
/// **highly sensitive**, containing enough key material to break all security
/// of the corresponding session.
///
/// `set_`, `insert_`, `remove_` and `take_` operations are mutating; this isn't
/// expressed in the type system to allow implementations freedom in
/// how to achieve interior mutability.  `Mutex` is a common choice.
pub trait ClientSessionStore: fmt::Debug + Send + Sync {
    /// Remember what `NamedGroup` the given server chose.
    fn set_kx_hint(&self, server_name: ServerName<'static>, group: NamedGroup);

    /// This should return the value most recently passed to `set_kx_hint`
    /// for the given `server_name`.
    ///
    /// If `None` is returned, the caller chooses the first configured group,
    /// and an extra round trip might happen if that choice is unsatisfactory
    /// to the server.
    fn kx_hint(&self, server_name: &ServerName<'_>) -> Option<NamedGroup>;

    /// Remember a TLS1.2 session.
    ///
    /// At most one of these can be remembered at a time, per `server_name`.
    fn set_tls12_session(
        &self,
        server_name: ServerName<'static>,
        value: persist::Tls12ClientSessionValue,
    );

    /// Get the most recently saved TLS1.2 session for `server_name` provided to `set_tls12_session`.
    fn tls12_session(
        &self,
        server_name: &ServerName<'_>,
    ) -> Option<persist::Tls12ClientSessionValue>;

    /// Remove and forget any saved TLS1.2 session for `server_name`.
    fn remove_tls12_session(&self, server_name: &ServerName<'static>);

    /// Remember a TLS1.3 ticket that might be retrieved later from `take_tls13_ticket`, allowing
    /// resumption of this session.
    ///
    /// This can be called multiple times for a given session, allowing multiple independent tickets
    /// to be valid at once.  The number of times this is called is controlled by the server, so
    /// implementations of this trait should apply a reasonable bound of how many items are stored
    /// simultaneously.
    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        value: persist::Tls13ClientSessionValue,
    );

    /// Return a TLS1.3 ticket previously provided to `add_tls13_ticket`.
    ///
    /// Implementations of this trait must return each value provided to `add_tls13_ticket` _at most once_.
    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<persist::Tls13ClientSessionValue>;
}

/// A trait for the ability to choose a certificate chain and
/// private key for the purposes of client authentication.
pub trait ResolvesClientCert: fmt::Debug + Send + Sync {
    /// Resolve a client certificate chain/private key to use as the client's
    /// identity.
    ///
    /// `root_hint_subjects` is an optional list of certificate authority
    /// subject distinguished names that the client can use to help
    /// decide on a client certificate the server is likely to accept. If
    /// the list is empty, the client should send whatever certificate it
    /// has. The hints are expected to be DER-encoded X.500 distinguished names,
    /// per [RFC 5280 A.1]. See [`DistinguishedName`] for more information
    /// on decoding with external crates like `x509-parser`.
    ///
    /// `sigschemes` is the list of the [`SignatureScheme`]s the server
    /// supports.
    ///
    /// Return `None` to continue the handshake without any client
    /// authentication.  The server may reject the handshake later
    /// if it requires authentication.
    ///
    /// [RFC 5280 A.1]: https://www.rfc-editor.org/rfc/rfc5280#appendix-A.1
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<sign::CertifiedKey>>;

    /// Return true if the client only supports raw public keys.
    ///
    /// See [RFC 7250](https://www.rfc-editor.org/rfc/rfc7250).
    fn only_raw_public_keys(&self) -> bool {
        false
    }

    /// Return true if any certificates at all are available.
    fn has_certs(&self) -> bool;
}

/// Common configuration for (typically) all connections made by a program.
///
/// Making one of these is cheap, though one of the inputs may be expensive: gathering trust roots
/// from the operating system to add to the [`RootCertStore`] passed to `with_root_certificates()`
/// (the rustls-native-certs crate is often used for this) may take on the order of a few hundred
/// milliseconds.
///
/// These must be created via the [`ClientConfig::builder()`] or [`ClientConfig::builder_with_provider()`]
/// function.
///
/// # Defaults
///
/// * [`ClientConfig::max_fragment_size`]: the default is `None` (meaning 16kB).
/// * [`ClientConfig::resumption`]: supports resumption with up to 256 server names, using session
///   ids or tickets, with a max of eight tickets per server.
/// * [`ClientConfig::alpn_protocols`]: the default is empty -- no ALPN protocol is negotiated.
/// * [`ClientConfig::key_log`]: key material is not logged.
///
/// [`RootCertStore`]: crate::RootCertStore
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Which ALPN protocols we include in our client hello.
    /// If empty, no ALPN extension is sent.
    pub alpn_protocols: Vec<Vec<u8>>,

    /// Whether to check the selected ALPN was offered.
    ///
    /// The default is true.
    pub check_selected_alpn: bool,

    /// How and when the client can resume a previous session.
    ///
    /// # Sharing `resumption` between `ClientConfig`s
    /// In a program using many `ClientConfig`s it may improve resumption rates
    /// (which has a significant impact on connection performance) if those
    /// configs share a single `Resumption`.
    ///
    /// However, resumption is only allowed between two `ClientConfig`s if their
    /// `client_auth_cert_resolver` (ie, potential client authentication credentials)
    /// and `verifier` (ie, server certificate verification settings) are
    /// the same (according to `Arc::ptr_eq`).
    ///
    /// To illustrate, imagine two `ClientConfig`s `A` and `B`.  `A` fully validates
    /// the server certificate, `B` does not.  If `A` and `B` shared a resumption store,
    /// it would be possible for a session originated by `B` to be inserted into the
    /// store, and then resumed by `A`.  This would give a false impression to the user
    /// of `A` that the server certificate is fully validated.
    pub resumption: Resumption,

    /// The maximum size of plaintext input to be emitted in a single TLS record.
    /// A value of None is equivalent to the [TLS maximum] of 16 kB.
    ///
    /// rustls enforces an arbitrary minimum of 32 bytes for this field.
    /// Out of range values are reported as errors from [ClientConnection::new].
    ///
    /// Setting this value to a little less than the TCP MSS may improve latency
    /// for stream-y workloads.
    ///
    /// [TLS maximum]: https://datatracker.ietf.org/doc/html/rfc8446#section-5.1
    /// [ClientConnection::new]: crate::client::ClientConnection::new
    pub max_fragment_size: Option<usize>,

    /// How to decide what client auth certificate/keys to use.
    pub client_auth_cert_resolver: Arc<dyn ResolvesClientCert>,

    /// Whether to send the Server Name Indication (SNI) extension
    /// during the client handshake.
    ///
    /// The default is true.
    pub enable_sni: bool,

    /// How to output key material for debugging.  The default
    /// does nothing.
    pub key_log: Arc<dyn KeyLog>,

    /// If set to `true`, requires the server to support the extended
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

    /// Source of randomness and other crypto.
    pub(super) provider: Arc<CryptoProvider>,

    /// Supported versions, in no particular order.  The default
    /// is all supported versions.
    pub(super) versions: versions::EnabledVersions,

    /// How to verify the server certificate chain.
    pub(super) verifier: Arc<dyn verify::ServerCertVerifier>,

    /// Request a specific number of TLS 1.3 session tickets via [RFC 9149].
    ///
    /// Set to `None` to disable sending the extension (the default).
    ///
    /// [RFC 9149]: https://datatracker.ietf.org/doc/html/rfc9149
    pub send_ticket_request: Option<TicketRequest>,
}

/// Desired session ticket counts for the RFC 9149 `ticket_request` extension.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TicketRequest {
    /// Tickets desired when the server negotiates a new connection.
    pub new_session_count: u8,
    /// Tickets desired when the server resumes using a presented ticket.
    pub resumption_count: u8,
}

impl ClientConfig {
    /// Create a builder for a client configuration with
    /// [the process-default `CryptoProvider`][CryptoProvider#using-the-per-process-default-cryptoprovider]
    /// and safe protocol version defaults.
    ///
    /// For more information, see the [`ConfigBuilder`] documentation.
    pub fn builder() -> ConfigBuilder<Self, WantsVerifier> {
        Self::builder_with_protocol_versions(versions::DEFAULT_VERSIONS)
    }

    /// Create a builder for a client configuration with
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

    /// Create a builder for a client configuration with a specific [`CryptoProvider`].
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
    /// Create a builder for a client configuration with no default implementation details.
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

    /// Access configuration options whose use is dangerous and requires
    /// extra care.
    pub fn dangerous(&mut self) -> danger::DangerousClientConfig<'_> {
        danger::DangerousClientConfig { cfg: self }
    }

    pub(super) fn needs_key_share(&self) -> bool {
        self.supports_version(ProtocolVersion::TLSv1_3)
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

    pub(super) fn find_cipher_suite(&self, suite: CipherSuite) -> Option<SupportedCipherSuite> {
        self.provider
            .cipher_suites
            .iter()
            .copied()
            .find(|&scs| scs.suite() == suite)
    }

    pub(super) fn find_kx_group(
        &self,
        group: NamedGroup,
        version: ProtocolVersion,
    ) -> Option<&'static dyn SupportedKxGroup> {
        self.provider
            .kx_groups
            .iter()
            .copied()
            .find(|skxg| skxg.usable_for_version(version) && skxg.name() == group)
    }

    pub(super) fn current_time(&self) -> Result<UnixTime, Error> {
        self.time_provider
            .current_time()
            .ok_or(Error::FailedToGetCurrentTime)
    }
}

/// Configuration for how/when a client is allowed to resume a previous session.
#[derive(Clone, Debug)]
pub struct Resumption {
    /// How we store session data or tickets. The default is to use an in-memory
    /// [super::handy::ClientSessionMemoryCache].
    pub(super) store: Arc<dyn ClientSessionStore>,

    /// What mechanism is used for resuming a TLS 1.2 session.
    pub(super) tls12_resumption: Tls12Resumption,
}

impl Resumption {
    /// Create a new `Resumption` that stores data for the given number of sessions in memory.
    ///
    /// This is the default `Resumption` choice, and enables resuming a TLS 1.2 session with
    /// a session id or RFC 5077 ticket.
    pub fn in_memory_sessions(num: usize) -> Self {
        Self {
            store: Arc::new(super::handy::ClientSessionMemoryCache::new(num)),
            tls12_resumption: Tls12Resumption::SessionIdOrTickets,
        }
    }

    /// Use a custom [`ClientSessionStore`] implementation to store sessions.
    ///
    /// By default, enables resuming a TLS 1.2 session with a session id or RFC 5077 ticket.
    pub fn store(store: Arc<dyn ClientSessionStore>) -> Self {
        Self {
            store,
            tls12_resumption: Tls12Resumption::SessionIdOrTickets,
        }
    }

    /// Disable all use of session resumption.
    pub fn disabled() -> Self {
        Self {
            store: Arc::new(NoClientSessionStorage),
            tls12_resumption: Tls12Resumption::Disabled,
        }
    }

    /// Configure whether TLS 1.2 sessions may be resumed, and by what mechanism.
    ///
    /// This is meaningless if you've disabled resumption entirely, which is the case in `no-std`
    /// contexts.
    pub fn tls12_resumption(mut self, tls12: Tls12Resumption) -> Self {
        self.tls12_resumption = tls12;
        self
    }
}

impl Default for Resumption {
    /// Create an in-memory session store resumption with up to 256 server names, allowing
    /// a TLS 1.2 session to resume with a session id or RFC 5077 ticket.
    fn default() -> Self {
        let ret = Self::in_memory_sessions(256);

        ret
    }
}

/// What mechanisms to support for resuming a TLS 1.2 session.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tls12Resumption {
    /// Disable 1.2 resumption.
    Disabled,
    /// Support 1.2 resumption using session ids only.
    SessionIdOnly,
    /// Support 1.2 resumption using session ids or RFC 5077 tickets.
    ///
    /// See[^1] for why you might like to disable RFC 5077 by instead choosing the `SessionIdOnly`
    /// option. Note that TLS 1.3 tickets do not have those issues.
    ///
    /// [^1]: <https://words.filippo.io/we-need-to-talk-about-session-tickets/>
    SessionIdOrTickets,
}

/// Container for unsafe APIs
pub(super) mod danger {
    use super::ClientConfig;
    use super::verify::ServerCertVerifier;
    use crate::sync::Arc;

    /// Accessor for dangerous configuration options.
    #[derive(Debug)]
    pub struct DangerousClientConfig<'a> {
        /// The underlying ClientConfig
        pub cfg: &'a mut ClientConfig,
    }

    impl DangerousClientConfig<'_> {
        /// Overrides the default `ServerCertVerifier` with something else.
        pub fn set_certificate_verifier(&mut self, verifier: Arc<dyn ServerCertVerifier>) {
            self.cfg.verifier = verifier;
        }
    }
}

mod connection {
    use alloc::vec::Vec;
    use core::fmt;
    use core::ops::{Deref, DerefMut};

    use pki_types::ServerName;

    use super::{ClientConnectionData, ClientExtensionsInput};
    use crate::ClientConfig;
    use crate::conn::{ConnectionCommon, ConnectionCore};
    use crate::error::Error;
    use crate::sync::Arc;

    /// This represents a single TLS client connection.
    pub struct ClientConnection {
        inner: ConnectionCommon<ClientConnectionData>,
    }

    impl fmt::Debug for ClientConnection {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ClientConnection")
                .finish()
        }
    }

    impl ClientConnection {
        /// Make a new ClientConnection.  `config` controls how
        /// we behave in the TLS protocol, `name` is the
        /// name of the server we want to talk to.
        pub fn new(config: Arc<ClientConfig>, name: ServerName<'static>) -> Result<Self, Error> {
            Self::new_with_alpn(config.clone(), name, config.alpn_protocols.clone())
        }

        /// Make a new ClientConnection with custom ALPN protocols.
        pub fn new_with_alpn(
            config: Arc<ClientConfig>,
            name: ServerName<'static>,
            alpn_protocols: Vec<Vec<u8>>,
        ) -> Result<Self, Error> {
            Ok(Self {
                inner: ConnectionCommon::from(ConnectionCore::for_client(
                    config,
                    name,
                    ClientExtensionsInput::from_alpn(alpn_protocols),
                )?),
            })
        }
        /// Returns the number of TLS1.3 tickets that have been received.
        pub fn tls13_tickets_received(&self) -> u32 {
            self.inner.tls13_tickets_received
        }
    }

    impl Deref for ClientConnection {
        type Target = ConnectionCommon<ClientConnectionData>;

        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    impl DerefMut for ClientConnection {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }

    #[doc(hidden)]
    impl<'a> TryFrom<&'a mut crate::Connection> for &'a mut ClientConnection {
        type Error = ();

        fn try_from(value: &'a mut crate::Connection) -> Result<Self, Self::Error> {
            use crate::Connection::*;
            match value {
                Client(conn) => Ok(conn),
                Server(_) => Err(()),
            }
        }
    }

    impl From<ClientConnection> for crate::Connection {
        fn from(conn: ClientConnection) -> Self {
            Self::Client(conn)
        }
    }
}
pub use connection::ClientConnection;

impl ConnectionCore<ClientConnectionData> {
    pub(crate) fn for_client(
        config: Arc<ClientConfig>,
        name: ServerName<'static>,
        extra_exts: ClientExtensionsInput,
    ) -> Result<Self, Error> {
        let mut common_state = CommonState::new(Side::Client);
        common_state.set_max_fragment_size(config.max_fragment_size)?;
        let mut data = ClientConnectionData::new();

        let mut cx = hs::ClientContext {
            common: &mut common_state,
            data: &mut data,
            // `start_handshake` won't produce plaintext
            sendable_plaintext: None,
        };

        let input = ClientHelloInput::new(name, &extra_exts, config)?;
        let state = input.start_handshake(extra_exts, &mut cx)?;
        Ok(Self::new(state, data, common_state))
    }
}

/// State associated with a client connection.
#[derive(Debug)]
pub struct ClientConnectionData {}

impl ClientConnectionData {
    fn new() -> Self {
        Self {}
    }
}

impl crate::conn::SideData for ClientConnectionData {}

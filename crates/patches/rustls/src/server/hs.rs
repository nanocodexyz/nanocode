use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec::Vec;

use pki_types::DnsName;

use super::server_conn::ServerConnectionData;
use super::tls12;
use crate::common_state::{KxState, State};
use crate::conn::ConnectionRandoms;
use crate::crypto::SupportedKxGroup;
use crate::enums::{
    AlertDescription, CipherSuite, HandshakeType, ProtocolVersion,
    SignatureAlgorithm, SignatureScheme,
};
use crate::error::{Error, PeerIncompatible, PeerMisbehaved};
use crate::hash_hs::{HandshakeHash, HandshakeHashBuffer};
use crate::log::{debug, trace};
use crate::msgs::enums::{Compression, NamedGroup};
use crate::msgs::handshake::SessionId;
use crate::msgs::handshake::{
    ClientHelloPayload, HandshakePayload, ProtocolName, Random,
    ServerExtensions, ServerNamePayload, SingleProtocolName,
};
use crate::msgs::message::{Message, MessagePayload};
use crate::msgs::persist;
use crate::server::common::ActiveCertifiedKey;
use crate::server::{ClientHello, ServerConfig, tls13};
use crate::sync::Arc;
use crate::{SupportedCipherSuite, suites};

pub(super) type NextState<'a> = Box<dyn State<ServerConnectionData> + 'a>;
pub(super) type NextStateOrError<'a> = Result<NextState<'a>, Error>;
pub(super) type ServerContext<'a> = crate::common_state::Context<'a, ServerConnectionData>;

pub(super) fn can_resume(
    suite: SupportedCipherSuite,
    sni: &Option<DnsName<'_>>,
    using_ems: bool,
    resumedata: &persist::ServerSessionValue,
) -> bool {
    // The RFCs underspecify what happens if we try to resume to
    // an unoffered/varying suite.  We merely don't resume in weird cases.
    //
    // RFC 6066 says "A server that implements this extension MUST NOT accept
    // the request to resume the session if the server_name extension contains
    // a different name. Instead, it proceeds with a full handshake to
    // establish a new session."
    //
    // RFC 8446: "The server MUST ensure that it selects
    // a compatible PSK (if any) and cipher suite."
    resumedata.cipher_suite == suite.suite()
        && (resumedata.extended_ms == using_ems || (resumedata.extended_ms && !using_ems))
        && &resumedata.sni == sni
}

#[derive(Default)]
pub(super) struct ExtensionProcessing {
    // extensions to reply with
    pub(super) extensions: Box<ServerExtensions>,
    pub(super) send_ticket: bool,
}

impl ExtensionProcessing {
    pub(super) fn process_common(
        &mut self,
        config: &ServerConfig,
        cx: &mut ServerContext<'_>,
        ocsp_response: &mut Option<&[u8]>,
        hello: &ClientHelloPayload,
        resumedata: Option<&persist::ServerSessionValue>,
    ) -> Result<(), Error> {
        // ALPN
        let our_protocols = &config.alpn_protocols;
        if let Some(their_protocols) = &hello.protocols {
            cx.common.alpn_protocol = our_protocols
                .iter()
                .find(|ours| {
                    their_protocols
                        .iter()
                        .any(|theirs| theirs.as_ref() == ours.as_slice())
                })
                .map(|bytes| ProtocolName::from(bytes.clone()));
            if let Some(selected_protocol) = &cx.common.alpn_protocol {
                debug!("Chosen ALPN protocol {selected_protocol:?}");

                self.extensions.selected_protocol =
                    Some(SingleProtocolName::new(selected_protocol.clone()));
            } else if !our_protocols.is_empty() {
                return Err(cx.common.send_fatal_alert(
                    AlertDescription::NoApplicationProtocol,
                    Error::NoApplicationProtocol,
                ));
            }
        }

        let for_resume = resumedata.is_some();
        // SNI
        if let (false, Some(ServerNamePayload::SingleDnsName(_))) = (for_resume, &hello.server_name)
        {
            self.extensions.server_name_ack = Some(());
        }

        // Send status_request response if we have one.  This is not allowed
        // if we're resuming, and is only triggered if we have an OCSP response
        // to send.
        if !for_resume
            && hello
                .certificate_status_request
                .is_some()
        {
            if ocsp_response.is_some() && !cx.common.is_tls13() {
                // Only TLS1.2 sends confirmation in ServerHello
                self.extensions
                    .certificate_status_request_ack = Some(());
            }
        } else {
            // Throw away any OCSP response so we don't try to send it later.
            ocsp_response.take();
        }

        Ok(())
    }

    pub(super) fn process_tls12(
        &mut self,
        config: &ServerConfig,
        hello: &ClientHelloPayload,
        using_ems: bool,
    ) {
        // Renegotiation.
        // (We don't do reneg at all, but would support the secure version if we did.)

        use crate::msgs::base::PayloadU8;
        let secure_reneg_offered = hello.renegotiation_info.is_some()
            || hello
                .cipher_suites
                .contains(&CipherSuite::TLS_EMPTY_RENEGOTIATION_INFO_SCSV);

        if secure_reneg_offered {
            self.extensions.renegotiation_info = Some(PayloadU8::new(Vec::new()));
        }

        // Tickets:
        // If we get any SessionTicket extension and have tickets enabled,
        // we send an ack.
        if hello.session_ticket.is_some() && config.ticketer.enabled() {
            self.send_ticket = true;
            self.extensions.session_ticket_ack = Some(());
        }

        // Confirm use of EMS if offered.
        if using_ems {
            self.extensions
                .extended_master_secret_ack = Some(());
        }
    }

}

pub(super) struct ExpectClientHello {
    pub(super) config: Arc<ServerConfig>,
    pub(super) transcript: HandshakeHashOrBuffer,
    pub(super) session_id: SessionId,
    pub(super) using_ems: bool,
    pub(super) previous_hello: Option<PreviousClientHello>,
    pub(super) send_tickets: usize,
}

impl ExpectClientHello {
    pub(super) fn new(
        config: Arc<ServerConfig>,
    ) -> Self {
        let mut transcript_buffer = HandshakeHashBuffer::new();

        if config.verifier.offer_client_auth() {
            transcript_buffer.set_client_auth_enabled();
        }

        Self {
            config,
            transcript: HandshakeHashOrBuffer::Buffer(transcript_buffer),
            session_id: SessionId::empty(),
            using_ems: false,
            previous_hello: None,
            send_tickets: 0,
        }
    }

    /// Continues handling of a `ClientHello` message once config and certificate are available.
    pub(super) fn with_certified_key(
        self,
        mut sig_schemes: Vec<SignatureScheme>,
        client_hello: &ClientHelloPayload,
        m: &Message<'_>,
        cx: &mut ServerContext<'_>,
    ) -> NextStateOrError<'static> {
        let tls13_enabled = self
            .config
            .supports_version(ProtocolVersion::TLSv1_3);
        let tls12_enabled = self
            .config
            .supports_version(ProtocolVersion::TLSv1_2)
            && !self.previous_hello.is_some();

        // Are we doing TLS1.3?
        let version = if let Some(versions) = &client_hello.supported_versions {
            if versions.tls13 && tls13_enabled {
                ProtocolVersion::TLSv1_3
            } else if !versions.tls12 || !tls12_enabled {
                return Err(cx.common.send_fatal_alert(
                    AlertDescription::ProtocolVersion,
                    PeerIncompatible::Tls12NotOfferedOrEnabled,
                ));
            } else {
                ProtocolVersion::TLSv1_2
            }
        } else if u16::from(client_hello.client_version) < u16::from(ProtocolVersion::TLSv1_2) {
            return Err(cx.common.send_fatal_alert(
                AlertDescription::ProtocolVersion,
                PeerIncompatible::Tls12NotOffered,
            ));
        } else if !tls12_enabled && tls13_enabled {
            return Err(cx.common.send_fatal_alert(
                AlertDescription::ProtocolVersion,
                PeerIncompatible::SupportedVersionsExtensionRequired,
            ));
        } else {
            ProtocolVersion::TLSv1_2
        };

        cx.common.negotiated_version = Some(version);

        // We communicate to the upper layer what kind of key they should choose
        // via the sigschemes value.  Clients tend to treat this extension
        // orthogonally to offered ciphersuites (even though, in TLS1.2 it is not).
        // So: reduce the offered sigschemes to those compatible with the
        // intersection of ciphersuites.
        let client_suites = self
            .config
            .provider
            .cipher_suites
            .iter()
            .copied()
            .filter(|scs| {
                client_hello
                    .cipher_suites
                    .contains(&scs.suite())
            })
            .collect::<Vec<_>>();

        sig_schemes
            .retain(|scheme| suites::compatible_sigscheme_for_suites(*scheme, &client_suites));

        // We adhere to the TLS 1.2 RFC by not exposing this to the cert resolver if TLS version is 1.2
        let certificate_authorities = match version {
            ProtocolVersion::TLSv1_2 => None,
            _ => client_hello
                .certificate_authority_names
                .as_deref(),
        };
        // Choose a certificate.
        let certkey = {
            let client_hello = ClientHello {
                server_name: &cx.data.sni,
                signature_schemes: &sig_schemes,
                alpn: client_hello.protocols.as_ref(),
                cipher_suites: &client_hello.cipher_suites,
                certificate_authorities,
                named_groups: client_hello.named_groups.as_deref(),
            };
            trace!("Resolving server certificate: {client_hello:#?}");

            let certkey = self
                .config
                .cert_resolver
                .resolve(client_hello);

            certkey.ok_or_else(|| {
                cx.common.send_fatal_alert(
                    AlertDescription::AccessDenied,
                    Error::General("no server certificate chain resolved".to_owned()),
                )
            })?
        };
        let certkey = ActiveCertifiedKey::from_certified_key(&certkey);

        let (suite, skxg) = self
            .choose_suite_and_kx_group(
                version,
                certkey.get_key().algorithm(),
                client_hello
                    .named_groups
                    .as_deref()
                    .unwrap_or_default(),
                &client_hello.cipher_suites,
            )
            .map_err(|incompat| {
                cx.common
                    .send_fatal_alert(AlertDescription::HandshakeFailure, incompat)
            })?;

        // RFC 9846 section 4.2.4: the server must negotiate the same cipher suite it
        // named in its HelloRetryRequest
        if let Some(PreviousClientHello {
            suite: before_retry,
            ..
        }) = self.previous_hello
        {
            if before_retry != suite.suite() {
                return Err(cx.common.send_fatal_alert(
                    AlertDescription::IllegalParameter,
                    PeerMisbehaved::CipherSuiteDifferedOnRetry,
                ));
            }
        }

        debug!("decided upon suite {suite:?}");
        cx.common.suite = Some(suite);
        cx.common.kx_state = KxState::Start(skxg);

        // Start handshake hash.
        let starting_hash = suite.hash_provider();
        let transcript = match self.transcript {
            HandshakeHashOrBuffer::Buffer(inner) => inner.start_hash(starting_hash),
            HandshakeHashOrBuffer::Hash(inner)
                if inner.algorithm() == starting_hash.algorithm() =>
            {
                inner
            }
            _ => {
                return Err(cx.common.send_fatal_alert(
                    AlertDescription::IllegalParameter,
                    PeerMisbehaved::HandshakeHashVariedAfterRetry,
                ));
            }
        };

        // Save their Random.
        let randoms = ConnectionRandoms::new(
            client_hello.random,
            Random::new(self.config.provider.secure_random)?,
        );
        match suite {
            SupportedCipherSuite::Tls13(suite) => tls13::CompleteClientHelloHandling {
                config: self.config,
                transcript,
                suite,
                randoms,
                previous_hello: self.previous_hello,
                send_tickets: self.send_tickets,
            }
            .handle_client_hello(cx, certkey, m, client_hello, skxg, sig_schemes),
            SupportedCipherSuite::Tls12(suite) => tls12::CompleteClientHelloHandling {
                config: self.config,
                transcript,
                session_id: self.session_id,
                suite,
                using_ems: self.using_ems,
                randoms,
                send_ticket: self.send_tickets > 0,
            }
            .handle_client_hello(
                cx,
                certkey,
                m,
                client_hello,
                skxg,
                sig_schemes,
                tls13_enabled,
            ),
        }
    }

    fn choose_suite_and_kx_group(
        &self,
        selected_version: ProtocolVersion,
        sig_key_algorithm: SignatureAlgorithm,
        client_groups: &[NamedGroup],
        client_suites: &[CipherSuite],
    ) -> Result<(SupportedCipherSuite, &'static dyn SupportedKxGroup), PeerIncompatible> {
        let Some(skxg) = client_groups.iter().find_map(|offered_group| {
            self.config
                .provider
                .kx_groups
                .iter()
                .find(|skxg| {
                    skxg.usable_for_version(selected_version) && skxg.name() == *offered_group
                })
        }) else {
            return Err(PeerIncompatible::NoKxGroupsInCommon);
        };

        let mut suitable_suites_iter = self
            .config
            .provider
            .cipher_suites
            .iter()
            .filter(|suite| {
                // Reduce our supported ciphersuites by the certified key's algorithm.
                suite.usable_for_signature_algorithm(sig_key_algorithm)
                // And version
                && suite.version().version == selected_version
            });

        let suite = if self.config.ignore_client_order {
            suitable_suites_iter.find(|suite| client_suites.contains(&suite.suite()))
        } else {
            let suitable_suites = suitable_suites_iter.collect::<Vec<_>>();
            client_suites
                .iter()
                .find_map(|client_suite| {
                    suitable_suites
                        .iter()
                        .find(|x| *client_suite == x.suite())
                })
                .copied()
        }
        .ok_or(PeerIncompatible::NoCipherSuitesInCommon)?;

        Ok((*suite, *skxg))
    }
}

pub(super) struct PreviousClientHello {
    pub(super) offered_psk: bool,
    pub(super) suite: CipherSuite,
}

impl State<ServerConnectionData> for ExpectClientHello {
    fn handle<'m>(
        self: Box<Self>,
        cx: &mut ServerContext<'_>,
        m: Message<'m>,
    ) -> NextStateOrError<'m>
    where
        Self: 'm,
    {
        let (client_hello, sig_schemes) =
            process_client_hello(&m, self.previous_hello.is_some(), cx)?;
        self.with_certified_key(sig_schemes, client_hello, &m, cx)
    }

    fn into_owned(self: Box<Self>) -> NextState<'static> {
        self
    }
}

/// Configuration-independent validation of a `ClientHello` message.
///
/// This represents the first part of the `ClientHello` handling, where we do all validation that
/// doesn't depend on a `ServerConfig` being available and extract everything needed to build a
/// [`ClientHello`] value for a [`ResolvesServerCert`].
///
/// Note that this will modify `data.sni` even if config or certificate resolution fail.
///
/// [`ResolvesServerCert`]: crate::server::ResolvesServerCert
pub(super) fn process_client_hello<'m>(
    m: &'m Message<'m>,
    done_retry: bool,
    cx: &mut ServerContext<'_>,
) -> Result<(&'m ClientHelloPayload, Vec<SignatureScheme>), Error> {
    let client_hello =
        require_handshake_msg!(m, HandshakeType::ClientHello, HandshakePayload::ClientHello)?;
    trace!("we got a clienthello {client_hello:?}");

    if !client_hello
        .compression_methods
        .contains(&Compression::Null)
    {
        return Err(cx.common.send_fatal_alert(
            AlertDescription::IllegalParameter,
            PeerIncompatible::NullCompressionRequired,
        ));
    }

    // No handshake messages should follow this one in this flight.
    cx.common.check_aligned_handshake()?;

    // Extract and validate the SNI DNS name, if any, before giving it to
    // the cert resolver. In particular, if it is invalid then we should
    // send an Illegal Parameter alert instead of the Internal Error alert
    // (or whatever) that we'd send if this were checked later or in a
    // different way.
    //
    // [RFC6066][] specifies that literal IP addresses are illegal in
    // `ServerName`s with a `name_type` of `host_name`.
    //
    // Some clients incorrectly send such extensions: we choose to
    // successfully parse these (into `ServerNamePayload::IpAddress`)
    // but then act like the client sent no `server_name` extension.
    //
    // [RFC6066]: https://datatracker.ietf.org/doc/html/rfc6066#section-3
    let sni = match &client_hello.server_name {
        Some(ServerNamePayload::SingleDnsName(dns_name)) => Some(dns_name.to_lowercase_owned()),
        Some(ServerNamePayload::IpAddress) => None,
        Some(ServerNamePayload::Invalid) => {
            return Err(cx.common.send_fatal_alert(
                AlertDescription::IllegalParameter,
                PeerMisbehaved::ServerNameMustContainOneHostName,
            ));
        }
        None => None,
    };

    // save only the first SNI
    if let (Some(sni), false) = (&sni, done_retry) {
        // Save the SNI into the session.
        // The SNI hostname is immutable once set.
        assert!(cx.data.sni.is_none());
        cx.data.sni = Some(sni.clone());
    } else if cx.data.sni != sni {
        return Err(PeerMisbehaved::ServerNameDifferedOnRetry.into());
    }

    let sig_schemes = client_hello
        .signature_schemes
        .as_ref()
        .ok_or_else(|| {
            cx.common.send_fatal_alert(
                AlertDescription::HandshakeFailure,
                PeerIncompatible::SignatureAlgorithmsExtensionRequired,
            )
        })?;

    Ok((client_hello, sig_schemes.to_owned()))
}

pub(crate) enum HandshakeHashOrBuffer {
    Buffer(HandshakeHashBuffer),
    Hash(HandshakeHash),
}

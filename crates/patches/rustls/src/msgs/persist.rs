use alloc::vec::Vec;
use core::{cmp, fmt};

use pki_types::{DnsName, UnixTime};
use crate::zero::Zeroizing;

use crate::client::ResolvesClientCert;
use crate::enums::{CipherSuite, ProtocolVersion};
use crate::error::InvalidMessage;
use crate::msgs::base::{MaybeEmpty, PayloadU8, PayloadU16};
use crate::msgs::codec::{Codec, Reader};
use crate::msgs::handshake::SessionId;
use crate::msgs::handshake::{CertificateChain, ProtocolName};
use crate::sync::{Arc, Weak};
use crate::tls12::Tls12CipherSuite;
use crate::tls13::Tls13CipherSuite;
use crate::verify::ServerCertVerifier;

pub(crate) struct Retrieved<T> {
    pub(crate) value: T,
    retrieved_at: UnixTime,
}

impl<T> Retrieved<T> {
    pub(crate) fn new(value: T, retrieved_at: UnixTime) -> Self {
        Self {
            value,
            retrieved_at,
        }
    }

    pub(crate) fn map<M>(&self, f: impl FnOnce(&T) -> Option<&M>) -> Option<Retrieved<&M>> {
        Some(Retrieved {
            value: f(&self.value)?,
            retrieved_at: self.retrieved_at,
        })
    }
}

impl Retrieved<&Tls13ClientSessionValue> {
    pub(crate) fn obfuscated_ticket_age(&self) -> u32 {
        let age_secs = self
            .retrieved_at
            .as_secs()
            .saturating_sub(self.value.common.epoch);
        // nb. tickets have an upper age limit of ~7 days, well short of the 49 days here
        let age_millis = u32::try_from(age_secs)
            .unwrap_or(u32::MAX)
            .saturating_mul(1000);
        age_millis.wrapping_add(self.value.age_add)
    }
}

impl<T: core::ops::Deref<Target = ClientSessionCommon>> Retrieved<T> {
    pub(crate) fn has_expired(&self) -> bool {
        let common = &*self.value;
        common.lifetime_secs != 0
            && common
                .epoch
                .saturating_add(u64::from(common.lifetime_secs))
                < self.retrieved_at.as_secs()
    }
}

impl<T> core::ops::Deref for Retrieved<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

pub struct Tls13ClientSessionValue {
    suite: &'static Tls13CipherSuite,
    age_add: u32,
    pub(crate) common: ClientSessionCommon,
    quic_params: PayloadU16,
}

impl Tls13ClientSessionValue {
    pub(crate) fn new(
        suite: &'static Tls13CipherSuite,
        ticket: Arc<PayloadU16>,
        secret: &[u8],
        server_cert_chain: CertificateChain<'static>,
        server_cert_verifier: &Arc<dyn ServerCertVerifier>,
        client_creds: &Arc<dyn ResolvesClientCert>,
        time_now: UnixTime,
        lifetime_secs: u32,
        age_add: u32,
    ) -> Self {
        Self {
            suite,
            age_add,
            common: ClientSessionCommon::new(
                ticket,
                secret,
                time_now,
                lifetime_secs,
                server_cert_chain,
                server_cert_verifier,
                client_creds,
            ),
            quic_params: PayloadU16::new(Vec::new()),
        }
    }

    pub fn suite(&self) -> &'static Tls13CipherSuite {
        self.suite
    }

    #[doc(hidden)]
    /// Test only: rewind epoch by `delta` seconds.
    pub fn rewind_epoch(&mut self, delta: u32) {
        self.common.epoch -= delta as u64;
    }

    pub fn set_quic_params(&mut self, quic_params: &[u8]) {
        self.quic_params = PayloadU16::new(quic_params.to_vec());
    }

    pub fn quic_params(&self) -> Vec<u8> {
        self.quic_params.0.clone()
    }
}

impl fmt::Debug for Tls13ClientSessionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            suite,
            age_add,
            common,
            quic_params,
        } = self;
        f.debug_struct("Tls13ClientSessionValue")
            .field("suite", suite)
            .field("age_add", age_add)
            .field("common", common)
            .field("quic_params", quic_params)
            .finish()
    }
}

impl core::ops::Deref for Tls13ClientSessionValue {
    type Target = ClientSessionCommon;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

#[derive(Debug, Clone)]
pub struct Tls12ClientSessionValue {
    suite: &'static Tls12CipherSuite,
    pub(crate) session_id: SessionId,
    extended_ms: bool,
    #[doc(hidden)]
    pub(crate) common: ClientSessionCommon,
}

impl Tls12ClientSessionValue {
    pub(crate) fn new(
        suite: &'static Tls12CipherSuite,
        session_id: SessionId,
        ticket: Arc<PayloadU16>,
        master_secret: &[u8],
        server_cert_chain: CertificateChain<'static>,
        server_cert_verifier: &Arc<dyn ServerCertVerifier>,
        client_creds: &Arc<dyn ResolvesClientCert>,
        time_now: UnixTime,
        lifetime_secs: u32,
        extended_ms: bool,
    ) -> Self {
        Self {
            suite,
            session_id,
            extended_ms,
            common: ClientSessionCommon::new(
                ticket,
                master_secret,
                time_now,
                lifetime_secs,
                server_cert_chain,
                server_cert_verifier,
                client_creds,
            ),
        }
    }

    pub(crate) fn ticket(&mut self) -> Arc<PayloadU16> {
        self.common.ticket.clone()
    }

    pub(crate) fn extended_ms(&self) -> bool {
        self.extended_ms
    }

    pub(crate) fn suite(&self) -> &'static Tls12CipherSuite {
        self.suite
    }

    #[doc(hidden)]
    /// Test only: rewind epoch by `delta` seconds.
    pub fn rewind_epoch(&mut self, delta: u32) {
        self.common.epoch -= delta as u64;
    }
}

impl core::ops::Deref for Tls12ClientSessionValue {
    type Target = ClientSessionCommon;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

#[derive(Clone)]
pub struct ClientSessionCommon {
    ticket: Arc<PayloadU16>,
    secret: Zeroizing<PayloadU8>,
    epoch: u64,
    lifetime_secs: u32,
    server_cert_chain: Arc<CertificateChain<'static>>,
    server_cert_verifier: Weak<dyn ServerCertVerifier>,
    client_creds: Weak<dyn ResolvesClientCert>,
}

impl ClientSessionCommon {
    fn new(
        ticket: Arc<PayloadU16>,
        secret: &[u8],
        time_now: UnixTime,
        lifetime_secs: u32,
        server_cert_chain: CertificateChain<'static>,
        server_cert_verifier: &Arc<dyn ServerCertVerifier>,
        client_creds: &Arc<dyn ResolvesClientCert>,
    ) -> Self {
        Self {
            ticket,
            secret: Zeroizing::new(PayloadU8::new(secret.to_vec())),
            epoch: time_now.as_secs(),
            lifetime_secs: cmp::min(lifetime_secs, MAX_TICKET_LIFETIME),
            server_cert_chain: Arc::new(server_cert_chain),
            server_cert_verifier: Arc::downgrade(server_cert_verifier),
            client_creds: Arc::downgrade(client_creds),
        }
    }

    pub(crate) fn compatible_config(
        &self,
        server_cert_verifier: &Arc<dyn ServerCertVerifier>,
        client_creds: &Arc<dyn ResolvesClientCert>,
    ) -> bool {
        let same_verifier = Weak::ptr_eq(
            &Arc::downgrade(server_cert_verifier),
            &self.server_cert_verifier,
        );
        let same_creds = Weak::ptr_eq(&Arc::downgrade(client_creds), &self.client_creds);

        match (same_verifier, same_creds) {
            (true, true) => true,
            (false, _) => {
                crate::log::trace!("resumption not allowed between different ServerCertVerifiers");
                false
            }
            (_, _) => {
                crate::log::trace!(
                    "resumption not allowed between different ResolvesClientCert values"
                );
                false
            }
        }
    }

    pub(crate) fn server_cert_chain(&self) -> &CertificateChain<'static> {
        &self.server_cert_chain
    }

    pub(crate) fn secret(&self) -> &[u8] {
        self.secret.0.as_ref()
    }

    pub(crate) fn ticket(&self) -> &[u8] {
        self.ticket.0.as_ref()
    }
}

impl fmt::Debug for ClientSessionCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            ticket,
            secret: _,
            epoch,
            lifetime_secs,
            server_cert_chain,
            server_cert_verifier,
            client_creds,
        } = self;
        f.debug_struct("ClientSessionCommon")
            .field("ticket", ticket)
            .field("epoch", epoch)
            .field("lifetime_secs", lifetime_secs)
            .field("server_cert_chain", server_cert_chain)
            .field("server_cert_verifier", server_cert_verifier)
            .field("client_creds", client_creds)
            .finish_non_exhaustive()
    }
}

static MAX_TICKET_LIFETIME: u32 = 7 * 24 * 60 * 60;

// --- Server types ---
pub struct ServerSessionValue {
    pub(crate) sni: Option<DnsName<'static>>,
    pub(crate) version: ProtocolVersion,
    pub(crate) cipher_suite: CipherSuite,
    pub(crate) master_secret: Zeroizing<PayloadU8>,
    pub(crate) extended_ms: bool,
    pub(crate) client_cert_chain: Option<CertificateChain<'static>>,
    pub(crate) alpn: Option<PayloadU8>,
    pub(crate) application_data: PayloadU16,
    pub creation_time_sec: u64,
    pub(crate) age_obfuscation_offset: u32,
}

impl fmt::Debug for ServerSessionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            sni,
            version,
            cipher_suite,
            master_secret: _,
            extended_ms,
            client_cert_chain,
            alpn,
            application_data,
            creation_time_sec,
            age_obfuscation_offset,
        } = self;
        f.debug_struct("ServerSessionValue")
            .field("sni", sni)
            .field("version", version)
            .field("cipher_suite", cipher_suite)
            .field("extended_ms", extended_ms)
            .field("client_cert_chain", client_cert_chain)
            .field("alpn", alpn)
            .field("application_data", application_data)
            .field("creation_time_sec", creation_time_sec)
            .field("age_obfuscation_offset", age_obfuscation_offset)
            .finish_non_exhaustive()
    }
}

impl Codec<'_> for ServerSessionValue {
    fn encode(&self, bytes: &mut Vec<u8>) {
        if let Some(sni) = &self.sni {
            1u8.encode(bytes);
            let sni_bytes: &str = sni.as_ref();
            PayloadU8::<MaybeEmpty>::encode_slice(sni_bytes.as_bytes(), bytes);
        } else {
            0u8.encode(bytes);
        }
        self.version.encode(bytes);
        self.cipher_suite.encode(bytes);
        self.master_secret.encode(bytes);
        (u8::from(self.extended_ms)).encode(bytes);
        if let Some(chain) = &self.client_cert_chain {
            1u8.encode(bytes);
            chain.encode(bytes);
        } else {
            0u8.encode(bytes);
        }
        if let Some(alpn) = &self.alpn {
            1u8.encode(bytes);
            alpn.encode(bytes);
        } else {
            0u8.encode(bytes);
        }
        self.application_data.encode(bytes);
        self.creation_time_sec.encode(bytes);
        self.age_obfuscation_offset
            .encode(bytes);
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, InvalidMessage> {
        let has_sni = u8::read(r)?;
        let sni = if has_sni == 1 {
            let dns_name = PayloadU8::<MaybeEmpty>::read(r)?;
            let dns_name = match DnsName::try_from(dns_name.0.as_slice()) {
                Ok(dns_name) => dns_name.to_owned(),
                Err(_) => return Err(InvalidMessage::InvalidServerName),
            };

            Some(dns_name)
        } else {
            None
        };

        let v = ProtocolVersion::read(r)?;
        let cs = CipherSuite::read(r)?;
        let ms = Zeroizing::new(PayloadU8::read(r)?);
        let ems = u8::read(r)?;
        let has_ccert = u8::read(r)? == 1;
        let ccert = if has_ccert {
            Some(CertificateChain::read(r)?.into_owned())
        } else {
            None
        };
        let has_alpn = u8::read(r)? == 1;
        let alpn = if has_alpn {
            Some(PayloadU8::read(r)?)
        } else {
            None
        };
        let application_data = PayloadU16::read(r)?;
        let creation_time_sec = u64::read(r)?;
        let age_obfuscation_offset = u32::read(r)?;

        Ok(Self {
            sni,
            version: v,
            cipher_suite: cs,
            master_secret: ms,
            extended_ms: ems == 1u8,
            client_cert_chain: ccert,
            alpn,
            application_data,
            creation_time_sec,
            age_obfuscation_offset,
        })
    }
}

impl ServerSessionValue {
    pub(crate) fn new(
        sni: Option<&DnsName<'_>>,
        v: ProtocolVersion,
        cs: CipherSuite,
        ms: &[u8],
        client_cert_chain: Option<CertificateChain<'static>>,
        alpn: Option<ProtocolName>,
        application_data: Vec<u8>,
        creation_time: UnixTime,
        age_obfuscation_offset: u32,
    ) -> Self {
        Self {
            sni: sni.map(|dns| dns.to_owned()),
            version: v,
            cipher_suite: cs,
            master_secret: Zeroizing::new(PayloadU8::new(ms.to_vec())),
            extended_ms: false,
            client_cert_chain,
            alpn: alpn.map(|p| PayloadU8::new(p.as_ref().to_vec())),
            application_data: PayloadU16::new(application_data),
            creation_time_sec: creation_time.as_secs(),
            age_obfuscation_offset,
        }
    }

    pub(crate) fn set_extended_ms_used(&mut self) {
        self.extended_ms = true;
    }
}


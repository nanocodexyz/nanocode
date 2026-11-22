use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use nanocode_sync::SwapOption;
use nanocode_tls::rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};

use crate::connect::ALPN_HTTP1;

#[derive(Clone)]
pub struct Identity {
    certificates: Vec<CertificateDer<'static>>,
    key_pem: Vec<u8>,
}

impl Identity {
    pub fn from_pem(certificate_pem: &[u8], key_pem: &[u8]) -> io::Result<Self> {
        let invalid = |detail: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid client identity: {detail}"),
            )
        };
        let certificates = CertificateDer::pem_slice_iter(certificate_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| invalid(error.to_string()))?;
        if certificates.is_empty() {
            return Err(invalid("no PEM certificates found".to_owned()));
        }
        PrivateKeyDer::from_pem_slice(key_pem).map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            certificates,
            key_pem: key_pem.to_vec(),
        })
    }

    fn key(&self) -> io::Result<PrivateKeyDer<'static>> {
        PrivateKeyDer::from_pem_slice(&self.key_pem)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Identity")
            .field("certificates", &self.certificates.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct RootsBuilder {
    built_in: bool,
    anchors: RootCertStore,
    files: Vec<PathBuf>,
    identity: Option<Identity>,
}

impl Default for RootsBuilder {
    fn default() -> Self {
        Self {
            built_in: true,
            anchors: RootCertStore::empty(),
            files: Vec::new(),
            identity: None,
        }
    }
}

impl RootsBuilder {
    pub const fn built_in(&mut self, enabled: bool) {
        self.built_in = enabled;
    }

    pub fn add_pem(&mut self, pem: &[u8]) -> io::Result<()> {
        let anchors_before = self.anchors.len();
        add_pem(&mut self.anchors, pem, "configured PEM bytes")?;
        assert!(self.anchors.len() > anchors_before);
        Ok(())
    }

    pub fn add_pem_file(&mut self, path: PathBuf) {
        self.files.push(path);
    }

    pub fn identity(&mut self, identity: Identity) {
        self.identity = Some(identity);
    }

    pub fn build(self) -> io::Result<Option<Arc<Roots>>> {
        let configured =
            !self.anchors.is_empty() || !self.files.is_empty() || self.identity.is_some();
        if !configured {
            if self.built_in {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "built-in TLS roots are disabled and no root certificates are configured",
            ));
        }
        Ok(Some(Arc::new(Roots {
            built_in: self.built_in,
            anchors: self.anchors,
            files: self.files,
            identity: self.identity,
            loaded: SwapOption::empty(),
        })))
    }
}

pub struct Roots {
    built_in: bool,
    anchors: RootCertStore,
    files: Vec<PathBuf>,
    identity: Option<Identity>,
    loaded: SwapOption<Configs>,
}

pub struct Configs {
    pub base: Arc<ClientConfig>,
    pub http1: Arc<ClientConfig>,
}

impl Roots {
    pub async fn configs(self: &Arc<Self>) -> io::Result<Arc<Configs>> {
        if let Some(configs) = &*self.loaded.load() {
            return Ok(Arc::clone(configs));
        }
        let roots = Arc::clone(self);
        let base = nanocode_platform::spawn_blocking(move || roots.load())
            .await
            .map_err(io::Error::other)
            .and_then(|loaded| loaded)?;
        assert!(base.alpn_protocols.is_empty());
        let mut http1 = base.clone();
        http1.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
        let configs = Arc::new(Configs {
            base: Arc::new(base),
            http1: Arc::new(http1),
        });
        self.loaded.store(Some(Arc::clone(&configs)));
        Ok(configs)
    }

    fn load(&self) -> io::Result<ClientConfig> {
        nanocode_tls::cpu::check()?;
        nanocode_tls::install_default_rustls_crypto_provider();
        let mut store = RootCertStore::empty();
        if self.built_in {
            let native = nanocode_platform::trust::load_native_certificates();
            let (added, _) = store.add_parsable_certificates(native.certificates);
            if added == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no usable native TLS trust roots",
                ));
            }
        }
        for file in &self.files {
            add_pem_file(&mut store, file)?;
        }
        store.roots.extend(self.anchors.roots.iter().cloned());
        if store.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "built-in TLS roots are disabled and no root certificates are configured",
            ));
        }
        let builder = ClientConfig::builder().with_root_certificates(store);
        match &self.identity {
            None => Ok(builder.with_no_client_auth()),
            Some(identity) => builder
                .with_client_auth_cert(identity.certificates.clone(), identity.key()?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }
}

fn add_pem_file(store: &mut RootCertStore, path: &Path) -> io::Result<()> {
    let pem = std::fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot read root certificate file '{}': {error}",
                path.display()
            ),
        )
    })?;
    add_pem(store, &pem, &format!("'{}'", path.display()))
}

fn add_pem(store: &mut RootCertStore, pem: &[u8], source: &str) -> io::Result<()> {
    let invalid = |detail: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid root certificates in {source}: {detail}"),
        )
    };
    let mut added = 0_usize;
    for certificate in CertificateDer::pem_slice_iter(pem) {
        let certificate = certificate.map_err(|error| invalid(error.to_string()))?;
        store
            .add(certificate)
            .map_err(|error| invalid(error.to_string()))?;
        added += 1;
    }
    if added == 0 {
        return Err(invalid("no PEM certificates found".to_owned()));
    }
    Ok(())
}

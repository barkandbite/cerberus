//! TLS adapter (ADR-0006): `TlsProvider` via rustls with the `ring` crypto
//! backend and **bundled** Mozilla roots (webpki-roots) for reproducible,
//! system-independent trust.
//!
//! No rustls type crosses the boundary: [`RustlsProvider::connect`] returns a
//! `Box<dyn ReadWrite>`, so callers depend only on our `cerberus-net` traits.

use cerberus_net::{NetError, ReadWrite, TlsProvider};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::sync::Arc;

/// A `TlsProvider` backed by rustls + ring + bundled roots.
pub struct RustlsProvider {
    config: Arc<ClientConfig>,
}

impl RustlsProvider {
    /// Build a provider trusting the bundled Mozilla root set (the default).
    pub fn new() -> Self {
        Self::from_roots(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
    }

    /// Build a provider trusting the operating system's root store instead of
    /// the bundled set. Not the default — useful behind a TLS-inspecting
    /// corporate/egress proxy whose CA is installed system-wide. Linux path.
    pub fn with_system_roots() -> std::io::Result<Self> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::CertificateDer;

        const BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
        let mut roots = RootCertStore::empty();
        let iter = CertificateDer::pem_file_iter(BUNDLE)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        for cert in iter.flatten() {
            let _ = roots.add(cert);
        }
        Ok(Self::from_roots(roots))
    }

    fn from_roots(roots: RootCertStore) -> Self {
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring provider supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();

        Self {
            config: Arc::new(config),
        }
    }
}

impl Default for RustlsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TlsProvider for RustlsProvider {
    fn connect(
        &self,
        server_name: &str,
        transport: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, NetError> {
        let name = ServerName::try_from(server_name)
            .map_err(|_| NetError::Tls(format!("invalid server name: {server_name}")))?
            .to_owned();
        let connection = ClientConnection::new(self.config.clone(), name)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        Ok(Box::new(StreamOwned::new(connection, transport)))
    }
}

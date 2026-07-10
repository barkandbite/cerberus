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
    ///
    /// Honors `$SSL_CERT_FILE` (the OpenSSL convention) when set and non-empty,
    /// falling back to the distro default bundle. This lets one binary work in
    /// environments — containers, CI, egress proxies — that stage the CA bundle
    /// somewhere other than the distro path.
    #[cfg(target_os = "linux")]
    pub fn with_system_roots() -> std::io::Result<Self> {
        const DEFAULT_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
        let bundle = std::env::var_os("SSL_CERT_FILE")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_BUNDLE));
        Self::roots_from_pem_bundle(&bundle)
    }

    /// Load a PEM bundle of CA certificates from `path` into a fresh provider.
    #[cfg(target_os = "linux")]
    fn roots_from_pem_bundle(path: &std::path::Path) -> std::io::Result<Self> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::CertificateDer;

        let mut roots = RootCertStore::empty();
        let iter = CertificateDer::pem_file_iter(path)
            .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;
        for cert in iter.flatten() {
            let _ = roots.add(cert);
        }
        Ok(Self::from_roots(roots))
    }

    /// `--system-roots` reads the OS trust store; only the Linux PEM-bundle path
    /// is implemented today. On other platforms this returns a clear error so the
    /// CLI can tell the user to omit the flag — the default bundled Mozilla roots
    /// ([`RustlsProvider::new`]) work everywhere. A native store reader (e.g.
    /// `rustls-native-certs` / SChannel / Security.framework) is a deferred
    /// follow-up (ADR-0015).
    #[cfg(not(target_os = "linux"))]
    pub fn with_system_roots() -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "--system-roots is only supported on Linux; omit it to use the bundled Mozilla roots",
        ))
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn pem_bundle_errors_name_the_missing_path() {
        match RustlsProvider::roots_from_pem_bundle(Path::new("/no/such/ca-bundle.pem")) {
            Ok(_) => panic!("a missing bundle must error"),
            Err(err) => assert!(
                err.to_string().contains("/no/such/ca-bundle.pem"),
                "the error should name the path it tried: {err}"
            ),
        }
    }

    #[test]
    fn system_roots_honor_ssl_cert_file() {
        // Serial within this one test to avoid racing the process-global env.
        let saved = std::env::var_os("SSL_CERT_FILE");

        // A bogus SSL_CERT_FILE is used (and named in the error) — proving the env
        // var is consulted ahead of the distro default.
        std::env::set_var("SSL_CERT_FILE", "/no/such/from-env.pem");
        match RustlsProvider::with_system_roots() {
            Ok(_) => panic!("a bogus SSL_CERT_FILE path must error"),
            Err(err) => assert!(
                err.to_string().contains("/no/such/from-env.pem"),
                "SSL_CERT_FILE must be honored: {err}"
            ),
        }

        // An empty value is ignored (falls back to the distro default bundle,
        // which exists on the CI/build image).
        std::env::set_var("SSL_CERT_FILE", "");
        if Path::new("/etc/ssl/certs/ca-certificates.crt").exists() {
            assert!(
                RustlsProvider::with_system_roots().is_ok(),
                "empty SSL_CERT_FILE should fall back to the default bundle"
            );
        }

        match saved {
            Some(v) => std::env::set_var("SSL_CERT_FILE", v),
            None => std::env::remove_var("SSL_CERT_FILE"),
        }
    }
}

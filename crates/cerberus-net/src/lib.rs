//! Networking seams.
//!
//! `TlsProvider` is the seam for rustls (approved; wired at M1). `DnsResolver`
//! is the seam for DoH. `HttpClient` is our own HTTP/1.1 stack (also ours to
//! build at M1) expressed as a trait so it, too, is swappable. No foreign type
//! (rustls config, socket addr internals, ...) crosses these boundaries.
//!
//! The scaffold ships `BuiltinHttpClient`, which serves the internal
//! `cerberus:` pages so the M0 render path has something to fetch. Real network
//! fetch is M1 and is deliberately absent here.

use cerberus_url::Url;
use std::io::{Read, Write};
use std::net::IpAddr;

/// A bidirectional byte stream (TCP, or TLS over TCP). Blanket-implemented so
/// adapters can hand back any `Read + Write + Send` without naming a foreign
/// type to callers.
pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

/// Establishes TLS client sessions. The seam for rustls (M1).
pub trait TlsProvider: Send + Sync {
    /// Wrap an existing transport in a TLS client session for `server_name`.
    fn connect(
        &self,
        server_name: &str,
        transport: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, NetError>;
}

/// Resolves hostnames. The seam for DNS-over-HTTPS (M1).
pub trait DnsResolver: Send + Sync {
    /// Resolve `host` to one or more IP addresses.
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError>;
}

/// Fetches resources. Our own HTTP/1.1 client implements this at M1.
pub trait HttpClient: Send + Sync {
    /// Perform a GET and return the full response.
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError>;
}

/// A fully-buffered HTTP response.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The value of `Content-Type`, if present.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
    }
}

/// Networking errors.
#[derive(Clone, Debug)]
pub enum NetError {
    /// The scheme/URL is not handled by this client.
    Unsupported(String),
    /// DNS resolution failed.
    Dns(String),
    /// Transport/IO failure.
    Io(String),
    /// TLS handshake failure.
    Tls(String),
}

/// The HTML served at `cerberus:home`. Small on purpose — it exercises the
/// tokenizer, layout, and paint path without a network.
pub const HOME_HTML: &str =
    "<html><body><h1>Cerberus</h1><p>three heads, one process</p></body></html>";

/// Serves the internal `cerberus:` pages. Real network fetch is M1.
#[derive(Debug, Default)]
pub struct BuiltinHttpClient;

impl HttpClient for BuiltinHttpClient {
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        match (url.is_builtin(), url.opaque.as_deref()) {
            (true, Some("home")) => Ok(HttpResponse {
                status: 200,
                headers: vec![(
                    "content-type".to_string(),
                    "text/html; charset=utf-8".to_string(),
                )],
                body: HOME_HTML.as_bytes().to_vec(),
            }),
            (true, Some(other)) => Err(NetError::Unsupported(format!(
                "cerberus:{other} is unknown"
            ))),
            _ => Err(NetError::Unsupported(format!(
                "{}: network fetch is not wired until M1",
                url.scheme
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_builtin_home() {
        let url = cerberus_url::parse("cerberus:home").unwrap();
        let resp = BuiltinHttpClient.get(&url).unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.content_type().unwrap().contains("text/html"));
    }

    #[test]
    fn refuses_network_until_m1() {
        let url = cerberus_url::parse("https://example.com/").unwrap();
        assert!(matches!(
            BuiltinHttpClient.get(&url),
            Err(NetError::Unsupported(_))
        ));
    }
}

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

use cerberus_types::{InstanceId, Origin};
use cerberus_url::Url;
use std::io::{Read, Write};
use std::net::IpAddr;

pub mod http1;

mod cache;
mod engine;
pub use cache::HttpCache;
pub use engine::{HttpEngine, Router, DEFAULT_USER_AGENT};

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

/// Which page context a fetch belongs to — determines the first-party origin
/// used for cookie attach/capture decisions.
#[derive(Clone, Debug)]
pub enum FetchKind {
    /// A top-level navigation. Each redirect hop is its *own* first party
    /// (the user is "at" wherever the redirect currently points), so a
    /// cross-host 302 never carries the previous host's cookies forward.
    Navigation,
    /// A subresource (image, script, …) loaded by a page whose first-party
    /// origin is fixed for every hop.
    Subresource { first_party: Origin },
}

/// Identity context for a fetch: which sealed instance it runs in, and what
/// kind of fetch it is. Travels with the request so cookie decisions happen
/// per hop, inside the engine, where redirects are followed.
#[derive(Clone, Debug)]
pub struct FetchContext {
    pub instance: InstanceId,
    pub kind: FetchKind,
}

impl FetchContext {
    /// The first-party origin in effect for a hop currently at `request`.
    pub fn first_party_for(&self, request: &Url) -> Option<Origin> {
        match &self.kind {
            FetchKind::Navigation => request.origin(),
            FetchKind::Subresource { first_party } => Some(first_party.clone()),
        }
    }
}

/// The cookie seam: the engine asks it what to attach before each hop and
/// hands it every `Set-Cookie` after. Implemented by the app over sealed
/// per-instance storage (`cerberus-net` never depends on `cerberus-storage`;
/// the dependency arrow stays app → both, per ADR-0001).
pub trait CookieJar: Send + Sync {
    /// The `Cookie` header value for a request, or `None` to send none.
    fn cookie_header(
        &self,
        instance: InstanceId,
        request: &Url,
        first_party: &Origin,
    ) -> Option<String>;

    /// Offer one `Set-Cookie` header value for storage (policy decides whether
    /// it is kept, quarantined, or dropped).
    fn set_cookie(&self, instance: InstanceId, request: &Url, first_party: &Origin, value: &str);
}

/// Fetches resources. Our own HTTP/1.1 client implements this at M1.
pub trait HttpClient: Send + Sync {
    /// Perform a GET and return the full response.
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError>;

    /// Like [`get`](Self::get), but inside an identity context, so the engine
    /// can attach/capture cookies per hop. Context-free implementations (the
    /// built-in pages, tests) fall back to `get`.
    fn get_in(&self, url: &Url, ctx: &FetchContext) -> Result<HttpResponse, NetError> {
        let _ = ctx;
        self.get(url)
    }
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
    /// A malformed HTTP response (or too many redirects).
    Protocol(String),
}

/// The HTML served at `cerberus:home`. Small on purpose — it exercises the
/// tokenizer, layout, and paint path without a network.
pub const HOME_HTML: &str =
    "<html><body><h1>Cerberus</h1><p>three heads, one process</p></body></html>";

/// The HTML served at `cerberus:about`. A second built-in page so navigation
/// and history have somewhere to go before the network stack (M1).
pub const ABOUT_HTML: &str =
    "<html><body><h1>About Cerberus</h1><p>privacy-first, memory-lean</p></body></html>";

/// Serves the internal `cerberus:` pages. Real network fetch is M1.
#[derive(Debug, Default)]
pub struct BuiltinHttpClient;

fn builtin_html(html: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )],
        body: html.as_bytes().to_vec(),
    }
}

impl HttpClient for BuiltinHttpClient {
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        match (url.is_builtin(), url.opaque.as_deref()) {
            (true, Some("home")) => Ok(builtin_html(HOME_HTML)),
            (true, Some("about")) => Ok(builtin_html(ABOUT_HTML)),
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

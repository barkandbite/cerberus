//! Connection engine: resolve, connect, (optionally) TLS, request, follow
//! redirects. This is *our* HTTP client (bootstrapped); rustls and the DoH
//! resolver are injected behind the `TlsProvider`/`DnsResolver` traits.

use crate::{
    http1, BuiltinHttpClient, DnsResolver, HttpClient, HttpResponse, NetError, ReadWrite,
    TlsProvider,
};
use cerberus_url::Url;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// A fixed, minimal User-Agent. We do **not** impersonate another browser
/// (see the threat model's non-goals).
pub const DEFAULT_USER_AGENT: &str = "Cerberus/0.0";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: u8 = 5;

/// Our HTTP/1.1 client over injected TLS + DNS.
pub struct HttpEngine {
    tls: Box<dyn TlsProvider>,
    dns: Box<dyn DnsResolver>,
    user_agent: String,
}

impl HttpEngine {
    /// Build an engine over a TLS provider and DNS resolver.
    pub fn new(tls: Box<dyn TlsProvider>, dns: Box<dyn DnsResolver>) -> Self {
        Self {
            tls,
            dns,
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }
    }

    fn fetch_once(&self, url: &Url) -> Result<HttpResponse, NetError> {
        let https = match url.scheme.as_str() {
            "https" => true,
            "http" => false,
            other => return Err(NetError::Unsupported(format!("scheme {other}"))),
        };
        if url.host.is_empty() {
            return Err(NetError::Unsupported("URL has no host".into()));
        }
        let port = url.port.unwrap_or(if https { 443 } else { 80 });

        let ip = self
            .dns
            .resolve(&url.host)?
            .into_iter()
            .next()
            .ok_or_else(|| NetError::Dns(format!("no address for {}", url.host)))?;

        let tcp = TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_TIMEOUT)
            .map_err(|e| NetError::Io(e.to_string()))?;
        tcp.set_read_timeout(Some(IO_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(IO_TIMEOUT)).ok();

        let mut stream: Box<dyn ReadWrite> = if https {
            self.tls.connect(&url.host, Box::new(tcp))?
        } else {
            Box::new(tcp)
        };

        let path = full_path(url);
        http1::send(
            stream.as_mut(),
            &http1::Request {
                method: "GET",
                host: &url.host,
                path: &path,
                user_agent: &self.user_agent,
                headers: &[],
                body: &[],
            },
        )
    }
}

impl HttpClient for HttpEngine {
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let resp = self.fetch_once(&current)?;
            if (300..400).contains(&resp.status) {
                if let Some(location) = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone())
                {
                    current = resolve_location(&current, &location)?;
                    continue;
                }
            }
            return Ok(resp);
        }
        Err(NetError::Protocol("too many redirects".into()))
    }
}

/// Dispatches by scheme: built-in `cerberus:` pages are served locally; `http`/
/// `https` go to the network engine. This is what the app holds.
pub struct Router {
    engine: HttpEngine,
}

impl Router {
    /// Build a router whose network engine uses `tls` + `dns`.
    pub fn new(tls: Box<dyn TlsProvider>, dns: Box<dyn DnsResolver>) -> Self {
        Self {
            engine: HttpEngine::new(tls, dns),
        }
    }
}

impl HttpClient for Router {
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        if url.is_builtin() {
            BuiltinHttpClient.get(url)
        } else {
            self.engine.get(url)
        }
    }
}

fn full_path(url: &Url) -> String {
    let path = if url.path.is_empty() { "/" } else { &url.path };
    match &url.query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    }
}

fn authority(url: &Url) -> String {
    match url.port {
        Some(p) => format!("{}:{}", url.host, p),
        None => url.host.clone(),
    }
}

/// Resolve a redirect `Location` (absolute, protocol-relative, root-relative, or
/// path-relative) against `base`.
fn resolve_location(base: &Url, location: &str) -> Result<Url, NetError> {
    let loc = location.trim();
    let absolute = if loc.starts_with("http://") || loc.starts_with("https://") {
        loc.to_string()
    } else if let Some(rest) = loc.strip_prefix("//") {
        format!("{}://{}", base.scheme, rest)
    } else if loc.starts_with('/') {
        format!("{}://{}{}", base.scheme, authority(base), loc)
    } else {
        let dir = match base.path.rfind('/') {
            Some(i) => &base.path[..=i],
            None => "/",
        };
        format!("{}://{}{}{}", base.scheme, authority(base), dir, loc)
    };
    cerberus_url::parse(&absolute).map_err(|e| NetError::Protocol(format!("bad redirect: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_full_path_with_query() {
        let url = cerberus_url::parse("https://example.com/a/b?x=1").unwrap();
        assert_eq!(full_path(&url), "/a/b?x=1");
        let root = cerberus_url::parse("https://example.com").unwrap();
        assert_eq!(full_path(&root), "/");
    }

    #[test]
    fn resolves_redirect_targets() {
        let base = cerberus_url::parse("https://example.com/dir/page").unwrap();
        assert_eq!(
            resolve_location(&base, "https://other.test/x")
                .unwrap()
                .host,
            "other.test"
        );
        assert_eq!(resolve_location(&base, "/root").unwrap().path, "/root");
        assert_eq!(
            resolve_location(&base, "sibling").unwrap().path,
            "/dir/sibling"
        );
    }
}

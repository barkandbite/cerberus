//! Connection engine: resolve, connect, (optionally) TLS, request, follow
//! redirects. This is *our* HTTP client (bootstrapped); rustls and the DoH
//! resolver are injected behind the `TlsProvider`/`DnsResolver` traits.

use crate::{
    http1, BuiltinHttpClient, DnsResolver, HttpClient, HttpResponse, NetError, ReadWrite,
    TlsProvider,
};
use cerberus_url::Url;
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

/// Our honest, default identity. We lead with this on every origin and only
/// escalate (see [`UA_LADDER`]) when a site refuses to serve it.
pub const DEFAULT_USER_AGENT: &str = "Cerberus/0.0";

/// User-Agent ladder, tried in order. We identify honestly *first*; only when an
/// origin answers a soft block (see [`is_soft_block`]) do we escalate to a
/// common, mainstream string — the minimum needed to get served. The fallback
/// strings are version-stable and shared by *every* Cerberus user, so they add
/// no per-user fingerprinting entropy (the Tor-Browser "one UA for all" stance).
/// This is deliberately not impersonation-by-default: a site that serves
/// `Cerberus/0.0` never sees anything else.
const UA_LADDER: &[&str] = &[
    DEFAULT_USER_AGENT,
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/142.0.0.0 Safari/537.36",
];

/// Status codes that typically mean "bot management refused this client" rather
/// than a genuine error — worth one retry up the UA ladder. A real 403/404 for
/// the escalated UA too is simply returned to the caller.
fn is_soft_block(status: u16) -> bool {
    matches!(status, 403 | 429 | 503)
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: u8 = 5;

/// Our HTTP/1.1 client over injected TLS + DNS.
pub struct HttpEngine {
    tls: Box<dyn TlsProvider>,
    dns: Box<dyn DnsResolver>,
    /// Identity ladder (honest first). See [`UA_LADDER`].
    user_agents: Vec<String>,
    /// Per-host memo of the ladder rung that last got served, so subresources
    /// (images, etc.) on an origin that needed escalation don't re-probe the
    /// honest UA and eat an extra blocked round-trip each.
    ua_memo: Mutex<HashMap<String, usize>>,
}

impl HttpEngine {
    /// Build an engine over a TLS provider and DNS resolver.
    pub fn new(tls: Box<dyn TlsProvider>, dns: Box<dyn DnsResolver>) -> Self {
        Self {
            tls,
            dns,
            user_agents: UA_LADDER.iter().map(|s| s.to_string()).collect(),
            ua_memo: Mutex::new(HashMap::new()),
        }
    }

    fn fetch_once(&self, url: &Url, user_agent: &str) -> Result<HttpResponse, NetError> {
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
                user_agent,
                headers: &[],
                body: &[],
            },
        )
    }

    /// The User-Agent we currently present to `host`: the memoized rung that
    /// last got served, or the honest default if this origin never forced an
    /// escalation. This is the single source of truth a caller threads into the
    /// JS `navigator.userAgent`, so the header and the script-visible identity
    /// can never disagree.
    pub fn user_agent_for(&self, host: &str) -> String {
        let idx = *self.ua_memo.lock().unwrap().get(host).unwrap_or(&0);
        self.user_agents
            .get(idx)
            .cloned()
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string())
    }

    /// Fetch with a fixed User-Agent, following redirects.
    fn fetch_redirected(&self, url: &Url, user_agent: &str) -> Result<HttpResponse, NetError> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let resp = self.fetch_once(&current, user_agent)?;
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

impl HttpClient for HttpEngine {
    /// Fetch `url`, climbing the UA ladder only as far as the origin forces us.
    /// We start at the rung that last worked for this host (honest by default),
    /// and escalate one rung per soft block until a request is served. Network
    /// errors are *not* retried up the ladder — a different UA won't fix them.
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        let start = *self.ua_memo.lock().unwrap().get(&url.host).unwrap_or(&0);
        let mut last_blocked = None;
        for idx in start..self.user_agents.len() {
            let resp = self.fetch_redirected(url, &self.user_agents[idx])?;
            if is_soft_block(resp.status) {
                last_blocked = Some(resp);
                continue;
            }
            // Served: remember the rung so this origin's subresources start here.
            self.ua_memo.lock().unwrap().insert(url.host.clone(), idx);
            return Ok(resp);
        }
        // Every rung was soft-blocked: hand back the last response (e.g. 403) so
        // the caller can render the site's own block page rather than an error.
        last_blocked.ok_or_else(|| NetError::Protocol("no user-agent to try".into()))
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

    /// The User-Agent presented to `url`'s origin (see
    /// [`HttpEngine::user_agent_for`]). Built-in pages are served locally and
    /// always carry our honest identity.
    pub fn user_agent_for(&self, url: &Url) -> String {
        if url.is_builtin() {
            DEFAULT_USER_AGENT.to_string()
        } else {
            self.engine.user_agent_for(&url.host)
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
    fn ua_ladder_leads_with_the_honest_identity() {
        // Honesty-by-default: the first rung is always our real identity, so a
        // site that accepts it never sees an impersonating string.
        assert_eq!(UA_LADDER.first().copied(), Some(DEFAULT_USER_AGENT));
        assert!(
            UA_LADDER.len() >= 2,
            "ladder needs at least one fallback rung"
        );
    }

    #[test]
    fn soft_block_is_limited_to_bot_management_codes() {
        for s in [403, 429, 503] {
            assert!(is_soft_block(s), "{s} should escalate the UA");
        }
        for s in [200, 204, 301, 404, 410, 500, 502] {
            assert!(!is_soft_block(s), "{s} should NOT escalate the UA");
        }
    }

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

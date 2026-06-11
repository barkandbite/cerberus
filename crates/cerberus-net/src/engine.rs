//! Connection engine: resolve, connect, (optionally) TLS, request, follow
//! redirects. This is *our* HTTP client (bootstrapped); rustls and the DoH
//! resolver are injected behind the `TlsProvider`/`DnsResolver` traits.

use crate::{
    http1, BuiltinHttpClient, CookieJar, DnsResolver, FetchContext, HttpClient, HttpResponse,
    NetError, ReadWrite, TlsProvider,
};
use cerberus_url::Url;
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
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
    /// Cookie attach/capture seam, consulted per hop (so redirects are
    /// covered). `None` (tests, tooling) sends and stores nothing.
    jar: Option<Arc<dyn CookieJar>>,
}

impl HttpEngine {
    /// Build an engine over a TLS provider and DNS resolver, with no cookies.
    pub fn new(tls: Box<dyn TlsProvider>, dns: Box<dyn DnsResolver>) -> Self {
        Self::with_jar(tls, dns, None)
    }

    /// Build an engine that attaches/captures cookies through `jar`.
    pub fn with_jar(
        tls: Box<dyn TlsProvider>,
        dns: Box<dyn DnsResolver>,
        jar: Option<Arc<dyn CookieJar>>,
    ) -> Self {
        Self {
            tls,
            dns,
            user_agents: UA_LADDER.iter().map(|s| s.to_string()).collect(),
            ua_memo: Mutex::new(HashMap::new()),
            jar,
        }
    }

    fn fetch_once(
        &self,
        url: &Url,
        user_agent: &str,
        ctx: Option<&FetchContext>,
    ) -> Result<HttpResponse, NetError> {
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

        // Cookie attach/capture happens here, per hop, so redirect chains are
        // covered and the first party is recomputed for navigations.
        let cookie_ctx = match (self.jar.as_deref(), ctx) {
            (Some(jar), Some(ctx)) => ctx.first_party_for(url).map(|fp| (jar, ctx.instance, fp)),
            _ => None,
        };
        let cookie_value = cookie_ctx
            .as_ref()
            .and_then(|(jar, instance, fp)| jar.cookie_header(*instance, url, fp));
        let mut extra_headers: Vec<(&str, &str)> = Vec::new();
        if let Some(value) = cookie_value.as_deref() {
            extra_headers.push(("Cookie", value));
        }

        let path = full_path(url);
        let resp = http1::send(
            stream.as_mut(),
            &http1::Request {
                method: "GET",
                host: &url.host,
                path: &path,
                user_agent,
                headers: &extra_headers,
                body: &[],
            },
        )?;

        if let Some((jar, instance, fp)) = cookie_ctx {
            for (k, v) in &resp.headers {
                if k.eq_ignore_ascii_case("set-cookie") {
                    jar.set_cookie(instance, url, &fp, v);
                }
            }
        }
        Ok(resp)
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
    fn fetch_redirected(
        &self,
        url: &Url,
        user_agent: &str,
        ctx: Option<&FetchContext>,
    ) -> Result<HttpResponse, NetError> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let resp = self.fetch_once(&current, user_agent, ctx)?;
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

impl HttpEngine {
    /// Fetch `url`, climbing the UA ladder only as far as the origin forces us.
    /// We start at the rung that last worked for this host (honest by default),
    /// and escalate one rung per soft block until a request is served. Network
    /// errors are *not* retried up the ladder — a different UA won't fix them.
    fn get_ctx(&self, url: &Url, ctx: Option<&FetchContext>) -> Result<HttpResponse, NetError> {
        let start = *self.ua_memo.lock().unwrap().get(&url.host).unwrap_or(&0);
        let mut last_blocked = None;
        for idx in start..self.user_agents.len() {
            let resp = self.fetch_redirected(url, &self.user_agents[idx], ctx)?;
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

impl HttpClient for HttpEngine {
    fn get(&self, url: &Url) -> Result<HttpResponse, NetError> {
        self.get_ctx(url, None)
    }

    fn get_in(&self, url: &Url, ctx: &FetchContext) -> Result<HttpResponse, NetError> {
        self.get_ctx(url, Some(ctx))
    }
}

/// Dispatches by scheme: built-in `cerberus:` pages are served locally; `http`/
/// `https` go to the network engine. This is what the app holds.
pub struct Router {
    engine: HttpEngine,
}

impl Router {
    /// Build a router whose network engine uses `tls` + `dns`, with no cookies.
    pub fn new(tls: Box<dyn TlsProvider>, dns: Box<dyn DnsResolver>) -> Self {
        Self::with_jar(tls, dns, None)
    }

    /// Build a router whose network engine attaches/captures cookies via `jar`.
    pub fn with_jar(
        tls: Box<dyn TlsProvider>,
        dns: Box<dyn DnsResolver>,
        jar: Option<Arc<dyn CookieJar>>,
    ) -> Self {
        Self {
            engine: HttpEngine::with_jar(tls, dns, jar),
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

    fn get_in(&self, url: &Url, ctx: &FetchContext) -> Result<HttpResponse, NetError> {
        if url.is_builtin() {
            BuiltinHttpClient.get(url)
        } else {
            self.engine.get_in(url, ctx)
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

    // ---- Cookie flow through the engine (hermetic, loopback-only) ----

    use crate::{CookieJar, FetchContext, FetchKind};
    use cerberus_types::{InstanceId, Origin};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    /// TLS provider that must never be reached (plain-http test traffic).
    struct NoTls;
    impl TlsProvider for NoTls {
        fn connect(
            &self,
            _server_name: &str,
            _transport: Box<dyn ReadWrite>,
        ) -> Result<Box<dyn ReadWrite>, NetError> {
            Err(NetError::Tls("TLS unexpected in this test".into()))
        }
    }

    /// Resolves every host to loopback.
    struct LoopbackDns;
    impl DnsResolver for LoopbackDns {
        fn resolve(&self, _host: &str) -> Result<Vec<std::net::IpAddr>, NetError> {
            Ok(vec![std::net::IpAddr::from([127, 0, 0, 1])])
        }
    }

    /// Records every Set-Cookie it is offered and attaches all recorded pairs.
    #[derive(Default)]
    struct RecordingJar {
        seen: Mutex<Vec<(String, String)>>, // (first_party site, raw header)
    }
    impl CookieJar for RecordingJar {
        fn cookie_header(
            &self,
            _instance: InstanceId,
            _request: &Url,
            _first_party: &Origin,
        ) -> Option<String> {
            let seen = self.seen.lock().unwrap();
            if seen.is_empty() {
                None
            } else {
                Some(
                    seen.iter()
                        .map(|(_, raw)| raw.split(';').next().unwrap_or("").trim().to_string())
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            }
        }
        fn set_cookie(
            &self,
            _instance: InstanceId,
            _request: &Url,
            first_party: &Origin,
            value: &str,
        ) {
            self.seen
                .lock()
                .unwrap()
                .push((first_party.site(), value.to_string()));
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn cookies_are_captured_and_attached_per_redirect_hop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            // Hop 1: no cookies yet; answer 302 + Set-Cookie.
            let (mut s1, _) = listener.accept().unwrap();
            let req1 = read_request(&mut s1);
            assert!(!req1.contains("Cookie:"), "hop 1 must carry no cookies");
            let resp1 = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/next\r\n\
                 Set-Cookie: a=1; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            s1.write_all(resp1.as_bytes()).unwrap();
            drop(s1);

            // Hop 2: the redirect target must carry the hop-1 cookie.
            let (mut s2, _) = listener.accept().unwrap();
            let req2 = read_request(&mut s2);
            assert!(req2.contains("Cookie: a=1"), "hop 2 request: {req2:?}");
            s2.write_all(
                b"HTTP/1.1 200 OK\r\nSet-Cookie: b=2; Path=/\r\n\
                  Content-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .unwrap();
        });

        let jar = Arc::new(RecordingJar::default());
        let engine =
            HttpEngine::with_jar(Box::new(NoTls), Box::new(LoopbackDns), Some(jar.clone()));
        let url = cerberus_url::parse(&format!("http://127.0.0.1:{port}/start")).unwrap();
        let ctx = FetchContext {
            instance: InstanceId::from_u64_pair(0, 1),
            kind: FetchKind::Navigation,
        };
        let resp = engine.get_in(&url, &ctx).unwrap();
        server.join().unwrap();

        assert_eq!(resp.status, 200);
        let seen = jar.seen.lock().unwrap();
        // Both hops' Set-Cookie headers were captured (302 included).
        assert_eq!(seen.len(), 2, "captured: {seen:?}");
        assert!(seen[0].1.starts_with("a=1"));
        assert!(seen[1].1.starts_with("b=2"));
    }

    #[test]
    fn context_free_get_neither_attaches_nor_captures() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let req = read_request(&mut s);
            assert!(!req.contains("Cookie:"));
            s.write_all(
                b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        });

        let jar = Arc::new(RecordingJar::default());
        // Pre-seed the jar to prove nothing is attached without a context.
        jar.seen
            .lock()
            .unwrap()
            .push(("http://x".into(), "pre=1".into()));
        let engine =
            HttpEngine::with_jar(Box::new(NoTls), Box::new(LoopbackDns), Some(jar.clone()));
        let url = cerberus_url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let resp = engine.get(&url).unwrap();
        server.join().unwrap();

        assert_eq!(resp.status, 200);
        // Capture also did not happen: only the pre-seeded entry remains.
        assert_eq!(jar.seen.lock().unwrap().len(), 1);
    }
}

//! Concrete `DnsResolver`s that need no external deps: the operating-system
//! resolver and an ordered fallback chain. DoH itself lives in the
//! `cerberus-dns-doh` adapter; these compose with it so the app can run
//! **multi-DoH then a system fallback** (ADR-0006): privacy-preserving encrypted
//! resolvers first, and only if every one is unreachable does name resolution
//! fall back to the OS — so a network that blocks or mangles our DoH (e.g. a
//! middlebox that answers the DoH POST with HTTP 505) no longer kills all
//! browsing.

use crate::{DnsResolver, NetError};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Resolves via the operating system (`getaddrinfo`). This is the only path that
/// exposes lookups to the local network, so it is meant to sit **last** in a
/// [`FallbackResolver`] — used solely when every encrypted resolver fails.
pub struct SystemResolver;

impl DnsResolver for SystemResolver {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        // An IP literal needs no resolution.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        // The port is irrelevant to name resolution; 0 keeps it explicit.
        use std::net::ToSocketAddrs;
        let ips: Vec<IpAddr> = (host, 0u16)
            .to_socket_addrs()
            .map_err(|e| NetError::Dns(format!("system DNS: {e}")))?
            .map(|sa| sa.ip())
            .collect();
        if ips.is_empty() {
            return Err(NetError::Dns(format!("system DNS: no records for {host}")));
        }
        Ok(ips)
    }
}

/// Tries each resolver in order and returns the first non-empty success. Built
/// so one blocked or misbehaving resolver does not fail the whole navigation:
/// the next resolver — and ultimately the OS — still gets a turn. The error
/// returned on total failure is the last one seen (most-fallback resolver).
pub struct FallbackResolver {
    resolvers: Vec<Box<dyn DnsResolver>>,
}

impl FallbackResolver {
    /// Build from an ordered list, most-preferred first. An empty list is legal
    /// but always fails to resolve.
    pub fn new(resolvers: Vec<Box<dyn DnsResolver>>) -> Self {
        Self { resolvers }
    }
}

impl DnsResolver for FallbackResolver {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        let mut last = NetError::Dns("no resolver configured".into());
        for resolver in &self.resolvers {
            match resolver.resolve(host) {
                Ok(ips) if !ips.is_empty() => return Ok(ips),
                Ok(_) => last = NetError::Dns(format!("no records for {host}")),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// Default positive-cache lifetime. Deliberately short — a privacy browser
/// wants tight DNS lifetimes — but long enough to collapse the burst of
/// same-host lookups a single page load triggers (origin + a few CDNs, resolved
/// once for the document and reused for every subresource / redirect hop).
pub const DEFAULT_DNS_TTL: Duration = Duration::from_secs(30);

/// A TTL-aware **positive** resolution cache wrapping any inner resolver. With
/// the DoH-first chain each `resolve()` is a full encrypted round-trip, so
/// re-resolving the same host for every subresource is pure latency; this serves
/// a recent answer instead. Only successes are cached (a transient DoH blip must
/// not pin a host as unresolvable), the map is bounded, and entries expire by
/// TTL. It changes nothing about *when* resolution happens — the proxied path
/// still never calls `resolve()` at all, so the no-local-leak guarantee holds.
pub struct CachingResolver {
    inner: Box<dyn DnsResolver>,
    ttl: Duration,
    max_entries: usize,
    cache: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
}

impl CachingResolver {
    /// Wrap `inner` with the default TTL and a modest entry cap.
    pub fn new(inner: Box<dyn DnsResolver>) -> Self {
        Self::with_ttl(inner, DEFAULT_DNS_TTL)
    }

    /// Wrap `inner` with an explicit TTL (used by tests to exercise expiry).
    pub fn with_ttl(inner: Box<dyn DnsResolver>, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            max_entries: 256,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl DnsResolver for CachingResolver {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        // An IP literal needs no resolution and no cache entry.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        let now = Instant::now();
        if let Ok(cache) = self.cache.lock() {
            if let Some((ips, expiry)) = cache.get(host) {
                if now < *expiry {
                    return Ok(ips.clone());
                }
            }
        }
        let ips = self.inner.resolve(host)?;
        if !ips.is_empty() {
            if let Ok(mut cache) = self.cache.lock() {
                // Keep the map bounded: drop expired entries when full, and if it
                // is *still* full, clear it (a DNS cache holds a handful of hosts;
                // hitting the cap means something unusual — bounded beats unbounded).
                if cache.len() >= self.max_entries && !cache.contains_key(host) {
                    cache.retain(|_, (_, e)| now < *e);
                    if cache.len() >= self.max_entries {
                        cache.clear();
                    }
                }
                cache.insert(host.to_string(), (ips.clone(), now + self.ttl));
            }
        }
        Ok(ips)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    struct Always(Vec<IpAddr>);
    impl DnsResolver for Always {
        fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetError> {
            Ok(self.0.clone())
        }
    }
    struct Fails(&'static str);
    impl DnsResolver for Fails {
        fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetError> {
            Err(NetError::Dns(self.0.into()))
        }
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// Counts how many times the underlying `resolve()` runs, via a shared
    /// counter the test can read after handing the resolver to the cache.
    struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>, Vec<IpAddr>);
    impl DnsResolver for Counting {
        fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.1.clone())
        }
    }

    #[test]
    fn caching_resolver_collapses_repeat_lookups_within_ttl() {
        use std::sync::atomic::Ordering;
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = CachingResolver::new(Box::new(Counting(n.clone(), vec![ip(1, 2, 3, 4)])));
        assert_eq!(r.resolve("example.com").unwrap(), vec![ip(1, 2, 3, 4)]);
        assert_eq!(r.resolve("example.com").unwrap(), vec![ip(1, 2, 3, 4)]);
        // Two lookups of the same host → exactly one underlying resolve.
        assert_eq!(n.load(Ordering::Relaxed), 1);
        // A different host resolves independently.
        let _ = r.resolve("other.test").unwrap();
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn caching_resolver_reresolves_after_expiry() {
        use std::sync::atomic::Ordering;
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A zero TTL means every entry is already expired, so nothing is reused.
        let r = CachingResolver::with_ttl(
            Box::new(Counting(n.clone(), vec![ip(1, 1, 1, 1)])),
            Duration::ZERO,
        );
        let _ = r.resolve("example.com").unwrap();
        let _ = r.resolve("example.com").unwrap();
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn caching_resolver_short_circuits_ip_literals() {
        use std::sync::atomic::Ordering;
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = CachingResolver::new(Box::new(Counting(n.clone(), vec![ip(9, 9, 9, 9)])));
        assert_eq!(r.resolve("203.0.113.7").unwrap(), vec![ip(203, 0, 113, 7)]);
        // An IP literal never touches the inner resolver.
        assert_eq!(n.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn falls_through_to_the_first_success() {
        let r = FallbackResolver::new(vec![
            Box::new(Fails("doh blocked")),
            Box::new(Always(vec![ip(1, 2, 3, 4)])),
            Box::new(Always(vec![ip(9, 9, 9, 9)])), // never reached
        ]);
        assert_eq!(r.resolve("example.com").unwrap(), vec![ip(1, 2, 3, 4)]);
    }

    #[test]
    fn empty_success_is_skipped() {
        let r = FallbackResolver::new(vec![
            Box::new(Always(vec![])), // resolved nothing → try next
            Box::new(Always(vec![ip(1, 1, 1, 1)])),
        ]);
        assert_eq!(r.resolve("example.com").unwrap(), vec![ip(1, 1, 1, 1)]);
    }

    #[test]
    fn reports_the_last_error_when_all_fail() {
        let r = FallbackResolver::new(vec![Box::new(Fails("first")), Box::new(Fails("last"))]);
        let err = r.resolve("example.com").unwrap_err();
        assert!(format!("{err:?}").contains("last"), "got {err:?}");
    }

    #[test]
    fn empty_chain_fails() {
        let r = FallbackResolver::new(vec![]);
        assert!(r.resolve("example.com").is_err());
    }

    #[test]
    fn system_resolver_passes_through_ip_literals() {
        // Deterministic and network-free: an IP literal resolves to itself.
        assert_eq!(
            SystemResolver.resolve("127.0.0.1").unwrap(),
            vec![ip(127, 0, 0, 1)]
        );
    }
}

//! Concrete `DnsResolver`s that need no external deps: the operating-system
//! resolver and an ordered fallback chain. DoH itself lives in the
//! `cerberus-dns-doh` adapter; these compose with it so the app can run
//! **multi-DoH then a system fallback** (ADR-0006): privacy-preserving encrypted
//! resolvers first, and only if every one is unreachable does name resolution
//! fall back to the OS — so a network that blocks or mangles our DoH (e.g. a
//! middlebox that answers the DoH POST with HTTP 505) no longer kills all
//! browsing.

use crate::{DnsResolver, NetError};
use std::net::IpAddr;

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

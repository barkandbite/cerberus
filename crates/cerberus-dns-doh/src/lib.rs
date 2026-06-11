//! DNS-over-HTTPS resolver (ADR-0006).
//!
//! Implements `DnsResolver` by POSTing an RFC 1035 query (RFC 8484 wire format)
//! to a DoH endpoint over an injected `TlsProvider`, reusing the bootstrapped
//! `http1` codec. DoH-only: there is no plaintext-DNS fallback. The resolver's
//! IP is known, so reaching it needs no prior name lookup.
//!
//! Default endpoint: **Quad9** (9.9.9.9, SNI/Host `dns.quad9.net`). Only A
//! (IPv4) records are returned, which keeps connections on a widely-reachable
//! path; AAAA can be added later.

use cerberus_net::{http1, DnsResolver, NetError, TlsProvider};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Total DoH attempts before giving up. A single transient hiccup at the
/// resolver (a 503 from the DoH endpoint, a dropped connection) otherwise fails
/// the entire navigation — name resolution is the very first step, so it has no
/// safety net below it. Kept small to bound worst-case latency.
const DOH_ATTEMPTS: usize = 3;

/// Whether a DoH HTTP status is worth retrying. 5xx and 429 are transient
/// resolver-side conditions; a 4xx means our request is wrong and a retry would
/// just repeat it.
fn is_retriable_status(status: u16) -> bool {
    status >= 500 || status == 429
}

/// A DoH resolver over a `TlsProvider`.
pub struct DohResolver {
    tls: Box<dyn TlsProvider>,
    server_ip: IpAddr,
    server_name: String,
    path: String,
}

impl DohResolver {
    /// Build a resolver for an explicit endpoint.
    pub fn new(
        tls: Box<dyn TlsProvider>,
        server_ip: IpAddr,
        server_name: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            tls,
            server_ip,
            server_name: server_name.into(),
            path: path.into(),
        }
    }

    /// The default Quad9 endpoint (privacy-focused, no-logging).
    pub fn quad9(tls: Box<dyn TlsProvider>) -> Self {
        Self::new(
            tls,
            IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
            "dns.quad9.net",
            "/dns-query",
        )
    }
}

impl DnsResolver for DohResolver {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        // An IP literal needs no resolution.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }

        let query = encode_query(host)?;
        let mut last = NetError::Dns("DoH: no attempt made".into());
        for attempt in 0..DOH_ATTEMPTS {
            match self.resolve_once(host, &query) {
                Ok(ips) => return Ok(ips),
                // Definitive (bad request, NODATA): retrying changes nothing.
                Err((false, e)) => return Err(e),
                // Transient (5xx/429, dropped connection): back off and retry.
                Err((true, e)) => {
                    last = e;
                    if attempt + 1 < DOH_ATTEMPTS {
                        std::thread::sleep(Duration::from_millis(150 * (attempt as u64 + 1)));
                    }
                }
            }
        }
        Err(last)
    }
}

impl DohResolver {
    /// One DoH round-trip. The `bool` in the error is whether it is worth
    /// retrying: transport failures and retriable statuses are `true`; a wrong
    /// request or an authoritative empty answer is `false`.
    fn resolve_once(&self, host: &str, query: &[u8]) -> Result<Vec<IpAddr>, (bool, NetError)> {
        let tcp = TcpStream::connect_timeout(&SocketAddr::new(self.server_ip, 443), TIMEOUT)
            .map_err(|e| (true, NetError::Dns(format!("DoH connect: {e}"))))?;
        tcp.set_read_timeout(Some(TIMEOUT)).ok();
        tcp.set_write_timeout(Some(TIMEOUT)).ok();

        let mut stream = self
            .tls
            .connect(&self.server_name, Box::new(tcp))
            .map_err(|e| (true, e))?;
        let resp = http1::send(
            stream.as_mut(),
            &http1::Request {
                method: "POST",
                host: &self.server_name,
                path: &self.path,
                user_agent: cerberus_net::DEFAULT_USER_AGENT,
                headers: &[
                    ("Content-Type", "application/dns-message"),
                    ("Accept", "application/dns-message"),
                ],
                body: query,
            },
        )
        .map_err(|e| (true, e))?;

        if resp.status != 200 {
            return Err((
                is_retriable_status(resp.status),
                NetError::Dns(format!("DoH HTTP {}", resp.status)),
            ));
        }
        let ips = decode_a_records(&resp.body).map_err(|e| (true, e))?;
        if ips.is_empty() {
            return Err((false, NetError::Dns(format!("no A records for {host}"))));
        }
        Ok(ips)
    }
}

/// Encode an A-record query for `host`.
fn encode_query(host: &str) -> Result<Vec<u8>, NetError> {
    let mut msg = Vec::with_capacity(host.len() + 18);
    msg.extend_from_slice(&0x1234u16.to_be_bytes()); // id (TLS-authenticated; not security critical)
    msg.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: recursion desired
    msg.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    msg.extend_from_slice(&[0; 6]); // ancount/nscount/arcount = 0

    for label in host.split('.').filter(|l| !l.is_empty()) {
        if label.len() > 63 {
            return Err(NetError::Dns("DNS label too long".into()));
        }
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0); // root label
    msg.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    msg.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    Ok(msg)
}

/// Parse A (IPv4) records out of a DNS response message.
fn decode_a_records(buf: &[u8]) -> Result<Vec<IpAddr>, NetError> {
    if buf.len() < 12 {
        return Err(NetError::Dns("short DNS response".into()));
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(buf, pos)?;
        pos = pos
            .checked_add(4)
            .filter(|p| *p <= buf.len())
            .ok_or_else(|| NetError::Dns("truncated question".into()))?;
    }

    let mut ips = Vec::new();
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        if pos + 10 > buf.len() {
            return Err(NetError::Dns("truncated answer header".into()));
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlen = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > buf.len() {
            return Err(NetError::Dns("truncated rdata".into()));
        }
        if rtype == 1 && rdlen == 4 {
            ips.push(IpAddr::V4(Ipv4Addr::new(
                buf[pos],
                buf[pos + 1],
                buf[pos + 2],
                buf[pos + 3],
            )));
        }
        pos += rdlen;
    }
    Ok(ips)
}

/// Advance past a DNS name (handling a compression pointer as a terminator).
fn skip_name(buf: &[u8], mut pos: usize) -> Result<usize, NetError> {
    loop {
        let len = *buf
            .get(pos)
            .ok_or_else(|| NetError::Dns("truncated name".into()))?;
        if len == 0 {
            return Ok(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            return Ok(pos + 2); // pointer: two bytes, name ends
        }
        pos += 1 + len as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_transient_doh_statuses() {
        for s in [500, 502, 503, 504, 429] {
            assert!(is_retriable_status(s), "{s} should be retried");
        }
        for s in [200, 400, 403, 404, 451] {
            assert!(!is_retriable_status(s), "{s} should NOT be retried");
        }
    }

    #[test]
    fn encodes_a_well_formed_query() {
        let q = encode_query("example.com").unwrap();
        assert_eq!(&q[0..2], &0x1234u16.to_be_bytes()); // id
        assert_eq!(&q[4..6], &1u16.to_be_bytes()); // qdcount = 1
                                                   // QNAME: 7 'example' 3 'com' 0
        assert_eq!(q[12], 7);
        assert_eq!(&q[13..20], b"example");
        assert_eq!(q[20], 3);
        assert_eq!(&q[21..24], b"com");
        assert_eq!(q[24], 0);
        assert_eq!(&q[q.len() - 4..], &[0, 1, 0, 1]); // QTYPE A, QCLASS IN
    }

    #[test]
    fn decodes_an_a_record() {
        // header: id, flags, qd=1, an=1, ns=0, ar=0
        let mut msg = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        // question: 1.2 (compressed-free) "a" 0, type A, class IN
        msg.extend_from_slice(&[1, b'a', 0, 0, 1, 0, 1]);
        // answer: name pointer 0xC00C, type A, class IN, ttl, rdlen 4, 93.184.216.34
        msg.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
        let ips = decode_a_records(&msg).unwrap();
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
    }
}

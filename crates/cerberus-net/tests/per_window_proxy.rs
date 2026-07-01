//! End-to-end proof that per-window egress proxies route each instance's
//! traffic through its own CONNECT proxy, exercised through the *public*
//! `cerberus_net` API (`Router::with_proxies` + the `HttpClient::get_in` trait)
//! the way the app's mirror driver uses it — not the crate-internal engine
//! tests.
//!
//! Two real localhost TCP listeners act as minimal CONNECT proxies, each
//! serving a distinct body. Instance 1 is mapped to proxy 1 and instance 2 to
//! proxy 2; a `get_in` for each instance must come back with the body its own
//! proxy served, proving the instance→proxy routing holds across the boundary.
//! The DNS resolver panics if the *target* host is ever looked up, so a passing
//! test also proves no target DNS leak beside the tunnel.

use cerberus_net::{
    DnsResolver, FetchContext, FetchKind, HttpClient, NetError, ProxyConfig, ReadWrite, Router,
    TlsProvider,
};
use cerberus_types::InstanceId;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;

/// TLS provider that must never be reached: all test traffic is plain HTTP, so
/// any TLS handshake would be a bug.
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

/// DNS that fails the test if anything is ever resolved. A proxied fetch names
/// only `host:port` to the proxy and resolves nothing locally, so any call here
/// is a target (or literal-IP proxy) DNS leak.
struct NoDns;
impl DnsResolver for NoDns {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        panic!("DNS leak: tried to resolve {host:?} while proxied");
    }
}

/// Read a request/preamble up to (and including) the blank-line terminator.
fn read_headers(stream: &mut TcpStream) -> String {
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

/// A mock CONNECT proxy that accepts one tunnel, verifies the `CONNECT
/// host:port` preamble, sends `200 Connection Established`, reads the tunneled
/// request, then serves `body` as the tunneled response. Returns its port.
fn spawn_connect_proxy(body: &'static [u8]) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // The CONNECT preamble names host:port — never a path.
        let connect = read_headers(&mut s);
        assert!(
            connect.starts_with("CONNECT example.test:80 HTTP/1.1\r\n"),
            "CONNECT line: {connect:?}"
        );
        s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .unwrap();
        // The tunneled plain-http request flows through unchanged.
        let tunneled = read_headers(&mut s);
        assert!(tunneled.starts_with("GET /x HTTP/1.1\r\n"), "{tunneled:?}");
        assert!(tunneled.contains("Host: example.test\r\n"), "{tunneled:?}");
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
    });
    (port, handle)
}

#[test]
fn two_instances_egress_through_their_own_proxies_via_the_public_router() {
    // Per-window proxy, end-to-end through the public `Router`/`HttpClient`
    // surface: two instances, two proxies. Each instance's `get_in` must come
    // back with the body *its* proxy served — proving instance→proxy routing —
    // while sharing one Router and never resolving the target (NoDns panics).
    let (port_1, srv_1) = spawn_connect_proxy(b"via-proxy-1");
    let (port_2, srv_2) = spawn_connect_proxy(b"via-proxy-2");

    let inst_1 = InstanceId::from_u64_pair(0, 1);
    let inst_2 = InstanceId::from_u64_pair(0, 2);

    let mut proxies = HashMap::new();
    proxies.insert(
        inst_1,
        ProxyConfig {
            host: "127.0.0.1".into(),
            port: port_1,
        },
    );
    proxies.insert(
        inst_2,
        ProxyConfig {
            host: "127.0.0.1".into(),
            port: port_2,
        },
    );

    let router = Router::with_proxies(
        Box::new(NoTls),
        Box::new(NoDns),
        None,
        None, // no default proxy: an unmapped instance would go direct.
        proxies,
    );

    let url = cerberus_url::parse("http://example.test/x").unwrap();
    let resp_1 = router
        .get_in(
            &url,
            &FetchContext {
                instance: inst_1,
                kind: FetchKind::Navigation,
            },
        )
        .unwrap();
    let resp_2 = router
        .get_in(
            &url,
            &FetchContext {
                instance: inst_2,
                kind: FetchKind::Navigation,
            },
        )
        .unwrap();

    srv_1.join().unwrap();
    srv_2.join().unwrap();

    assert_eq!(resp_1.status, 200);
    assert_eq!(resp_2.status, 200);
    assert_eq!(resp_1.body, b"via-proxy-1", "instance 1 used proxy 1");
    assert_eq!(resp_2.body, b"via-proxy-2", "instance 2 used proxy 2");
}

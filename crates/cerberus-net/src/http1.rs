//! Minimal HTTP/1.1 over an established stream (bootstrapped — no deps).
//!
//! We always send `Connection: close` and read the response to EOF, decoding
//! `Transfer-Encoding: chunked` when present. This is intentionally small: it is
//! the wire codec only; connection setup, TLS, DNS, redirects, and caching live
//! in [`crate::engine`]. HTTP/2 and compression are later work.

use crate::{HttpResponse, NetError, ReadWrite};
use std::io::ErrorKind;

/// A request to write to a stream.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub user_agent: &'a str,
    /// Extra headers (besides Host/User-Agent/Accept/Connection/Content-Length).
    pub headers: &'a [(&'a str, &'a str)],
    /// Request body (empty for GET).
    pub body: &'a [u8],
}

/// Write `req` to `stream` and read the full response.
pub fn send(stream: &mut dyn ReadWrite, req: &Request<'_>) -> Result<HttpResponse, NetError> {
    // `Accept-Language` is sent on every request and is uniform across all users
    // (no per-user locale entropy); it matches the script-visible
    // `navigator.language`/`languages` so the header and the DOM agree.
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nAccept-Language: en-US,en;q=0.9\r\nAccept-Encoding: gzip, deflate\r\nConnection: close\r\n",
        req.method, req.path, req.host, req.user_agent
    );
    for (k, v) in req.headers {
        // Sink-side guard (issue #57): every header line is validated here,
        // regardless of where it originated (page-controlled `fetch()`
        // headers, a `Set-Cookie`-derived `Cookie:` value, or a header built
        // internally). A page can smuggle a CR/LF/NUL byte into a header
        // *value* past `is_engine_owned_header`'s name-only allow-list (e.g.
        // `"X": "a\r\nCookie: stolen=1"`) to inject an arbitrary extra header
        // or split the request; rejecting here — the one chokepoint every
        // caller funnels through — closes that off for good instead of
        // requiring every call site to remember to sanitize.
        validate_header_name(k)?;
        validate_header_value(v)?;
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !req.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).map_err(io_err)?;
    if !req.body.is_empty() {
        stream.write_all(req.body).map_err(io_err)?;
    }
    stream.flush().map_err(io_err)?;

    let raw = read_to_end_tolerant(stream)?;
    parse_response(&raw)
}

/// Map a socket I/O error to a [`NetError`], giving a clear message for a
/// read/write timeout (`WouldBlock`/`TimedOut`, i.e. EAGAIN from the socket's
/// `SO_RCVTIMEO`/`SO_SNDTIMEO`) instead of the opaque "Resource temporarily
/// unavailable (os error 11)". Because rustls handshakes lazily on the first
/// I/O, a timeout during the request write is the TLS handshake stalling on an
/// unresponsive server — hence the general wording.
fn io_err(e: std::io::Error) -> NetError {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        NetError::Io("connection timed out: the server did not respond in time".into())
    } else {
        NetError::Io(e.to_string())
    }
}

/// Reject a header name that is not a valid RFC 7230 `token` (the grammar a
/// header field-name must satisfy): non-empty, and every byte one of the
/// allowed `tchar`s (`[!#$%&'*+\-.^_`|~0-9A-Za-z]`). This excludes `:`,
/// whitespace, and all CTLs — in particular CR/LF/NUL — so a name can never
/// terminate the header line early or introduce a new one.
fn validate_header_name(name: &str) -> Result<(), NetError> {
    fn is_tchar(b: u8) -> bool {
        matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
            | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
    }
    if !name.is_empty() && name.bytes().all(is_tchar) {
        Ok(())
    } else {
        Err(NetError::Protocol(format!("invalid header name {name:?}")))
    }
}

/// Reject a header value containing a CR, LF, or NUL byte. These are the
/// bytes that let a page-controlled (or otherwise untrusted) header value
/// break out of its own line — injecting an extra header (e.g. smuggling a
/// `Cookie:` line past [`crate::engine`]'s name-only allow-list) or splitting/
/// desyncing the request entirely. This is a byte-validity check only: it does
/// not second-guess *which* headers are allowed (see `is_engine_owned_header`
/// in [`crate::engine`], which is unrelated and untouched by this).
fn validate_header_value(value: &str) -> Result<(), NetError> {
    if value.bytes().any(|b| matches!(b, 0x0D | 0x0A | 0x00)) {
        Err(NetError::Protocol(format!(
            "header value contains CR, LF, or NUL: {value:?}"
        )))
    } else {
        Ok(())
    }
}

/// Hard cap on a single response (raw bytes off the wire, pre-decompression):
/// a large or hostile/endless response must not OOM the process. 32 MiB is far
/// above any real page/subresource while bounding worst-case memory.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Read until EOF, tolerating a TLS peer that closes without `close_notify`
/// (common) — we already have the body in that case. Aborts past
/// [`MAX_RESPONSE_BYTES`] so a huge/endless response can't exhaust memory.
fn read_to_end_tolerant(stream: &mut dyn ReadWrite) -> Result<Vec<u8>, NetError> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err(NetError::Protocol(format!(
                        "response exceeds {MAX_RESPONSE_BYTES} bytes"
                    )));
                }
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // A socket read timeout / not-ready read (`WouldBlock`/`TimedOut`,
            // i.e. EAGAIN from `SO_RCVTIMEO`). We send `Connection: close`, so a
            // well-behaved peer closes after the body — but an intermediary
            // (e.g. a CONNECT proxy) may hold the connection open, leaving our
            // read to stall *after* the whole response has already arrived. If
            // what we have is a complete response, return it. Otherwise the
            // server stalled mid-response: surface a clear timeout rather than
            // the opaque "Resource temporarily unavailable (os error 11)".
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if response_is_complete(&raw) {
                    break;
                }
                return Err(NetError::Io(
                    "read timed out: the server stalled before the response completed".into(),
                ));
            }
            Err(e) => return Err(NetError::Io(e.to_string())),
        }
    }
    Ok(raw)
}

/// Whether `raw` already holds a complete HTTP/1.1 response: a full header block
/// plus a body satisfying its framing — `Content-Length` reached, a chunked
/// stream closed by its `0`-size chunk, or a status that carries no body
/// (`1xx`/`204`/`304`). Used to tell a benign "peer went quiet after sending the
/// whole response" apart from a truncating mid-body stall when a read times out.
/// A response with no length framing is delimited by connection close, so it is
/// only "complete" at EOF and this returns `false` for it.
fn response_is_complete(raw: &[u8]) -> bool {
    let Some(sep) = find(raw, b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&raw[..sep]) else {
        return false;
    };
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| parse_status(l).ok());
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse::<usize>().ok();
            } else if k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    // 1xx / 204 / 304 never carry a body, regardless of headers.
    if matches!(status, Some(s) if (100..200).contains(&s) || s == 204 || s == 304) {
        return true;
    }
    let body = &raw[sep + 4..];
    if chunked {
        chunked_complete(body)
    } else if let Some(cl) = content_length {
        body.len() >= cl
    } else {
        false
    }
}

/// Whether `body` is a complete chunked stream (walks the chunk framing and
/// reaches the terminating `0`-size chunk). Conservative: any malformed or
/// truncated framing returns `false` (treat as not-yet-complete).
fn chunked_complete(mut body: &[u8]) -> bool {
    loop {
        let Some(nl) = find(body, b"\r\n") else {
            return false;
        };
        let size_hex = std::str::from_utf8(&body[..nl])
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            return false;
        };
        body = &body[nl + 2..];
        if size == 0 {
            return true;
        }
        if body.len() < size {
            return false;
        }
        body = &body[size..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, NetError> {
    if raw.is_empty() {
        return Err(NetError::Protocol(
            "empty response: the server closed the connection without sending anything".into(),
        ));
    }
    let sep =
        find(raw, b"\r\n\r\n").ok_or_else(|| NetError::Protocol("no header terminator".into()))?;
    let head = std::str::from_utf8(&raw[..sep])
        .map_err(|_| NetError::Protocol("non-utf8 headers".into()))?;
    let mut lines = head.split("\r\n");

    let status = parse_status(lines.next().unwrap_or(""))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let body = raw[sep + 4..].to_vec();
    let chunked = header(&headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let body = if chunked { dechunk(&body)? } else { body };
    // Decode the body per Content-Encoding (gzip/deflate); identity / absent
    // pass through (ADR-0020).
    let body = match header(&headers, "content-encoding") {
        Some(enc) if !enc.is_empty() => crate::decompress::decode(enc, body)?,
        _ => body,
    };

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn parse_status(line: &str) -> Result<u16, NetError> {
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| NetError::Protocol(format!("bad status line: {line:?}")))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn dechunk(body: &[u8]) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let nl = find(rest, b"\r\n").ok_or_else(|| NetError::Protocol("bad chunk size".into()))?;
        let size_field = std::str::from_utf8(&rest[..nl]).unwrap_or("");
        let size_hex = size_field.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| NetError::Protocol("bad chunk size".into()))?;
        rest = &rest[nl + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size {
            return Err(NetError::Protocol("chunk truncated".into()));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    Ok(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello";
        let resp = parse_response(raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.content_type(), Some("text/html"));
    }

    #[test]
    fn decodes_chunked_body() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let resp = parse_response(raw).unwrap();
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_response(b"not http").is_err());
    }

    #[test]
    fn response_completeness_respects_framing() {
        // Content-Length satisfied / not.
        assert!(response_is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi"
        ));
        assert!(!response_is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhi"
        ));
        // Headers not yet fully received.
        assert!(!response_is_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
        // Bodyless statuses are complete once the headers are in.
        assert!(response_is_complete(b"HTTP/1.1 304 Not Modified\r\n\r\n"));
        assert!(response_is_complete(b"HTTP/1.1 204 No Content\r\n\r\n"));
        // Chunked: complete only once the 0-size terminator arrives.
        assert!(response_is_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n"
        ));
        assert!(!response_is_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n"
        ));
        // No length framing (close-delimited) is never "complete" without EOF.
        assert!(!response_is_complete(b"HTTP/1.1 200 OK\r\n\r\nsome body"));
    }

    #[test]
    fn read_tolerates_a_timeout_after_a_complete_response() {
        use std::io::{Read, Write};
        // A peer that hands back one complete (Content-Length) response and then
        // stalls with a read timeout (EAGAIN) — as a CONNECT proxy holding the
        // connection open would. The full response must be returned, not failed.
        struct HeldOpen {
            sent: bool,
        }
        impl Read for HeldOpen {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.sent {
                    return Err(std::io::Error::from(ErrorKind::WouldBlock));
                }
                self.sent = true;
                let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
                buf[..resp.len()].copy_from_slice(resp);
                Ok(resp.len())
            }
        }
        impl Write for HeldOpen {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let raw = read_to_end_tolerant(&mut HeldOpen { sent: false }).expect("complete response");
        let resp = parse_response(&raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hi");
    }

    #[test]
    fn read_still_errors_on_a_timeout_mid_body() {
        use std::io::{Read, Write};
        // A timeout while the body is still short of its Content-Length is a
        // genuine truncation and must stay an error (not silently truncated).
        struct StallMidBody {
            sent: bool,
        }
        impl Read for StallMidBody {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.sent {
                    return Err(std::io::Error::from(ErrorKind::WouldBlock));
                }
                self.sent = true;
                let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhi";
                buf[..resp.len()].copy_from_slice(resp);
                Ok(resp.len())
            }
        }
        impl Write for StallMidBody {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = read_to_end_tolerant(&mut StallMidBody { sent: false }).unwrap_err();
        match err {
            NetError::Io(msg) => assert!(
                msg.contains("timed out"),
                "a mid-body stall should read as a timeout, got {msg:?}"
            ),
            other => panic!("expected Io timeout, got {other:?}"),
        }
    }

    #[test]
    fn aborts_oversized_response() {
        use std::io::{Read, Write};
        // A peer that never stops sending must be cut off, not allowed to OOM.
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
        }
        impl Write for Endless {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = read_to_end_tolerant(&mut Endless).unwrap_err();
        assert!(matches!(err, NetError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn request_carries_uniform_identity_headers() {
        use std::io::{Cursor, Read, Write};

        struct Mock {
            written: Vec<u8>,
            resp: Cursor<Vec<u8>>,
        }
        impl Read for Mock {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.resp.read(buf)
            }
        }
        impl Write for Mock {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut mock = Mock {
            written: Vec::new(),
            resp: Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec()),
        };
        let resp = send(
            &mut mock,
            &Request {
                method: "GET",
                host: "example.test",
                path: "/p",
                user_agent: "Cerberus/0.0",
                headers: &[],
                body: &[],
            },
        )
        .unwrap();
        assert_eq!(resp.status, 200);

        let req = String::from_utf8(mock.written).unwrap();
        assert!(
            req.starts_with("GET /p HTTP/1.1\r\n"),
            "request line: {req:?}"
        );
        assert!(req.contains("Host: example.test\r\n"));
        assert!(req.contains("User-Agent: Cerberus/0.0\r\n"));
        // Uniform locale, matching navigator.language/languages (no per-user
        // entropy) so the header and the script-visible identity agree.
        assert!(
            req.contains("Accept-Language: en-US,en;q=0.9\r\n"),
            "missing uniform Accept-Language: {req:?}"
        );
    }

    /// A minimal in-memory `Read + Write` used to capture what `send` writes
    /// to the wire, and hand back a canned response (mirrors the `Mock` in
    /// `request_carries_uniform_identity_headers`).
    struct Mock {
        written: Vec<u8>,
        resp: std::io::Cursor<Vec<u8>>,
    }
    impl std::io::Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.resp.read(buf)
        }
    }
    impl std::io::Write for Mock {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn mock_ok() -> Mock {
        Mock {
            written: Vec::new(),
            resp: std::io::Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec()),
        }
    }

    #[test]
    fn rejects_crlf_injected_header_value() {
        // A page-controlled header value carrying "\r\nX-Injected: 1" must not
        // reach the wire as a second header line (issue #57): either `send`
        // errors, or — if it somehow didn't — the serialized request must not
        // contain the injected line. We assert the stronger property: an error.
        let mut mock = mock_ok();
        let err = send(
            &mut mock,
            &Request {
                method: "GET",
                host: "example.test",
                path: "/p",
                user_agent: "Cerberus/0.0",
                headers: &[("X", "a\r\nX-Injected: 1")],
                body: &[],
            },
        )
        .unwrap_err();
        assert!(matches!(err, NetError::Protocol(_)), "got {err:?}");
        assert!(
            !String::from_utf8_lossy(&mock.written).contains("X-Injected"),
            "the injected header must never reach the wire"
        );
    }

    #[test]
    fn rejects_nul_byte_in_header_value() {
        let mut mock = mock_ok();
        let err = send(
            &mut mock,
            &Request {
                method: "GET",
                host: "example.test",
                path: "/p",
                user_agent: "Cerberus/0.0",
                headers: &[("X", "a\0b")],
                body: &[],
            },
        )
        .unwrap_err();
        assert!(matches!(err, NetError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn rejects_invalid_header_name() {
        // A name containing a colon or CTL is not a valid RFC 7230 token.
        let mut mock = mock_ok();
        let err = send(
            &mut mock,
            &Request {
                method: "GET",
                host: "example.test",
                path: "/p",
                user_agent: "Cerberus/0.0",
                headers: &[("X-Bad:\r\nHeader", "1")],
                body: &[],
            },
        )
        .unwrap_err();
        assert!(matches!(err, NetError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn well_formed_header_round_trips_unchanged() {
        let mut mock = mock_ok();
        send(
            &mut mock,
            &Request {
                method: "GET",
                host: "example.test",
                path: "/p",
                user_agent: "Cerberus/0.0",
                headers: &[("X-Custom", "value")],
                body: &[],
            },
        )
        .unwrap();
        let req = String::from_utf8(mock.written).unwrap();
        assert!(
            req.contains("X-Custom: value\r\n"),
            "well-formed header missing: {req:?}"
        );
    }

    #[test]
    fn validate_header_name_accepts_tokens_and_rejects_separators() {
        assert!(validate_header_name("Content-Type").is_ok());
        assert!(validate_header_name("X-Foo_Bar.Baz~1").is_ok());
        assert!(validate_header_name("").is_err());
        assert!(validate_header_name("Bad:Name").is_err());
        assert!(validate_header_name("Bad Name").is_err());
        assert!(validate_header_name("Bad\r\nName").is_err());
    }

    #[test]
    fn validate_header_value_rejects_cr_lf_nul() {
        assert!(validate_header_value("normal value").is_ok());
        assert!(validate_header_value("a\r\nInjected: 1").is_err());
        assert!(validate_header_value("a\nb").is_err());
        assert!(validate_header_value("a\0b").is_err());
    }
}

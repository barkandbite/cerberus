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
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\nAccept-Language: en-US,en;q=0.9\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        req.method, req.path, req.host, req.user_agent
    );
    for (k, v) in req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !req.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .map_err(|e| NetError::Io(e.to_string()))?;
    if !req.body.is_empty() {
        stream
            .write_all(req.body)
            .map_err(|e| NetError::Io(e.to_string()))?;
    }
    stream.flush().map_err(|e| NetError::Io(e.to_string()))?;

    let raw = read_to_end_tolerant(stream)?;
    parse_response(&raw)
}

/// Read until EOF, tolerating a TLS peer that closes without `close_notify`
/// (common) — we already have the body in that case.
fn read_to_end_tolerant(stream: &mut dyn ReadWrite) -> Result<Vec<u8>, NetError> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(NetError::Io(e.to_string())),
        }
    }
    Ok(raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, NetError> {
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
}

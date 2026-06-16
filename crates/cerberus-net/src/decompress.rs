//! HTTP response body decompression (ADR-0020).
//!
//! A small bytes → bytes seam over `miniz_oxide` (the cached, pure-Rust inflate
//! already pulled in by the image decoder). No foreign type crosses the module
//! boundary — callers see only `Vec<u8>` / [`NetError`]. `gzip` and `deflate`
//! are decoded; `identity`/empty pass through; anything else errors.

use crate::NetError;

/// Hard cap on a decompressed body — a coarse decompression-bomb guard. A
/// stream-bounded inflate is the proper fix (follow-up); this still stops a
/// body from ballooning unbounded into resident memory.
const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// Decode a response body per its `Content-Encoding`.
pub(crate) fn decode(encoding: &str, body: Vec<u8>) -> Result<Vec<u8>, NetError> {
    let out = match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => return Ok(body),
        "gzip" | "x-gzip" => inflate_gzip(&body)?,
        "deflate" => inflate_deflate(&body)?,
        other => {
            return Err(NetError::Protocol(format!(
                "unsupported content-encoding: {other}"
            )))
        }
    };
    if out.len() > MAX_DECOMPRESSED {
        return Err(NetError::Protocol(format!(
            "decompressed body exceeds {MAX_DECOMPRESSED} bytes"
        )));
    }
    Ok(out)
}

fn decomp_err(e: impl core::fmt::Debug) -> NetError {
    NetError::Protocol(format!("decompression failed: {e:?}"))
}

/// Inflate a gzip stream (RFC 1952): parse the header, raw-inflate the deflate
/// body. The CRC32/ISIZE trailer is not verified.
fn inflate_gzip(b: &[u8]) -> Result<Vec<u8>, NetError> {
    if b.len() < 18 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 8 {
        return Err(NetError::Protocol("bad gzip header".into()));
    }
    let flg = b[3];
    let mut i = 10usize;
    if flg & 0b0000_0100 != 0 {
        // FEXTRA: a 2-byte length then that many bytes.
        if i + 2 > b.len() {
            return Err(NetError::Protocol("truncated gzip FEXTRA".into()));
        }
        let xlen = u16::from_le_bytes([b[i], b[i + 1]]) as usize;
        i += 2 + xlen;
    }
    if flg & 0b0000_1000 != 0 {
        i = skip_cstr(b, i)?; // FNAME
    }
    if flg & 0b0001_0000 != 0 {
        i = skip_cstr(b, i)?; // FCOMMENT
    }
    if flg & 0b0000_0010 != 0 {
        i += 2; // FHCRC
    }
    let end = b
        .len()
        .checked_sub(8)
        .filter(|e| *e >= i)
        .ok_or_else(|| NetError::Protocol("truncated gzip body".into()))?;
    miniz_oxide::inflate::decompress_to_vec(&b[i..end]).map_err(decomp_err)
}

fn skip_cstr(b: &[u8], mut i: usize) -> Result<usize, NetError> {
    while i < b.len() && b[i] != 0 {
        i += 1;
    }
    if i >= b.len() {
        return Err(NetError::Protocol("unterminated gzip string field".into()));
    }
    Ok(i + 1)
}

/// Inflate a `Content-Encoding: deflate` body — zlib-wrapped per the spec, but
/// some servers send raw deflate, so fall back to that.
fn inflate_deflate(b: &[u8]) -> Result<Vec<u8>, NetError> {
    miniz_oxide::inflate::decompress_to_vec_zlib(b)
        .or_else(|_| miniz_oxide::inflate::decompress_to_vec(b))
        .map_err(decomp_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        // 10-byte header (FLG=0), raw deflate, 8-byte (unverified) trailer.
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 0xff];
        out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
        out.extend([0u8; 8]);
        out
    }

    #[test]
    fn decodes_gzip() {
        let data = b"hello, gzip world ".repeat(50);
        assert_eq!(decode("gzip", gzip(&data)).unwrap(), data);
    }

    #[test]
    fn decodes_deflate_zlib_and_raw() {
        let data = b"deflate payload ".repeat(40);
        let zlib = miniz_oxide::deflate::compress_to_vec_zlib(&data, 6);
        assert_eq!(decode("deflate", zlib).unwrap(), data);
        let raw = miniz_oxide::deflate::compress_to_vec(&data, 6);
        assert_eq!(decode("deflate", raw).unwrap(), data);
    }

    #[test]
    fn identity_passes_through_unknown_errors() {
        assert_eq!(decode("identity", b"abc".to_vec()).unwrap(), b"abc");
        assert_eq!(decode("", b"abc".to_vec()).unwrap(), b"abc");
        assert!(decode("br", b"abc".to_vec()).is_err());
    }

    #[test]
    fn rejects_bad_gzip_header() {
        assert!(decode("gzip", vec![0u8; 32]).is_err());
    }
}

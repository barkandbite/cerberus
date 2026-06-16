# ADR-0020: gzip/deflate response decompression (miniz_oxide)

- Status: Accepted
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Amends: ADR-0003 (dependency policy — adds `miniz_oxide` as a direct dep)
- Related: ADR-0006 (networking), the production-readiness review, #6

## Context

The HTTP/1.1 client advertised `Accept-Encoding: identity` and could not decode
compressed bodies. Real servers and CDNs serve gzip (and some `deflate`)
heavily; identity-only means larger transfers and outright failures where a CDN
ignores the request and gzips anyway. HTTP/2 is deferred (its own milestone).

## Decision

Advertise `Accept-Encoding: gzip, deflate` and decode the response body by its
`Content-Encoding`, via **`miniz_oxide`** — a pure-Rust, `unsafe`-free inflate
that is **already in the dependency tree** (transitive through the image
decoder's PNG path) and cached, so promoting it to a direct dependency adds no
download and no new license to vet.

- Wrapped behind a small bytes → bytes module (`cerberus-net::decompress`); no
  foreign type crosses the boundary (callers see `Vec<u8>` / `NetError`),
  honoring ADR-0001.
- `gzip`: parse the RFC-1952 header (FLG/FEXTRA/FNAME/FCOMMENT/FHCRC) and
  raw-inflate the deflate body (CRC32/ISIZE trailer not verified).
- `deflate`: zlib-wrapped per spec, with a raw-deflate fallback for servers that
  send it bare.
- `identity`/absent pass through; any other encoding (`br`, …) errors.
- A 64 MB decompressed-size cap is a coarse decompression-bomb guard; a
  stream-bounded inflate is a documented follow-up.

## Consequences

- **Easier:** real-world loads shrink and previously-failing gzip-only endpoints
  work; the change is confined to `cerberus-net`.
- **Harder:** one new direct dependency (already vetted/cached) and the gzip
  header parser. The bomb guard is post-hoc, not streaming (follow-up).
- **Reversible:** internal to `cerberus-net::{decompress, http1}`.

## Alternatives considered

- **Hand-roll RFC-1951 inflate (zero new deps).** Preserves the bootstrap ethos
  but is ~500+ lines of correctness-critical code; rejected given a vetted,
  already-present pure-Rust implementation.
- **`flate2`.** Also cached, with a higher-level gzip API, but it is a wrapper
  over `miniz_oxide` — using `miniz_oxide` directly is the leaner choice.
- **HTTP/2 now.** Deferred — large (framing, HPACK, multiplexing, ALPN) and not
  required for correctness.

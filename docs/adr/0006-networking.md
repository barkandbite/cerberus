# ADR-0006: M1 networking — HTTP/1.1, TLS, DoH

- Status: Accepted
- Date: 2026-06-09
- Deciders: bbarker@barkbite.org (approved the choices below), engineering

## Context

M1 makes Cerberus fetch real pages: an HTTP/1.1 client, TLS, and DNS — privately.
TLS and the crypto behind DoH are exactly the "lean on an audited crate" cases
from ADR-0003; the HTTP/1.1 codec, the DoH query/response wire format, the
redirect logic, and the cache are ours.

## Decision

### Module layout (ports & adapters)

- **`cerberus-net`** (bootstrapped, no new deps): the `http1` request/response
  codec (Content-Length + chunked), `HttpEngine` (`impl HttpClient`: DNS → TCP →
  optional TLS → request → capped redirects), and `Router` (built-in `cerberus:`
  pages local, `http(s)` to the engine). Fixed minimal `User-Agent`
  (`Cerberus/0.0`) — **no browser impersonation** (threat-model non-goal).
- **`cerberus-tls-rustls`** (new adapter): `TlsProvider` via rustls. No rustls
  type crosses the boundary (returns `Box<dyn ReadWrite>`).
- **`cerberus-dns-doh`** (new adapter): `DnsResolver` via DoH (RFC 8484 wire
  format), reusing `http1` over a `TlsProvider`. DoH-only, no plaintext fallback.

### Owner decisions (2026-06-09)

| Decision | Choice |
| --- | --- |
| TLS stack | **rustls + `ring` backend + bundled `webpki-roots`** (no cmake/C++, reproducible, system-independent). |
| DoH resolver | **Quad9** (9.9.9.9, SNI `dns.quad9.net`) — privacy-focused, no-logging. |
| HTTP scheme policy | **https-first**: try https; if unavailable, **prompt the user to accept the risk**, and only then allow plaintext `http` — otherwise block. Headless blocks silently. |
| Load model | **Background thread** for page loads in the window (UI stays responsive; Stop cancels). Headless `render` is synchronous. |

### New dependencies (approved)

`rustls` (0.23, `ring`/`std`/`tls12`), `webpki-roots` (1.x), and their transitive
crates (`ring`, `rustls-webpki`, `rustls-pki-types`, `untrusted`, `subtle`,
`getrandom`, `zeroize`). The HTTP/1.1 codec, DoH wire format, redirects, and
cache add **no** dependencies.

### Privacy specifics

- **Cache is partitioned per `InstanceId`** (a shared HTTP cache is a cross-site
  tracking vector — sealed like cookies). Honors `Cache-Control: max-age/no-store`.
- DoH-only resolution; no plaintext DNS ever.
- Fixed UA; `Accept-Encoding: identity` for now.
- `RustlsProvider::with_system_roots()` is a **non-default** option for users
  behind a TLS-inspecting corporate/egress proxy (whose CA is installed
  system-wide). The default rejects such interception — verified: in the build
  sandbox, outbound TLS is MITM'd by an internal egress CA, and the bundled-roots
  default correctly returns `UnknownIssuer`.

## Out of scope (later milestones)

Cookie attach / `Set-Cookie` capture (M4), consent on third-party subresources
(M5), gzip/brotli decompression (a later dependency decision), HTTP/2, connection
keep-alive/pooling.

## Status / verification

- Fetch path **wired and live-verified**: `https://example.com` and
  `https://www.rust-lang.org` return 200 end-to-end through DoH → rustls →
  HTTP/1.1 → parse → layout → paint (via `render --url … --system-roots` in the
  MITM sandbox).
- Hermetic unit tests cover the `http1` parser (Content-Length + chunked) and the
  DoH query/response codec; the network itself is not exercised by `cargo test`.
- **Wired (windowed):** the per-instance `HttpCache`, the https→prompt→block
  policy, and background-thread loading (a worker thread + an event-loop `Waker`)
  are in `BrowserApp`. The load state machine (https upgrade, risk prompt, cache
  hit, Stop-cancels) is covered by hermetic tests via a `FakeLoader`; the window
  itself needs a display to run.

## Consequences

- **Easier:** real pages load over an auditable, memory-safe TLS stack with
  private resolution; each piece is swappable behind its trait.
- **Costs:** rustls/ring enlarge the tree (justified — TLS is a top CVE surface);
  our own HTTP/1.1 + DoH codecs are ours to keep correct (covered by tests).

## Alternatives considered

- **aws-lc-rs backend:** rejected — needs cmake/C++ to build; ring is simpler and
  reproducible here.
- **System/native roots by default:** rejected for reproducibility; offered as a
  non-default option for proxy environments.
- **JSON DoH** (`application/dns-json`): rejected — RFC 8484 wire format is
  provider-neutral and standard; the encoder/decoder is small.
- **A blocking fetch on the UI thread:** rejected — freezes the window; a worker
  thread is proper browser behavior.

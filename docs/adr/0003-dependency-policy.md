# ADR-0003: Dependency policy & initial approved list

- Status: Proposed
- Date: 2026-06-08
- Deciders: bbarker@barkbite.org (owner), engineering

## Context

A privacy browser people are asked to trust must keep its dependency tree small,
auditable, and reproducible. But some subsystems are *more* dangerous to
hand-roll than to delegate — historically crypto, TLS, JS engines, and
font/image decoders are the largest browser CVE sources.

## Decision

### Principles

1. **Default to bootstrapping.** Write it ourselves unless a dependency is the
   *smaller* security risk.
2. **No crate enters the tree without an ADR and the owner's approval.** The ADR
   states: what it does, why we shouldn't bootstrap it now, its license, its
   maintenance/audit status, and the trait it will sit behind.
3. **Every dependency sits behind one of our traits** in a dedicated adapter
   crate (ADR-0001). No vendor type crosses a boundary.
4. **Lean on dependencies only for** crypto, TLS, the JS engine, and
   font/shaping/image decoding. Everything else (shell, event loop, networking
   logic, URL, HTML parser, DOM, layout, cookie store, vault logic, cookie
   groups, isolation, consent, farbling, identity manager, headless) is ours.

### Approval workflow

Propose → ADR (with the fields above) → owner approves → add crate **and** its
adapter in the same PR → `deny.toml` license/advisory check passes → `Cargo.lock`
committed.

### Initial approved list (each behind a trait)

| Dependency | Trait / crate | Status | License | Why not bootstrap |
| --- | --- | --- | --- | --- |
| `rustls` (+ `rustls-pki-types`, a verifier) | `TlsProvider` / `cerberus-tls-rustls` | **Approved**, wire at M1 | Apache-2.0 / MIT / ISC | TLS is a top CVE surface; memory-safe, audited |
| V8 via `v8` (rusty_v8) | `JsEngine` / `cerberus-js-v8` | **Approved** (ADR-0002), wire at M3 | BSD-3-Clause | Engine is infeasible & dangerous to write |

### Proposed for the vault (needs your approval — M4)

We do **not** hand-roll AEAD or the KDF. Proposed specific crates, all RustCrypto,
widely used, with published audits/RFC test vectors:

| Purpose | Proposed crate | License | Notes |
| --- | --- | --- | --- |
| AEAD | `chacha20poly1305` (XChaCha20-Poly1305) | Apache-2.0 / MIT | RFC 8439; 24-byte XNonce eases nonce management for at-rest blobs. `aes-gcm` is the alternative if hardware-AES is preferred. |
| KDF | `argon2` | Apache-2.0 / MIT | Argon2id, as mandated; tunable memory/time cost. |
| Key hygiene | `zeroize` | Apache-2.0 / MIT | Volatile zeroization; replaces the best-effort placeholder in `cerberus-crypto`. |
| Locked memory | `region` **or** direct `libc` `mlock` | Apache-2.0/MIT | For `mlock`/`munlock` of key pages; smallest viable option TBD at M4. |

Behind `Aead` / `Kdf` (already defined in `cerberus-crypto`) plus a small
key-locking helper. Until approved, `cerberus-crypto` ships **only the traits and
zeroizing key types** (best-effort, clearly documented) and **no primitives**;
tests use throwaway in-crate impls that are never shipped.

### Heads-up for M2 (propose then, not now)

Font rasterization, text shaping, and image decoding will need crates. Likely
candidates to evaluate (each behind `Rasterizer`/`TextShaper`/`ImageDecoder`):
shaping — `rustybuzz` or `swash`; rasterization — `ab_glyph`/`fontdue` or `swash`;
image decode — `image` or the `zune-*` family / `png`. Formal proposals at M2.

### Not approved / explicitly bootstrapped

URL, HTML tokenizer/parser, DOM, layout, HTTP/1.1 client, DoH client, cookie
store, vault composition, consent engine, farbling, identity manager, CLI arg
parsing, PPM output. (No `clap`, `serde`, `tokio`, `hyper`, `url`, `html5ever`,
etc. without their own ADR.)

### Enforcement

- `deny.toml`: license allow-list (Apache-2.0, MIT, BSD-2/3, ISC, Unicode-3.0,
  Zlib), advisory denial, duplicate-version warnings, source restriction to
  crates.io. `cargo deny` wired into CI once the tool itself is approved (it adds
  nothing to the browser's own tree).
- `Cargo.lock` committed; CI builds `--locked`.
- Reproducible-build measures per PLAN §8.

## Consequences

- **Easier:** small, reviewable tree; clear story for users/auditors; each risky
  surface is isolated and swappable.
- **Harder:** we write more code (URL, HTML, layout, networking) and own its
  bugs — mitigated by traits, tests, and fuzzing.

## Alternatives considered

- **Use the ecosystem freely** (`url`, `html5ever`, `hyper`, `reqwest`, …).
  Faster to a demo, but bloats the tree and the trust surface and undercuts the
  whole premise. Rejected.
- **Hand-roll crypto/TLS.** Rejected outright — the canonical way to ship a
  catastrophic CVE.

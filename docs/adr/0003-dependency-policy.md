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
| `rustls` (+ `rustls-pki-types`, a verifier) | `TlsProvider` / `cerberus-tls-rustls` | **Approved**, wired at M1 | Apache-2.0 / MIT / ISC | TLS is a top CVE surface; memory-safe, audited |
| `ab_glyph` (+ bundled Roboto) | `TextShaper`/`Rasterizer` / `cerberus-text` | **Approved** (ADR-0005), wired at M2 | Apache-2.0 | Font rasterization is a CVE surface; lean pure-Rust |
| `image` (png/jpeg/gif/webp/bmp) | `ImageDecoder` / `cerberus-image` | **Approved** (ADR-0005), wired at M2 | MIT/Apache-2.0 | Image decoding is a top CVE surface |
| `resvg`/`usvg`/`tiny-skia` (SVG, text feature off) | `ImageDecoder` / `cerberus-image` | **Approved** (ADR-0009), wired post-M3 | Apache-2.0 OR MIT / BSD-3-Clause | SVG is a large spec + CVE surface; logos/icons are SVG on real pages |
| `rquickjs` 0.9 (bundled QuickJS) | `JsEngine` / `cerberus-js-quickjs` | **Approved** (ADR-0002), wiring at M3 | MIT (QuickJS) / MIT (binding) | Engine is infeasible & dangerous to write; QuickJS chosen over V8 for memory |
| ~~V8 via `v8` (rusty_v8)~~ | ~~`cerberus-js-v8`~~ | **Superseded** by QuickJS (ADR-0002, 2026-06-10); kept as a documented swap-in if compat demands | BSD-3-Clause | — |

### Vault crates (Approved & wired at M4, 2026-06-11)

We do **not** hand-roll AEAD or the KDF. The proposed crates were ratified per
the standing decision directive and are wired behind `Aead`/`Kdf` in the
dedicated adapter crate `cerberus-crypto-rustcrypto` (named only by the
composition root):

| Purpose | Crate | Status | License | Notes |
| --- | --- | --- | --- | --- |
| AEAD | `chacha20poly1305` (XChaCha20-Poly1305) | **Approved**, wired M4 | Apache-2.0 / MIT | RFC 8439 + 24-byte XNonce: random nonce per at-rest blob, no counter state. |
| KDF | `argon2` | **Approved**, wired M4 | Apache-2.0 / MIT | Argon2id, m=19 MiB / t=2 / p=1 (OWASP band); cost is transient at unlock. |
| Key hygiene | `zeroize` | **Approved**, wired M4 | Apache-2.0 / MIT | Replaced the best-effort `compiler_fence` placeholder in `cerberus-crypto::Secret`. |
| Locked memory | ~~`region` / `libc` `mlock`~~ | **Rejected for v1** | — | Would require `unsafe` (denied workspace-wide) for a partial mitigation. Key pages may swap; documented in THREAT_MODEL with OS swap encryption as the compensating control. |

`cargo deny check` passes (advisories/bans/licenses/sources) over the resulting
tree. The vault additionally seals a check sentinel at first unlock so a wrong
passphrase fails *at unlock* (and nonces are always random — a persisted
counter would reuse nonces across restarts).

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

## Update — 2026-06-10: deny gate actually enforced (run locally)

CI does not run automatically in the current environment, so `cargo deny check`
is run **locally before any dependency change** rather than relied on in CI. Its
first real run against the post-M0 tree (M1–M3 deps) surfaced three things, now
resolved so all four checks pass (`advisories/bans/licenses/sources ok`):

- **License allow-list extended** (this ADR is the required review). Two
  permissive, non-copyleft licenses ride in with already-approved deps:
  `CDLA-Permissive-2.0` (`webpki-roots`, the Mozilla CA list, via rustls) and
  `0BSD` (`adler2`, the Adler-32 checksum, via the `image` PNG path). Both added.
- **Targets scoped** to the desktop triples we build/ship (`[graph].targets`).
  This drops target-specific stubs we never link — notably `r-efi` (LGPL, UEFI),
  which is not in any platform graph we ship.
- **Workspace crates marked `publish = false`.** They are first-party path deps
  (no version → a `*` requirement); `allow-wildcard-paths` only exempts private
  crates, so marking them unpublished (which they are — this is an application,
  not crates.io libraries) clears the wildcard ban without inventing versions.

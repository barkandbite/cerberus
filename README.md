# Cerberus

A privacy-first, memory-lean web browser, built from the ground up in Rust.

> **Status: M0 scaffold — under review.** The workspace, all subsystem trait
> boundaries, and an end-to-end trivial render are in place; no feature code
> (M1+) has been written yet. See **[PLAN.md](PLAN.md)** and the
> **[ADRs](docs/adr/)**, which are up for sign-off before M1.

## What makes Cerberus different

The differentiator is the **privacy model**, not the renderer:

- **Three identities ("heads")** — work / personal / throwaway — used one at a
  time, each with its own sealed cookie partition and farbling seed.
- **Sealed per-instance cookies** — an instance can only ever resolve *its own*
  cookies. Cross-instance correlation is impossible **by construction**, not by a
  policy check.
- **Quarantine vault** — intercepted/cross-site cookies are held AEAD-encrypted in
  a vault and never attached to a request until you release them. The key is
  derived from your passphrase (Argon2id) and lives nowhere at rest.
- **Cookie groups** — classify cookies as *active* (always available in their
  instance) or *quarantined*; everyday first-party browsing isn't gated.
- **Consent gate** — third-party storage defaults to **deny**, with a prompt in
  headed mode.
- **Farbling** — per-head, per-session bounded noise on fingerprintable surfaces
  (canvas, audio, WebGL, fonts), so trackers can't build a stable cross-site
  identity. This randomizes our *own* surface; it never impersonates another
  browser.

**Memory is priority #1:** one process, one JS engine instance live at a time
(the active identity's), instantiated lazily and torn down on switch.

What Cerberus is **not**: an anti-detect/anti-bot or automation tool. See the
non-goals in **[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md)**.

## Architecture in one paragraph

Cargo **workspace**, one crate per subsystem, **ports & adapters**: every
third-party dependency is wrapped behind one of our traits in a dedicated adapter
crate, so any part can be swapped without touching its callers. The mandated trait
seams (`JsEngine`, `LayoutEngine`, `Rasterizer`, `TextShaper`, `ImageDecoder`,
`TlsProvider`, `Aead`, `Kdf`, `CookieStore`/`Vault`, `FarblingProvider`) are all
defined. Details in **[ADR-0001](docs/adr/0001-architecture-and-trait-boundaries.md)**.

## Build & run

The scaffold is **std-only and builds offline**. The toolchain is pinned in
`rust-toolchain.toml`.

```sh
cargo build --workspace
cargo test  --workspace

# Render the built-in page to a PPM and print a summary:
cargo run -p cerberus-app -- render --out home.ppm

# The CI memory regression gate:
cargo run -p cerberus-app --release -- mem-gate --budget-mb 64
```

Example `render` output (idle RSS for the dependency-free scaffold is ~2–4 MB):

```
rendered cerberus:home (800x600)
  http status     : 200
  active head     : work
  js engine       : null (engines live: 1, realms: 1)
  active cookies  : 1
  3rd-party access : Deny
  resident memory : 3.9 MB
```

## Documentation

- **[PLAN.md](PLAN.md)** — milestones, memory budget, crate layout, risks, open
  decisions.
- **[docs/adr/](docs/adr/)** — architecture, JS engine, dependency policy.
- **[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md)** — who we protect against + the
  non-goals.
- **[SECURITY.md](SECURITY.md)** · **[CONTRIBUTING.md](CONTRIBUTING.md)**

## License

Apache-2.0 (provisional — see PLAN §10 / ADR-0002).

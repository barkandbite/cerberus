# Cerberus

A privacy-first, memory-lean web browser, built from the ground up in Rust.

> **Status: all milestones (M0–M9) delivered.** Real networking (HTTP/1.1 +
> rustls + DoH), our own HTML/CSS/layout/paint pipeline, QuickJS page scripts,
> sealed cookies on the live fetch path, the encrypted quarantine vault,
> enforced consent, per-head farbling, switchable identities with a leak gate,
> headless PNG/PDF automation, and a reproducible build. See
> **[PLAN.md](PLAN.md)** and the **[ADRs](docs/adr/)**.

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

The toolchain is pinned in `rust-toolchain.toml`; builds use the committed
`Cargo.lock` (see [docs/REPRODUCIBLE.md](docs/REPRODUCIBLE.md) to byte-verify
a release binary).

```sh
cargo build --workspace --locked
cargo test  --workspace --locked

# Open the browser in a window (needs a display):
cargo run -p cerberus-app --features windowing -- run

# Headless: render any page to PPM / PNG / PDF (picked by extension):
cargo run -p cerberus-app --release -- render --url https://example.com --out page.png
cargo run -p cerberus-app --release -- render --url https://example.com --out page.pdf --dump-text

# Persistent profile (cookies, vault, consent rules, head seeds survive runs;
# omit --data-dir for the fully-ephemeral default):
cargo run -p cerberus-app --release -- render --url https://example.com --out p.png --data-dir ~/.cerberus

# Single egress proxy (CONNECT tunnel; target hosts are never resolved locally):
cargo run -p cerberus-app --release -- render --url https://example.com --out p.png --proxy 127.0.0.1:3128

# The CI gates: memory (idle + head-switch leak) and the pipeline benchmark:
cargo run -p cerberus-app --release -- mem-gate --budget-mb 64 --switches 25
cargo run -p cerberus-app --release -- bench --assert-total-ms 500
```

Example `render` output against a real site:

```
rendered https://github.com/ (800x600)
  http status     : 200
  active head     : work
  js engine       : quickjs (engines live: 1, realms: 1)
  page scripts    : 6 executed
  active cookies  : 3
  3rd-party access : Deny
  blocked subres  : 0
  resident memory : 29.6 MB
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

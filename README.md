# Cerberus

A privacy-first, memory-lean web browser, built from the ground up in Rust.

## Quick start — download and run

No toolchain, no build. Grab the latest binary for your OS and run it.

### Linux

```sh
# 1. Download the latest binary
curl -L -o cerberus https://github.com/barkandbite/cerberus/releases/latest/download/cerberus-linux-x86_64

# 2. Make it executable
chmod +x cerberus

# 3. Run it (opens the browser window)
./cerberus
```

You need a graphical desktop (X11 or Wayland) — run it from a terminal, not by
double-clicking in a file manager. Most desktops already have everything needed;
if the window won't open, install your distro's display libraries:

```sh
sudo apt install libxkbcommon0 libwayland-client0   # Debian/Ubuntu
```

No display (server/SSH)? Render a page to an image instead:

```sh
./cerberus render --url https://example.com --out page.png
```

### Windows

1. Download **[cerberus-windows-x86_64.exe](https://github.com/barkandbite/cerberus/releases/latest/download/cerberus-windows-x86_64.exe)**.
2. Double-click it to open the browser.

Windows SmartScreen may warn about an unrecognized app (the binary isn't
code-signed yet) — click **More info → Run anyway**.

### macOS

Not yet published — [build from source](#build-from-source) for now.

That's it. For everything below this line you can stop reading unless you want
the details.

---

## What Cerberus is

The differentiator is the **privacy model**, not the renderer:

- **Three identities ("heads")** — work / personal / throwaway — used one at a
  time, each with its own sealed cookie partition, fingerprint profile, farbling
  seed, and (optionally) its own egress proxy. Cross-identity correlation is
  impossible *by construction*.
- **Coherent per-window fingerprint** — each head presents one internally
  consistent browser persona (a real device class), with per-head bounded noise
  on canvas/audio/WebGL/fonts so trackers can't build a stable cross-site id.
- **Quarantine vault** — cross-site cookies are held AEAD-encrypted and never
  attached to a request until you release them; the key is derived from your
  passphrase (Argon2id) and lives nowhere at rest.
- **Transparent, user-controlled cookies** — per-cookie Allow / Session / Timed /
  Block, with a visual inspector and a `cookies` CLI; third-party storage
  defaults to deny.
- **Memory is priority #1** — one process, one JS engine live at a time (the
  active identity's), instantiated lazily and torn down on switch (~8 MB
  resident).

What Cerberus is **not**: an anti-detect / anti-bot or automation tool. See the
non-goals in **[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md)**.

## Common commands

```sh
cerberus                       # open the browser window (default)
cerberus run --mirror          # drive N sealed identity windows from one master
cerberus render --url URL --out page.png    # headless render to PNG/PDF/PPM
cerberus identities            # list / manage the heads (and per-head proxies)
cerberus cookies               # inspect / retune per-cookie rules, headlessly
cerberus --help                # full usage
```

Add `--data-dir ~/.cerberus` to any command to keep cookies, vault, consent
rules, and head seeds across runs (the default is fully ephemeral).

## Build from source

Requires the Rust toolchain pinned in `rust-toolchain.toml`; builds use the
committed `Cargo.lock` (see [docs/REPRODUCIBLE.md](docs/REPRODUCIBLE.md) to
byte-verify a release binary).

```sh
cargo build --release --locked -p cerberus-app   # produces target/release/cerberus-app
cargo test  --workspace --locked                 # run the test suite
cargo run   -p cerberus-app --release -- run      # build and open the browser
```

## Documentation

- **[PLAN.md](PLAN.md)** — milestones, memory budget, crate layout, open decisions.
- **[docs/adr/](docs/adr/)** — architecture, trait boundaries, JS engine, dependency policy.
- **[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md)** — who we protect against + the non-goals.
- **[SECURITY.md](SECURITY.md)** · **[CONTRIBUTING.md](CONTRIBUTING.md)**

## License

Apache-2.0 (provisional — see PLAN §10 / ADR-0002).

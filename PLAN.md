# Cerberus — Plan

> **Status: all milestones (M0–M9) complete.**
> The plan below is retained as the architectural reference; every milestone
> in [§6](#6-milestones) has shipped, the decision log lives in
> [§10](#10-decisions), and the delivery details are in the ADRs
> ([index](docs/adr/README.md)). The standing decision directive resolved the
> remaining sign-offs with the ADRs' recommended defaults (noted per item).

Cerberus is a privacy-first, memory-lean web browser written from the ground up
in Rust. The differentiator is the **privacy model** — sealed per-instance
cookies, the quarantine vault, cookie groups, the consent gate, farbling, and
the three identities. Rendering is treated as undifferentiated heavy lifting: a
means, modular enough to recode, not the point.

---

## 1. Prime directive: memory first

Memory footprint is priority **#1**, ahead of features and speed. The goal is
many tabs / sessions / identities at minimal RAM. The architecture enforces
this in two ways:

- **One process. One JS engine instance live at a time** — the active identity's
  engine, instantiated lazily and torn down on identity switch. This is how we
  run three identities without paying for three engines. Enforced today by
  `cerberus-identity` (`HeadManager::engines_live()` is always 0 or 1; see its
  tests) and by the `JsEngineFactory` seam (dropping the `Box<dyn JsEngine>` is
  the teardown).
- **Arena/bump allocation** for parse trees and short-lived render state, to
  avoid per-node heap churn. **Done for the DOM (M2):** `cerberus-dom` stores the
  parse tree in one flat `Vec<NodeData>` with `NodeId` children, read through a
  `NodeRef` cursor (the css + app consumers were migrated to it — the original
  "no caller changes" hope didn't survive the public-field API, but the cutover
  was output-identical). Short-lived render state (layout) is a later candidate.

The memory budget in [§5](#5-memory-budget-proposed-for-sign-off) is gated in CI
as a regression test.

---

## 2. Architecture overview

**Ports & adapters (hexagonal).** One Cargo crate per subsystem. Every
third-party dependency is wrapped behind one of *our* traits in a dedicated
adapter crate, so no foreign/vendor type ever crosses a module boundary —
callers depend only on our traits. "Modular" has a concrete test: *delete an
adapter crate, write a new one implementing the same trait, and everything else
compiles unchanged.* Full rationale in
[ADR-0001](docs/adr/0001-architecture-and-trait-boundaries.md).

The single binary, `cerberus-app`, is the **composition root** — the only place
that names concrete adapters and wires them together via dependency injection.

### Crate map

| Crate | Role | Key trait(s) it owns | Third-party today |
| --- | --- | --- | --- |
| `cerberus-types` | Shared value types (ids, geometry, color, origin) | — | none |
| `cerberus-url` | URL parsing (ours) | — | none |
| `cerberus-net` | Networking: http1 codec, engine, router | `TlsProvider`, `DnsResolver`, `HttpClient` | none (bootstrapped) |
| `cerberus-tls-rustls` | TLS adapter | impls `TlsProvider` | rustls + ring + webpki-roots (ADR-0006) |
| `cerberus-dns-doh` | DNS-over-HTTPS adapter (Quad9) | impls `DnsResolver` | none (bootstrapped) |
| `cerberus-dom` | DOM + HTML parser (ours) | — | none |
| `cerberus-style` | Computed-style types + `StyleEngine` seam | `StyleEngine` | none |
| `cerberus-css` | Our CSS engine (parser, cascade, UA sheet) | impls `StyleEngine` | none |
| `cerberus-layout` | Block/inline flow over the styled tree | `LayoutEngine` | none |
| `cerberus-paint` | Display list, framebuffer, paint | `Rasterizer`, `TextShaper`, `ImageDecoder` | none |
| `cerberus-text` | Software shaper + rasterizer (bundled Roboto) | impls `TextShaper`, `Rasterizer` | ab_glyph (ADR-0005) |
| `cerberus-image` | Image-decoder adapter (web formats, 1600px cap) | impls `ImageDecoder` | image (ADR-0005) |
| `cerberus-js` | JS engine seam | `JsEngine`, `JsEngineFactory` | none (V8 at M3) |
| `cerberus-crypto` | Crypto seam + key material | `Aead`, `Kdf` | none (RustCrypto at M4) |
| `cerberus-storage` | One storage env, sealed cookies, vault | `CookieStore`* , `Vault` | none |
| `cerberus-consent` | Third-party detection, default-deny, rules | `ConsentPolicy` | none |
| `cerberus-farbling` | Per-head seeded noise + JS prologue | `FarblingProvider` | none |
| `cerberus-identity` | The three heads; engine swap on switch | — | none |
| `cerberus-ui` | Minimal toolbar (back/fwd/reload/stop/URL/head/settings) | — | none |
| `cerberus-shell` | Platform surface + `FrameApp` seam | `PlatformSurface`, `FrameApp` | none |
| `cerberus-shell-winit` | Windowing adapter (windowed/fullscreen) | drives `FrameApp` | winit, softbuffer (ADR-0004) |
| `cerberus-headless` | Render-to-PPM/PNG, automation | — | none (PNG at M2) |
| `cerberus-app` | Composition root + CLI + memory gate | — | none |

\* The required `CookieStore` trait is realized as the `StorageEnvironment` /
`InstanceStore` API; the sealing guarantee is structural (see ADR-0001 §"Storage
sealing"). It can be lifted into a named trait if you prefer a uniform trait per
subsystem — flagged in [§10](#10-open-decisions-needs-your-sign-off).

### Dependency graph (acyclic)

```
types ──┬──────────────────────────────────────────────────────────┐
        ├─ url ─ net                                                 │
        ├─ dom ─ layout ─┐                                           │
        ├─ paint ────────┴─ headless                                 │
        ├─ paint ─ shell                                             │
        ├─ js ─ identity ─┐                                          │
        ├─ farbling ──────┘                                          │
        ├─ crypto ─ storage                                          │
        └─ consent                                                   │
                                                                     │
        app  ─────────── depends on all of the above ───────────────┘
```

---

## 3. The required trait set

The mandated minimum trait set, and where each lives / what it will wrap:

| Trait | Crate | Adapter plan |
| --- | --- | --- |
| `JsEngine` (+`JsEngineFactory`) | `cerberus-js` | `cerberus-js-v8` (rusty_v8), M3; QuickJS later |
| `LayoutEngine` | `cerberus-layout` | ours (`BlockLayout` now → real layout M2) |
| `Rasterizer` | `cerberus-paint` | font rasterizer adapter, M2 |
| `TextShaper` | `cerberus-paint` | shaping adapter, M2 |
| `ImageDecoder` | `cerberus-paint` | image-decoder adapter, M2 |
| `TlsProvider` | `cerberus-net` | `cerberus-tls-rustls`, M1 |
| `Aead` | `cerberus-crypto` | RustCrypto AEAD adapter, M4 |
| `Kdf` | `cerberus-crypto` | `argon2` adapter, M4 |
| `CookieStore` | `cerberus-storage` | ours (sealed by construction) |
| `Vault` | `cerberus-storage` | ours over `Aead`+`Kdf`, M4 |
| `FarblingProvider` | `cerberus-farbling` | ours (`SeededFarbling`) |

Every adapter is a *separate crate* depending on the trait crate, so removing or
replacing it never touches callers.

---

## 4. Dependency policy (summary)

Default to **bootstrapping**. A crate enters the tree only where rolling our own
is the *bigger* security risk — historically crypto, TLS, the JS engine, and
font/image decoders. No crate is added without an ADR and your approval. Full
policy, the initial approved list, and the specific crates proposed for the
vault are in [ADR-0003](docs/adr/0003-dependency-policy.md). Enforcement:
`deny.toml` (license allow-list + advisories), a committed `Cargo.lock`, and
`--locked` builds in CI.

---

## 5. Memory budget (proposed, for sign-off)

**Ratified as proposed (M9; standing decision directive).** Real-world
measurements sit far inside every row: idle ~7 MB; a scripted Wikipedia
article ~38 MB; the worst image-heavy page measured (apple.com) ~61 MB —
all under the 64 MB CI gate, with QuickJS (not V8) as the engine.

| Scenario | Proposed budget (RSS) | Notes |
| --- | --- | --- |
| Idle — no JS engine instantiated (shell + render core + 1 blank tab) | **≤ 50 MB** | engine is lazy; this is the "cold" baseline |
| Idle — active head, JS isolate live, 1 blank tab | **≤ 150 MB** | dominated by the V8 isolate; calibrate when ADR-0002 lands |
| Marginal per additional blank tab (same head) | **≤ 8 MB** | arena-backed DOM/layout, no per-node churn |
| Marginal per additional tab, typical static page | **≤ 40 MB** | excludes large media |
| After identity switch (engine torn down + re-instantiated) | **within +10%** of pre-switch idle | proves no engine/realm leak |

**How CI gates it.** `cerberus-app mem-gate --budget-mb N` renders the built-in
page (exercising the whole pipeline, live QuickJS included) and reads `VmRSS`
from `/proc/self/status`, failing if over budget (`N = 64`). The
identity-switch row is enforced by `mem-gate --switches K`: RSS after K live
head switches must stay within +10% of the pre-switch idle (2 MB absolute
floor for allocator noise). Measured: 25 switches grow RSS 7.1 → 7.2 MB.

**Methodology to formalize at M3/M7:** measure peak RSS in a fixed headless
scenario on the CI runner; record a baseline; fail on regression beyond the
budget. Cross-platform RSS reading is abstracted (Linux procfs now; macOS/Windows
when those targets are added).

---

## 6. Milestones

All complete:

| # | Name | Exit criteria | Delivered |
| --- | --- | --- | --- |
| **M0** | Scaffold | Workspace, crate-per-subsystem, trait boundaries stubbed, a trivial render, PLAN+ADRs. | ✅ 15 crates, every seam, structural-guarantee tests |
| **M1** | Network | HTTP/1.1 + rustls + DoH behind `TlsProvider`/`DnsResolver`; fetch + cache. | ✅ + https-first policy, per-instance cache (ADR-0006) |
| **M2** | Render core | Real HTML parser → DOM (arena) → layout → paint; font/shaping/image crates wired. | ✅ + CSS engine, tables, forms, SVG (ADR-0005/7/9) |
| **M3** | JS | Engine behind `JsEngine`; lazy per-head instantiation; budget recalibrated. | ✅ QuickJS (owner choice over V8), snapshot/replay DOM bridge (ADR-0002/8) |
| **M4** | Storage | One data-store env; per-instance sealed cookies; vault (Argon2id + AEAD); groups persisted. | ✅ real cookie flow per redirect hop; XChaCha20-Poly1305 vault; `--data-dir` profiles (ADR-0010) |
| **M5** | Consent | Real eTLD+1 + default-deny + prompt UX + rule store. | ✅ vendored-PSL matcher; enforcement at every subresource/cookie; banner UX; persisted instance-scoped rules |
| **M6** | Farbling | Per-head seeded noise on canvas/audio/WebGL/font surfaces + tests. | ✅ seeded shims in every realm; deterministic per head, uncorrelated across heads |
| **M7** | Heads | Three switchable identities; switch = swap head + engine; leak tests. | ✅ per-profile random seeds/instances; `mem-gate --switches` (+10% gate) |
| **M8** | Headless | Scoped rendering (PNG/PDF) + automation; third-party deny; single proxy. | ✅ bootstrapped PNG/PDF encoders; `--dump-text`; CONNECT-tunnel egress, no DNS leak |
| **M9** | Harden | Reproducible build, full test + benchmark suite green, docs complete. | ✅ byte-identical double build verified; bench gate in CI; docs trued |

---

## 7. Risks & mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| V8 dominates RSS (conflicts with #1 priority) | High | One isolate at a time; lazy/teardown; `JsEngine` seam to swap QuickJS later with no caller changes (ADR-0002). |
| FFI `unsafe` in engine/crypto adapters | High | `unsafe_code = "deny"` workspace-wide; adapters opt in explicitly and visibly; unsafe confined to adapter crates and reviewed. |
| Rolling our own HTML parser / layout introduces bugs | Med | Keep them behind traits; fuzz the tokenizer; arena allocation; bounded scope for v1. |
| Hand-rolling crypto | Critical | We do not. AEAD/KDF/zeroize are approved, audited crates behind `Aead`/`Kdf` (ADR-0003). |
| Vault passphrase loss = data loss | Med (by design) | Documented trade-off; key lives nowhere at rest, not in OS keystore. |
| Reproducible builds drift | Med | Pinned toolchain, committed lockfile, `--locked`, planned `--remap-path-prefix` + vendoring (§8). |
| Memory budget unachievable once V8 lands | Med | Budgets are proposals; recalibrate at M3; QuickJS escape hatch. |
| Windowing dependency not yet chosen | Low | Headless surface today; windowing behind `PlatformSurface`; pick via ADR before M-window. |

---

## 8. Reproducible builds

Essential for a privacy browser people are asked to trust. Plan:

- Toolchain pinned in `rust-toolchain.toml` (channel + components).
- `Cargo.lock` committed; all CI builds use `--locked`.
- `--remap-path-prefix` to strip absolute paths from artifacts (M9).
- `panic = "abort"`, `codegen-units = 1`, `lto` in the release profile (set now).
- Vendored dependencies (`cargo vendor`) for audited, offline builds once we have
  dependencies (M9).
- Document a from-source build that a third party can byte-reproduce (M9).

---

## 9. Testing strategy

The differentiator tests are first-class and already encoded against the
scaffold (they assert structural guarantees, not feature behavior):

| Requirement | Where (today) |
| --- | --- |
| Cross-instance leak: cookie in A unreadable in B (by construction) | `cerberus-storage` `cross_instance_cookie_is_unreadable_by_construction` |
| Quarantine: never sent until released | `cerberus-storage` `quarantined_cookie_is_never_sent_until_released` |
| Third-party block: default deny + consent event headed | `cerberus-consent` tests |
| Farbling: deterministic, bounded, two heads don't correlate | `cerberus-farbling` tests |
| One engine at a time | `cerberus-identity` `engine_is_lazy_and_at_most_one_lives` |
| End-to-end render | `cerberus-app` / `cerberus-headless` tests |
| Memory budget | `cerberus-app mem-gate` (CI) |
| Network hygiene (no telemetry; DoH active) | added with M1 |

258 tests pass; `fmt`, `clippy -D warnings`, `cargo deny check`, the memory
gate (idle + head-switch), and the benchmark gate are green. Network hygiene:
DoH-only resolution (Quad9), no telemetry, and with `--proxy` a single
CONNECT-tunneled egress with no local target resolution at all.

---

## 10. Decisions

### Resolved (owner sign-off, 2026-06-09)

- **Windowing** → `winit` + `softbuffer` behind `PlatformSurface`, CPU-only
  (ADR-0004). Windowed, fullscreen, and headless share one render→present path.
- **Rendering stack** → full text shaping + font rasterization + image decoding,
  with fonts **bundled** (system fonts never enumerated — anti-fingerprinting)
  (ADR-0005).
- **UI** → one minimal toolbar: Back, Forward, Refresh, Stop, a URL box,
  a tiny head switcher, and a Settings button. **No bookmarks. No tabs**
  (single-page; Back/Forward walk history). Identity switching and vault unlock
  live behind the head switcher / Settings. Implemented in `cerberus-ui`.
- **Networking (M1, complete)** → rustls + `ring` + bundled `webpki-roots`
  (`TlsProvider`); **Quad9** DoH, DoH-only (`DnsResolver`); https-first →
  user-risk-prompt → block for plaintext `http`; background-thread windowed
  loads (worker + event-loop `Waker`); per-instance HTTP cache (ADR-0006). Fetch
  path **live-verified**; the load state machine (upgrade/prompt/cache/Stop) is
  covered by hermetic tests.
- **CSS / rendering (M2)** → our own `cerberus-style` (types + `StyleEngine`) +
  `cerberus-css` (parser, selectors, specificity cascade, UA stylesheet — no
  deps) drive a styled block/inline layout: color, backgrounds, font
  size/weight, text-align, margins, lists, links (ADR-0007).
- **Images (M2)** → `cerberus-image` wraps the `image` crate behind
  `ImageDecoder` (PNG/JPEG/GIF/WebP/BMP), decoding capped at 1600px on the long
  edge so one image can't blow the RSS budget. `<img>` sub-resources are fetched
  on the network worker (interactive) or up front (one-shot `render`), keyed by
  absolute URL in a **per-page** store that is cleared on every navigation;
  layout sizes them from intrinsic or `width`/`height` and clamps to the content
  box, with a gray placeholder while a sized image is in flight and `[alt]` text
  otherwise. **SVG** is rasterized via `resvg`/`usvg`/`tiny-skia` (text feature
  off) behind the same `ImageDecoder`, under the same 1600px cap (ADR-0009).
  **Live-verified** end-to-end: rust-lang 9/9, Wikipedia 9/12, HN 2/2 decoded,
  all within the 64 MB gate.
- **Form controls (M2)** → layout renders `<input>` (text/search/password/…,
  checkbox, radio, submit/reset/button), `<button>`, `<textarea>`, and
  `<select>` as bordered inline-block boxes from their DOM state (value,
  placeholder, `checked`, selected `<option>`, `size`/`rows`/`cols`), clamped to
  the content box; `type=hidden` paints nothing. **Live-verified** (Wikipedia's
  search field + button + checkboxes).
- **Form interactivity (M2)** → the controls are now usable without JS: layout
  emits a `FormFieldBox` hit region + stable field id for each control (the id is
  the pre-order index of `<input>`/`<textarea>`/`<select>`/`<button>`, shared
  verbatim by layout and the app so a clicked box maps to the right DOM control,
  even inside table cells). Click a text field/textarea to focus it (blinking-
  less caret) and type/backspace; click toggles a checkbox, keeps a radio group
  mutually exclusive within its form, and cycles a `<select>`; a submit button or
  Enter serializes that form's successful controls into a urlencoded `action?query`
  (GET) and navigates. Field state is per-page (cleared on navigation). Covered by
  hermetic app tests (typing→store, encoded submission URL, checkbox/radio/select).
  Richer events + POST arrive with JS at M3.
- **Tables (M2)** → `<table>` (with `<thead>`/`<tbody>`/`<tfoot>`, `<tr>`,
  `<td>`/`<th>`, `<caption>`) lays out as a bordered grid: equal-width columns,
  each cell's content flowed into its own box (so nested links/images/tables
  work), `<th>` bold + centred with a grey fill, and a `<caption>` line above.
  **Live-verified** (kernel.org's release tables render as real grids with the
  in-cell links preserved). Content-based column sizing and colspan/rowspan are
  noted follow-ups.
- **HTML parser (M2)** → `parse_html` replaces the M0 placeholder: a quote-aware
  tokenizer (so `>` inside an attribute value can't end a tag early),
  rawtext/RCDATA for `<script>`/`<style>`/`<title>`/`<textarea>`, comment +
  doctype skipping, entity decoding (text **and** attribute values), and a tree
  builder with the common optional-end-tag rules (a new `<li>`/`<tr>`/`<td>`/
  `<option>` closes the previous one; block elements close an open `<p>`). Not
  the full HTML5 tree-construction algorithm, but it parses real pages without
  mis-nesting. Head-only elements (`<title>`, `<meta>`, …) are UA `display:none`,
  so they no longer leak into the page.
- **Arena DOM (M2)** → the parse tree is now an **arena** (PLAN §1): a `Document`
  owns one flat `Vec<NodeData>` and children are `NodeId` indices, replacing the
  recursive `Box`/`Vec<Node>` tree (fewer scattered allocations, better
  locality — memory is priority #1). Reads go through a `Copy` `NodeRef` cursor
  (`tag`/`text`/`attr`/`attrs`/`children`/`text_content`/`id`); app-generated
  pages build via `DocumentBuilder`. The parser's tokenizer/tree-construction
  behavior is unchanged; the css + app consumers were migrated to the cursor.
  Verified output-identical (a deterministic page renders byte-for-byte the same
  as before the swap) with the full suite green.
- **JS engine (M3, started)** → `cerberus-js-quickjs` wraps **QuickJS** (via
  `rquickjs` 0.9, bundled) behind the `JsEngine` seam — the owner chose QuickJS
  over V8 for memory (ADR-0002). One `Runtime` (one GC heap) per active head,
  one `Context` per realm; the engine is instantiated lazily and torn down on
  head switch (still ≤1 live). `JsEngine` is no longer `Send` (single-threaded
  VM, lives on the UI thread). Wired into the composition root: home renders with
  a live `quickjs` engine + the per-head farbling prologue, at **~10 MB RSS**
  (QuickJS adds ≈1.4 MB — well within the 64 MB gate).
- **Page scripts (M3)** → a page's own inline `<script>`s now run, via an
  engine-agnostic **snapshot/replay DOM bridge** (`cerberus-js-dom`, ADR-0008):
  the parser retains inline scripts (`Document::scripts()`), the immutable DOM is
  serialized to JSON, scripts run against a JS `document`/`window` model (built
  in JS, so the `JsEngine` seam stays eval-only — no `unsafe`, no live bindings),
  and the mutated tree is serialized back and rebuilt via `DocumentBuilder`
  before styling/layout. Runs between `parse_html` and layout in both app paths;
  script-built and `DOMContentLoaded`-built content appears in the render. The
  document model covers `getElementById`/compound + combinator `querySelector`,
  `createElement`/`textContent`/`classList`/attributes/`append`·`insert`·`remove`,
  **`innerHTML`/`outerHTML` (reusing our Rust `parse_html` via deferred reparse)**,
  and a `window` environment — `location` (parsed from the page URL),
  `localStorage`/`sessionStorage`, `getComputedStyle`, `matchMedia` (always
  `matches:false`, speed-first), `history`, and a deliberately **low-entropy
  `navigator`** (per-head fingerprint farbling stays M6, not here). Cost: a
  transient ~2× DOM serialization, only on script-laden pages. The JS document
  model puts all node behavior on **shared prototypes** (nodes carry data only;
  `style`/listeners are lazy) — without this, ~40 per-node closures blew RSS to
  **85 MB on a Wikipedia article**; with it the same page renders at **~38 MB**
  (validated on live sites: HN/cnn/rust-lang/Wikipedia all well under the 64 MB
  gate, zero crashes across redirects/404/forms). Next: external `<script src>`,
  a live-binding swap if profiling demands, broader event/DOM coverage.
- **Speed-first / raw render** → Cerberus **ignores programmed delays**: CSS
  `opacity`/`animation`/`transition`/`transform`/`visibility` are not honored;
  lazy-loading is ignored — `data-src` is preferred over a placeholder `src` and
  every image is fetched immediately, never on scroll. On the JS side (M3) a
  prelude installed into every realm makes `setTimeout`/`setInterval`/
  `requestAnimationFrame`/`requestIdleCallback`/`queueMicrotask` fire
  **immediately** and `IntersectionObserver.observe` report the target visible
  **at once** (so scroll-/timer-gated content appears without waiting);
  `setInterval` fires once, not forever. Content renders immediately (ADR-0007).

### Resolved at M9 (standing decision directive, 2026-06-11)

The remaining sign-offs were closed with the ADRs' own recommended defaults:

1. **Memory budget numbers** (§5) — ratified as proposed; the CI gate stays
   64 MB and the switch-leak gate enforces the +10% row.
2. **JS engine** — QuickJS (already owner-resolved 2026-06-10, ADR-0002);
   V8 remains the documented swap-in if compat ever demands it.
3. **Vault crates** — `chacha20poly1305` + `argon2` + `zeroize` approved and
   wired (ADR-0003/0010); `mlock` rejected for v1 (would require `unsafe` for
   a partial mitigation — documented in the threat model).
4. **License** — Apache-2.0 confirmed.
5. **`CookieStore`** — the structural `StorageEnvironment` API stays; the
   sealing guarantee is the construction, not a trait name.
6. **Edition** — Rust 2021 stays pinned.

---

## 11. Delivery status (M9)

- **Done:** everything in §6. The differentiator spine is real end-to-end:
  sealed per-instance cookies ride the actual fetch path (captured/attached
  per redirect hop), quarantine flows into an XChaCha20-Poly1305 vault keyed
  by an Argon2id passphrase that never exists at rest, consent default-deny
  is enforced on every third-party subresource and cookie with a banner +
  persisted instance-scoped rules over a vendored-PSL eTLD+1, farbling shims
  cover canvas/audio/WebGL/font per head, three identities switch with an
  enforced no-leak gate, and headless automation emits PNG/PDF through a
  single optional CONNECT-proxy egress.
- **Profiles:** `--data-dir` opts into persistence (ephemeral remains the
  default posture); profile layout + formats in ADR-0010.
- **Hardening:** byte-reproducible release build (verified by double-build
  hash equality; see docs/REPRODUCIBLE.md), `cargo deny` green, benchmark
  gate in CI, 258 tests.
- **Deliberate v1 bounds (documented):** `Expires` cookie dates (Max-Age
  only), no `mlock`, default-deny means cross-site CDNs (e.g. wikimedia.org
  under wikipedia.org) wait for one banner Allow per site, and the JS bridge
  remains snapshot/replay (ADR-0008) — richer event-driven re-render is the
  natural next phase.

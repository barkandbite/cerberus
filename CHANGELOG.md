# Changelog

All notable changes to Cerberus are recorded here. Versions are small while the
browser is pre-1.0; this is the first tagged preview.

## [Unreleased]

## [0.0.15] - 2026-07-21

Speed & stability release, from real-world Windows testing that surfaced a hard
freeze and general sluggishness — the two things the project is supposed to get
right.

### Fixed
- **A page can no longer freeze the window ("Not Responding").** All page
  JavaScript runs synchronously on the UI thread, and the only limit was a
  per-eval 5s watchdog re-armed for every script — so approving a site's
  third-party access (e.g. cnn.com) let its many external scripts, each with a
  full event-loop drain, run back-to-back and starve the OS message pump. The JS
  event loop now takes a wall-clock budget (`EventLoopBudget::interactive()`,
  the batch default plus a 50 ms cap) so a single drain can't hog the thread, and
  `poll()` processes completed jobs under a ~48 ms slice and re-wakes to continue
  — the work drains across serviced frames instead of one frozen burst. A page
  that drains quickly is unaffected; the one-time first-paint drain keeps the
  full budget.
- **Scrolling and interaction are much faster.** Every redraw used to re-run a
  full styled-tree layout — re-shaping every text run through rustybuzz — and
  scroll, click, and keypress each trigger a redraw, so one scroll notch paid for
  a complete relayout of the page. Layout is now cached and reused for any redraw
  that doesn't change it (the big one: scrolling only shifts the cached display
  list); it's recomputed only when the page, a form, an image/stylesheet, the
  viewport size, or an edit actually changes it.
- **Rendering.** A childless `<button>` (an icon button whose glyph comes from
  CSS/JS we don't run) no longer stamps the literal word "Button" on the page
  (seen top-left on React sites); it renders empty, as a real browser does. Form
  controls now honor `border-radius`, so a pill-shaped input (eBay's search
  field) draws rounded instead of a square rect with corners protruding past the
  outline.

## [0.0.14] - 2026-07-21

### Fixed
- **Flex columns no longer collapse to one word per line.** `flex-basis: auto`
  now resolves to a flex item's explicit `width` (per spec) instead of ignoring
  it and measuring content — so a `width:70%` flex column (e.g. Tachyons
  `w-70-l`) takes its share of the row rather than shrinking toward min-content.
  This fixed narrow, one-word-per-line text seen across real sites (rust-lang.org's
  hero tagline, and other flex layouts). No parity regressions.

### Added
- **`clip-path: polygon(...)` decorative dividers.** A solid (or colored) box
  background with a `polygon()` clip now paints as that shape — angled and
  stepped "hero" dividers common on marketing pages (e.g. mozilla.org) — instead
  of a full rectangle. Vertices accept `%` (of the box's width/height) and `px`
  (origin-relative) coordinates; the fill is a real even-odd scanline rasterizer
  (`DisplayItem::Polygon`). Works in both the flow walker and the taffy engine,
  and for absolutely-positioned `::before`/`::after` overlays.
- **Web fonts: substitute, don't download.** A page's own `@font-face` families
  are now reported as loaded by `document.fonts.check()` — matching a real
  browser that fetched them — while Cerberus never downloads the bytes and keeps
  rendering with a metric-compatible bundled face (ADR-0005). This closes a
  fingerprinting tell (a site could otherwise flag "this browser didn't load my
  own web font") without adding a font-fetch/cache/timing signal. The page's
  `@font-face` names are parsed from inline and external CSS and injected as
  `__CERBERUS_PAGE_FONTS__` ahead of page scripts (and refreshed when external
  sheets arrive). Local system-font enumeration stays defended by the existing
  per-head farbling.

## [0.0.13] - 2026-07-19

### Added
- **Whole UI on the design system.** Every remaining chrome surface now draws
  from `cerberus_ui::theme` and its rounded widgets, so the browser reads as one
  product instead of a set of hand-placed rectangles: the toolbar (rounded
  buttons, an accent URL field with an accent caret, an accent-tint head chip),
  the consent banner (an attention-amber strip with green "Allow" / red "Deny"
  pills), the cookie inspector and the MIRC roster (scrim + rounded card + soft
  shadow, uppercase section headers, zebra rows, and semantic disposition/state
  **pills** — green allow/live, amber session/diverged, red block), the
  performance HUD and the mirror "driven" badge (the rounded dark developer-
  tooling `INK` surface with an accent header and green figures). New shared
  tokens (semantic chip tints + inks, an attention/warning colour, a positive
  on-dark value) and helpers (`round_button`, `round_icon_button`, `pill`) keep
  every button and chip one shape. A `chrome_preview` example renders the
  toolbar, banner, HUD, and cookie inspector to PNGs for review.
- **UI design system + redesigned settings panel.** A small shared visual
  language (`cerberus_ui::theme`) — one surface palette, the toolbar's blue as
  the single accent, one spacing rhythm, type scale, and corner radius — plus
  reusable widgets (rounded cards, iOS-style toggles, section headers, chevron
  rows, a masked field) built on it. The settings menu is rebuilt on top: a
  centred modal card that dims the page, with grouped sections (Privacy & data,
  Performance, Identity vault) of real toggle/nav rows and a proper passphrase
  field — replacing the old flat "clickable text" rows. `SettingsPanel`
  (`layout`/`paint`/`hit_test`) is a pure view like the rest of the crate. The
  developer console is rebuilt on the same tokens (see below).
- **Web Workers + more Web platform APIs.** Page script can now spawn `Worker`s
  (from `Blob` object URLs, with `postMessage`/`importScripts`), validated
  against Google's Comlink SDK and the real Web Platform Tests `testharness.js`
  running end-to-end in a worker. Also new/upgraded: `Blob`, `URL` +
  `URLSearchParams` (relative resolution, dot-segment normalization,
  special-scheme origins), `Image`/`WebSocket`, `DOMException` +
  `QuotaExceededError`, `Event`/`CustomEvent`/`EventTarget` (with `performance`
  as an EventTarget), `TextDecoder` utf-16, spec-correct `crypto.getRandomValues`
  validation, and `atob`/`btoa` `InvalidCharacterError`. Real `<img>`/beacon and
  `navigator.sendBeacon` requests now go out through the sealed network path.
- **Developer console (F12), rebuilt on the design system.** A dark bottom
  drawer that reads as a developer tool while sharing the design system's
  accent, spacing, radii, and type scale: a titled tab strip (Console active;
  Elements/Network/Storage signposted for later), a row of live stat chips
  (DOM nodes · links · fields · cookies), the page URL, and the page's captured
  `console.*` output (most recent last, tail-clipped) **colour-coded by level**
  (`console.error` red, `console.warn` amber). Toggle with F12; the
  drawer swallows clicks so content behind it isn't activated. `DevConsole`
  (`drawer_rect`/`paint`) is a pure view. Next: an interactive command line and
  populated Elements/Network/Storage panels.
- **Settings: working "images" toggle.** The settings panel now has a real
  images control (graphical ↔ text-only) alongside the existing cookie-manager
  and performance-HUD rows, replacing greyed-out placeholder text with a live
  button. Text-only skips image fetches entirely (privacy + speed); toggling
  reloads the current page so the new policy takes full effect.
- **Page scrolling.** The main content area now scrolls: mouse wheel and
  trackpad, `↑`/`↓` (48 px), `Page Up`/`Page Down` (90% of the viewport), and
  `Home`/`End` (jump to top/bottom). The offset is clamped to the document
  height (computed from the display list via `cerberus_paint::content_height`)
  and reset to the top on every navigation. Link, form-control, and element hit
  boxes — and scripted-page `getBoundingClientRect` — follow the scroll offset,
  so clicks land on what's visible. Previously the viewport was fixed to the top
  of the page with no way to reach content below the fold.

## [0.0.12] — 2026-07-13

Tooling release: trustworthy parity references. No engine behavior changes.

### Added
- **Full-fidelity mirror (`scripts/full-mirror.py`).** Headless Chrome cannot
  reach the network in the dev environment (the agent proxy resets its
  connections), so parity references are rendered from a local mirror. The
  previous mirror stripped cross-origin CSS/JS — but modern sites serve their
  stylesheets from CDNs, so Chrome was rendering a degraded, unstyled page and
  the "Chrome reference" was wrong for any CDN-CSS site. The new tool downloads
  the HTML plus **all** stylesheets (same- and cross-origin), their `@import`s,
  `url()` fonts/images, and `<img>` sources via `curl` (which does reach the
  network through the proxy), rewriting each reference to a local file; only
  `<script>` is dropped. Chrome then renders the real styled page, turning
  degraded-both comparisons into valid ones. Documented in
  `docs/RENDERING_PARITY_PLAN.md` §15.

## [0.0.11] — 2026-07-13

Rendering-parity release: the glyph pipeline is now Blink-exact end to end, real
bold/italic and inline SVG render, and a batch of CSS/layout gaps that broke
real brand pages are closed. Every change is measured against headless Chrome
(pixel-diff corpus + per-cause calibration pages).

### Added
- **Real bold/italic font faces.** Bundled the Liberation (Arial/Times/Courier
  metric) and DejaVu bold/italic/bold-italic companions; runs now select the real
  face by weight/style instead of the faux 1px smear / shear. Measured advances
  land within 0.5px of Chrome across 60 face × style × text cases; the synthetic
  path survives only where no real face exists (icon/CJK).
- **Inline `<svg>` rendering.** Inline SVG subtrees are serialized and rasterized
  through the existing resvg path (content-hash keyed, decoded once), keeping the
  `svg` tag so author CSS still sizes/toggles them. The cascade's computed `fill`
  (including `currentColor` and `light-dark()`-driven values) is injected into the
  payload, so CSS-painted logos and icons render in the right color.
- **Modern CSS values.** `light-dark()` (resolves to the light argument on the
  fixed light persona); the CSS **guaranteed-invalid value** for `var()` (an
  undefined / `initial` custom property with no fallback correctly invalidates,
  so a wrapping `var(--x, fallback)` takes its fallback); `min()`/`max()`/
  `clamp()`; Media Queries 4 range syntax (`(width <= 1000px)`); `calc()`
  percentages resolved against the right base; `:is()`/`:where()`/`:has()`
  (direct-child subset); sr-only `clip`/`clip-path` hiding.
- **Layout coverage.** Anonymous flex/grid items for bare text children; explicit
  numeric grid line placement (`grid-column: 2 / 9`, `1 / -1`); `inline-flex`/
  `inline-grid` as atomic inline boxes; `display: table-cell` rows.

### Fixed
- **Blink-exact glyph placement.** Integer baseline (rounded ascent), sub-pixel
  pen advances within a run, **sub-pixel word-run origins** across the line, and
  **skrifa auto-hinted** outlines (light mode — the mode Chrome-on-Linux actually
  uses). Ink-pixel mismatch on aligned text fell from ~71% to ~20% (the remaining
  residual is anti-aliasing coverage). Measure scratches keep integer widths so
  table/flex sizing stays stable.
- **Dark-theme inversion.** Sites built with `light-dark()` / the standard PostCSS
  color-scheme polyfill (e.g. MDN) rendered entirely in their dark palette on a
  light persona; the header now matches Chrome. Fixed by the `light-dark()` and
  guaranteed-invalid-value work above.
- **Text metrics.** `line-height: normal` is Blink's per-component-rounded integer;
  explicit fractional line-heights accumulate with per-line rounding; inter-word
  gaps are fractional (wrap points now flip where Chrome's flip); baseline-aligned
  inline images reserve the strut descent.
- **Inline whitespace (#137).** Whitespace fidelity across inline element
  boundaries — no phantom space before punctuation, the space before a `nowrap`
  span is kept, multi-word links underline continuously, and whitespace-only nodes
  no longer break float bands.
- **Legacy/table layout.** `<center>` block-centering stops at table-cell
  boundaries; table row heights follow their cells (cell-less spacer rows count,
  no table-font floor, trailing cell margins contained); list markers hang outside
  the content edge.

## [0.0.10] — 2026-07-04

Coherent per-window fingerprint personas: each identity now presents one
internally-consistent browser, and the fingerprint surface a real page (or an
anti-bot sensor) reads is complete and self-consistent rather than a mix of
honest and hardcoded values.

### Added
- **Coherent per-window profiles (`cerberus-profile`).** Every head derives a
  full, internally-consistent persona from its seed — OS, `navigator.userAgent`
  and platform, UA client hints, hardware concurrency / device memory, screen
  resolution and DPR, WebGL vendor/renderer, timezone, languages, and fonts — all
  picked from a market-share-weighted table of real device classes so the axes
  are coherent by construction (never, say, a Windows UA over an Apple/Metal GPU).
  The persona is injected ahead of the per-head farbling prologue in both the
  single-window and mirror paths, so the DOM model and WebGL shims read one
  identity per window.
- **Complete JS fingerprint surface.** The DOM model now exposes the full set a
  real Chrome does: `screen.orientation`, seeded `crypto` (`getRandomValues`/
  `subtle`/`randomUUID`), `Intl` (`DateTimeFormat`/`NumberFormat`/`Collator`),
  `navigator.plugins`/`mimeTypes`/`userAgentData`/`connection`/`mediaDevices`/
  `permissions`/`getBattery`/`storage`, `window.chrome`/`visualViewport`/
  frame-identity/`CSS`/`TextEncoder`/`TextDecoder`, and document metadata
  (`characterSet`/`compatMode`/`visibilityState`/`fonts`/`implementation`). A
  missing or impossible read is itself the tell these fill in.

### Fixed
- **Split-brain identity (critical).** `navigator.userAgent`/`platform` used to
  track the honest env UA while `userAgentData`, high-entropy hints, and the
  WebGL renderer were hardcoded Chrome-on-Windows, so the OS axes disagreed. All
  axes now read the one injected persona; a non-Chromium persona exposes no UA-CH,
  matching a real Firefox.
- **Impossible window geometry.** `outerHeight` could exceed `screen.height` (a
  window taller than its monitor). The screen is now a real monitor larger than
  the viewport with the work area reserving OS chrome, so
  `screen ≥ avail ≥ outer ≥ inner` holds with real browser chrome.
- **Constant `crypto` stream across heads.** The per-head seed for
  `crypto.getRandomValues`/`randomUUID` was never reachable, so every head (and
  every install) emitted an identical random stream — a perfect cross-head
  correlation key. Each head now seeds a distinct stream.
- **reese84 solver crash.** DOM nodes now expose `__proto__`, so the obfuscated
  Imperva sensor's `node.__proto__.method` access no longer throws; the process
  time zone is pinned to the persona's zone so `Date`/`Intl` agree.
- **Conformance polish.** `PermissionStatus`/`mediaDevices`/`getBattery` are now
  `EventTarget`s (`addEventListener` no longer throws); `performance` reports an
  epoch-anchored `timeOrigin` with a coherent monotonic timing sequence instead
  of an all-zero clock; `TextEncoder` substitutes U+FFFD for unpaired surrogates
  (matching Chrome byte-for-byte); and a WebGL2 context advertises the WebGL2
  extension set, core WebGL2 limits, and WebGL2 methods rather than the WebGL1
  surface.

### Notes
- The JS fingerprint surface a page reads is now coherent and complete, and the
  Imperva sensor executes without error. The browser still does **not** pass a
  live anti-bot challenge end-to-end: the network `User-Agent` remains
  honest-first (the persona drives the script-visible surface, not yet the HTTP
  request header), and the sensor still defers its final solution to the
  `/_Incapsula_Resource` sub-document, which is not yet executed. Passing a live
  challenge is not a supported outcome — see the standing note in 0.0.9.

## [0.0.9] — 2026-07-03

reese84/Imperva bot-challenge handshake machinery, and the page-script fidelity
needed to run real-world sites.

### Added
- **`XMLHttpRequest`** over the existing `fetch` transport, so a page (or a bot
  sensor) can XHR a payload and read the response (status, headers, body).
- **`document.cookie` ↔ sealed jar bridge.** Script cookie reads are seeded from
  the active instance's jar (read-only; `HttpOnly` cookies stay hidden from
  script); writes are captured verbatim and persisted through the same consent
  gate and per-cookie disposition a network `Set-Cookie` takes — first-party
  only, per sealed head.
- **Script navigation.** `location.assign`/`replace`/`reload`, `location.href =`,
  and `window.location = "…"` now navigate (http(s) only — not the internal
  `cerberus:` scheme), so a cookie-gated reload actually re-fetches. A per-gesture
  budget caps a page that reloads on every load.
- **External `<script src>` execution.** External scripts are fetched on the
  worker and run against the page realm; previously only inline `<script>` ran.
- **`probe` subcommand.** A headless interactive driver — load a URL through the
  real worker loop and print the settled result — used for live testing.

### Notes
- Against a live Imperva site (e.g. pokemoncenter.com) the browser now fetches
  the interstitial, captures the Incapsula cookies, and executes the real
  obfuscated sensor without error. It does **not** pass the full challenge: the
  sensor defers to the `/_Incapsula_Resource` sub-document, and the privacy
  farbling that randomizes fingerprints is itself what anti-bot systems flag.
  Passing a live anti-bot challenge is not a supported outcome — the machinery
  is built and functional, but the privacy posture and bot-challenge success are
  fundamentally in tension.

## [0.0.8] — 2026-07-01

Per-window egress proxy: the last axis of per-identity isolation.

### Added
- **Per-identity ("per-window") proxy.** Each head can now egress through its
  own `HTTP CONNECT` proxy, in addition to the global `--proxy`. Under
  `run --mirror` — one process, one shared engine driving N sealed windows —
  each window tunnels through its own proxy, so identities driven in lockstep
  no longer share a network vantage point. Combined with the existing sealed
  per-head cookies and farbling seed, an identity is now isolated across
  storage, fingerprint, **and** egress (ADR-0047).
  - Set it with `identities --set-proxy <idx>=<host:port>` and remove it with
    `--clear-proxy <idx>`; the `identities` listing shows `proxy=<host:port>`
    on a head that has one.
  - Persisted in `heads.txt` as an optional, backward-compatible
    `proxy <head-id> <host:port>` line.
  - Resolution per window: the head's own proxy if set, else the global
    `--proxy`, else direct. A proxied target is never resolved locally (no DNS
    leak), and a malformed proxy fails closed rather than leaking around it.

## [0.0.7] — 2026-07-01

Continuing the quality/hardening pass: four more self-contained security
issues from the review backlog, fixed.

### Fixed
- **`fetch()` header injection.** Page-controlled request headers are now
  validated (RFC 7230 token name, no CR/LF/NUL in the value) before being
  written to the wire, and rejected earlier at the `fetch()` decode step too.
  Previously a CRLF in a header value could smuggle an extra header (e.g. a
  `Cookie:` line) past the engine's name-only header allow-list.
- **JS-splice injection via U+2028/U+2029.** The JSON-for-JS escaper used to
  splice DOM/env values into `eval`'d source now escapes the two code points
  that are legal raw JSON but act as line terminators in pre-ES2019 JS string
  literals — closing a latent injection that would matter on a future
  non-QuickJS engine.
- **Decompression bomb.** gzip/deflate inflation now bounds its output
  *during* inflation instead of fully inflating an unbounded stream and
  checking the size afterward — a small bomb can no longer be materialized
  in memory before rejection.
- **Unbounded allocation from a crafted profile file.** The on-disk record
  reader now validates its field count against the remaining bytes before
  reserving a `Vec`, so a corrupted `cookies.bin`/`vault.bin` can't force a
  multi-GB allocation attempt.

## [0.0.6] — 2026-07-01

A quality/hardening pass: a codebase-wide review reconciled the stale-issue
backlog and fixed nine issues it surfaced, four of them security-relevant.

### Fixed
- **QuickJS runtime sandboxing.** The JS engine now caps its per-engine heap,
  sets an explicit interpreter stack limit, and enforces a wall-clock deadline
  on every top-level script evaluation. A page script can no longer hang or
  OOM the whole browser process with an infinite loop or an unbounded
  allocation.
- **Mirror-mode farbling.** Windows in a mirror group (multiple identities
  side-by-side) now each get their own anti-fingerprinting JS prologue
  injected, matching the single-window path. Mirrored identities no longer
  share a byte-identical canvas/audio/WebGL fingerprint.
- **Decrypted-secret hygiene.** Locking the vault now also clears any
  decrypted autofill profiles a mirror session is holding, and the vault
  passphrase (both the GUI field and the CLI path) is actually zeroized on
  every reset, not just `.clear()`ed (which doesn't wipe the backing buffer).
- **`is_public_suffix` operator-precedence bug.** A bare single-label host
  (e.g. `localhost`) was incorrectly treated as a public suffix regardless of
  the PSL data, breaking cookie storage on local/intranet hosts.
- **CSS correctness.** `object-position`/`background-position` with two
  same-axis keywords (e.g. `left right`) no longer silently misparses; the
  `background` shorthand now resets `background-position`/`background-size`
  to their initial values instead of leaking a prior longhand; `srcset`
  candidate parsing no longer shears a URL that legitimately contains a comma
  (query string, `data:` URI).
- **Consent rule persistence.** Site fields are now percent-escaped on
  serialization, so a whitespace-containing host can no longer shift
  adjacent fields when the rule file round-trips.

## [0.0.5] — 2026-06-22

Responsive-image fidelity: pictures now keep their aspect ratio, anchor where the
design asks, and download an appropriately sized source.

### Added
- **`object-fit` / `background-size` — `cover` & `contain`.** Images are no longer
  always stretched to their box; `cover` fills and crops, `contain` fits and
  letterboxes, both preserving aspect ratio (ADR-0044).
- **`object-position` / `background-position`.** Cover/contain images anchor by
  keyword or percentage (e.g. a hero cropped to its top), and the ubiquitous
  `background: url(…) center/cover` shorthand is now read, so cover/contain
  actually trigger on real sites (ADR-0045).
- **Responsive images — `srcset` / `sizes`.** `<img>` selects an appropriately
  sized candidate (bandwidth-first, at device-pixel-ratio 1) instead of fetching a
  blurry placeholder or an oversized original; fetch-time and draw-time agree on
  the choice so the bytes fetched are the bytes drawn (ADR-0046).

## [0.0.4] — 2026-06-19

UI polish + robustness: a real icon set, crisp hi-DPI text, a legible cookie
manager, faster (parallel) page loads, and assorted fixes.

### Changed
- **Real icon set.** The toolbar (back, forward, reload, stop, settings) and the
  cookie manager (close, reveal-eye, delete-trash) now render glyphs from a
  bundled icon font (IcoMoon subset) through the crisp glyph pipeline — replacing
  the letters `R`/`S` and the earlier hand-drawn vector shapes. A new
  `FontStyle::ICON` + `TextShaper::shape_icon` select the icon font per run.
- **HiDPI rendering is now crisp.** The shell previously rendered at logical size
  and bitmap-upscaled to the physical surface (soft text at >1×). The app now
  lays out in logical pixels and paints at physical resolution via a scaled
  display list — glyphs are **re-outlined** at the larger size — so text is sharp
  at 200%. Hit-testing stays in logical pixels (`DisplayList::scaled`,
  `FrameApp::set_scale_factor`).
- **Cookie manager is legible.** The per-cookie chip cycled through five
  unexplained states; it's now a clear three-state control — **allow / session /
  block** — with a legend ("allow = keep · session = forget on close · block =
  never store") and color-coding (green / amber / red) so the state reads at a
  glance. `Timed`/`Allow-once` remain in the engine (CLI/programmatic) but are
  out of the everyday cycle.

### Performance
- **Subresources load in parallel.** Page subresources (images, …) were fetched
  one-at-a-time on a single worker thread, so image-heavy pages crawled (Walmart's
  subresources took ~6.8 s). A small worker pool (4) now fetches them concurrently
  off a shared queue; memory stays within the gate (~15 MB).

### Security
- **HTTP response size is capped (DoS guard).** A response is now bounded at
  32 MiB of raw bytes; a huge or endless response is aborted instead of being
  read into memory until the process OOMs (issue #13).

### Fixed
- **Named HTML entities render correctly.** `&copy;`, `&mdash;`, `&eacute;`, … were
  shown literally (only `&amp;`/`&lt;`/`&gt;`/`&quot;`/`&nbsp;` + numeric refs were
  decoded). The decoder now covers the common named set (symbols, punctuation,
  accented Latin); numeric refs (`&#169;`, `&#x2764;`) handle the long tail.
- **In-page `#fragment` links no longer reload the page.** Clicking an anchor like
  `#maincontent` refetched the whole document (it would sit on "Loading…");
  same-document fragment navigation now just records history and updates the
  address bar without a network round-trip.
- **All buttons now align consistently.** Button labels were hand-placed per call
  site, so they drifted out of their boxes — most visibly the consent banner,
  whose `Allow`/`Deny`/`×` labels rendered *below* their chips. Every button
  (toolbar, consent banner, cookie manager) now draws through one
  `draw_button` primitive that centers the label, so this class of misalignment
  can't recur (ADR-0028). Close/delete use a proper `×`.

## [0.0.3] — 2026-06-17

Fixes from field testing on Windows: the browser now works on networks that
interfere with DNS-over-HTTPS, respects the display scale factor, centers its
button labels, and selects the whole address on focus.

### Fixed
- **Address bar: caret + select-all on focus.** Clicking the URL box now selects
  the whole address (highlighted) so the next keystroke replaces it — the
  browser convention — and a caret marks the insertion point while editing.
  (A steady caret for now; blinking, click-to-position, and drag/double-click
  selection need richer mouse/timer events from the shell and are a follow-up.)
- **Toolbar and cookie-manager button labels are centered.** Glyphs were drawn at
  a fixed corner offset, so the `< > R X` / `S` / head-chip labels and the cookie
  inspector's close/scroll/chip buttons looked misaligned. They're now centered
  in their boxes (a shared `push_centered` helper), and the close/delete glyph is
  a proper `×`. Buttons also gain a standard 1px border (affordance, a shade
  darker than the fill) and the URL box is bordered like a text field.
- **HiDPI scaling: the UI is no longer tiny on high-DPI displays.** The window
  shell ignored the OS scale factor, so at 200% the toolbar and fonts were drawn
  at half their intended on-screen size. The shell now renders in logical pixels
  (`physical ÷ scale`) and upscales to the physical surface, and maps pointer
  coordinates back through the scale. (Native crisp glyph rendering at >1× is a
  tracked follow-up; this fixes the size.)
- **Pages no longer fail to load when DNS-over-HTTPS is blocked.** Resolution was
  DoH-only against a single Quad9 endpoint with no fallback, so a network that
  blocked or mangled the DoH connection (e.g. a middlebox answering the DoH POST
  with HTTP 505) failed *every* page. Resolution is now an ordered chain —
  **Quad9 → Cloudflare → Google DoH, then the OS resolver as a last resort** —
  so a blocked resolver no longer kills all browsing. Encrypted resolvers are
  tried first; the system resolver runs only if all DoH endpoints are unreachable
  (ADR-0027).
- **DNS failures are reported accurately**, not as the misleading "this site
  doesn't support HTTPS" prompt — switching to plaintext http can't fix a name
  that never resolved.

## [0.0.2] — 2026-06-17

Preview fix release: the desktop binary now opens the browser when launched,
and carries the multi-identity UX + efficiency arc.

### Fixed
- **Double-clicking the desktop binary now opens the browser.** With no arguments
  the binary defaulted to the headless `render` command, so launching the `.exe`
  from a file manager just flashed a console and wrote `cerberus-home.ppm` then
  exited. A desktop (windowing) build now defaults to `run` (opens the window); a
  headless build still defaults to `render`. Run any subcommand explicitly to
  override (e.g. `cerberus render`, `cerberus run --mirror`).

### Added
- **Drivable mirror typing**: clicking a text field on the master captures it as
  the typing focus; keystrokes route to every sealed window as one coalesced
  `Action::Input`, so a follower converges in a single replay. Form controls are
  now clickable in the mirror too — ADR-0025.
- **"N profiles being driven" badge**: a small overlay on the mirror master
  window, e.g. "23 profiles being driven · github.com" — ADR-0025.

### Performance
- **Layout**: intrinsic-width measurement reuses one scratch context instead of
  allocating per flex/grid item per render; line buffers retain capacity — output
  identical — ADR-0026.
- **Mirror**: re-focusing a window already converged to the head of the log no
  longer rebuilds its realm or reloads the page (it renders from its resident
  snapshot); driving rebuilds on demand. At N=256 a warm focus sweep drops from
  ~2.3 s to ~0.2 ms — ADR-0026.
- **`mirror-bench`** gate: drives N sealed instances (focus sweep) and asserts
  resident memory after releasing dormant snapshots stays within budget (~12 MB at
  N=256), beside `mem-gate`/`bench` — ADR-0026.

## [0.0.1] — 2026-06-16

First tagged build. A memory-lean (~7 MB resident, ~8 MB binary), privacy-first
browser that renders real pages, with the multi-window/identity and rendering
work from this development arc. **Preview quality** — see "Known limits".

### Added
- **Rendering**: real CSS selector engine (child/sibling/pseudo-class/attribute
  combinators), `@media` queries (viewport-evaluated), and honored
  `visibility`/`opacity` (composited). **Flexbox v1** (row/column, `gap`,
  `justify-content`, `align-items`, wrap) and **CSS Grid v1** (explicit
  `grid-template-columns`, row-major placement) — ADR-0023.
- **Live layout-measurement JS**: `getBoundingClientRect`, `getComputedStyle`
  (cascade, not just inline), and `matchMedia` now return real values — ADR-0021.
- **Concurrent multi-window mirror groups** (`run --mirror`): one master drives N
  sealed-session windows of a site, reconciled to the ≤1-live-engine invariant via
  a semantic action log + lazy catch-up; dormant windows release their DOM so N
  stays cheap — ADR-0017/0018.
- **Arbitrary-N identities** (`identities` command) with per-identity sealed
  sessions.
- **Autofill**: per-identity login/address/card profiles, vault-sealed
  (incl. CVV); heuristic form-field detection; one master `Fill` fills every
  mirror window from its **own** profile; managed via the `profile` command —
  ADR-0022/0024.
- **Networking**: gzip/deflate response decompression (`miniz_oxide`) — ADR-0020.

### Changed / hardened
- Poison-tolerant `Mutex` locks — a panic in one section no longer sinks the
  session.

### Platforms
- **Linux (x86_64)** and **Windows (x86_64)** binaries are published on this
  release. macOS is not yet built/tested.

### Known limits (preview)
- Flex/grid are pragmatic v1 (no `flex-grow`/`-basis` factors, no grid
  auto-placement/spanning/`minmax`). HTTP/2 not implemented. Autofill's in-window
  manager UI and single-window fill gesture are not yet wired (use `profile` +
  `run --mirror`). CSS transitions/`@keyframes`/`transform` are not animated.

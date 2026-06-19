# Changelog

All notable changes to Cerberus are recorded here. Versions are small while the
browser is pre-1.0; this is the first tagged preview.

## [Unreleased]

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

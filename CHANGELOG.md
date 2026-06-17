# Changelog

All notable changes to Cerberus are recorded here. Versions are small while the
browser is pre-1.0; this is the first tagged preview.

## [Unreleased]

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

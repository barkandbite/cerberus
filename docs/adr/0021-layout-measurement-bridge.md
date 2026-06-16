# ADR-0021: Layout-measurement JS bridge (getBoundingClientRect / getComputedStyle / matchMedia)

- Status: Accepted
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0008/0012 (DOM bridge, persistent realm), ADR-0019 (CSS), #6

## Context

`getBoundingClientRect` returned all-zero, `getComputedStyle` returned only
inline declarations, and `matchMedia` never matched. SPAs (React/Vue/Angular,
infinite scroll, dropdowns, responsive JS) rely on these, so they misbehaved —
flagged by the production-readiness review.

## Decision

Feed Rust-side layout + cascade results into the **live** JS model over the
existing eval seam (no wire-format change, persistent realm intact), mirroring
the `__cerberusSetValue` pattern:

- New prelude setters `__cerberusSetGeometry(map)` and
  `__cerberusSetComputedStyles(map)` store per-node `__geometry` / `__computedStyles`
  keyed by JS node id. `getBoundingClientRect` returns the stored box;
  `getComputedStyle` merges the cascade map then inline `style=` (inline wins).
- Rust helpers `set_geometry(&[(id, Rect)])` and
  `set_computed_styles(&[(id, [(prop, val)])])` build the JS object literals and
  eval them — to be called by the app after layout/styling, keyed via the
  existing `node_to_js` map.
- `matchMedia(q)` is evaluated against `__CERBERUS_ENV__` (the viewport already
  injected at `install_page`), so it works immediately with no extra plumbing —
  `min/max-width`, `min/max-height`, `orientation`, comma-OR branches — matching
  the Rust `@media` semantics (ADR-0019).

## Consequences

- **Easier:** layout-dependent SPA scripts get real measurements; `matchMedia`
  feature-detection works now. Pure-additive to the bridge.
- **Harder:** the app must push geometry/computed-styles after each
  layout/restyle for `getBoundingClientRect`/`getComputedStyle` to be live in the
  window (the seam + the data path are in place and unit-tested; wiring into the
  interactive `render_frame` and one-shot `render` is the integration step).
- **Honesty:** geometry is the painted box extent (content), not per-fragment
  client rects; good enough for the dominant measure-an-element use, refinable.

## Alternatives considered

- **A new wire field per node carrying geometry/style.** Heavier and re-serializes
  every cycle; the live-inject setters avoid touching the snapshot format.
- **Run a full CSSOM in JS.** Large and duplicates the Rust cascade; pushing the
  already-computed values is leaner and single-source-of-truth.

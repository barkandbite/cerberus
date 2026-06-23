# ADR-0052: Evaluate `@media` against the real viewport (responsive CSS)

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Iteration 2 of the Wikipedia-parity push. The Vector header (`.mw-header`) is
`display:grid` at desktop widths but rendered as the narrow mobile `display:flex`
fallback. The grid rule lives in `@media screen and (min-width:1680px)`, which
*should* match a 1920px render — but didn't.

Root cause: the cascade evaluated `@media` against a **hardcoded 1280×800**
viewport (`CssEngine::new()`), regardless of the actual render/window size. So:

- Headless `render --width 1920` still asked "is the viewport ≥ 1680?" against
  1280 → **no** → the desktop rule was dropped.
- The interactive app cached its styled tree and never re-evaluated `@media` on
  resize, so breakpoints were frozen at whatever width the page first loaded at
  (and the engine's media was 1280 anyway).

This silently broke **every responsive layout** at any width ≠ 1280 — the parser
handled `screen and (min-width:…)` fine; the inputs were just wrong.

## Decision

Point `@media` evaluation at the real layout viewport, and keep it in sync.

- `CssEngine::set_media(w, h)` updates the media context without rebuilding the UA
  index.
- **Headless**: `render` builds the engine with `CssEngine::with_media(viewport)`.
- **Interactive app**: `update_media()` sets the engine's media to the current
  content-area size before every cascade (`set_document`, `restyle_with_sheets`),
  and records the width in `styled_w`. `render_frame` re-styles when the content
  width changes, so resizing across a breakpoint reflows — what a real browser
  does.

## Consequences

- `@media (min-width:1680px)` (and every other width/height query) now resolves
  correctly: at 1920 the Vector header resolves to `display:grid` as intended.
  Responsive layouts across the web — which overwhelmingly key off `min-width`/
  `max-width` — now select the right breakpoint at the user's actual window size.
- Resizing the window re-resolves media queries (previously frozen). The re-style
  is gated on a width change and the cascade is fast (ADR-0047), so steady-state
  frames don't re-style.
- This fix alone doesn't make the header *full-width*: as a shrink-to-fit flex
  item, the now-grid header's `minmax(0,1fr)` column collapses because our grid
  **intrinsic (max-content) width** under-counts flexible tracks. That's a
  distinct foundational gap (grid container intrinsic sizing) — the next
  iteration. Correct media selection is the prerequisite and stands on its own.
- New `media_type_and_feature_combined_track_the_real_viewport` test; full suite +
  clippy + bench (~31ms) + mem-gate (8.2MB) green; no new deps.

## Alternatives considered

- **Pass the viewport per `style()` call instead of storing it:** larger API
  churn; the engine already owns a `MediaContext`, so `set_media` is the minimal
  surface and supports resize cheaply.
- **Re-style every frame:** simpler but wasteful; gating on a width change keeps
  steady-state frames free.

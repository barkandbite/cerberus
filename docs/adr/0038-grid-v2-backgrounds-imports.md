# ADR-0038: Grid v2, `url()` backgrounds, `@import`/`@supports`, font policy

- Status: Accepted
- Date: 2026-06-21
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

With external stylesheets flowing (ADR-0037), the remaining rendering-fidelity
gaps were visible on real sites: CSS Grid was explicit-tracks-only (no
`minmax()`/`auto-fill`/spanning), `background-image: url()` wasn't fetched or
painted, `@import`/`@supports` rules were dropped, and downloadable web fonts
weren't handled. This ADR closes those, completing the fidelity arc.

## Decision

### Grid v2 (ADR layout)
- **Track sizing:** `minmax(min, max)` (a px floor + flexing max) and
  `repeat(auto-fill|auto-fit, <track>)` whose **column count is derived from the
  container width** (`floor((avail+gap)/(min+gap))`). A unified resolver gives
  each track a fixed base + `fr` weight and shares the leftover.
- **Placement:** items are auto-placed row-major into a 2-D occupancy grid that
  honors `grid-column`/`grid-row` **spans** (`span N` or `a / b`), so spanning
  items widen/reserve cells and never overlap.
- **Rows:** sized from `grid-template-rows`/`grid-auto-rows` when given, else by
  content; a multi-row item pushes any height deficit onto its last row.

### `url()` backgrounds
- `ComputedStyle.background_image` parses `background-image: url(...)` and the
  `url()` in the `background` shorthand (quotes stripped; `data:`/gradients/`none`
  yield none — only fetchable URLs).
- Background image URLs are collected from the **styled tree** (they live in
  computed style, not the DOM) and fetched through the **same image
  pipeline/consent gate** as `<img>`. A `paint_background` helper paints the
  color then the (stretched) image behind content in block/flex/grid boxes.

### `@import` and `@supports`
- `@import url(...)` is **recursively inlined** (bounded depth, consent-gated,
  resolved against the importing sheet) ahead of the sheet's own rules, in the
  one-shot fetch path.
- `@supports (...)` rules are **applied** (the condition isn't evaluated — we
  can't probe support; the inner rules are overwhelmingly safe progressive
  enhancements), recovering styles that were previously dropped.

### Web fonts — privacy-preserving substitution
- `@font-face` blocks are skipped and `font-family` is intentionally **not
  honored**: the font set stays **fixed to the bundled face**. Downloadable/system
  fonts are never read — a deliberate **anti-fingerprinting** property — and we
  avoid parsing untrusted font binaries (a classic attack surface) and a brotli
  (woff2) dependency. Text in any family renders in the bundled font, so pages
  render correctly; only the exact typeface differs.

## Consequences

- Responsive card grids (`repeat(auto-fill, minmax(...))`), dashboard spans, hero
  background images, `@import`ed and `@supports`-wrapped styles all render now —
  verified hermetically (grid column counts/spans/minmax via item background-rect
  widths; bg image paint; `@supports` applied + `@font-face` ignored) and across
  live site categories.
- The privacy posture is unchanged: background images and imported sheets are
  consent-gated like any subresource; fonts add no fingerprinting surface.

## Limitations (follow-ups)

- Grid: no explicit line-name placement, `grid-template-areas`, `dense` packing,
  or `fit-content()`; `%` rows need a definite container height (content-sized).
- Backgrounds: a single image, stretched to the box (no `background-size`/
  `-position`/`-repeat`, no multiple layers, no CSS gradients).
- `@import` inlining is the one-shot path (the interactive browser loads `<link>`
  sheets async; nested `@import` there is a follow-up).
- Web fonts are substituted by design (see above), not downloaded.

## Alternatives considered

- **Download web fonts for typeface fidelity:** rejected — conflicts with the
  fixed-font-set privacy property, needs a woff2/brotli dependency and untrusted
  font parsing, for cosmetic gain. Substitution renders pages correctly.
- **Evaluate `@supports` conditions:** rejected for now — we can't truthfully
  report feature support; applying inner rules is the higher-fidelity default.
- **Paint backgrounds from the DOM:** rejected — `background-image` is a computed
  style; collecting from the styled tree is correct and catches CSS-set images.

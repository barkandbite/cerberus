# ADR-0039: Block `width`/`max-width` + `float`/`clear`

- Status: Accepted
- Date: 2026-06-21
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Cross-category verification (ADR-0038) surfaced two layout features whose absence
broke real sites: plain-block **`width`/`max-width`** (centered max-width content
containers rendered edge-to-edge; flex/grid-only sizing couldn't express them)
and **`float`** (Bootstrap-3 and other float-grid sites — e.g. product grids,
sidebars — stacked vertically instead of sitting side-by-side). These are
required for the retail/ecommerce and general content categories to render
properly.

## Decision

Add both to the single-pass block flow.

- **`width` / `max-width` / `min-width` + `margin:auto`:** a block resolves its
  used content width from `width` (clamped by `min`/`max`), and is placed within
  the available width — `margin-left/right: auto` centers it, otherwise
  `margin-left` offsets it. The flow then runs in the constrained box, and the
  element's background and hit box use that box (so backgrounds are the content
  box, not the full line). `auto` width with no `max` is unconstrained (fills the
  line, as before).
- **`float: left/right` + `clear`:** consecutive `float` children pack
  left-to-right into a **float band**, wrapping to a new row when one doesn't fit
  (the column-grid pattern); a non-float child, text, or `clear` drops the flow
  below the band. Each float is sized from its `width`/`max-width` (else
  shrink-to-fit) and laid via the normal `walk` (so a floated block/flex/grid
  lays out correctly inside its box).

## Consequences

- **Verified live:** books.toscrape (retail) now renders a category sidebar +
  multi-column product-card grid (was a vertical stack); gov.uk now places its
  "Featured" sidebar beside the services list; centered max-width content
  containers no longer run edge-to-edge. No regressions on the previously-good
  sites (gov.uk, MDN, Apple, Stripe, Tailwind, Wikipedia).
- Backgrounds/hit-boxes are now the block's content box (more correct; excludes
  the left margin) — a deliberate behavior change verified against the suite.

## Cross-category verification (final)

One properly-rendering example per category, exercising positioning, `var()`/
`calc()`, flexbox v2, external CSS, grid v2, backgrounds, width, and float:

- **Government:** gov.uk — hero, 3-col "Popular" grid, services + Featured sidebar.
- **Reference/search:** Wikipedia, MDN — articles + multi-column card grids.
- **Retail/ecommerce:** apple.com (flex tile grid), books.toscrape (float grid).
- **Design/portfolio:** stripe.com, tailwindcss.com.
- **Video/streaming:** archive.org item page — nav, view/favorite/review stats,
  download options, collection metadata (the JS `<video>` player region is empty,
  as we don't run media playback).

## Limitations (follow-ups)

- `float`: right floats are left-packed; **text does not wrap around** a float
  (following content drops below the band). Good enough for column grids.
- `width`: no `box-sizing: border-box` accounting for padding/border (we don't
  model padding/border boxes); percentage widths resolve against the parent
  content width.
- **Client-rendered SPAs** (YouTube, TED, Vimeo, DuckDuckGo homepages) still
  paint blank — they build the DOM with framework JS that needs the full
  event-driven loop. This is the remaining cross-cutting gap (a separate arc),
  orthogonal to CSS fidelity; SSR/static pages in every category render.

## Alternatives considered

- **Approximate `float` as flexbox:** rejected — floats interleave with in-flow
  content and wrap differently; the float-band models the column-grid use
  directly while leaving normal flow intact.
- **Skip `width`, lean on flex/grid only:** rejected — centered max-width
  containers and float grids are pervasive and need real width resolution.

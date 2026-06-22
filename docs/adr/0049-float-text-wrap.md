# ADR-0049: In-flow content wraps beside floats (floated-infobox fix)

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

ADR-0039 introduced floats as a "float band" — floats pack left/right and wrap
into rows — but explicitly **did not model text wrap-around**: any in-flow child
following a float dropped *below* the band. Real content sites lead with a tall
`float:right` infobox/sidebar (Wikipedia's is ~1000px), so the entire article
body was pushed ~1000px down — below the fold, leaving the viewport apparently
blank even though the whole page (129 KB of text) was parsed and laid out. This
was the single biggest "renders wrong" symptom, affecting any floated-sidebar
page, and was independent of the consent/asset blocking fixed in ADR-0048.

## Decision

Let in-flow content **wrap alongside** open floats instead of always dropping
below them.

- New `flow_among_floats` runs before each in-flow text/element child while a
  float band is open. If the float still occupies this band (`self.y` is above
  its bottom) and at least `MIN_FLOAT_WRAP_WIDTH` (120px) remains between the
  left- and right-float cursors, it narrows the content box to that free band
  (`[fb.x, fb.right_x]`) so the child flows beside the float. Otherwise it drops
  below the band as before, restoring the full width.
- A `clear` still drops below the band unconditionally.
- Floats themselves are always sized against the container's full content box, so
  an earlier wrap never shrinks a later float; the full width is restored when
  the band closes.
- `MIN_FLOAT_WRAP_WIDTH` preserves ADR-0039's column-grid behavior: when floats
  fill the row (no meaningful free width) content still drops below. Wrapping is
  also skipped during intrinsic-width measurement, so measured widths (an upper
  bound) are unchanged.

## Consequences

- **Wikipedia's body now wraps to the left of its infobox** — the ~1000px gap is
  gone and the article renders as expected at a normal viewport; the same fix
  helps any floated-sidebar / pull-quote / floated-image-with-text layout.
- Approximate, not full CSS float layout: a block that *starts* beside a float
  keeps the narrowed width for its whole height rather than re-widening exactly at
  the float's bottom edge; once flow passes the float bottom, subsequent blocks
  get full width. This trades pixel-exact reflow for a large, robust correctness
  win with no line-box rewrite.
- No regression: all 67 layout tests pass unchanged (the column-grid and
  drop-below paths are preserved by the width guard); bench/mem-gate flat; no new
  deps. Float-infobox sizing (infobox sometimes wider than ideal) is a separate,
  smaller follow-up.

## Alternatives considered

- **Full line-box float intrusion (per-line shortening, à la browsers):** the
  correct general model, but a large rewrite of inline layout with high
  regression risk; the band-narrowing approximation fixes the dominant pattern at
  a fraction of the cost.
- **Special-casing `table.infobox`:** brittle and site-specific; wrapping any
  float is general and matches author intent.
- **Leaving it (document-only):** rejected — it was the top visible rendering
  defect on common sites.

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

Track each float individually (not one cumulative band) and let in-flow content
**wrap alongside** the floats that still overlap the current line, reclaiming the
full width once flow passes a float's bottom.

- `Floats` records every placed float as `(inner edge, bottom)` — left floats by
  their right edge, right floats by their left edge — plus the container box.
  `band(y)` returns the available `[left, right]` at vertical position `y`,
  counting only floats whose `bottom > y`; expired floats no longer constrain.
  This is the key correctness point: a tall lead infobox stops affecting
  paragraphs once flow drops past it, instead of every float on the page
  accumulating into one ever-narrowing band.
- `place_float` sizes the float (explicit `width`/`max-width` else shrink-to-fit),
  then drops to the first `y` whose `band` is wide enough (so a row of floats that
  fills the width wraps the next one below — the column-grid pattern), and records
  it.
- `flow_among_floats` runs before each in-flow child: it sets the content box to
  `band(self.y)`; if less than `MIN_FLOAT_WRAP_WIDTH` (120px) remains it steps `y`
  down past floats until the band widens (or fully clears), so content wraps
  beside a float and reclaims the full width below it.
- A `clear` (and the container's end) drops below all floats. Floats size against
  the container's full box; wrapping is skipped during intrinsic-width
  measurement, so measured widths (an upper bound) are unchanged.

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

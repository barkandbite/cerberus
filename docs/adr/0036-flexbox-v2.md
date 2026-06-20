# ADR-0036: Flexbox v2 — flexible item sizing + alignment

- Status: Accepted
- Date: 2026-06-20
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Flexbox v1 laid items at their content (intrinsic) width and only distributed
*leftover* space via `justify-content`. It had no concept of **flexible item
sizing** — `flex-grow`, `flex-shrink`, `flex-basis` — which is what real
layouts depend on: `flex: 1` to fill, a fixed-basis sidebar beside a growing
main, equal/proportional columns, and shrink-to-fit navbars. Without it, flex
containers under-/over-flowed and looked nothing like intended. Item-level
`align-self` and `order`, `flex-direction: *-reverse`, and `space-evenly` were
also missing.

## Decision

Implement a pragmatic CSS Flexbox Level 1 sizing + alignment pass over the
existing single-pass engine, sharing the row/column sub-layout machinery.

- **Style/CSS:** add item properties `flex-grow`, `flex-shrink`, `flex-basis`
  (`auto`/`content`/px/%), `align-self`, `order`, the `flex` shorthand
  (`none`/`auto`/`<grow> [<shrink>] [<basis>]`), plus container `flex-reverse`
  (from `*-reverse`) and `justify-content: space-evenly`. Item properties are
  reset per element (not inherited).
- **Flexible lengths (`resolve_flex`):** per flex line, compute each item's base
  size from `flex-basis` (else its max-content width), then distribute positive
  free space by `grow`, or negative free space by `shrink × basis`, iteratively
  freezing items at their **min-content** floor and redistributing — so a
  shrinking item wraps to its longest word rather than clipping to nothing. A
  new `measure_min_content_width` (lay out at 1px width, take the widest line)
  supplies the floor, and is only measured when a line actually overflows.
- **Placement:** items are ordered by `order` (stable), reversed for
  `*-reverse`; leftover space (when nothing grew) is placed by `justify-content`
  including `space-evenly`; the cross axis aligns per `align-self` (falling back
  to the container's `align-items`). `merge_sub` now shifts on both axes so the
  cross axis can offset.
- **Column direction:** the main axis (height) is content-sized (the container
  has no definite height), so grow/shrink there are no-ops; column items stack
  and align/stretch on the cross (width) axis (incl. `column-reverse`).
- **Scope:** only the page's main flow flexes its descendants; sub-flows
  (table cells, intrinsic measurement) are unchanged.

While implementing this, `StyledNode` grew past clippy's `large_enum_variant`
threshold, so `StyledChild::Element` is now **boxed** — which also *reduces*
memory: a `Text` child no longer reserves a whole `StyledNode`'s slack in the
children vector (a real saving on text-heavy pages).

## Consequences

- **Fixed:** `flex: 1` fills; fixed-basis + growing-main splits; equal and
  proportional (`flex:2` ≈ 2×`flex:1`) columns; navbars that push links to the
  edge with a flex spacer; `order` reordering; `space-between`/`-evenly`;
  `align-self`; row/column reverse — all verified hermetically (item background
  rects give exact widths) and on a visual demo through the real pipeline.
- **Memory:** boxing `StyledChild::Element` shrinks children-vector slots for
  text nodes from ~240 → 24 bytes (net win on text-heavy DOMs), at the cost of
  one box allocation per element node.
- **Gated by external CSS (the real-world multiplier):** re-rendering a site
  whose layout CSS is external (e.g. rust-lang.org) is *unchanged*, because
  Cerberus currently fetches only `<img>` subresources — it never loads
  `<link rel="stylesheet">`, so the cascade sees only inline `<style>` and
  `style=` attributes. Flex v2 (and positioning, `var()`, grid) therefore only
  manifest on inline-CSS content until external stylesheet loading lands. That
  is now the highest-leverage next step.

## Limitations (follow-ups)

- Cross-axis `stretch` in a **row** (item *height* filling the line) is treated
  as top-aligned; column cross-axis stretch (width) works.
- Column **main-axis** grow/shrink need a definite container height (not yet
  modeled), so they are no-ops.
- `*-reverse` reverses item order but packs from the start edge (the free-space
  side is not flipped).
- No `align-content` (multi-line cross distribution), `flex`/item `min/max-*`
  clamping beyond min-content, or nested-flex definite-cross sizing.

## Alternatives considered

- **Keep v1's uniform scale-to-fit:** rejected — it cannot express grow, fixed
  bases, or proportional columns, which are the whole point.
- **Allow `clippy::large_enum_variant` instead of boxing:** rejected — boxing is
  the recommended fix *and* saves memory (the project's #1 priority) on
  text-heavy pages.

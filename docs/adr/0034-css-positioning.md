# ADR-0034: CSS positioning (relative / absolute / fixed + z-index)

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Live-testing graphically-dense sites showed Cerberus reads content well but
**collapses the 2-D layout to a single source-order column**. Research + the code
confirmed `position` was entirely unsupported (the layout source even noted
"positioning … still ahead"), so every `relative`/`absolute`/`fixed`/`sticky`
element fell into normal flow — overlays, dropdowns, sticky headers, and
absolutely-placed chrome all land in the wrong place. Per the owner's pick,
positioning is the first rendering-fidelity track.

## Decision

Add CSS positioning to the single-pass flow engine as a **lay-in-flow then
reposition** pass (no engine rewrite), plus `z-index` paint ordering.

- **Style/CSS:** `ComputedStyle` gains `position` (`Position` enum),
  `inset_{top,right,bottom,left}` (a `Len` of `auto`/px/`%`), and `z_index`. The
  CSS engine parses `position`, the four inset longhands + the `inset` shorthand,
  and `z-index`. Positioning is not inherited.
- **`relative`:** laid out in flow, then its produced display items + hit boxes
  are **translated in place** by the resolved insets (left wins over right, top
  over bottom). It **keeps its flow space** (following content doesn't move).
- **`absolute`/`fixed`:** laid out in flow to measure, then **lifted out** — its
  items/boxes are drained into a `PositionedLayer`, translated to an absolute
  origin, and the flow is rewound so siblings ignore it. Width is
  **shrink-to-fit** (the element's intrinsic width), or the stretched width when
  both `left` and `right` are set, so right/bottom anchoring lands correctly.
  Containing block (v1): the **viewport** (initial containing block) —
  nearest-positioned-ancestor tracking is a follow-up.
- **`z-index`:** positioned layers paint **after** in-flow content, sorted by
  `z-index` then document order (so the higher z paints on top).
- **`sticky`:** parsed but laid out as normal flow until scroll containers exist.
- **Scope guard:** only the **root flow** positions; sub-flows (table cells,
  intrinsic-width measurement) keep today's in-flow behavior, so nothing
  regresses there (v1).

## Consequences

- **Easier / fixed:** overlays, badges, fixed bars, sticky-ish headers, modals,
  and absolutely-placed elements now render at their intended place and on the
  correct paint layer instead of dumping into the column (verified hermetically:
  absolute is out-of-flow at its inset origin; relative shifts while keeping its
  slot; z-index orders the layers — and visually on a synthetic page).
- **Still gated elsewhere:** the dense sites we tested (Wikipedia, rust-lang) are
  laid out with **CSS Grid + flexbox**, not absolute/fixed, so they need
  **flex/grid v2** to de-linearize — positioning helps the (very common) overlay/
  sticky/modal class of breakage, not grid-based page scaffolds.
- **v1 limitations (follow-ups):** containing block is the viewport only (no
  nearest-positioned-ancestor); positioned **flex/grid containers** aren't
  repositioned (they return before the positioning tail); positioned elements
  inside table cells stay in-flow; negative `z-index` still paints above in-flow
  content; real `sticky` needs scroll containers.

## Alternatives considered

- **A full out-of-flow sub-layout pass with a containing-block stack:** the
  "correct" architecture, but a large change to a 1230-line single-pass monolith
  (issue #20); the lay-in-flow-then-reposition approach reuses the existing block
  layout and ships a correct, tested v1 with far less risk.
- **Skip shrink-to-fit (use full flow width):** rejected — it pushes
  right/bottom-anchored boxes off-screen (observed); shrink-to-fit via the
  existing intrinsic-width measurement fixes it.
- **Implement `sticky` now:** deferred — it needs scroll containers, which don't
  exist yet.

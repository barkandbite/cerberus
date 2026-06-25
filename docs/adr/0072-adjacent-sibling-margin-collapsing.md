# ADR-0072: Adjacent-sibling vertical margin collapsing

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The block layout summed adjacent vertical margins (ADR-0049 era): the bottom
margin of one block plus the top margin of the next. CSS instead **collapses**
them — the gap is the *larger* of the two, not their sum. Without this, vertical
spacing between paragraphs, sections, and stacked components is roughly doubled
wherever both blocks carry vertical margins (the common case for prose and
card stacks).

Full margin collapsing (parent-child, empty-block, through-clearance) is famously
intricate, and the dangerous failure mode is **under-spacing → content overlap**,
which is strictly worse than the prior over-spacing and hard to catch without
visual review. So the bar was: a version that is *provably incapable of causing
overlap* and *local* enough to avoid the subtle flush-point bugs of a full
deferred-margin model.

## Decision

Implement **adjacent-sibling collapsing only**, as a local computation in the
parent's child-layout loop:

- Track `prev_block_mb` = the bottom margin of the previous **in-flow block**
  child (display `block`/`list-item`, not floated, not absolutely/fixed
  positioned). It is a loop-local, so it never leaks across containers.
- Before laying the next in-flow block sibling, subtract the overlap:
  `self.y -= min(prev_block_mb, next_margin_top)` (both clamped ≥ 0). Since the
  previous block already advanced `y` by its full bottom margin, the resulting
  gap is `prev_mb + mt − min(prev_mb, mt) = max(prev_mb, mt)`.
- Reset `prev_block_mb = None` on any **meaningful** intervening child —
  non-whitespace text, an inline/inline-block element, or a positioned/floated
  box — so collapsing happens only between *directly consecutive* block siblings.
  Whitespace-only text nodes (ubiquitous between block tags in HTML source) do
  **not** reset it.

### Why this can't overlap
Both margins are clamped ≥ 0, so the collapsed gap `max(prev_mb, mt)` is always
≥ 0 and ≥ each individual margin. The change only ever *removes* space that was
double-counted; it can never make two blocks closer than the larger of their
margins, so content cannot overlap. The worst case of any logic slip is reverting
to the prior (safe) over-spacing.

## Consequences

- **Correct paragraph/section/card spacing** for the overwhelmingly common case
  (consecutive block siblings). Prose and stacked components are no longer
  double-spaced.
- **Deliberately not handled** (still over-space, which is safe): parent–child
  margin collapsing (a parent's margin with its first/last child's), empty-block
  self-collapsing, collapse-through, and negative-margin collapsing (negatives
  clamp to 0, i.e. they sum as before). These are rarer and their omission only
  adds spacing, never removes it.
- **No regression:** all 81 layout tests stay green; the change is inert unless
  two in-flow block siblings both have vertical margins. Verified end-to-end by
  re-rendering Wikipedia/GitHub/Slickdeals/Target (identical status / scripts /
  images, clean spacing, no overlap — screenshot reviewed).
- Gates: `fmt`, `clippy -D warnings`, `cargo test --workspace`, `mem-gate`
  (7.5 MB), `bench` (48 ms). No new deps.

## Alternatives considered

- **Full deferred-margin model** (a `pending_margin` carried on the shared layout
  context, flushed at container boundaries / before inline content / before
  floats): models more of the spec (parent-child, empty blocks), but its
  correctness hinges on getting every flush point right, and a missed flush
  *under-spaces* → overlap. Rejected as too risky to land without per-site visual
  verification; the local sibling-only model gets the dominant benefit with an
  overlap-proof guarantee.
- **Leave margins summed:** the status quo — visibly wrong (doubled) spacing on
  most real content.

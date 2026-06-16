# ADR-0023: Flexbox + Grid v1

- Status: Accepted
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0005 (rendering stack), ADR-0007 (CSS engine), #6

## Context

`cerberus-layout` was block/inline flow only — `display:flex`/`grid` folded to
block, so the layout primitives the vast majority of modern sites rely on
rendered as a vertical stack. This was the biggest remaining rendering-fidelity
gap. The owner chose a **pragmatic v1** (common cases), not full spec.

## Decision

Add `Display::{Flex, Grid}` and the computed properties (`flex-direction`,
`flex-wrap`, `justify-content`, `align-items`, `gap`,
`grid-template-columns`/`-rows` as `Track = Px|Fr|Auto`), parsed in
`cerberus-css` (with a paren-aware track tokenizer + `repeat()`). In
`cerberus-layout`, `walk` branches `Flex`→`flex_layout` and `Grid`→`grid_layout`
before the block path, reusing the existing `Ctx::sub` sub-layout and a new
`merge_sub` (absolute-coord merge with a cross-axis `dy` shift):

- **Intrinsic sizing** (the capability the engine lacked): `Ctx` tracks `max_x`
  (the farthest the inline cursor reached); `measure_intrinsic_width` lays a
  subtree into an unbounded sub-context and reads it.
- **Flex:** row/column; items content-sized (intrinsic width clamped to the
  container), shrink-to-fit on overflow, `flex-wrap` into lines; `justify-content`
  (start/center/end/space-between/-around) distributes free space; `align-items`
  (start/center/end/stretch) aligns the cross axis; `gap` between items and lines.
- **Grid:** explicit `grid-template-columns` (Px fixed; Fr/Auto share the
  leftover after gaps); children placed row-major into cells; each row's height is
  its tallest cell.

Each flex/grid container also emits its background + an `ElementBox` over its
area, and per-item element boxes, so clicks/event-dispatch still work.

## Consequences

- **Easier:** nav bars, button rows, card grids, and column layouts render
  horizontally instead of stacking — a large step toward real-site fidelity.
- **Cost:** an intrinsic-measure pass per flex item (a throwaway sub-layout).
  Acceptable — `mem-gate` holds at 7.1 MB and the bench budget is unaffected for
  flex-free pages; flex/grid pages pay only for their own items.
- **Reversible:** the branches sit beside `table()`; removing them reverts to the
  block fallback.

## Scope / limits (v1, documented)

- Flex: no `flex-grow`/`flex-shrink` factors or `flex-basis` (items are
  intrinsic-sized then shrink-to-fit); single nesting axis per container.
- Grid: explicit columns only — no `grid-template-rows` sizing (rows are
  content-height), no auto-placement spanning, no `minmax()`/named lines.
- No `order`, no `align-self`/`justify-self`, no `place-*` shorthands.

These cover the dominant real-world cases; the long tail can grow behind the same
`flex_layout`/`grid_layout` seams.

## Alternatives considered

- **Full CSS Flexbox + Grid spec.** Months of multi-PR work; rejected for v1.
- **Keep the block fallback.** Status quo — modern layouts render wrong. Rejected.

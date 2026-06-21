# ADR-0042: Phase 2 layout correctness — heights, positioned CB, inline-block

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

After the box model + visual fidelity, the remaining *layout* errors were:
full-height heroes collapsed to content (`height`/`min-height` and `vh`/`vw`
unsupported), absolutely-positioned children resolved against the viewport
instead of their positioned ancestor, and `inline-block` lost its box model
(buttons/tags/nav cramped to their text).

## Decision

### Heights + viewport units
- `Len` gains `Vw(f32)`/`Vh(f32)`; a `resolve_vp(extent, vw, vh)` resolves them
  against the real viewport (already in `Ctx`). `parse_inset` accepts
  `vw`/`vh`/`vmin`/`vmax`.
- `ComputedStyle` gains `height`/`min-height`/`max-height` (`Len`). At a block's
  (and flex/grid container's) close, `resolve_block_height` enforces them
  (box-sizing aware; `%` heights — indefinite parent — are treated as auto).
- A flex container taller than its content **centers/﻿end-aligns** the content on
  the block axis (row → `align-items`, column → `justify-content`), by
  translating the laid items + hit boxes — the full-height hero pattern.

### Nearest-positioned containing block
- `Ctx` keeps a `cb_stack` of positioned ancestors' (in-flow) border boxes,
  pushed/popped around a positioned block's children. `absolute` resolves
  against the stack top (else viewport); `fixed` always the viewport;
  `relative` against the stack top (else page content area). Because a subtree is
  translated as a unit when its ancestor is lifted, resolving in the in-flow
  space is correct. Insets now use `resolve_vp` (so `vh`/`vw` insets work).

### inline-block
- `Display::InlineBlock`. In flow it's routed to `add_inline_block`, which lays
  it into a shrink-to-fit (or `width`-sized) sub **with the full block box model**
  (a one-shot `as_block_once` flag flips the block path on for the atom, avoiding
  re-routing/recursion), then places that box on the current line, advancing the
  inline cursor and wrapping if it overflows.

## Consequences

- Full-height heroes get their height (Apple's hero tiles now tall, not
  collapsed); badges/dropdowns inside `position:relative` cards land correctly;
  inline-block buttons/tags get padding/width and flow inline. No regressions on
  gov.uk/MDN/Stripe.
- **Memory/speed unchanged**: per-element style grows by three `Len` height fields
  (~24 bytes) and the `cb_stack` is a small transient vector; mem-gate 7.2 MB,
  bench layout+paint ~6.8 ms. 60 layout tests (incl. min-height/vh, flex vertical
  centering, nested-CB, inline-block) + full suite, fmt, clippy green.

## Limitations (follow-ups)
- The CB-stack pushes the ancestor's *in-flow* box with a viewport-height
  fallback, so `bottom`/`%`-height insets inside a positioned ancestor are
  approximate; cross sub-context (flex item / table cell) CB inheritance isn't
  tracked.
- inline-block: top-aligned on the line (no baseline alignment); horizontal
  margins and centered-line mixing with text are approximate.
- `%` heights against an indefinite parent are auto (not resolved).
- **Deferred to a later phase:** `overflow: hidden/scroll` clipping (and
  carousel sanity), and float text-wrap-around / right-floats.

## Alternatives considered
- **Resolve `vh`/`vw` at parse time** (via the engine's media size): rejected —
  the engine's media is a fixed default, not the actual render viewport; resolving
  at layout against `Ctx`'s real `vw`/`vh` is correct across sizes.
- **A full inline formatting context for inline-block** (baseline-aligned atoms
  in the line box): deferred — the atom-on-line approach covers buttons/tags/nav
  without rewriting the line builder.

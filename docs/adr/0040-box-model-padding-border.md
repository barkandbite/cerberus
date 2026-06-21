# ADR-0040: Box model — padding, border, box-sizing

- Status: Accepted
- Date: 2026-06-21
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

After the layout-structure work (flex, grid, float, width), real sites still
"looked like garbage" in one consistent way: **no internal spacing or
definition**. The engine modeled only margins — `padding`, `border`, and
`box-sizing` were ignored entirely. So cards had no internal breathing room,
buttons were cramped to their text, hero sections sat flush against edges, and
bordered elements (cards, inputs, separators) had no outline. This is the single
biggest *design-level* gap: padding/border are on nearly every styled box.

## Decision

Add the standard box model to block, flex, and grid boxes.

- **Style/CSS:** `ComputedStyle` gains `padding_{top,right,bottom,left}`,
  `border_{top,right,bottom,left}` widths + a single `border_color`, and
  `box_sizing`. Parsed from `padding`/`border`/`box-sizing` and their longhands +
  shorthands (`border: 1px solid #ccc`, per-side `border-top` etc., `padding`
  1–4 values, `border-width`/`-color`/`-style`). `border-style: none` clears it.
- **Layout (block):** a block's **border box** is placed within the available
  width (width/`margin:auto` from ADR-0039, now box-sizing aware); its **content
  box** is the border box inset by border + padding; flow advances by
  top/bottom border + padding. The background/`background-image` fill the border
  box, the **border paints as four solid edge rects** behind content, and the hit
  box is the border box.
- **box-sizing:** `content-box` (default) adds padding+border to `width`;
  `border-box` includes them within `width`.
- **Flex/grid containers:** content is inset by the container's border + padding,
  and the container paints its border box (items already get their own padding via
  the block path). Intrinsic-width measurement adds the right padding+border so
  padded boxes (e.g. buttons) size correctly.

## Consequences

- **Broad visual lift (verified live):** gov.uk's hero is padded and its services
  list has separators with a Featured card sidebar; MDN's Featured/Latest cards
  are bordered and padded; books.toscrape's product cards and "add to basket"
  buttons have definition; Apple's hero/product tiles have breathing room;
  Stripe's nav buttons read as buttons. This directly addresses the "cramped /
  undefined" look.
- Backgrounds/borders/hit boxes are the border box (correct box model). 51 layout
  tests (3 new: padding insets + grows; border paints 4 edges; box-sizing) + 66
  suites green; mem-gate 7.3 MB; bench layout+paint ~6.7 ms.

## Limitations (follow-ups)

- No `border-radius` rendering (corners are square; the paint layer draws rects).
- A single `border-color` (no per-side colors) and border styles render as solid
  (dashed/dotted/double collapse to solid).
- No padding/border on inline boxes or replaced elements beyond the existing form
  controls; `padding`/`border` `%` is treated as px-less (length only).
- `box-sizing` applies to width; height is content-driven (no explicit `height`).

## Alternatives considered

- **Keep deferring the box model:** rejected — it was the dominant remaining
  design-quality gap; layout structure without padding/border still looks
  unfinished on essentially every site.
- **Render `border-radius`:** deferred — needs rounded-rect support in the paint
  layer; square borders already give the needed definition.

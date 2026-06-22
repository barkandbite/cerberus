# ADR-0043: `overflow` clipping + right floats

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The two deferred Phase-2 items: content overflowing a box was never clipped
(`overflow: hidden/clip/scroll/auto` ignored — carousels laid every slide,
decorative symbol grids spilled, height-limited boxes leaked), and `float:right`
was left-packed like `float:left`.

## Decision

### `overflow` clipping (clip-marker approach)
- `ComputedStyle.overflow_clip: bool` — set by `overflow`/`-x`/`-y` =
  `hidden`/`clip`/`scroll`/`auto` (we clip rather than scroll; `visible` clears).
- Two tiny display items, `ClipPush { rect }` / `ClipPop`. `paint_box` wraps a
  clipping box's **content** (laid before the call) in a push/pop around the
  padding box, leaving the box's own background/border outside the clip.
- The `Framebuffer` holds one `clip: Option<Rect>`; `fill_rect`/`blend_pixel`
  drop writes outside it (with a whole-rect fast path, so the unclipped common
  case is ~one branch). The rasterizer keeps a small clip **stack**, intersecting
  on push — so nested clips compose. `translate_item` offsets `ClipPush` so a
  clipped subtree positions/merges correctly; drained positioned subtrees keep
  their push/pop together (balanced).

### `float:right`
- `FloatBand` tracks both a left cursor (`x`) and a right cursor (`right_x`);
  `place_float` packs `float:left` from the left and `float:right` from the
  right, wrapping when the cursors would cross.

## Consequences

- Carousels/scroll regions clip to their box (only the visible slide shows);
  height-limited boxes hide overflow; **MDN's decorative SVG-symbol "mandala"
  noise is now clipped away** (its container is `overflow:hidden`), leaving a
  clean hero. `float:right` elements sit at the right edge. No over-clipping
  regressions (gov.uk/Apple unchanged; content with no definite size isn't
  clipped because the box fits it).
- Memory/speed unchanged: 1 bool/element + 2 markers per clipped box + a small
  rasterizer stack; mem-gate ~8 MB after switches, bench layout+paint ~6.9 ms.
- 63 layout + paint tests (clip drop, overflow markers, right-float placement)
  + full suite, fmt, clippy green.

## Limitations (follow-ups)
- We clip rather than scroll (no scrollbars / scroll position); `overflow:scroll`
  content past the box is simply hidden.
- Clipping is rectangular (no `border-radius` corner clipping of content).
- An `absolute` descendant of an `overflow:hidden` *positioned* ancestor escapes
  the clip (painted in the on-top positioned layer).
- **Float text-wrap-around** (in-flow text flowing *beside* a float) is still not
  modeled — following in-flow content drops below the float band.

## Alternatives considered
- **Per-item clip rect** (every display item carries its clip): rejected — fatter
  items and more work; the push/pop + framebuffer-clip-stack is leaner and
  composes nested clips naturally.
- **Real scrolling for `overflow:scroll/auto`:** out of scope (no scroll model);
  static clipping is the correct first approximation.

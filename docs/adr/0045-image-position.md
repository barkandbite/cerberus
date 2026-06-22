# ADR-0045: `object-position` / `background-position`

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

ADR-0044 added `cover`/`contain`, but both always **centered** the crop or
letterbox. Real designs anchor elsewhere — a hero cropped to its top, a portrait
to its face, a thumbnail pinned bottom-right. The CSS controls are
`object-position` (`<img>`) and `background-position` (backgrounds), and — just
as importantly — most real cover backgrounds arrive through the **`background`
shorthand** (`background: url(x) center/cover no-repeat`), which ADR-0044 didn't
read, so `cover`/`contain` weren't even triggering on those sites.

## Decision

### Position as a fraction pair
- `cerberus_types::ImagePos { x: f32, y: f32 }` — each axis a fraction: `0.0`
  left/top … `1.0` right/bottom. Constants `CENTER` (`object-position` initial)
  and `TOP_LEFT` (`background-position` initial).
- `ComputedStyle.object_position` (default `CENTER`) and `background_position`
  (default `TOP_LEFT`), both **not inherited**. Plain 8-byte values, no `Box`:
  boxing an 8-byte payload would cost an allocation to save 4 inline bytes — not
  worth it, and it matches the plain-enum `object_fit`/`background_size` fields.

### Parsing (cerberus-css)
- Keywords (`left/right/top/bottom/center`) and percentages map to fractions; a
  one-value form centers the other axis; the keyword-order swap (`top left` ==
  `left top`) is handled by an axis hint per token. **Lengths are ignored** (no
  box at parse time, and pixel positions only matter for sprite sheets we don't
  tile — see Limitations).
- **`background` shorthand geometry:** a `<position> / <size>` group (e.g.
  `center / cover`) and a bare `cover`/`contain` are now extracted from the
  shorthand. Parenthesized spans (`url(...)`, `linear-gradient(...)`) are masked
  to spaces first, so a `/` inside a URL or a `%` in a gradient stop is never
  mistaken for geometry.

### Rasterization (cerberus-text `draw_image`)
- The anchoring offset generalizes ADR-0044's centering: `off = pos · (box −
  scaled)` per axis (the old code was the `pos = 0.5` special case). For `Cover`
  the leftover is negative (it slides the crop window); for `Contain` it's
  positive (it slides the image within the letterbox). `Fill` ignores position.
- `DisplayItem::Image` carries `pos` alongside `fit`; `<img>` uses
  `object_position`, block backgrounds use `background_position`.

## Consequences

- Cover/contain now anchor where the design asks (and, via the shorthand fix,
  cover/contain actually fire on the many sites that use
  `background: …/cover`). `center` reproduces ADR-0044 exactly.
- Memory: **+16 bytes per `ComputedStyle`** (two `ImagePos`) and +8 bytes per
  `Image` display item; no allocation, no new dependency. mem-gate 7.5 MB (flat).
- Speed: the per-pixel inner loop is unchanged (still two subtracts + a compare);
  only the once-per-image offset term changed. bench layout+paint ~7.0 ms, total
  ~32 ms ≪ 500 ms.
- Tests: css (one/two-value, keyword swap, ignored lengths; shorthand
  `center/cover`, `left top/contain`, bare `cover`, url-slash and gradient-`%`
  masking), layout (`pos` reaches the `Image` item for `<img>` and for a
  shorthand background; default center), and a rasterizer test proving left vs
  center shifts the cover crop and top vs bottom flips the contain letterbox.
  Full suite, fmt, clippy green.

## Limitations (follow-ups)

- **Lengths** (`object-position: 10px 20px`, sprite offsets) are ignored —
  position resolves to the default. Sprites also need `background-repeat`/tiling
  and `background-size: auto`, none of which we model, so this is consistent.
- Three/four-value positions with edge offsets (`right 10px bottom 20px`) are not
  parsed (the offset component is dropped → keyword fraction only).
- Only the single, first background layer is positioned (no comma-separated
  multi-layer backgrounds).

## Alternatives considered

- **Box the position (`Option<Box<ImagePos>>`):** rejected — an 8-byte payload
  doesn't justify an allocation; the inline pair is smaller in practice and
  branch-free.
- **Support length positions now:** deferred — they require box size at layout
  time and pair with sprite tiling we don't do; keyword/percentage covers the
  cases where cover/contain positioning is actually used.
- **A full `background` shorthand parser (layers, repeat, attachment, clip):**
  out of scope; the masked `<position>/<size>` extraction captures the dominant
  real-world pattern without a combinatorial grammar.

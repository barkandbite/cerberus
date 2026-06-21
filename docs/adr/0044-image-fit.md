# ADR-0044: `object-fit` / `background-size` (cover & contain)

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Phase-3 media. Images were always **stretched** to their box: `<img>` ignored
`object-fit`, and `background-size` was ignored, so a non-matching aspect ratio
distorted the picture (squashed hero photos, smeared logos, oval avatars). The
two CSS keywords that actually change scaling are `cover` (fill the box, crop the
overflow) and `contain` (fit inside, letterbox) — both aspect-ratio preserving.

## Decision

### One tiny enum, two style fields
- `cerberus_types::ImageFit { Fill (default), Cover, Contain }` — `Copy`, one
  byte, lives in the dependency-root types crate so paint/layout/style share it.
- `ComputedStyle.object_fit` and `ComputedStyle.background_size` (separate CSS
  properties on the same element, so tracked apart). Both default to `Fill` and
  are **not inherited**. They're plain enums (no `Box`) — `Fill` is the zero
  value, so the common element pays one byte each, no allocation.

### Parsing (cerberus-css)
- `parse_image_fit`: `cover` → `Cover`; `contain` and `scale-down` → `Contain`;
  `fill`, `none`, explicit sizes (`100% 50%`, `200px`), `auto`, anything else →
  `Fill` (stretch). Only the aspect-ratio keywords matter to the rasterizer.

### Plumbing
- `DisplayItem::Image` carries `fit: ImageFit`. `<img>` layout uses
  `node.style.object_fit`; a block's background image uses `style.background_size`.

### Rasterization (cerberus-text `draw_image`)
- One nearest-neighbor loop, parameterized by a per-axis source-step `(sxr, syr)`
  and a dest-space centering `(off_x, off_y)`:
  - `Fill`: step `iw/rw, ih/rh`, no offset — the existing stretch.
  - `Cover`: scale `s = max(rw/iw, rh/ih)`, step `1/s` both axes, negative offset
    centers the crop; samples outside the source never occur.
  - `Contain`: scale `s = min(...)`, positive offset centers; dest pixels whose
    back-mapped source coordinate falls outside `[0,iw)×[0,ih)` are skipped,
    leaving the box's existing background as the letterbox band.

## Consequences

- Photos/logos/avatars keep their aspect ratio: `cover` heroes crop instead of
  squash; `contain` thumbnails letterbox instead of stretch. Default behavior is
  byte-for-byte unchanged (`Fill` is the old path).
- Memory: **+2 bytes per `ComputedStyle`**, +1 byte per `Image` display item; no
  allocation, no new dependency. mem-gate 7.4 MB (flat).
- Speed: the per-pixel cost adds two subtracts + a compare on the image path
  only; bench layout+paint ~7.0 ms (within noise of the prior ~6.9 ms), total
  32 ms ≪ 500 ms gate.
- Tests: css parse (cover/contain/scale-down/fill/none + independence of the two
  properties), layout (the `fit` reaches the `Image` item for `<img>` and for a
  block background; default stays `Fill`), and a rasterizer test proving the three
  modes read distinctly (fill shows the cropped-in source edge, cover crops it and
  fully covers, contain leaves a letterbox band). Full suite, fmt, clippy green.

## Limitations (follow-ups)

- `object-position` / `background-position` are not honored — cover crops and
  contain letterboxes are **centered** only.
- `background-repeat` (tiling) and multi-value `background-size` per layer are out
  of scope (single, centered background image).
- `none`/explicit-size `object-fit` collapse to `Fill` rather than rendering at
  intrinsic size; acceptable until an intrinsic-size path exists.

## Alternatives considered

- **Honor `object-position`/explicit sizes now:** deferred — `cover`/`contain`
  are the overwhelming majority of real use; positioning is a clean follow-up
  that reuses the same offset term.
- **Resample (bilinear) while we're here:** rejected — nearest-neighbor matches
  the existing image path and is cheaper; quality is a separate, orthogonal call.
- **A single `image_fit` field shared by `<img>` and backgrounds:** rejected —
  they're distinct CSS properties that can differ on one element.

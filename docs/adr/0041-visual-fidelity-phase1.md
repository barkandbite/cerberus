# ADR-0041: Phase 1 visual fidelity — synthetic styling + gradients/radius/shadow

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

After the box model (ADR-0040), the remaining design-level gaps were *visual*:
bold was a faux smear and **italic rendered upright**; `line-height`,
`text-transform`, `letter-spacing` were ignored; and **gradients, rounded
corners, and shadows weren't painted** — so gradient heroes were blank, every
corner was square, and cards had no elevation. The owner's overriding constraint
is **speed/memory**: do this without bloating the resident set or the per-frame
cost.

## Decision

Implement Phase 1 memory-first — no new font assets, bounded rasterization, and
the rare style fields boxed.

### Typography (cerberus-text / -style / -css)
- **Faux-italic**: shear each glyph scanline rightward above the baseline (~12°)
  in the rasterizer; faux-bold (1px smear) unchanged. **Zero new bytes** — real
  weight/slant faces remain a drop-in asset swap behind the same path.
- **`line-height`** (normal/number/%/length → px), **`text-transform`**
  (upper/lower/capitalize, applied to a transient run String before shaping),
  **`letter-spacing`** (added to glyph advances in place). All inherited.

### Paint effects (cerberus-paint / -text / -css / -layout)
- Three lean `DisplayItem`s: `RoundRect`, `Gradient` (two-stop, vertical/
  horizontal), `Shadow`. Style gains `border_radius: u16` plus **boxed**
  `Option<Box<Gradient>>` / `Option<Box<BoxShadow>>` so the common element (no
  gradient/shadow) pays only a null pointer.
- `paint_box` emits: shadow (behind) → background (gradient/color/image) →
  border. With a radius the border is an outer `RoundRect` under the inset
  rounded fill; otherwise four square edge rects.
- CSS parses `linear-/radial-gradient` (first/last stop, direction), `box-shadow`
  (outer first layer), `border-radius` (uniform), with paren-aware comma/space
  tokenizers so `rgba(…)` survives.

### Rasterization — bounded by construction
- `RoundRect`: opaque `fill_rect` interior; only the four `r×r` corners are
  anti-aliased per-pixel.
- `Gradient`: one opaque scanline (row/col) per step when unrounded — `O(h)` or
  `O(w)`, not `O(area)`; per-pixel only when rounded.
- `Shadow`: only the ring outside the box (the box covers the interior), blur
  clamped to ≤ 40 px, quadratic falloff.

## Consequences

- **Visible:** italic emphasis (e.g. Stripe's hero) now slants; rounded cards/
  collages (Tailwind) and gradient/shadow boxes paint; heading weight and line
  spacing read correctly.
- **Memory/speed:** no new resident bytes; per-element style grows by ~18 bytes
  (radius + two null pointers); effects cost only when present and are bounded.
  Verified: mem-gate 7.3 MB (unchanged), bench layout+paint ~6.5 ms (unchanged).
- 62 layout + paint + css tests added/green across the three crates; full suite,
  fmt, clippy clean.

## Limitations (follow-ups)
- Gradients are two-stop (multi-stop collapses to first/last); angled gradients
  snap to vertical/horizontal; radial approximates as vertical.
- One `border-radius` (uniform) and one `border-color`; non-solid border styles
  render solid; `box-shadow` is outer, single-layer, no spread; `inset` ignored.
- Real bold/italic faces (crisper than synthesis) remain an optional later swap.

## Alternatives considered
- **Bundle Roboto Bold/Italic/BoldItalic faces:** rejected for now — ~0.5 MB+ of
  always-resident binary for a cosmetic gain; synthesis gives distinct bold/italic
  for zero bytes and is reversible.
- **Per-pixel gradient/round-rect everywhere:** rejected — the scanline/corner-
  only fast paths keep paint cost flat; per-pixel is used only where unavoidable
  (rounded gradients).

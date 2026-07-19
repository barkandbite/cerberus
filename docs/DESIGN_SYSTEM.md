# Cerberus design system

The shared visual language for **all** Cerberus chrome — the toolbar, the consent
banner, the settings panel, the developer console, the cookie inspector, the MIRC
roster, the performance HUD, and the mirror "driven" badge. The goal is that the
UI reads as *one product* instead of a set of hand-placed rectangles: one surface
palette, one accent, one spacing rhythm, one type scale, one set of corner radii,
one button/chip shape.

**Any new chrome must be built on these tokens and widgets. Do not introduce raw
hex, ad-hoc spacing, or bespoke button shapes.**

## Source of truth

The tokens and widget helpers live in
[`crates/cerberus-ui/src/lib.rs`](../crates/cerberus-ui/src/lib.rs) — the
`pub mod theme` block and the "reusable widgets" helpers just below it. **Code is
authoritative for exact values;** the hexes in this document are a quick reference
and may drift. When they disagree, believe the code and fix this doc.

Every UI surface is a **pure view**: it owns no state. It takes a borrowed model
(or plain args) plus a `Size`, and exposes `layout` / `paint` / `hit_test`.
Painting emits a `DisplayList`; nothing touches windowing or the network. Keep new
surfaces to that shape.

## Principles

1. **One accent.** The toolbar blue (`ACCENT`) is the only brand colour. Use it
   sparingly — for the primary/active state, focus, and links — never as a
   background for large areas.
2. **Tokens, not hex.** A widget reads `theme::ACCENT`, never `Color::rgb(...)`.
   A palette change then happens in exactly one place.
3. **One shape.** Every button and chip is a rounded rect from the shared
   helpers. Squares (`stroke_rect` borders) are legacy; do not add new ones.
4. **Semantics are paired.** A status colour is always a soft **tint** fill with a
   matching deep **ink** for its text (`SUCCESS_TINT` + `SUCCESS_INK`, etc.), so
   the label stays legible on the chip.
5. **Light surfaces for content chrome; the dark `INK` family for developer
   tooling** (console, HUD, driven badge). Both share the accent, spacing, radii,
   and type scale.

## Tokens

### Colour — surfaces & lines

| Token | Hex | Use |
|-------|-----|-----|
| `SCRIM` | `#14181F` @ 40% | Full-window dim behind a modal |
| `SURFACE` | `#FBFCFD` | Panel / card / toolbar-bar background |
| `SUNKEN` | `#F1F3F6` | Inset control (row, field, secondary button) |
| `RAISED` | `#FFFFFF` | Text field / focused input face |
| `DIVIDER` | `#E7EAEE` | Hairline between regions |
| `BORDER` | `#D4D9E0` | Control / panel edge |

### Colour — text

| Token | Hex | Use |
|-------|-----|-----|
| `TEXT` | `#1B2027` | Primary text |
| `TEXT_MUTED` | `#5C6672` | Secondary / supporting |
| `TEXT_FAINT` | `#8B94A0` | Section headers, placeholders, legends |

### Colour — accent & semantics

| Token | Hex | Use |
|-------|-----|-----|
| `ACCENT` | `#1E66E0` | The one brand colour: primary/active fill, focus, links |
| `ON_ACCENT` | `#FFFFFF` | Text/icon on an accent fill |
| `SUCCESS` | `#1EA55B` | Positive dot/indicator |
| `WARNING` | `#C77D11` | Attention (consent strip, diverged) |
| `DANGER` | `#C23838` | Error text / negative |
| `TRACK_OFF` | `#C6CCD4` | Toggle off-track |

### Colour — semantic chip tints (fill + ink pairs)

Always used together via [`pill`](#widgets): a soft fill with a deep, legible ink.

| State | Tint (fill) | Ink (text) |
|-------|-------------|------------|
| positive | `SUCCESS_TINT #DCF1E4` | `SUCCESS_INK #146C3A` |
| attention | `WARNING_TINT #FBECCF` | `WARNING_INK #8A5A0F` |
| negative | `DANGER_TINT #F7DADA` | `DANGER_INK #9A2A2A` |
| neutral | `NEUTRAL_TINT #EBEEF2` | `TEXT_MUTED` |
| accent (secondary verbs) | `ACCENT_TINT #E1EBFB` | `ACCENT_INK #1A4CA8` |

### Colour — dark developer-tooling surface (`INK` family)

| Token | Hex | Use |
|-------|-----|-----|
| `INK` | `#1B1E24` | Console / HUD / badge background |
| `INK_RAISED` | `#252A32` | Raised element on `INK` (title bar, stat chip) |
| `INK_BORDER` | `#39404B` | Border/divider on a dark surface |
| `ON_INK` | `#E6E9ED` | Primary text on dark |
| `ON_INK_MUTED` | `#99A2AE` | Muted text on dark |
| `ACCENT_ON_INK` | `#5B9CFF` | Accent that reads on dark (headers) |
| `ON_INK_POS` | `#7EE09A` | Positive value on dark (HUD figures) |
| `CONSOLE_ERROR` | `#FF7474` | `console.error` line |
| `CONSOLE_WARN` | `#E7B453` | `console.warn` line |

### Spacing (device px)

`SP_1 = 4`, `SP_2 = 8`, `SP_3 = 12`, `SP_4 = 16`, `SP_5 = 24`. Prefer these over
literals for gaps, insets, and padding.

### Corner radii

`RADIUS_SM = 6` (buttons, fields, rows), `RADIUS_MD = 8` (small cards / HUD),
`RADIUS_LG = 12` (modal cards). A **pill** uses `height / 2`.

### Type scale (px)

`TYPE_TITLE = 20` (panel title), `TYPE_BODY = 14` (row title, control label),
`TYPE_CAPTION = 12` (subtitle, detail, console/HUD text),
`TYPE_SECTION = 12` (uppercase section header, drawn in `TEXT_FAINT`).

## Widgets

Build with these; don't re-implement their geometry.

- `fill_round(list, rect, color, radius)` — a filled rounded rect.
- `bordered_round(list, rect, fill, border, radius)` — fill + a crisp 1px border
  (a grown rounded rect painted behind), so corners stay clean. The base for
  every card, button, and field.
- `round_button(list, shaper, rect, label, fill, border, text, px)` — a rounded
  text button (`RADIUS_SM`).
- `round_icon_button(list, shaper, rect, icon, px, fill, border, color)` — a
  rounded icon-glyph button. Icon codepoints are the bundled IcoMoon subset
  (`IC_CLOSE`, `IC_EYE`, `IC_TRASH`, `IC_GEAR`, …).
- `pill(list, shaper, rect, label, fill, text, px)` — a fully-rounded status chip
  (radius = half-height). Feed it a tint+ink pair.
- `toggle(list, rect, on)` — the iOS-style switch (accent track on, `TRACK_OFF`
  off, white knob with a soft shadow).
- `chevron_right(list, cx, cy, size, color)` — the "opens a sub-panel" affordance.
- `push_text(list, shaper, x, top, text, px, color)` — a left-anchored run
  (top-left origin; empty text is a no-op).
- `push_centered(list, shaper, rect, text, px, color)` — centred both axes.
- `section_header(list, shaper, x, top, text)` — a faint uppercase label.
- `row_labels(list, shaper, x, row, title, subtitle)` — the two-line body of a
  settings-style row.
- `text_width(shaper, text, px)` — advance width, for right-alignment / carets.

## Composition patterns

**Modal panel** (settings, cookie inspector, MIRC): full-window `SCRIM`, a
`Shadow` under the card, then `bordered_round(SURFACE, BORDER, RADIUS_LG)`. Title
in `TYPE_TITLE`/`TEXT`, subtitle in `TYPE_CAPTION`/`TEXT_MUTED`, a `DIVIDER`
hairline, and a `round_icon_button(IC_CLOSE, SUNKEN, BORDER, TEXT_MUTED)` close.
Group rows under `section_header`s; zebra alternate rows with a
`fill_round(SUNKEN, RADIUS_SM)`.

**Bar chrome** (toolbar, consent banner): a flat `SURFACE`/tinted strip with a 1px
`BORDER`/semantic separator; controls are rounded buttons. Focused field: `RAISED`
fill, `ACCENT` border, `ACCENT` caret, `ACCENT_TINT` select-all highlight.

**Dark overlay** (console, HUD, driven badge): `INK` (or `bordered_round` with
`INK`/`INK_BORDER`), `ACCENT_ON_INK` headers, `ON_INK`/`ON_INK_MUTED` text,
`ON_INK_POS` for positive figures, `CONSOLE_ERROR`/`CONSOLE_WARN` for log levels.

**Status chip**: `pill` with the matching tint+ink from the table above. Same
shape everywhere — cookie disposition, MIRC state, login state.

## Working rules

- **New chrome uses tokens + widgets only.** No raw `Color::rgb(...)`, no
  hand-placed labels, no square `stroke_rect` buttons.
- **Keep views pure** — no owned state; `layout`/`paint`/`hit_test` over a
  borrowed model.
- **Tests assert the rounded primitives.** A paint test counts `RoundRect` items
  (border+fill pairs), not `Rect`, for buttons/chips/cards.
- **Preview before committing.** Render the affected surface to a PNG with one of
  the example binaries and eyeball it:
  - `cargo run -p cerberus-app --example settings_preview`
  - `cargo run -p cerberus-app --example mirc_preview`
  - `cargo run -p cerberus-app --example dev_console_preview`
  - `cargo run -p cerberus-app --example chrome_preview` (toolbar, banner, HUD,
    cookie inspector)
- **Add a preview** for any genuinely new surface, so the look stays reviewable
  without a display server.
- If you need a new colour, add a **token** (and, for a status, a tint+ink pair)
  rather than inlining a hex — and record it here.

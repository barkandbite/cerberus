# ADR-0028: Standard button primitive + identity terminology (3-heads / MIRC)

- Status: Accepted
- Date: 2026-06-17
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Two recurring sources of confusion surfaced during Windows field testing:

1. **Button rendering drifted per call site.** Each button hand-placed its label
   at a fixed pixel offset (`rect.x + 6, rect.y + 15`, …). On HiDPI and across
   widgets this read as misaligned — most visibly the consent banner, whose
   labels rendered *below* their chips. The same drift was visible in the Linux
   screenshots too.
2. **Identity vocabulary was ambiguous.** The `work | personal | throwaway`
   switcher and the (future) feature for driving many identities at once were
   both loosely called "profiles/identities," making discussion error-prone.

## Decision

### Standard button primitive

All buttons are drawn through one helper, `cerberus_ui::draw_button(list, shaper,
rect, label, fill, text, px)`, which fills the rect and centres the label on both
axes (`push_centered`). No call site hand-places a button label. New buttons must
use it. This is the single source of truth for button alignment, so the class of
"label drifted out of its box" bugs cannot recur. (Glyphs are limited to what the
bundled Roboto contains — e.g. `×` U+00D7 is present, but geometric triangles
U+25B2/BC are not and render as tofu, so ASCII `^`/`v` are used for scrolling.)

### Identity terminology

- **3-heads button** — the `work | personal | throwaway` switcher (the existing
  fixed identity switch, tied to the Cerberus three-heads metaphor).
- **MIRC — Multi-Identity Remote Control** — a *separate, future* button/feature
  for configuring and **driving many identities at once** (the mirror-group
  remote control). Distinct from the 3-heads switch. **Deferred**: the basics
  (rendering, input, cookie UX) come first; MIRC is built on top later.

These names are used in code comments, docs, and UI going forward.

## Consequences

- **Easier:** consistent, correctly-aligned buttons everywhere; one place to
  evolve button styling (borders, hover, focus) later; unambiguous language for
  planning the headline MIRC feature.
- **Costs:** none material — `draw_button` is a thin wrapper; MIRC remains a
  design placeholder until scheduled.

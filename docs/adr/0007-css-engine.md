# ADR-0007: CSS engine & the speed-first "raw render" principle

- Status: Accepted
- Date: 2026-06-09
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

Modern sites are unrenderable at a basic level without CSS. It must be **modular**
(swappable later) and, per the dependency policy, **bootstrapped** (no foreign
crates — we can rewrite any part with our own code).

The owner also set a product directive: **speed first, a raw take of the page.**
Cerberus must **ignore programmed delays** — CSS animations/transitions, and
JS/scroll-triggered lazy reveals — and render content immediately. "There are
other browsers for ideal viewing; this one's function is pure speed."

## Decision

### Two crates, fully ours, behind a trait

- **`cerberus-style`** — neutral result types: `ComputedStyle`, `Display`,
  `TextAlign`, a `StyledNode`/`StyledDom` tree, and the **`StyleEngine` trait**
  (the seam). Layout depends only on these.
- **`cerberus-css`** — our `CssEngine: StyleEngine`: a CSS tokenizer/parser,
  selector matching, a specificity cascade, a built-in UA stylesheet, and color
  parsing. **No dependencies.**

Swap test (ADR-0001): delete `cerberus-css`, write another `StyleEngine`, and
layout/app compile unchanged.

### Supported subset (this iteration)

- **Selectors:** `*`, type, `.class`, `#id`, grouping `,`, descendant
  (whitespace). Child/sibling parsed but treated as descendant; pseudo-classes &
  attribute selectors tolerated and ignored. `@`-rules (`@media`, `@keyframes`,
  `@font-face`) skipped.
- **Cascade:** origin (UA < author < inline `style=`) → specificity → source
  order. `!important` stripped (treated as normal) for now.
- **Properties:** `color`, `background`/`background-color`, `font-size`
  (px/em/rem/%/pt + keywords), `font-weight` (bold), `font-style` (italic,
  tracked), `font` (shorthand, minimal), `text-align`, `text-decoration`,
  `display` (block/inline/list-item/none; flex/grid/table→block), `margin`
  (+ longhands), `white-space: pre`.
- **Colors:** `#rgb`/`#rrggbb`, `rgb()/rgba()`, ~50 named colors, `transparent`.
- **Sources:** UA stylesheet, `<style>` blocks, inline `style=`.

### Speed-first / raw render (cross-cutting principle)

Cerberus **never honors a programmed delay**. Concretely:

- **CSS:** `opacity`, `animation*`, `transition*`, `transform`, `visibility` are
  **not implemented** and never hide content. An element set `opacity:0` pending
  a fade/scroll-in renders immediately at full visibility (tested).
- **Images (next slice):** ignore `loading="lazy"`; prefer `data-src` over a
  placeholder `src`. Fetch everything up front — no scroll-gating.
- **JS (M3) — recorded as a hard requirement:** make `setTimeout`/`setInterval`
  fire effectively immediately, and report `IntersectionObserver` as
  always-intersecting, so scroll-triggered lazy loaders run at once.

`display:none` is still honored (it is a state, not a delay); when JS lands we'll
neutralize the reveal *triggers* rather than force-show every hidden node.

### Layout consumes the styled tree

`cerberus-layout` now flows a `StyledDom`, using `ComputedStyle` for display,
font size, color, **bold (faux-bold today)**, underline, margins, **element
backgrounds**, **text-align**, and list bullets. Links still emit clickable boxes.

## Deferred (flagged, behind the same seams)

External `<link rel=stylesheet>` (needs async sub-resource fetch), real Bold/
Italic fonts (faux-bold for now; a later asset swap), image **decoding**, form
**interactivity**, and `@media`/flex/grid/float/positioning.

## Consequences

- **Easier:** real, legible, colored pages with the cascade; the whole CSS layer
  is ours and swappable; the speed-first rules are centralized and testable.
- **Costs:** we own a (growing) CSS implementation; the subset will need to grow.
  Faux-bold is approximate. No new dependencies — consistent with the policy.

## Alternatives considered

- **A CSS crate (`cssparser`, `selectors`, `lightningcss`):** faster to broad
  coverage, but adds heavy dependencies and cedes the engine. Rejected — the
  modularity/own-our-code mandate wants this bootstrapped.
- **Honoring animations/transitions:** rejected by the speed-first directive.

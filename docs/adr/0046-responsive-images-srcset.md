# ADR-0046: Responsive images — `srcset` / `sizes`

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

`<img>` only ever fetched `data-src`/`src`, ignoring `srcset`/`sizes`. On
responsive sites that meant fetching whatever the bare `src` pointed at — often a
tiny placeholder (blurry) or a huge full-resolution original (wasted bandwidth and
decode memory, the opposite of the project's priorities). The selection must agree
between two passes: the **fetch-time collector** (`cerberus-app`) decides what to
download, and **layout** (`cerberus-layout`) decides what to draw — if they
disagree, layout looks up a URL that was never fetched and the image is blank.

## Decision

### One shared selector, called with the same viewport width
- `pub fn pick_img_url(attr, viewport_w)` in `cerberus-layout`: `data-src` (explicit
  lazy alias) wins; else the best `data-srcset`/`srcset` candidate; else plain
  `src`. It takes an `attr` closure so the DOM-walking collector (`NodeRef`) and
  layout (`StyledNode`) call the **identical** code.
- Both pass the **layout viewport width**: the collector uses
  `toolbar.content_size(last_size).w` (headless: `config.viewport.w`); layout uses
  `self.vw`. Same inputs ⇒ same choice ⇒ the fetched bytes are the ones drawn.

### Bandwidth-first candidate choice (`select_srcset`)
- **Width (`w`) descriptors:** resolve `sizes` to a target width, then take the
  **smallest** candidate whose width ≥ target (we render at device-pixel-ratio 1),
  or the largest if none covers it. Smallest-adequate = least bandwidth/memory.
- **Density (`x`, or bare = `1x`) descriptors:** take `1x` (the smallest density
  ≥ 1), since the framebuffer is 1×. No retina over-fetch.

### `sizes` resolution
- First entry whose media condition matches the viewport wins; a trailing
  condition-less entry is the default; absent/unparseable ⇒ `100vw`.
- Lengths: `px`, `vw`, `%` (of viewport). `em`/`calc()` fall back to the default.
- Media: `(max-width: Npx)` / `(min-width: Npx)` joined by `and`, compared to the
  viewport width; bare media types (`screen`) are ignored; unrecognized ⇒ no match.

## Consequences

- Responsive `<img>` fetches an appropriately-sized source instead of a placeholder
  or an oversized original — sharper where it was blurry, leaner where it was bloated.
- Memory/speed: selection is a few string splits per `<img>` at fetch + layout
  time; no new per-element state, no new dependency. mem-gate 7.6 MB, bench
  layout+paint ~7.0 ms, total ~32 ms — all flat.
- Tested: unit (`select_srcset` density 1x/bare/all-`>1`; width with `sizes` media
  branches, no-`sizes` `100vw`, none-adequate→largest, empty→`None`),
  `pick_img_url` precedence (data-src ▸ srcset ▸ src), and an end-to-end layout
  test where the provider serves only the expected key, so an emitted `Image`
  proves the selection. Full suite, fmt, clippy green.

## Limitations (follow-ups)

- ~~**`<picture>` `<source srcset media>` is not selected**~~ — **done in
  ADR-0048.** `pick_picture_url` now selects the first `<source>` whose `type` we
  can decode and whose `media` matches, resolving through the same `select_srcset`,
  and falls back to the `<img>` otherwise.
- Length positions in `sizes` beyond `px`/`vw`/`%` (e.g. `calc()`, `em`) fall back
  to the default width.
- DPR is fixed at 1, so density `2x`/`3x` assets are never chosen (by design — the
  software framebuffer is 1×; HiDPI paint up-scaling is a separate concern).
- A transient window resize between fetch and the next layout can momentarily pick a
  not-yet-fetched candidate; it converges on the following image request.

## Alternatives considered

- **Always fetch the smallest candidate:** rejected — too low quality for
  full-width imagery; smallest-*adequate* is the right bandwidth/quality balance.
- **Two-pass (layout first, then fetch by measured box width):** rejected — far more
  invasive than sharing one viewport-width-keyed selector across the existing passes.
- **Parse `srcset` per the full WHATWG grammar (commas inside URLs):** rejected —
  comma-split is adequate (URLs encode commas as `%2C`); the spec's edge cases
  aren't worth the code in a lean engine.

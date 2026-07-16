# ADR-0048: `<picture>` / `<source>` responsive-image selection

- Status: Accepted
- Date: 2026-07-11
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

ADR-0046 shared one viewport-keyed selector (`pick_img_url`) across the
fetch-time collector and layout so a responsive `<img srcset>` fetches and draws
the same candidate. It left `<picture>` as a follow-up: the `<img>` fallback child
rendered and the `<source>` children were ignored. That is safe (the fallback
always has a valid `src`) but misses two real cases:

- **Art direction** — `<source media="(max-width: 600px)" srcset="mobile.jpg">`
  supplies a different crop per viewport. Ignoring it always drew the desktop
  `<img>`, even on a narrow window.
- **Modern formats** — sites serve `<source type="image/avif">` /
  `type="image/webp">` ahead of a JPEG/PNG `<img>`. We must pick the first
  `type` our decoder actually handles (the bundled `image` crate does png/jpeg/
  gif/webp/bmp, plus SVG via resvg — but not AVIF), then fall through.

As with `srcset`, the **fetch collector** (`cerberus-app`, DOM `NodeRef`) and
**layout** (`cerberus-layout`, `StyledNode`) must resolve the *same* URL, or
layout looks up bytes that were never fetched and the image is blank.

## Decision

### One shared selector, same viewport (extends ADR-0046)
`pub fn pick_picture_url(sources, img_attr, vw, vh)` in `cerberus-layout`: walk
the `<source>` candidates in document order; take the first whose `type` we can
decode (`image_type_supported`) **and** whose `media` matches
(`picture_media_matches`), resolving its URL through the existing `select_srcset`
(so `srcset`/`sizes` still apply); otherwise fall back to the `<img>`'s own
`pick_img_url`. Both passes call this identical code with the same viewport, so
the fetched candidate is the drawn one.

### Type support mirrors the codec
`image_type_supported(mime)` is the static set of formats the `cerberus-image`
codec is built with. A `<source>` naming anything else is skipped rather than
fetched-and-failed.

### A self-contained `media` matcher
`picture_media_matches(query, vw, vh)` evaluates the dimension/orientation
features art direction uses — `min/max-width`, `min/max-height`, `orientation`,
and the `screen`/`all` media types — against the layout viewport. Every other
type or feature (`print`, `prefers-*`, unknown) does **not** match, so such a
`<source>` yields to the next candidate and ultimately to the `<img>`. This keeps
`cerberus-layout` free of a `cerberus-css` dependency and tracks the CSS engine's
fixed desktop-screen persona: a `<source>` gated on a preference we don't
advertise falls through to the plain `<img>`, the safe visible default. Because
both passes run the same matcher on the same viewport, they never diverge — even
where the matcher is deliberately conservative.

### Rendering hook
The layout walker gains a `"picture"` arm: it stashes the `<source>` candidates on
the context, lays the single `<img>` child out exactly as a bare `<img>` (so its
box, `alt`, `width`/`height`, and object-fit all still apply), then clears the
stash — it never leaks to a sibling image. A `display:block` `<picture>` breaks
the line around its image.

## Consequences

- Narrow viewports get the art-directed crop; AVIF-first `<picture>`s resolve to a
  format we can paint instead of relying on the fallback. Where no `<source>`
  qualifies, behaviour is exactly as before (the `<img>` renders).
- Memory/speed: a few string splits per `<picture>` at fetch + layout time; one
  `Option<Vec<..>>` of borrowed-then-owned source attrs held only across a single
  image's layout. No new dependency, no new per-frame allocation on `<img>`-only
  pages.
- Tested: unit (`image_type_supported` set; `picture_media_matches` width/
  orientation/`and`/`or`/media-type/empty/preference; `pick_picture_url` skips an
  undecodable `type` then falls back), end-to-end layout (a narrow viewport draws
  the `max-width` source, a mid viewport with no match draws the `<img>` — the
  provider serves only the expected key, so an emitted `Image` proves selection),
  and a collector test asserting the fetch list carries exactly the selected URL
  (not the `<img>` too). Full suite, fmt, clippy green.

## Limitations (follow-ups)

- `media` supports width/height/orientation only; a `<source>` gated purely on a
  preference (`prefers-color-scheme`) always yields to the `<img>`. That matches
  our fixed light-desktop persona and never mis-fetches, but a dark-mode image
  swap won't be honoured.
- DPR stays 1 (ADR-0046): a `<source>`'s density `srcset` still resolves at 1×.
- The `<picture>`'s own block box beyond the line break (margins/padding on the
  wrapper itself) is not modelled; the `<img>` carries the geometry, as in browsers.

## Alternatives considered

- **Resolve `<picture>` inside `cerberus-css` during styling:** rejected — the
  cascade crate has the media engine but not `select_srcset` (which lives in
  layout), and the DOM-walking collector wouldn't see a styled-tree mutation.
  Hosting the selector in `cerberus-layout` and calling it from both passes reuses
  ADR-0046's proven shape.
- **Reuse the CSS crate's full `@media` matcher:** rejected for the layout hot
  path — it would make `cerberus-layout` depend on `cerberus-css` (a sibling), for
  features `<picture media>` art direction never uses. The conservative in-crate
  matcher is consistent across both passes by construction.
- **Mutate the styled tree with the resolved `src`:** rejected — every restyle
  site would have to re-run resolution, and the collector walks the DOM, not the
  styled tree; a stateless selector at each walk avoids both.

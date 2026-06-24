# ADR-0056: `<img>` with one dimension preserves aspect ratio

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Multi-site parity (Slickdeals). A promo banner `<img height="60">` (a 1066×804
PNG with no `width`) rendered as a **garbled full-width, vertically-squished
band**. The decode was correct (`image` crate) and the blit was correct — the bug
was in layout sizing: with only `height` given, we used the image's **natural
width** (1066) together with the attr height (60), so the banner stretched to
1066×60 instead of its ~80×60 aspect-correct box. Setting only one of
`width`/`height` is extremely common; HTML/CSS derives the other from the
intrinsic aspect ratio.

## Decision

When exactly one of the `width`/`height` attributes is present, derive the other
from the decoded image's intrinsic ratio:

- both present → use both (explicit override);
- width only → `height = width * nat_h / nat_w`;
- height only → `width = height * nat_w / nat_h`;
- neither → natural size.

(The existing container-width clamp still applies afterward, scaling height with
it.)

## Consequences

- The Slickdeals banner now renders as a small, correctly-proportioned icon, not a
  stretched band. General fix: any image with a single dimension attribute keeps
  its aspect ratio. 72 layout tests + a new
  `img_single_dimension_attr_preserves_aspect_ratio`; clippy/bench/mem-gate green.
- Applies to decoded images (intrinsic size known). The not-yet-decoded
  placeholder path still uses the raw attrs (no intrinsic size to derive from),
  which is correct — it reserves the declared box until the image arrives.
- CSS `aspect-ratio` and `width:auto;height:Npx` (the CSS-property equivalents,
  vs. the HTML attrs handled here) remain a separate follow-up.

## Alternatives considered

- **Always use natural size, ignoring single attrs:** breaks pages that size an
  image by one dimension (the common case) — the original bug.
- **Honor the attrs literally (stretch):** what we did, and wrong — it ignores the
  intrinsic ratio the author relied on.

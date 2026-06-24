# ADR-0055: `max-content` / `min-content` / `fit-content` sizing keywords

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Multi-site parity. Real sites size boxes to their content with the intrinsic
keywords — e.g. Slickdeals' header nav has `min-width: max-content` so it never
shrinks below its links. We only parsed lengths/`%`/`auto`/viewport units, so
`width`/`min-width: max-content` was dropped and such boxes collapsed or wrapped.
These keywords are common, foundational modern CSS.

## Decision

Add the intrinsic sizing keywords to `Len` and resolve them by measuring content.

- **Style**: `Len::MaxContent` and `Len::MinContent`; `Len::is_intrinsic()`.
- **Parse**: `max-content` / `-webkit-max-content` / `fit-content[(…)]` →
  `MaxContent`; `min-content` / `-webkit-min-content` → `MinContent`. (`fit-content`
  is approximated as `max-content`.)
- **Layout**: a block with `width: max-content` resolves to
  `measure_intrinsic_width(node)`; `min-content` to `measure_min_content_width(node)`
  (longest unbreakable run), plus padding/border, clamped to the available width.
  **Critically, this is skipped during the measuring pass** — measuring the very
  node whose width we're resolving would recurse infinitely (caught by the new
  test as a stack overflow). During measuring the probe width already yields the
  content extent.

## Consequences

- `width: max-content|min-content|fit-content` now sizes blocks to their content,
  a building block many layouts rely on. 72 layout tests incl. a new
  `width_intrinsic_keywords_size_to_content` (which surfaced and now guards the
  recursion); clippy/bench/mem-gate green; no new deps.
- Scoped to **block** `width` for now. `min-width`/`max-width: max-content` and the
  flex/grid track contexts still ignore the keywords (they resolve to `None`), so
  e.g. Slickdeals' `min-width: max-content` on a flex item isn't yet honored — a
  follow-up, since those paths need the keyword threaded through their own sizing.
- `min-content` uses the existing longest-run measurement; `fit-content`'s clamp
  argument is ignored (treated as `max-content`).

## Alternatives considered

- **Resolve intrinsic widths in `resolve_block_width` (the free function):** it has
  no access to content measurement; resolving at the `walk` call site (which owns
  the measuring scratch) is the natural place and avoids plumbing a measurement
  callback into a pure helper.

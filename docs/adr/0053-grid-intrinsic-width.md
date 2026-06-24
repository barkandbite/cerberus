# ADR-0053: Grid intrinsic width is the sum of its columns

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Iteration 3 of the Wikipedia-parity push. With ADR-0052, the Vector header
correctly resolves to `display:grid` (`15.5rem` sidebar + `minmax(0,1fr)`), but it
still rendered far too narrow (~332px) and stayed centered, because it's a
shrink-to-fit flex item — its size comes from its **max-content width**.

Our grid max-content (the measuring path in `grid_layout`) collapsed the whole
grid to **one column = the widest single item**. So a two-column grid measured as
`max(col0, col1)` instead of `col0 + col1`. A grid container's max-content is, by
definition, the sum of its column tracks — measuring it as the widest item makes
any multi-column grid used as a shrink-to-fit item (page-shell headers, inline
grids, floated grids) come out far too narrow.

## Decision

When measuring a grid with an **explicit column template** (no `repeat(auto-fill)`),
size each track and **sum** them:

- `Px(n)` and `minmax(min, Px(n))` → the fixed size (`max(min, n)`).
- `Fr`, `Auto`, `minmax(min, fr|auto)` → the content max-content (floored by `min`).

`repeat(auto-fill, …)` keeps the prior one-column heuristic, because an unbounded
measuring probe would otherwise fabricate thousands of columns.

## Consequences

- A multi-column grid's intrinsic width is now its real max-content (sum of
  tracks). The Vector header widens accordingly (~332 → ~600px), and any grid used
  as an inline/float/flex shrink-to-fit box sizes correctly. All 69 layout tests
  pass plus a new `grid_intrinsic_width_sums_its_columns`; bench/mem-gate green.
- The header is improved but **still not full-width**: its flexible column's
  content is the `vector-header-end` flex box, whose own max-content under-counts
  (the search input's `max-width:31.25rem` and `flex-grow` aren't reflected in
  *flex* intrinsic sizing). Fully sizing the header needs that deeper
  intrinsic-sizing chain (flex max-content + `max-width` in intrinsic) — a
  separate, larger effort with diminishing per-pixel return on one strip.
- The flexible-column content is approximated as the grid's widest item rather
  than per-column placement; this can over-count when flexible columns hold
  differently-sized content, but over-counting (clamped to available) is far less
  harmful than the prior under-count, and the common page-shell case (fixed
  sidebar + flexible content) is exact.

## Alternatives considered

- **Per-column placement during measuring:** the fully-correct max-content, but it
  duplicates the placement logic into the hot measuring path with real divergence
  risk; the track-type approximation captures the dominant cases at far lower risk.
- **Leave it (widest-item):** wrong by definition for multi-column grids and the
  direct cause of the collapsed header.

# ADR-0054: `grid-auto-flow: column` + `grid-auto-columns`

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Multi-site parity testing (Slickdeals). The header's button row
(Deal Alerts / Post a Deal / Giveaways / Sign Up) rendered **stacked vertically**
instead of as a horizontal toolbar. The CSS:

```css
.iconSection {
  display: grid;
  grid-auto-flow: column;
  grid-auto-columns: minmax(0, 13ch);
}
```

We only implemented row-major auto-placement (the default `grid-auto-flow: row`)
and had no `grid-auto-columns`, so with no explicit column template every item
landed in its own row → a vertical stack. `grid-auto-flow: column` is a common
idiom for horizontal toolbars, chip rows, and carousels.

## Decision

Support column-major auto-placement and implicit column sizing.

- **Style**: `grid_auto_flow_column: bool`, `grid_auto_columns: Option<Track>`.
- **Parse**: `grid-auto-flow` (sets the flag if the value contains `column`),
  `grid-auto-columns`, and the `ch` length unit (≈ `0.5em`, a good stand-in for
  the width of `0`).
- **Layout**: when `grid-auto-flow: column` and there is no explicit column
  template, create `ceil(items / rows)` implicit columns (rows from
  `grid-template-rows`, else 1), each sized by `grid-auto-columns`
  (`auto_column_px`), and place items column-major (`item i → (i % rows, i / rows)`).

## Consequences

- Slickdeals' header buttons (and any `grid-auto-flow: column` toolbar/chip row)
  now lay out horizontally as authored. 70 layout tests + a new
  `grid_auto_flow_column_lays_items_horizontally`; clippy/bench/mem-gate green; no
  new deps.
- Scoped to the dominant case (column flow with implicit columns). The mixed case
  (`grid-auto-flow: column` *with* an explicit `grid-template-columns`) still uses
  the explicit template; full column-major packing across a fixed row count beyond
  `i % rows` is a later refinement.
- `ch` is approximated as `0.5em` rather than measured from the font's `0`
  glyph — close enough for track sizing; exact `ch` would need glyph metrics.

## Alternatives considered

- **Map `grid-auto-flow: column` to flexbox-row:** loses grid track sizing
  (`grid-auto-columns`, gaps, alignment) and the grid placement model; a real
  column-flow placement is simpler to reason about and composes with the rest of
  the grid engine.

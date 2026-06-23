# ADR-0051: CSS Grid named template areas

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

First iteration of a site-by-site parity push (render a real page, compare to a
mainstream browser, fix the *foundational* thing the page's authors assumed every
browser supports). Target: Wikipedia's Vector 2022 shell, whose three columns
(left TOC sidebar / content / right tools) never laid out — the sidebar spanned
full width and the content stacked below it.

The shell is a textbook **named-grid-areas** layout:

```css
.mw-page-container-inner {
  display: grid;
  grid-template: min-content 1fr min-content / 12.25rem minmax(0,1fr);
  grid-template-areas: 'siteNotice siteNotice' 'columnStart pageContent' 'footer footer';
}
.vector-column-start { grid-area: columnStart }   /* 12.25rem sidebar */
.mw-content-container { grid-area: pageContent }   /* 1fr content */
```

We resolved grid *tracks* (px/fr/auto/minmax/repeat) and span placement, but
`grid-area: <name>` was explicitly unresolved (a heuristic dumped named items in
the widest track) and there was no `grid-template-areas`. So every named item
landed in the 1fr column and stacked. Named areas are how modern sites lay out
their whole page shell — distinctly foundational.

## Decision

Implement named template areas end to end.

- **Style**: `grid_template_areas: Vec<Vec<String>>` on the container,
  `grid_area: Option<String>` on the item.
- **Parse**: `grid-template-areas` (quoted rows → cells; `.` = empty);
  `grid-area: <name>`; and the `grid-template: <rows> / <cols>` shorthand
  (top-level `/` split so `minmax(0,1fr)` survives). Line-number `grid-area`
  (`a / b / c / d`) still falls back to the content-track heuristic.
- **Layout**: `build_grid_area_map` reduces the area grid to each name's
  `(row, col, row-span, col-span)` bounding rectangle; a named item is placed
  there (within the resolved column tracks). Un-named items keep auto-flow. Track
  sizing is unchanged, so `12.25rem minmax(0,1fr)` yields a ~196px sidebar + a
  flexible content column.

## Consequences

- **Wikipedia's shell now lays out in three columns** (sidebar x=50 w=208 /
  content x=282 w=1096 / tools x=1254 w=124) instead of a full-width stack — the
  content column is the right width and the structure matches the reference. Any
  site whose page shell uses `grid-template-areas` (most modern CMS/skins)
  benefits.
- The TOC *text* inside the sidebar is still hidden by Vector's
  `.client-js … .vector-toc-list-item { display:none }` progressive-enhancement
  rules, which expect full JS expand/collapse — a JS-state limitation, not a
  layout one. Tracked separately.
- Row sizing for named areas is content-based (we don't yet honor
  `min-content`/`1fr` *row* tracks) — adequate for shells whose rows are
  content-sized; noted for a later pass. No new dependencies; all gates green,
  with a new `grid_named_areas_place_items_in_their_tracks` test.

## Alternatives considered

- **Keep the widest-track heuristic:** collapses every distinct area into one
  column — exactly the bug. Named areas need real row/column resolution.
- **Full grid line-name resolution (`[name]` lines, `grid-area: a / b`):** larger;
  named *areas* cover the dominant page-shell pattern. The line-name path still
  degrades to the heuristic.

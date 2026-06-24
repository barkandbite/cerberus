# ADR-0054: `calc()` in media-query values (breakpoint correctness)

- Status: Accepted
- Date: 2026-06-23
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Iteration 4 of the Wikipedia-parity push. The Vector header's account links
("Donate / Create account / Log in") rendered as zero width (`intrinsic=1`), so
the header stayed narrow even after the grid/intrinsic fixes. The links were
hidden by a *mobile* rule that collapses the user menu:

```css
@media screen and (max-width: calc(640px - 1px)) {
  .vector-user-links-main .user-links-collapsible-item { /* collapsed */ }
}
```

Our media-feature parser read the value by taking leading ASCII digits, so
`calc(640px - 1px)` yielded **no number** → the feature failed to parse → it was
dropped → the query's feature list became **empty** → `all()` over an empty list
is vacuously **true** → the rule matched at *every* width. A mobile breakpoint
therefore leaked into desktop and hid the links. Sites pin breakpoints with
`calc(<bp> - 1px)` constantly, so this silently mis-applied many `max-width`
rules.

## Decision

Evaluate `calc()` (and unit lengths) in media-query values:

- `eval_media_px` resolves `Npx`, `Nem`/`Nrem` (×16), a bare number, or a
  `calc(A ± B)` of those (CSS requires spaces around `+`/`-`, which makes the
  split unambiguous). `max-width: calc(640px - 1px)` → `639`.
- `min/max-width` and `min/max-height` now use it; an unresolvable value still
  yields `None` (the feature is skipped, as before).

## Consequences

- `@media … (max-width: calc(640px - 1px))` now matches only below 640px. At 1920
  the user menu is no longer collapsed: the account links measure their real width
  (`intrinsic` 1 → 197), so the header populates correctly (its content width
  522 → 719) and matches the reference's "logo · search · account links" row.
  Because Wikipedia (and most responsive frameworks) pin breakpoints with `calc`,
  this corrects many other `max-width` rules across the page too.
- The header is now content-correct but still centered rather than edge-to-edge:
  a flex container (`.vector-header-container`) shrink-wraps its single grid child,
  and full-bleed would need flex/`max-width` intrinsic-sizing refinements — a
  smaller, separate item with diminishing visual return.
- New `media_query_calc_breakpoint_does_not_vacuously_match` test; full suite +
  clippy + bench (~25ms) + mem-gate (8.1MB) green; no new deps.

## Alternatives considered

- **Treat an unparseable feature as non-matching (instead of vacuous-match):**
  more spec-correct for genuinely-unknown features and a good future hardening,
  but broader (could drop rules that currently render acceptably); fixing `calc`
  addresses the actual, common cause with no collateral.
- **A general `calc()` evaluator:** overkill here; media `calc` is overwhelmingly
  `<bp> ± <small>`, so the two-operand form suffices.

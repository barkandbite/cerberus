# ADR-0071: `!important` in the cascade

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The code-grounded audit (Epic #40) found a real cascade **bug**: the CSS parser
**stripped and discarded** `!important` (`parse_declaration_block` truncated the
value at `!important`), so an author's important declaration was treated as a
normal one. That mis-renders any site that uses `!important` to override an
otherwise-winning rule — e.g. a `.override { color: red !important }` losing to a
more specific `#id { color: green }`, or to an inline `style=`. This is a
correctness defect that changes what pages look like, not just a missing API.

## Decision

Preserve `!important` through parsing and honor it in the cascade with a
**two-pass apply**:

1. `parse_declaration_block` keeps `!important` in the value (no longer strips it).
2. The cascade's per-element apply runs **all normal declarations first** (matched
   rules in `(origin, specificity, source-order)` order, then inline), **then a
   second pass for important declarations** in the same source order. Because the
   important pass runs last, an important declaration wins over any normal one
   regardless of specificity or origin.
3. `apply_declarations` takes an `important_only` flag, calls a small
   `split_important` helper (fast-pathed on the absence of `!`) to separate the
   flag from the value, and skips declarations whose importance doesn't match the
   current pass — stripping `!important` before the property parser sees the value.
4. `collect_vars` (custom properties) and the `::before`/`::after` `content` pass
   also strip `!important` so it never leaks into `var()` substitutions or
   generated text.

**Scope simplification (correct for this engine):** the full CSS cascade reverses
origin order for important declarations (UA-important is the highest tier). Our UA
stylesheet contains **no** `!important`, so that tier is empty; applying author +
inline important declarations in source order after the normal pass is exactly
correct here. If a future UA rule needs `!important`, the important pass would gain
a reversed-origin sort — a localized change.

## Consequences

- **`!important` now works** — author and inline important declarations override
  normal ones regardless of specificity, matching browsers. Sites that rely on it
  (overrides, utility classes, print/spacing fixes) render correctly.
- **No regression:** all 44 `cerberus-css` tests and the full workspace suite stay
  green; the normal cascade is byte-identical for declarations without
  `!important` (the two-pass collapses to the old single pass when nothing is
  important). `split_important` fast-paths values with no `!`, so the per-value
  cost is a single `contains('!')` check.
- Gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace`,
  `mem-gate --budget-mb 64` (7.5 MB), `bench` (48 ms). No new deps.

## Alternatives considered

- **Tag declarations `(prop, value, important)` (a 3-tuple) through the type
  system:** the "purest" representation, but ripples the tuple type through
  `Rule.declarations`, `MatchedRule`, `apply_declarations`, `collect_vars`, and the
  pseudo pass. Keeping `(prop, value)` and reading importance from the retained
  value at apply time is lower-ripple and equally correct.
- **Per-declaration importance sort instead of two passes:** would require
  flattening rules into individual declarations and a custom comparator; the
  two-pass apply over the already-sorted `matched` list is simpler and reuses the
  existing ordering.

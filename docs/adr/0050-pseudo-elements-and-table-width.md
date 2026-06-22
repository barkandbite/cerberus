# ADR-0050: Pseudo-elements never match; tables honor their width

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Even after assets loaded (ADR-0048) and floats wrapped (ADR-0049), a Wikipedia
article's body still rendered as a ~160px-wide ribbon down the left edge with a
huge empty gap. Box-dumping the layout traced it to two distinct correctness bugs
in CSS handling:

1. **Pseudo-elements leaked onto real elements.** Wikipedia's stylesheet has
   `p::before { content:''; display:block; width:120pt; … }` (a print-mode
   spacer). We don't generate pseudo-element boxes, and the selector parser
   *dropped* the `::before` and applied the rule to the `<p>` itself — so **every
   paragraph got `width:120pt` (=160px)**. `_ => {}` ("ignore unknown pseudo")
   silently turned a pseudo-element selector into a matching pseudo-class one.
2. **Tables ignored their own width.** `table()` always spanned `self.left ..
   self.right`, so an infobox's `width: 22em` was discarded and the table ballooned
   to fill (and overflow) the available space.

## Decision

- **Pseudo-elements force a non-match.** The selector parser now distinguishes a
  pseudo-element (`::`-prefixed, or the legacy single-colon `:before` / `:after` /
  `:first-line` / `:first-letter`, plus `::marker` / `::selection` /
  `::placeholder` / `::backdrop` / `::file-selector-button`) from a pseudo-class.
  A pseudo-element pushes `Pseudo::Never`, so a selector ending in one never
  matches a real element and its declarations can't leak. Pseudo-*classes* are
  unchanged (`:first-child` still matches; unknown ones are still ignored
  leniently).
- **Tables honor `width`/`max-width`.** `table()` resolves the table's own width
  against the available space (`auto` / `100%` still fill it) and lays the columns
  within that, so a `width: 22em` infobox is 22em and floats reserve the right
  amount beside it.

## Consequences

- **Wikipedia's body text now fills the column** (lead paragraph 160px → ~956px)
  and wraps beside a correctly-sized ~200px infobox — the article reads like the
  reference rendering. Both fixes are general: pseudo-element spacers/quotes/icons
  no longer distort their host elements, and width-constrained tables (infoboxes,
  sidebars, fixed-layout tables) size correctly across sites.
- Pseudo-element *content* still isn't rendered (no `::before` text/markers) — out
  of scope here; the fix is purely about not corrupting the real element. A
  follow-up could generate simple `::before/::after { content }` boxes.
- No new dependencies. Guarded by a new selector-matching test
  (`pseudo_elements_do_not_match_the_element`) and the table-width path is covered
  by the existing table tests; full suite + bench/mem-gate green.

## Alternatives considered

- **Drop rules containing pseudo-elements entirely at parse time:** loses the
  (future) ability to render `::before` content and is coarser than marking the
  selector non-matching; `Pseudo::Never` reuses the existing state-pseudo path.
- **Map only `::before`/`::after` to non-match:** misses `:first-line` et al.,
  which would keep leaking; keying on "is this a pseudo-element" is complete.

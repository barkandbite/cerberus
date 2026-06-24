# ADR-0057: `font` shorthand — weight is not size

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Multi-site parity (Slickdeals). Section headings ("Trending", "Deals",
"Categories", "Popular", …) rendered at a **600px** font size — gigantic black
words overlapping the page. A font-size probe pinned it: those headings use the
`font` shorthand, e.g. `font: 600 16px/1.4 Arial`.

Our shorthand parser walked the tokens and called `parse_size` on **any token
containing a digit**, setting font-size from it. So `600` (the *weight*) parsed as
`600px` and became the font-size; the real size token `16px/1.4` then failed to
parse (the `/1.4` made the unit unrecognized), leaving 600px. The `font` shorthand
is ubiquitous, so this mis-sized headings across the web.

## Decision

Parse the `font` shorthand per its grammar:

- A token starting with a digit is the **size** only if it carries a unit or is
  `size/line-height` (has `/`, a letter, or `%`); take the part before `/`.
- A *bare* number is the **font weight** (100–900): set bold for ≥ 600, never the
  size.
- `bold`/`bolder` → bold; `italic`/`oblique` → italic (unchanged).

## Consequences

- `font: 600 16px/1.4 …` now yields size 16 + semibold, not a 600px font. Slickdeals'
  headings, store/category cards, and sidebar deals render at normal sizes; the page
  reads correctly. This was a broadly-impactful bug — any `font: <weight> <size>`
  shorthand was affected. New `font_shorthand_weight_is_not_size` test (covers the
  `px/lh` and `em` forms); full suite + clippy + bench/mem-gate green.
- Still a pragmatic subset of the shorthand: `font-variant`, `font-stretch`, and
  system-font keywords (`caption`, `menu`, …) are ignored; line-height is parsed
  off but not yet applied from the shorthand. Size + weight + style — the parts
  that change layout — are now correct.

## Alternatives considered

- **Parse strictly positionally (size is the token before the family):** more
  faithful but more brittle to optional leading tokens; keying on "has a unit vs.
  bare number" is robust and matches how the size/weight actually differ.

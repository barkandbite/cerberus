# ADR-0058: Honor the HTML `hidden` attribute

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Multi-site parity (GitHub). The logged-out page rendered a tall column of
screen-reader text down the left edge — "You signed in with another tab or
window. Reload to refresh your session…" — GitHub's `aria-live` status regions,
which carry the HTML `hidden` attribute until JS surfaces them.

Our UA stylesheet hid `script`/`style`/`svg` etc. but had **no `[hidden]` rule**,
so every element with the `hidden` attribute rendered. The `hidden` attribute is
ubiquitous (status regions, collapsed panels, JS-toggled content), so this leaked
hidden content onto many pages.

## Decision

Add `[hidden] { display: none }` to the UA stylesheet (the spec's UA rule for the
attribute). Our cascade already supports presence attribute selectors, so a JS
toggle that removes the attribute (or an author rule with higher specificity)
overrides it normally.

## Consequences

- GitHub's logged-out hero now renders clean (hero, sign-up form) with no
  screen-reader text column. General fix: any `hidden`-attributed element is
  removed from rendering across all sites. New `hidden_attribute_is_display_none`
  test; full suite + clippy + bench/mem-gate green.
- This is the attribute only. The CSS visually-hidden *clip* pattern
  (`clip: rect(0,0,0,0)` / `clip-path: inset(100%)` on a 1px box) is a separate
  mechanism we still don't honor — a follow-up if a site relies on it rather than
  `hidden`.

## Alternatives considered

- **Special-case `aria-live`/`role=status`:** narrower and wrong — those regions
  are often meant to be visible; it's the `hidden` attribute that removes them.
- **Honor it in the layout instead of UA CSS:** the UA-stylesheet rule is the
  spec-defined mechanism and composes with the cascade (authors/JS can override),
  unlike a hard-coded layout skip.

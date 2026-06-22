# ADR-0047: CSS selector subject-indexing (cascade perf)

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

On real pages the **style phase dominated load time** — ~1000 ms on a Wikipedia
article — and, because every interaction re-styles, it also made clicks feel
laggy. The cause: the cascade (`CssEngine::build`, run per element) scanned
**every** UA rule and **every** author rule and ran `Rule::matches` (which walks
the ancestor path) for each. With large inline stylesheets (Wikipedia inlines
thousands of rules) that is O(elements × rules) ancestor-walking matches.

## Decision

Index each stylesheet's rules by their selector **subject** (the rightmost
compound) and, per element, test only the rules that can possibly match.

- `RuleIndex` (parser.rs) buckets rule indices by the subject's most selective
  key: **id → first class → tag → universal**. A rule joins every bucket any of
  its selectors keys into (so `.a, #b` is found by class `a` *or* id `b`).
- Keying on a *necessary* condition for a match means indexing never drops a real
  match — the full `Rule::matches` still runs on each candidate, so specificity,
  combinators, `:nth-child`, `:not`, attributes, and `@media` are unchanged.
- `candidates(el)` unions the element's id/class/tag buckets + the (small)
  universal bucket, then sorts/dedups to **ascending source order** — so the
  cascade's `(origin, specificity, source-order)` tiebreak is byte-for-byte
  unchanged; only rules that never had a chance are skipped.
- The UA index is built once per engine (the UA sheet is fixed); the author index
  is built once per `style_with_sheets` call, not per element.

## Consequences

- **style on a Wikipedia article: ~1000 ms → ~120 ms (≈8×)**; full page load
  ~5.4 s → ~3.6 s. Interactions that re-style are correspondingly snappier.
- Output is identical — all 33 cerberus-css tests (selectors, specificity,
  combinators, `@media`, cascade order) pass unchanged, as do the layout tests
  that consume the cascade.
- Memory: one transient index per style pass — a few `HashMap<String, Vec<usize>>`
  bounded by rule count, dropped after styling. No new dependency; mem-gate flat.
- The remaining load cost is now JS execution (QuickJS), a separate concern.

## Alternatives considered

- **Cache the whole styled tree across renders:** larger, and wrong when the DOM
  mutates; indexing is a pure speedup with no invalidation surface.
- **Bloom-filter ancestor pruning (à la Servo/WebKit):** more complex; subject
  bucketing captures the bulk of the win for our rule counts.
- **Index by the full compound (tag+class together):** marginal extra selectivity
  for more buckets and more bookkeeping; single-key subject buckets are simpler
  and already cut the per-element set ~10–100×.

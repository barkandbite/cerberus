# ADR-0019: CSS selector engine, @media, and visibility/opacity

- Status: Accepted
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0007 (CSS engine), the production-readiness review, #6

## Context

The CSS engine (ADR-0007) matched only type/`.class`/`#id`/universal selectors
and the descendant combinator; child (`>`) and sibling (`+`, `~`) combinators
were parsed but treated as descendant, attribute and pseudo-class selectors were
ignored, and `@media` blocks were skipped entirely. `visibility` and `opacity`
were deliberately ignored under the old "speed-first raw render" directive, so
content a site hides behind a fade or `visibility:hidden` rendered anyway. The
production-readiness review flagged these as core reasons modern sites mis-render.

## Decision

Grow the bootstrapped CSS engine (still no dependencies) to a real selector
engine plus `@media` and the two display-affecting properties:

- **Combinators.** Each compound carries its relation to the compound on its
  left (`Descendant`/`Child`/`Adjacent`/`General`). Matching is a recursive
  backtracking walk over the ancestor path. Sibling combinators and structural
  pseudo-classes need sibling context, so each `ElemRef` on the path now carries
  its parent's element-children (shared via `Rc<[SiblingRef]>`, so the cascade
  stays O(n)) and its index among them — no parent pointers added to the DOM.
- **Attribute selectors:** `[a]`, `[a=v]`, and `~= |= ^= $= *=`.
- **Pseudo-classes:** structural `:first-child`/`:last-child`/`:only-child`/
  `:nth-child(an+b)`/`:not(…)`/`:root`, computed statically. State pseudo-classes
  (`:hover`/`:focus`/`:active`/`:visited`/`:link`/…) parse but **never match** in
  the static cascade — better to omit a hover style than to wrongly apply it at
  rest. Specificity keeps the `(id, class+attr+pseudo, type)` tuple.
- **`@media`.** Blocks are parsed (not skipped); each rule keeps its optional
  `MediaQuery` (an OR of AND-ed `min/max-width`, `min/max-height`, `orientation`
  features). `CssEngine::with_media(w, h)` threads the viewport; rules are gated
  on it in the cascade. `CssEngine::new()` keeps a desktop default.
- **`visibility` + `opacity`** become computed-style fields (`visibility`
  inherits; `opacity` resets per element). Honoring them in layout/paint is
  ADR-0021. Time-based effects (`animation`/`transition`/`transform`) remain
  ignored — no compositor/timeline yet.

## Consequences

- **Easier:** real-world stylesheets (responsive layouts, `nth-child` striping,
  attribute-driven form styling, sibling spacing) now cascade correctly — the
  single biggest correctness lever for modern pages short of flex/grid.
- **Harder:** the cascade allocates a per-level `Rc<[SiblingRef]>` (transient,
  O(n), watched by `mem-gate`). The backtracking matcher is more code than the
  old descendant-only walk.
- **Honesty:** state pseudo-classes are intentionally inert (no runtime hover
  state in the static render); documented, not silently mismatched.

## Alternatives considered

- **Match against the live DOM with parent/sibling pointers.** Rejected: the
  arena `Document` is immutable and parent-pointer-free by design (memory); the
  shared-sibling-list path keeps that property.
- **Keep ignoring `visibility`/`opacity`.** Rejected: it's a visible correctness
  bug (hidden content shown), and the fields are cheap to compute.
- **Full pseudo-class state tracking (`:hover` etc.).** Deferred: needs runtime
  input state plumbed into restyle; not required for first-paint correctness.

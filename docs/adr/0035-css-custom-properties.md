# ADR-0035: CSS custom properties + `var()` / `calc()`

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

After positioning (ADR-0034), the next rendering-fidelity gap was **CSS custom
properties** (`--name`) and the `var()` / `calc()` functions — entirely
unsupported, so any value referencing them was dropped and the property fell back
to its initial value. Modern sites theme almost everything (colors, spacing,
sizes) through `:root` design tokens, so a single unresolved `var()` cascades
into many wrong colors and gaps. It is also **foundational**: flex/grid v2 (the
next track) will read gaps/sizes that are themselves `var()`/`calc()` values, so
those must resolve first.

## Decision

Resolve custom properties, `var()`, and `calc()` inside the existing cascade,
*before* the per-property parsers run — so every property gains support for free.

- **Custom-property registry:** during the cascade each element gets a `Vars`
  (`Rc<HashMap<name → raw value>>`) that is its parent's registry plus any `--*`
  it declares (cascade order, later wins). It **inherits** down the tree. The
  `Rc` is shared by reference for elements that declare none — the common case —
  so only declaring elements pay for a clone (clone-on-write). Custom properties
  set no computed value themselves.
- **`var(--name, fallback)`:** substituted **at use** (when a normal property
  references it), recursively (a variable may reference another), with a comma
  **fallback** and a **cycle/depth guard** (a cycle resolves to empty, leaving
  the property at its initial value). Lookup keys are lowercased to match the
  parser's property-name folding (custom properties are effectively
  case-insensitive — a small, practical deviation).
- **`calc()`:** evaluated to a px length (or number) via a small recursive-descent
  parser supporting `+ - * /`, parentheses, and px/em/rem/pt/% units. `var()` is
  substituted first, so `calc(var(--pad) * 2)` works. Unevaluable `calc()` is
  left untouched.
- **Integration + fast path:** resolution runs in `apply_declarations`; values
  containing neither `var(` nor `calc(` (the vast majority) take a zero-copy fast
  path, so the common case is unaffected.

## Consequences

- **Fixed:** themed colors and spacing now resolve site-wide — `:root` tokens,
  inheritance, nested variables, `var()` fallbacks, and `calc()` length math all
  work (verified hermetically and on a themed demo page through the real
  pipeline). This unblocks correct values for the flex/grid work to consume.
- **Not a structural change:** pages whose breakage is *layout* (flex/grid, e.g.
  rust-lang) still linearize — `var()` corrects values, not structure. Flex/grid
  v2 remains the next visible unlock.
- **v1 limitations (follow-ups):** `calc()` resolves `%` against the font size
  (consistent with the engine's existing `%`→px handling) and reduces to px, so
  `calc()` mixing `%` with px loses the symbolic `%` that plain insets keep;
  custom properties are case-insensitive and the `var(`/`calc(` keywords are
  matched case-sensitively (matches ~all real usage); registered/typed custom
  properties (`@property`), and animating variables, are out of scope.

## Alternatives considered

- **Store resolved custom-property values eagerly at declaration time:** rejected
  — forward/cross references (`--a: var(--b)` where `--b` comes later or from a
  sibling rule) wouldn't resolve. Storing raw + resolving recursively at use is
  both simpler and more correct.
- **Deep-clone the registry for every element:** rejected for cost on
  token-heavy pages; clone-on-write keyed on "declares a custom property" keeps
  the walk O(n) for the overwhelmingly common non-declaring element.
- **A full tokenizer / typed `calc()` with unit algebra and symbolic `%`:**
  deferred — the recursive-descent px evaluator covers the common theming math at
  a fraction of the complexity; symbolic `%` matters mainly for insets/sizes that
  layout resolves, a later refinement.

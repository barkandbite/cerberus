# ADR-0067: `::before` / `::after` generated content

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The web-platform completion arc asks that modern, CSS-heavy sites render
correctly. A frequent remaining visual gap was CSS **generated content**:
`::before` / `::after` pseudo-elements with a `content` property. Sites use these
pervasively for non-decorative, *visible* text — breadcrumb separators
(`content: " › "`), required-field marks (`content: "* "`), quotation marks,
chip/badge labels, and icon-font glyphs. Without them, separators and labels
silently vanish from the page.

The cascade already *parsed* these selectors, but deliberately forced them to
**never match** a real element (`Pseudo::Never` on the compound). That was a
correctness fix for a real leak — `p::before { width: 120pt }` was sizing every
`<p>` — but it also meant their `content` was dropped entirely. We don't generate
independent pseudo-element boxes (no box-tree node, no separate computed style),
so the question was how to surface the generated *text* without re-introducing the
leak or growing per-node memory (priority #1).

## Decision

Render `::before` / `::after` `content` as the host element's **leading /
trailing inline text**, inheriting the host's text style. The entire feature lives
in `cerberus-css`; no other crate changed.

- **Selectors keep their host matchable, but stay out of the element cascade.**
  Each `Selector` is tagged with a `PseudoEl` (`None` / `Before` / `After`) lifted
  from its subject compound. `Rule::matches` (element styling) now filters to
  `pseudo == None`, so a `::before` rule still never styles the real element — the
  leak fix is preserved, now at the selector level instead of via `Pseudo::Never`.
  A new `Rule::matches_pseudo(path, kind)` feeds a separate pass.
- **A separate, additive pseudo-content pass** (`pseudo_content`) runs the *same*
  subject-indexed cascade (ADR-0047), restricted to `::before` / `::after`
  selectors, resolves the winning `content` (cascade-ordered, `var()`-substituted),
  and injects a `StyledChild::Text` at the front / back of the host's children.
  Layout, paint, and `--dump-text` then handle it as ordinary inline text — zero
  new code in `cerberus-layout` / `cerberus-paint`.
- **`content` value support:** quoted strings with CSS escapes (`\2192`-style hex
  with the single-trailing-space terminator, and `\<char>` literals), `attr(name)`
  against the host, and mixed sequences (`"[" attr(label) "]"`). `none` / `normal`
  / empty generate nothing; `counter()` / `url()` / `open-quote` and other
  functions we can't render as text are skipped (not mis-rendered).
- **Scoped to the renderable two.** Every other pseudo-element (`::first-line`,
  `::marker`, `::selection`, …) still maps to `Pseudo::Never` — we generate no box
  for them, so they must not match. Replaced / void hosts (`img`, `input`, `br`,
  …) and `display:none` hosts generate no pseudo content.

## Consequences

- **Generated separators, marks, and labels now render** — the common, visible
  uses of `::before` / `::after`. Inheriting the host's text style is correct for
  the overwhelming majority (the generated run is short text in the host's font /
  color).
- **Zero per-node memory cost.** `content` is *not* added to `ComputedStyle`
  (which is stored on every styled node); it is resolved transiently in the pass
  and discarded. The only memory is the injected text on pages that actually use
  generated content. `mem-gate` stays at 7.6 MB.
- **Near-zero cost when unused.** The per-element pass runs only if some author
  rule targets a pseudo-element (`Rule::has_pseudo`, checked once per render);
  pages without generated content pay one boolean.
- All gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace`,
  `mem-gate --budget-mb 64` (7.6 MB), `bench` (37 ms). No new third-party deps.

## Limitations (deliberate v1 bounds)

- The generated content inherits the host's text style; **pseudo-specific styling
  is not applied** (its own `color` / `font-size` / `background` / box `width` /
  borders). This is the box-tree generalization we explicitly defer — the text is
  the high-frequency win. A pseudo with heavy own-box styling (a sized icon chip)
  renders its text but not its independent box.
- `counter()` / `counters()` (list/section numbering) and quote nesting are not
  evaluated; `content: attr()` and strings are.
- `::marker` content is still dropped (list markers come from the UA `list-item`
  path, not generated content).

## Alternatives considered

- **Add `content` to `ComputedStyle`:** would reuse `apply_declarations`'
  `var()` / cascade handling for free, but costs ~24 bytes on *every* styled node
  for a property only pseudo passes read — rejected against the memory-first
  priority. Resolving transiently in the pass costs nothing per node.
- **Generate real pseudo-element box nodes (synthetic `StyledNode`s):** the
  faithful long-term model (own computed style, hit testing), but materially more
  invasive across layout / paint / event correlation and higher risk. Deferred;
  the text-injection path is a strict subset its future version can subsume.
- **Keep `Pseudo::Never` and special-case in layout:** would scatter pseudo logic
  into the layout crate; keeping the whole feature in the cascade (where selector
  matching already lives) is cleaner and leaves `cerberus-layout` untouched.

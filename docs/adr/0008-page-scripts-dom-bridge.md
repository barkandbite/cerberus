# ADR-0008: Page-script execution & the DOM bridge (snapshot/replay)

- Status: Accepted
- Date: 2026-06-10
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

M3 wired a real JS engine (QuickJS, ADR-0002) behind the `JsEngine` seam, but it
only runs the per-head farbling prologue and the speed-first prelude — **not a
page's own `<script>`s**. Running page scripts so they can build/transform
content is the next M3 step, and it forces a question the seam deliberately left
open: **how does JavaScript see and mutate the DOM?**

Two hard constraints shape the answer:

1. **`cerberus-dom`'s `Document` is immutable.** It is built once (by the parser
   or `DocumentBuilder`) and thereafter read-only via `NodeRef`; there is no
   in-place mutation API. This was a deliberate "render tree doesn't change"
   choice.
2. **The `JsEngine` seam is eval-only** (`eval`, `inject_prologue`,
   `create_realm`, …) and DOM-agnostic — by design, so the engine stays
   swappable (ADR-0001 / ADR-0002). It exposes no way to bind native callbacks.

A live, browser-style DOM (JS objects backed by Rust closures mutating the arena)
would fight *both*: it needs a mutable `Document`, a shared `Rc<RefCell<…>>`, an
escape hatch to bind native functions through the seam, and `unsafe`-adjacent
lifetime juggling in the engine adapter.

## Decision

**Bridge the DOM by snapshot/replay, in an engine-agnostic crate
`cerberus-js-dom` that depends only on the `JsEngine` *trait* and
`cerberus-dom`.** One render pass with scripts is:

```
parse → Document ──serialize──▶ JSON snapshot ──eval──▶ JS `document` model
                                                            │  run page <script>s
                                                            ▼
render ◀── rebuild Document ◀── JSON ◀── eval(JSON.stringify(serialize))
        (DocumentBuilder)         (Rust JSON parser)
```

1. **Parser keeps inline scripts.** `cerberus-dom` collects inline `<script>`
   bodies out-of-band into `Document::scripts()` (document order); they never
   enter the render tree, so they can't render.
2. **Snapshot in.** `serialize_document` walks the immutable `Document` to a
   compact JSON tree (flat node list + child-id links) and injects it as a
   global the JS model reads.
3. **A `document` model implemented in JavaScript** (`DOM_MODEL_PRELUDE`,
   evaluated into the realm before the page scripts) provides `getElementById`/
   `querySelector` (simple selectors), `createElement`, `textContent`,
   `classList`, `appendChild`, `addEventListener`, `window`, `console`, etc.,
   operating on the snapshot. `DOMContentLoaded`/`load` are fired synchronously
   after the scripts (no waiting — consistent with the speed-first principle,
   ADR-0007). A throwing page script does **not** abort the run (browsers
   continue to the next script).
4. **Replay out.** The model serializes itself back to JSON; the Rust side
   parses it and **rebuilds a fresh `Document` via `DocumentBuilder`**. The app
   swaps in the rebuilt document, then lays out and paints.

### Why snapshot/replay over live bindings

- **Fits the immutable-`Document` design.** We *rebuild* via the existing
  builder; we never add mutation to `cerberus-dom`.
- **Keeps the seam pristine.** The bridge uses only `eval`, so it is
  **engine-agnostic** — it works against any `JsEngine`, not just QuickJS. No
  native-callback escape hatch, no `Rc<RefCell<Document>>`, no engine-adapter
  lifetime gymnastics, and **no `unsafe`**.
- **Reuses our own parser for `innerHTML`** later: an `innerHTML` assignment is
  carried back as a raw string and reparsed with `parse_html` during rebuild —
  no HTML parser reimplemented in JS.
- **Dependency-free JSON.** Per ADR-0003 we bootstrap a tiny JSON emitter +
  parser in Rust rather than pull in `serde`; the JS side uses native `JSON`.

### The memory trade-off, stated plainly

Memory is priority #1, and snapshot/replay allocates a transient ~2× copy of the
DOM (in + out JSON, plus the JS model heap) **for the duration of a script-laden
render**, freed afterwards. We accept this now because: it only happens on pages
that *have* scripts (the memory gate's built-in pages have none and are
unaffected); the cost is bounded and transient; and — crucially — **the seam
does not change**, so swapping to live bindings later (if profiling demands it)
is an internal change to one crate, not a rewrite. We will measure on real,
script-heavy pages and revisit with data.

## Consequences

- **Easier:** page scripts run with no change to the engine adapter or the DOM's
  immutability; the bridge is independently testable (pure-Rust round-trip +
  engine-driven mutation tests) and reusable across engines.
- **Harder:** a DOM API surface must be maintained in JavaScript; large pages pay
  the serialization cost; the JS model is a snapshot, not a live view, so
  semantics that depend on continuous layout/observation are approximations.
- **Reversible:** by design — replace `cerberus-js-dom`'s internals with live
  bindings behind the same `run_page_scripts` entry point.

## Alternatives considered

- **Live Rust-backed DOM bindings.** The "real browser" model and best long-term
  fidelity, but it requires a mutable `Document`, shared interior mutability, a
  native-callback hole in the seam, and `unsafe`-adjacent plumbing in the engine
  adapter — rejected as the *first* increment, kept as the documented upgrade.
- **`serde`/`serde_json` for the wire format.** Convenient, but a new dependency
  the bootstrap-first policy (ADR-0003) tells us to avoid when a small
  hand-rolled JSON layer suffices.
- **Reparse the whole page after scripts.** Only works if scripts emit HTML; most
  manipulate the DOM directly, so a structural round-trip is needed regardless.

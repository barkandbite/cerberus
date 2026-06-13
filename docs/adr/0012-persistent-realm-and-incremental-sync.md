# ADR-0012: Persistent JS realm & incremental DOM sync (evolves ADR-0008)

- Status: Accepted
- Date: 2026-06-13
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

ADR-0008 bridges page scripts to our immutable `Document` by **snapshot → run →
serialize → rebuild**, entirely over the eval-only `JsEngine` seam. It was scoped
to a **single, one-shot render**: `run_page_scripts` installs the JS document
model, runs the page's `<script>`s, fires load, serializes the mutated tree back
out, and the app rebuilds a fresh Rust `Document`.

That is enough for static pages but not for **SPAs** — the project's north star
(PLAN). An interactive app wires up event handlers, defers work on timers, and
loads data after first paint; for any of that to *do* something, the **realm and
its live document model must stay alive between interactions**, and each
interaction's mutations must be read back out to drive a re-render. Today every
call to `run_page_scripts` re-installs the model (`__cerberusInstallDOM()` resets
the id counter and node index and rebuilds from the snapshot), discarding exactly
the script-created state an SPA depends on.

Two existing facts make a *small* change sufficient:

1. The QuickJS realm is **already persistent per head**: `cerberus-identity`'s
   `engine()` instantiates the engine and calls `create_realm` **once** (lazily),
   and tears it down only on head switch (ADR-0002). The speed-first prelude and
   the realm already survive across renders.
2. The JS document model **already stamps every node with a stable `__id`** and
   indexes nodes in a `byId` map (`DOM_MODEL_PRELUDE`). Within a live realm, node
   identity is therefore already stable — we simply never read it back.

## Decision

**Keep snapshot/replay and the eval-only seam unchanged; split the one-shot
`run_page_scripts` into composable persistent-realm seams, and surface the live
model's node identity to Rust.** In `cerberus-js-dom`:

- `install_page(engine, realm, document, env)` — install the env globals, the
  `DOM_MODEL_PRELUDE`, and a snapshot of `document`. **Run once per navigation.**
- `run_scripts(engine, realm, scripts)` — evaluate page scripts into the
  already-installed realm (a throwing script is not fatal).
- `fire_load(engine, realm)` — fire `DOMContentLoaded`/`load`.
- `serialize_dom(engine, realm) -> RebuiltDom` — read the realm's **current** live
  model back into a fresh Rust `Document` **without resetting or re-running
  anything**. This is the re-render seam an interaction (event, timer, async
  resolve) feeds.
- `RebuiltDom { document, id_map }` — the rebuilt document plus a
  **JS-id → `NodeId`** map, so a rendered Rust node can be correlated back to the
  live JS node it came from (needed for event dispatch / hit-testing in M12b and
  for scoping re-renders in M12c).

`run_page_scripts` is retained as the one-shot composition of the four
(`install → run → fire_load → serialize`), so the headless one-shot `render` path
and all existing callers/tests are unchanged.

The Rust `Document` stays **immutable**: each interaction still rebuilds it via
`DocumentBuilder` from the serialized live model. What persists between
interactions is the **realm and its JS model**, not the Rust `Document`.

### Why not mutate the arena in place (the deferred upgrade)

A higher-fidelity design applies a **mutation journal** from the live model
directly to a *mutable* `Document` keyed by stable id, avoiding the
per-interaction re-serialize/rebuild. We deliberately **defer** it: it relaxes the
immutable-`Document` design (ADR-0008 constraint #1) and is a larger, riskier
change, whereas the re-serialize cost is the same one we already pay per render and
is bounded by page size. We revisit **with profiling data** if the persistent
re-render loop shows up in the budget — exactly as ADR-0008 deferred live
bindings. The `serialize_dom`/`id_map` interface is forward-compatible: a journal
can replace the full serialize behind the same entry point.

## Consequences

- **Easier:** SPAs can keep handlers/timers/closures alive across interactions; the
  app reads each result back with one call; node correlation for dispatch is a map
  lookup. The engine seam does **not** change — still eval-only, still swappable.
- **Harder:** the app must install exactly once per navigation and drive the realm
  thereafter (wired in M12b); a persistent live model holds memory between
  interactions (still one model, one realm — the ≤1-engine invariant is
  unchanged), watched by the `mem-gate` budget.
- **Reversible:** by design — the deferred arena-mutation upgrade slots in behind
  `serialize_dom` with no caller change.

## Alternatives considered

- **Re-install per interaction (status quo ante).** Simple, but resets the model
  and loses all script-created state — defeats SPAs. Rejected.
- **Live Rust-backed DOM bindings.** Best fidelity, but needs a mutable `Document`,
  interior mutability, and a native-callback hole in the seam — the reasons
  ADR-0008 rejected it as the first step. Still the documented long-term option.
- **Arena mutation via a journal now.** The deferred upgrade above; rejected as the
  *first* increment on risk/immutability grounds, kept as the data-gated next step.

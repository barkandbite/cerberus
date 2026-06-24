# ADR-0060: Dynamic `<script>` injection + a wall-clock JS budget

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

After external `<script src>` loading (ADR-0059), client-rendered pages still
showed nothing: modern bundlers (webpack/Next) load their real code as **dynamic
chunks** injected at runtime — `document.createElement('script'); s.src = url;
head.appendChild(s)` — and wait for each script's `load` event. We created the
node but never fetched/ran it, so the chunk graph never advanced. Target's
homepage is exactly this shape.

Two risks came with running that much JS: a heavy/looping page could **hang** the
render, and a page pulling dozens of chunks could take tens of seconds of network
time (Target hit ~27s) — both violating the "fast" requirement.

## Decision

**Dynamic script injection.** Inserting a `<script>` with a `src` (property or
attribute) into the tree queues it on a per-realm script-load queue (mirroring the
`fetch` queue). The host's fetch-drain (`drive_fetches`) takes the queue
(`take_script_loads`), fetches each via the same client, **`eval`s the module in
the realm**, and fires its `load`/`error` event so the loader's callback chain
continues. Works alongside the data `fetch()` drain already there.

**Wall-clock JS budget (`JS_BUDGET_MS`, 1.5s).** JavaScript is now *best-effort*:
- A QuickJS **interrupt handler** with a deadline aborts any running script (initial
  run or a chunk) once the budget elapses — `JsEngine::set_deadline`/`clear_deadline`.
- `drive_fetches` also checks the deadline (the per-request *network* time isn't JS,
  so the interrupt alone wouldn't bound a many-chunk drain).
- `run_page_scripts[_with_fetch]` run the phases best-effort (`let _ =`), clear the
  deadline, then serialize — so a throw, an interrupt, or a slow page renders the
  **DOM built so far** instead of failing or hanging.

## Consequences

- **Target now renders** its real content (deal-days header, full category nav,
  deal cards with images) — chunk loading built the page; the engine's existing
  `IntersectionObserver`/timer stubs let the app code run. Four of the five
  multi-site targets now render (Wikipedia, Slickdeals, GitHub-marketing, Target).
- No render can hang; the budget caps JS. All workspace tests pass (no regression),
  clippy/bench/mem-gate (8.3 MB) green.
- **Speed cost.** JS-heavy pages are slower (Wikipedia 3.6s→8s; Target ~18s). The
  budget bounds *eval* and the fetch-drain, but a page doing **synchronous XHR** in
  its initial scripts isn't bounded by the eval-interrupt (the wait is in the host
  client). Bounding the fetch client itself, and skipping external JS on
  content-complete SSR pages (a heuristic — Wikipedia gains nothing from its
  modules), are the follow-ups for "fast".
- **Pokémon Center remains blank — and always will for any client.** It serves a
  ~1KB **anti-bot challenge** (an obfuscated detection script), never the product
  page. That's not a rendering gap; the content is never delivered.

## Alternatives considered

- **No budget:** Target hung ~27s+ — unacceptable.
- **Bound only the event loop (not eval):** a single heavy synchronous bundle would
  still hang; the engine-level interrupt is the only seam that bounds arbitrary eval.

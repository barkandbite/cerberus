# ADR-0064: Keep QuickJS; build the Web-platform layer on top (don't migrate to V8/SpiderMonkey)

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The next milestone is making modern, JS-heavy sites render — using
`pokemoncenter.com` (Imperva/Incapsula `reese84`) as the end-to-end integration
target. A directive asked us to evaluate the engine: *"If the existing engine is a
pure-Rust interpreter that cannot handle full web-compatibility workloads, recommend
embedding a production engine — `v8` (rusty_v8) or SpiderMonkey (`mozjs`)."*

A code audit (2026-06-24) establishes the actual state:

- The engine is **QuickJS** (Fabrice Bellard's complete C engine) via **`rquickjs` 0.9**
  — `crates/cerberus-js-quickjs/src/lib.rs`. It is an **interpreter (no JIT)**, one
  `Runtime` (GC heap) + N `Context` realms (one per tab/head), not `Send`
  (single-threaded, UI thread). It is **not** a from-scratch pure-Rust interpreter,
  and it **already executes obfuscated production JavaScript** — the premise of the
  conditional is false.
- The `JsEngine` trait (`crates/cerberus-js/src/lib.rs`) already abstracts the engine
  and is commented *"QuickJS today, V8 later"* — a swap is anticipated behind the seam.
- What actually blocks a site like `reese84` is **not** the engine's language
  capability; it is the **Web-platform surface**: missing `crypto.getRandomValues`/
  `crypto.subtle`, `performance.now`, `XMLHttpRequest`; canvas/WebGL/audio are
  **farbling stubs** (PRNG noise, no real backend); `localStorage` is in-memory and
  not origin-partitioned (so a cached token does not persist); `document.cookie` is a
  separate in-memory jar **not bridged** to the network cookie jar; and the event loop
  is **virtual-clock** (timing-based bot checks see non-wall-clock timing).

None of those gaps are closed by swapping the JS engine — V8 and SpiderMonkey ship a
JS VM, **not** a DOM or these Web APIs; we build that layer regardless of engine.

## Decision

**Keep QuickJS. Build the Web-platform layer on top of it.** Do not migrate to V8 or
SpiderMonkey now.

### Rationale (ranked by Cerberus's own priorities)

1. **Memory is priority #1 (64 MB budget, enforced by `mem-gate`).** Cerberus's reason
   for being is a memory-lean, multi-identity browser that drives *N* windows over one
   stack. QuickJS resident footprint is small (whole-process renders measure ~7–18 MB
   here). V8's base isolate + heap is ~10–30 MB+ *per context*, and `rusty_v8` ships a
   large prebuilt binary; SpiderMonkey via `mozjs` is comparable and embeds awkwardly.
   Either would blow the 64 MB budget by 1–2 orders of magnitude and break the N-window
   model — i.e. it would defeat the product, to fix a problem the engine isn't causing.
2. **The premise doesn't hold.** QuickJS runs the obfuscated `reese84` JS fine; the
   blocker is API surface + DOM liveness + timing + storage, all of which we build
   ourselves on either engine.
3. **"Don't write a JS engine from scratch" is already honored** — QuickJS *is* a real,
   complete engine. The work ahead is the Web platform, not a language VM.
4. **The seam is already there.** `JsEngine` abstracts the engine, so a future,
   *data-driven* swap remains open without re-architecting.

### Tradeoffs documented

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| **Keep QuickJS** | Tiny memory (fits the N-window 64 MB model); already integrated; runs obfuscated prod JS; trait seam intact | **No JIT** → heavy compute (e.g. a PoW) is slower; we build all Web APIs ourselves (true for any engine) | **Chosen** |
| Embed V8 (`rusty_v8`) | Best web-compat; JIT speed | Massive memory (kills the core value); large binary + build complexity; still must build the DOM/Web APIs | Rejected (wrong trade for *this* browser) |
| SpiderMonkey (`mozjs`) | JIT; mature | Large memory + complex embedding; same DOM/API work remains | Rejected |

### Escape hatch (data-driven, not preemptive)

The one real QuickJS weakness is the missing JIT: a sufficiently heavy proof-of-work
could exceed the JS budget. We mitigate with the existing wall-clock budget +
interrupt (ADR-0060). **If, and only if,** profiling on a real workload shows a
bounded-but-legitimate PoW that QuickJS cannot clear within an acceptable budget, we
revisit — at that point the `JsEngine` seam lets us trial V8 for *that* path behind a
feature flag, weighed explicitly against the memory cost. We do not migrate on
speculation.

## Consequences

- The Web-platform epic (DOM bindings → event loop → script semantics → networking →
  storage → real Web APIs → realm isolation) targets QuickJS and the existing
  snapshot/serialize DOM bridge (ADR-0008), extending both rather than replacing them.
- Phase 6 ("real, un-noised, internally-consistent" canvas/WebGL/audio values) is the
  largest new build and **conflicts with farbling** (Cerberus's per-head fingerprint
  noise). It will be implemented behind a **per-context compatibility mode** so
  farbling remains the privacy default; the default is an owner decision recorded in
  that phase's issue. There is **no real canvas/WebGL/audio backend today** — these
  must be built on the CPU `Framebuffer`/rasterizer (Canvas 2D, OfflineAudioContext
  DSP) and a software GL (WebGL), which the plan scopes honestly.
- Correctness is established by **offline conformance + fixture suites** (Test262 / WPT
  subsets + local fixtures) that run in CI from a datacenter environment; the live
  `pokemoncenter.com` render is a **residential-machine** confirmation (WAFs challenge
  datacenter IPs by reputation — ADR-0062 — which is not an engine defect).

# ADR-0070: Test262-style engine conformance subset

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Web-platform **Phase 0** asks to "wire up a Test262 subset and report the pass
rate" — the engine half of the correctness oracle. The milestone hinges on QuickJS
executing heavily obfuscated production JS (the reese84 bundle uses Proxy, typed
arrays, generators, BigInt, a bytecode-VM-in-JS). We needed a regression gate that
proves — and keeps proving — that the language surface those bundles rely on
actually works on our embedded engine, and that catches any regression (or a
future engine swap behind the `JsEngine` seam) that silently drops a feature.

## Decision

Add an offline, CI-friendly conformance test (`crates/cerberus-js-quickjs/tests/
test262_subset.rs`): a faithful subset of the upstream Test262 assertion harness
(`Test262Error`, `assert`, `assert.sameValue`/`notSameValue` with spec **SameValue**
semantics — `NaN` matches, `+0`/`-0` differ — and `assert.throws`) plus a **curated
set of conformance cases written in the Test262 idiom**, run **one fresh realm per
case** against the real QuickJS engine, reporting `N/M passed`.

Coverage spans the surface obfuscated bundles and sensors use: Proxy/Reflect,
accessor properties, class inheritance + `static` + **private fields/methods** and
`#x in obj`, generators + custom iterator protocol, destructuring/defaults/rest,
spread + object spread, template + tagged literals, optional chaining + nullish,
BigInt, typed arrays + `DataView` endianness, Symbols + well-known symbols
(`toPrimitive`, `toStringTag`), Map/Set/WeakMap, JSON reviver/replacer, modern
Array/String/Object methods, `Number`/`Math`, RegExp named groups + lookbehind +
`matchAll`, `let`/`const` block scope + TDZ, Promise/async *shape*, error
subclassing, labeled break/continue.

**Baseline: 27/27 pass** — QuickJS-ng runs the entire curated surface.

## Consequences

- **Engine conformance is now gated.** Any regression that drops a language feature
  fails the build with the offending case named. This is the engine half of the
  "establish correctness from conformance + fixtures" oracle, and it runs from the
  datacenter/cloud CI with **no network**.
- **Confirms the engine decision (ADR-0064).** Empirically, QuickJS already executes
  the modern-JS surface; the milestone's blockers are the Web-platform layer, not
  the VM — so no V8/SpiderMonkey swap is warranted.
- **Test-only.** No production code changed; `mem-gate`/`bench` are unaffected.
  `fmt`, `clippy -D warnings`, `cargo test --workspace` all green.

## Limitations / follow-ups (tracked under #41)

- This is a **curated subset in the Test262 style, not the upstream tc39/test262
  corpus.** It proves the listed features work; it does not exercise the corpus's
  thousands of edge cases. Vendoring a slice of the real corpus — with its
  frontmatter metadata (`includes`, `flags`, `negative`), the `$262` host object,
  and `sta.js`/`assert.js`/`propertyHelper.js` — is the faithful next step, behind
  a small runner that parses the frontmatter and honors negative/async tests.
- **Async result tests are shape-only.** A snippet asserts an `async` function
  returns a `Promise`, but not the awaited value (which resolves on a later
  microtask). Real-time/`$DONE`-style async conformance pairs with the WPT
  event-loop subset, a separate follow-up.
- The companion **WPT subset** (DOM / Fetch / Storage / Canvas) is already partly
  covered by `web_platform_fixtures.rs`; broadening it is the other half of #41.

## Alternatives considered

- **Vendor the full upstream corpus now:** the faithful option, but large (size +
  BSD attribution), needs a frontmatter-aware runner + the `$262` harness, and may
  need network to fetch — a slice on its own. The curated subset delivers a real
  gate today; corpus vendoring layers on without rework (same harness shape).
- **Put it in `cerberus-js-dom`:** the conformance target is the *engine*, not the
  DOM bridge, so it belongs in `cerberus-js-quickjs` and uses only the public
  `JsEngineFactory`/`JsEngine` seam (so it survives an engine swap unchanged).

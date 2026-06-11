# ADR-0002: JavaScript engine choice

- Status: Accepted (owner ratified **QuickJS-first**, 2026-06-10 — see Update)
- Date: 2026-06-08 (updated 2026-06-10)
- Deciders: bbarker@barkbite.org (owner), engineering

## Context

We need JavaScript execution. Writing a JS engine is out of the question — it is
one of the largest CVE surfaces in any browser and a multi-year effort — so this
is a place we lean on a dependency (consistent with ADR-0003). But memory is
priority #1, and JS engines are the single largest RAM consumer in a browser, so
the choice and the *lifecycle* matter as much as the engine itself.

Candidates:

| Engine | Rust binding | Pros | Cons |
| --- | --- | --- | --- |
| **V8** | `v8` (rusty_v8) | Fastest; best web compat; well-maintained binding (Deno); JIT | **Heaviest RAM**; large build; BSD-3 license |
| **SpiderMonkey** | `mozjs` | Strong compat; Servo's choice; battle-tested | Heavy; awkward build/embedding; **MPL-2.0** has copyleft implications |
| **QuickJS** | `rquickjs` / `quickjs` | **Tiny RAM & binary**; simple embedding; ES2020 | Interpreter (slow); weaker site compat; smaller community |

## Decision

**Ship V8 first, behind the `JsEngine` trait, and keep QuickJS as the documented
leaner swap-in.** Concretely:

1. The first engine adapter is `cerberus-js-v8` (rusty_v8), wired at **M3**.
2. **Run only the active identity's isolate.** The engine is instantiated lazily
   for the active head and **torn down on identity switch** — dropping the
   `Box<dyn JsEngine>` frees the isolate. `cerberus-identity` already enforces
   "at most one engine live" (`engines_live()` ∈ {0, 1}); this is the mechanism
   that lets three identities exist without three engines.
3. One realm/context per tab within the active head, created/destroyed on tab
   lifecycle.
4. If RSS proves unacceptable after M3's recalibration, swap in a
   `cerberus-js-quickjs` adapter implementing the same `JsEngine` trait — **no
   caller changes** (the modularity test from ADR-0001).

### The memory trade-off, stated plainly

V8 is the heavy option, and memory is our #1 priority — these are in tension. We
accept V8 *now* because it gets us off the ground with real-world web
compatibility, and the modular path defuses the tension: the cost is contained to
one isolate at a time, and the `JsEngine` seam means choosing leanness later
(QuickJS) is an adapter swap, not a rewrite. We will measure V8's marginal RSS at
M3 against the §5 budget and decide whether to switch *with data*.

### License implication

V8's BSD-3-Clause is permissive and compatible with our **Apache-2.0** choice
(ADR-0003 / PLAN §10). Choosing SpiderMonkey instead would pull MPL-2.0
file-level copyleft considerations into the build and is the main reason it is not
the default. This couples the engine decision to the license decision — both are
ratified here / in PLAN §10.

## Consequences

- **Easier:** real sites work early; mature binding; permissive licensing.
- **Harder:** larger build and RAM; V8 requires `unsafe` FFI — confined to the
  adapter crate, which opts out of the workspace `unsafe_code = "deny"` explicitly
  and is reviewed accordingly.
- **Reversible:** by design, via the trait.

## Alternatives considered

- **QuickJS first.** Tempting for the memory budget, but weaker site compat would
  hurt early usefulness; we keep it as the escape hatch rather than the default.
- **SpiderMonkey.** Comparable weight to V8 with a harder embedding story and MPL
  considerations; no decisive advantage for us.
- **No JS / our own engine.** Out of scope and a security non-starter,
  respectively.

## Update — 2026-06-10: owner ratified QuickJS-first (reverses the V8 default)

The owner chose **QuickJS now**, not V8. Memory is priority #1 and the
anti-bloat stance is explicit; V8's RAM/build cost lost to QuickJS's tiny
footprint. The original "V8-first, QuickJS-as-escape-hatch" framing above is
inverted: we ship QuickJS first and keep V8 as the documented swap-in *if*
real-world compatibility later demands it (still a pure adapter swap behind the
`JsEngine` seam — the modularity test holds either way).

Decision, as implemented at M3:

1. The first engine adapter is **`cerberus-js-quickjs`** over **`rquickjs` 0.9**
   (bundled QuickJS; compiles its vendored C — no system lib). Added to the
   ADR-0003 approved-dependency list.
2. **`JsEngine` is no longer `: Send`.** QuickJS (like a V8 isolate) is a
   single-threaded VM bound to its creating thread, and `rquickjs`'s handles are
   `!Send`. Nothing required the engine to be `Send` — it lives on the UI thread
   with the active head, and the network worker never touches it (verified). If
   JS ever moves off-thread it will be a channel-based *handle* (itself `Send`),
   not a `Send` engine.
3. **One realm = one `rquickjs::Context`** within the engine's single
   `Runtime`; created/destroyed on tab lifecycle, sharing the one GC heap. The
   "at most one engine live per active head" invariant (ADR-0001 /
   `cerberus-identity`) is unchanged.
4. **Speed-first delay neutralization** (product directive: "pure speed, ignore
   programmed delays") is installed as a JS prelude in every realm before any
   page script: `setTimeout`/`setInterval`/`requestAnimationFrame`/
   `requestIdleCallback`/`queueMicrotask` fire **immediately** (delays ignored;
   `setInterval` fires once, not forever), and `IntersectionObserver.observe`
   reports the target as intersecting **immediately** (so lazy/scroll-in content
   loads at once). `ResizeObserver`/`MutationObserver` exist as safe no-ops.

`unsafe`: the adapter needs none — `rquickjs`'s safe API suffices, so the crate
keeps the workspace `unsafe_code = "deny"` (unlike the V8 path, which would have
opted out for FFI).

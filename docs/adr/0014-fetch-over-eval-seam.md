# ADR-0014: `fetch` over the eval-only seam (bounded host-drained I/O)

- Status: Accepted
- Date: 2026-06-14
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

SPAs load data after first paint — `fetch`/XHR is the other half of the SPA
enabler (M12). But the `JsEngine` seam is deliberately **eval-only** (ADR-0002/
0008): the host can `eval(realm, source) -> JsValue`, and there are **no native
function callbacks** from JS into Rust. `fetch` is also **async** (returns a
Promise) and must perform real network I/O — through Cerberus's whole **privacy
stack** (default-deny consent, sealed per-instance cookies, the single egress
proxy, the per-origin User-Agent ladder), the product's entire point.

Two facts shape the design:

1. **Eval-only.** A JS API cannot call Rust directly. The existing DOM bridge
   already solves this by **enqueue-in-JS / drain-from-Rust** (e.g. the M12b
   event queue, `__cerberusDispatch`). `fetch` follows the same shape.
2. **Threading (from the load-path map).** The interactive browser does all
   network on a **worker thread**; page scripts run **synchronously on the UI
   thread** with the page HTML already buffered, and the privacy stack
   (`Router` + `SealedJar` + consent) lives in the worker. A *blocking* `fetch`
   on the UI thread would freeze it. The one-shot headless `render` path, by
   contrast, is fully synchronous with direct `Router` access.

## Decision

**`fetch` enqueues and returns a Promise; the Rust host drains the queue,
performs each request through the privacy stack, and resolves the Promise —
folded into the M12c bounded event loop.** No engine-seam change.

### JS side (`cerberus-js-dom` `DOM_MODEL_PRELUDE`)
- `fetch(input, init)` builds a request descriptor `{id, url, method, headers,
  body}`, pushes it on a per-realm queue, creates a `Promise`, stores its
  resolve/reject under `id`, and returns the Promise. Plus minimal `Headers` and
  a `Response` (`ok/status/statusText/url/headers`, `.text()`, `.json()`).
- Drain/settle entry points: `__cerberusTakeFetches()` (returns + clears the
  queue), `__cerberusResolveFetch(id, resp)`, `__cerberusRejectFetch(id, msg)`.

### Rust side (`cerberus-js-dom`)
- Net-agnostic types `FetchRequest`/`FetchResponse` and a **`FetchClient`** trait
  (`fetch(&FetchRequest) -> Result<FetchResponse, String>`) — so this crate never
  depends on `cerberus-net`; the host supplies the client.
- `take_fetches` / `resolve_fetch` / `reject_fetch` (eval glue, same idiom as
  `dispatch_event`), and **`drive_fetches`**: the async pump — run the bounded
  event loop (drain timers + microtasks), take the fetch queue, perform each via
  the `FetchClient`, resolve/reject, repeat until quiescent or a **cap** trips.
- `run_page_scripts_with_fetch` composes install → run → load → `drive_fetches`
  → serialize. The fetch-free `run_page_scripts` is unchanged.

### Caps (termination + memory)
`FetchBudget { max_rounds, max_requests }` bounds the async loop the way
`EventLoopBudget` bounds timers, so a script that re-fetches from every `.then`
cannot loop forever, and a response-body size cap (host-side, per the 1600px
image-cap philosophy) keeps one response from blowing the RSS budget.

### Two host execution paths (one JS/glue layer)
- **Headless / one-shot `render`:** a **synchronous** `FetchClient` calls the
  `Router` directly (already a blocking, synchronous context). `fetch` works
  end-to-end in automation.
- **Interactive browser:** requests route through the **existing network
  worker** (async, non-blocking UI): the pump dispatches queued fetches to the
  worker; results arrive in the UI poll loop; the Promise is resolved, the loop
  re-pumped, and the DOM re-rendered. The persistent realm (ADR-0012) is what
  makes resolving a Promise across a worker round-trip possible.

### Privacy (non-negotiable)
Every JS `fetch` is performed by the **same** path as page loads and `<img>`
subresources — `HttpClient` through `HttpEngine` (extended to non-GET verbs):
default-deny consent on third parties, sealed-cookie attach + `Set-Cookie`
capture, the egress proxy, and the head's UA ladder. The host owns
`Host`/`User-Agent`/`Cookie`; caller headers (e.g. `Content-Type`) are merged
but can never override them. A script cannot exfiltrate or load third-party data
outside the gate.

## Consequences

- **Easier:** SPAs can load data and re-render; `fetch` rides the existing
  privacy stack with zero new bypass; the JS/glue layer is shared between
  headless and interactive; the async loop always terminates under caps.
- **Harder:** the interactive path needs worker plumbing for arbitrary requests
  (method/headers/body + id correlation + re-render on delivery); the one-shot
  path blocks (acceptable — it already does). `HttpClient` grows a `fetch_in`
  for non-GET verbs.
- **Reversible:** `fetch` lives behind the `FetchClient` seam and the prelude
  shim; the wire entry points are isolated.

## Deferred (documented, not in this increment)

- **`XMLHttpRequest`** (a thin shim over the same queue), **streaming**/binary
  bodies (`ArrayBuffer`/`Blob` — v1 bodies are UTF-8 text), **`AbortController`**
  cancellation, and request/response **caching** of JS fetches. The
  `FetchClient`/queue interface is forward-compatible with all of them.

## Alternatives considered

- **Native `fetch` binding (host function exposed to JS).** Direct and async-
  friendly, but punches a callback hole through the eval-only seam (needs
  `unsafe`/engine-specific glue) — the very thing ADR-0008 avoided. Rejected.
- **Synchronous `fetch` on the UI thread for the interactive browser.** Simplest,
  but freezes the UI for the request duration and needs the privacy stack on the
  UI thread (it lives in the worker). Kept only for the already-synchronous
  headless path. Rejected as the interactive design.
- **A full async runtime / reactor in the engine.** Faithful but heavy, against
  the single-threaded, memory-first model. Rejected.

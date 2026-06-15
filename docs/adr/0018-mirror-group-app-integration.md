# ADR-0018: Mirror-group app integration (real `PageSource` over the sync stack)

- Status: Accepted
- Date: 2026-06-15
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0006 (per-instance sessions), ADR-0008 (DOM bridge), ADR-0012
  (persistent realm), ADR-0017 (mirror groups), #6

## Context

ADR-0017 delivered `cerberus-mirror` as a tested model with a [`PageSource`]
seam, exercised only by a test `FakeSource`. To make mirror groups usable by the
real browser, the model must load *real* pages per identity through the app's
privacy stack (sealed cookie jar, consent, proxy, honest-UA ladder).

The interactive browser loads pages **asynchronously** (a network worker calls
back into `handle_page`). But the one-shot `render()` path already loads
**synchronously** — `network_client(roots, jar, proxy).get_in(url, ctx)` for
http(s), `BuiltinHttpClient` for `cerberus:` pages, then `parse_html`. Crucially,
mirror **catch-up is itself synchronous** (install → run → serialize per
instance, on the UI thread holding the single engine), so reusing the synchronous
load path avoids inventing an async bridge.

## Decision

Add an **app-layer adapter** — `cerberus-app` gains a `cerberus-mirror`
dependency — implementing `PageSource` over the existing synchronous stack:

- **builtin** `cerberus:` URLs via `BuiltinHttpClient`;
- **http(s)** via `network_client(system_roots, Some(jar), proxy)` and
  `get_in(&url, &FetchContext { instance, kind: Navigation })`, returning
  `parse_html(&body)`.

The sealed jar / consent / storage are **shared but partitioned by
`InstanceId`** through the `FetchContext`, so each instance fetches under *its
own* sealed session (cookies, identity, UA) — the multi-account property — while
the group runs the **single** engine (a realm per focused instance, ≤1 live).

A builder constructs a `MirrorGroup` from the `HeadManager`'s heads: each head's
`(instance, label)` becomes a member. **Engine ownership:** the group owns the
one engine for the duration of a mirror session; entering mirror mode tears down
the single-window engine first, so the **global ≤1-live-engine invariant
holds** (PLAN §1).

**Scope/limits (v1, documented):**
- JS `fetch()` *during* a mirror page load is deferred — the group uses
  `run_scripts` (not the fetch-aware `run_page_scripts_with_fetch`). The privacy
  stack still gates the top-level navigation of every instance; page scripts that
  fetch at load simply don't on followers yet. A fetch-aware catch-up is a later
  refinement behind the same seam.
- Profiles still come from the existing heads here; arbitrary-N profiles and the
  driving UI land in later ADRs (autofill + the multi-window shell).

## Consequences

- **Easier:** mirror groups now load real per-identity pages through the privacy
  stack — the feature is reachable from the app, not just tests.
- **Harder:** the app owns engine-mode arbitration (single-window vs. mirror);
  JS-fetch-on-load is deferred.
- **Testable here:** headlessly over **builtin pages** (no network) — drive a
  group, catch a follower up, assert convergence and `live_realms ≤ 1`. Real
  per-identity *divergence* stays covered by the mirror crate's `FakeSource`
  tests; live network is validated manually/headed.

## Alternatives considered

- **Async bridge (block the catch-up on the network worker).** Unnecessary — the
  synchronous load path already exists and catch-up is synchronous. Rejected as
  needless complexity.
- **Group reuses `HeadManager`'s engine handle directly.** Tighter coupling and
  awkward ownership; a fresh group-owned engine is cleaner and still ≤1 live.
- **Put the adapter in `cerberus-mirror`.** Would drag the whole network/storage
  stack into the pure model crate. Rejected; the adapter belongs in the
  composition root.

[`PageSource`]: ../../crates/cerberus-mirror/src/source.rs

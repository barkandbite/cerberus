# Idea: pass the reese84 / Imperva bot challenge, per sealed window

> **Status: plan / not yet started.** Captured 2026-07-03. A product +
> architecture plan for the [#40 web-platform epic](../../PLAN.md), *not* a
> decision (no ADR yet; each phase earns its own ADR when it lands). The litmus
> test for the epic — "render more than a blank screen or an error page on
> `pokemoncenter.com`" — currently **fails**; this document says exactly why and
> the finishable path to fixing it.

## Where we actually are (measured)

`cerberus-app render --url https://www.pokemoncenter.com/ --dump-text` returns
**HTTP 200 with empty page text** — a blank screen. The 200 is not the store; it
is Imperva/Incapsula's bot-challenge interstitial (reese84 is Imperva's product):

```html
<script src="/vice-come-…" async></script>              <!-- obfuscated sensor JS -->
<iframe id="main-iframe" src="/_Incapsula_Resource?SWUDNSAI=31&xinfo=…&incident_id=…">
  Request unsuccessful. Incapsula incident ID: 1543000130026243706-…
</iframe>
```

We execute the one script and set two cookies, then stop. The challenge never
completes, so the real HTML is never served, so nothing paints.

## The handshake we must reproduce

```
GET /                                  → interstitial (what we get today)
  1. sensor JS fingerprints the browser via real Web APIs
  2. sensor POSTs its computed payload to /_Incapsula_Resource   (XMLHttpRequest)
  3. server replies 200 Set-Cookie: reese84=… ; visid_incap_…=…
  4. page reloads (location.reload / meta refresh / script GET)
  5. GET / carries the cookies                                   → real HTML → paints
```

Every link after step 1 is currently broken. Each is a specific, bounded gap in
the browser plumbing — not a missing engine.

## Why the plumbing is closer than the blank screen implies

Two load-bearing pieces already exist:

- **Async network from script.** `fetch()` runs on a
  JS-queue → `pump_fetches` → `FetchContext{instance, first_party}` → net worker
  → `resolve_fetch`/`reject_fetch` back into the realm → re-run the event loop
  (ADR-0013/0014). XHR is a *different JS API over the same transport*.
- **Per-instance sealed cookies.** `SealedJar` (a `CookieJar`) already attaches
  cookies per redirect hop, captures `Set-Cookie`, and gates both by consent and
  first-party — keyed by `InstanceId`. Every window already has its own jar,
  farbling seed, and `RealmId(head.id)`.

So the work is *wiring the missing links through machinery we already own*, not
building a network stack or an identity model from scratch.

## The gaps → the phases

Each phase is a shippable PR arc with the handshake as its acceptance north star.
Issue numbers reference the [#40] epic's phases.

### Phase A — `XMLHttpRequest` over the existing fetch rails (#45, part 1)
The sensor POSTs via XHR. We have zero XHR support.
- Add an `XMLHttpRequest` shim to `DOM_MODEL_PRELUDE` that **enqueues onto the
  existing `__cerberusFetchQueue`** with a `kind: "xhr"` tag (method, url,
  headers, body, sync/async, a JS callback id).
- `pump_fetches`/`resolve_fetch` deliver the response to
  `onreadystatechange`/`onload`/`responseText` instead of resolving a Promise.
  The transport, consent gate, and sealed jar are reused verbatim.
- **Low-risk:** no new network path; XHR inherits the instance's cookies and
  consent for free.

### Phase B — bridge `document.cookie` ↔ the sealed jar (#45, part 2)
Today `document.cookie` is an in-memory `__cookie` string: reads are seeded once
at install, **writes vanish**. The challenge token can arrive via `Set-Cookie`
(covered once XHR captures response headers through `SealedJar::set_cookie`) *or*
via `document.cookie = …` from script.
- **Setter:** emit a capture record (mirroring the fetch queue) drained after the
  event loop and fed to `SealedJar::set_cookie(instance, url, first_party, value)`
  — same per-window store, same consent gate as a network cookie.
- **Getter:** reflect the live jar for the instance on each event-loop turn, not
  just at install.
- This is the correct general web-platform fix, not a reese84 special-case.

### Phase C — execute the challenge sub-document (`/_Incapsula_Resource`)
The interstitial drives the challenge through a same-origin `<iframe>`. We do not
fetch or run sub-documents.
- Minimal version: detect an interstitial (heuristic — single async script + one
  same-origin iframe + empty body), fetch the iframe `src` as a subresource **in
  the same instance/realm**, run its scripts through the existing pipeline, and
  let its XHR/`document.cookie` effects flow through Phases A/B.
- A scoped stepping-stone toward real nested browsing contexts (#48); no iframe
  layout commitment yet.

### Phase D — cookie-gated reload → real content (#22 / #42)
After the token is set the page reloads itself; JS navigation is inert and
re-render is manual today.
- `location.assign/replace/reload` + `<meta http-equiv=refresh>` → `begin_load`
  (which re-attaches the sealed jar per hop, so the token rides the retry).
- Fire mutation-driven re-render (the `reconcile_dispatched` path exists; #42/#22
  make it fire on the challenge's DOM swap) so the post-challenge HTML paints.

### Phase E — fingerprint surface coverage (#47)
The sensor reads dozens of APIs; a missing property (`undefined`) makes it bail.
Guessing is wasteful, so **start with a scoping spike**: instrument the realm to
log every `navigator.*` / `screen.*` / canvas / WebGL / `Date` / `performance`
property the live sensor touches, diff against what we expose, and fill the gaps
with plausible, **farbled-per-head** values. M6 farbling already gives seeded
canvas/audio/WebGL noise — the work is *coverage*, not net-new fingerprinting.

## The invariant that must not break: one solve *per window*

This is the part that makes it "natural for each window" instead of a hack.

**Do not solve the challenge once and share the cookie.** That would hand N
sealed identities the same reese84 token and a byte-identical fingerprint,
collapsing the entire privacy model (§1 of PLAN, ADR-0006/0017).

- Each mirror instance already owns its **own `SealedJar` + farbling seed +
  `RealmId`**. The challenge runs **inside each instance's realm**, producing an
  **uncorrelated** payload and its **own** token in that instance's sealed store.
- Respect the **≤1-live-engine budget**: the master solves eagerly; mirrors solve
  **lazily on focus/catch-up** (the challenge JS becomes part of the catch-up
  replay), and the resulting token is cached in the sealed jar so a converged
  instance re-focuses without re-solving (reuses the converged-snapshot skip,
  ADR-0025/0026).
- Net effect: N windows → N independent solves → N uncorrelated identities. The
  challenge handling *is* the identity model, not bolted onto it.

## Acceptance — and an honest caveat

- **Deterministic gate (the real definition of "done"):** a local
  **reese84-shaped fixture** — interstitial → sensor script → an XHR sensor
  endpoint that validates the payload shape and sets a cookie → cookie-gated real
  page. CI asserts the full handshake renders non-empty content, and that **two
  mirror instances receive different challenge cookies** (the per-window
  invariant). We test our plumbing, not a live third party.
- **Live check (aspirational, not a contract):** `pokemoncenter.com --dump-text`
  renders product text. reese84 is an adversarial, continuously-updated
  detector; passing the *live* endpoint is not a stable or guaranteeable target
  (production headless browsers lose this race routinely). The engineering
  deliverable is a **correct, complete challenge-handshake browser** (Phases
  A–E), each piece independently valuable for every JS-heavy site, with live
  pokemoncenter as the integration aspiration.

## Sequencing

```
A (XHR) → B (cookie bridge)
   └─ checkpoint: run the fixture; observe how far the live sensor gets
      → C (iframe sub-document) + D (reload / re-render)
         → E (fingerprint coverage, iterated against the spike)
```

A + B are worth landing regardless of reese84 — they are the top of #45 and
unblock most modern JS sites. A workspace **version bump is justified when the
fixture handshake passes end-to-end** (the first genuinely user-facing
web-platform milestone since the M12+ arc).

## Related work already noted

- #137 (inline-whitespace spurious spaces) touches the same text path the
  re-render work leans on — confirm its root-cause before Phase D.
- #47's privacy decision (compatibility-mode Web APIs vs. anti-fingerprinting)
  must be made explicitly, per the epic's ⚠️ flag, before shipping Phase E.

## Prerequisite: audit the recent fidelity/perf PRs first

Before building Phases A–E on top of them, verify the ~27 fidelity/perf/
robustness PRs that landed just prior. Triage by blast radius rather than
reviewing all uniformly:

- **Low-risk — spot-check only:** the UA-stylesheet one-liners (list markers,
  `del/s/ins/u/mark`, `center/nobr`, `dfn/address`, `details/summary`,
  `figure`, `dd`, `a[href]`). Confirm rule + test; done.
- **Medium/high-risk — adversarial re-read + added property/fuzz tests:** the
  changes that touched **shared types or hot paths** —
  - the `WhiteSpace` and `LineHeight` enum refactors (touched `cerberus-style`
    and every consumer): re-check the inherit/initial split and layout
    resolution.
  - form-control numbering over the styled tree: correctness-critical for
    submission; fuzz `display:none` / `type=hidden` / nested-form permutations.
  - the HTTP-cache nested map and the in-place `find_ci` scan: edge cases (empty
    needle, unbalanced tags, `len`/`is_empty` accounting).
  - the `&nbsp;` / ASCII-whitespace change and the CSS splitter unification:
    malformed-input fuzzing.
- **Method:** run `/code-review` on each merged diff by SHA, re-run the full
  suite twice + all three gates (mem-gate / bench / mirror-bench), and add the
  tests the triage flags. Anything that surfaces gets its own fix PR.


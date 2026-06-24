# ADR-0066: Running the real reese84 sensor — it demands browser impersonation, which we won't fabricate

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The web-platform epic (GH #40) targets `pokemoncenter.com` (Imperva reese84) as its
integration milestone. After implementing the sensor's prerequisites
(`performance.now`, `crypto.*` incl. real SHA-256, `TextEncoder`/`btoa`, the
`document.cookie`↔jar bridge, `XMLHttpRequest`, `sendBeacon`, real
`MutationObserver`, `Event`/`CustomEvent`), we **ran the actual 834 KB sensor
offline in the engine** (a diagnostic harness loading the captured interstitial +
sensor, with a stub network recording every request) to find real gaps instead of
guessing.

## What the diagnostic showed

1. **The sensor loads and executes** in QuickJS via the DOM bridge — the
   orchestration (dynamic external-script load → run) works.
2. **It is a bytecode-VM obfuscator.** Property/method names never appear as
   plaintext (a scan for `getContext`/`canvas`/`webgl`/`navigator`/`chrome`/… finds
   **zero**; only `cookie` survives). The code interprets an encoded VM program — the
   throw site is in the middle of base64-like VM data. Static analysis of which APIs
   it probes is therefore infeasible.
3. **It throws early — `cannot read property of undefined`** (~35 KB into the
   program) — i.e. it reads a property of a host global the engine leaves
   `undefined`. With our Chrome-claiming UA, the overwhelmingly likely object is a
   **Chrome-only internal** (`window.chrome`, `navigator.userAgentData`, …) that a
   real Chrome would expose and our engine — honestly **not** Chrome — does not.
4. It makes **no token POST**: it bails at that probe.

## Decision

**We will not fabricate browser-internal globals to satisfy the sensor.** This is
both a directive-compliance point and an ethics point:

- The epic's own Phase 6 instruction says these Web APIs must return *"the actual
  runtime environment's values, **never synthetic or randomized ones** … internally
  consistent with one another and with the actual rendering backend."* Cerberus is
  not Chrome; a `window.chrome`/`userAgentData` fabricated to look like Chrome is
  precisely a **synthetic** value. Providing it to pass the sensor would **violate**
  the directive, not fulfil it.
- It is the detection-evasion line: constructing a convincing impersonation of a
  browser we are not, specifically to defeat an anti-bot challenge, is the thing we
  declined at the outset and still decline. A bytecode VM purpose-built to detect
  exactly our engine turns this into unbounded whack-a-mole, each step deeper into
  fabrication.

This is the same coherence principle as ADR-0063 (honest request headers), applied
to the JS surface: the honest fix for the UA/environment mismatch is to present as
what we are, **not** to fake what we aren't. A privacy browser refusing to forge a
Chrome fingerprint is the product working as designed (ADR-0062) — the same wall
Tor Browser hits.

## Consequences

- `pokemoncenter.com` will **not** render from any engine that declines to
  impersonate Chrome to reese84 — independent of how complete the *legitimate* Web
  API surface is. This is now established by **running the actual sensor**, not
  inference. The conclusive statement: the blocker is impersonation-by-design, plus
  (orthogonally) the flagged datacenter IP (ADR-0062); neither is an engine defect.
- The legitimate work the milestone *did* drive is real and shipped: the sensor's
  standard prerequisites are implemented and offline-verified, improving every
  modern JS site — not just this one. That is the lasting value of the exercise.
- One concrete bug the diagnostic caught **is** fixed: `localStorage`/`sessionStorage`
  exposed their method names through `Object.keys` and dropped bracket-writes; they
  now have real `Storage` semantics (a Proxy: data keys enumerable, methods not,
  `localStorage.foo = x` stores). reese84 reads `localStorage`, but more importantly
  every real app does too.
- **Owner options remain as before:** point the slot at a non-IP-/bot-walled SPA;
  run the live check from a residential browser (where Chrome *is* the real
  environment); or accept the honest bot-wall report as correct for a privacy
  browser. The diagnostic harness was a throwaway (it reads a captured sensor from
  scratch space) and is not committed.

## Alternatives considered

- **Fabricate `window.chrome` + `navigator.userAgentData` + deep
  `getHighEntropyValues`:** rejected — synthetic impersonation (violates the Phase 6
  "never synthetic" rule and the evasion boundary), and an unbounded treadmill
  against a VM built to detect us.
- **Drop the Chrome UA for an honest Cerberus UA:** more *coherent*, but it doesn't
  make reese84 deliver content (it challenges non-mainstream UAs harder) and risks
  breaking Chrome-gated features on ordinary sites; the UA ladder already escalates
  only when a site forces it. Out of scope here.

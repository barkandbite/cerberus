# ADR-0063: Browser-fidelity request headers (`Accept` + `Sec-Fetch-*`)

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Every request the engine sent carried `Accept: */*` — the default of an automation
client (curl/fetch), **not** a browser. A real browser varies `Accept` by
destination (an HTML navigation negotiates `text/html,application/xhtml+xml,…`; an
image `image/avif,image/webp,…`) and, on a top-level navigation, sends the
`Sec-Fetch-*` metadata set and `Upgrade-Insecure-Requests: 1`. Two costs:

1. **Content negotiation is wrong.** Servers that branch on `Accept` (AMP vs full
   HTML, `image/webp` vs `image/jpeg`, `application/json` vs `text/html`) can serve
   the wrong representation to a client claiming to accept anything.
2. **It is an anomaly signal.** `Accept: */*` on a document navigation, with no
   `Sec-Fetch-*`, is one of the cheapest "not a browser" tells. Sites behind bot
   management (this came up testing `pokemoncenter.com`, behind Imperva reese84) and
   ordinary CDNs both look at it. The header set should match the browser the
   User-Agent already claims to be — coherence, not deception.

## Decision

The `Accept` value is now **caller-chosen per request kind** (`http1::Request.accept`),
and the engine sets a browser-correct value from the `FetchContext`:

- **Navigation** → `text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8`, plus `Sec-Fetch-Dest: document`, `Sec-Fetch-Mode: navigate`, `Sec-Fetch-Site: none`, `Sec-Fetch-User: ?1`, and `Upgrade-Insecure-Requests: 1` (https).
- **Subresource** → `*/*` (the per-destination Accept — `text/css`, `image/*` — isn't
  known at this layer; `*/*` on a subresource is far less anomalous than on a
  navigation, and is left as a future refinement).
- **DoH** → `application/dns-message` (moved from an extra header to the `accept`
  field so it isn't duplicated).

**Privacy invariant preserved.** Every value is **uniform across all users** — no
per-user entropy, exactly like the existing uniform `Accept-Language`. This is the
uniformity model (all Cerberus heads send identical request metadata), not per-user
noise. The header set stays coherent with the script-visible identity
(`navigator.language`, the claimed UA).

## Consequences

- Content-negotiating origins get a correct, browser-shaped request; navigations no
  longer announce themselves as automation via `Accept: */*`.
- One genuine "not a browser" tell is removed on **every** site, which is the
  legitimate, general lesson the Pokémon Center / Imperva case surfaced on the
  *request* side (distinct from the rendering side).
- It does **not**, by itself, pass a bot wall that keys on IP reputation + a
  fingerprint proof-of-work: `pokemoncenter.com` still returns the reese84 challenge
  to this datacenter egress IP (correctly reported as a bot wall, ADR-0062). Header
  fidelity is necessary-not-sufficient, and that is the honest boundary — Cerberus
  fixes the request, and declines to forge the fingerprint.
- Gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace` (incl. the
  updated http1 wire test asserting the caller `Accept` is written verbatim),
  mem-gate, bench. No new dependencies.

## Alternatives considered

- **Keep `Accept: */*` everywhere:** simplest, but wrong content negotiation and a
  needless anomaly tell.
- **Spoof a full per-destination `Accept` + `sec-ch-ua` client-hints to impersonate a
  specific Chrome build:** rejected — that crosses from honest browser metadata into
  fingerprint forgery (and `sec-ch-ua` would have to match a real Chromium version we
  aren't). Uniform, coherent, honest headers only.
- **Thread the precise subresource destination (style/script/image) down to set its
  exact `Accept`:** worthwhile but larger; deferred. `*/*` is a safe interim.

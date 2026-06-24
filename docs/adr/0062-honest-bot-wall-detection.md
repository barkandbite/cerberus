# ADR-0062: Honest bot-wall detection (decline the fingerprint, report the block)

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The multi-site goal includes `pokemoncenter.com/category/plush`. That origin never
serves the product page to this client: it returns a ~1 KB **Imperva/Incapsula**
interstitial — an obfuscated `reese84` sensor (`<script src="/vice-come-…">`, ~838 KB
of fingerprinting + proof-of-work) plus an `_Incapsula_Resource` iframe reading
"Request unsuccessful. Incapsula incident ID …". The egress datacenter IP is flagged
at the reputation layer (`cip=160.79.106.x` in the incident), so the response is a
**hard denial**, not a passable cookie challenge.

This is categorically different from the other four targets (Wikipedia, Slickdeals,
GitHub, Target all deliver content and render). It is **not a rendering gap**: there
is no page to render. The only way to obtain the content is to run the reese84 sensor
and return a passing **browser fingerprint** (real canvas/WebGL/audio surface) plus a
solved proof-of-work — and even then the flagged IP would likely still be denied.

Two things were wrong with the prior behavior:

1. The engine treated the 1 KB challenge as an ordinary near-empty SPA shell
   (`ssr_chars < 800`) and tried to **fetch and execute the 838 KB sensor** — pure
   latency (and, on a flagged IP, pointless) for a payload whose entire purpose is to
   fingerprint the client.
2. It then rendered the challenge stub as if it were the page, with no signal that the
   content had been withheld at the network edge.

## Decision

**Detect the wall; report it; decline the sensor.** Recognize bot-management
interstitials from the response body with high-precision, vendor-unique signatures
(`detect_bot_wall`), covering Imperva/Incapsula (reese84), Cloudflare managed
challenge, Akamai Bot Manager, PerimeterX/HUMAN, and DataDome. On a match:

- **Skip external-script execution** — do not fetch or run the vendor's fingerprinting
  sensor (`needs_js = bot_wall.is_none() && …`). Inline scripts (harmless, e.g.
  `distil_referrer`) still run.
- **Surface the block honestly** — `RenderOutcome.bot_wall` carries the vendor +
  reason; the `render` CLI prints a `⚠ bot wall` notice making clear the content was
  withheld at the network edge and that Cerberus *declines to run the fingerprinting
  sensor as a matter of privacy posture*.

Signatures scan only a bounded, UTF-8-boundary-safe head slice (16 KiB), so a large
real page costs ~nothing and is never misreported (verified: Target, which rides
Akamai, renders its full content with no wall — its `ak_bmsc` marker lives in a
`Set-Cookie` header, not the HTML body).

## Why this is the *right* behavior, not a limitation

Cerberus is a **privacy** browser. Its design refuses exactly what a bot wall demands:
per-head farbling and the deliberate absence of a stable canvas/WebGL/audio
fingerprinting surface (the differentiator the whole project is built around). A site
that withholds content until the client submits an invasive, stable fingerprint is in
direct tension with that model — the same wall blocks Tor Browser and other
hardened/anti-fingerprint clients. **Declining the fingerprint and reporting the wall
is the privacy posture working as designed**, not an engine deficiency.

Passing the wall would require *forging* a convincing fingerprint (no real GPU/canvas
surface here) and/or replaying tokens — i.e. building detection-evasion tooling to
defeat a site's access controls. That was explicitly **declined** on both ethical
grounds (circumventing an access control so an automated agent can scrape) and
feasibility (no fingerprint surface; flagged IP; PoW cost). Faithfully running the
sensor wouldn't pass either — empty fingerprint reads are themselves bot-tells.

## Consequences

- Pokémon Center now renders **fast and honest**: ~2.8 s (network + realm warm-up),
  17.7 MB, with a clear `⚠ bot wall: Imperva/Incapsula …` notice — instead of
  silently executing 838 KB of hostile code and painting a stub. The product page is
  still not shown **because it is never delivered**; that is reported, not hidden.
- The four content-delivering targets are unaffected (detection returns `None` on real
  pages by construction; the `needs_js` gate only changes when a wall is matched).
  Target verified end-to-end (69 scripts, real deal content, no wall).
- Generalizes beyond this one URL: any site behind these five vendors now degrades to
  an honest, fast block notice rather than a hang or a blank.
- All gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace`
  (72 app tests incl. three new wall tests), mem-gate, bench.

## The unsatisfiable part of the goal (owner action required)

The goal asks that `pokemoncenter.com/category/plush` *render*. From this environment
that is **impossible by legitimate means** — the content is withheld by a WAF on a
flagged egress IP, and Cerberus will not build fingerprint-forgery/token-replay to get
around it. Resolving the literal URL requires an **owner decision**, any of which the
engine already supports:

- point that test slot at an equivalent, non-IP-walled product-grid SPA (rendered on
  request), or
- run the build from a residential network / unflagged egress (the same binary would
  then either render it or, if still challenged, run the sensor as a real browser
  would — within the privacy farbling already in place), or
- accept "honest bot-wall report" as the correct outcome for a privacy browser hitting
  an anti-fingerprinting wall, and treat the other four as the rendering bar.

## Alternatives considered

- **Build reese84/Incapsula evasion (forge fingerprint, solve PoW, replay tokens):**
  rejected — detection-evasion against a site's access controls; also infeasible here
  (no real fingerprint surface, flagged IP).
- **Keep running the sensor and render whatever results:** rejected — executes 838 KB
  of hostile fingerprinting code for no content gain, and still paints a stub.
- **Per-site allow/deny list:** rejected — brittle; the vendor-signature signal
  generalizes across sites and is self-documenting.

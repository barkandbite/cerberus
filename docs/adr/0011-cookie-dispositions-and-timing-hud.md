# ADR-0011: Per-cookie dispositions & the Rust-side timing HUD

- Status: Accepted
- Date: 2026-06-12
- Deciders: owner, engineering

## Context

Two post-v1 features were requested: a *fully transparent, user-customizable*
cookie experience (decide per cookie what is kept, for how long, or dropped),
and a *nanosecond timing overlay* that is persistent and readable (page load,
server response, function/form times) without bouncing around.

## Decision

### Cookie dispositions (M10)

Every accepted cookie carries a **disposition**, resolved per cookie from a
three-tier policy (per-`(site, name)` override → per-site default → global
default) and applied *under* the consent gate (consent decides third-party
visibility; disposition decides lifetime/persistence):

- **Allow** — honor the site's own lifetime (Max-Age/Expires; session if none).
- **Session** — usable this run only; never written to disk; gone on close.
- **Timed(secs)** — persisted, but the lifetime is the *user's*: expiry =
  now + secs, overriding whatever the site asked for.
- **Block** — never stored, never sent.
- **Allow-once** — sent on exactly the next matching request, then dropped.

The policy lives in `cerberus-storage` (`CookiePolicy`, serialized to the
human-auditable `cookies.policy`). `ScopedCookie` carries the resolved
disposition and an Allow-once send budget; the on-disk cookie record gains a
9th disposition field, and a legacy 8-field record loads as `Allow` (existing
profiles keep working — no format-version bump). Session/Allow-once never
touch disk. `Set-Cookie` `Expires` is now parsed (a no-dep HTTP-date parser),
closing the M9 gap so `Allow` is faithful and `Timed` is a true override.
Transparency is delivered by the cookie inspector overlay (`CookieManager` in
`cerberus-ui`) and a headless `cookies` subcommand.

Global default stays **Allow** (preserves prior behavior; the consent gate
still governs third-party). Tightening the global/per-site default to
`Session` or `Block` gives a stricter posture.

### Timing HUD (M11)

All timing is measured **Rust-side**, at the boundaries the browser already
drives (the fetch call, `run_scripts`, `style`, `layout`+`paint`). It is
**never exposed to page JS**: pages have no clock (`Date.now`/`performance.now`
are absent and the speed-first prelude fires timers immediately, ADR-0007), so
adding a page-facing high-res clock would be a new fingerprint/timing-attack
surface. Measuring on our side avoids that entirely.

Rows live in a stable-ordered table keyed by label, so values **update in
place** — the on-screen HUD never reorders or bounces, which is the
readability requirement. Subresources are summed into one row so image-heavy
pages don't flood it. The HUD (`PerfHud` in `cerberus-ui`) is a fixed
top-right panel; `--timers` / `RenderOutcome.timings` expose the same data
headlessly.

Function/handler/`fetch` timings (e.g. "add to cart") arrive with M12's real
event dispatch + fetch; until then those rows are simply absent.

## Consequences

- **Easier:** users see and control exactly what persists; timings are honest
  (our work, not a clock the page can game) and add no fingerprint surface.
- **Harder:** one more policy tier to resolve and persist; the cookie record
  format grew a field (handled by accepting both lengths).
- The page-facing `performance.now()` some SPAs expect is still absent; when
  M12 adds it, it must be coarsened/farbled (noted for that milestone).

# ADR-0027: DNS resilience — multi-DoH with a system fallback

- Status: Accepted
- Date: 2026-06-17
- Deciders: benz.benbarker@gmail.com (owner, approved 2026-06-17), engineering
- Amends: ADR-0006 (which chose DoH-only, a single resolver, no plaintext fallback)

## Context

ADR-0006 made resolution **DoH-only against a single hardcoded resolver (Quad9)
with no fallback**, for privacy. In the field this proved brittle: on a network
that blocks or mangles the Quad9 DoH connection (observed on a Windows machine: a
middlebox answering our DoH POST with HTTP 505), resolution fails — and because
DNS is the very first step of every navigation, *every* page fails with no
recovery. A browser that cannot resolve any name is unusable on that network.

## Decision

Resolution becomes an ordered **fallback chain**:

1. **Quad9** DoH (9.9.9.9, `dns.quad9.net`) — unchanged default, tried first.
2. **Cloudflare** DoH (1.1.1.1, `cloudflare-dns.com`).
3. **Google** DoH (8.8.8.8, `dns.google`).
4. **System resolver** (`getaddrinfo`) — last resort only.

The first resolver to return records wins. Encrypted (DoH) resolvers are tried
first and in order, so privacy is preserved whenever any of them is reachable.
The OS resolver — the only path that exposes lookups to the local network — runs
**only if all three DoH providers are unreachable**, so the browser still works
on networks that block public DoH, at the cost of one plaintext lookup in that
degraded case.

### Module layout

- `cerberus-dns-doh` gains `DohResolver::cloudflare` / `::google` beside `::quad9`.
- `cerberus-net` gains two dependency-free resolvers behind the existing
  `DnsResolver` trait: `SystemResolver` (OS `getaddrinfo`) and `FallbackResolver`
  (the ordered chain). **No new external dependencies.**
- The app composes `FallbackResolver::new([quad9, cloudflare, google, system])`.

### Error reporting

A DNS failure is no longer misreported as "this site doesn't support HTTPS" (the
https-first prompt). Switching to plaintext http cannot fix a name that never
resolved, so DNS failures are surfaced with their real cause instead of offering
the misleading insecure prompt.

## Consequences

- **Easier:** the browser keeps working through DoH outages or blocks; users on
  locked-down networks can browse; errors name the real cause.
- **Costs:** a small privacy reduction *only* in the all-DoH-blocked case (one OS
  lookup). A blocked first endpoint adds latency before the chain falls through
  (bounded by the DoH connect timeout × attempts per endpoint); results are cached
  per instance, so this is a first-resolution cost only. Reducing per-endpoint
  attempts when chained is a possible future tuning.

## Alternatives considered

- **Keep strict DoH-only:** rejected — leaves the browser dead on networks that
  block public DoH, unacceptable for a usable browser.
- **System DNS by default:** rejected — weakest privacy default; DoH-first keeps
  lookups encrypted whenever possible.
- **Bundling DoT or a recursive resolver:** deferred — the DoH chain plus an OS
  fallback covers the observed failure with no new dependencies.

# ADR-0048: "Stop bugging me" — global allow-all consent switch + per-site exemptions

- Status: Accepted
- Date: 2026-06-22
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Cerberus defaults to **deny third-party** access (its defining privacy property).
On real sites that blocks the cross-site CSS/images many pages depend on — a
Wikipedia article had **55 subresources blocked**, rendering it nearly unstyled —
and, headed, it prompts repeatedly. Users wanted a way to say "just let
everything through" without abandoning the privacy-first default for everyone.

## Decision

Add a **global allow-all switch** plus **per-site exemptions** to
`DefaultDenyPolicy`, with the strict default-deny **kept as the out-of-the-box
default** (owner decision).

- `allow_all: bool` — the "stop bugging me" switch (default **off**). When on,
  third-party access is permitted with no prompt.
- `exempt_sites: Vec<String>` — first-party site keys whose behavior **inverts**
  the global switch: `site_allows_all(fp) = allow_all XOR is_exempt(fp)`. So you
  can *allow everything except* one site (toggle on, exempt the sketchy one), or
  *stay strict except* one site (leave off, exempt the trusted one). One switch +
  occasional exceptions — no per-resource management.
- Precedence in `evaluate`: first-party → allow; an explicit standing rule
  (banner Allow/Deny) → its verdict; else the allow-all/exempt default; else the
  existing prompt(headed)/deny(headless).
- **Persisted** by extending the existing human-auditable consent-rules file with
  `mode all` and `exempt <site>` lines (round-tripped by `serialize_rules` /
  `load_rules`), so it survives restarts via the same path standing rules use.

### Surfaces
- **Settings overlay** (`BrowserApp`): an "allow all sites (stop bugging me)"
  toggle row, and an "exempt this site" row showing the current site's *effective*
  state (`allowed`/`strict`). Toggling either persists and immediately re-fetches
  now-permitted images/stylesheets so the page reflows without a reload.
- **Headless**: a `render --allow-all` flag for one-shot rendering with everything
  permitted (also handy for testing/automation).

## Consequences

- The privacy-first default is unchanged for new users; one click flips to
  convenience. With allow-all on, the Wikipedia article's 55 blocked subresources
  drop to **0** and its images load. (A separate layout bug — article body parsed
  but not painted — is tracked independently; allow-all is necessary, not
  sufficient, for full fidelity there.)
- Trade-off: allow-all fetches more (more requests, more memory, slower load) and
  reduces privacy — hence it is opt-in, per-site-overridable, and clearly labeled.
- Memory/footprint: one `bool` + a small `Vec<String>` of exempt sites on the
  single shared policy; no new dependency. All gates green; 18 consent tests
  (allow-all permits without an event, exemption inverts both ways, persistence
  round-trip) + an app test that the settings rows flip the policy.

## Alternatives considered

- **Flip the default to allow-all:** rejected — abandons the product's defining
  privacy property for everyone; the owner chose strict-default + a switch.
- **Three-state global mode (allow / ask-per-site / block):** more UI for little
  gain; a boolean switch + per-site exemptions covers the same space "simpler"
  (owner's word) and matches the requested mental model.
- **Per-resource allow lists:** the existing standing-rule mechanism already does
  fine-grained allow/deny; this adds the coarse "everything" control people
  actually asked for.

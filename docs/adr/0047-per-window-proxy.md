# ADR-0047: Per-window egress proxy

- Status: Accepted
- Date: 2026-07-01
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Cerberus already isolated identities ("heads") on two axes: each head has its own
sealed cookie partition (`InstanceId`) and its own farbling seed, so two heads
never correlate through storage or fingerprint surface. The one shared axis left
was **network egress**: the browser had a single global `--proxy <host:port>`
(an `HTTP CONNECT` tunnel, no local DNS resolution of the target), and *every*
head's traffic went through it or, without it, directly.

For the multi-identity mirror mode — one master window driving N sealed windows
of the same site, each its own session — a shared egress is the weakest link:
two identities that egress from the same IP are trivially correlatable at the
network layer no matter how well their cookies and fingerprints are partitioned.
Giving each window its own proxy completes the per-identity isolation story
(sealed cookies + farbling seed + **egress path**) while keeping the memory-first
invariant of one process and one shared network engine.

## Decision

### The `InstanceId` in `FetchContext` is the routing key

Every fetch already carries a `FetchContext { instance, kind }` so cookie
decisions happen per hop inside the engine. That same `instance` now also
selects the egress proxy — no new signal is threaded through the request path.

- `HttpEngine`/`Router` gain `with_proxies(tls, dns, jar, default_proxy,
  proxies: HashMap<InstanceId, ProxyConfig>)`. `with_options` is unchanged (it
  passes an empty map).
- `open_transport` takes the resolved `Option<&ProxyConfig>` rather than reading
  a single `self.proxy`. `fetch_once` computes it once via `proxy_for(ctx)`.
- **Resolution order:** the instance's own proxy if it has one, else the default
  proxy (the global `--proxy`), else direct. A context-free fetch (built-in
  pages, tooling) always uses the default. A proxied target is *never* resolved
  locally — unchanged from the global-proxy behavior, so there is no DNS leak on
  any per-window proxy either.

One engine still serves every window; only the CONNECT target differs per
instance. This keeps the ≤1-live-engine / one-process memory model intact.

### Per-identity proxy config lives on the head

- `Head` gains `proxy: Option<String>` (the raw `host:port`; `None` = use the
  app default / direct). Parsed to `ProxyConfig` only in the app/network layer.
- Persisted in `heads.txt` as an optional `proxy <head-id> <host:port>` line
  after the head it names. This is a v1-compatible addition: older files simply
  have no such line, and the `head` line format (label = rest of line) is
  unchanged. Ordering within the file does not matter — proxy lines are matched
  to heads by id after all heads are read.
- `cerberus-app::head_proxies(&[Head])` builds the per-instance map, and
  `network_client_with_proxies` wires it into every network entry point: the
  mirror client, the foreground browser's loader (so switching heads in the
  single window also switches egress), and the one-shot `render` path (which
  fetches under the active head's instance, so it egresses through that head's
  proxy too).

### CLI

- `identities --set-proxy <idx>=<host:port>` assigns identity `<idx>` its own
  proxy; `--clear-proxy <idx>` removes it. The `identities` listing shows
  `proxy=<host:port>` on a head that has one.

### Fail-closed

A malformed proxy string is rejected at set-time (never persisted) and, at
load/build time, aborts rather than silently falling back to direct or the
default egress. A window whose proxy is misconfigured must not quietly leak
traffic around it — the same discipline the global `--proxy` already enforces.

## Consequences

- Each mirror window can egress through its own proxy, so N identities driven in
  lockstep from one master no longer share a network vantage point. Combined with
  the existing sealed cookies and per-head farbling, an identity is now isolated
  across storage, fingerprint, and egress.
- No new per-request plumbing: the routing key (`InstanceId`) was already on
  every `FetchContext`, so the change is confined to proxy selection and config.
- The engine remains shared (one process, one live JS engine), so the memory
  budget is unaffected — only the CONNECT destination varies per instance.
- Scope, deliberately: this is an `HTTP CONNECT` proxy per identity, static for
  the session (no automatic rotation), and per-window (not per-tab-within-a-
  window). SOCKS and rotation are out of scope here.

## Verification

- `cerberus-net` unit tests: two instances mapped to two mock CONNECT proxies
  each tunnel through their own (proven by a distinct body per proxy); an
  unmapped instance falls back to the default proxy; `NoDns` proves no target
  lookup leaks on any per-window proxy.
- `cerberus-net` integration test (`tests/per_window_proxy.rs`): the same routing
  proven through the public `Router`/`HttpClient` API the app uses.
- `cerberus-app` integration test: `identities --set-proxy`/`--clear-proxy`
  round-trips through `heads.txt`, rejects a malformed proxy and an out-of-range
  index (fail-closed), and leaves the good value intact after a rejected attempt.

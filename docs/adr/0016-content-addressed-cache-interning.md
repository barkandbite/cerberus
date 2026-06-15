# ADR-0016: Content-addressed cache body interning (shared memory, sealed sessions)

- Status: Accepted
- Date: 2026-06-15
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

The prime directive is memory (PLAN §1): run many sessions/identities of the
same site at minimal RAM. ADR-0006 made the HTTP cache **per-instance** —
entries keyed by `InstanceId` — because a cache *shared* across identities is a
cross-identity tracking vector: a site can time a resource load and infer you
visited it under another identity (the classic cache-timing leak). So one
identity must never see another's cache **hit/miss**.

But per-instance entries mean the response *bytes* are stored once **per
instance**: N sessions of the same site hold N copies of the same image/script/
page body. That is exactly the "bloat of multiple memory caches" we want to
avoid when running multiple instances of one site.

These two pulls look opposed — privacy wants isolation, memory wants sharing —
but they act on different things: privacy is about **behavior** (hit/miss
timing), memory is about **storage** (the byte allocation).

## Decision

**Keep per-instance cache *entries* unchanged (privacy), and intern the response
*body bytes* in a content-addressed store shared across instances (memory).**

`HttpCache` keeps `entries: HashMap<(InstanceId, String), Entry>` exactly as
before — each instance's hit/miss/freshness is independent, so the anti-tracking
guarantee of ADR-0006 is untouched (a fresh identity still **misses and
fetches**; sharing only happens *after* its own fetch). The `Entry`'s body is now
an `Arc<[u8]>` drawn from a content-addressed pool:

- `bodies: HashMap<content_hash, Vec<Weak<[u8]>>>`. On store, `intern_body`
  hashes the bytes, finds a live identical allocation (exact compare guards hash
  collisions), and returns a shared `Arc` clone; otherwise it allocates once.
- The pool holds **weak** references, so a body frees as soon as its last `Entry`
  drops (expiry / `clear_instance` / overwrite); dead weaks are pruned lazily on
  the next intern of that hash. No leak, no manual refcount.

So N instances caching identical content cost **one** body allocation plus N
small entries — while each session stays sealed (cookies/storage/identity
unchanged; hit/miss per-instance). `get` still hands callers an owned
`HttpResponse` (a transient copy for rendering); only the resident *cache* is
deduped.

## Consequences

- **Easier:** running multiple instances/heads of the same site no longer
  multiplies cached-byte memory — the memory-first payoff for multi-session use,
  with the privacy model intact. The interning is invisible to callers
  (`get`/`store` signatures unchanged).
- **Harder:** a content hash per store (cheap; stores are per-load, not hot) and
  an exact-compare on hash hits. Header vectors are still per-entry (small
  relative to bodies; interning them is possible later if it ever matters).
- **Reversible:** entirely internal to `cerberus-net::cache`; reverting to a
  `Vec<u8>` body is a local change.

## Alternatives considered

- **Share cache *entries* across instances.** Maximum sharing, but reintroduces
  the cross-identity cache-timing leak ADR-0006 exists to prevent. Rejected.
- **Make `HttpResponse.body` an `Arc<[u8]>` everywhere.** Would also dedup the
  transient serve copy, but ripples through the whole net/engine/app API for a
  small extra win. Rejected; the cache-internal intern captures the resident
  savings that matter.
- **On-disk shared cache.** Bigger memory win across runs, but a larger design
  (eviction, integrity, the same privacy partitioning on disk) — deferred, as
  ADR-0006 already noted on-disk caching is later work.

## Note on full concurrent multi-instance

This ADR delivers the **shared-memory cache** half of "run multiple instances of
the same site without duplicate caches, sessions separate." Running many
instances *concurrently* (multiple live windows) is the larger, separate effort
sketched in `docs/ideas/multi-window-mirroring.md` — it must reconcile N
instances with the ≤1-live-engine invariant (the macro/catch-up model). The
interned cache is the memory foundation that effort builds on.

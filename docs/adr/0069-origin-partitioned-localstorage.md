# ADR-0069: Origin-partitioned, persisted `localStorage`

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Web-platform **Phase 5**. The 2026-06-25 audit (Epic #40) found `localStorage`
and `sessionStorage` were real `Storage` proxies but **run-scoped, in-memory, not
origin-partitioned, and not persisted** — each `makeStorage()` was a fresh
`Object.create(null)`, wiped on every navigation. So a value a page stored on one
visit was gone on the next, and there was no per-origin sealing. Modern JS-heavy
sites (and sensor scripts like reese84) cache state/tokens in `localStorage` and
read them back on a later visit; without persistence that round-trip can't happen.

## Decision

Give `localStorage` an origin-partitioned backing that survives navigations,
seeded into the realm before scripts run and snapshotted back after.

- **Backing — `cerberus-storage`.** `StorageEnvironment` gains a
  `web_storage: HashMap<(InstanceId, String), String>` keyed by `(identity,
  origin)`, each value the origin's store as a JSON object string. Accessors
  `local_storage(instance, origin)` / `set_local_storage(instance, origin, json)`.
  Partitioned by **both** `InstanceId` (the existing per-identity seal) **and** web
  origin — a cross-instance or cross-origin read is impossible by construction
  (unit-tested), matching the cookie jar's sealing guarantee.
- **Bridge — `cerberus-js-dom`.** `makeStorage(data)` now takes its backing object
  so the host can reach it. `localStorage`'s backing is exposed as
  `__cerberusLocalStorageData`; the prelude **seeds** it from
  `__CERBERUS_ENV__.localStorage` (a JSON string carried on `PageEnv.local_storage`)
  before scripts run, and exposes `__cerberusSnapshotLocalStorage()` →
  `snapshot_local_storage()` to read the full post-run state back (so cleared keys
  persist as gone). `sessionStorage` stays a private per-context store.
- **Host wiring — render path.** Before scripts: seed `PageEnv.local_storage` from
  `web_storage[(instance, origin)]`. After scripts: persist the snapshot back. The
  origin key is the full web origin (`scheme://host[:port]`), so subdomains don't
  share a store. This mirrors exactly where the `document.cookie`↔jar bridge is
  wired today (render path); the interactive path seeds neither yet (one shared
  follow-up).

## Consequences

- **`localStorage` is now sealed per identity + per origin and survives
  navigations** within a session that shares a `StorageEnvironment` — the
  foundation reese84-style token caching and SPA state restoration need. Within a
  single run it behaves as before (the in-realm proxy already handled
  read-after-write); the new part is the cross-navigation, origin-keyed seal.
- **No new third-party deps; memory-first preserved.** The store is plain in-memory
  `HashMap`; the JSON is shuttled opaquely between JS and Rust (no serde). `mem-gate`
  stays at 7.4 MB.
- **Gates green:** `fmt`, `clippy -D warnings`, `cargo test --workspace`
  (new `local_storage_is_sealed_per_instance_and_origin` in storage,
  `local_storage_seeds_from_env_and_snapshots_back` in js-dom; existing
  `local_storage_*` semantics tests still pass), `mem-gate --budget-mb 64`,
  `bench` (49 ms).

## Limitations / follow-ups (tracked under #46)

- **In-memory, not on-disk.** `web_storage` lives for the `StorageEnvironment`'s
  lifetime (it survives navigations in a session), but isn't written to the vault
  yet, so it doesn't survive a restart. Disk persistence (vault-backed, like
  cookies) is the next step — the same module note already flags M4-style on-disk
  format as pending.
- **Interactive path not yet seeded.** Like the cookie bridge, the headless render
  path is wired; `BrowserApp`'s interactive `run_scripts` seeds neither cookies nor
  `localStorage` yet — one follow-up wires both there (where cross-navigation
  persistence becomes user-visible).
- **`sessionStorage` is still per-run** (not persisted, not origin-keyed in the
  store) — correct for its semantics (scoped to the browsing context), and the
  realm already persists it across interactions within a context.
- **No `StorageEvent`, no quota, no IndexedDB.**

## Alternatives considered

- **Persist via the vault blob store (`store_blob`/`load_blob`):** would give
  encryption + disk persistence for free, but those require an **unlocked vault**,
  which the render path doesn't have (no passphrase) — so it'd be a locked-vault
  no-op there. A plain in-memory map is always available (incl. ephemeral renders)
  and vault-backed disk persistence can layer on later. Web storage also isn't a
  secret the way quarantined cookies are, so vault encryption isn't required for
  correctness.
- **Key by `Origin::site()` (eTLD+1):** simpler and matches cookie/consent
  partitioning, but merges subdomains — looser than the spec's origin scoping.
  Keyed by full origin instead, which is correct and costs nothing.
- **Seed via a separate call instead of `PageEnv`:** `PageEnv` is installed exactly
  once, before scripts, atomically with url/cookies/viewport — the right place; a
  separate call would race the install ordering.

# ADR-0033: Security hardening pass (CSPRNG, at-rest perms, secret zeroization, autofill origin binding)

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Before loading **real credentials** (via the CSV importer, ADR-0030) and driving
them through forms, the four highest-severity open security issues are fixed —
each directly defeats a promise in `SECURITY.md`. No new third-party crates beyond
two already in the dependency tree (`getrandom`, `zeroize`).

## Decision

### #9 — Fail-closed CSPRNG
`cerberus_storage::random_bytes` (the single entropy source for the Argon2id
salt, every AEAD nonce, each `InstanceId`, and each farbling seed) dropped to a
**SplitMix64** stream seeded from time/pid/stack if `/dev/urandom` couldn't be
read — reachable in production (notably Windows, which has no `/dev/urandom`). It
now uses the audited **`getrandom`** crate on every platform and **panics
(fail-closed)** if the OS RNG is unavailable, rather than minting predictable key
material. The insecure fallback is deleted.

### #8 — At-rest file permissions (+ #15)
Every profile file routes through `atomic_write`. It now creates files **`0600`
on Unix** (owner-only) via `OpenOptions::mode`, so `heads.txt` (farbling seeds),
`cookies.bin` (plaintext cookie values), `vault.salt`, and the policy files are
not world-readable — closing the "local disk inspection" adversary for the
everyday jar + seeds (the vault was already encrypted). While here, the shared
`.tmp` sibling became a **unique** per-write name (pid + counter) so concurrent
writers don't race (#15), with cleanup on failure. (Windows ACL hardening is a
tracked follow-up.)

### #17 — Zeroize decrypted autofill secrets
`Login` and `Card` now `#[derive(ZeroizeOnDrop)]`, so the `password`, card
`number`, and `cvv` heap buffers are **wiped when dropped** instead of freed in
cleartext (swap/core-dump/heap-reuse exposure). Their `Debug` is now manual and
**redacts** the secrets. Remaining follow-ups: zeroize the transient fill-plan
strings, and proactively clear the `ProfileFillProvider` map on vault lock (today
it wipes when the session/map drops).

### #12 — Origin-bound autofill secrets
A `Profile` gains an **`origin`** host. Autofill (`value_for`/`fill_plan`, now
taking the page host) refuses to emit a **secret** (password, any card field)
unless the profile's `origin` **covers** the page host — equal, or a dot-boundary
subdomain (`example.gov` ⊇ `login.example.gov`, ∌ `notexample.gov`). An unbound
profile (`origin == ""`) **never** autofills secrets (fail-closed). Non-secret
fields (name/address/email/phone/username) still fill anywhere. The host is
extracted userinfo-safely (`https://trusted@evil.test/` → `evil.test`). `origin`
is persisted (blob format v2, back-compatible with v1) and is a new CSV column +
`profile --set origin=` key.

## Consequences

- **Easier / safer:** predictable-entropy, world-readable-at-rest, cleartext-
  secret-in-RAM, and cross-origin-secret-leak classes are closed before real
  credentials land. The mirror's one-`Fill`-fills-each-identity path now also
  enforces per-identity origin binding, which fits the "each identity logs into
  its own account on a specific site" model.
- **Behavioral change:** secrets won't autofill until a profile's `origin` is
  set — imported CSVs must include the `origin` column (or `profile --set
  origin=…`) to enable password/card autofill. This is the intended fail-closed
  default.
- **Costs / follow-ups:** Windows file ACLs (#8 is Unix-mode only), fill-plan
  string zeroization + provider-clear-on-lock (#17), and PSL-aware origin
  matching (today a dot-boundary host suffix) remain. The CSPRNG now aborts on
  RNG failure (correct for a browser that must seal data).

## Alternatives considered

- **Keep the SplitMix64 fallback `cfg`-gated to debug:** rejected — any transient
  `/dev/urandom` failure could still hit it; fail-closed is the only safe posture
  for key material.
- **Hand-roll a Windows CSPRNG via FFI:** rejected — security-critical code we
  should not hand-roll; `getrandom` is audited and already in the tree.
- **Per-field `Zeroizing<String>`:** rejected for now — it changes field types
  across every read/write site; `ZeroizeOnDrop` on the structs wipes on drop with
  far less churn (struct-update syntax on those types is the only cost).
- **Origin allowlist per credential / full per-site vault:** heavier model; the
  single bound `origin` per identity matches the product's one-profile-per-
  identity design and the claims-automation use case.

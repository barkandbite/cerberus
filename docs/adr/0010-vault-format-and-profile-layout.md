# ADR-0010: Vault on-disk format & persistent-profile layout

- Status: Accepted
- Date: 2026-06-11
- Deciders: owner (standing decision directive), engineering

## Context

M4 requires one data-store environment with per-instance sealed cookies, an
encrypted vault for quarantined cookies (Argon2id + AEAD), and persisted
cookie groups. The dependency policy (ADR-0003) mandates bootstrapped
serialization — no serde — and the crypto adapters were approved as
`chacha20poly1305` (XChaCha20-Poly1305) + `argon2` behind our `Aead`/`Kdf`
seams in `cerberus-crypto-rustcrypto`.

## Decision

### Profile layout (`--data-dir <DIR>`; absent ⇒ fully ephemeral, zero writes)

```
<data-dir>/
  heads.txt                       # text: per-profile random head ids/instances/seeds + active index
  consent.rules                   # text: allow|deny <instance> <fp-site> <request-site>
  vault.salt                      # 16 raw bytes, minted once per profile
  vault.bin                       # CERB records: check sentinel + ciphertext blobs
  instances/<instance-hex>/cookies.bin   # CERB records: active + quarantined cookie metadata
```

Human-auditable text for human-scale files (heads, rules); binary records for
anything carrying arbitrary bytes. Every write is atomic (tmp sibling → fsync
→ rename), so a crash leaves the previous version, never a torn file.

### Binary record format (`disk.rs`)

`CERB` magic + `u16` format version + `u16` kind, then records: `u32`-LE
field count, then per field `u32`-LE length + bytes. A truncated tail is an
error, not a silent EOF. Kinds: 1 = cookies, 2 = vault.

### Cookie records

Active: `["A", fp_site, name, value, domain, path, expires(8B LE | empty),
flags]`; quarantined: `["Q", state, fp_site, name, domain, path, expires,
flags]` — a quarantined value exists **only** as vault ciphertext. Session
cookies (no expiry) are not restored across restarts; expired cookies are
dropped at load. Sealing stays structural on disk: one directory per
instance, and `StorageEnvironment::load` rebuilds the same
no-foreign-instance API.

### Vault format & key lifecycle

`vault.bin` holds `["C", nonce, ct]` (the unlock-check sentinel) and
`["B", instance, key, nonce, ct]` per blob. Ciphertext only: the key is
derived at unlock (Argon2id, m=19 MiB/t=2/p=1, salt from `vault.salt`), lives
in a zeroizing `Key`, and is dropped on lock — it never exists at rest, nor
in the OS keystore. Blobs are sealed with **fresh random 24-byte XNonces**
(never a counter — a persisted counter would reuse nonces after a restart,
issue #2) and bound to their `(instance, key)` slot via AAD, so a blob cannot
be relocated to another instance without failing authentication. The sentinel
makes a wrong passphrase fail *at unlock* while the vault stays locked.
Passphrase loss = vault loss, by design (PLAN §7).

## Consequences

- Easier: a profile is a directory; backup/audit/delete is `cp`/`less`/`rm`.
- Easier: the ephemeral default stays the privacy posture — no flag, no disk.
- Harder: we own the codec (mitigated: versioned header, atomic writes,
  round-trip + tamper tests, and the vault tested against a reload).
- `mlock` is not used in v1 (would require `unsafe` for a partial
  mitigation); swap exposure is documented in the threat model with OS swap
  encryption as the compensating control.

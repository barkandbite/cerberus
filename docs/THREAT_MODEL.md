# Cerberus — Threat Model

## Who we protect against

Cerberus defends its user against **trackers, ad networks, and surveillance**:

- **Cross-site trackers & ad networks** building a profile by correlating cookies
  and storage across the sites you visit.
- **Fingerprinters** building a stable, cross-site identity from device/browser
  signals (canvas, audio, WebGL, fonts).
- **Network observers** seeing what you resolve and fetch (DNS, TLS metadata).
- **Local disk inspection** of the browser's data at rest (someone with your
  files, but not your vault passphrase).

The product realizes this through: sealed per-instance cookies, the quarantine
vault, cookie groups, the consent gate, farbling, and three isolated identities.

## What we explicitly do NOT defend against (non-goals)

The threat model is **trackers, ad networks, and surveillance — not anti-fraud or
anti-bot systems.** Cerberus is not an "anti-detect" or automation tool. We do
**not** build:

- **No parallel/concurrent orchestration of the heads** against a target. Heads
  are foreground, one at a time.
- **No fingerprint impersonation / anti-detect** (the Multilogin/Kameleo
  pattern). Farbling randomizes *our own* surface; it never impersonates another
  browser or device.
- **No warm-session-pool, identity-warming, or session-aging.**
- **No rotating-proxy pools, no CAPTCHA solving, no anti-bot/anti-fraud evasion.**

These are deliberate product boundaries, not yet-to-be-built features.

## Assets

| Asset | Protection |
| --- | --- |
| Identity separation (work / personal / throwaway) | Sealed per-instance cookie partitions; per-head farbling seed |
| Cookie confidentiality at rest | AEAD-encrypted vault; key derived from passphrase via Argon2id |
| Unlinkability across heads | Distinct sealed partitions + distinct farbling seeds; one engine at a time |
| Unlinkability across sites within a head | Quarantine + consent gate on cross-site storage; per-session farbling |
| Network metadata | DoH for resolution; rustls for transport; no telemetry |

## Trust boundaries

1. **Instance seal (structural).** An instance's runtime can only resolve cookies
   tagged with its own `instance_id`. Cross-instance reads are impossible *by
   construction* (no API exists to do it), not by a policy check. See
   ADR-0001 §"Storage sealing".
2. **The vault.** AEAD-encrypted at rest at a randomized on-disk path. The key is
   derived from the passphrase via Argon2id, held only in locked (`mlock`'d),
   zeroized memory, and **lives nowhere at rest and not in the OS keystore**.
   Full disk access reveals nothing without the passphrase.
   - **Accepted trade-off:** unlock requires the passphrase; losing the
     passphrase loses the vault.
3. **The farbling surface.** Per-instance, per-session deterministic, bounded
   noise on fingerprintable APIs. Goal: deny a tracker a stable cross-site
   identity of the active head. The three heads carry independent seeds so they
   do not correlate.
4. **The consent gate.** Cross-site / third-party storage defaults to **deny**; a
   consent event is raised in headed mode. Headless denies silently.

## Headless mode

Scoped to rendering (PNG/PDF) and automated tests. It **inherits farbling**;
third-party storage defaults to **deny**; there is **no auto-release of
quarantine**; and only a **single user-configured proxy** is allowed (no pools).

## Out of scope

- A local attacker who already has your vault passphrase.
- Kernel, firmware, or hardware compromise; malicious OS; cold-boot attacks
  beyond the `mlock`/zeroize best effort.
- Compromised audited dependencies we rely on (mitigated by the dependency policy
  in ADR-0003, not eliminated).
- Active de-anonymization by a global passive adversary correlating traffic
  timing (Tor's threat model, not ours).

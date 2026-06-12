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
- **No general fingerprint impersonation / anti-detect** (the Multilogin/Kameleo
  pattern). The *device* surface is always our own — uniform, low-entropy, and
  farbled — and never spoofs a specific machine. The **User-Agent is the one
  deliberate exception**: honest (`Cerberus/0.0`) by default, it escalates to a
  common, mainstream string *only* when an origin refuses to serve us, and the
  whole identity (request header, `navigator.userAgent`, OS-derived `platform`,
  `Accept-Language`) stays coherent so the escalation can't be detected by a
  mismatch. This is a narrow compatibility measure, not orchestrated anti-detect —
  there is still no proxy rotation, session warming, or CAPTCHA solving (below).
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
2. **The vault.** XChaCha20-Poly1305-encrypted at rest (`vault.bin`,
   ADR-0010): ciphertext blobs bound to their `(instance, key)` slot via AAD,
   each sealed with a fresh random 24-byte nonce. The key is derived from the
   passphrase via Argon2id (m=19 MiB), held only in zeroized memory, and
   **lives nowhere at rest and not in the OS keystore**. A wrong passphrase
   fails at unlock (check sentinel) and the vault stays locked. Full disk
   access reveals nothing without the passphrase.
   - **Accepted trade-off:** unlock requires the passphrase; losing the
     passphrase loses the vault.
   - **Accepted trade-off (v1):** key pages are *not* `mlock`'d — that would
     require `unsafe` (denied workspace-wide) for a partial mitigation. Under
     memory pressure key material could reach swap; OS-level swap encryption
     is the compensating control.
3. **The farbling surface.** Per-instance, per-session deterministic, bounded
   noise on fingerprintable APIs. Goal: deny a tracker a stable cross-site
   identity of the active head. The three heads carry independent seeds so they
   do not correlate. Underneath the noise, the scripting environment is
   **uniform and minimal**: device signals (`hardwareConcurrency`, screen,
   `language`) report fixed low-entropy values for every user, and the
   high-entropy surfaces a tracker reaches for — `plugins`, `mediaDevices`,
   `deviceMemory`, the Battery API — are simply **absent**, so there is
   nothing to read. The active-noise layer (M6) ships: canvas readbacks are
   synthesized per (head seed, draw-op log), WebGL identifies uniformly as
   "Cerberus" while `readPixels` carries per-head noise, audio readbacks are
   seeded near-silence, and `measureText` jitters ≤2% per head — all
   deterministic within a head, uncorrelated across heads.
4. **The consent gate.** Cross-site / third-party access defaults to **deny**,
   enforced on the real pipeline: subresource fetches and cookie
   attach/capture all consult the policy, keyed by registrable domain
   (eTLD+1) over a vendored Public Suffix List snapshot — so `cdn.site.com`
   is first-party to `site.com`, while `alice.github.io` and `bob.github.io`
   are different sites. Headed mode prompts (banner) and persists
   instance-scoped rules; headless denies silently.
   On top of the gate, every accepted cookie carries a user **disposition**
   (Allow / Session / Timed(user duration) / Block / Allow-once) resolved from
   a per-cookie/per-site/global policy — so the user, not the site, decides
   what persists and for how long. Session/Allow-once cookies never touch disk;
   the cookie inspector and the `cookies` CLI make the whole jar visible
   (ADR-0011).

## Headless mode

Scoped to rendering (PNG/PDF) and automated tests. It **inherits farbling**;
third-party storage defaults to **deny** (silently — no prompts); there is
**no auto-release of quarantine**; and only a **single user-configured proxy**
is allowed (no pools) — `--proxy HOST:PORT` tunnels *all* traffic (DoH
included) through one HTTP CONNECT egress, never resolves target hosts
locally, and fails closed on misconfiguration.

## Out of scope

- A local attacker who already has your vault passphrase.
- Kernel, firmware, or hardware compromise; malicious OS; cold-boot attacks
  beyond the `mlock`/zeroize best effort.
- Compromised audited dependencies we rely on (mitigated by the dependency policy
  in ADR-0003, not eliminated).
- Active de-anonymization by a global passive adversary correlating traffic
  timing (Tor's threat model, not ours).

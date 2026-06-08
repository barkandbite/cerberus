# Security Policy

Cerberus is a privacy-and-security project; we take vulnerability reports
seriously and welcome them.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

- Preferred: GitHub **Private vulnerability reporting** (Security → Report a
  vulnerability) on this repository.
- Or email **bbarker@barkbite.org** with a description, reproduction steps, and
  impact. (A PGP key will be published here before 1.0.)

We aim to acknowledge within **72 hours** and to agree on a disclosure timeline
with you. We support coordinated disclosure and will credit reporters who wish to
be credited.

## Scope

In scope:

- Breaks of the **instance seal** (reading another instance's cookies/storage).
- **Vault** weaknesses: key recovery without the passphrase, key material
  reaching disk or the OS keystore, missing zeroization.
- **Quarantine** bypass: a quarantined cookie attached to a request without
  explicit release.
- **Consent** bypass: third-party storage allowed without a rule or prompt.
- **Farbling** flaws that allow stable cross-site or cross-head correlation.
- **Network hygiene**: unexpected outbound connections, telemetry, DoH bypass.
- Memory-safety issues, especially in (future) `unsafe` FFI adapters.

Out of scope: the items under "Out of scope" in
[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) (e.g. an attacker who already holds
your vault passphrase, kernel/hardware compromise). Anti-bot/anti-fraud evasion
is a **non-goal**, not a vulnerability.

## Supported versions

Pre-1.0: only the `main` branch is supported. Security fixes land on `main`;
there are no maintained release branches yet.

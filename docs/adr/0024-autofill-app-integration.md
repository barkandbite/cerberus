# ADR-0024: Autofill app integration (per-identity fill across a mirror group)

- Status: Accepted
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0022 (autofill engine), ADR-0017/0018 (mirror groups), ADR-0010 (vault), #6

## Context

ADR-0022 added `cerberus-autofill` — the pure engine that detects a page's form
fields and produces a fill plan from a `Profile` (login/address/card). That crate
is policy-free and app-agnostic. Shipping autofill in 0.0.1 also needed the
**app wiring**: where profiles live, how they are sealed per identity, and how a
single master gesture fills every mirror window from *its own* profile without
ever running more than one live engine. This ADR records that integration (the
piece `CHANGELOG.md` cites as ADR-0024).

## Decision

- **Vault-sealed, per-identity profiles.** Each identity's `Profile` is stored
  under the vault key `autofill.profile` (`AUTOFILL_PROFILE_KEY`) via the
  storage `store_blob`/`load_blob` seam, serialized with the engine's own
  length-prefixed format (no serde, per the dependency policy). The `profile`
  CLI command shows/sets fields; the passphrase comes from `CERBERUS_VAULT_PASS`,
  never an argument.
- **`FillProvider` seam.** `cerberus-mirror` defines `trait FillProvider { fn
  fills(instance, kind, doc) -> Vec<(NodeId, String)> }`. The app implements it
  as `ProfileFillProvider`, a map from `InstanceId` to the vault-loaded
  `Profile`, calling `cerberus_autofill::fill_plan` per instance.
- **One gesture, each its own profile.** A master `Action::Fill(kind)` is logged
  once; on each window (master now, followers on catch-up) the group resolves the
  fill through the `FillProvider` keyed by *that window's* `InstanceId`, sets each
  field's value, and fires `input`. So one action fills N windows, each with its
  own credentials, still under the ≤1-live-engine invariant.

## Consequences

- **Easier:** operating many accounts of one site — fill all of them at once,
  each with the right identity, by acting once on the master.
- **Sealed:** profiles never leave the vault unencrypted; a window only ever sees
  its own profile (the provider is keyed by `InstanceId`).
- **Cost:** the app must hold the per-identity profile map for the running group;
  it is built once when entering mirror mode from the already-unlocked vault.

## Scope / limits (0.0.1, documented)

- The in-window **manager UI** and the **single-window fill gesture** were not
  wired in 0.0.1 (use `profile` + `run --mirror`). They are follow-ups —
  see ADR-0025.

## Alternatives considered

- **Fill in the engine crate.** Rejected — `cerberus-autofill` stays pure;
  vault/identity/group policy belongs in the app.
- **One shared profile for the group.** Defeats the multi-identity purpose;
  rejected in favor of per-`InstanceId` resolution.

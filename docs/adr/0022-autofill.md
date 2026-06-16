# ADR-0022: Autofill — per-identity profiles + form-field detection

- Status: Accepted (core); wiring in progress
- Date: 2026-06-16
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0010 (vault), ADR-0017/0018 (mirror groups), #6

## Context

The owner asked for simple, memory-frugal autofill for logins, addresses, and
cards, tied to an identity profile, decided earlier: **vault-encrypted (CVV
included), fill-only** (submit is a normal/mirrored click, never automated), a
**profile = exactly one login + one address + one card**, and **arbitrary N**
profiles (one per identity).

## Decision

A new pure crate **`cerberus-autofill`** holds the model + detection, mirroring
how `cerberus-mirror` was built (model first, wired after):

- `Profile { login, address, card }` value types.
- `classify(field)` detects a control's [`FieldKind`] by strongest-signal-first
  heuristics: input `type` (`password`/`email`/`tel`), then the `autocomplete`
  token (`cc-number`, `postal-code`, …), then `name`/`id`/`placeholder` patterns.
  Submit/checkbox/hidden/etc. are never fillable.
- `fill_plan(doc, profile, kind)` returns the `(NodeId, value)` pairs to set,
  scoped by `FillKind` (Login/Address/Payment/All) so a "fill login" gesture only
  touches credential fields. Empty profile values are skipped; `cc-exp` is
  composed from month/year. **Fill-only** — submit is never included.

The crate depends only on `cerberus-dom`. **Deferred to the wiring step (app +
mirror):** `Action::Fill(FillKind)` + a `FillProvider` in `cerberus-mirror` so a
master fill broadcasts and each window fills its *own* profile; encrypted-vault
persistence per identity (`cerberus-storage`); and the autofill manager UI.

## Consequences

- **Easier:** the autofill brain (data + detection + planning) is a small,
  dependency-light, fully unit-tested unit, decoupled from storage/UI — exactly
  the per-window fill the mirror model wants (`fill_plan` is keyed by `NodeId`,
  applied via `set_node_value`).
- **Harder/limits:** `<select>` (e.g. country dropdowns) is not yet handled
  (text inputs/textarea only); detection is heuristic; the vault + UI + mirror
  `Action::Fill` wiring is the remaining integration.

## Alternatives considered

- **Browser-style saved-form capture (learn fields per site).** Heavier and
  needs storage/UX; the deterministic profile model fits the multi-identity use
  case and is simpler.
- **Full HTML `autocomplete` spec.** v1 covers the common token + heuristic set;
  the long tail can grow behind the same `classify` seam.

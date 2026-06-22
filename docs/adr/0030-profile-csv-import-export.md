# ADR-0030: Bulk profile setup via CSV import/export

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

MIRC drives many identities, each filling its **own** account (login / address /
card) from its sealed [`Profile`](../../crates/cerberus-autofill). Today profiles
are entered one field at a time (`profile --set "key=value;…"`, per identity) —
unworkable when standing up dozens of identities from records. To even begin
end-to-end MIRC testing the owner needs to load real test credentials in bulk, so
a **bulk import/export** path must come before the richer per-identity field/
relationship UI.

Constraints: no new third-party crates (dependency policy → hand-roll), and the
data is messy (addresses are full of commas), so the format must be robust.

## Decision

A small, dependency-free **CSV codec** for a *table* of profiles lives in
`cerberus-autofill` (it owns `Profile`), and the app exposes it through the
existing `profile` command.

### Format
- One row per identity: an `identity` label column + the 16 `Profile` fields, in
  `Profile::to_bytes` order, with header names matching the `profile --set` keys
  (`login.username`, `address.city`, `card.number`, …). See `CSV_HEADERS`.
- **Columns are mapped by header name**, so column order is free, unknown columns
  are ignored, and absent columns default empty. Only `identity` is required.
- **Delimiter is configurable** (`--delimiter <char|name>`), default `:` (the
  owner's preference; commas are common in the data). On **import the delimiter
  is auto-detected** from the header, so a file prepared with `,` / `;` / tab /
  `|` still loads.
- **RFC-4180 quoting** (wrap a field containing the delimiter/quote/newline in
  `"`, double interior `"`) makes any field lossless regardless of delimiter.

### Commands (`cerberus profile …`)
- `--template <FILE|->` — write a no-frills template (header + two example rows).
  Needs no vault; it is pure text.
- `--export <FILE>` — write every identity's sealed profile (empty profiles
  included, so the export doubles as a labeled template to edit and re-import).
- `--import <FILE>` — seal each row's profile in the vault, mapping rows to
  identities **by label**; a label with no existing identity is **created**
  (minted like `identities --add`) so a filled template stands up many identities
  at once. Duplicate labels in a file are rejected.

Export/import unlock the vault with `CERBERUS_VAULT_PASS` (never an argument),
consistent with `profile --set`.

## Consequences

- **Easier:** stand up N identities + credentials in one file instead of N×16
  `--set` calls; the export/template gives a ready-to-edit starting point; the
  same sealed `Profile` + vault path is reused (no new storage seam). Unblocks
  driving 3–5 profiles end-to-end with real test data.
- **Costs / sharp edges:** import **creates** identities by default — convenient
  for setup but a typo'd label mints a stray identity (reported per row, and
  removable via `identities --remove`). Secrets (passwords, CVV) live in
  plaintext in the CSV on disk — acceptable for an explicit, owner-driven
  import/export of test data, but the file should be deleted after import.
  Cookies are **not** part of the CSV (profiles only) — a later addition.

## Alternatives considered

- **A `csv` crate / `serde`:** rejected by the dependency policy; the hand-rolled
  codec is ~150 lines and sufficient.
- **Comma as the default delimiter (true "CSV"):** addresses routinely contain
  commas; the owner prefers `:`. Quoting makes either safe, and import
  auto-detects, so the default is just a template/export convenience.
- **JSON/TOML:** heavier to hand-edit in a spreadsheet/records workflow and would
  still need a parser; CSV is the lowest-friction bulk-entry format here.
- **Position-based columns:** brittle; header-name mapping tolerates reordering
  and partial files.

# Contributing to Cerberus

Thanks for your interest. Cerberus is a privacy-first, memory-lean browser in
Rust. Please read [PLAN.md](PLAN.md) and the
[ADRs](docs/adr/) before significant work — the architecture has firm rules.

## Two non-negotiables

1. **No dependency without an ADR + owner approval.** Default to bootstrapping.
   If you want a crate, open an ADR (see [docs/adr/README.md](docs/adr/README.md))
   covering what it does, why not bootstrap now, its license, its
   maintenance/audit status, and the trait it sits behind. We lean on
   dependencies only for crypto, TLS, the JS engine, and font/image decoders
   (ADR-0003).
2. **No foreign types across module boundaries.** Every third-party dependency is
   wrapped behind one of our traits in a dedicated adapter crate. Callers depend
   only on our traits and `cerberus-types`. The test: one should be able to
   delete an adapter crate, reimplement the trait, and have everything else
   compile unchanged (ADR-0001).

## Memory is priority #1

Features and speed come after memory. Before adding allocation on a hot path,
consider arenas/bump allocation. Keep "one engine live at a time" intact. The CI
memory gate (`cerberus-app mem-gate`) must stay green and within
[budget](PLAN.md#5-memory-budget-proposed-for-sign-off).

## Development loop

```sh
cargo fmt --all                 # format (CI checks --check)
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p cerberus-app -- render          # trivial render -> cerberus-home.ppm
cargo run -p cerberus-app --release -- mem-gate --budget-mb 64 --switches 25
cargo run -p cerberus-app --release -- bench --assert-total-ms 500
cargo deny check   # licenses/advisories/bans/sources — required for any dep change
```

The toolchain is pinned in `rust-toolchain.toml`; `rustup show` installs it.
The scaffold is **std-only and builds offline** — keep it that way until a
dependency is approved.

## Code conventions

- `unsafe_code` is denied workspace-wide. An adapter that genuinely needs FFI
  opts in explicitly (`#![allow(unsafe_code)]` at the crate root) and documents
  why; expect extra review on those.
- Public items carry doc comments; modules start with a `//!` summary.
- New subsystem capability → new trait in the subsystem crate; new dependency →
  new adapter crate implementing an existing trait.
- Add tests that assert *structural* guarantees where possible (see the
  cross-instance and quarantine tests in `cerberus-storage`).

## Commits & PRs

- Small, focused commits with clear messages (imperative mood).
- Reference the milestone (e.g. "M1:") where relevant.
- PRs must pass CI: fmt, clippy (warnings = errors), build, tests, and the memory
  gate. Open as a draft until ready for review.
- Architectural changes need an ADR in the same PR.

## No telemetry

Cerberus does not phone home. Do not add analytics, crash upload, or any silent
outbound connection. Network hygiene is part of the threat model.

## License

By contributing you agree your contributions are licensed under the repository's
license (currently Apache-2.0; see PLAN §10 and ADR-0002 for the pending
ratification).

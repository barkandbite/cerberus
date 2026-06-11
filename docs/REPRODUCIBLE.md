# Reproducible builds

A privacy browser asks for trust; a byte-reproducible build lets anyone verify
that the published binary is exactly this source. Cerberus pins everything
that feeds the compiler:

- **Toolchain**: `rust-toolchain.toml` pins the exact rustc (channel + 
  components). `rustup show` installs it.
- **Dependencies**: `Cargo.lock` is committed; CI and release builds use
  `--locked`. The dependency tree itself is policy-gated (ADR-0003,
  `deny.toml`, `cargo deny check`).
- **Profile**: the release profile sets `codegen-units = 1`, `lto = "thin"`,
  `panic = "abort"`, and `strip = true` (Cargo.toml), removing parallel-codegen
  nondeterminism and host symbol paths.
- **Paths**: absolute workspace paths are stripped from remaining metadata via
  `--remap-path-prefix`.

## Byte-reproduce a release binary

```sh
rustup show                       # installs the pinned toolchain
RUSTFLAGS="--remap-path-prefix=$PWD=/cerberus" \
  cargo build --release --locked -p cerberus-app
sha256sum target/release/cerberus-app
```

Run the same commands from the same commit on another machine (same target
triple) and the hashes must match. The local smoke check is:

```sh
RUSTFLAGS="--remap-path-prefix=$PWD=/cerberus" cargo build --release --locked -p cerberus-app
sha256sum target/release/cerberus-app > /tmp/h1
cargo clean -p cerberus-app
RUSTFLAGS="--remap-path-prefix=$PWD=/cerberus" cargo build --release --locked -p cerberus-app
sha256sum target/release/cerberus-app | diff /tmp/h1 -
```

## Offline / audited builds

`cargo vendor vendor/` snapshots every dependency source into the tree; with

```toml
# .cargo/config.toml (not committed; created by cargo vendor's output)
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
```

the build runs with the network disabled, against sources you can audit and
hash. The vendor snapshot is not committed to keep the repository lean; the
`Cargo.lock` checksums pin the exact bytes `cargo vendor` must produce.

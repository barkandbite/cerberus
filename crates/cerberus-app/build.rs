//! Give the Windows executable a main-thread stack that matches other platforms.
//!
//! Windows reserves only 1 MiB for the main thread's stack, where Linux and
//! macOS give 8 MiB. The browser drives QuickJS engine setup/teardown on the
//! main thread (e.g. the mem-gate's head switches, or switching identities at
//! runtime), and that native call depth can exceed 1 MiB — overflowing the
//! stack on Windows ("thread 'main' has overflowed its stack") while every
//! other platform, with 8x the headroom, is fine. Raise the PE stack *reserve*
//! to 8 MiB so Windows behaves like the rest. This is not a workaround for
//! unbounded recursion (the JS engine keeps its own bounded stack cap); it just
//! removes an arbitrary platform inconsistency.
//!
//! Done from a build script rather than `.cargo/config.toml` `[target.*]
//! rustflags` because a `RUSTFLAGS` environment variable — which CI sets to
//! `-D warnings` — *replaces* config rustflags instead of merging with them,
//! which would silently drop this flag. Build-script `rustc-link-arg-bins`
//! directives are always applied, and only to the binary (not tests/benches).
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // 8 MiB, matching the Linux/macOS default main-thread stack.
        const RESERVE: u64 = 8 * 1024 * 1024;
        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            // MSVC linker (the shipped/CI target): /STACK:reserve.
            Ok("msvc") => println!("cargo::rustc-link-arg-bins=/STACK:{RESERVE}"),
            // GNU/ld (the mingw cross-build used for local Wine testing).
            _ => println!("cargo::rustc-link-arg-bins=-Wl,--stack,{RESERVE}"),
        }
    }
}

# ADR-0015: Windows port (baseline)

- Status: Accepted
- Date: 2026-06-15
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

Cerberus is pure-Rust and hexagonal (ADR-0001): one crate per subsystem, every
third party behind one of our traits. The platform-touching pieces are few and
already chosen for portability:

- **Windowing** — winit + softbuffer (ADR-0004): cross-platform (Win32/AppKit/
  X11/Wayland), CPU-only.
- **TLS** — rustls + ring + **bundled** webpki-roots (ADR-0006): the default
  trust path carries its own roots, no OS store needed.
- **Crypto / fonts / images / SVG / JS** — RustCrypto, ab_glyph (bundled
  Roboto), the `image` crate, resvg, and bundled QuickJS via rquickjs: all build
  on `x86_64-pc-windows-msvc` (the Windows runners ship the MSVC C toolchain
  ring and QuickJS need).

A full-workspace audit for OS-specific code (`/proc`, `std::os::unix`, `libc`,
unix permission bits, hardcoded paths, signals) found **exactly two** Linux-only
spots, both already non-fatal off Linux:

1. `resident_set_kb()` reads `/proc/self/status` for the `mem-gate` budget check.
2. `RustlsProvider::with_system_roots()` reads the Linux CA bundle
   (`/etc/ssl/certs/ca-certificates.crt`) for the opt-in `--system-roots` flag.

## Decision

**Port to Windows by making those two seams explicit and adding a Windows CI
build; change nothing in the core.** No new dependencies; no `unsafe` added.

- `resident_set_kb()` is now `#[cfg(target_os = "linux")]` (procfs) vs.
  `#[cfg(not(...))]` returning `None`. The `mem-gate` command already degrades
  gracefully on `None` (it prints "unavailable on this platform; skipping" and
  does not assert), so the budget gate stays Linux-only without breaking the
  build or the command elsewhere.
- `with_system_roots()` is `#[cfg(target_os = "linux")]` (the PEM bundle) vs.
  `#[cfg(not(...))]` returning a clear error. The **default** TLS path
  (`RustlsProvider::new`, bundled Mozilla roots) is unchanged and works on every
  platform; only the niche TLS-inspecting-proxy flag is Linux-only for now.
- CI gains a `windows-latest` matrix leg that runs **build + test + bench**
  (the cross-platform port gate); `fmt`/`clippy` (source-only) and the procfs
  `mem-gate` stay Linux-only. This is the real verification: cross-building the
  C deps (ring, QuickJS) from Linux is not viable, so a native MSVC runner is the
  source of truth.

macOS is explicitly out of scope for this increment; the same `cfg(not(linux))`
arms already compile there, so it is the same two follow-ups when we add it.

## Resolved after the fact

- **Native Windows RSS** for `mem-gate` — implemented as option (b): a
  **no-dependency** `cerberus-sysmem` adapter crate wrapping Win32
  `GetProcessMemoryInfo` (the process working set), with the single `unsafe` FFI
  isolated and reviewed there (PLAN §7). No third-party crate, so no ADR-0003
  sign-off was needed. The Windows memory-budget gate now enforces (the CI
  `mem-gate` step runs on both OSes). macOS reuses the adapter's `None` arm until
  a `task_info` probe is added.

## Deferred (need owner sign-off — ADR-0003 governs dependencies)

- **Windows `--system-roots`** via `rustls-native-certs` (SChannel) — a new
  dependency, deferred behind owner approval. The default bundled Mozilla roots
  work on Windows today, so this only affects the niche TLS-inspecting-proxy
  flag.

## Consequences

- **Easier:** the core browser (render, JS engine, events, the bounded loop,
  fetch, the whole privacy stack) builds and runs on Windows with zero core
  changes — the hexagonal boundaries paid off. New modules we build inherit the
  portability for free.
- **Harder:** two capabilities are Linux-only until the deferred items land
  (Windows memory-budget enforcement; `--system-roots`). Both degrade with a
  clear message, neither blocks browsing.
- **Reversible:** the two seams are single `cfg` pairs; the CI leg is one matrix
  entry.

## Alternatives considered

- **Add `memory-stats` now** for a fully-working Windows `mem-gate`. Rejected as
  the *baseline* step: it adds a dependency, which ADR-0003 requires the owner to
  approve; surfaced above as the first deferred option.
- **Cross-compile the Windows `.exe` from Linux** (windows-gnu + mingw).
  Rejected: ring/QuickJS C builds make this fragile; a native MSVC CI runner is
  both the honest build and the artifact source.
- **`rustls-native-certs` for the default trust path.** Rejected: the bundled
  webpki-roots are deliberately reproducible and OS-independent (ADR-0006); the
  OS store stays opt-in only.

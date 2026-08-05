# Cerberus — agent guide

## Commit & PR identity — MANDATORY, never violate

Every git commit and every pull request in this repository MUST be authored as
one of the owner's identities:

- `Ben Barker <benz.benbarker@gmail.com>`  (default)
- `Ben Barker <bbarker@barkbite.org>`

At the start of every session, **before committing anything**, set the identity
(the ephemeral container otherwise defaults to the wrong one):

    git config user.name  "Ben Barker"
    git config user.email "benz.benbarker@gmail.com"

NEVER author a commit as `Claude <noreply@anthropic.com>` or any other identity.
After committing, verify with `git log -1 --format='%an <%ae>'`. This applies to
every branch and every session, without exception.

## CI & cross-platform testing — hard-won lessons

- **CI is manual-only.** `.github/workflows/ci.yml` triggers on `workflow_dispatch`
  only (to respect the Actions budget), building + testing on **ubuntu-latest AND
  windows-latest** with `RUSTFLAGS: "-D warnings"`. No CI runs automatically on
  push/PR — trigger it explicitly after pushing (`actions_run_trigger` / the
  Actions tab) when a change touches anything platform-sensitive. A green local
  `cargo test` does **not** mean CI is green: Windows is a separate target and the
  `-D warnings` flag turns any warning into a failure.
- **This sandbox is Linux only — Windows behavior cannot be *faithfully*
  reproduced locally.** So for a Windows-specific failure, do **not** guess magic
  numbers and burn CI rounds nudging the same knob. Re-derive from each run's
  evidence, and prefer a fix that *removes the platform dependency* over one that
  happens to land on the right value for one platform.
- **But Windows can now be *smoke-tested* from Linux via Wine** (added v0.0.19+).
  `.cargo/config.toml` wires the mingw-w64 linker for the `x86_64-pc-windows-gnu`
  target; `scripts/win-test.sh [--release] [--gui]` cross-builds the `.exe`,
  asserts its PE subsystem, runs the headless `render` path under Wine, and with
  `--gui` screenshots the actual browser window on an Xvfb display
  (`scripts/win-gui-shot.sh`). Use it to catch gross Windows regressions (does it
  build / run / render / open a window, is the subsystem right) **before** paying
  for a CI round. Caveat: Wine's software GDI + font stack are *close but not
  byte-identical* to real Windows, so this is for build/layout/"does it render"
  checks — **not** pixel-parity judgements, which still need the real binary.
  Requires `gcc-mingw-w64-x86-64 wine64 xvfb imagemagick x11-utils`.
- **Debug builds have much larger stack frames than release** (especially MSVC on
  Windows — unoptimized frames can be many KiB each). A test that exercises deep
  recursion or raw stack limits can pass on Linux and overflow on Windows debug
  while the shipped **release** build is fine. Don't retune a global *production*
  limit to satisfy a debug-only test — that erodes real safety margins. Instead
  make the test deterministic: run it on an explicit large-stack thread
  (`std::thread::Builder::stack_size(...)`) with a per-instance config override,
  so it validates the mechanism independent of build profile or default thread
  stack. See `cerberus-js-quickjs` `deep_but_bounded_recursion_still_works` /
  `with_limits` for the pattern.
- **When a CI fix doesn't work on the first try, re-diagnose — don't nudge.** The
  new failure output is fresh evidence; read it and form a new hypothesis rather
  than assuming the previous approach just needed a bigger number.

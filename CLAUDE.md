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
- **This sandbox is Linux only — Windows behavior cannot be reproduced locally.**
  So for a Windows-specific failure, do **not** guess magic numbers and burn CI
  rounds nudging the same knob. Re-derive from each run's evidence, and prefer a
  fix that *removes the platform dependency* over one that happens to land on the
  right value for one platform.
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

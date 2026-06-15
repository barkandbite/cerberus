# ADR-0017: Concurrent multi-window mirror groups (master-driven, ≤1 live engine)

- Status: Accepted
- Date: 2026-06-15
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0001 (trait boundaries), ADR-0006 (per-instance sessions/cache),
  ADR-0012 (persistent realm), ADR-0013 (bounded event loop), ADR-0014 (fetch
  over the eval seam), ADR-0016 (shared-memory cache),
  `docs/ideas/multi-window-mirroring.md`, #6

## Context

The owner's use case: open one site in several windows at once and drive them
all from a single **master** window — every navigation, scroll, click, and
keystroke mirrored onto each **follower** — while each window is a *separate
sealed session* (its own cookies/storage/identity/proxy/UA). Multi-account,
side by side, in lockstep.

Two foundations are already in place. ADR-0006 seals each session per
`InstanceId` (cookies/storage/identity isolated; cache hit/miss per-instance so
there is no cross-identity timing leak). ADR-0016 then interns cached *body
bytes* in a content-addressed pool, so N windows of the same site share one copy
of each resource instead of N. What is missing is **concurrency**: presenting
and driving N instances *at the same time*.

The obstacle is PLAN §1's prime directive: **at most one live JS engine**. N
windows each holding a live realm would be N engines — a direct violation, and
N× the per-engine RAM that the whole project exists to avoid. So "concurrent"
here cannot mean "N live engines."

## Decision

Introduce **mirror groups**, built from three pieces: a **semantic action
log**, an **`Instance`** abstraction, and a **≤1-live-engine catch-up
scheduler**.

1. **Semantic action log.** The master records user intent as portable
   `Action`s — `Navigate(Url)`, `Click { target }`, `Input { target, text }`,
   `Scroll { pos }`, `Submit { target }` — where `target` is a *stable,
   re-resolvable descriptor* (element `id`; else a structural selector path;
   else role + text), **never** a pixel coordinate or a live realm node handle.
   Targets are resolved **per window, against that window's own DOM, at apply
   time**, so they survive the legitimate divergence between sessions (a
   logged-in vs. logged-out layout, an A/B variant). The log is append-only with
   a monotonic per-instance cursor.

2. **`Instance`.** Extract today's single-session state out of the application
   into a reusable unit: identity (`InstanceId`), the sealed session
   (cookies/storage/proxy/UA), the current URL, the render state
   (document → styled → layout), and `realm: Option<…>`. The existing
   navigate/input/render logic operates on one `Instance`. The app moves from
   "one instance + head-switching" to "a set of instances."

3. **≤1-live-engine catch-up scheduler.** At most one instance — the **focused**
   one — owns the single live realm. Background followers are **dormant**: each
   is just *(its action-log cursor + a serialized DOM snapshot)* and holds **no
   engine**. Focusing a follower (a) serializes and tears down the currently
   live realm, (b) instantiates the realm for the newly focused instance, and
   (c) **fast-forwards** it from its cursor through the action log — replaying
   `Navigate`/`Submit`/`Click`/`Input` in *its own* session — until it converges
   with the master. This is the macro/catch-up model: logically all N windows
   track the master; physically only one runs JS at any instant. The invariant
   `live_realms ≤ 1` is asserted in the scheduler and enforced by tests.

Each instance keeps its **own sealed session** (ADR-0006) and they **share
interned cache bytes** (ADR-0016). Sessions never merge; only immutable bytes
are shared.

**Divergence.** When a follower cannot resolve an action's `target` (logged out,
captcha, a different variant), the scheduler marks that instance **diverged**
rather than guessing or fabricating input; the window surfaces the divergence
for manual attention and stops auto-applying until re-synced. Faithfulness beats
forced lockstep.

**Shell.** `cerberus-shell-winit` extends to a multi-surface model: N OS
windows, one per instance, exactly one focused. Non-focused windows present
their last serialized DOM and redraw on focus (after catch-up). This is the only
part that requires a display.

## Consequences

- **Easier:** the owner's mirror workflow becomes possible within the memory
  budget — N windows, N sealed sessions, but **≤1 engine and one interned
  cache**. The `Instance` extraction also generalizes the existing
  head-switching cleanly.
- **Harder:** a focus change now costs a serialize + realm rebuild + catch-up
  replay, bounded by the action log (mitigated by DOM snapshotting, by capping
  log length, and by coalescing consecutive scrolls). The action log must keep
  targets stable to avoid mis-replay.
- **Reversible:** the group controller sits *above* the per-`Instance` core; a
  single-window build simply never constructs a group. The `Action`/log types
  are additive.
- **Honesty:** the *model* (actions, instances, group, catch-up, session
  isolation, the ≤1-engine invariant) is fully headless-testable and gated in
  CI. The *visual* multi-OS-window layer is compile-verified where no display
  exists.

## Alternatives considered

- **N live engines (one per window).** Truest concurrency, trivial mirroring (no
  catch-up). Rejected: violates ≤1-engine and multiplies RAM — exactly what
  PLAN §1 forbids.
- **Pixel/coordinate replay.** Record clicks as `(x, y)` and replay verbatim.
  Rejected: breaks the moment two sessions diverge in layout; not faithful.
- **One shared DOM mirrored read-only to N windows.** One engine, N dumb views.
  Rejected: defeats the purpose — each window must be its *own session* (own
  cookies, own JS state, own server-rendered HTML), not a copy of one.
- **Eager catch-up (keep every follower continuously converged).** Lower focus
  latency, but requires running JS for background windows — back to N engines.
  Rejected; lazy catch-up *on focus* preserves the invariant.

## Phasing (each phase gated before the next)

1. `Action` + target descriptor + append-only log + per-DOM target resolver —
   unit-tested.
2. `Instance` extraction from the app (no behavior change) — existing tests stay
   green.
3. `MirrorGroup` controller + record/broadcast + the ≤1-engine catch-up
   scheduler — headless integration tests (convergence, session isolation,
   `live_realms ≤ 1`, divergence flag).
4. `cerberus-shell-winit` multi-surface (N windows) — compile-verified.
5. Divergence UX + log coalescing/caps + polish.

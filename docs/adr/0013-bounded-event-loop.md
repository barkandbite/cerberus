# ADR-0013: Bounded virtual-clock event loop (evolves ADR-0002 speed-first)

- Status: Accepted
- Date: 2026-06-13
- Deciders: bbarker@barkbite.org (directed), engineering

## Context

ADR-0002's speed-first prelude neutralizes programmed delays by running timer
callbacks **synchronously at call time**: `setTimeout`/`setInterval` invoke the
callback immediately (interval once), `requestAnimationFrame`/`requestIdleCallback`
fire at once, and `queueMicrotask` runs synchronously. Promise reactions, by
contrast, already use QuickJS's real job queue, drained by `eval`'s post-pump.

This was right for one-shot static rendering but is wrong for SPAs (M12):

1. **Ordering is incorrect.** The spec runs *all* pending microtasks between
   macrotasks, and `setTimeout(0)` only after the current task and its
   microtasks. Firing timers — and `queueMicrotask` — synchronously at call time
   reorders work relative to Promises, observable to app code that sequences via
   `setTimeout(0)` / microtasks (router flushes, batched state updates).
2. **No termination guarantee.** `setInterval` was special-cased to fire *once*
   precisely because a real repeat hangs the single-threaded engine — but a
   self-rescheduling `setTimeout` (`function t(){…; setTimeout(t,0);}`) still
   loops forever under synchronous firing.

M12's persistent realm (ADR-0012) keeps timers and closures alive across
interactions, so a correct **and terminating** loop is now required.

## Decision

**Replace synchronous firing with a queue plus a host-driven, bounded
virtual-clock loop — still entirely over the eval-only `JsEngine` seam.**

In the speed-first prelude (`cerberus-js-quickjs`):

- A realm-global **virtual clock** (`now`, starts 0) and a **task queue**.
- `setTimeout(fn, delay, …args)` / `setInterval(fn, delay, …)` **enqueue** a task
  due at `now + max(delay, 0)` (intervals clamp the period to ≥1 virtual ms so the
  clock always advances); `requestAnimationFrame`/`requestIdleCallback` enqueue
  near-term tasks. `clear*`/`cancel*` remove by id. Nothing fires at call time.
- `queueMicrotask(fn)` becomes a real microtask (`Promise.resolve().then`), so it
  orders correctly against Promise reactions (both drain via the job queue).
- `__cerberusStepTimer(maxClock)` runs **one** task: pick the earliest-due,
  non-cancelled task with `due ≤ maxClock`; if none, return `0`; else advance the
  clock to its `due`, re-arm it if it is an interval, run its (guarded) callback,
  and return `1`.

In the orchestration layer (`cerberus-js-dom`):

- `run_event_loop(engine, realm, budget) -> EventLoopStats` drives the loop
  **entirely through `eval`**: it repeatedly evaluates `__cerberusStepTimer` —
  and each `eval` already drains the microtask (job) queue afterward, so
  microtasks run **between** macrotasks, in order — until the stepper reports no
  due task or a cap trips.
- `EventLoopBudget { max_tasks, max_virtual_ms }` bounds termination on **two**
  axes: `max_tasks` caps total macrotasks (stops 0-delay `setTimeout` recursion,
  whose virtual clock never advances), and `max_virtual_ms` caps virtual time
  (stops `setInterval`, whose clock advances each tick). Defaults:
  `max_tasks = 10_000`, `max_virtual_ms = 60_000`.
- `run_page_scripts` and `dispatch_event` run the loop (after `fire_load` / after
  the dispatch) before serializing, so timer and async work that a page or an
  event handler scheduled is reflected in the rebuilt DOM.

The **engine seam does not change** — no trait method is added; the loop is pure
`eval` orchestration, preserving ADR-0002/0008 swappability. `NullJsEngine`
returns `Undefined` from the stepper eval, so the loop is a no-op there.

## Consequences

- **Easier:** correct sync → microtask → macrotask ordering, so SPAs that
  sequence via `setTimeout(0)` / microtasks behave; every page is **guaranteed to
  terminate** under the caps; `setInterval` is no longer a one-shot hack — it
  ticks until the virtual-clock budget, both more correct and still bounded.
- **Harder:** firing is no longer "instant at call" — callers must run the loop
  (folded into `run_page_scripts` / `dispatch_event`, and the app's run path). A
  pathological page burns up to `max_tasks` steps; each step is one `eval`, so the
  cost is bounded and visible in the timing HUD.
- **Reversible:** the prelude shims and the driver are isolated; reverting to
  synchronous firing is a prelude swap, and the budget is one struct.

## Alternatives considered

- **Keep synchronous firing.** Simple and fast, but wrong ordering and no
  termination guarantee for self-rescheduling timers. Rejected for SPAs.
- **Drive the loop inside the engine (a `run_event_loop` trait method).** Lets the
  adapter interleave `execute_pending_job` and macrotasks with no per-step `eval`,
  and is arguably the honest home for an event loop. Rejected *for now* to keep
  the seam eval-only and swappable (consistent with ADR-0008); revisit if the
  per-step `eval` cost shows up in profiling.
- **Real wall-clock timers / an async runtime.** Faithful, but reintroduces the
  very delays speed-first removes and needs threads/a reactor — against the
  product directive and the single-threaded model. Rejected.
- **A CPU watchdog (interrupt handler) instead of task caps.** Bounds a single
  runaway *synchronous* callback — an orthogonal concern; the task/clock caps
  bound the *loop*. Complementary, not a substitute; noted as a future addition.

# ADR-0026: Mirror & layout efficiency — scratch reuse, refocus skip, large-N gate

- Status: Accepted
- Date: 2026-06-17
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0023 (flex/grid intrinsic measure), ADR-0017 (mirror groups), ADR-0015 (sysmem/mem-gate), #6

## Context

Memory is priority #1 and the mirror's "N can be thousands" model only holds if
per-render and per-focus costs stay flat. Two hot paths wasted work: flex/grid
intrinsic measurement allocated a throwaway layout context (with five output
buffers) **per item per render** (ADR-0023 flagged this cost), and the mirror
rebuilt an instance's realm — create realm + reload page + replay the whole log —
on **every** focus, even when re-focusing a window already converged to the head
of the log. There was also no gate guarding the large-N catch-up/memory behaviour.

## Decision

- **Layout intrinsic-measure scratch reuse (E1).** `Ctx` holds one reusable
  scratch sub-context; `measure_intrinsic_width` clears (does not drop) it between
  items, so a flex/grid page no longer allocates a context + five buffers per
  item. `commit_line` drains the per-line buffer through a moved-out `Vec` so its
  capacity persists across lines. Output is byte-identical (all layout tests
  unchanged); this also speeds every mirror render.
- **Converged-snapshot refocus skip (E2).** A window renders from its serialized
  snapshot, not its realm, so a resident instance already at the head of the log
  needs no work to be *viewed*: `focus` drops any live realm and just marks it
  focused — no realm rebuild, no page reload. Driving still needs a populated
  realm, so `act` routes through a new `ensure_live` that rebuilds on demand. A
  `released` flag distinguishes a dropped snapshot (must rebuild) from a resident
  one; `release_dormant` now spares the focused window (it may hold no realm yet
  still be rendered). The ≤1-live-realm invariant is unchanged.
- **Large-N gate (E3).** `mirror-bench` (and `mirror_bench(n)`) builds a group of
  N sealed instances, sweeps focus across all of them cold (each rebuilds) then
  warm (each reuses its snapshot), and reads resident memory after
  `release_dormant`. It sits beside `mem-gate`/`bench` for CI.

## Consequences

- **Measured:** at N=256 the warm focus sweep is ~0.2 ms versus ~2.3 s cold (the
  refocus skip), and resident memory after releasing dormant snapshots is ~12 MB —
  i.e. catch-up cost for re-viewing is ~free and memory is bounded by the live
  document, not by N.
- **Realm teardown is fundamental.** Because defocus destroys the live realm (the
  ≤1 invariant), true *incremental* JS replay across focus is impossible — the JS
  heap is gone. The sound win is reusing the already-converged **snapshot** for
  viewing and rebuilding the realm only when an instance is actually driven; this
  ADR records that constraint so it is not re-litigated.
- **Reversible:** E1 is a pure allocation optimization behind `measure_intrinsic_width`;
  E2 is isolated to `focus`/`ensure_live`/`release_dormant`.

## Alternatives considered

- **Keep multiple realms warm to avoid rebuilds.** Violates the ≤1-live-engine
  prime directive (PLAN §1); rejected.
- **A flat layout arena for sub-contexts.** A larger rewrite; deferred — the
  scratch reuse captured the dominant allocation without changing semantics.

# Idea: multi-window mirrored control (synchronized multi-identity browsing)

> **Status: parked / not scheduled.** Captured from an owner idea (2026-06-13).
> A product/architecture sketch, *not* a decision (no ADR yet). Natural earliest
> start is **after M12** (events + bounded loop + fetch), because it is built
> directly on top of the event-dispatch and a new action-record layer.

## The concept

One **master** window that the user actually drives, plus **N mirror** windows
all showing the *same site*. Every navigation, scroll, click, and keystroke the
user performs in the master is **replicated** to every mirror. The windows are
identical in *behavior* but differ in **identity**: each has its own sealed
cookies, storage, proxy/egress, and farbling seed.

The payoff: **operate many accounts on one site at once** — fill a form, place an
order, post, or check N dashboards — by acting **once** in the master and having
every persona follow. It is the natural product expression of Cerberus's
differentiator (the three sealed identities), generalized from 3 fixed heads to
**N concurrent, mirrored instances**.

## Why it fits Cerberus specifically

- **Identity isolation already exists.** `cerberus-identity` heads + the sealed
  per-instance cookie store, per-head farbling seed, and per-instance
  proxy/egress are exactly the "different cookies/data/proxy per window" this
  needs. A mirror group is "N heads that share an action stream."
- **Event dispatch already exists (M12b).** Replaying an action in a mirror =
  dispatching the same DOM event into that window's realm via `dispatch_event`,
  using the JS-id↔`NodeId` correlation (`RebuiltDom.id_map`). The replay
  primitive is the one we just built.
- **The bounded event loop gives convergence points (M12c).** After each replayed
  action we already drain timers/microtasks under caps and re-serialize — a
  deterministic "settled" checkpoint to compare/advance windows against.

## The hard part, and the elegant reconciliation

**Prime-directive tension (PLAN §1): one JS engine instance live at a time.** N
windows each with a live realm would be N engines — a direct violation, and an
RSS blowout. This is the make-or-break constraint.

The owner's **macro / catch-up** idea *is* the reconciliation, not just a lag
fallback:

- The master records an **ordered action log** (a macro): semantic actions, not
  pixels — `navigate(url)`, `click(target)`, `input(target, value)`,
  `scroll(pos)`, `submit(form)`, with the settle checkpoint after each.
- A mirror does **not** need a live engine to "keep up." It stores its position
  in the log. It only **instantiates a realm and fast-forwards** through the log
  to convergence **when it is brought forward** (focused / surfaced / queried).
- So at most one (or a small, bounded pool of) realm is live at any instant; the
  other windows are just **a stored log position + their sealed identity**. That
  keeps the ≤1-engine invariant intact while N windows *logically* track the
  master. The lag is a feature: backgrounded personas are cheap.

This makes the catch-up model the core design, with optional **live mirroring**
(broadcast each action immediately) as a latency optimization only for the few
windows currently surfaced.

## Action model (semantic, not coordinate)

Replay must survive **per-identity DOM divergence** (different accounts see
different content). So actions address targets **semantically**, resolved
per-window at replay time:

- Prefer a **stable target descriptor** (id / a CSS-selector path / a role+text
  anchor) over raw coordinates, resolved against each mirror's own DOM.
- Each action carries its **settle checkpoint**; a mirror replays up to the
  checkpoint, draining the bounded loop, before the next action.
- **Divergence handling:** if a target doesn't resolve in a mirror (button
  absent, logged-out, captcha, A/B variant), the action **fails soft** — flag
  that window as *diverged* and surface it for manual attention rather than
  guessing. Never fabricate a click.

## Open questions / risks

- **Target addressing** robust across divergent DOMs (the central problem).
- **Async divergence:** `fetch`/XHR (M12d) resolve at different times per
  identity/proxy; the per-action settle checkpoint bounds this but cross-window
  timing still needs care.
- **Per-identity friction:** captchas, step-up auth, rate limits, anti-bot — each
  persona may hit different walls; the diverged-window flag is the escape hatch.
- **Realm pool sizing** vs. the memory budget: how many live realms (1? a small
  LRU?) before catch-up kicks in; gate with `mem-gate`.
- **Window/tab model:** today the shell is deliberately single-window, no tabs
  (PLAN §10). This needs a multi-surface shell — a `PlatformSurface` extension,
  its own decision.
- **Ethics / authorized use:** simultaneous multi-account automation can breach
  site terms. Cerberus is privacy tooling for legitimate multi-persona use (work
  vs. personal vs. throwaway), **not** abuse/bot tooling; framing and guardrails
  would need an explicit stance before this ships.

## Dependencies / sequencing

1. **M12** events + bounded loop + fetch (the replay + settle + async substrate). ✅/⏳
2. An **action-record/replay** layer (record macro on master; resolve+dispatch on
   a mirror) — a thin layer over `dispatch_event` + a semantic target resolver.
3. A **multi-surface shell** (multiple windows) + a mirror-group controller.
4. A **realm pool + catch-up scheduler** honoring the ≤1-ish-engine budget.

Revisit after M12 lands; promote to an ADR if/when scheduled.

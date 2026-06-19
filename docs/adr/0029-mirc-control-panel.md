# ADR-0029: MIRC control panel — SYNC count badge + roster/orchestrator overlay

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

MIRC (Multi-Identity Remote Control, ADR-0028) is the headline feature: one
master window drives many sealed sessions, each with its own identity/account, so
a single action fans out and each follower acts in **its own** account. The use
case driving it is automating legitimate, repetitive work on the owner's own
authorized systems (e.g. clearing a large internal claims backlog) while keeping
each identity privacy-compartmentalized.

The mirror **engine** already exists (`cerberus-mirror`: a semantic action log,
the ≤1-live-realm invariant, lazy per-focus catch-up, per-identity sealed
sessions and autofill). What was missing is the **front of house**: the owner
specified a single master window plus a status panel — *not* a wall of follower
OS windows. The SYNC button (ADR-0028 placeholder) shipped first as a broadcast
*toggle*; the owner corrected the model: the button should be a **count badge**
that **opens a control panel**, and selecting a session there is what **lazily
renders** it.

## Decision

Phase 2a builds the panel as a **rendered prototype** so the design can be
reacted to before the full orchestration seam is wired. No new third-party deps;
the panel is a pure `cerberus-ui` component like `CookieManager`/`ConsentBanner`.

### SYNC button = count-badge that opens the panel
- `ToolbarAction::ToggleSync` → **`ToolbarAction::OpenSync`**. The button no
  longer toggles broadcast; clicking it opens the MIRC panel.
- `Toolbar` gains `sync_count: usize`, drawn as a small notification-style count
  badge on the button corner (`push_count_badge`, capped at `99+`). The blue
  broadcasting glow is retained as an at-a-glance "broadcast on" signal.
- Broadcast on/off **moves into the panel** (it is an orchestration concern, not
  a toolbar toggle).

### `MircPanel` — one reusable overlay (paint + hit-test only)
A pure component: `MircPanel::paint(window, shaper, broadcasting, site, rows,
scroll) -> DisplayList` and `MircPanel::hit_test(...) -> MircAction`. The app owns
the data and applies actions. Surfaced data per row (`MircRow`): identity
`label`, `account` (the session's login on the site, or a sealed-session tag),
`state` (`MircState::{Live, Dormant, Diverged}` — mirroring the engine's
≤1-live/dormant/divergence model), and `logged_in`. The panel shows:
- a **control bar**: broadcast on/off, plus the bulk verbs `navigate all` /
  `login all` (present, stubbed until the live group seam);
- a **scrollable roster** (status dot · identity · account · state chip · login
  pill · **open**), where `open` is the lazy "select → render" gesture;
- a legend and scroll affordances (ASCII `^`/`v`, per ADR-0028's glyph note).

### Single-window wiring (honest prototype)
Hosted in `BrowserApp` exactly like the cookie inspector: `mirc_open` /
`mirc_scroll`, painted in `render_frame`, owning clicks in `pointer_down`,
swallowing text input while open. The roster is built from the identities
(`HeadManager`): the active head reads **live**, the rest **dormant**;
`logged_in` is the **real** signal of whether that sealed session holds cookies
for the current site; `account` is the identity's stored autofill login when the
vault is unlocked. `open` surfaces the chosen identity via the existing head
switch (a stand-in for focusing a live `MirrorGroup` instance); broadcast and
scroll are wired; the bulk verbs acknowledge with a status note.

A `cargo run -p cerberus-app --example mirc_preview` renders the panel (and the
SYNC badge) to PNG so the UI can be reviewed headlessly.

## Consequences

- **Easier:** the owner's "master + status panel only" model is now concrete and
  reviewable; the panel is the single seam where multi-identity orchestration
  (broadcast scope, divergence desk, per-identity credential editor, pacing,
  per-identity proxy/farbling for privacy) will attach; reusing the
  `CookieManager` shape keeps it consistent and pure (testable without a window).
- **Costs / deferred:** Phase 2a does not yet drive a live `MirrorGroup` from the
  single-window app, so `navigate all` / `login all` and true select-to-render
  across N live followers are stubbed; `open` maps to a head switch for now.
  These land when `BrowserApp` hosts a `MirrorGroup` (the next phase).

## Alternatives considered

- **A separate OS window per follower** (the cascade in the concept art):
  rejected by the owner in favor of one master + status panel — and it fights the
  memory-first, ≤1-live-realm model (dormant sessions must stay cheap).
- **Keep SYNC as a broadcast toggle + a separate badge:** rejected; the owner
  wants one button that *is* the count and opens the orchestrator.
- **A second settings sub-page instead of a modal overlay:** rejected for
  consistency — the cookie inspector overlay is the established pattern, and the
  panel needs to float over the live master page.

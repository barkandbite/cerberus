# ADR-0025: Multi-identity UX — drivable mirror typing + driven badge

- Status: Accepted
- Date: 2026-06-17
- Deciders: benz.benbarker@gmail.com (directed), engineering
- Related: ADR-0017/0018 (mirror groups), ADR-0024 (autofill integration), #6

## Context

Mirror groups (ADR-0017/0018) shipped clicks-only: `MirrorShell::text_input` was
a no-op, so a follower never received typed text — the mirror could not actually
*drive* a login. And the owner's headline indicator — "N profiles being driven on
xyz.com" — did not exist. This ADR records the UX increment that makes the mirror
fully drivable by keyboard and shows the driven count.

## Decision

- **Mirror text-input routing.** A click on a text field (`<input>` of a
  text-like type, or `<textarea>`) captures it as the master's typing focus,
  seeded with the field's current value. Each keystroke sends the *whole* field
  value as the existing `Action::Input { target, text }`. Form controls are not
  `ElementBox`es, so the shell also hit-tests the master's form-field boxes and
  maps a hit to its node via the existing `collect_controls` numbering — which
  also makes controls clickable in the mirror for the first time. (`Document::node`
  is added as a small reusable `NodeId → NodeRef` accessor.)
- **Keystroke coalescing.** `MirrorGroup::act` coalesces a run of consecutive
  same-target `Action::Input`s into a single log entry (`ActionLog::replace_last`):
  each keystroke carries the full value, so a follower converges in one replay and
  per-character typing does not bloat catch-up.
- **"N profiles being driven" badge.** `cerberus-ui::DrivenBadge` is a pure
  component (label/rect/paint/hit_test) rendering a small pill — e.g. "23 profiles
  being driven · github.com" — right-anchored on the **master** window only (the
  mirror has no toolbar). `MirrorShell` composites it after painting the page,
  taking the count from `driven_count()` and the site from the master's host
  (`driven_site()`). `hit_test` is exposed for the click-to-open-panel wiring.

## Consequences

- **Easier:** the mirror now drives real logins — type a username/password on the
  master and every sealed window fills it in its own session. The badge makes the
  "you are driving N profiles" state legible.
- **Cheap:** coalescing keeps the action log ~one entry per field regardless of
  keystroke count, so catch-up cost is unchanged by typing length.
- **Contained:** routing reuses the existing `Action::Input` and `collect_controls`;
  no new action variant, no new third-party dependency.

## Scope / limits (documented)

- The **identities panel** (click the badge → create profiles / toggle which
  identities drive this site), **per-site driven selection**, and the
  **single-window fill gesture** are follow-ups on these same seams (the badge
  `hit_test` and `FillProvider`/`Action::Fill` already exist).
- Mirror typing routes printable characters and backspace; Enter/Tab are not yet
  routed (no submit-on-Enter in the mirror).

## Alternatives considered

- **A full toolbar on the master.** Rejected — the mirror has no URL/back/forward
  semantics; a focused overlay badge matches the owner's ask without dead chrome.
- **One `Action::Input` per keystroke.** Rejected — bloats the log and every
  follower's catch-up; coalescing is behaviour-identical and far cheaper.

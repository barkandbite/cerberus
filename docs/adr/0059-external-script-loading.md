# ADR-0059: External `<script src>` loading

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

We executed only **inline** `<script>` bodies; external `<script src="…">` was
never fetched or run. Client-rendered pages ship almost all their code as external
bundles, so they couldn't execute any of it — Pokémon Center and Target rendered
blank, and progressive-enhancement on server-rendered sites never ran.

## Decision

Capture and run external scripts in document order, gated by consent.

- **DOM**: scripts are collected as `ScriptRef::Inline(body)` /
  `ScriptRef::External(src)` (a typed enum, in document order) instead of
  inline-only `String`s.
- **Resolution**: `cerberus_js_dom::resolve_scripts(scripts, fetch)` flattens the
  list to runnable source — inline bodies as-is, external `src`s fetched via the
  callback (skipped on block/failure).
- **Headless render**: the fetch callback resolves each `src` against the page URL
  and fetches it through the **consent gate** (scripts are subresources — same
  privacy model as stylesheets/images), then runs the bodies via the existing
  `run_page_scripts_with_fetch` (which already gives JS a working `fetch`/XHR).

## Consequences

- External scripts now run: Wikipedia 4→5, Pokémon Center 1→2 scripts executed,
  with **no regression** to the working renders. This is the prerequisite for any
  JS-driven content and helps every progressive-enhancement site.
- **Not sufficient for modern SPAs.** Empirically, running the initial external
  scripts does **not** render Pokémon Center: Next.js/webpack apps load their real
  code as **dynamic chunks** injected at runtime (`createElement('script')` +
  `appendChild`), which we don't yet fetch+run, plus a data layer and many browser
  APIs (IntersectionObserver, matchMedia, history, …). Those are the next, much
  larger layers — documented here so the ceiling is explicit.
- The **interactive** browser still runs inline-only (its fetch is async on a
  worker); wiring external fetch there is a follow-up. Running all external
  scripts also costs load time (Wikipedia 3.6s→8s) — a later optimization
  (skip/defer non-essential scripts).

## Alternatives considered

- **Keep inline-only:** leaves every client-rendered page blank — not viable for
  the goal.
- **Run external scripts before all inline (simpler ordering):** breaks pages where
  an inline script depends on an earlier external one; the `ScriptRef` list
  preserves true document order.

# ADR-0061: Skip external scripts on content-complete (SSR) pages

- Status: Accepted
- Date: 2026-06-24
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Running external `<script src>` + dynamic chunks (ADR-0059/0060) is what makes
client-rendered pages appear, but it's pure latency on pages that already arrive
**content-complete from the server**: Wikipedia's article is in the HTML, so
fetching + running its ResourceLoader modules (~2s) changed nothing visible. The
goal asks for rendering that is also **fast**.

## Decision

Gate external-script execution on whether the page actually needs JS to build its
content: measure the page's rendered (non-whitespace) **visible text before any
script runs**, and run external scripts only when it's below a small threshold
(`< 800` chars) — i.e. a near-empty shell that the server expects the client to
fill in. Inline scripts always run.

- Server-rendered, text-complete pages (Wikipedia, GitHub-marketing) skip their
  bundles → fast.
- Client-rendered shells (Target's data-blob + empty mount; Pokémon's 1KB) fall
  below the threshold → their JS runs and builds the page. (Target's deal cards
  live in a JSON blob, not the rendered DOM, so its visible text is small and JS
  correctly runs.)

## Consequences

- Content-complete SSR pages no longer pay for bundles that don't change their
  render; shells still get their app code. Combined with the JS budget (ADR-0060),
  pages render without hanging and without needless JS latency.
- Heuristic, not exact: a page that is *both* text-rich SSR *and* depends on JS for
  critical extra content would have its external JS skipped. Acceptable for the
  common case; a page could be re-run with JS forced if needed (a future flag).
- The remaining per-page time on heavy commercial sites is dominated by **network**
  (large CSS + dozens of images), not the engine (bench ~30 ms; cascade ~8×) — an
  orthogonal concern (caching, concurrency, fewer image decodes).

## Alternatives considered

- **Always run external JS:** simplest, but slow on every SSR page for no visual
  gain (and risks long chunk drains).
- **Never run external JS:** fast, but leaves client-rendered shells (Target)
  blank — defeats the multi-site goal.
- **Per-site allow/deny list:** brittle; the content-presence signal generalizes.

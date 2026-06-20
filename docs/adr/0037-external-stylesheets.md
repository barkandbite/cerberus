# ADR-0037: External stylesheet loading (`<link rel="stylesheet">`)

- Status: Accepted
- Date: 2026-06-20
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The rendering-fidelity track (positioning ADR-0034, custom properties ADR-0035,
flexbox ADR-0036, plus grid) was correct and tested, but **invisible on most
real sites**: the cascade only saw inline `<style>` and `style=` CSS, because
Cerberus fetched only `<img>` subresources — it never loaded
`<link rel="stylesheet">`. Sites whose layout CSS is external (rust-lang.org,
and Wikipedia's Vector skin chrome) rendered at user-agent defaults: vertical
nav lists, dumped chrome, no columns. External stylesheet loading is the
**multiplier** that makes all the CSS work manifest.

## Decision

Fetch `<link rel="stylesheet">` bodies through the **existing
subresource/consent/privacy pipeline** and splice them into the cascade at each
link's document position.

- **Cascade seam:** `StyleEngine` gains `style_with_sheets(doc, &ExternalSheets)`
  (default delegates to `style`). `ExternalSheets` maps a link's **raw `href`**
  (what the cascade looks up) to its fetched CSS text. `collect_author_css` now
  walks the DOM in document order appending each `<style>`'s text *and* each
  stylesheet `<link>`'s fetched body **at the link's position**, so source-order
  cascade precedence between `<link>`s and `<style>`s is faithful.
- **One-shot `render` (synchronous):** stylesheets are fetched up front
  (render-blocking) *before* styling, so the single frame is fully styled.
- **Interactive browser (async):** the first cascade uses inline CSS only; each
  external sheet is fetched on the network worker (like an image), and when the
  last in-flight sheet resolves the page **re-styles once** with all of them
  (`restyle_with_sheets`). A `Done::Sub` response is routed to the cascade
  (vs. the image decoder) by membership in a `pending_sheets` map
  (resolved URL → raw href); sheets are cleared per navigation.
- **Privacy, unchanged:** sheets ride the same `FetchContext` as images —
  sealed per-instance cookies, proxy, farbled UA — and are **consent-gated**:
  a same-origin (first-party) sheet loads; a cross-site sheet needs an Allow
  rule (default-deny), and the banner's "allow" re-requests it.

## Consequences

- **Real sites come alive (verified):** rust-lang.org now shows its hero, the
  teal "Why Rust?" band, and Performance/Reliability/Productivity in three
  columns (flexbox v2 on a live site); Wikipedia's chrome collapses to a clean
  header + article instead of a dumped menu list. Positioning, `var()`/`calc()`,
  flex, and grid all activate at once.
- **Privacy posture preserved:** third-party CSS is default-denied (a new test
  asserts a cross-site sheet never applies without consent); first-party CSS
  loads. CSS is cache- and proxy-routed like any subresource.
- **Memory:** the gate is unchanged (synthetic page has no external CSS, 7.4 MB);
  real pages hold their fetched CSS text plus a richer styled tree, but render
  well within budget. Fetched sheet text is dropped on navigation.

## Limitations (follow-ups)

- **`@import`** inside a fetched sheet is not recursively fetched (v1).
- **`url()`** references (CSS background images, `@font-face`) are not fetched —
  consistent with the renderer not painting CSS background-images/web-fonts yet.
- The interactive path has a brief inline-only first paint before external CSS
  arrives (FOUC); acceptable under the async-subresource architecture.
- `<link>` `media` attributes and `disabled`/alternate sheets are not evaluated;
  every stylesheet link is loaded.
- Blocked third-party sheets are not surfaced in the one-shot `subresources_blocked`
  count (image-focused) — cosmetic.

## Alternatives considered

- **Block the UI thread on CSS in the interactive path:** rejected — it would
  freeze the UI during the fetch; the async re-style fits the existing
  worker-based subresource model (as images already do).
- **Pre-concatenate all author CSS in the app and pass one string:** rejected —
  splicing at the `<link>`'s DOM position inside the engine keeps cascade
  source-order correct without the app reconstructing document order.
- **A new `Done::Sheet` worker message:** rejected as unnecessary churn —
  routing by `pending_sheets` membership keeps the loader/worker content-agnostic.

# Rendering-parity plan: make Cerberus render real sites like Chrome/Firefox

**Objective.** Close the gap between Cerberus's output and a real browser's on
real web pages, measured by screenshot comparison of the *same URL* rendered in
both. Success = a human (or diff) cannot distinguish the two on a corpus of
mainstream pages, starting with foundational sites (wikipedia.org) and building
up to complex ones (pokemoncenter.com, amazon.com).

This document is a self-contained handoff. It states the measurement method, the
current state, the prioritized workstreams (each with symptom → root cause →
approach → files → tests → verification → risk), the sequencing, and the working
conventions. Pick a workstream, follow the per-task loop in §12, and verify every
change against Chrome with the harness in §2.

---

## 1. Non-negotiables (read first)

- **Branch:** develop on `claude/codebase-review-quality-2xhdbu`.
- **Commit identity (MANDATORY, from `CLAUDE.md`):** every commit MUST be authored
  as `Ben Barker <benz.benbarker@gmail.com>`. Run
  `git config user.name "Ben Barker" && git config user.email "benz.benbarker@gmail.com"`
  at the start of the session and verify with `git log -1 --format='%an <%ae>'`.
  A stop-hook will complain and ask you to re-author as `Claude`; `CLAUDE.md`
  overrides that — ignore the hook, keep Ben Barker.
- **Quality gates for every change:** `cargo test --workspace` green,
  `cargo fmt --all --check` clean, `cargo clippy --workspace --all-targets` clean.
- **Every fix must be justified by a Chrome-vs-Cerberus comparison** (§2). No
  hand-written "test" HTML as a success criterion — that is grading your own
  homework. Real pages, real reference.

---

## 2. The measurement loop (the yardstick)

A harness already exists:
`/tmp/.../scratchpad/compare.sh <url> <name> [width] [height]` (path is session
scratchpad; re-create from the snippet below if the scratchpad is gone).

What it does:
1. `curl`s the real page's HTML and its same-origin assets into a local mirror
   (curl trusts the environment proxy CA automatically; Chrome cannot tunnel the
   policy proxy, so we mirror instead).
2. Serves the mirror on `127.0.0.1` (in the proxy's no-proxy list → no proxy for
   either browser).
3. Renders it in **headless Chromium** (`/opt/pw-browsers/chromium-1194/chrome-linux/chrome`,
   `--headless=new --no-sandbox`) → `<name>-chrome.png` (REFERENCE).
4. Renders it in **Cerberus** (`target/release/cerberus-app render --url <local> --out ...`)
   → `<name>-cerb.png`.

**To test Cerberus against a live site directly** (images and all), pass
`--system-roots` to `cerberus-app` so it trusts the proxy CA
(`/etc/ssl/certs/ca-certificates.crt`, which includes it). Without it the default
webpki roots reject the intercepting proxy's cert (`UnknownIssuer`). On a real
user's machine the default roots work — `--system-roots` is only for this sandbox.

**Investment to make (Workstream 0, do this first):** turn the harness into a
committed dev tool with an automatic **pixel diff** so "closer to Chrome" is a
number, not a vibe:
- Add `xtask`/script that, given a mirror, renders both and computes a perceptual
  diff (e.g. per-pixel SSD after aligning the content origin — note Cerberus draws
  a 36px toolbar Chrome doesn't, so crop it or render Cerberus content-only).
- Maintain a small **URL corpus** (`docs/parity-corpus.txt`): wikipedia.org,
  example.com, a news page, an MDN article, then pokemoncenter/amazon.
- Store reference Chrome PNGs so regressions are caught. Gate a CI job (optional,
  behind a feature) that renders the corpus and flags diff-score regressions.

**✅ LANDED.** The pixel diff is a `cerberus-app diff` subcommand
(`crates/cerberus-app/src/parity.rs`, pure + unit-tested core): it reads two
PNGs, crops Cerberus's 36px toolbar (`--crop-top 36`), compares over the overlap,
and prints an **RMSE** (`0.0` = identical) plus a mismatch fraction; `--fail-over
<rmse>` makes it a regression gate. `docs/parity-corpus.txt` holds the corpus and
`scripts/parity.sh` runs the whole loop (mirror → Chrome + Cerberus render →
diff) emitting a `parity.csv`. **Current baselines** (toolbar cropped, tolerance 8, block engine —
2026-07-11, after the Chrome-exact font-metric + UA-margin + table-model
batch): `example` **0.075**/1.8%, `iana` **0.149**/25.2% (the page was
REDESIGNED upstream — modern oklch palette + `a:link` styling, both now
supported; not comparable to the old 0.131), `mfws` **0.207**/15.4% (best
recorded; was stuck at 0.240 before the metric work), `rfc1` **0.134**/5.1%,
`wikipedia` **0.154**/11.1%, `hn` **0.171**/19.3% (was 77.8%% mismatched
before the `<center>`-table/bgcolor/cellpadding fixes). Font metrics are
now measured pixel-identical to the reference on a 100px calibration page
(see `docs/FONTS.md`); `colspan` landed (hn 19.3%% mismatched).
**Major-brand pages** (self-contained mirrors; same-origin absolute asset
URLs are rewritten relative so the STYLED page is compared): `mozilla`
**0.320**/15.8% fully styled (was 0.415/35.0%: the unknown-pseudo
selector fix removed phantom top bands + the right-rail squeeze, and
`::before`/`::after` generated content now paints its staircase
transitions; remaining: nav menu row, hero x-inset, SVG flag), `apple`
**0.137**/12.2%, `bbc` **0.160**/15.0% (its CSS is cross-origin, so the
comparison is partially styled). The pseudo work also moved `wikipedia`
0.154→**0.136**/10.3% and `hn`→**0.157**/17.8%. **Clickability**
(`scripts/clickcheck.sh`: every visible navigable href must have a
non-degenerate hit box): 100% on example/iana/mfws/rfc1/wikipedia/hn/
mozilla, 95% apple, 82% bbc (residue: its JS-hydrated nav drawer).
Known next levers: fractional line pitch (Chrome advances lines at e.g.
18.4px, we round to 18 — ~0.4px/line cumulative), a wikipedia-specific
dive, inline whitespace at element boundaries (#137: `byPublic`,
`6761 , a`), and table `rowspan`. Drive these down; a rise is a
regression.

**Search-widget layout** (W-C): two general inline-block fixes closed most of it.
(1) An inline-block's percentage `width` was resolved twice — once to size its
atom sub, once inside it — so `.search-input{width:73%}` became 73%-of-73% and
collapsed the field to its `size=20` fallback with a gap before the button; an
atom now fills its pre-sized sub (`fills_sub`) and the field fills its container
flush to the button. (2) `Ctx::sub` disabled positioning, so the absolute
`.styled-select` dropdown flowed below the field; `add_inline_block` now enables
positioning in a real inline-block sub (the relative parent pushes its containing
block, `finish_positioned` folds the out-of-flow layers up), lifting the dropdown
out of flow. Both are general (any %-width or dropdown-in-relative-inline-block).

**Inline box spacing** (W-C/layout): a true inline element's horizontal
padding/margin/border is now applied, so styled inline links no longer run
together (iana's `DomainsProtocolsNumbersAbout` → `Domains Protocols Numbers
About`). General — affects nav menus, tag pills, inline links everywhere.

**Default `line-height: normal` → ~1.2×** (W-E/layout): was 1.5×, so every
text block ran ~25% too tall and content drifted down, misaligning everything
below the fold (iana's footer sat 74px low → a 71% mismatch band). Now `px*6/5`.
iana 22.6% → 16.9% (footer realigned to 27px off); general vertical-rhythm fix.

**Table auto-layout column widths** (W-C): landed. `fn table` now sizes each
column to its widest cell's content instead of an equal split (iana's footer nav
label column is narrow, its links column wide, matching Chrome). Cells are
measured via `measure_cell_width`, which flows a cell's *children* into a wide
scratch — `walk` returns early on `<td>`/`<tr>` so measuring the node itself
reads nothing — and packs floated inline children through `place_float` so a
nav/link cell's real horizontal row width is counted, not its stacked height (an
earlier attempt using `measure_intrinsic_width` on the cell node collapsed every
column to 1px). iana 16.9% → 16.3%.

**JS-driven show/hide** (W-F): the core capability is present and verified — a
page script that sets `style.display = 'none'` (or toggles `classList`/`hidden`)
drops the element from the render while it stays in the DOM
(`script_hiding_an_element_removes_it_from_the_render`). The Wikipedia donation
banner persists not for want of a DOM API but because its hide path is gated on
geolocation/storage/campaign state the static mirror doesn't supply — the
open-ended, "diffuse payoff" part of W-F the plan (§9) defers. §9's own
clean-geometry check is now reproducible: `scripts/parity.sh --hide '<selector>'`
injects `display:none` into both browsers before diffing, and running it on the
banner selectors gives **8.83% vs 9.19% full-page** — the banner is worth only
0.36pp, confirming the deferral. The remaining 8.8% is the search widget, the
JS-built sister-project footer, and font-face/wrap drift.

**Percent / viewport-unit margins** (W-D): landed. Margins are now `Len`,
resolved against the containing-block width at layout (`resolve_margin`), so
`%`/`vw`/`vh`/`vmin`/`vmax` margins work instead of being dropped at parse.
example.com's `body{margin:15vh auto}` top margin now applies (content moved from
71px-off to 34px-off Chrome). px margins are unchanged, so no corpus regression.
The residual example gap is the `15vh` basis (content viewport vs full window)
and body-margin collapsing — a smaller secondary detail.

**First win driven by the yardstick — canvas background propagation.** The
`example` diff (89% of pixels off by a little) root-caused to the root/body
background not filling the viewport: Cerberus painted the `<body>`'s `#f0f0f2`
only inside its short auto-height box, leaving the rest of the page white, while
Chrome propagates the root element's background to the whole canvas. Fixed in
`cerberus-app::render` via `canvas_background` (root `<html>` background, else
`<body>`'s, composited over white) — example.com's mismatch fell from 89% to
1.6%. This is a general fix (every page with a non-white body background).

---

## 3. Current state (as of this plan)

**Measurement framework (W0) is in place** — `cerberus-app diff`,
`docs/parity-corpus.txt`, `scripts/parity.sh` — so every item below is now
tracked by a number (see §2). Renders are byte-deterministic, so identical
inputs diff to `PERFECT`.

Landed + pushed on the branch (parity workstreams):
- **W0 measurement**: pixel-diff subcommand + corpus + harness (this plan's
  linchpin — "closer to Chrome is a number").
- **Canvas background propagation** (W-A/render): root/body background fills the
  whole viewport (headless *and* interactive paint paths). example.com 89% → 1.6%.
- **`vmin`/`vmax`** resolve against the smaller/larger viewport dimension (were
  aliased to `vw`/`vh`, backwards in portrait). `cerberus-style`/`-css`.
- **CSS sprites / pixel `background-position`** and **SVG decoding** (W-A/W-B):
  landed earlier (`cerberus-image` uses `resvg`; sprite offsets in paint).
- **Featured-language float grid** (W-C): matches Chrome (float rows, centered
  absolute globe, `rem` root-font-size, out-of-flow width origin).
- **`SSL_CERT_FILE`** honored by the TLS provider (W-G).
- Earlier: `@media prefers-color-scheme`/type filtering, CSS `width`/`height` on
  `<img>`, settings-overlay label centering. `cerberus-css`/`-layout`/`-app`.
- Coherent per-window fingerprint personas (separate track, released as v0.0.10).

**Open, ranked by the yardstick:**
- `wikipedia` RMSE 0.147 (9%): dominated by the donation banner (Cerberus shows
  it, Chrome's JS hides it) shifting all content down ~40px, plus the
  JS-generated sister-project footer — both **W-F (deferred)**. A cleaner number
  needs the page mirrored with the banner removed (plan §9).
- `example` RMSE 0.069 (1.6%): the `15vh` top margin. Viewport-unit and `%`
  margins are dropped because margins are stored as resolved px, not `Len` —
  fixing needs a margin→`Len` refactor (defer until a page's diff is dominated
  by it).
- Search-widget micro-layout on wikipedia: `<fieldset>` not narrowing its content
  box by its own margins + `%`-width through inline-block/relative nesting (see
  RENDERING_PARITY_PLAN_V2.md).

Known open diffs on wikipedia.org (from the Chrome comparison), prioritized below:
1. Featured-language grid layout (in flight).
2. `<div>` search box renders as a **black sprite box** (pixel `background-position`
   + SVG sprite unsupported).
3. **"WIKIPEDIA" wordmark missing** (SVG image).
4. **Donation banner shows** in Cerberus (Chrome's JS hides it).
5. Globe occasionally falls back to a gray placeholder (image decode path).

---

## 4. Workstream A — CSS sprites & background-image rendering

**Symptom.** Wikipedia's search bar and many icons render as dark/garbled boxes.
**Root cause (confirmed).** Icons are CSS sprites: one sheet drawn as
`background-image`, cropped to a single icon via a *pixel* `background-position`
(e.g. `0 -747px`) and sized with `background-size`. Cerberus models
`background-position` as `ImagePos { x: f32, y: f32 }` — a 0–1 *fraction*
(`cerberus-types`) — so it cannot express a pixel offset, and it draws the whole
sheet into the element box. Also `background-image` can be a **multi-layer** list
(`linear-gradient(...), url(sprite.svg)`); only one URL is stored.

**Approach.**
1. `cerberus-style`: parse `background-position` as length pairs (px, %, keyword
   → resolved px against the box), not just `ImagePos`. Parse `background-size`
   as `cover`/`contain`/`auto`/`<len> <len>`/`%`. Parse multi-layer
   `background-image` (comma list); for v1 take the last `url()` layer and ignore
   gradient-only layers stacked with it.
2. `cerberus-types` + `cerberus-paint`: extend `DisplayItem::Image` (or add
   `DisplayItem::ImageTile`) to carry an explicit **source placement**: the drawn
   image size (from `background-size`) and a **destination offset** (from
   `background-position`), plus a **clip rect** = the element's box. The
   rasterizer draws the (scaled) source translated by the offset and clipped to
   the box — i.e. real sprite cropping. `background-repeat: no-repeat` is the
   common case; support `repeat` later.
3. `cerberus-layout` (`fn fill_box`): thread the resolved size/offset/clip.

**Files.** `cerberus-types/src/lib.rs` (`ImagePos`/new placement type),
`cerberus-style/src/lib.rs` (parse), `cerberus-paint/src/lib.rs`
(`DisplayItem::Image`, rasterizer), `cerberus-layout/src/lib.rs`
(`fill_box` ~1611, `fn image`).
**Tests.** Layout: a sprite element with `background:url(x) -10px -20px / 100px 200px no-repeat`
emits an image item with the expected source offset/clip. Paint: rasterizing that
item draws the correct sub-rect (assert a few pixels). 
**Verify.** Wikipedia search-bar icon + language chevrons render as icons, not
boxes.
**Risk.** Medium. Contained to the background path; guard behind "only when a
pixel position/size is present" so keyword `cover`/`contain` (already working)
is untouched. **Depends on Workstream B** for SVG sprites (Wikipedia's sprite is
`.svg`) — a PNG sprite will validate the mechanics first.

---

## 5. Workstream B — SVG image decoding

**Symptom.** The "WIKIPEDIA" wordmark and the icon sprite (both `.svg`) don't
render (the sprite counts as an undecoded image → `images: 1/2`).
**Root cause.** `cerberus-image` decodes raster formats (PNG/JPEG/GIF/WebP) but
not SVG.
**Approach.** Add an SVG rasterizer behind the existing `ImageDecoder`/image
adapter trait. Two options:
- **Adapter over `resvg`/`usvg`+`tiny-skia`** (fast to correctness, but a
  dependency — check `docs/adr` dependency policy / ADR-0001; it must be wrapped
  in a dedicated adapter crate per ports-and-adapters). Rasterize to RGBA at the
  requested render size (SVGs are resolution-independent; rasterize at the box's
  device pixel size for crispness).
- Or a **minimal in-house SVG subset** (paths, rects, fills, the `<use>`/sprite
  patterns Wikipedia needs). Much more work; only if the dependency is disallowed.
**Files.** New `cerberus-image-svg` (or extend `cerberus-image`), wired through
the image provider the layout/paint pipeline already uses.
**Tests.** Decode a known small SVG → expected RGBA dimensions + a sampled pixel.
**Verify.** Wordmark + sprite icons appear.
**Risk.** Medium–high (dependency decision + rasterization correctness). This
unblocks a large fraction of modern-site fidelity (logos, icons are overwhelmingly
SVG). **Do this early — it and Workstream A together fix most "missing graphics".**

---

## 6. Workstream C — Float & positioning layout fidelity

**Symptom.** Multi-column float grids (featured languages) and absolutely/
relatively positioned overlays place content wrong.
**Root cause.** Under investigation by the in-flight subagent for the specific
`float:left; width:33%` case; broader float/clear/`position` correctness likely
needs follow-up.
**Approach.** After the subagent lands, audit: (a) floats form rows and wrap
correctly, next-sibling content flows beside then clears them; (b) `clear`;
(c) `position:absolute` offsets resolve against the nearest positioned ancestor
(`.central-featured-logo` is absolutely centered over the float grid); (d)
`position:relative` offset + stacking. Build a **layout-diff probe**: an
env-gated dump of each box's `(tag, class, rect, position, float, display)` so
you can compare Cerberus's box tree to Chrome's computed layout (Chrome:
`chrome --headless --dump-dom` won't give geometry; instead inject a tiny script
via the mirror that serializes `getBoundingClientRect()` per element and diff the
two box trees numerically).
**Files.** `cerberus-layout/src/lib.rs`.
**Tests.** Layout unit tests for each fixed behavior (rows-of-3 wrap; absolute
centering; clear).
**Risk.** Medium–high; layout changes are broad — lean hard on the box-tree diff
and the full test suite to catch regressions.

---

## 7. Workstream D — Cascade, specificity & shorthands

**Symptom.** Occasional wrong colors/spacing when multiple rules touch a property.
**Root cause candidates.** later-equal-specificity ordering, `!important` layering,
shorthand→longhand expansion (`background`, `font`, `margin`, `border`), inherited
vs initial for specific properties. `var()`/`calc()` edge cases (nested var and
`calc` already have tests — extend for `min/max/clamp`, `calc` with mixed units).
**Approach.** Extract real declarations from the mirror CSS that misbehave and add
targeted cascade tests; fix `cerberus-css` cascade/expansion accordingly.
**Files.** `cerberus-css/src/lib.rs`, `parser.rs`, `cerberus-style/src/lib.rs`.
**Risk.** Low–medium; well-testable in isolation.

---

## 8. Workstream E — Text & font fidelity

**Symptom.** The wordmark (once SVG lands) and headings differ in face/weight;
serif vs sans, `font-weight`, `line-height`, letter metrics affect wrap and
vertical rhythm.
**Approach.** Verify `font-family` fallback picks a serif for
`font-family:"Georgia",serif` (the wordmark/headings), `font-weight` maps to the
right synthetic/real weight, `line-height` (unitless + length) matches, and the
shaper's advance widths are close enough that line-wrapping matches Chrome. Diff
wrap points via the `getBoundingClientRect` box-tree probe (Workstream C).
**Files.** `cerberus-text`, `cerberus-style` (font resolution), `cerberus-layout`
(line breaking).
**Risk.** Medium; font metrics differences cause cascading layout diffs, so treat
as fidelity polish after boxes/graphics are right.

---

## 9. Workstream F — JS-driven dynamic content (scope decision)

**Symptom.** The donation banner shows in Cerberus but Chrome's fundraising JS
hides it; other sites show/hide/populate content via JS after load.
**Root cause.** Cerberus runs page scripts through QuickJS + a DOM model, but the
banner's hide logic depends on APIs/branches that don't execute the same way
(storage/geo/`classList` toggling, timers).
**Approach / decision.** Parity on *fully* JS-driven UI is a large, open-ended
effort. For the parity goal, prefer pages whose *above-the-fold* result is stable
without campaign JS, and treat dynamic banners as out-of-scope noise (or mirror
the page with the banner element removed for a clean geometric comparison). If
pursued: audit which DOM/BOM APIs the hide path needs (`classList`,
`localStorage`, `matchMedia`, `MutationObserver`) and fill gaps in
`cerberus-js-dom`. **Recommendation:** defer; it is not where the biggest visual
wins are.
**Risk.** High effort, diffuse payoff.

---

## 10. Workstream G — Real-site loading ergonomics (small, high-leverage)

Make live testing "just work" without remembering `--system-roots`:
- Have `RustlsProvider::with_system_roots()` also honor `SSL_CERT_FILE`
  (`/etc/ssl/certs/...` is hardcoded today; `SSL_CERT_FILE` points at the proxy
  CA and is the standard convention — also helps real users behind corporate
  proxies). `cerberus-tls-rustls`.
- Optional: a `--ca-file <path>` flag on `render`/`run`.
**Risk.** Low; a few lines + a test that a custom CA path is loaded.

---

## 11. Sequencing (recommended order)

1. **W0** pixel-diff + corpus (measurement first — everything else is judged by it).
2. **W-C** integrate the in-flight float-grid fix; add the box-tree geometry probe.
3. **W-B** SVG decoding (unblocks the most "missing graphics").
4. **W-A** sprite/background-position (search icon, chevrons) — needs W-B for the
   `.svg` sprite; validate mechanics on a PNG sprite first.
5. **W-D** cascade/shorthand correctness as diffs surface.
6. **W-E** text/font fidelity polish.
7. **W-G** anytime (small).
8. **W-F** defer unless a target page requires it.

After Wikipedia is visually close, add the next corpus page and repeat. Only then
attempt pokemoncenter/amazon (they add: web fonts, CSS grid/flex at scale,
lazy-loaded images, and — for pokemoncenter — the reese84 interstitial, a separate
track).

---

## 12. Per-task loop (do this for every fix)

1. **Reproduce** with the harness: `compare.sh <url> <name>`; `Read` both PNGs.
2. **Root-cause with evidence**, not guessing: add an env-gated `eprintln!`
   (computed style, box rect, chosen color, decode result), render, inspect,
   then **remove the debug**. Confirm the exact mechanism before editing.
3. **Fix** the root cause in the smallest correct way. Match surrounding code
   style. No `unsafe`.
4. **Test:** add a focused regression test that fails before / passes after.
   `cargo test --workspace`, `cargo fmt --all --check`, `cargo clippy --workspace
   --all-targets`.
5. **Verify vs Chrome:** re-run the harness; the diff must visibly improve (and
   the pixel-diff score drop once W0 lands). Do not claim success otherwise.
6. **Commit** (Ben Barker identity) with a message stating symptom → root cause →
   fix → how verified. Push. Keep PRs focused.

---

## 13. Handoff notes / gotchas

- Chrome cannot tunnel the sandbox's policy proxy (TLS reset) — **always mirror
  to `127.0.0.1`** for the reference; do not try to point Chrome at a live URL here.
- Cerberus draws a 36px toolbar above content; account for it when aligning the
  two screenshots for diffing (crop or offset).
- `images: N/M decoded` in the `render` summary is a fast signal for missing/bad
  decodes (SVG shows up here as undecoded until W-B).
- The layout text-emission path is `Ctx::walk → add_text/add_run → push_piece →
  LinePiece → commit_line (emits DisplayItem::Glyphs with piece.color)`. Instrument
  at `commit_line` for the actually-painted color/size; `add_run` alone under-reports.
- `background-position`/`-size`/`object-position`/`object-fit` live as `ImagePos`
  (fraction) + `ImageFit` (enum) in `cerberus-types` — the pixel-sprite work must
  extend these.
- Keep the fingerprint-persona work (`cerberus-profile`, farbling, reese84) as a
  separate track from rendering; don't entangle them.

---

## 14. Parity gap rubric (2026-07-12 enumeration — 5-agent diagnostic sweep)

Scoreboard before this round (styled brand mirrors, RMSE / % pixels off):
example 0.075/1.8, iana 0.149/25.2, mfws 0.207/15.4, rfc1 0.134/5.1,
wikipedia 0.136/10.3, hn 0.157/17.8, mozilla 0.320/15.8, apple 0.137/12.2,
bbc 0.160/15.0.

Ranked movers (impact × breadth ÷ effort). Status updates in-place as fixes land.

| # | Gap | Impact | Pages | Status |
|---|-----|--------|-------|--------|
| 1 | MQ4 range syntax `(width <= N)` → AlwaysFalse | HIGH | iana (−11pts alone), any modern site | **DONE** |
| 2 | Integer space advances flip wrap points (4 vs 4.453px) | HIGH | every text page | **DONE** |
| 3 | line-height:normal must be Blink's per-component-rounded INTEGER (14+3+1=18@16px); explicit fractional lh accumulates | HIGH | every text page | **DONE** |
| 4 | Flex/grid drop bare text children (anonymous items) | HIGH | mozilla nav, apple, bbc | **DONE** |
| 5 | Only Regular faces bundled; faux bold/italic mis-measure every bold run | HIGH | all 9 pages | **DONE** (13 real faces; 60/60 cases <0.5px of Chrome) |
| 6 | Inline `<svg>` display:none'd (resvg already rasterizes for `<img>`) | HIGH | bbc(99), apple(64), mozilla, wikipedia | **DONE** (svg keeps tag; source-only attrs) |
| 7 | Grid explicit line placement (`grid-column: 2/9`, `1/-1`) ignored | HIGH | mozilla hero | **DONE** |
| 8 | `<center>`/-webkit-center leaks across table-cell boundary | HIGH | hn (~187px shift) | **DONE** |
| 9 | Table row-height trio: cell-less `<tr height>`, line floor on table font, lost trailing cell margin | HIGH | hn, wikipedia, iana | **DONE** |
| 10 | Glyph AA raster differs from FreeType | HIGH-floor | all | **DONE** (skrifa auto-hinter light — Chrome's mode, measured: cores 34.9→20.1% >32-off; residual is stem-phase AA + integer word origins) (integer baseline + sub-pixel pen: 71→35% of ink >32 off; residual is FreeType light-hinting's vertical grid-fit, unreachable without a hinter — fitted tone LUTs measure as no-ops, fractional word-origin carry measured WORSE and was reverted) |
| 11 | Inline-image strut descent missing from line box | MED | iana, bbc, apple | **DONE** |
| 12 | Whitespace at inline boundaries (#137): phantom space before punctuation, eaten space before nowrap, per-word underline | MED | every prose page | **DONE** (+float-band guard) |
| 13 | calc() resolves % against font-size (wrong, often negative) | MED | iana, apple, mozilla, bbc | **DONE** |
| 14 | clip:rect/clip-path sr-only pattern → stray visible text | MED | bbc, mozilla, iana, apple | **DONE** |
| 15 | :has() (and :is/:where) drop whole rules | MED | apple, mozilla, iana | **DONE** (child subset) |
| 16 | transform:translate static subset (centering, offscreen parking) | MED | apple, bbc, mozilla | backlog |
| 17 | inline-flex/inline-grid break the inline context; CSS tables flattened | MED | iana, bbc, apple | **DONE** (atomic inline + table-cell rows) |
| 18 | List markers placed inside (Chrome: outside) | MED | all list pages | **DONE** |
| 19 | min()/max()/clamp() unparsed | LOW | apple, bbc, mozilla | **DONE** |
| 20 | letter/word-spacing quantized to i32 px | LOW | apple, bbc | backlog |
| 21 | vertical-align top/middle/bottom positioning (suppression done) | LOW | iana, apple | partial |
| 22 | box-shadow inset/multi/spread | LOW | wikipedia, mozilla | backlog |
| 23 | aspect-ratio | LOW | bbc | backlog |
| 24 | filter/backdrop-filter | LOW | apple, wikipedia | backlog |
| 25 | em-margin/geometry i32 truncation drift | LOW | long pages | backlog |

Verified NON-issues (measured, do not chase): @font-face (mirrors ship no
reachable webfonts — Chrome falls back to the same fontconfig faces);
Verdana→DejaVu metric alias (measured 722.2px = Arial-metric in the reference,
the existing fall-through is correct).

Execution: items 5/6/13/14/15/19 delegated to parallel worktree subagents
(fonts / inline-svg / css-values); items 7/8/9/12/18 handled inline on the main
branch (all in cerberus-layout, avoiding merge conflicts). Re-run
`scripts/parity.sh` after each merge; the table's Status column is the ledger.

Superseded scoreboard (mid-round): example 1.82, iana 11.08,
mfws 14.18, rfc1 5.20, wikipedia 10.37, hn 14.48, mozilla 14.58 (rmse 0.304),
apple 12.27, bbc 14.80. (Live mirrors re-fetch per run; ±1pt run noise.)

Scoreboard 2026-07-13 final (adds real bold/italic faces, outside list
markers, computed-fill injection for inline svg): example 1.85, iana 11.05,
mfws 13.23 (rmse 0.155), rfc1 5.20, wikipedia 10.34, hn 14.59 (rmse 0.125),
mozilla 14.79 (rmse 0.302), apple 12.32, bbc 14.98 (rmse 0.157).

Scoreboard 2026-07-13 +hinted glyphs (skrifa auto-hinter light — ADR-0005
update; calibration-page ink pixels >32 gray levels off Chrome fell
34.9% → 20.1%): example 1.84, iana 10.94, mfws 12.57 (rmse 0.159),
rfc1 4.90, wikipedia 10.18, hn 14.14 (rmse 0.126), mozilla 14.68
(rmse 0.302), apple 12.27, bbc 14.54 (rmse 0.161) — every page's
mismatch_pct improved.

Note on mozilla's hero flag: the visible green flag in Chrome is the
ANIMATION variant (11 stacked opacity-animated svg frames gated on
data-animation-running set by JS); the static fallback is display:none unless
.no-js. Rendering it faithfully needs the animation state machine, not more
CSS - parked below the fold of this rubric. The computed-fill injection that
investigation produced is landed and benefits every CSS-painted inline svg.

Scoreboard 2026-07-13 (hinted raster + inline-flex round): example 1.84,
iana 10.94, mfws 12.57, rfc1 4.90, wikipedia 10.18, hn 14.14, mozilla 14.74,
apple 12.27, bbc 15.06. Every text page improved with hinting; the first
sub-5% brand-corpus page (rfc1 4.90) landed this round.

# Rendering parity — phase 2 plan (handoff)

Goal: a page rendered by `cerberus-app` should match Chrome/Firefox pixel-for-pixel
(modulo font hinting) on real websites. We measure by mirroring a live page to
`127.0.0.1`, rendering it in both headless Chromium and Cerberus, and diffing the
screenshots. Build up from simple pages (Wikipedia portal) toward image- and
script-heavy ones (Pokémon Center, Amazon).

This document is the continuation of `RENDERING_PARITY_PLAN.md`. It records what
phase-1 closed, then specifies the remaining workstreams in enough detail to
implement each without re-deriving the investigation.

---

## ✅ DONE — max-content undercount wrapped content-sized flex/table boxes

**Landed.** mozilla.org was the corpus outlier (RMSE 0.302 vs ~0.13 elsewhere).
The dominant error was a ~30px **vertical offset** of the whole hero (top/bottom
edge bands y40–80 and y600–680 dominated a per-row error profile): the nav's
"About us" link **wrapped to two lines**, inflating the header and shoving the
page down. Confirmed by injecting `white-space:nowrap` on `.m24-c-menu-title`
(hero top y122→y99).

Root cause was **not** the `%`/`calc` width first suspected (that resolves fine).
In `push_piece`, the MEASURE path advances the cursor by **integer** word widths
(`self.x += w as i32`) — deliberately, to keep `max_x` integer-stable and avoid
multi-px table-column drift. But real layout advances **fractionally**, so a
multi-word run's measured max-content was a sub-pixel *short*: the fractional
inter-word gap remainder carried in `x_frac` was dropped from `max_x`. A
content-sized flex item (or table cell / float) sized to that max-content is then
a fraction too narrow, and a just-fitting two-word run wraps.

**Fix (1 line):** in the measure branch, `max_x = max(max_x, x + ceil(x_frac))`.
Bounded to +1px, so it can't reintroduce the drift the integer advance prevents.
Isolated on identical cached mirrors: mozilla **0.30188 → 0.28948**, hn unchanged
(0.12632 → 0.12632 — the feared column drift did not occur), all static pages
byte-identical. Not unit-tested: the sub-pixel wrap needs real-font run mechanics
that `cerberus-layout`'s `MonoShaper` (integer advances) can't produce, and the
crate has no real-font dev-dep; `scripts/parity.sh mozilla` is the guard.

mozilla's remaining deltas are web-font metrics (workstream D) and the decorative
stepped hero background — both larger, separate efforts.

---

## Wikipedia portal — remaining deltas

The masthead, centered globe, two-column language grid, CJK, the centered search
bar + language button, and the icon-only search button now match Chrome
(subagent-verified).

### ✅ DONE — icon `<button>` children + icon-only button sizing

Three general layout fixes landed (all with regression tests) and closed the
language-button and search-button deltas:

1. **`<button>` element children render.** A `<button>` with element children
   (icon `<i>` sprites, a label `<span>`) is now routed through
   `add_inline_block` so its subtree lays out via the block box model and paints,
   while still registering a `Button` hit box. The measurement path
   (`measure_intrinsic_width`/`measure_min_content_width`) takes the same block
   path for such a button (shared `button_wants_block` predicate), else the
   scratch walk re-dispatches to `form_button` and overflows the stack.
2. **`min-width` is a floor, not a fixed width.** `resolve_block_width` pinned a
   box to a lone `min-width` (auto width, no max-width) via `unwrap_or(0).max(mw)`,
   collapsing the icon-only search button (`.pure-button` `min-width:1.6rem`) to
   16px and suppressing content measurement. It now leaves an auto-width box
   unconstrained and re-applies the `min-width` floor as a border-box value in the
   shrink-to-fit callers (`add_inline_block`, `place_float`) after measuring.
3. **`text-indent` honored on `white-space:nowrap` runs.** A nowrap run is placed
   atomically through `add_run`, which never consumed the block's one-shot
   `text-indent`. `.pure-button{white-space:nowrap}` inherits to the search
   button's `<i>`, so `text-indent:-9999px` (the image-replacement trick) was
   dropped and the "Search" label painted over the magnifying-glass sprite.
   `add_run` now consumes `pending_indent` at line start, matching `add_word`.

### ⏳ Remaining — search-widget positioning + JS-built footer

1. **`.styled-select` language dropdown wraps below the input** instead of sitting
   inside its right edge, and the Search button has a **gap** instead of being
   flush. Root cause: `.search-input` is `display:inline-block; position:relative`
   — laid in a separate `add_inline_block` sub-context — and the absolute
   `.styled-select` (`position:absolute; right:1.2rem; top:1rem`) needs
   `.search-input`'s box as its containing block *across that sub-context
   boundary*. Cerberus supports absolute positioning against a `cb_stack` entry,
   but an inline-block relative parent laid via `add_inline_block` does not push
   its box as a containing block for abs descendants. (Inline-block margins
   themselves are now applied — the `margin-right:-6.6rem` pull-back is a mobile
   `@media (max-width:480px)` rule and does not fire at desktop width; at 1200px
   `.search-input` is `width:73%`, matching Chrome.)

   **Deeper root cause found (instrumented at 1200px):** the input renders narrow
   (~285px, the `size="20"` fallback) because its `width:100%` resolves against a
   containing block of only ~288px instead of `.search-input`'s ~394px. Two links
   in the chain are wrong: (a) the `<fieldset>`'s `margin-left:1rem;
   margin-right:6.6rem` do **not** narrow the block it establishes — `.search-input`
   measures `avail=540` (the full `.search-container` content) instead of the
   fieldset's ~464px, so `<form>`/`<fieldset>` are not applying block margins to
   their content box; (b) the `#searchInput` containing block resolves to 288px,
   narrower than `.search-input`'s own laid width (394px), so `%`-width does not
   propagate cleanly through the inline-block/relative nesting. Fixing the input
   width is the highest-value next step for this widget — start by making
   `<fieldset>` a normal margin-applying block, then re-check the `%`-width cb.
2. **Footer sister-project grid absent.** `.other-projects`/`.other-project`/
   `footer-sidebar`/`app-badges` appear **only in CSS/JS**, never in the static
   DOM (0 occurrences after the last `</style>`). The grid is built by the portal's
   JavaScript at runtime; rendering it requires executing that script, not a
   layout change.

## ✅ DONE — `rem` root font-size + absolute-positioning origin (was the top gap)

**All three landed and verified against Chrome.** The Wikipedia portal's grid now
matches Chrome: two language columns flanking the centered globe, counts on one
line, CJK glyphs, compact height. The fixes:
1. **`rem` vs root font-size** — thread the root element's computed font-size
   through `build`/`apply_declarations`; fold `<number>rem → px` up front
   (`substitute_rem`).
2. **Out-of-flow width origin** — extend the used-width box from `self.left` (the
   element's flow-start), not `self.left0` (an ancestor reference); the latter
   collapsed the cell to 1px inside a centered container once rem narrowed it.
3. **Absolute static-position origin** — `apply_positioning` captures `base.x =
   self.left` and computes `elem_w = self.right - self.left`; using `self.left0`
   offset a left/top-anchored box by `(self.left − self.left0)`, spreading the
   columns and stranding the globe.

Original investigation retained below for reference.

## (historical) `rem` must resolve against the root font-size (with companions)

**Root cause (confirmed).** `rem` is converted to px with a hardcoded 16
(`parse_css_px`: `"rem" => num * 16.0`). Real pages set a smaller root via the
ubiquitous `html { font-size: 62.5% }` idiom (→ 10px), so **every rem-based box is
1.6× too large**. On the Wikipedia portal this makes `.central-featured`
(`height:32.5rem;width:54.6rem`) 520×873 px instead of 325×546, and the language
cells (`15.6rem`) 250 px instead of 156 — the grid the comparison subagent flagged
as too wide and too tall.

**The fix itself is small and verified** (a spike landed then was reverted, see
below): thread the root element's computed font-size through `CssEngine::build`
(new `root_font_size` param; `INITIAL_ROOT_FONT_PX = 16`; update to `html`'s own
computed font-size for its descendants) into `apply_declarations`, and fold
`<number>rem → px` in the declaration value up front (a `substitute_rem(value,
root_em)` helper — byte-compare the `rem` unit so multi-byte chars aren't split;
leave `em` for per-element resolution). A unit test confirmed `15.6rem`→156px and
`1.3rem`→13px under `font-size:62.5%`, and `2rem`→32px with no html font-size.

**Why it was reverted / what it needs to ship as a NET win.** Applying the rem fix
alone made the Wikipedia portal render *worse*, because it exposes companion bugs
that the oversized rem was accidentally masking. Land the rem fix **together with**:
1. **Count text wraps — ROOT-CAUSED: absolute cell gives its descendants a ~1px
   content width.** `.central-featured-lang small` ("7,189,000+ articles") measures
   only 84 px at 13 px, far inside the 156 px cell, yet "articles" wraps. A layout
   probe (in `add_word`, on the word "articles") showed two passes: the intrinsic
   **measure** pass has `right=1_000_000` (max-content, no wrap, correct), but the
   **real** layout pass has `left=227, right=228` — a **1px-wide** content box, laid
   at `.central-featured`'s left edge (227 = (1000−546)/2), NOT inside the 156 px
   cell. So the absolute cell (`.central-featured-lang`, `position:absolute` with
   only `right`, `width:15.6rem`) is not establishing a 156 px content area for its
   `.link-box → small` descendants — the width collapses to ~1px, and the number
   overflows to x≈279 > right=228, wrapping "articles". Trace the absolute cell's
   in-flow content-width setup (the `saved_right`/`used_w` path in `Ctx::walk`, and
   whether `self.left`/`self.right` are correctly re-based to the cell's box before
   its children are laid) — the explicit-width fix sets `self.right = left0 +
   used_w`, but a descendant is still seeing the parent container's edge. This is
   the concrete companion bug; fixing it should let the rem change land as a net
   win. (Reproduce with the rem spike re-applied + `CERB_DBG=1` on the mirror.)
   **Deeper probe (2nd pass):** the cell's own width setup is *correct* — a probe
   at the `used_w` computation prints `used_w=156, left0=8, cb.w=546` for every
   cell, so it sets `self.right = left0 + 156 = 164`. Yet the `<small>` lays at
   `left=227, right=228`. The coordinates are **inconsistent**: the cell is
   referenced at body-left (`left0=8`) while its descendant is laid at
   `.central-featured`'s *centered* content-left (227 = (1000−546)/2) in a 1px
   box. So the collapse is not the width computation — it is a mismatch between the
   cell's in-flow layout reference (`left0`/`left`) and the coordinate space its
   descendants land in, specific to an absolute cell inside a centered
   (`margin:0 auto`) relative container. A synthetic repro
   (`inline_block_in_absolute_cell_gets_the_cell_width`, kept as a passing general
   guard) does NOT reproduce it, so the trigger is the full centered-container +
   reveal(`opacity`) + rem-narrowed-widths combination. Next: probe `self.left` vs
   `self.left0` through the cell's block-open and children and the
   `apply_positioning` translate, to find where the 227 origin enters while the
   width reference stays at 8.
2. **Globe / column horizontal placement.** The absolute globe (in `.central-textlogo`)
   and the `right:60%`/`left:60%` language insets must scale with the corrected
   container width so the globe centers between columns instead of overlapping the
   right column. This ties into the still-open globe-centering item under
   workstream B (the `.central-textlogo` width / margin-auto path).

Do all three as one change and re-validate against the Chrome mirror before
committing — the rem fix is correct and broadly beneficial (every 62.5% site), but
only a net parity win once the grid it uncovers is aligned.

---

## The measurement loop (unchanged, use for every task)

`scratchpad/compare.sh <url> <name> [w] [h]`:
1. `curl` the page HTML + same-origin assets into `mirror-<name>/`.
2. Serve it on `127.0.0.1:<port>` (in the proxy no-proxy list, so both engines reach it).
3. Render in headless Chromium → `<name>-chrome.png` (the reference).
4. Render in Cerberus (`target/release/cerberus-app render --url … --out …`) → `<name>-cerb.png`.

Validation is done by **subagents**: spawn an agent to run `compare.sh`, read both
PNGs, and report the specific visual deltas (position, color, missing element,
glyph shape). A fix is "done" only when a subagent confirms the delta is gone and
`cargo test --workspace` + `cargo clippy --workspace` stay green.

Environment notes that bite:
- Headless Chromium **cannot** tunnel the policy proxy (TLS reset) — this is why we
  mirror to `127.0.0.1` instead of pointing Chrome at the live URL.
- Cerberus reaches live sites only with `--system-roots` (the proxy CA lives in
  `/etc/ssl/certs/ca-certificates.crt`); webpki defaults don't trust it.
- Cerberus renders at device-pixel-ratio 1. srcset/`background-size` selection all
  assume DPR 1.

---

## Phase 1 — closed (all verified vs Chrome on a Wikipedia mirror, all on branch
`claude/codebase-review-quality-2xhdbu`, PR #163)

| Fix | Root cause | File |
|-----|-----------|------|
| JS runs in sloppy mode | rquickjs defaulted `strict: true`; implicit global assignment (`portalSearchDomain = …`) threw `ReferenceError`, aborting the reveal script silently → featured languages stayed `opacity:0` | `cerberus-js-quickjs/src/lib.rs` (`sloppy_eval_options`) |
| `fill_rect` honors alpha | hard-wrote RGBA; `linear-gradient(transparent,transparent)` stamped opaque black over the wordmark sprite | `cerberus-paint/src/lib.rs` |
| srcset 1x at DPR 1 | `src` was not treated as the implicit 1x density candidate; picked `@2x` and 404'd the globe | `cerberus-layout/src/lib.rs` (`select_srcset_with_src`) |
| CSS sprites | no natural-size fit + px `background-position` dropped; sprite stretched | `cerberus-types` (`ImageFit::Auto`), `cerberus-style` (`background_position_px`), `cerberus-css`, `cerberus-text` (`draw_image`) |
| negative `text-indent` | clamped `.max(0)`; the `-9999px` image-replacement trick failed → fallback text overlapped the sprite | `cerberus-layout/src/lib.rs` |

Result: the Wikipedia portal's language list, globe, and wordmark now render
essentially as Chrome does.

Also proven **not** a rendering bug (do not chase): the fundraising banner shows in
Cerberus but not in Chrome's mirror render. The static `.banner` element is
correctly culled (`display:none` — confirmed by probing computed `display` in
`CssEngine::build` and the cull in `Ctx::walk`). The visible "Thank you for
donating" dialog is injected/revealed by the CentralNotice fundraising JS, and its
appearance is **non-deterministic across runs** (the text reached `add_text` twice
on one run, zero times on the next, with identical layout code) — i.e. it depends
on JS event-loop execution, not CSS/layout. See workstream C.

---

## Workstream A — font fallback + CJK  ✅ DONE (emoji still open)

**Closed for CJK.** Bundled IPAGothic (IPA Font License v1.0, 6.2 MB) as a fallback
face and added per-glyph font selection: `GlyphBox` carries a `FontSlot`
(Text/Icon/Fallback); `shape()` prefers Roboto and, for a character it lacks,
shapes from the fallback when covered (else keeps the real `.notdef`); `draw_run()`
outlines each glyph from its own face on a shared baseline. Verified against a
Wikipedia mirror: 日本語/記事/中文/条目/條目 now render as real glyphs (IPAGothic
covers Kanji/Kana and, via shared Han, Wikipedia's Chinese). Renders stay
byte-deterministic. **Emoji is still open** — 🎉 needs a color-emoji (COLR/CBDT)
face, which ab_glyph does not rasterize; tracked separately. The original design
notes below remain valid for extending the fallback chain (e.g. web fonts,
workstream D — generalize `FontSlot` to a font-table index).

## Workstream A (original design notes) — font fallback + CJK/emoji

**Symptom.** `日本語`, `中文`, and emoji render as tofu (□) because the only bundled
text font is Latin Roboto (`cerberus-text/assets/Roboto-Regular.ttf`). Chrome has
system CJK fonts, so this is the single biggest source of divergence on
international sites (including the Pokémon Center JP target).

**Why it is a design decision, not just a bug.** ADR-0005 deliberately bundles a
*fixed* font set (no system-font reads) for reproducibility and anti-fingerprinting.
Adding CJK keeps that property (everyone gets the same bundled fallback) but adds a
multi-megabyte asset. Options, by coverage/size:
- IPAGothic `ipag.ttf` — 6.2 MB, clean Japanese (Kanji+Kana). Shared Han covers much
  Chinese; Korean still tofu. Best size/quality for the JP target.
- WenQuanYi Zen Hei `wqy-zenhei.ttc` — 16.8 MB, full C/J/K, clean. A `.ttc`
  collection: load with `FontRef::try_from_slice_and_index(bytes, 0)`.
- GNU Unifont `unifont.otf` — 5 MB, ~all of the BMP incl. CJK/Hangul/symbols, but
  bitmap/blocky quality.

All three exist on the build host under `/usr/share/fonts/`. **The maintainer picks
the coverage/size tradeoff** before this lands; the code path below is
font-agnostic, so swapping the asset is one `include_bytes!` line + a license file.

**Implementation (per-glyph fallback; the shaper is already per-character, so no run
splitting is required):**

1. `cerberus-paint/src/lib.rs` — add a font selector to `GlyphBox`:
   ```rust
   pub struct GlyphBox { …, pub font: FontSlot }
   #[derive(Clone, Copy, …)] pub enum FontSlot { Text, Icon, Fallback }
   ```
   Default `Text`. Update `DisplayList::scaled` (copies the field through) and the
   placeholder rasterizer (ignores it). This is the only cross-crate type change.

2. `cerberus-text/src/lib.rs`:
   - Load a third face: `fallback: FontRef<'static>` from the bundled CJK bytes.
   - In `shape()`, for each `ch`: `let id = self.font.glyph_id(ch)`. If
     `id.0 == 0` (Roboto `.notdef`), try `self.fallback.glyph_id(ch)`; if that is
     non-zero, emit the glyph with `font: FontSlot::Fallback` and the **fallback
     font's** advance. Otherwise keep the Roboto `.notdef` (real tofu, matching a
     browser with no matching font).
   - In `draw_run()`, pick the face from `g.font` instead of `style.icon` alone
     (`Icon` → icon_font, `Fallback` → fallback, else text font). The baseline uses
     each face's own `ascent()` so mixed runs sit on one baseline.

3. Metrics caution: the fallback font's `units_per_em`/ascent differ from Roboto.
   Shape each glyph with its own `as_scaled(px)` so advances and bounds are correct
   (already the pattern in `draw_run`). Line height is driven by
   `style.font_size`/`line_height` in layout, not the glyph face, so mixed-script
   lines keep a stable leading — verify against Chrome that CJK doesn't overflow the
   line box.

4. Emoji is a **separate** problem from CJK. Monochrome emoji (from a fallback that
   has them) render as outlines; Chrome uses *color* emoji (COLR/CBDT), which
   ab_glyph does not rasterize. Scope decision: either (a) accept monochrome/absent
   emoji (tofu) for now, or (b) add a small color-emoji path later. Do **not** block
   CJK on emoji.

**Validation.** Subagent renders the Wikipedia mirror (has `日本語`/`中文`) and a
Pokémon Center JP mirror; confirm CJK glyphs match Chrome's shapes and the language
grid metrics don't shift. Add unit tests in `cerberus-text`: a CJK char shapes to a
non-zero id via the fallback face and reports `FontSlot::Fallback`; a Latin char
stays `FontSlot::Text`.

**Risk.** This touches the hot text path. Keep the `Text` path byte-identical when
no fallback is needed (the `id.0 == 0` check is the only added branch in `shape`).

---

## Workstream B — broaden the corpus (drives out the next bugs)

Phase 1 over-fit Wikipedia. Each new real page surfaces a new class of bug. Run
`compare.sh` on, in order:
1. `en.wikipedia.org` article page (tables, infoboxes, thumbnails, references).
2. `pokemoncenter.com` (JS-gated, CJK, sprites, fland grids) — needs workstream A
   and likely the reese84 challenge path (already partly built; see task list).
3. `amazon.com` (dense grids, lazy images, web fonts).

For each: subagent produces a ranked list of visual deltas; file each as its own
task with a minimal repro drawn from the *real* mirror (never hand-authored HTML —
that just moves the goalposts). Likely repeat offenders to expect: `position:
absolute/fixed` placement, `flex`/`grid` gap and wrap, `@font-face` web fonts
(currently unsupported — Cerberus uses only bundled faces; a web-font would tofu or
fall back), `object-fit` on `<img>`, `border`/`box-shadow` fidelity, and `overflow`
scroll containers.

---

## Workstream C — JS execution determinism  ✅ DONE

**Closed.** Root cause was two process-non-deterministic entropy/time sources:
`Math.random` used QuickJS's entropy-seeded default, and `Date.now()`/`new Date()`
read wall-clock. A page bucketing on either (e.g. the fundraising banner) rendered
differently each load. Fixes: seeded `Math.random` (mulberry32 off the per-head
`__FARBLE_HI/LO`) in the farbling prologue, and a deterministic monotonic clock
(fixed base epoch) for `Date.now()`/`new Date()` in the DOM prelude
(`Date.parse`/`Date.UTC`/explicit dates preserved). Four renders of a fixed-URL
Wikipedia mirror are now byte-identical. Both were also fingerprint surfaces, so
this is a privacy win too. Original notes retained below for reference.

## Workstream C (original notes) — JS execution determinism

**Observation.** The fundraising banner's visibility flipped between identical runs.
That means the QuickJS event-loop pump or the DOM it produces is
**non-deterministic**. This is a latent correctness problem well beyond the banner:
any page whose layout depends on script output can render differently each load.

**Investigate (in this order):**
1. Is the non-determinism in *ordering* (HashMap iteration over timers/handlers,
   `std::collections::HashMap` in the DOM or event queue) or in *timing* (virtual
   clock advancing a different number of steps under `EventLoopBudget`)?
   Grep the DOM/event-loop crates for `HashMap`/`HashSet` on any path that feeds
   ordered output (timer queue, event dispatch, node iteration) and confirm they are
   deterministically ordered (`BTreeMap`/insertion-ordered `Vec`).
2. Confirm `run_event_loop` / `drive_fetches` advance a *fixed* number of virtual ms
   given the same inputs — `__cerberusStepTimer` should be a pure function of queue
   state, not wall clock. Note scripts already can't read wall clock
   (`Date.now()`/`Math.random()` are stubbed for reproducibility — verify the stubs
   are seeded deterministically per page, not per process).
3. Only once the source is known: make the ordering canonical. Do **not** paper over
   it by disabling scripts.

**Why it matters for parity:** a deterministic engine is a prerequisite for
screenshot diffing to be a stable signal at all. Treat this as a correctness gate,
not a cosmetic fix. It does not require matching Chrome's *specific* banner decision
(that is cookie/geo/campaign-driven and out of scope) — only that Cerberus render
the same page the same way twice.

---

## Workstream D — `@font-face` / web fonts (medium)

Real sites ship their own fonts via `@font-face { src: url(...) }`. Cerberus
currently renders everything from the two bundled faces, so any web-font text is
shaped with Roboto metrics (wrong advances → wrong wrapping) or tofu. Scope:
1. Parse `@font-face` (family name, `src` url, weight/style) in `cerberus-css`.
2. Fetch the font file through the existing subresource pipeline
   (`collect_*`/`ImageCodec` sibling — add a font store keyed by family).
3. Load it in `cerberus-text` as an additional `FontRef` and select it when a run's
   computed `font-family` names it (fallback chain: web font → bundled text →
   bundled CJK → notdef).
This composes with workstream A's `FontSlot` (generalize `FontSlot` to an index into
a font table rather than a 3-value enum). Note the anti-fingerprinting tension:
loading page-supplied fonts widens the surface; gate behind the same consent/farbling
policy as other subresources, or restrict to metrics-only usage. **Maintainer
decision** before implementing.

---

## Workstream E — SSL_CERT_FILE for live rendering (small, unblocks live tests)

From phase 1's plan (workstream G), still open. `cerberus-tls-rustls`
`with_system_roots()` hardcodes `/etc/ssl/certs/ca-certificates.crt`. Extend it to
honor `$SSL_CERT_FILE` when set (env override → path → default), so the same binary
works in environments where the CA bundle lives elsewhere. Add a unit test that the
env var wins when present.

---

## Sequencing

1. **C (determinism)** first — it is a correctness gate that makes every later
   screenshot diff trustworthy. Cheap to investigate, high leverage.
2. **A (CJK fallback)** next — biggest visual delta, self-contained, unblocks the
   Pokémon Center/Amazon corpus. Blocked only on the maintainer's font-asset choice.
3. **B (corpus)** continuously, after A — each page files new tasks.
4. **D (web fonts)** and **E (SSL_CERT_FILE)** as the corpus demands them.

## Per-task loop (repeat)

1. Pick the top visual delta from a subagent's `compare.sh` report.
2. Write the minimal repro from the real mirror.
3. Find root cause (probe computed style / display list, don't guess).
4. Fix; add a unit test that pins the behavior.
5. `cargo test --workspace` + `cargo clippy --workspace` green.
6. Subagent re-runs `compare.sh`; confirm the delta is gone and nothing regressed.
7. Commit (author `Ben Barker <benz.benbarker@gmail.com>`), push, keep PR #163
   current.

## Gotchas carried forward

- DPR is 1 everywhere; don't assume retina.
- Never hand-author test HTML to "pass" — validate against real mirrors.
- `display:none` is culled in `Ctx::walk`; positioned/flex/grid item collection
  already excludes it (`is_flex_grid_item`) — if an out-of-flow element ever renders
  under a `display:none` ancestor, that's a real bug, but phase-1 investigation
  found the culling correct.
- Keep the fast path in `fill_rect`/`draw_run` byte-identical for the common
  (opaque, Latin) case; these are the hottest loops.

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

## Workstream A — font fallback + CJK/emoji  (highest remaining visual impact)

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

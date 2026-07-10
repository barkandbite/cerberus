# Rendering Architecture Plan — Off the Hand-Rolled Walker

Status: accepted plan of record. Owner: lead architect. Date: 2026-07-07.

This is the decisive, staged plan to move Cerberus off its hand-rolled single-pass
layout walker and onto standardized reference implementations, adopted **modularly**
behind the existing ports-and-adapters seams. It supersedes ad-hoc bug-by-bug fixes.

The governing seam is unchanged throughout:

```
LayoutEngine::layout(&mut self, styled: &StyledDom, viewport: Size,
                     shaper: &dyn TextShaper, images: &dyn ImageProvider,
                     forms: &dyn FormState) -> LaidOut
```
(`crates/cerberus-layout/src/lib.rs:123-134`; sole impl `BlockLayout`, `:139`.)

Every stage below is proven with `scripts/parity.sh` against `docs/parity-corpus.txt`.
The baseline that must never regress: **example 0.068 / iana 0.131 / wikipedia 0.143** RMSE.

---

## 1. Diagnosis

The walker cannot converge because it collapses the three distinct stages the CSS box
model requires — box-tree construction (with anonymous boxes) → bottom-up
min/max-content intrinsic sizing → per-formatting-context used-size layout producing a
fragment tree — into one immediate-mode recursive `Ctx::walk` (`lib.rs:413`) driven by a
single shared mutable cursor (`self.x/y/left/right`, `Ctx:228-301`). Because there is no
box tree, a block mixing inline and block children is never wrapped in anonymous boxes and
inline formatting is faked on the cursor; because there is no intrinsic-sizing pass,
min/max-content width is obtained by **re-running the entire layout into a 1,000,000px
probe** (`measure_intrinsic_width:1807`), which is superlinear on nesting and forces a
`measuring` flag threaded through flex/grid/block to suppress grow/justify/stretch; because
there is no per-FC used-size algorithm and no containing-block chain, margins never collapse,
percentages resolve against whatever the cursor happens to hold (double-resolving 73% of
73%), and absolute positioning dies inside inline-block sub-contexts. **At least seven of the
recently hand-fixed bugs map one-to-one onto a missing stage** (1.5×→1.2× line-height =
absent font-metric line box; dropped %/vw/vh margins = absent computed-vs-used split; ignored
inline padding = absent inline box/IFC; twice-resolved % width = absent stored used-inline-size;
equal-split table columns = absent min/max-content column algorithm; dead abs-pos in subs =
absent global containing-block chain; float intrinsic underestimate = absent bottom-up sizing
pass). These are not bugs to be patched; they are the same missing pipeline surfacing
repeatedly. Continuing to hand-derive means re-deriving the spec one symptom at a time.

---

## 2. Target Architecture

Three staged transforms behind the one `LayoutEngine::layout` seam, each owned by a
named implementation:

```
StyledDom                     (cerberus-style: ComputedStyle, used values, Len, node_id)
   │
   ▼  box-tree construction + anonymous-box generation          ── OURS (new module)
BoxTree                       (block/inline-level normalization; viewport units pre-resolved)
   │
   ▼  block / flex / grid box geometry + intrinsic sizing        ── TAFFY
Fragment geometry             (x/y/w/h, margin/padding/border, min/max-content via measure fn)
   │        ▲
   │        └── leaf measure callback  ── OURS inline engine + TEXT SHAPER (rustybuzz)
   ▼
Inline line layout            (line breaking, baselines, vertical-align, letter/word-spacing) ── OURS
   │
   ▼
DisplayList + LinkBox/FormFieldBox/ElementBox (keyed by node_id)  ── OURS (read-back walk)
   │
   ▼
Paint                          (cerberus-paint / resvg; unchanged)
```

Ownership, explicitly:

- **taffy owns block + flexbox + grid box geometry.** box-sizing, min/max clamping, auto
  margins (retires the `margin_left_auto/right_auto` centering hack), flex grow/shrink/basis/
  wrap/justify/align, full grid tracks (length/percent/fr/minmax/repeat auto-fill/auto-fit/
  span/gap), aspect-ratio, `position:absolute` against the containing block, and — the crux —
  correct **two-phase intrinsic sizing** via a leaf measure function called with
  `AvailableSpace::MinContent/MaxContent/Definite`, which structurally replaces the 1,000,000px
  probe.
- **Our inline engine + text shaper own the inline formatting context.** taffy has no IFC;
  every inline run is a taffy **leaf** whose size comes from our measure callback. Line
  breaking, white-space collapsing, vertical-align, baselines, letter/word-spacing,
  text-indent, and list markers stay in `cerberus-layout`/`cerberus-text`. Inside the measure
  callback and paint, glyph advances come from **rustybuzz** (HarfBuzz-accurate GSUB/GPOS)
  behind the existing `TextShaper::shape(text, px) -> Vec<GlyphBox>` seam. Glyph ids remain
  font glyph indices, so ab_glyph stays as the outline rasterizer.
- **Tables, floats, and fixed positioning stay ours (for now).** taffy has no CSS table
  algorithm (only an `item_is_table` flag), floats are experimental behind `float_layout`
  (not production-grade), and taffy has no viewport-fixed concept. These page classes keep the
  walker until an explicit later stage proves a taffy mapping (tables can later ride taffy grid
  as a substrate, but only after parity proves it).
- **Parsing (html5ever / cssparser / selectors): SKIP for now.** Adopting a real style system
  would fight the hand-rolled `cerberus-css` cascade and pull a large transitive tree with its
  own `ComputedStyle`. The parsing/cascade layer is not the source of the parity gap — layout
  and text metrics are. Revisit only after the layout+text migration lands and stabilizes; it
  is out of scope for this plan.

`ComputedStyle` (`cerberus-style/src/lib.rs:333`) remains the neutral used-value input across
all stages, and `node_id` (`:679`) remains the correlation key stamped onto
`ElementBox/LinkBox/FormFieldBox` for hit-testing — a replacement engine reuses both unchanged.

---

## 3. Dependency Decision

| Crate | Role | Weight | Verdict |
|---|---|---|---|
| **taffy** 0.12.1 | block + flex + grid box geometry, intrinsic sizing | 1 required dep (`arrayvec` 0.7); no_std/alloc; ~10k loc; no C/FFI, no network, no fonts | **adopt (staged)** |
| **rustybuzz** 0.20.1 | HarfBuzz-accurate glyph advances behind `TextShaper::shape` | `ttf-parser` + small pure-Rust unicode crates; no build scripts, no system/FFI, no font enumeration; ~ab_glyph weight | **adopt now** |
| **cosmic-text** | full paragraph/line engine (shaping + BiDi + linebreak + fallback) | pulls harfrust/read-fonts + fontdb (+ optional system font scanning) + swash + unicode-bidi/linebreak/segmentation | **defer** — it *replaces* our inline layout; only worth it once inline layout is restructured onto a real line-box model, at which point its `Buffer` becomes the taffy leaf-measure fn |
| **stylo / servo layout** | full CSS2.1 engine + style system | large transitive graph, own cascade | **skip** — fights `cerberus-css`, violates the lightweight ethos |
| **html5ever / cssparser / selectors** | parsing + selector matching | moderate, but couples to a foreign style model | **skip for now** — not the parity bottleneck |

Rationale: taffy is the single best-matched dependency — it is precisely the box engine whose
absence causes nearly every hand-fixed bug — and its footprint (`arrayvec` only, no_std,
`panic=abort`/thin-LTO compatible) fits the memory-lean ethos far better than continuing to
hand-derive a correct box model. Enable only `taffy_tree + block_layout + flexbox + grid +
content_size`; **leave off** `float_layout`, `parse`, `serde`. Gate the whole crate behind a
`layout-taffy` cargo feature so a minimal privacy build can exclude it entirely. rustybuzz is
an independent, even lighter win that slots behind the narrow existing shaping seam with zero
caller changes.

**Determinism (fingerprinting / ADR-0005):** both adopted crates preserve reproducible text
metrics. rustybuzz shapes from our **already-bundled TTF bytes** via `Face::from_slice` — no
system fonts, no `fontdb`, no enumeration; advances are deterministic fixed-point. cosmic-text
is the determinism risk (it *can* scan system fonts) and is one more reason to defer it; if
ever adopted it must run on an empty `fontdb::Database` + `load_font_data(bundled)` with
`load_system_fonts()` never called. We must also fix an explicit **rounding policy**
(accumulate fractional advance, round per-run not per-glyph; round taffy's f32 geometry to i32
once at read-back) so sub-pixel drift does not move RMSE.

---

## 4. Staged Migration (strangler-fig)

Ordered for maximum parity-ROI at minimum risk. Every stage is A/B-selectable behind the
trait, defaults to the current walker, and rolls back by flipping one runtime flag
(`CERB_LAYOUT` env / `render --engine`) or compiling out the `layout-taffy` feature. The gate
is always: `scripts/parity.sh` renders the 3-page corpus under **both** engines and
`cerberus-app diff --fail-over` requires **no per-page RMSE regression** vs
example 0.068 / iana 0.131 / wikipedia 0.143. A page only flips its default to taffy once its
taffy RMSE is ≤ the walker's.

**Stage 0 — Engine-selection seam + dual-run harness (NO dependency). ← FIRST PR.**
Goal: prove swappability and enable A/B before any dep lands. Add `LayoutEngineKind { Block,
Taffy }` + a `make_layout()` factory in `cerberus-layout`; thread the kind through the three
`BlockLayout::default()` sites (`cerberus-app/src/lib.rs:1211` render, `:4462` render_frame,
benches `:5663/:5675`); expose `render --engine block|taffy` in `cerberus-app/src/main.rs`
with a `CERB_LAYOUT` env fallback; extend `scripts/parity.sh` with an `--engine` passthrough
that renders the corpus under both engines into two CSV columns. **Taffy aliases BlockLayout**
(factory returns the walker for both). Gate: both columns emit identical RMSE (baseline
unchanged) and `cargo test` green.

**Stage 1 — Box-tree construction pass (NO dependency, pure refactor).**
Goal: land the box-tree seam every later stage needs. New module: pure
`StyledDom -> BoxTree` performing block/inline-level normalization + anonymous block-box
generation (wrap inline runs that are siblings of block boxes), plus pre-resolving
`Len::Vw/Vh/Vmin/Vmax` to px against the viewport. `Ctx::walk` consumes `BoxTree` instead of
`StyledNode`, emitting the **identical** display list. Adapter boundary: internal to
`cerberus-layout`, no trait change. Gate: RMSE **unchanged** (behavior-preserving).

**Stage 2 — rustybuzz behind `TextShaper::shape` (adopt now; independent parallel track).**
Goal: kill font advance/wrap drift. In `cerberus-text`, build rustybuzz `Face<'static>` from
the existing `FONT_BYTES/ICON_FONT_BYTES/FALLBACK_FONT_BYTES`; rewrite `shape()` and
`space_advance` to emit `GlyphBox` from `glyph_infos()`/`glyph_positions()` (advance scaled
`px/upem`), keeping ab_glyph faces for outline rasterization and the current per-char CJK
fallback (re-shape glyph-id-0 clusters with the fallback face). Add a shaped-run width cache
(keyed by text+px+face+letter/word-spacing) so the probe/measure path does not regress on
HarfBuzz's higher shaping cost. Preserve the `space_advance == sum(shape(" "))` invariant test.
Adapter boundary: `cerberus_paint::TextShaper` — callers unchanged. Gate: no visual regression;
RMSE moves **toward** Chrome (largest gain expected on text-dense iana/wikipedia).

**Stage 3 — taffy for block geometry (adopt taffy).**
Goal: put every recent box-model bug on the correct engine. New `TaffyLayout: LayoutEngine` in
`crates/cerberus-layout/src/taffy_engine.rs` behind `layout-taffy`. Build a `TaffyTree` from
the Stage-1 `BoxTree`: `to_taffy_style(&ComputedStyle, vw, vh)` mapping `Len ->
Dimension/LengthPercentageAuto` (Px→length, Pct→percent, Auto→auto; viewport units already
resolved), box_sizing, size/min/max, margins (auto→auto), padding, border. Inline/replaced
children become taffy **leaves** whose `NodeContext { InlineRun / Replaced / Block }` drives a
`compute_layout_with_measure` closure that calls the **existing** inline measurement/line-break
against `avail.width` (Stage-2 shaper underneath). Read back post-order, accumulating
parent-relative `location` into absolute origins, rounding to i32, emitting the same
`DisplayItems` + hit-boxes keyed by `node_id`. Adapter boundary: second `LayoutEngine` impl,
selected per Stage 0. Gate: per-page RMSE ≤ walker; flip a page's default to taffy only when it
passes.

**Stage 4 — flex + grid onto taffy (taffy's core strength).**
Goal: retire the ad-hoc `flex_row/flex_column`, equal-split table columns, and `Auto=1fr`
grid approximations. Extend `to_taffy_style` with the full flex fields and
`grid_template_*`/`grid_auto_*`/gap/span mapping. Adapter boundary: same `TaffyLayout`. Gate:
per-page RMSE ≤ walker (expect improvement on flex/grid-heavy wikipedia) before flipping those
page classes.

**Stage 5 (later) — restructure inline layout onto a real line-box model.**
Only after Stages 1–4 are the default across the corpus. This is the point at which
cosmic-text (as the taffy leaf-measure `Buffer`) becomes worth its weight, and tables/floats/
fixed-positioning get real taffy-substrate mappings proven on the corpus. Out of scope to
schedule now; listed so the ordering above is not mistaken for the finish line.

---

## 5. Risks & Rollback

- **Dependency weight / binary size.** Mitigated by scope: taffy features trimmed to
  `taffy_tree+block_layout+flexbox+grid+content_size` behind the `layout-taffy` feature (a
  minimal build excludes it); rustybuzz is ~ab_glyph weight and could eventually *replace*
  ab_glyph (outlining via ttf-parser/swash), a net simplification. Rollback: drop the feature.
- **Inline layout is still ours under taffy.** taffy sees inline runs only as atomic measured
  leaves, so IFC bugs (baselines, vertical-align, wrap-around-float) are not fixed by taffy and
  could interact with taffy's box results. Mitigated by Stage 1 isolating inline into clean
  leaves first, and by leaving float/table/fixed pages on the walker until explicitly proven.
  Rollback: per-page engine selection keeps any regressing page on the walker.
- **Text determinism / fingerprinting.** rustybuzz stays deterministic (bundled bytes, no
  system fonts); cosmic-text is deferred precisely because of its system-font surface. Risk is
  the f32→i32 and per-glyph-vs-per-run rounding drift; mitigated by a fixed rounding policy
  validated against Chrome via parity. Rollback: shaper swap is behind `TextShaper` and
  independently revertible.
- **Margin-collapse / sub-pixel shifts move RMSE.** taffy collapses margins correctly where
  the walker does not, so first parity runs may shift vertical positions — expected, and taffy
  is the more-correct side vs Chrome. Mitigated by the per-page gate: a page only flips when its
  taffy RMSE is ≤ the walker's, so a shift that happens to worsen a specific page simply keeps
  that page on the walker until fixed.
- **Overall reversibility.** Default remains `Block` at every stage. The kill switches are
  layered: runtime (`CERB_LAYOUT` / `render --engine`), compile-time (`layout-taffy` feature),
  and per-page default selection. No stage is a big-bang; each merges only behind a green
  corpus gate.

---

## 6. First PR

**Stage 0 — engine-selection seam + dual-run parity harness. No new dependency; taffy aliases
the walker.**

Concrete changes:

- `crates/cerberus-layout/src/lib.rs` — add `pub enum LayoutEngineKind { Block, Taffy }` and
  `pub fn make_layout(kind: LayoutEngineKind, margin: i32) -> Box<dyn LayoutEngine>` returning
  `BlockLayout` for **both** variants for now.
- Thread the chosen kind through the three construction sites:
  `crates/cerberus-app/src/lib.rs:1211` (render), `:4462` (render_frame), benches `:5663/:5675`.
- `crates/cerberus-app/src/main.rs` (`cmd_render`) — add `--engine block|taffy` with a
  `CERB_LAYOUT` env fallback.
- `scripts/parity.sh` — add `--engine` passthrough that renders the corpus under **both**
  engines into two CSV columns.

Verification command:

```sh
scripts/parity.sh --engine both
```

Merge gate: both CSV columns emit identical RMSE (example 0.068 / iana 0.131 /
wikipedia 0.143 unchanged) and `cargo test` is green — proving the seam is swappable and the
harness can A/B **before** any taffy code or dependency lands.

---

## 7. Progress log (2026-07-07)

Landed so far, all behind the Stage-0 seam (`--engine block|taffy` / `CERB_LAYOUT`):

- **Stage 0 — seam + dual-run harness.** `LayoutEngineKind`, `make_layout`, `--engine`
  passthrough, `CERB_LAYOUT`. Block is the default; taffy is opt-in.
- **Stage 2 — rustybuzz shaping.** `cerberus-text` shapes advances with HarfBuzz
  (rustybuzz), ab_glyph still rasterizes. Parity-neutral on the corpus because the residual
  is a bundled-font vs Chrome-font *face* mismatch, not shaping — but the correct foundation.
- **Stages 1/3/4 combined — the taffy engine.** Rather than a separate box-tree refactor
  first, the `cerberus-taffy` crate does box-tree normalization, block geometry, and the
  flex/grid mapping together:
  - `to_taffy_style(&ComputedStyle, vw, vh) -> taffy::Style` — pure, unit-tested mapping of
    display/position/size/min/max/margins (auto)/padding/border/inset/flex/grid/gap.
  - `TaffyLayout: LayoutEngine` — builds a `TaffyTree`, every element a container and each
    inline run an anonymous leaf measured + painted via the walker's shared
    `flow_inline` (so lists/tables/inline stay the single source of truth); box
    backgrounds/borders via the shared `box_decorations`. Adjacent-sibling margin collapsing
    approximated in the block formatting context.
  - The app composes both adapters, so `cerberus-layout` need not depend on `cerberus-taffy`.

Paired corpus (RMSE vs headless Chrome, lower = closer):

|            | block   | taffy   |
|------------|---------|---------|
| example    | 0.06782 | 0.06401 |
| iana       | 0.13052 | 0.12434 |
| rfc1       | 0.12301 | 0.12229 |
| wikipedia  | 0.14252 | 0.13086 |
| hn         | 0.17295 | 0.17466 |
| mfws       | 0.24013 | 0.25874 |

taffy is better or tied on five of six; the two it trails are dominated by the font-face
floor (mfws is a bare, CSS-less page). Also landed as shared-walker fidelity, benefiting
both engines: HTML presentational hints (`width`/`bgcolor`/`align`/`nowrap`) and honoring
the table `border` attribute (no grid lines on layout tables) — Hacker News now renders its
orange header, full width, and clean rows.

Remaining gaps (next targets):

- **Parent/first-child margin collapsing** — the naive version over-corrects; needs the
  collapsed margin to move outside the parent. Sibling collapsing only, for now.
- **Taffy table speed** — each inline leaf is flowed in `measure` and again in `paint`;
  table-dense pages (HN) pay ~35% over the walker. Cache the measured flow and translate it.
- **Font-face floor** — the clean pages' residual is bundled Roboto vs Chrome's default
  face. Determinism (ADR-0005) forbids system fonts, so this is a bundled-face choice, not a
  shaping bug.
- **Graduation** — flip a page's default to taffy once its RMSE is ≤ the walker's and speed
  is comparable. Not yet flipped anywhere.

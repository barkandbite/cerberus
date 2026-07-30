//! Software text adapter (ADR-0005).
//!
//! Wraps `ab_glyph` and a **bundled** Roboto font (Apache-2.0) behind our paint
//! traits. One [`TextEngine`] implements both `TextShaper` (char → glyph ids +
//! advances) and `Rasterizer` (paints rects/images, and anti-aliased glyph
//! outlines). System fonts are never read — the font set is fixed, which is both
//! reproducible and an anti-fingerprinting choice (see ADR-0005).
//!
//! Glyph **advances** come from `rustybuzz` (a pure-Rust HarfBuzz port) — real
//! shaping with kerning/ligatures/GPOS/GSUB, the foundation for complex scripts.
//! Glyph **outlines** come from `skrifa` with FreeType-style light hinting
//! (vertical grid-fitting, matching the reference Chrome — see `hinted.rs`),
//! filled by `ab_glyph_rasterizer`; `ab_glyph` remains for vertical metrics and
//! as the unhinted fallback fill (glyph ids are shared font indices). All read
//! the same bundled bytes, so text metrics stay reproducible (ADR-0005). See
//! `RENDERING_ARCHITECTURE_PLAN.md`.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, FontSlot, Framebuffer, GlyphBox, Rasterizer, TextShaper,
};
use cerberus_types::{Color, FontStyle, GenericFamily, ImageFit, ImagePos, Point, Rect};
use skrifa::MetadataProvider;

mod hinted;

/// The bundled font (Roboto Regular, Apache-2.0). See `assets/Roboto-LICENSE.txt`.
const FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Regular.ttf");
/// Bundled icon font (user-supplied IcoMoon subset). See `assets/icomoon-LICENSE.txt`.
const ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/icomoon.ttf");
/// Bundled CJK fallback (IPAGothic, IPA Font License v1.0). See
/// `assets/IPAGothic-LICENSE.txt`. Renders characters Roboto lacks (Kanji/Kana,
/// and — via shared Han — much Chinese) instead of tofu. Bundled, not read from
/// the system, so the font set stays fixed and reproducible (ADR-0005).
const FALLBACK_FONT_BYTES: &[u8] = include_bytes!("../assets/IPAGothic.ttf");
/// Bundled serif face (Liberation Serif, SIL OFL 1.1 — Times-metric). Chrome's
/// generic `serif` requests "Times New Roman", which fontconfig substitutes with
/// Liberation Serif — measured directly against the reference (a 100px `H` run
/// advances 72.3px/em ≈ Times' 0.722em, not DejaVu Serif's 0.872em). See
/// `assets/Liberation-LICENSE.txt`.
const SERIF_FONT_BYTES: &[u8] = include_bytes!("../assets/LiberationSerif-Regular.ttf");
/// Bundled generic-monospace face (DejaVu Sans Mono, Bitstream Vera license) —
/// measured as what the reference Chrome's generic `monospace` (`<pre>`/`<code>`)
/// actually renders. See `assets/DejaVu-LICENSE.txt`.
const MONO_FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono.ttf");
/// Bundled Courier-metric mono face (Liberation Mono, SIL OFL 1.1) — what a page
/// that names Courier New specifically renders (fontconfig metric alias),
/// distinct from the generic monospace. See `assets/Liberation-LICENSE.txt`.
const COURIER_FONT_BYTES: &[u8] = include_bytes!("../assets/LiberationMono-Regular.ttf");
/// Bundled system-UI sans (DejaVu Sans, Bitstream Vera license) — what the
/// reference resolves `system-ui` to. See `assets/DejaVu-LICENSE.txt`.
const SYSTEM_SANS_FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
/// Bundled Arial-metric sans face (Liberation Sans, SIL OFL 1.1). Chrome's
/// generic `sans-serif` requests "Arial" → fontconfig → Liberation Sans, so this
/// face serves BOTH the generic and named-Arial routes (measured: 72.0px/em `H`
/// at 100px ≈ Arial's 0.722em). Roboto stays the UI/chrome face. See
/// `assets/Liberation-LICENSE.txt`.
const SANS_FONT_BYTES: &[u8] = include_bytes!("../assets/LiberationSans-Regular.ttf");
// Real bold/italic variants per slot (same upstream releases as the regular
// faces above, taken from the system fontconfig dirs; covered by the bundled
// Liberation/DejaVu license texts). The rasterizer's faux-bold smear and
// faux-italic shear remain only as fallbacks for slots without a real variant
// (Roboto UI text, DejaVu Sans italic, icon/CJK-fallback faces).
const SANS_BOLD_BYTES: &[u8] = include_bytes!("../assets/LiberationSans-Bold.ttf");
const SANS_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationSans-Italic.ttf");
const SANS_BOLD_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationSans-BoldItalic.ttf");
const SERIF_BOLD_BYTES: &[u8] = include_bytes!("../assets/LiberationSerif-Bold.ttf");
const SERIF_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationSerif-Italic.ttf");
const SERIF_BOLD_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationSerif-BoldItalic.ttf");
const COURIER_BOLD_BYTES: &[u8] = include_bytes!("../assets/LiberationMono-Bold.ttf");
const COURIER_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationMono-Italic.ttf");
const COURIER_BOLD_ITALIC_BYTES: &[u8] = include_bytes!("../assets/LiberationMono-BoldItalic.ttf");
const MONO_BOLD_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono-Bold.ttf");
const MONO_ITALIC_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono-Oblique.ttf");
const MONO_BOLD_ITALIC_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono-BoldOblique.ttf");
/// DejaVu Sans ships no italic in the reference set — `system-ui` italic stays
/// synthetic (residual), exactly as the reference Chrome obliques it (measured:
/// italic advances equal regular's).
const SYSTEM_SANS_BOLD_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

/// One bundled face: `ab_glyph` outlines + a `rustybuzz` shaping face over the
/// same static bytes, so metrics and rasterization can never disagree about
/// which font they describe (ADR-0005 — no system fonts, no `fontdb`).
struct Face {
    ab: FontRef<'static>,
    rb: rustybuzz::Face<'static>,
    /// skrifa outline collection over the same bytes — the hinted rasterizer
    /// (FreeType-style light grid-fitting; see `hinted.rs`). Glyph ids are
    /// font indices shared by all three views.
    sk: skrifa::outline::OutlineGlyphCollection<'static>,
    /// The bundled bytes themselves — their address keys the per-(face, px)
    /// hinting-instance cache.
    bytes: &'static [u8],
}

impl Face {
    fn new(bytes: &'static [u8], what: &str) -> Self {
        let ab =
            FontRef::try_from_slice(bytes).unwrap_or_else(|_| panic!("bundled {what} is valid"));
        let rb = rustybuzz::Face::from_slice(bytes, 0)
            .unwrap_or_else(|| panic!("bundled {what} shapes"));
        let sk = skrifa::FontRef::new(bytes)
            .unwrap_or_else(|_| panic!("bundled {what} parses in skrifa"))
            .outline_glyphs();
        Self { ab, rb, sk, bytes }
    }

    /// Units-per-em. Scaling glyph advances by `px / upem` is the CSS
    /// convention (`font-size` is the em size): Chrome/FreeType scale exactly
    /// this way. (Scaling by ab_glyph's height metric — ascent−descent —
    /// rendered every face at `upem/height` of its CSS size: ~85% for Roboto,
    /// ~89% for the Liberation faces, so all content text was uniformly
    /// smaller and narrower than the reference and every wrap point drifted.)
    fn upem(&self) -> f32 {
        self.rb.units_per_em() as f32
    }
}

/// The style variants bundled for one font slot. Slots whose upstream ships
/// bold/italic files carry real variants; `styled` picks the closest real face
/// and reports the *residual* style — the bold/italic bits no bundled variant
/// covers — for the rasterizer to synthesize (smear/shear).
struct FaceSet {
    regular: Face,
    bold: Option<Face>,
    italic: Option<Face>,
    bold_italic: Option<Face>,
}

impl FaceSet {
    /// A slot with no style variants (Roboto UI text): every requested style is
    /// residual, preserving the previous all-synthetic behavior.
    fn regular_only(regular: Face) -> Self {
        Self {
            regular,
            bold: None,
            italic: None,
            bold_italic: None,
        }
    }

    /// The best bundled face for `style`, plus the residual style bits it could
    /// not satisfy (wanted bold+italic with only a bold face → residual italic;
    /// no variants at all → residual = requested).
    fn styled(&self, style: FontStyle) -> (&Face, FontStyle) {
        let residual = |bold: bool, italic: bool| FontStyle {
            bold,
            italic,
            icon: style.icon,
        };
        match (style.bold, style.italic) {
            (false, false) => (&self.regular, residual(false, false)),
            (true, false) => match &self.bold {
                Some(f) => (f, residual(false, false)),
                None => (&self.regular, residual(true, false)),
            },
            (false, true) => match &self.italic {
                Some(f) => (f, residual(false, false)),
                None => (&self.regular, residual(false, true)),
            },
            (true, true) => match (&self.bold_italic, &self.bold, &self.italic) {
                (Some(f), _, _) => (f, residual(false, false)),
                (None, Some(f), _) => (f, residual(false, true)),
                (None, None, Some(f)) => (f, residual(true, false)),
                (None, None, None) => (&self.regular, residual(true, true)),
            },
        }
    }
}

/// A software text shaper + rasterizer over the bundled text, icon, and fallback
/// fonts. Glyph **advances** come from rustybuzz (a HarfBuzz port) so run widths
/// — and therefore soft-wrap points — match a real browser (kerning, ligatures,
/// GPOS/GSUB), instead of `ab_glyph`'s per-character unkerned advances. Glyph
/// **outlining** stays on `ab_glyph`: rustybuzz returns font glyph indices, which
/// `ab_glyph` rasterizes by the same id, so shaping and rasterization decouple.
pub struct TextEngine {
    /// UI/chrome face (Roboto) — regular only; bold/italic stay synthetic.
    text: FaceSet,
    /// Per-`GenericFamily` content faces (metric-compatible with Times / Arial /
    /// Courier — see the byte constants above), each with the real bold/italic
    /// variants its upstream ships. ab_glyph outlines; rustybuzz shapes.
    serif: FaceSet,
    sans: FaceSet,
    mono: FaceSet,
    courier: FaceSet,
    system_sans: FaceSet,
    /// Single style-less faces: private-use icon glyphs and the CJK fallback.
    icon: Face,
    fallback: Face,
    /// Per-(face, px) skrifa hinting instances for the hinted raster path.
    hinter: hinted::HintCache,
}

impl TextEngine {
    /// Load the bundled text + icon + fallback fonts.
    pub fn new() -> Self {
        Self {
            text: FaceSet::regular_only(Face::new(FONT_BYTES, "Roboto")),
            serif: FaceSet {
                regular: Face::new(SERIF_FONT_BYTES, "Liberation Serif"),
                bold: Some(Face::new(SERIF_BOLD_BYTES, "Liberation Serif Bold")),
                italic: Some(Face::new(SERIF_ITALIC_BYTES, "Liberation Serif Italic")),
                bold_italic: Some(Face::new(
                    SERIF_BOLD_ITALIC_BYTES,
                    "Liberation Serif Bold Italic",
                )),
            },
            sans: FaceSet {
                regular: Face::new(SANS_FONT_BYTES, "Liberation Sans"),
                bold: Some(Face::new(SANS_BOLD_BYTES, "Liberation Sans Bold")),
                italic: Some(Face::new(SANS_ITALIC_BYTES, "Liberation Sans Italic")),
                bold_italic: Some(Face::new(
                    SANS_BOLD_ITALIC_BYTES,
                    "Liberation Sans Bold Italic",
                )),
            },
            mono: FaceSet {
                regular: Face::new(MONO_FONT_BYTES, "DejaVu Sans Mono"),
                bold: Some(Face::new(MONO_BOLD_BYTES, "DejaVu Sans Mono Bold")),
                italic: Some(Face::new(MONO_ITALIC_BYTES, "DejaVu Sans Mono Oblique")),
                bold_italic: Some(Face::new(
                    MONO_BOLD_ITALIC_BYTES,
                    "DejaVu Sans Mono Bold Oblique",
                )),
            },
            courier: FaceSet {
                regular: Face::new(COURIER_FONT_BYTES, "Liberation Mono"),
                bold: Some(Face::new(COURIER_BOLD_BYTES, "Liberation Mono Bold")),
                italic: Some(Face::new(COURIER_ITALIC_BYTES, "Liberation Mono Italic")),
                bold_italic: Some(Face::new(
                    COURIER_BOLD_ITALIC_BYTES,
                    "Liberation Mono Bold Italic",
                )),
            },
            system_sans: FaceSet {
                regular: Face::new(SYSTEM_SANS_FONT_BYTES, "DejaVu Sans"),
                bold: Some(Face::new(SYSTEM_SANS_BOLD_BYTES, "DejaVu Sans Bold")),
                // No italic in the upstream set — residual (synthetic shear),
                // matching the reference Chrome's synthesized oblique.
                italic: None,
                bold_italic: None,
            },
            icon: Face::new(ICON_FONT_BYTES, "icon font"),
            fallback: Face::new(FALLBACK_FONT_BYTES, "IPAGothic fallback"),
            hinter: hinted::HintCache::new(),
        }
    }

    /// The best bundled face for `slot` styled `style`, plus the residual style
    /// bits no bundled variant covers (which the rasterizer synthesizes). This
    /// is THE (slot, style) → face function: shaping derives glyph ids and
    /// advances through it and the rasterizer derives outlines through it, so
    /// the ids painted are always ids of the face that shaped them. Icon and
    /// CJK-fallback slots bundle a single face — any styling stays residual —
    /// and fallback runs keep their face regardless of the requested family.
    fn styled_face(&self, slot: FontSlot, style: FontStyle) -> (&Face, FontStyle) {
        let set = match slot {
            FontSlot::Text => &self.text,
            FontSlot::Serif => &self.serif,
            FontSlot::Monospace => &self.mono,
            FontSlot::Sans => &self.sans,
            FontSlot::CourierMono => &self.courier,
            FontSlot::SansSystem => &self.system_sans,
            FontSlot::Icon => return (&self.icon, style),
            FontSlot::Fallback => return (&self.fallback, style),
        };
        set.styled(style)
    }

    /// The regular (style-less) face of a slot — vertical metrics and the
    /// untouched single-style call sites read this.
    fn regular_face(&self, slot: FontSlot) -> &Face {
        self.styled_face(slot, FontStyle::REGULAR).0
    }

    /// The bundled font slot a `GenericFamily` renders in — each mapping
    /// measured against the reference Chrome: generic serif/sans request
    /// Times/Arial (→ the Liberation faces); generic monospace resolves to
    /// DejaVu Sans Mono while a named Courier gets Liberation Mono;
    /// `system-ui` is DejaVu Sans; and cursive/fantasy fall back to the
    /// STANDARD (serif) face, exactly as the reference does when their
    /// preferred faces are uninstalled.
    fn slot_for_family(family: GenericFamily) -> FontSlot {
        match family {
            GenericFamily::Serif | GenericFamily::Cursive | GenericFamily::Fantasy => {
                FontSlot::Serif
            }
            GenericFamily::Monospace => FontSlot::Monospace,
            GenericFamily::MonoCourier => FontSlot::CourierMono,
            GenericFamily::SansArial | GenericFamily::SansSerif => FontSlot::Sans,
            GenericFamily::SansSystem => FontSlot::SansSystem,
        }
    }

    /// The ab_glyph scale that rasterizes a `px` CSS font size in `face`:
    /// ab_glyph's `PxScale` divides by the face height (ascent−descent), so
    /// multiply it back out to net the CSS `px / upem` — keeping painted glyphs
    /// the same size the shaped advances promise. Takes the face (not a slot):
    /// bold/italic variants have their own height/upem ratios.
    fn px_scale_of(face: &Face, px: u32) -> PxScale {
        let h = face.ab.height_unscaled();
        let upem = face.ab.units_per_em().unwrap_or(h);
        PxScale::from(px.max(1) as f32 * h / upem.max(1.0))
    }

    /// [`px_scale_of`](Self::px_scale_of) for a slot's regular face.
    fn px_scale(&self, slot: FontSlot, px: u32) -> PxScale {
        Self::px_scale_of(self.regular_face(slot), px)
    }

    /// The space glyph's advance in `face`, scaled `px / upem`. Fractional
    /// (Liberation Sans @16px is 4.453px): the inline flow carries the
    /// sub-pixel remainder across gaps so wrap points match Chrome's.
    fn space_advance_of_f(face: &Face, px: u32) -> f32 {
        let units_to_px = px.max(1) as f32 / face.upem().max(1.0);
        face.rb
            .glyph_index(' ')
            .and_then(|g| face.rb.glyph_hor_advance(g))
            .map(|a| (a as f32 * units_to_px).max(0.0))
            .unwrap_or_else(|| (px.max(2) / 2) as f32)
    }

    /// The space advance in a slot's REGULAR face — shared by the family-less
    /// UI path (Text/Roboto) and the unstyled per-family content path.
    fn space_advance_in_f(&self, px: u32, slot: FontSlot) -> f32 {
        Self::space_advance_of_f(self.regular_face(slot), px)
    }

    fn space_advance_in(&self, px: u32, slot: FontSlot) -> u32 {
        self.space_advance_in_f(px, slot).round() as u32
    }

    /// Shape `text` at `px` with `primary` as the face for text-covered runs
    /// (CJK still itemizes to the fallback face). `shape` is `primary = Text`.
    fn shape_in(&self, text: &str, px: u32, primary: FontSlot) -> Vec<GlyphBox> {
        self.shape_in_styled(text, px, primary, FontStyle::REGULAR)
    }

    /// [`shape_in`](Self::shape_in) with a style: the styled face drives BOTH
    /// advances and glyph ids, through the same `styled_face(slot, style)`
    /// derivation the rasterizer applies per glyph — so the ids shaped here are
    /// ids of the exact face that will outline them. Fallback (CJK) runs keep
    /// the fallback face regardless of family or style.
    fn shape_in_styled(
        &self,
        text: &str,
        px: u32,
        primary: FontSlot,
        style: FontStyle,
    ) -> Vec<GlyphBox> {
        let mut out = Vec::with_capacity(text.len());
        let pxf = px.max(1) as f32;
        for (slot, run) in self.itemize(text) {
            // A text-covered run renders in the requested primary face; a fallback
            // (CJK) run keeps the fallback face regardless of family.
            let eff = if slot == FontSlot::Text {
                primary
            } else {
                slot
            };
            let (face, _residual) = self.styled_face(eff, style);
            let units_to_px = pxf / face.upem().max(1.0);
            shape_run_rb(&face.rb, run, px, eff, units_to_px, &mut out);
        }
        out
    }

    /// Which bundled face covers `ch`: the primary text font if it has a glyph
    /// (or the char is whitespace), else the CJK fallback, else the text font's
    /// `.notdef` (real tofu, as a browser with no matching font shows). This is
    /// the font-itemization a browser does before shaping.
    fn slot_for_char(&self, ch: char) -> FontSlot {
        if ch.is_whitespace() || self.text.regular.ab.glyph_id(ch).0 != 0 {
            FontSlot::Text
        } else if self.fallback.ab.glyph_id(ch).0 != 0 {
            FontSlot::Fallback
        } else {
            FontSlot::Text
        }
    }

    /// Split `text` into maximal same-face runs (font itemization), so each run
    /// is shaped by one face and adjacent kerning/ligatures apply within it.
    fn itemize<'t>(&self, text: &'t str) -> Vec<(FontSlot, &'t str)> {
        let mut runs: Vec<(FontSlot, &str)> = Vec::new();
        let mut cur: Option<(FontSlot, usize)> = None;
        for (i, ch) in text.char_indices() {
            let slot = self.slot_for_char(ch);
            match cur {
                Some((s, _)) if s == slot => {}
                Some((s, start)) => {
                    runs.push((s, &text[start..i]));
                    cur = Some((slot, i));
                }
                None => cur = Some((slot, i)),
            }
        }
        if let Some((s, start)) = cur {
            runs.push((s, &text[start..]));
        }
        runs
    }

    /// The regular ab_glyph face of a slot (vertical metrics, icon shaping).
    fn face_for(&self, slot: FontSlot) -> &FontRef<'static> {
        &self.regular_face(slot).ab
    }

    fn draw_run(
        &self,
        origin: Point,
        frac_x: f32,
        glyphs: &[GlyphBox],
        color: Color,
        style: FontStyle,
        target: &mut Framebuffer,
    ) {
        // The run's TRUE fractional origin: Chrome positions text runs at
        // fractional x; the pen starts there and every glyph rasterizes at
        // its exact sub-pixel position.
        let mut pen_x = origin.x as f32 + frac_x;
        for g in glyphs {
            // Each glyph names its own slot (a run can mix the primary face with
            // the CJK fallback); `styled_face` re-derives the exact face the glyph
            // was shaped from — same (slot, style) function — so `g.id` indexes
            // the face we outline. The baseline uses that face's ascent so mixed
            // scripts share a line. Residual style bits (bold/italic no bundled
            // variant covers — Roboto, DejaVu Sans italic, icon/fallback) are
            // synthesized: faux-bold smears a second sample 1px right, faux-italic
            // shears each scanline rightward above the baseline (~12°).
            let (face, residual) = self.styled_face(g.font, style);
            let scale = Self::px_scale_of(face, g.px);
            let scaled = face.ab.as_scaled(scale);
            // Integer baseline, exactly as Blink rounds font metrics before
            // rasterizing (lround(ascent) — 14 for 16px Arial-metric, not
            // 14.48): a fractional baseline drew every glyph ~half a pixel low
            // AND vertically smeared across an extra row, which alone
            // mismatched most ink pixels against Chrome on perfectly laid-out
            // lines.
            let baseline = origin.y as f32 + scaled.ascent().round();
            let slant = if residual.italic { 0.21f32 } else { 0.0 };

            // Hinted path (skrifa, FreeType-style light grid-fitting — see
            // `hinted.rs`, measured: 34.9% → 20.1% of ink pixels >32 gray
            // levels off Chrome): outline+fill only; ids, advances, and the
            // integer baseline are exactly the ab_glyph path's. Falls through
            // to the unhinted ab_glyph fill only if hinting fails for this
            // face/glyph.
            if self.hinter.draw_glyph(
                &face.sk,
                face.bytes.as_ptr() as usize,
                g.id,
                g.px,
                pen_x,
                baseline,
                color,
                residual,
                target,
            ) {
                pen_x += g.advance_f;
                continue;
            }

            let glyph = GlyphId(g.id).with_scale_and_position(scale, point(pen_x, baseline));
            if let Some(outlined) = face.ab.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let y = bounds.min.y as i32 + gy as i32;
                    let shear = if slant != 0.0 {
                        (slant * (baseline - y as f32)) as i32
                    } else {
                        0
                    };
                    let x = bounds.min.x as i32 + gx as i32 + shear;
                    target.blend_pixel(x, y, color, coverage);
                    // Faux-bold: smear one pixel to the right.
                    if residual.bold {
                        target.blend_pixel(x + 1, y, color, coverage);
                    }
                });
            }
            // TRUE fractional pen advance: each glyph rasterizes at its
            // sub-pixel x, matching Chrome's subpixel text positioning.
            pen_x += g.advance_f;
        }
    }

    /// Draw a decoded image into `rect` (nearest-neighbor, alpha-blended) with the
    /// given fit: `Fill` stretches; `Cover` scales to cover and crops; `Contain`
    /// fits inside and letterboxes (ADR-0044). `pos` anchors a `Cover`/`Contain`
    /// image in its box (`0.0`=left/top … `1.0`=right/bottom — ADR-0045).
    fn draw_image(
        &self,
        rect: Rect,
        image: &DecodedImage,
        fit: ImageFit,
        pos: ImagePos,
        pos_px: Point,
        target: &mut Framebuffer,
    ) {
        if rect.w == 0 || rect.h == 0 || image.size.w == 0 || image.size.h == 0 {
            return;
        }
        let (rw, rh) = (rect.w as f32, rect.h as f32);
        let (iw, ih) = (image.size.w as f32, image.size.h as f32);
        // Per-axis scale (px of source per px of dest) and an anchoring offset in
        // dest space; `Fill` is the stretch identity. The offset places the scaled
        // image so `pos` (0..1) of the leftover space is on the left/top — center
        // (0.5) reproduces the old centered crop/letterbox. `Auto` draws at natural
        // size (scale 1) anchored by `pos`, then shifted by the `pos_px` length —
        // this is the CSS-sprite path, where the source is clipped to the box.
        let (sxr, syr, mut off_x, mut off_y) = match fit {
            ImageFit::Fill => (iw / rw, ih / rh, 0.0, 0.0),
            ImageFit::Auto => (1.0, 1.0, pos.x * (rw - iw), pos.y * (rh - ih)),
            ImageFit::Cover => {
                let s = (rw / iw).max(rh / ih); // dest px per source px
                (
                    1.0 / s,
                    1.0 / s,
                    pos.x * (rw - iw * s),
                    pos.y * (rh - ih * s),
                )
            }
            ImageFit::Contain => {
                let s = (rw / iw).min(rh / ih);
                (
                    1.0 / s,
                    1.0 / s,
                    pos.x * (rw - iw * s),
                    pos.y * (rh - ih * s),
                )
            }
        };
        // The length component of `background-position` (e.g. `-304px`) shifts the
        // image within its box, on top of the fractional anchor.
        off_x += pos_px.x as f32;
        off_y += pos_px.y as f32;
        for dy in 0..rect.h {
            // Source row for this dest row (dest minus the centering offset).
            let syf = (dy as f32 - off_y) * syr;
            if syf < 0.0 || syf >= ih {
                continue; // letterbox band (Contain)
            }
            let sy = (syf as u32).min(image.size.h - 1);
            for dx in 0..rect.w {
                let sxf = (dx as f32 - off_x) * sxr;
                if sxf < 0.0 || sxf >= iw {
                    continue;
                }
                let sx = (sxf as u32).min(image.size.w - 1);
                let si = ((sy * image.size.w + sx) * 4) as usize;
                let a = image.rgba[si + 3] as f32 / 255.0;
                target.blend_pixel(
                    rect.x + dx as i32,
                    rect.y + dy as i32,
                    Color::rgb(image.rgba[si], image.rgba[si + 1], image.rgba[si + 2]),
                    a,
                );
            }
        }
    }

    /// Draw an anti-aliased, round-capped line of stroke `width` from `a` to `b`.
    /// Coverage is the distance from each pixel centre to the segment, so curves
    /// built from many short segments (icons) read smoothly.
    fn draw_line(&self, a: Point, b: Point, width: u32, color: Color, target: &mut Framebuffer) {
        let (ax, ay) = (a.x as f32, a.y as f32);
        let (bx, by) = (b.x as f32, b.y as f32);
        let half = (width.max(1) as f32) / 2.0;
        let pad = half + 1.0;
        let x0 = (ax.min(bx) - pad).floor() as i32;
        let x1 = (ax.max(bx) + pad).ceil() as i32;
        let y0 = (ay.min(by) - pad).floor() as i32;
        let y1 = (ay.max(by) + pad).ceil() as i32;
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        for py in y0..=y1 {
            for px in x0..=x1 {
                let (pxf, pyf) = (px as f32 + 0.5, py as f32 + 0.5);
                let t = if len2 > 0.0 {
                    (((pxf - ax) * dx + (pyf - ay) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (cx, cy) = (ax + t * dx, ay + t * dy);
                let dist = ((pxf - cx).powi(2) + (pyf - cy).powi(2)).sqrt();
                let cov = (half + 0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    target.blend_pixel(px, py, color, cov);
                }
            }
        }
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Shape one same-face run with rustybuzz and append its glyphs to `out`.
/// Advances are HarfBuzz-accurate (kerning/ligatures/GPOS). `units_to_px` is the
/// CSS scale factor `px / units_per_em` (`font-size` is the em size — Chrome
/// scales this way); rasterization matches through `px_scale`, which converts
/// the same net scale into ab_glyph's height-based `PxScale`, so run widths and
/// painted glyphs agree. Rounding is **run-accurate**: fractional advances
/// accumulate and the running sum is rounded, so each glyph's integer advance
/// sums to the true run width — a per-glyph round would drift wrap points from
/// Chrome.
fn shape_run_rb(
    face: &rustybuzz::Face<'_>,
    text: &str,
    px: u32,
    slot: FontSlot,
    units_to_px: f32,
    out: &mut Vec<GlyphBox>,
) {
    if text.is_empty() {
        return;
    }
    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    buf.guess_segment_properties();
    let shaped = rustybuzz::shape(face, &[], buf);
    let infos = shaped.glyph_infos();
    let positions = shaped.glyph_positions();
    let mut acc = 0.0f32;
    let mut prev = 0i32;
    for (info, pos) in infos.iter().zip(positions) {
        let advance_f = pos.x_advance as f32 * units_to_px;
        acc += advance_f;
        let rounded = acc.round() as i32;
        let advance = (rounded - prev).max(0) as u32;
        prev = rounded;
        out.push(GlyphBox {
            advance,
            advance_f,
            w: 0,
            h: 0,
            id: info.glyph_id as u16,
            px,
            font: slot,
        });
    }
}

impl TextShaper for TextEngine {
    fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
        self.shape_in(text, px, FontSlot::Text)
    }

    fn shape_with(&self, text: &str, px: u32, family: GenericFamily) -> Vec<GlyphBox> {
        self.shape_in(text, px, Self::slot_for_family(family))
    }

    /// Styled shaping: advances AND glyph ids come from the real bold/italic
    /// variant when the slot bundles one (Times bold is genuinely wider — a 10-H
    /// run at 100px is 777.8px vs the regular 722.2px), so wrap points match the
    /// reference. Slots without a variant shape from the regular face and the
    /// rasterizer synthesizes the residual style, as before.
    fn shape_styled(
        &self,
        text: &str,
        px: u32,
        family: GenericFamily,
        style: FontStyle,
    ) -> Vec<GlyphBox> {
        self.shape_in_styled(text, px, Self::slot_for_family(family), style)
    }

    fn shape_icon(&self, ch: char, px: u32) -> Vec<GlyphBox> {
        let scale = self.px_scale(FontSlot::Icon, px);
        let scaled = self.icon.ab.as_scaled(scale);
        let id = self.icon.ab.glyph_id(ch);
        let advance_f = scaled.h_advance(id).max(0.0);
        let advance = advance_f.round() as u32;
        vec![GlyphBox {
            advance,
            advance_f,
            w: 0,
            h: 0,
            id: id.0,
            px,
            font: FontSlot::Icon,
        }]
    }

    /// Read the space glyph's advance directly — identical to the first (only)
    /// element of `shape(" ", px)` but with no `Vec` allocation, since inline
    /// layout calls this once per word. A lone space carries no GPOS adjustment,
    /// so the plain glyph advance matches what rustybuzz would shape.
    fn space_advance(&self, px: u32) -> u32 {
        // The family-less path is the Roboto text face (matching `shape`), used
        // for the browser's own UI/chrome.
        self.space_advance_in(px, FontSlot::Text)
    }

    /// The space advance in the requested family's face — a monospace space is
    /// wider than a proportional one, so `<pre>`/`<code>` word gaps use the mono
    /// face's advance (keeping column alignment) rather than the sans space.
    /// Fractional; the trait's `space_advance_with` default rounds this.
    fn space_advance_with_f(&self, px: u32, family: GenericFamily) -> f32 {
        self.space_advance_in_f(px, Self::slot_for_family(family))
    }

    /// The space advance in the styled variant of the family's face — a bold
    /// space can be wider than a regular one (DejaVu Sans bold is), so styled
    /// runs' word gaps read the same face their glyphs shape from.
    fn space_advance_styled_f(&self, px: u32, family: GenericFamily, style: FontStyle) -> f32 {
        let (face, _residual) = self.styled_face(Self::slot_for_family(family), style);
        Self::space_advance_of_f(face, px)
    }

    /// `line-height: normal` exactly as Blink derives it: ascent, descent, and
    /// line gap are each scaled to px and rounded to integers INDIVIDUALLY,
    /// then summed — measured against this exact Chromium (Arial-metric 16px →
    /// 14+3+1 = 18, 13px → 12+3+0 = 15, 20px → 18+4+1 = 23, DejaVu Mono 16px →
    /// 15+4+0 = 19; a 20-line block's height is exactly 20× these). The result
    /// is a whole number of px — only *explicit* fractional line-heights
    /// (`line-height: 1.15`) accumulate sub-pixel pitch in Chrome, which is why
    /// this returns f32 through the same fractional plumbing.
    fn natural_leading_f(&self, px: u32, family: GenericFamily) -> f32 {
        let f = self.face_for(Self::slot_for_family(family));
        let upem = f
            .units_per_em()
            .unwrap_or_else(|| f.height_unscaled())
            .max(1.0);
        let s = px.max(1) as f32 / upem;
        let asc = (f.ascent_unscaled() * s).round();
        let desc = (-f.descent_unscaled() * s).round();
        let gap = (f.line_gap_unscaled() * s).round();
        asc + desc + gap
    }

    /// Ascent/descent with Blink's per-component rounding (the same components
    /// `natural_leading_f` sums) — the line box under a baseline-aligned inline
    /// image extends `descent + half-leading` below it.
    fn ascent_descent(&self, px: u32, family: GenericFamily) -> (i32, i32) {
        let f = self.face_for(Self::slot_for_family(family));
        let upem = f
            .units_per_em()
            .unwrap_or_else(|| f.height_unscaled())
            .max(1.0);
        let s = px.max(1) as f32 / upem;
        (
            (f.ascent_unscaled() * s).round() as i32,
            (-f.descent_unscaled() * s).round() as i32,
        )
    }
}

/// Lerp two colors at `t` in `0..=1`.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::rgba(m(a.r, b.r), m(a.g, b.g), m(a.b, b.b), m(a.a, b.a))
}

/// Fill a uniformly rounded rect (ADR-0041): the interior is opaque `fill_rect`
/// (fast), only the four `r×r` corners are anti-aliased per-pixel.
/// Fill a solid polygon by even-odd scanlines (`clip-path: polygon(...)`
/// backgrounds). Each scanline's edge crossings are sorted and the spans between
/// consecutive pairs are filled, honouring the framebuffer clip and alpha via
/// `fill_rect`. Hard-edged — exact for the axis-aligned step polygons real pages
/// use as section dividers; a slanted edge staircases by a pixel (acceptable, and
/// improvable with coverage later).
fn draw_polygon(points: &[Point], color: Color, target: &mut Framebuffer) {
    if points.len() < 3 || color.a == 0 {
        return;
    }
    let min_y = points.iter().map(|p| p.y).min().unwrap_or(0);
    let max_y = points.iter().map(|p| p.y).max().unwrap_or(0);
    let n = points.len();
    let mut xs: Vec<f32> = Vec::with_capacity(n);
    for y in min_y..max_y {
        // Sample at the pixel centre so a vertex exactly on a scanline doesn't
        // double-count (standard top-left fill rule).
        let yc = y as f32 + 0.5;
        xs.clear();
        for i in 0..n {
            let a = points[i];
            let b = points[(i + 1) % n];
            let (ay, by) = (a.y as f32, b.y as f32);
            // Edge crosses this scanline (half-open, so shared vertices count once).
            if (ay <= yc && by > yc) || (by <= yc && ay > yc) {
                let t = (yc - ay) / (by - ay);
                xs.push(a.x as f32 + t * (b.x as f32 - a.x as f32));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let x0 = xs[i].round() as i32;
            let x1 = xs[i + 1].round() as i32;
            if x1 > x0 {
                target.fill_rect(Rect::new(x0, y, (x1 - x0) as u32, 1), color);
            }
            i += 2;
        }
    }
}

fn draw_round_rect(rect: Rect, color: Color, radius: i32, target: &mut Framebuffer) {
    let r = radius.min(rect.w as i32 / 2).min(rect.h as i32 / 2).max(0);
    if r == 0 {
        target.fill_rect(rect, color);
        return;
    }
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.x + rect.w as i32, rect.y + rect.h as i32);
    let mid_w = (rect.w as i32 - 2 * r).max(0) as u32;
    target.fill_rect(
        Rect::new(x0, y0 + r, rect.w, (rect.h as i32 - 2 * r).max(0) as u32),
        color,
    );
    target.fill_rect(Rect::new(x0 + r, y0, mid_w, r as u32), color);
    target.fill_rect(Rect::new(x0 + r, y1 - r, mid_w, r as u32), color);
    let alpha = color.a as f32 / 255.0;
    let rf = r as f32;
    // (corner-square origin, circle center)
    for &(sx, sy, cx, cy) in &[
        (x0, y0, x0 + r, y0 + r),
        (x1 - r, y0, x1 - r, y0 + r),
        (x0, y1 - r, x0 + r, y1 - r),
        (x1 - r, y1 - r, x1 - r, y1 - r),
    ] {
        for py in sy..sy + r {
            for px in sx..sx + r {
                let d = (((px as f32 + 0.5) - cx as f32).powi(2)
                    + ((py as f32 + 0.5) - cy as f32).powi(2))
                .sqrt();
                let cov = (rf - d + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    target.blend_pixel(px, py, color, cov * alpha);
                }
            }
        }
    }
}

/// Fill a two-stop linear gradient (ADR-0041). The unrounded case fills one
/// opaque scanline (row or column) per step — fast; a rounded gradient falls back
/// to per-pixel with corner anti-aliasing.
fn draw_gradient(
    rect: Rect,
    start: Color,
    end: Color,
    vertical: bool,
    radius: i32,
    target: &mut Framebuffer,
) {
    let (w, h) = (rect.w as i32, rect.h as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2).max(0);
    if r == 0 {
        if vertical {
            for row in 0..h {
                let t = row as f32 / (h - 1).max(1) as f32;
                target.fill_rect(
                    Rect::new(rect.x, rect.y + row, rect.w, 1),
                    lerp_color(start, end, t),
                );
            }
        } else {
            for col in 0..w {
                let t = col as f32 / (w - 1).max(1) as f32;
                target.fill_rect(
                    Rect::new(rect.x + col, rect.y, 1, rect.h),
                    lerp_color(start, end, t),
                );
            }
        }
        return;
    }
    // Rounded gradient: per-pixel color + corner coverage.
    let (x0, y0) = (rect.x, rect.y);
    let rf = r as f32;
    for row in 0..h {
        for col in 0..w {
            let t = if vertical {
                row as f32 / (h - 1).max(1) as f32
            } else {
                col as f32 / (w - 1).max(1) as f32
            };
            let cov = corner_coverage(col, row, w, h, rf);
            if cov > 0.0 {
                target.blend_pixel(x0 + col, y0 + row, lerp_color(start, end, t), cov);
            }
        }
    }
}

/// Coverage in `0..=1` for a pixel at `(col,row)` inside a `w×h` box rounded by
/// `r` (1 in the interior, anti-aliased in the corners).
fn corner_coverage(col: i32, row: i32, w: i32, h: i32, r: f32) -> f32 {
    let (x, y) = (col as f32 + 0.5, row as f32 + 0.5);
    let cx = if x < r {
        r
    } else if x > w as f32 - r {
        w as f32 - r
    } else {
        return 1.0;
    };
    let cy = if y < r {
        r
    } else if y > h as f32 - r {
        h as f32 - r
    } else {
        return 1.0;
    };
    (r - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() + 0.5).clamp(0.0, 1.0)
}

/// Paint a soft outer drop shadow: only the ring outside `rect` (the box covers
/// the interior), alpha falling off quadratically over `blur` px (ADR-0041).
fn draw_shadow(rect: Rect, blur: i32, color: Color, target: &mut Framebuffer) {
    let b = blur.clamp(1, 40);
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.x + rect.w as i32, rect.y + rect.h as i32);
    let base = color.a as f32 / 255.0;
    for py in (y0 - b)..(y1 + b) {
        for px in (x0 - b)..(x1 + b) {
            let dx = if px < x0 {
                x0 - px
            } else if px >= x1 {
                px - x1 + 1
            } else {
                0
            };
            let dy = if py < y0 {
                y0 - py
            } else if py >= y1 {
                py - y1 + 1
            } else {
                0
            };
            if dx == 0 && dy == 0 {
                continue; // interior is covered by the box
            }
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d >= b as f32 {
                continue;
            }
            let t = 1.0 - d / b as f32;
            let a = base * t * t * 0.6;
            if a > 0.003 {
                target.blend_pixel(px, py, color, a);
            }
        }
    }
}

/// The vertical `[top, bottom]` device-pixel span an item can ink, or `None`
/// for the clip primitives (which paint nothing but manage the clip stack, so
/// they must never be culled). A small margin absorbs anti-aliasing, hinting,
/// and italic-shear spill so the viewport cull below can never drop a pixel a
/// draw op would have painted.
fn paint_vspan(item: &DisplayItem) -> Option<(i32, i32)> {
    const M: i32 = 2;
    let (top, bottom) = match item {
        DisplayItem::Rect { rect, .. }
        | DisplayItem::RoundRect { rect, .. }
        | DisplayItem::Gradient { rect, .. }
        | DisplayItem::Image { rect, .. } => (rect.y, rect.y + rect.h as i32),
        // A blurred shadow spreads `blur` px beyond its box on every side.
        DisplayItem::Shadow { rect, blur, .. } => {
            let b = *blur as i32;
            (rect.y - b, rect.y + rect.h as i32 + b)
        }
        // `origin.y` is the top of the run's boxes; ink stays within the box
        // height (see `content_height`).
        DisplayItem::Glyphs { origin, glyphs, .. } => {
            let h = glyphs.iter().map(|g| g.h as i32).max().unwrap_or(0);
            (origin.y, origin.y + h)
        }
        DisplayItem::Line { a, b, width, .. } => {
            let w = *width as i32;
            (a.y.min(b.y) - w, a.y.max(b.y) + w)
        }
        DisplayItem::Polygon { points, .. } => {
            let mut lo = i32::MAX;
            let mut hi = i32::MIN;
            for p in points {
                lo = lo.min(p.y);
                hi = hi.max(p.y);
            }
            if lo > hi {
                return None; // empty polygon: nothing to paint, nothing to cull
            }
            (lo, hi)
        }
        DisplayItem::ClipPush { .. } | DisplayItem::ClipPop => return None,
    };
    Some((top - M, bottom + M))
}

impl Rasterizer for TextEngine {
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer) {
        // Clip stack: each push intersects with the current clip (ADR-0043).
        let mut clips: Vec<Rect> = Vec::new();
        // Viewport cull: a painting primitive whose vertical span lies entirely
        // above the surface or below it produces no pixels — draw ops already
        // clip to the framebuffer — so skip the dispatch. Clip push/pop carry no
        // ink but manage the clip stack, so they are never culled (`paint_vspan`
        // returns `None`). This makes per-frame paint cost scale with the visible
        // slice, not the whole document: scrolling a long page (e.g. cnn.com) no
        // longer re-dispatches tens of thousands of off-screen glyph runs.
        let surface_h = target.size.h as i32;
        for item in &list.items {
            if let Some((top, bottom)) = paint_vspan(item) {
                if bottom < 0 || top >= surface_h {
                    continue;
                }
            }
            match item {
                DisplayItem::Rect { rect, color } => target.fill_rect(*rect, *color),
                DisplayItem::RoundRect {
                    rect,
                    color,
                    radius,
                } => draw_round_rect(*rect, *color, *radius as i32, target),
                DisplayItem::Gradient {
                    rect,
                    start,
                    end,
                    vertical,
                    radius,
                } => draw_gradient(*rect, *start, *end, *vertical, *radius as i32, target),
                DisplayItem::Shadow { rect, blur, color } => {
                    draw_shadow(*rect, *blur as i32, *color, target)
                }
                DisplayItem::Image {
                    rect,
                    image,
                    fit,
                    pos,
                    pos_px,
                } => self.draw_image(*rect, image, *fit, *pos, *pos_px, target),
                DisplayItem::Glyphs {
                    origin,
                    frac_x,
                    glyphs,
                    color,
                    style,
                } => self.draw_run(*origin, *frac_x, glyphs, *color, *style, target),
                DisplayItem::Line { a, b, width, color } => {
                    self.draw_line(*a, *b, *width, *color, target)
                }
                DisplayItem::Polygon { points, color } => draw_polygon(points, *color, target),
                DisplayItem::ClipPush { rect } => {
                    let r = clips
                        .last()
                        .map_or(*rect, |prev| intersect_rect(*prev, *rect));
                    clips.push(r);
                    target.set_clip(Some(r));
                }
                DisplayItem::ClipPop => {
                    clips.pop();
                    target.set_clip(clips.last().copied());
                }
            }
        }
        target.set_clip(None);
    }
}

/// The intersection of two rects (empty if they don't overlap).
fn intersect_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w as i32).min(b.x + b.w as i32);
    let y1 = (a.y + a.h as i32).min(b.y + b.h as i32);
    Rect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_types::Size;

    #[test]
    fn image_fit_fill_cover_contain() {
        // A wide 20x10 source: column 0 green, the rest red. Drawn into a square
        // 20x20 box over white, the three fits read distinctly (ADR-0044).
        let mut rgba = vec![0u8; 20 * 10 * 4];
        for y in 0..10 {
            for x in 0..20 {
                let i = (y * 20 + x) * 4;
                let (r, g) = if x == 0 { (0, 255) } else { (255, 0) };
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 3] = 255;
            }
        }
        let image = DecodedImage {
            size: Size::new(20, 10),
            rgba,
        };
        let draw = |fit: ImageFit| {
            let mut fb = Framebuffer::new(Size::new(20, 20));
            fb.fill_rect(Rect::new(0, 0, 20, 20), Color::WHITE);
            TextEngine::new().draw_image(
                Rect::new(0, 0, 20, 20),
                &image,
                fit,
                ImagePos::CENTER,
                Point::ZERO,
                &mut fb,
            );
            fb
        };
        // Fill stretches the whole source in: the left edge column (green) shows.
        let fill = draw(ImageFit::Fill);
        assert_eq!(
            fill.pixel(0, 10).unwrap(),
            Color::rgb(0, 255, 0),
            "fill: edge"
        );
        // Cover scales to cover the box and crops the sides — the green edge is
        // cropped away (red there) and every pixel is painted (no letterbox).
        let cover = draw(ImageFit::Cover);
        assert_eq!(
            cover.pixel(0, 10).unwrap(),
            Color::rgb(255, 0, 0),
            "cover: edge cropped"
        );
        assert_eq!(
            cover.pixel(10, 1).unwrap(),
            Color::rgb(255, 0, 0),
            "cover: full coverage, no letterbox"
        );
        // Contain fits inside, leaving top/bottom letterbox bands untouched.
        let contain = draw(ImageFit::Contain);
        assert_eq!(
            contain.pixel(10, 1).unwrap(),
            Color::WHITE,
            "contain: top letterbox band"
        );
        assert_eq!(
            contain.pixel(10, 10).unwrap(),
            Color::rgb(255, 0, 0),
            "contain: image painted in the middle band"
        );
    }

    #[test]
    fn image_position_shifts_cover_crop_and_contain_letterbox() {
        // Same wide 20x10 source: column 0 green, the rest red.
        let mut rgba = vec![0u8; 20 * 10 * 4];
        for y in 0..10 {
            for x in 0..20 {
                let i = (y * 20 + x) * 4;
                let (r, g) = if x == 0 { (0, 255) } else { (255, 0) };
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 3] = 255;
            }
        }
        let image = DecodedImage {
            size: Size::new(20, 10),
            rgba,
        };
        let draw = |fit: ImageFit, pos: ImagePos| {
            let mut fb = Framebuffer::new(Size::new(20, 20));
            fb.fill_rect(Rect::new(0, 0, 20, 20), Color::WHITE);
            TextEngine::new().draw_image(
                Rect::new(0, 0, 20, 20),
                &image,
                fit,
                pos,
                Point::ZERO,
                &mut fb,
            );
            fb
        };
        // Cover anchored left keeps the green left edge; centered crops it away.
        let left = draw(ImageFit::Cover, ImagePos { x: 0.0, y: 0.5 });
        assert_eq!(
            left.pixel(0, 10).unwrap(),
            Color::rgb(0, 255, 0),
            "cover left: source left edge shown"
        );
        let center = draw(ImageFit::Cover, ImagePos::CENTER);
        assert_eq!(
            center.pixel(0, 10).unwrap(),
            Color::rgb(255, 0, 0),
            "cover center: left edge cropped"
        );
        // Contain top paints the top band and letterboxes the bottom; bottom flips it.
        let top = draw(ImageFit::Contain, ImagePos { x: 0.5, y: 0.0 });
        assert_eq!(
            top.pixel(10, 1).unwrap(),
            Color::rgb(255, 0, 0),
            "contain top"
        );
        assert_eq!(
            top.pixel(10, 18).unwrap(),
            Color::WHITE,
            "contain top: bottom band"
        );
        let bottom = draw(ImageFit::Contain, ImagePos { x: 0.5, y: 1.0 });
        assert_eq!(
            bottom.pixel(10, 1).unwrap(),
            Color::WHITE,
            "contain bottom: top band"
        );
        assert_eq!(
            bottom.pixel(10, 18).unwrap(),
            Color::rgb(255, 0, 0),
            "contain bottom"
        );
    }

    #[test]
    fn round_rect_clears_corners_keeps_center() {
        let mut fb = Framebuffer::new(Size::new(20, 20));
        fb.fill_rect(Rect::new(0, 0, 20, 20), Color::WHITE);
        draw_round_rect(Rect::new(0, 0, 20, 20), Color::rgb(0, 0, 0), 8, &mut fb);
        assert!(fb.pixel(0, 0).unwrap().r > 200, "corner stays background");
        assert_eq!(
            fb.pixel(10, 10).unwrap(),
            Color::rgb(0, 0, 0),
            "center filled"
        );
    }

    #[test]
    fn vertical_gradient_interpolates_top_to_bottom() {
        let mut fb = Framebuffer::new(Size::new(4, 10));
        fb.fill_rect(Rect::new(0, 0, 4, 10), Color::BLACK);
        draw_gradient(
            Rect::new(0, 0, 4, 10),
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
            true,
            0,
            &mut fb,
        );
        let top = fb.pixel(0, 0).unwrap().r;
        let bottom = fb.pixel(0, 9).unwrap().r;
        assert!(
            bottom > top + 150,
            "bottom is lighter than top: {top} -> {bottom}"
        );
    }

    #[test]
    fn clip_drops_content_outside_the_box() {
        let mut fb = Framebuffer::new(Size::new(40, 40));
        fb.fill_rect(Rect::new(0, 0, 40, 40), Color::WHITE);
        let list = DisplayList {
            items: vec![
                DisplayItem::ClipPush {
                    rect: Rect::new(0, 0, 20, 20),
                },
                DisplayItem::Rect {
                    rect: Rect::new(0, 0, 40, 40),
                    color: Color::rgb(255, 0, 0),
                },
                DisplayItem::ClipPop,
            ],
        };
        TextEngine::new().rasterize(&list, &mut fb);
        assert_eq!(
            fb.pixel(5, 5).unwrap(),
            Color::rgb(255, 0, 0),
            "inside clip painted"
        );
        assert_eq!(
            fb.pixel(30, 30).unwrap(),
            Color::WHITE,
            "outside clip dropped"
        );
    }

    #[test]
    fn shadow_inks_the_ring_not_center() {
        let mut fb = Framebuffer::new(Size::new(60, 60));
        fb.fill_rect(Rect::new(0, 0, 60, 60), Color::WHITE);
        draw_shadow(
            Rect::new(20, 20, 20, 20),
            8,
            Color::rgba(0, 0, 0, 255),
            &mut fb,
        );
        // A pixel just outside the box is darkened; the interior is untouched.
        assert!(fb.pixel(15, 30).unwrap().r < 250, "shadow darkens the ring");
        assert_eq!(
            fb.pixel(30, 30).unwrap(),
            Color::WHITE,
            "interior untouched"
        );
    }

    #[test]
    fn shapes_glyph_ids_and_advances() {
        let engine = TextEngine::new();
        let glyphs = engine.shape("Hi", 24);
        assert_eq!(glyphs.len(), 2);
        // Real glyphs have non-zero ids and advances.
        assert!(glyphs.iter().all(|g| g.id != 0));
        assert!(glyphs.iter().all(|g| g.advance > 0));
    }

    #[test]
    fn cjk_falls_back_to_the_bundled_fallback_face() {
        // Latin comes from the primary text font; CJK the primary font can't render
        // is shaped from the bundled fallback (real glyph, not tofu) — a browser's
        // font substitution. Both get non-zero ids and advances.
        let engine = TextEngine::new();

        let latin = &engine.shape("A", 24)[0];
        assert_eq!(latin.font, FontSlot::Text);
        assert!(latin.id != 0);

        for cjk in ["日", "本", "語", "中", "文"] {
            let g = &engine.shape(cjk, 24)[0];
            assert_eq!(
                g.font,
                FontSlot::Fallback,
                "{cjk} should use the fallback face"
            );
            assert!(g.id != 0, "{cjk} resolved to a real fallback glyph");
            assert!(g.advance > 0, "{cjk} has a positive advance");
        }
    }

    #[test]
    fn space_advance_matches_shaping_a_space() {
        // The allocation-free override must return exactly what the general
        // `shape(" ", px)` path would, so inline layout geometry is unchanged.
        let engine = TextEngine::new();
        for px in [12u32, 16, 24, 40, 100] {
            let via_shape: u32 = engine.shape(" ", px).iter().map(|g| g.advance).sum();
            assert_eq!(
                engine.space_advance(px),
                via_shape,
                "space_advance must equal shape(\" \") at {px}px"
            );
        }
    }

    #[test]
    fn rasterizes_real_ink() {
        let engine = TextEngine::new();
        let mut list = DisplayList::new();
        list.push(DisplayItem::Glyphs {
            origin: Point::new(2, 2),
            frac_x: 0.0,
            glyphs: engine.shape("A", 40),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        let mut fb = Framebuffer::new(Size::new(48, 48));
        fb.clear(Color::WHITE);
        engine.rasterize(&list, &mut fb);

        // Some pixels were inked (not all white), and stayed within bounds.
        let inked = fb
            .rgba
            .chunks_exact(4)
            .filter(|px| px[..3] != [255, 255, 255])
            .count();
        assert!(inked > 0, "expected anti-aliased glyph ink");
    }

    #[test]
    fn viewport_cull_is_output_identical_to_the_full_list() {
        // Culling off-screen primitives must produce byte-identical pixels to
        // rasterizing the whole list — it only skips dispatch for items that
        // would paint nothing. Build a tall document and render just the top
        // slice; the visible pixels must match whether or not the off-screen
        // runs above and below are present.
        let engine = TextEngine::new();
        let glyphs = engine.shape("The quick brown fox", 16);
        let visible = || DisplayItem::Glyphs {
            origin: Point::new(3, 20),
            frac_x: 0.0,
            glyphs: glyphs.clone(),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        };
        // A list with only the on-screen run.
        let lean = DisplayList {
            items: vec![visible()],
        };
        // The same on-screen run surrounded by many off-screen runs (far above
        // the surface and far below it) plus off-screen rects.
        let mut items = Vec::new();
        for i in 1..=500 {
            items.push(DisplayItem::Glyphs {
                origin: Point::new(3, -20 * i), // above the surface
                frac_x: 0.0,
                glyphs: glyphs.clone(),
                color: Color::rgb(255, 0, 0),
                style: FontStyle::REGULAR,
            });
            items.push(DisplayItem::Rect {
                rect: Rect::new(0, 5000 + 20 * i, 100, 10), // below the surface
                color: Color::rgb(0, 255, 0),
            });
        }
        items.push(visible());
        let fat = DisplayList { items };

        let render = |list: &DisplayList| {
            let mut fb = Framebuffer::new(Size::new(200, 48));
            fb.clear(Color::WHITE);
            engine.rasterize(list, &mut fb);
            fb
        };
        assert_eq!(
            render(&lean).rgba,
            render(&fat).rgba,
            "off-screen items must not change the visible pixels"
        );
    }

    #[test]
    fn viewport_cull_preserves_the_clip_stack() {
        // An off-screen paint item is culled, but the clip push/pop that bracket
        // it must still be honoured for the on-screen item that follows inside
        // the same clip. Otherwise culling would leak or drop a clip level.
        let mut fb = Framebuffer::new(Size::new(40, 40));
        fb.fill_rect(Rect::new(0, 0, 40, 40), Color::WHITE);
        let list = DisplayList {
            items: vec![
                DisplayItem::ClipPush {
                    rect: Rect::new(0, 0, 20, 40),
                },
                // Off-screen (far below): culled, but must not disturb the clip.
                DisplayItem::Rect {
                    rect: Rect::new(0, 9000, 40, 10),
                    color: Color::rgb(0, 0, 255),
                },
                // On-screen: painted, and must still be clipped to x<20.
                DisplayItem::Rect {
                    rect: Rect::new(0, 0, 40, 40),
                    color: Color::rgb(255, 0, 0),
                },
                DisplayItem::ClipPop,
            ],
        };
        TextEngine::new().rasterize(&list, &mut fb);
        assert_eq!(
            fb.pixel(5, 5).unwrap(),
            Color::rgb(255, 0, 0),
            "inside the clip still paints after a culled item"
        );
        assert_eq!(
            fb.pixel(30, 5).unwrap(),
            Color::WHITE,
            "clip still bounds the on-screen item (push/pop survived the cull)"
        );
    }

    #[test]
    fn glyph_cache_renders_identically_warm_and_cold() {
        // The rendered-glyph cache must be byte-identical whether a glyph is
        // rasterized fresh or blitted from cache — otherwise scrolling (which
        // re-renders the same glyphs from a warm cache) would shimmer against a
        // first paint. Uses a fractional origin so the sub-pixel path is exercised.
        let engine = TextEngine::new();
        let glyphs = engine.shape("Reading gy 123", 16);
        let render = |e: &TextEngine| {
            let mut list = DisplayList::new();
            list.push(DisplayItem::Glyphs {
                origin: Point::new(3, 30),
                frac_x: 0.37,
                glyphs: glyphs.clone(),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            let mut fb = Framebuffer::new(Size::new(200, 48));
            fb.clear(Color::WHITE);
            e.rasterize(&list, &mut fb);
            fb
        };
        let warm_first = render(&engine); // cold miss → populates the cache
        let warm_second = render(&engine); // fully warm → all blits
        assert_eq!(
            warm_first.rgba, warm_second.rgba,
            "a warm re-render (the scroll case) must match the first paint"
        );
        let cold = render(&TextEngine::new());
        assert_eq!(
            cold.rgba, warm_first.rgba,
            "cold-cache render must equal warm — a blit reproduces a fresh raster"
        );
        assert!(
            warm_first
                .rgba
                .chunks_exact(4)
                .any(|p| p[..3] != [255, 255, 255]),
            "expected glyph ink"
        );
    }

    #[test]
    fn icon_font_has_the_toolbar_glyphs() {
        let e = TextEngine::new();
        // users (MIRC), reload, gear, back, forward, close, eye, trash — all
        // present (non-.notdef).
        for cp in [
            '\u{e972}', '\u{e984}', '\u{e994}', '\u{ea38}', '\u{ea34}', '\u{ea0f}', '\u{e9ce}',
            '\u{e9ac}',
        ] {
            let g = e.shape_icon(cp, 16);
            assert_eq!(g.len(), 1);
            assert!(g[0].id != 0, "missing icon glyph U+{:04X}", cp as u32);
        }
    }

    /// Sum of a styled run's advances at `px` (what layout measures).
    fn styled_width(e: &TextEngine, text: &str, fam: GenericFamily, style: FontStyle) -> f32 {
        e.shape_styled(text, 100, fam, style)
            .iter()
            .map(|g| g.advance)
            .sum::<u32>() as f32
    }

    #[test]
    fn real_bold_faces_match_reference_advances() {
        // Reference Chromium (chromium-1194, --headless=new), 10×'H' at 100px:
        // Times New Roman bold 777.84 (regular 722.17) — bold Times is
        // genuinely WIDER, which no faux-bold smear can reproduce; Arial bold
        // 722.17 (same as regular); monospace bold 602.06 (fixed pitch).
        let e = TextEngine::new();
        let h10 = "HHHHHHHHHH";
        let serif_reg = styled_width(&e, h10, GenericFamily::Serif, FontStyle::REGULAR);
        let serif_bold = styled_width(&e, h10, GenericFamily::Serif, FontStyle::bold());
        assert!(
            (serif_bold - 777.84).abs() < 1.0,
            "Times bold 10H = {serif_bold}, Chrome 777.84"
        );
        assert!(
            (serif_reg - 722.17).abs() < 1.0,
            "Times regular 10H = {serif_reg}, Chrome 722.17"
        );
        assert!(
            serif_bold > serif_reg + 10.0,
            "bold serif is a real, wider face"
        );
        let sans_bold = styled_width(&e, h10, GenericFamily::SansSerif, FontStyle::bold());
        assert!(
            (sans_bold - 722.17).abs() < 1.0,
            "Arial bold 10H = {sans_bold}, Chrome 722.17"
        );
        let mono_bold = styled_width(&e, h10, GenericFamily::Monospace, FontStyle::bold());
        assert!(
            (mono_bold - 602.06).abs() < 1.0,
            "monospace bold 10H = {mono_bold}, Chrome 602.06"
        );
    }

    #[test]
    fn styled_lowercase_runs_match_reference() {
        // Reference Chromium 'Hamburgefonstiv' at 100px (measured on this exact
        // binary): Arial bold 822.33 vs regular 755.92; Times italic 690.69 and
        // bold-italic 722.27 (regular 698.05) — real variant faces reshape
        // lowercase advances, not just stems.
        let e = TextEngine::new();
        let t = "Hamburgefonstiv";
        let italic = FontStyle {
            bold: false,
            italic: true,
            icon: false,
        };
        let bold_italic = FontStyle {
            bold: true,
            italic: true,
            icon: false,
        };
        let sans_reg = styled_width(&e, t, GenericFamily::SansSerif, FontStyle::REGULAR);
        let sans_bold = styled_width(&e, t, GenericFamily::SansSerif, FontStyle::bold());
        assert!(
            (sans_reg - 755.92).abs() < 1.0,
            "Arial regular = {sans_reg}, Chrome 755.92"
        );
        assert!(
            (sans_bold - 822.33).abs() < 1.0,
            "Arial bold = {sans_bold}, Chrome 822.33"
        );
        let serif_italic = styled_width(&e, t, GenericFamily::Serif, italic);
        assert!(
            (serif_italic - 690.69).abs() < 1.0,
            "Times italic = {serif_italic}, Chrome 690.69"
        );
        let serif_bold_italic = styled_width(&e, t, GenericFamily::Serif, bold_italic);
        assert!(
            (serif_bold_italic - 722.27).abs() < 1.0,
            "Times bold-italic = {serif_bold_italic}, Chrome 722.27"
        );
    }

    #[test]
    fn slots_without_a_variant_report_residual_style() {
        // SansSystem (DejaVu Sans) bundles a bold but NO italic file — italic
        // stays residual for the rasterizer to shear, matching the reference
        // Chrome, which synthesizes the oblique (measured: italic advances
        // equal regular's). Bold+italic serves from the bold face with italic
        // residual. Roboto (Text) has no variants at all.
        let e = TextEngine::new();
        let italic = FontStyle {
            bold: false,
            italic: true,
            icon: false,
        };
        let bold_italic = FontStyle {
            bold: true,
            italic: true,
            icon: false,
        };
        let (_, r) = e.styled_face(FontSlot::SansSystem, italic);
        assert!(r.italic && !r.bold, "SansSystem italic is residual");
        let (_, r) = e.styled_face(FontSlot::SansSystem, FontStyle::bold());
        assert!(!r.bold && !r.italic, "SansSystem bold is a real face");
        let (_, r) = e.styled_face(FontSlot::SansSystem, bold_italic);
        assert!(
            !r.bold && r.italic,
            "SansSystem bold+italic: real bold, residual italic"
        );
        let (_, r) = e.styled_face(FontSlot::Text, bold_italic);
        assert!(
            r.bold && r.italic,
            "Roboto has no variants — full residual style"
        );
        // A slot with the full set clears every residual bit.
        let (_, r) = e.styled_face(FontSlot::Serif, bold_italic);
        assert!(!r.bold && !r.italic, "Serif bold-italic is a real face");
    }

    #[test]
    fn styled_shaping_and_rasterization_share_the_face() {
        // The glyph ids a styled run shapes must index the face the rasterizer
        // outlines (`styled_face` is the single derivation): bold Liberation
        // Serif's 'H' is a different glyph id in a different file than the
        // regular's, and both must outline real ink.
        let e = TextEngine::new();
        let style = FontStyle::bold();
        let glyphs = e.shape_styled("H", 40, GenericFamily::Serif, style);
        assert_eq!(glyphs.len(), 1);
        let (face, residual) = e.styled_face(glyphs[0].font, style);
        assert!(!residual.bold, "serif bold is real, not residual");
        // The shaped id resolves to an outline in the styled face.
        let scale = TextEngine::px_scale_of(face, 40);
        let glyph = GlyphId(glyphs[0].id).with_scale_and_position(scale, point(0.0, 30.0));
        assert!(
            face.ab.outline_glyph(glyph).is_some(),
            "styled id outlines in the styled face"
        );
        // And the run rasterizes ink end-to-end.
        let mut list = DisplayList::new();
        list.push(DisplayItem::Glyphs {
            origin: Point::new(2, 2),
            frac_x: 0.0,
            glyphs,
            color: Color::BLACK,
            style,
        });
        let mut fb = Framebuffer::new(Size::new(48, 48));
        fb.clear(Color::WHITE);
        e.rasterize(&list, &mut fb);
        let inked = fb
            .rgba
            .chunks_exact(4)
            .filter(|px| px[..3] != [255, 255, 255])
            .count();
        assert!(inked > 0, "styled glyph rasterizes real ink");
    }

    #[test]
    fn skrifa_and_rustybuzz_agree_on_glyph_ids() {
        // The hinted rasterizer outlines glyphs by the ids rustybuzz shaped.
        // Both read the same bundled bytes, so a codepoint's glyph index must
        // be identical through skrifa's cmap — for every content face.
        let e = TextEngine::new();
        let faces: [(&Face, &str); 4] = [
            (&e.sans.regular, "sans"),
            (&e.serif.regular, "serif"),
            (&e.mono.regular, "mono"),
            (&e.text.regular, "roboto"),
        ];
        for (face, name) in faces {
            let sk = skrifa::FontRef::new(face.bytes).unwrap();
            let cmap = sk.charmap();
            for ch in "Hamburgefonstiv 0123456789".chars() {
                let rb = face.rb.glyph_index(ch).map(|g| g.0 as u32);
                let sk_id = cmap.map(ch).map(|g| g.to_u32());
                assert_eq!(rb, sk_id, "{name}: glyph id for {ch:?} diverges");
            }
        }
    }

    #[test]
    fn hinted_advances_stay_within_half_a_pixel_of_rustybuzz() {
        // Light hinting grid-fits outlines vertically but must not disturb
        // horizontal metrics. skrifa (like FreeType) reports the hinted
        // advance ROUNDED to an integer, so exact parity with rustybuzz's
        // fractional advance is `round(advance_f)` — a delta of at most 0.5px.
        // Layout keeps rustybuzz's unrounded advances (as Blink keeps
        // HarfBuzz's, with subpixel positioning); this asserts the two views
        // describe the same metric, i.e. hinting moved nothing horizontally.
        use skrifa::{
            instance::{LocationRef, Size},
            outline::{DrawSettings, HintingInstance, HintingOptions, OutlinePen, SmoothMode},
            MetadataProvider,
        };
        struct NullPen;
        impl OutlinePen for NullPen {
            fn move_to(&mut self, _: f32, _: f32) {}
            fn line_to(&mut self, _: f32, _: f32) {}
            fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
            fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
            fn close(&mut self) {}
        }
        let e = TextEngine::new();
        for px in [13u32, 16, 24] {
            for (face, fam) in [
                (&e.sans.regular, GenericFamily::SansSerif),
                (&e.serif.regular, GenericFamily::Serif),
            ] {
                let sk = skrifa::FontRef::new(face.bytes).unwrap();
                let outlines = sk.outline_glyphs();
                let hinter = HintingInstance::new(
                    &outlines,
                    Size::new(px as f32),
                    LocationRef::default(),
                    HintingOptions {
                        // Same configuration as the production path in
                        // `hinted.rs`: auto-hinter, light target.
                        engine: skrifa::outline::Engine::Auto(None),
                        target: skrifa::outline::Target::Smooth {
                            mode: SmoothMode::Light,
                            symmetric_rendering: true,
                            preserve_linear_metrics: false,
                        },
                    },
                )
                .unwrap();
                for g in e.shape_with("Hamburgefonstiv", px, fam) {
                    let glyph = outlines
                        .get(skrifa::GlyphId::new(g.id as u32))
                        .expect("glyph outlines");
                    let m = glyph
                        .draw(DrawSettings::hinted(&hinter, false), &mut NullPen)
                        .unwrap();
                    if let Some(adv) = m.advance_width {
                        assert!(
                            (adv - g.advance_f).abs() <= 0.5 + 1e-3,
                            "{fam:?}@{px}px glyph {}: hinted advance {adv} vs rustybuzz {}",
                            g.id,
                            g.advance_f
                        );
                        assert!(
                            (adv - g.advance_f.round()).abs() < 1e-3,
                            "{fam:?}@{px}px glyph {}: hinted advance {adv} is not \
                             round({}) — hinting moved horizontal metrics",
                            g.id,
                            g.advance_f
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn hinted_path_rasterizes_ink_for_every_content_face() {
        // The skrifa hinted path must draw real ink (not fall back silently)
        // for every bundled face and calibration size — and be deterministic
        // (same bytes, same interpreter → same pixels; the farbling stance
        // never feeds system state into the raster).
        let e = TextEngine::new();
        let draw = |fam: GenericFamily, px: u32| {
            let mut list = DisplayList::new();
            list.push(DisplayItem::Glyphs {
                origin: Point::new(2, 2),
                frac_x: 0.0,
                glyphs: e.shape_with("Hg", px, fam),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            let mut fb = Framebuffer::new(Size::new(64, 48));
            fb.clear(Color::WHITE);
            e.rasterize(&list, &mut fb);
            fb.rgba
        };
        for fam in [
            GenericFamily::SansSerif,
            GenericFamily::Serif,
            GenericFamily::Monospace,
            GenericFamily::MonoCourier,
            GenericFamily::SansSystem,
        ] {
            for px in [13u32, 16, 24] {
                let a = draw(fam, px);
                let inked = a
                    .chunks_exact(4)
                    .filter(|p| p[..3] != [255, 255, 255])
                    .count();
                assert!(inked > 0, "{fam:?}@{px}px inked no pixels");
                let b = draw(fam, px);
                assert_eq!(a, b, "{fam:?}@{px}px raster is not deterministic");
            }
        }
    }

    #[test]
    fn shape_with_selects_the_bundled_face_per_family() {
        let e = TextEngine::new();
        // Each generic family shapes its text-covered glyphs from the matching
        // bundled face (so the rasterizer outlines the right shapes).
        let slot = |fam: GenericFamily| e.shape_with("Ag", 16, fam)[0].font;
        // Generic and named-Arial sans share the Arial-metric Liberation Sans
        // (Chrome's generic sans-serif requests Arial → fontconfig → Liberation).
        assert_eq!(slot(GenericFamily::SansSerif), FontSlot::Sans);
        assert_eq!(slot(GenericFamily::SansArial), FontSlot::Sans);
        assert_eq!(slot(GenericFamily::Serif), FontSlot::Serif);
        // Generic monospace is DejaVu Sans Mono; a named Courier is the
        // Courier-metric Liberation Mono; system-ui is DejaVu Sans (measured).
        assert_eq!(slot(GenericFamily::Monospace), FontSlot::Monospace);
        assert_eq!(slot(GenericFamily::MonoCourier), FontSlot::CourierMono);
        assert_eq!(slot(GenericFamily::SansSystem), FontSlot::SansSystem);
        // Cursive and fantasy fall back to the STANDARD serif, as the
        // reference does when their preferred faces are uninstalled.
        assert_eq!(slot(GenericFamily::Cursive), FontSlot::Serif);
        assert_eq!(slot(GenericFamily::Fantasy), FontSlot::Serif);
        // The monospace space is wider than the proportional one (fixed pitch).
        assert!(
            e.space_advance_with(16, GenericFamily::Monospace)
                > e.space_advance_with(16, GenericFamily::SansSerif),
            "monospace space is wider"
        );
    }
}

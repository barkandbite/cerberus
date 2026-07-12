//! Software text adapter (ADR-0005).
//!
//! Wraps `ab_glyph` and a **bundled** Roboto font (Apache-2.0) behind our paint
//! traits. One [`TextEngine`] implements both `TextShaper` (char → glyph ids +
//! advances) and `Rasterizer` (paints rects/images, and anti-aliased glyph
//! outlines). System fonts are never read — the font set is fixed, which is both
//! reproducible and an anti-fingerprinting choice (see ADR-0005).
//!
//! Glyph **advances** come from `rustybuzz` (a pure-Rust HarfBuzz port) — real
//! shaping with kerning/ligatures/GPOS/GSUB, the foundation for complex scripts —
//! while `ab_glyph` remains the outline **rasterizer** (glyph ids are shared font
//! indices). Both read the same bundled bytes, so text metrics stay reproducible
//! (ADR-0005). See `RENDERING_ARCHITECTURE_PLAN.md`.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, FontSlot, Framebuffer, GlyphBox, Rasterizer, TextShaper,
};
use cerberus_types::{Color, FontStyle, GenericFamily, ImageFit, ImagePos, Point, Rect};

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

/// A software text shaper + rasterizer over the bundled text, icon, and fallback
/// fonts. Glyph **advances** come from rustybuzz (a HarfBuzz port) so run widths
/// — and therefore soft-wrap points — match a real browser (kerning, ligatures,
/// GPOS/GSUB), instead of `ab_glyph`'s per-character unkerned advances. Glyph
/// **outlining** stays on `ab_glyph`: rustybuzz returns font glyph indices, which
/// `ab_glyph` rasterizes by the same id, so shaping and rasterization decouple.
pub struct TextEngine {
    font: FontRef<'static>,
    icon_font: FontRef<'static>,
    fallback_font: FontRef<'static>,
    /// rustybuzz shaping faces over the same bundled bytes (deterministic — no
    /// system fonts, no `fontdb`; ADR-0005).
    rb_font: rustybuzz::Face<'static>,
    rb_fallback: rustybuzz::Face<'static>,
    /// Bundled serif + monospace faces (metric-compatible with Times / Courier),
    /// selected per `GenericFamily` so `font-family` presents the right shape
    /// class. ab_glyph outlines; rustybuzz shapes — same split as the text face.
    serif_font: FontRef<'static>,
    mono_font: FontRef<'static>,
    sans_font: FontRef<'static>,
    courier_font: FontRef<'static>,
    system_sans_font: FontRef<'static>,
    rb_serif: rustybuzz::Face<'static>,
    rb_mono: rustybuzz::Face<'static>,
    rb_sans: rustybuzz::Face<'static>,
    rb_courier: rustybuzz::Face<'static>,
    rb_system_sans: rustybuzz::Face<'static>,
}

impl TextEngine {
    /// Load the bundled text + icon + fallback fonts.
    pub fn new() -> Self {
        let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled Roboto font is valid");
        let icon_font =
            FontRef::try_from_slice(ICON_FONT_BYTES).expect("bundled icon font is valid");
        let fallback_font = FontRef::try_from_slice(FALLBACK_FONT_BYTES)
            .expect("bundled IPAGothic fallback font is valid");
        let rb_font =
            rustybuzz::Face::from_slice(FONT_BYTES, 0).expect("bundled Roboto font shapes");
        let rb_fallback = rustybuzz::Face::from_slice(FALLBACK_FONT_BYTES, 0)
            .expect("bundled IPAGothic fallback shapes");
        let serif_font =
            FontRef::try_from_slice(SERIF_FONT_BYTES).expect("bundled Liberation Serif is valid");
        let mono_font =
            FontRef::try_from_slice(MONO_FONT_BYTES).expect("bundled DejaVu Sans Mono is valid");
        let rb_serif = rustybuzz::Face::from_slice(SERIF_FONT_BYTES, 0)
            .expect("bundled Liberation Serif shapes");
        let rb_mono = rustybuzz::Face::from_slice(MONO_FONT_BYTES, 0)
            .expect("bundled DejaVu Sans Mono shapes");
        let sans_font =
            FontRef::try_from_slice(SANS_FONT_BYTES).expect("bundled Liberation Sans is valid");
        let rb_sans = rustybuzz::Face::from_slice(SANS_FONT_BYTES, 0)
            .expect("bundled Liberation Sans shapes");
        let courier_font =
            FontRef::try_from_slice(COURIER_FONT_BYTES).expect("bundled Liberation Mono is valid");
        let rb_courier = rustybuzz::Face::from_slice(COURIER_FONT_BYTES, 0)
            .expect("bundled Liberation Mono shapes");
        let system_sans_font =
            FontRef::try_from_slice(SYSTEM_SANS_FONT_BYTES).expect("bundled DejaVu Sans is valid");
        let rb_system_sans = rustybuzz::Face::from_slice(SYSTEM_SANS_FONT_BYTES, 0)
            .expect("bundled DejaVu Sans shapes");
        Self {
            font,
            icon_font,
            fallback_font,
            rb_font,
            rb_fallback,
            serif_font,
            mono_font,
            sans_font,
            courier_font,
            system_sans_font,
            rb_serif,
            rb_mono,
            rb_sans,
            rb_courier,
            rb_system_sans,
        }
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

    /// The rustybuzz face for a slot, plus its units-per-em. Scaling glyph
    /// advances by `px / upem` is the CSS convention (`font-size` is the em
    /// size): Chrome/FreeType scale exactly this way. (Scaling by ab_glyph's
    /// height metric — ascent−descent — rendered every face at `upem/height`
    /// of its CSS size: ~85% for Roboto, ~89% for the Liberation faces, so all
    /// content text was uniformly smaller and narrower than the reference and
    /// every wrap point drifted.)
    fn shaping_face(&self, slot: FontSlot) -> (&rustybuzz::Face<'static>, f32) {
        let face = match slot {
            FontSlot::Fallback => &self.rb_fallback,
            FontSlot::Serif => &self.rb_serif,
            FontSlot::Monospace => &self.rb_mono,
            FontSlot::Sans => &self.rb_sans,
            FontSlot::CourierMono => &self.rb_courier,
            FontSlot::SansSystem => &self.rb_system_sans,
            _ => &self.rb_font,
        };
        (face, face.units_per_em() as f32)
    }

    /// The ab_glyph scale that rasterizes a `px` CSS font size: ab_glyph's
    /// `PxScale` divides by the face height (ascent−descent), so multiply it
    /// back out to net the CSS `px / upem` — keeping painted glyphs the same
    /// size the shaped advances promise.
    fn px_scale(&self, slot: FontSlot, px: u32) -> PxScale {
        let f = self.face_for(slot);
        let h = f.height_unscaled();
        let upem = f.units_per_em().unwrap_or(h);
        PxScale::from(px.max(1) as f32 * h / upem.max(1.0))
    }

    /// The space glyph's advance in a slot's face, scaled `px / upem` — shared
    /// by the family-less UI path (Text/Roboto) and the per-family content path.
    fn space_advance_in(&self, px: u32, slot: FontSlot) -> u32 {
        let (f, upem) = self.shaping_face(slot);
        let units_to_px = px.max(1) as f32 / upem.max(1.0);
        f.glyph_index(' ')
            .and_then(|g| f.glyph_hor_advance(g))
            .map(|a| (a as f32 * units_to_px).round().max(0.0) as u32)
            .unwrap_or_else(|| px.max(2) / 2)
    }

    /// Shape `text` at `px` with `primary` as the face for text-covered runs
    /// (CJK still itemizes to the fallback face). `shape` is `primary = Text`.
    fn shape_in(&self, text: &str, px: u32, primary: FontSlot) -> Vec<GlyphBox> {
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
            let (face, upem) = self.shaping_face(eff);
            let units_to_px = pxf / upem.max(1.0);
            shape_run_rb(face, run, px, eff, units_to_px, &mut out);
        }
        out
    }

    /// Which bundled face covers `ch`: the primary text font if it has a glyph
    /// (or the char is whitespace), else the CJK fallback, else the text font's
    /// `.notdef` (real tofu, as a browser with no matching font shows). This is
    /// the font-itemization a browser does before shaping.
    fn slot_for_char(&self, ch: char) -> FontSlot {
        if ch.is_whitespace() || self.font.glyph_id(ch).0 != 0 {
            FontSlot::Text
        } else if self.fallback_font.glyph_id(ch).0 != 0 {
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

    /// The face a glyph was shaped from.
    fn face_for(&self, slot: FontSlot) -> &FontRef<'static> {
        match slot {
            FontSlot::Text => &self.font,
            FontSlot::Icon => &self.icon_font,
            FontSlot::Fallback => &self.fallback_font,
            FontSlot::Serif => &self.serif_font,
            FontSlot::Monospace => &self.mono_font,
            FontSlot::Sans => &self.sans_font,
            FontSlot::CourierMono => &self.courier_font,
            FontSlot::SansSystem => &self.system_sans_font,
        }
    }

    fn draw_run(
        &self,
        origin: Point,
        glyphs: &[GlyphBox],
        color: Color,
        style: FontStyle,
        target: &mut Framebuffer,
    ) {
        let mut pen_x = origin.x as f32;
        // Synthetic styling — memory-first, no extra font faces (real weight/slant
        // faces would be a drop-in asset swap behind this path). Faux-bold smears a
        // second sample 1px right; faux-italic shears each scanline rightward above
        // the baseline (~12°).
        let slant = if style.italic { 0.21f32 } else { 0.0 };
        for g in glyphs {
            // Each glyph names its own face (a run can mix Roboto + CJK fallback);
            // the baseline uses that face's ascent so mixed scripts share a line.
            let font = self.face_for(g.font);
            let scale = self.px_scale(g.font, g.px);
            let scaled = font.as_scaled(scale);
            let baseline = origin.y as f32 + scaled.ascent();

            let glyph = GlyphId(g.id).with_scale_and_position(scale, point(pen_x, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
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
                    if style.bold {
                        target.blend_pixel(x + 1, y, color, coverage);
                    }
                });
            }
            pen_x += g.advance as f32;
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
        acc += pos.x_advance as f32 * units_to_px;
        let rounded = acc.round() as i32;
        let advance = (rounded - prev).max(0) as u32;
        prev = rounded;
        out.push(GlyphBox {
            advance,
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

    fn shape_icon(&self, ch: char, px: u32) -> Vec<GlyphBox> {
        let scale = self.px_scale(FontSlot::Icon, px);
        let scaled = self.icon_font.as_scaled(scale);
        let id = self.icon_font.glyph_id(ch);
        let advance = scaled.h_advance(id).round().max(0.0) as u32;
        vec![GlyphBox {
            advance,
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
    fn space_advance_with(&self, px: u32, family: GenericFamily) -> u32 {
        self.space_advance_in(px, Self::slot_for_family(family))
    }

    /// `line-height: normal` from the face's real vertical metrics —
    /// (ascent − descent + line gap) / upem, exactly what Chrome derives:
    /// ~1.15× for the Times/Arial-metric Liberation faces, ~1.17× for Roboto.
    /// Kept fractional (16px Arial-metric → 18.398): layout accumulates the
    /// exact pitch and rounds per line, matching Chrome's baseline positions.
    fn natural_leading_f(&self, px: u32, family: GenericFamily) -> f32 {
        let f = self.face_for(Self::slot_for_family(family));
        let h = f.height_unscaled() + f.line_gap_unscaled();
        let upem = f.units_per_em().unwrap_or_else(|| f.height_unscaled());
        px.max(1) as f32 * h / upem.max(1.0)
    }
}

/// Lerp two colors at `t` in `0..=1`.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::rgba(m(a.r, b.r), m(a.g, b.g), m(a.b, b.b), m(a.a, b.a))
}

/// Fill a uniformly rounded rect (ADR-0041): the interior is opaque `fill_rect`
/// (fast), only the four `r×r` corners are anti-aliased per-pixel.
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

impl Rasterizer for TextEngine {
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer) {
        // Clip stack: each push intersects with the current clip (ADR-0043).
        let mut clips: Vec<Rect> = Vec::new();
        for item in &list.items {
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
                    glyphs,
                    color,
                    style,
                } => self.draw_run(*origin, glyphs, *color, *style, target),
                DisplayItem::Line { a, b, width, color } => {
                    self.draw_line(*a, *b, *width, *color, target)
                }
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

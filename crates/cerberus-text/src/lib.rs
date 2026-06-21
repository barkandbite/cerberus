//! Software text adapter (ADR-0005).
//!
//! Wraps `ab_glyph` and a **bundled** Roboto font (Apache-2.0) behind our paint
//! traits. One [`TextEngine`] implements both `TextShaper` (char → glyph ids +
//! advances) and `Rasterizer` (paints rects/images, and anti-aliased glyph
//! outlines). System fonts are never read — the font set is fixed, which is both
//! reproducible and an anti-fingerprinting choice (see ADR-0005).
//!
//! Shaping here is per-character (good for Latin); complex-script shaping
//! (rustybuzz) can be added later behind the same `TextShaper` trait with no
//! caller changes. ab_glyph is chosen over swash as a leaner first rasterizer.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, Framebuffer, GlyphBox, Rasterizer, TextShaper,
};
use cerberus_types::{Color, FontStyle, Point, Rect};

/// The bundled font (Roboto Regular, Apache-2.0). See `assets/Roboto-LICENSE.txt`.
const FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Regular.ttf");
/// Bundled icon font (user-supplied IcoMoon subset). See `assets/icomoon-LICENSE.txt`.
const ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/icomoon.ttf");

/// A software text shaper + rasterizer over the bundled text and icon fonts.
pub struct TextEngine {
    font: FontRef<'static>,
    icon_font: FontRef<'static>,
}

impl TextEngine {
    /// Load the bundled text + icon fonts.
    pub fn new() -> Self {
        let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled Roboto font is valid");
        let icon_font =
            FontRef::try_from_slice(ICON_FONT_BYTES).expect("bundled icon font is valid");
        Self { font, icon_font }
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
        // Icon runs are outlined from the icon font; everything else from Roboto.
        let font = if style.icon {
            &self.icon_font
        } else {
            &self.font
        };
        // Synthetic styling — memory-first, no extra font faces (real weight/slant
        // faces would be a drop-in asset swap behind this path). Faux-bold smears a
        // second sample 1px right; faux-italic shears each scanline rightward above
        // the baseline (~12°).
        let slant = if style.italic { 0.21f32 } else { 0.0 };
        for g in glyphs {
            let scale = PxScale::from(g.px.max(1) as f32);
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

    /// Draw a decoded image scaled (nearest-neighbor) into `rect`, alpha-blended.
    fn draw_image(&self, rect: Rect, image: &DecodedImage, target: &mut Framebuffer) {
        if rect.w == 0 || rect.h == 0 || image.size.w == 0 || image.size.h == 0 {
            return;
        }
        for dy in 0..rect.h {
            let sy = (dy * image.size.h / rect.h).min(image.size.h - 1);
            for dx in 0..rect.w {
                let sx = (dx * image.size.w / rect.w).min(image.size.w - 1);
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

impl TextShaper for TextEngine {
    fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
        let scale = PxScale::from(px.max(1) as f32);
        let scaled = self.font.as_scaled(scale);
        text.chars()
            .map(|ch| {
                let id = self.font.glyph_id(ch);
                let advance = scaled.h_advance(id).round().max(0.0) as u32;
                GlyphBox {
                    advance,
                    w: 0,
                    h: 0,
                    id: id.0,
                    px,
                }
            })
            .collect()
    }

    fn shape_icon(&self, ch: char, px: u32) -> Vec<GlyphBox> {
        let scale = PxScale::from(px.max(1) as f32);
        let scaled = self.icon_font.as_scaled(scale);
        let id = self.icon_font.glyph_id(ch);
        let advance = scaled.h_advance(id).round().max(0.0) as u32;
        vec![GlyphBox {
            advance,
            w: 0,
            h: 0,
            id: id.0,
            px,
        }]
    }
}

impl Rasterizer for TextEngine {
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer) {
        for item in &list.items {
            match item {
                DisplayItem::Rect { rect, color } => target.fill_rect(*rect, *color),
                DisplayItem::Image { rect, image } => self.draw_image(*rect, image, target),
                DisplayItem::Glyphs {
                    origin,
                    glyphs,
                    color,
                    style,
                } => self.draw_run(*origin, glyphs, *color, *style, target),
                DisplayItem::Line { a, b, width, color } => {
                    self.draw_line(*a, *b, *width, *color, target)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_types::Size;

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
}

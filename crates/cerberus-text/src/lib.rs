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
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, GlyphBox, Rasterizer, TextShaper};
use cerberus_types::{Color, Point};

/// The bundled font (Roboto Regular, Apache-2.0). See `assets/Roboto-LICENSE.txt`.
const FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Regular.ttf");

/// A software text shaper + rasterizer over the bundled font.
pub struct TextEngine {
    font: FontRef<'static>,
}

impl TextEngine {
    /// Load the bundled font.
    pub fn new() -> Self {
        let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled Roboto font is valid");
        Self { font }
    }

    fn draw_run(&self, origin: Point, glyphs: &[GlyphBox], color: Color, target: &mut Framebuffer) {
        let mut pen_x = origin.x as f32;
        for g in glyphs {
            let scale = PxScale::from(g.px.max(1) as f32);
            let scaled = self.font.as_scaled(scale);
            let baseline = origin.y as f32 + scaled.ascent();

            let glyph = GlyphId(g.id).with_scale_and_position(scale, point(pen_x, baseline));
            if let Some(outlined) = self.font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let x = bounds.min.x as i32 + gx as i32;
                    let y = bounds.min.y as i32 + gy as i32;
                    target.blend_pixel(x, y, color, coverage);
                });
            }
            pen_x += g.advance as f32;
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
}

impl Rasterizer for TextEngine {
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer) {
        for item in &list.items {
            match item {
                DisplayItem::Rect { rect, color } => target.fill_rect(*rect, *color),
                DisplayItem::Image { rect, .. } => {
                    target.fill_rect(*rect, Color::rgb(192, 192, 192))
                }
                DisplayItem::Glyphs {
                    origin,
                    glyphs,
                    color,
                } => self.draw_run(*origin, glyphs, *color, target),
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
}

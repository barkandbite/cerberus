//! Painting: the display-list representation, an in-memory framebuffer, and the
//! three paint traits that wrap historically CVE-heavy third-party code.
//!
//! `Rasterizer`, `TextShaper`, and `ImageDecoder` are the seams for font
//! rasterization, text shaping, and image decoding. Real adapters wrapping
//! approved crates land at M2; this crate ships only the traits plus deliberately
//! trivial built-in stubs so the M0 render path is end-to-end.

use cerberus_types::{Color, Point, Rect, Size};

/// One drawing primitive in a resolution-independent display list.
#[derive(Clone, Debug)]
pub enum DisplayItem {
    /// A solid-filled rectangle.
    Rect { rect: Rect, color: Color },
    /// A run of shaped glyphs anchored at `origin` (top-left of the first box).
    Glyphs {
        origin: Point,
        glyphs: Vec<GlyphBox>,
        color: Color,
    },
    /// A decoded image placed into `rect` (referenced by id; decoding is the
    /// `ImageDecoder`'s job).
    Image { rect: Rect, image_id: u32 },
}

/// A flat, ordered list of paint primitives produced by layout.
#[derive(Clone, Debug, Default)]
pub struct DisplayList {
    pub items: Vec<DisplayItem>,
}

impl DisplayList {
    /// An empty display list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an item.
    pub fn push(&mut self, item: DisplayItem) {
        self.items.push(item);
    }
}

/// A shaped glyph reduced to its metrics. Real glyph ids/outlines arrive with
/// the M2 shaping adapter; the box is enough to lay out and paint placeholders.
#[derive(Clone, Copy, Debug)]
pub struct GlyphBox {
    /// Horizontal advance after this glyph.
    pub advance: u32,
    /// Inked width.
    pub w: u32,
    /// Inked height.
    pub h: u32,
}

/// An RGBA8 framebuffer (row-major, top-left origin).
#[derive(Clone, Debug)]
pub struct Framebuffer {
    pub size: Size,
    pub rgba: Vec<u8>,
}

impl Framebuffer {
    /// Allocate a transparent framebuffer of the given size.
    pub fn new(size: Size) -> Self {
        let len = (size.area() * 4) as usize;
        Self {
            size,
            rgba: vec![0; len],
        }
    }

    /// Fill the entire buffer with one color.
    pub fn clear(&mut self, c: Color) {
        for px in self.rgba.chunks_exact_mut(4) {
            px[0] = c.r;
            px[1] = c.g;
            px[2] = c.b;
            px[3] = c.a;
        }
    }

    /// Fill a rectangle (opaque write, clipped to bounds).
    pub fn fill_rect(&mut self, rect: Rect, c: Color) {
        let x0 = rect.x.max(0) as u32;
        let y0 = rect.y.max(0) as u32;
        let x1 = ((rect.x + rect.w as i32).max(0) as u32).min(self.size.w);
        let y1 = ((rect.y + rect.h as i32).max(0) as u32).min(self.size.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let idx = ((y * self.size.w + x) * 4) as usize;
                self.rgba[idx] = c.r;
                self.rgba[idx + 1] = c.g;
                self.rgba[idx + 2] = c.b;
                self.rgba[idx + 3] = c.a;
            }
        }
    }

    /// Read a pixel, if in bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.size.w || y >= self.size.h {
            return None;
        }
        let idx = ((y * self.size.w + x) * 4) as usize;
        Some(Color::rgba(
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ))
    }

    /// Copy `src` into this framebuffer with its top-left at `dest` (opaque
    /// copy, clipped to bounds). Used to composite the page under the toolbar.
    pub fn blit(&mut self, dest: Point, src: &Framebuffer) {
        for sy in 0..src.size.h {
            let dy = dest.y + sy as i32;
            if dy < 0 || dy as u32 >= self.size.h {
                continue;
            }
            for sx in 0..src.size.w {
                let dx = dest.x + sx as i32;
                if dx < 0 || dx as u32 >= self.size.w {
                    continue;
                }
                let si = ((sy * src.size.w + sx) * 4) as usize;
                let di = ((dy as u32 * self.size.w + dx as u32) * 4) as usize;
                self.rgba[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
            }
        }
    }
}

/// A decoded raster image.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub size: Size,
    pub rgba: Vec<u8>,
}

/// Errors from the paint subsystem.
#[derive(Clone, Debug)]
pub enum PaintError {
    /// The image bytes could not be decoded.
    Decode(String),
}

/// Turns a `DisplayList` into pixels. Wraps a font rasterizer (M2).
pub trait Rasterizer: Send {
    /// Rasterize `list` into `target`.
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer);
}

/// Shapes text into positioned glyphs. Wraps a shaping engine (M2).
pub trait TextShaper: Send + Sync {
    /// Shape `text` at the given pixel size into glyph boxes.
    fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox>;
}

/// Decodes image bytes. Wraps image decoders (M2) — a historically large CVE
/// surface, hence behind a trait from day one.
pub trait ImageDecoder: Send + Sync {
    /// Decode `bytes` into an RGBA image.
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, PaintError>;
}

/// Built-in placeholder shaper: fixed-width boxes, one per non-space character.
/// Stands in until the M2 shaping adapter lands.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonoShaper;

impl TextShaper for MonoShaper {
    fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
        let cell = px.max(2);
        text.chars()
            .map(|ch| {
                if ch.is_whitespace() {
                    GlyphBox {
                        advance: cell / 2,
                        w: 0,
                        h: 0,
                    }
                } else {
                    GlyphBox {
                        advance: cell / 2,
                        w: cell / 2 - 1,
                        h: cell,
                    }
                }
            })
            .collect()
    }
}

/// Built-in placeholder rasterizer: fills rects and draws glyphs as solid
/// boxes (so text is visibly present). Real outlines arrive at M2.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoxRasterizer;

impl Rasterizer for BoxRasterizer {
    fn rasterize(&self, list: &DisplayList, target: &mut Framebuffer) {
        for item in &list.items {
            match item {
                DisplayItem::Rect { rect, color } => target.fill_rect(*rect, *color),
                DisplayItem::Glyphs {
                    origin,
                    glyphs,
                    color,
                } => {
                    let mut pen_x = origin.x;
                    for g in glyphs {
                        if g.w > 0 && g.h > 0 {
                            target.fill_rect(Rect::new(pen_x, origin.y, g.w, g.h), *color);
                        }
                        pen_x += g.advance as i32;
                    }
                }
                DisplayItem::Image { rect, .. } => {
                    target.fill_rect(*rect, Color::rgb(192, 192, 192));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rect_is_clipped_and_readable() {
        let mut fb = Framebuffer::new(Size::new(4, 4));
        fb.clear(Color::WHITE);
        fb.fill_rect(Rect::new(-2, -2, 4, 4), Color::BLACK);
        assert_eq!(fb.pixel(0, 0), Some(Color::BLACK));
        assert_eq!(fb.pixel(3, 3), Some(Color::WHITE));
        assert_eq!(fb.pixel(4, 4), None);
    }

    #[test]
    fn stub_pipeline_paints_text_boxes() {
        let glyphs = MonoShaper.shape("hi", 8);
        let mut list = DisplayList::new();
        list.push(DisplayItem::Glyphs {
            origin: Point::new(0, 0),
            glyphs,
            color: Color::BLACK,
        });
        let mut fb = Framebuffer::new(Size::new(16, 16));
        fb.clear(Color::WHITE);
        BoxRasterizer.rasterize(&list, &mut fb);
        assert_eq!(fb.pixel(0, 0), Some(Color::BLACK));
    }
}

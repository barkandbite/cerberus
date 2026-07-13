//! Painting: the display-list representation, an in-memory framebuffer, and the
//! three paint traits that wrap historically CVE-heavy third-party code.
//!
//! `Rasterizer`, `TextShaper`, and `ImageDecoder` are the seams for font
//! rasterization, text shaping, and image decoding. Real adapters wrapping
//! approved crates land at M2; this crate ships only the traits plus deliberately
//! trivial built-in stubs so the M0 render path is end-to-end.

use cerberus_types::{Color, FontStyle, GenericFamily, ImageFit, ImagePos, Point, Rect, Size};
use std::sync::Arc;

/// One drawing primitive in a resolution-independent display list.
#[derive(Clone, Debug)]
pub enum DisplayItem {
    /// A solid-filled rectangle.
    Rect {
        rect: Rect,
        color: Color,
    },
    /// A solid fill with uniform rounded corners (ADR-0041).
    RoundRect {
        rect: Rect,
        color: Color,
        radius: u16,
    },
    /// A two-stop linear gradient fill (vertical or horizontal), optionally
    /// rounded (ADR-0041).
    Gradient {
        rect: Rect,
        start: Color,
        end: Color,
        vertical: bool,
        radius: u16,
    },
    /// A soft outer drop shadow with `blur`-px falloff (ADR-0041).
    Shadow {
        rect: Rect,
        blur: u16,
        color: Color,
    },
    /// A run of shaped glyphs anchored at `origin` (top-left of the first box).
    Glyphs {
        origin: Point,
        glyphs: Vec<GlyphBox>,
        color: Color,
        style: FontStyle,
    },
    /// A decoded image (shared) to draw into `rect` with the given fit and
    /// position (where a `Cover`/`Contain` image anchors — ADR-0045). `pos_px` is
    /// an additional pixel offset applied after `pos` — the length component of
    /// `background-position` (e.g. `0 -304px` for a CSS sprite), in dest pixels.
    Image {
        rect: Rect,
        image: Arc<DecodedImage>,
        fit: ImageFit,
        pos: ImagePos,
        pos_px: Point,
    },
    /// An anti-aliased, round-capped line segment of the given stroke width.
    /// Vector UI (icons) is built from these, so it scales crisply with
    /// [`DisplayList::scaled`].
    Line {
        a: Point,
        b: Point,
        width: u32,
        color: Color,
    },
    /// Push a clip rectangle (intersected with the current clip); items paint
    /// only inside it until the matching [`DisplayItem::ClipPop`] — ADR-0043.
    ClipPush {
        rect: Rect,
    },
    ClipPop,
}

/// Offset every primitive in `items` by `(dx, dy)` in place. Used to reuse a
/// display list laid at the origin at its final position without re-shaping (the
/// taffy engine flows an inline leaf once, while measuring, then translates it
/// into place at paint time).
pub fn translate_items(items: &mut [DisplayItem], dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    for it in items {
        match it {
            DisplayItem::Rect { rect, .. }
            | DisplayItem::RoundRect { rect, .. }
            | DisplayItem::Gradient { rect, .. }
            | DisplayItem::Shadow { rect, .. }
            | DisplayItem::Image { rect, .. }
            | DisplayItem::ClipPush { rect } => {
                rect.x += dx;
                rect.y += dy;
            }
            DisplayItem::Glyphs { origin, .. } => {
                origin.x += dx;
                origin.y += dy;
            }
            DisplayItem::Line { a, b, .. } => {
                a.x += dx;
                a.y += dy;
                b.x += dx;
                b.y += dy;
            }
            DisplayItem::ClipPop => {}
        }
    }
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

    /// A copy with every coordinate, size, and glyph pixel-size multiplied by
    /// `scale`. Glyph runs are re-scaled (`px` + `advance`), so the rasterizer
    /// re-outlines them at the larger size — **crisp**, not a bitmap upscale.
    /// Used to paint a logical-pixel UI onto a HiDPI (physical-pixel) surface.
    pub fn scaled(&self, scale: f32) -> DisplayList {
        if (scale - 1.0).abs() < f32::EPSILON {
            return self.clone();
        }
        let si = |v: i32| (v as f32 * scale).round() as i32;
        let su = |v: u32| (v as f32 * scale).round() as u32;
        let sr = |r: Rect| Rect::new(si(r.x), si(r.y), su(r.w), su(r.h));
        let items = self
            .items
            .iter()
            .map(|item| match item {
                DisplayItem::Rect { rect, color } => DisplayItem::Rect {
                    rect: sr(*rect),
                    color: *color,
                },
                DisplayItem::RoundRect {
                    rect,
                    color,
                    radius,
                } => DisplayItem::RoundRect {
                    rect: sr(*rect),
                    color: *color,
                    radius: su(*radius as u32) as u16,
                },
                DisplayItem::Gradient {
                    rect,
                    start,
                    end,
                    vertical,
                    radius,
                } => DisplayItem::Gradient {
                    rect: sr(*rect),
                    start: *start,
                    end: *end,
                    vertical: *vertical,
                    radius: su(*radius as u32) as u16,
                },
                DisplayItem::Shadow { rect, blur, color } => DisplayItem::Shadow {
                    rect: sr(*rect),
                    blur: su(*blur as u32) as u16,
                    color: *color,
                },
                DisplayItem::ClipPush { rect } => DisplayItem::ClipPush { rect: sr(*rect) },
                DisplayItem::ClipPop => DisplayItem::ClipPop,
                DisplayItem::Image {
                    rect,
                    image,
                    fit,
                    pos,
                    pos_px,
                } => DisplayItem::Image {
                    rect: sr(*rect),
                    image: image.clone(),
                    fit: *fit,
                    pos: *pos,
                    pos_px: Point::new(si(pos_px.x), si(pos_px.y)),
                },
                DisplayItem::Line { a, b, width, color } => DisplayItem::Line {
                    a: Point::new(si(a.x), si(a.y)),
                    b: Point::new(si(b.x), si(b.y)),
                    width: su(*width),
                    color: *color,
                },
                DisplayItem::Glyphs {
                    origin,
                    glyphs,
                    color,
                    style,
                } => DisplayItem::Glyphs {
                    origin: Point::new(si(origin.x), si(origin.y)),
                    glyphs: glyphs
                        .iter()
                        .map(|g| GlyphBox {
                            advance: su(g.advance),
                            w: su(g.w),
                            h: su(g.h),
                            id: g.id,
                            px: su(g.px).max(1),
                            font: g.font,
                        })
                        .collect(),
                    color: *color,
                    style: *style,
                },
            })
            .collect();
        DisplayList { items }
    }
}

/// Which bundled face a glyph's `id` indexes into. A run can mix faces when the
/// primary text font lacks a character (e.g. Latin from Roboto, CJK from the
/// fallback), so the face is tracked per glyph rather than per run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FontSlot {
    /// The primary text font (Roboto) — the browser's own UI/chrome face, and the
    /// page face when a page names Roboto specifically.
    #[default]
    Text,
    /// The bundled icon font (private-use icon glyphs).
    Icon,
    /// The bundled fallback face for characters the text font can't render
    /// (CJK, etc.).
    Fallback,
    /// The bundled serif face (Liberation Serif ≈ Times — what the reference
    /// Chrome's generic `serif` resolves to via fontconfig).
    Serif,
    /// The bundled generic-monospace face (DejaVu Sans Mono — the reference's
    /// measured `monospace` resolution).
    Monospace,
    /// The bundled Arial-metric sans face (Liberation Sans ≈ Arial) — the
    /// reference's generic `sans-serif` AND its named-Arial substitution.
    Sans,
    /// The bundled Courier-metric mono face (Liberation Mono) — a page naming
    /// Courier New specifically (fontconfig metric alias).
    CourierMono,
    /// The bundled system-UI sans (DejaVu Sans) — the `system-ui` resolution.
    SansSystem,
}

/// A shaped glyph: enough for both the placeholder box rasterizer (uses `w`/`h`)
/// and a real outline rasterizer (uses `id` + `px` to fetch the outline from the
/// glyph's font). `id` is `0` for the placeholder shaper.
#[derive(Clone, Copy, Debug)]
pub struct GlyphBox {
    /// Horizontal advance after this glyph.
    pub advance: u32,
    /// Inked width (placeholder rasterizer).
    pub w: u32,
    /// Inked height (placeholder rasterizer).
    pub h: u32,
    /// Glyph id within the face named by `font` (`0` for the placeholder).
    pub id: u16,
    /// Pixel size this glyph was shaped at (so the rasterizer can scale it).
    pub px: u32,
    /// Which bundled face `id` belongs to (per-glyph, for mixed-script runs).
    pub font: FontSlot,
}

/// An RGBA8 framebuffer (row-major, top-left origin).
#[derive(Clone, Debug)]
pub struct Framebuffer {
    pub size: Size,
    pub rgba: Vec<u8>,
    /// Current clip rect; writes outside it are dropped (`overflow` — ADR-0043).
    clip: Option<Rect>,
}

impl Framebuffer {
    /// Allocate a transparent framebuffer of the given size.
    pub fn new(size: Size) -> Self {
        let len = (size.area() * 4) as usize;
        Self {
            size,
            rgba: vec![0; len],
            clip: None,
        }
    }

    /// Set the active clip rect (`None` = unclipped). Writes outside it are
    /// dropped — used by the rasterizer's clip stack (ADR-0043).
    pub fn set_clip(&mut self, clip: Option<Rect>) {
        self.clip = clip;
    }

    /// Whether `(x, y)` is inside the active clip (always true when unclipped).
    #[inline]
    fn in_clip(&self, x: i32, y: i32) -> bool {
        match self.clip {
            None => true,
            Some(c) => x >= c.x && y >= c.y && x < c.x + c.w as i32 && y < c.y + c.h as i32,
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
        // A fully transparent fill paints nothing. This matters for real pages:
        // e.g. `background-image: linear-gradient(transparent, transparent), url(x)`
        // is a common layering hack where the gradient layer must be invisible.
        // Hard-writing its RGB (black) with alpha 0 would show as a black box once
        // the framebuffer is flattened to an opaque screenshot.
        if c.a == 0 {
            return;
        }
        let x0 = rect.x.max(0) as u32;
        let y0 = rect.y.max(0) as u32;
        let x1 = ((rect.x + rect.w as i32).max(0) as u32).min(self.size.w);
        let y1 = ((rect.y + rect.h as i32).max(0) as u32).min(self.size.h);
        // Fast path: the whole rect is inside the clip (or there is none).
        let clipped = self.clip.is_some_and(|cl| {
            !(x0 as i32 >= cl.x
                && y0 as i32 >= cl.y
                && x1 as i32 <= cl.x + cl.w as i32
                && y1 as i32 <= cl.y + cl.h as i32)
        });
        // Opaque fills overwrite; translucent fills alpha-blend over the backdrop
        // (source-over) so a semi-transparent overlay tints rather than replaces.
        let opaque = c.a == 255;
        let a = c.a as f32 / 255.0;
        for y in y0..y1 {
            for x in x0..x1 {
                if clipped && !self.in_clip(x as i32, y as i32) {
                    continue;
                }
                let idx = ((y * self.size.w + x) * 4) as usize;
                if opaque {
                    self.rgba[idx] = c.r;
                    self.rgba[idx + 1] = c.g;
                    self.rgba[idx + 2] = c.b;
                    self.rgba[idx + 3] = 255;
                } else {
                    for (i, ch) in [c.r, c.g, c.b].into_iter().enumerate() {
                        let bg = self.rgba[idx + i] as f32;
                        self.rgba[idx + i] = (bg * (1.0 - a) + ch as f32 * a).round() as u8;
                    }
                    self.rgba[idx + 3] = 255;
                }
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

    /// Alpha-blend `color` over the pixel at `(x, y)` with coverage `alpha`
    /// (0.0..=1.0). Used by the glyph rasterizer for anti-aliased text.
    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: f32) {
        if x < 0 || y < 0 || x as u32 >= self.size.w || y as u32 >= self.size.h {
            return;
        }
        if !self.in_clip(x, y) {
            return;
        }
        let a = alpha.clamp(0.0, 1.0);
        let idx = ((y as u32 * self.size.w + x as u32) * 4) as usize;
        for (i, channel) in [color.r, color.g, color.b].into_iter().enumerate() {
            let bg = self.rgba[idx + i] as f32;
            self.rgba[idx + i] = (bg * (1.0 - a) + channel as f32 * a).round() as u8;
        }
        self.rgba[idx + 3] = 255;
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

    /// Shape `text` at `px` in the given generic family (serif/monospace/…). The
    /// default ignores the family and shapes in the primary face, so shapers that
    /// bundle only one face stay correct; a multi-face shaper overrides this to
    /// pick the matching bundled face. Content layout calls this; UI/chrome text
    /// uses the family-less [`shape`](Self::shape).
    fn shape_with(&self, text: &str, px: u32, _family: GenericFamily) -> Vec<GlyphBox> {
        self.shape(text, px)
    }

    /// Shape `text` at `px` in `family` with the run's bold/italic style: a
    /// shaper bundling real weight/slant variants picks the styled face — whose
    /// advances AND glyph ids differ from the regular's (Times bold is wider) —
    /// so styled runs measure (and wrap) as the reference browser does. The
    /// default ignores the style and shapes the regular face, which stays
    /// correct for shapers whose styling is synthesized at raster time (the
    /// smear/shear preserves advances).
    fn shape_styled(
        &self,
        text: &str,
        px: u32,
        family: GenericFamily,
        _style: FontStyle,
    ) -> Vec<GlyphBox> {
        self.shape_with(text, px, family)
    }

    /// Shape a single icon glyph (a codepoint in the bundled icon font), to be
    /// painted in a run styled [`FontStyle::ICON`]. Default: no glyph (a shaper
    /// without an icon font draws nothing).
    fn shape_icon(&self, _ch: char, _px: u32) -> Vec<GlyphBox> {
        Vec::new()
    }

    /// The advance width of a single space at `px`, used for inter-word gaps in
    /// inline layout. This is called once per word on the hottest text path, so
    /// the default's throwaway one-element `Vec` allocation is worth avoiding —
    /// real shapers override this to read the space glyph's advance directly.
    /// Default: shape a single space and sum, so any shaper stays correct.
    fn space_advance(&self, px: u32) -> u32 {
        self.shape(" ", px).iter().map(|g| g.advance).sum()
    }

    /// The space advance in the given generic family — a monospace space is wider
    /// than a proportional one, so word gaps in `<pre>`/`<code>` need the right
    /// face. Default: ignore the family (single-face shapers).
    fn space_advance_with(&self, px: u32, family: GenericFamily) -> u32 {
        self.space_advance_with_f(px, family).round().max(0.0) as u32
    }

    /// [`space_advance_with`](Self::space_advance_with) without the rounding.
    /// Chrome accumulates fractional advances across a line: a Liberation Sans
    /// space at 16px is 4.453px, and rounding it to 4 starves a 20-space line
    /// of ~9px — enough to fit one more word and flip the wrap point, which
    /// cascades into a vertical shift of everything below. The inline flow
    /// carries the sub-pixel remainder across gaps and rounds per placement.
    fn space_advance_with_f(&self, px: u32, _family: GenericFamily) -> f32 {
        self.space_advance(px) as f32
    }

    /// [`space_advance_with_f`](Self::space_advance_with_f) with the run's
    /// bold/italic style — a bold face's space can be wider than the regular's,
    /// so styled runs' word gaps read the same face their glyphs shape from.
    /// Default: ignore the style (single-style shapers).
    fn space_advance_styled_f(&self, px: u32, family: GenericFamily, _style: FontStyle) -> f32 {
        self.space_advance_with_f(px, family)
    }

    /// The `line-height: normal` pitch for `px`-sized text in `family`. Browsers
    /// derive this from the face's own vertical metrics (ascent + descent +
    /// line gap): ~1.15× for the Times/Arial-metric faces, ~1.17× for Roboto —
    /// a flat 1.2 drifts one pixel every couple of lines and accumulates into
    /// visible below-the-fold misalignment on text-heavy pages. Default keeps
    /// the 1.2 approximation for shapers without real metrics.
    fn natural_leading(&self, px: u32, family: GenericFamily) -> i32 {
        self.natural_leading_f(px, family).round() as i32
    }

    /// The face's ascent and descent at `px`, each rounded to whole px exactly
    /// as Blink rounds font metrics (individually, before any use). Layout
    /// needs them to size the line box under a baseline-aligned inline image:
    /// the image's bottom sits ON the baseline, so the box extends
    /// `descent + half-leading` below it. Default approximates the common
    /// ~80/20 ascent/descent split for shapers without real metrics.
    fn ascent_descent(&self, px: u32, _family: GenericFamily) -> (i32, i32) {
        let p = px.max(1) as f32;
        ((p * 0.8).round() as i32, (p * 0.2).round() as i32)
    }

    /// [`natural_leading`](Self::natural_leading) as the f32 the inline flow
    /// accumulates. For `normal`, Blink's value is a whole number of px (it
    /// rounds ascent/descent/gap individually, then sums — see the TextEngine
    /// impl), so real shapers return an integer-valued f32 here; only explicit
    /// fractional `line-height`s (e.g. `1.15`) produce sub-pixel pitch, which
    /// layout accumulates and rounds per line so line N sits at
    /// `round(N × pitch)` exactly as Chrome places it.
    fn natural_leading_f(&self, px: u32, _family: GenericFamily) -> f32 {
        px.max(1) as f32 * 1.2
    }
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
                        id: 0,
                        px: cell,
                        font: FontSlot::Text,
                    }
                } else {
                    GlyphBox {
                        advance: cell / 2,
                        w: cell / 2 - 1,
                        h: cell,
                        id: 0,
                        px: cell,
                        font: FontSlot::Text,
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
                    ..
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
                // The placeholder approximates fills as solid and ignores
                // shadows/clips/lines.
                DisplayItem::RoundRect { rect, color, .. } => target.fill_rect(*rect, *color),
                DisplayItem::Gradient { rect, start, .. } => target.fill_rect(*rect, *start),
                DisplayItem::Shadow { .. }
                | DisplayItem::Line { .. }
                | DisplayItem::ClipPush { .. }
                | DisplayItem::ClipPop => {}
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
    fn fill_rect_transparent_is_a_noop() {
        // A fully transparent fill must not touch the backdrop — a black RGB with
        // alpha 0 (as `linear-gradient(transparent,transparent)` produces) would
        // otherwise stamp a black box over whatever it overlays.
        let mut fb = Framebuffer::new(Size::new(2, 2));
        fb.clear(Color::WHITE);
        fb.fill_rect(Rect::new(0, 0, 2, 2), Color::rgba(0, 0, 0, 0));
        assert_eq!(fb.pixel(0, 0), Some(Color::WHITE));
        assert_eq!(fb.pixel(1, 1), Some(Color::WHITE));
    }

    #[test]
    fn fill_rect_translucent_blends_over_backdrop() {
        // 50% black over white ≈ mid-grey, and the result stays opaque.
        let mut fb = Framebuffer::new(Size::new(1, 1));
        fb.clear(Color::WHITE);
        fb.fill_rect(Rect::new(0, 0, 1, 1), Color::rgba(0, 0, 0, 128));
        let p = fb.pixel(0, 0).unwrap();
        assert!((126..=129).contains(&p.r), "blended grey, got {}", p.r);
        assert_eq!(p.a, 255);
    }

    #[test]
    fn scaled_multiplies_geometry_and_glyph_pixels() {
        let mut list = DisplayList::new();
        list.push(DisplayItem::Rect {
            rect: Rect::new(3, 4, 10, 20),
            color: Color::BLACK,
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(5, 6),
            glyphs: vec![GlyphBox {
                advance: 8,
                w: 0,
                h: 0,
                id: 42,
                px: 16,
                font: FontSlot::Text,
            }],
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        let s = list.scaled(2.0);
        match &s.items[0] {
            DisplayItem::Rect { rect, .. } => {
                assert_eq!((rect.x, rect.y, rect.w, rect.h), (6, 8, 20, 40));
            }
            _ => panic!("expected rect"),
        }
        match &s.items[1] {
            DisplayItem::Glyphs { origin, glyphs, .. } => {
                assert_eq!((origin.x, origin.y), (10, 12));
                assert_eq!(glyphs[0].px, 32);
                assert_eq!(glyphs[0].advance, 16);
                assert_eq!(glyphs[0].id, 42, "glyph id is preserved (re-outlined)");
            }
            _ => panic!("expected glyphs"),
        }
        // Scale 1.0 is an identity copy.
        assert_eq!(list.scaled(1.0).items.len(), 2);
    }

    #[test]
    fn stub_pipeline_paints_text_boxes() {
        let glyphs = MonoShaper.shape("hi", 8);
        let mut list = DisplayList::new();
        list.push(DisplayItem::Glyphs {
            origin: Point::new(0, 0),
            glyphs,
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        let mut fb = Framebuffer::new(Size::new(16, 16));
        fb.clear(Color::WHITE);
        BoxRasterizer.rasterize(&list, &mut fb);
        assert_eq!(fb.pixel(0, 0), Some(Color::BLACK));
    }
}

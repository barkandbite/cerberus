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
use cerberus_types::{Color, FontStyle, ImageFit, ImagePos, Point, Rect};

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

    /// Read the space glyph's advance directly — identical to the first (only)
    /// element of `shape(" ", px)` but with no `Vec` allocation, since inline
    /// layout calls this once per word.
    fn space_advance(&self, px: u32) -> u32 {
        let scale = PxScale::from(px.max(1) as f32);
        let scaled = self.font.as_scaled(scale);
        scaled.h_advance(self.font.glyph_id(' ')).round().max(0.0) as u32
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
            TextEngine::new().draw_image(Rect::new(0, 0, 20, 20), &image, fit, pos, Point::ZERO, &mut fb);
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
}

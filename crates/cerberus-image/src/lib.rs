//! Image decoding adapter (ADR-0005): `ImageDecoder` over the `image` crate for
//! the common web formats (PNG, JPEG, GIF, WebP, BMP), plus **SVG** rasterized
//! via `resvg`/`usvg`/`tiny-skia` (ADR-0009).
//!
//! No `image` (or `resvg`) type crosses the boundary — `decode` returns our
//! `cerberus_paint::DecodedImage`. Large images are downscaled to a memory cap so
//! a single image can't blow the RSS budget (memory is priority #1) — and to
//! make that true for the *transient* decode too (not just the retained
//! bitmap), images whose header-declared resolution exceeds a pixel ceiling are
//! declined before any full-resolution buffer is allocated. SVG is a vector
//! format, so it is rasterized at its intrinsic size, capped to the same
//! longest-side budget.

use cerberus_paint::{DecodedImage, ImageDecoder, PaintError};
use cerberus_types::Size;
use std::io::Cursor;

/// Default ceiling on an image's *intrinsic* pixel count before we will fully
/// decode it. `max_dim` caps the *retained* bitmap, but the `image` crate
/// decodes at full resolution first and downscales after — so a 60-megapixel
/// JPEG would still spike RSS by ~240 MB transiently, breaking the very promise
/// in this module's header. We instead read the header-declared dimensions
/// (cheap, no pixel allocation) and decline anything past this ceiling, which
/// bounds the transient decode buffer to ~`6e6 * 4` ≈ 24 MB. ~6 MP comfortably
/// covers retina hero images (e.g. 2880×1620 ≈ 4.7 MP) while refusing the
/// pathological ones that would blow the budget.
pub const DEFAULT_MAX_DECODE_PIXELS: u64 = 6_000_000;

/// Decoder over the `image` crate.
pub struct ImageCodec {
    /// Longest-side cap in pixels; larger images are downscaled.
    max_dim: u32,
    /// Intrinsic pixel-count ceiling for full decode (see
    /// [`DEFAULT_MAX_DECODE_PIXELS`]); larger images are declined, not decoded.
    max_decode_pixels: u64,
}

impl ImageCodec {
    /// A decoder capping decoded images at 1600px on the longest side and
    /// declining intrinsic images above [`DEFAULT_MAX_DECODE_PIXELS`].
    pub fn new() -> Self {
        Self {
            max_dim: 1600,
            max_decode_pixels: DEFAULT_MAX_DECODE_PIXELS,
        }
    }

    /// A decoder with an explicit longest-side pixel cap (default decode ceiling).
    pub fn with_max_dim(max_dim: u32) -> Self {
        Self {
            max_dim: max_dim.max(1),
            max_decode_pixels: DEFAULT_MAX_DECODE_PIXELS,
        }
    }

    /// A decoder with an explicit longest-side cap *and* intrinsic-pixel decode
    /// ceiling — the latter bounds the transient decode buffer (memory priority).
    pub fn with_limits(max_dim: u32, max_decode_pixels: u64) -> Self {
        Self {
            max_dim: max_dim.max(1),
            max_decode_pixels: max_decode_pixels.max(1),
        }
    }

    /// Rasterize an SVG document into straight-alpha RGBA at its intrinsic size,
    /// scaled down so the longest side fits `max_dim`. `tiny-skia` paints
    /// premultiplied alpha; we demultiply so the result matches the unassociated
    /// alpha the rest of the paint path expects (`cerberus-text` treats RGB as
    /// straight colour and the A byte as coverage).
    fn decode_svg(&self, bytes: &[u8]) -> Result<DecodedImage, PaintError> {
        use resvg::{tiny_skia, usvg};

        let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
            .map_err(|e| PaintError::Decode(format!("svg: {e}")))?;
        let (iw, ih) = (tree.size().width(), tree.size().height());
        if !(iw.is_finite() && ih.is_finite()) || iw <= 0.0 || ih <= 0.0 {
            return Err(PaintError::Decode("svg has no positive size".into()));
        }

        let longest = iw.max(ih);
        let scale = if longest > self.max_dim as f32 {
            self.max_dim as f32 / longest
        } else {
            1.0
        };
        let tw = ((iw * scale).round() as u32).max(1);
        let th = ((ih * scale).round() as u32).max(1);

        let mut pixmap = tiny_skia::Pixmap::new(tw, th)
            .ok_or_else(|| PaintError::Decode("svg pixmap allocation failed".into()))?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        let mut rgba = Vec::with_capacity((tw as usize) * (th as usize) * 4);
        for px in pixmap.pixels() {
            let c = px.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        Ok(DecodedImage {
            size: Size::new(tw, th),
            rgba,
        })
    }
}

/// Sniff whether `bytes` is an SVG document. Raster formats lead with binary
/// magic (PNG `\x89PNG`, JPEG `\xFF\xD8`, GIF `GIF8`, WebP `RIFF…WEBP`, BMP `BM`)
/// that never contains the literal `<svg`, so finding that tag near the start —
/// past any BOM, `<?xml …?>` prolog, comments, or DOCTYPE — is a safe signal.
fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    text.to_ascii_lowercase().contains("<svg")
}

impl Default for ImageCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageDecoder for ImageCodec {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, PaintError> {
        if looks_like_svg(bytes) {
            return self.decode_svg(bytes);
        }

        // Bound the transient decode buffer: read the header-declared dimensions
        // (no pixel allocation) and decline images whose intrinsic size exceeds
        // the decode ceiling, rather than letting `load_from_memory` allocate a
        // full-resolution bitmap we would only shrink afterwards.
        let dims = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| PaintError::Decode(e.to_string()))?
            .into_dimensions()
            .map_err(|e| PaintError::Decode(e.to_string()))?;
        if (dims.0 as u64) * (dims.1 as u64) > self.max_decode_pixels {
            return Err(PaintError::Decode(format!(
                "intrinsic {}x{} exceeds {}px decode budget",
                dims.0, dims.1, self.max_decode_pixels
            )));
        }

        let img = image::load_from_memory(bytes).map_err(|e| PaintError::Decode(e.to_string()))?;

        let img = if img.width() > self.max_dim || img.height() > self.max_dim {
            img.resize(
                self.max_dim,
                self.max_dim,
                image::imageops::FilterType::Triangle,
            )
        } else {
            img
        };

        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Ok(DecodedImage {
            size: Size::new(w, h),
            rgba: rgba.into_raw(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 256) as u8, 10, 20, 255]);
        }
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decodes_png_to_rgba() {
        let bytes = png_bytes(8, 4);
        let decoded = ImageCodec::new().decode(&bytes).unwrap();
        assert_eq!(decoded.size, Size::new(8, 4));
        assert_eq!(decoded.rgba.len(), 8 * 4 * 4);
    }

    #[test]
    fn downscales_huge_images() {
        let bytes = png_bytes(40, 10);
        let decoded = ImageCodec::with_max_dim(20).decode(&bytes).unwrap();
        assert!(decoded.size.w <= 20 && decoded.size.h <= 20);
    }

    #[test]
    fn rejects_garbage() {
        assert!(ImageCodec::new().decode(b"not an image").is_err());
    }

    #[test]
    fn declines_images_over_the_decode_pixel_ceiling() {
        // 40x10 = 400 px. A 200-px ceiling must decline it *without* decoding,
        // while a generous ceiling still decodes the same bytes fine.
        let bytes = png_bytes(40, 10);
        assert!(ImageCodec::with_limits(1600, 200).decode(&bytes).is_err());
        assert!(ImageCodec::with_limits(1600, 10_000).decode(&bytes).is_ok());
    }

    const RED_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;

    #[test]
    fn decodes_svg_to_rgba() {
        let decoded = ImageCodec::new().decode(RED_SVG).unwrap();
        assert_eq!(decoded.size, Size::new(10, 10));
        let idx = ((5 * 10) + 5) * 4;
        assert_eq!(&decoded.rgba[idx..idx + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn svg_sniffed_past_xml_prolog() {
        let svg = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"2\" height=\"2\"><rect width=\"2\" height=\"2\" fill=\"#0000ff\"/></svg>";
        let decoded = ImageCodec::new().decode(svg).unwrap();
        assert_eq!(decoded.size, Size::new(2, 2));
        assert_eq!(&decoded.rgba[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn svg_alpha_is_demultiplied_to_straight_rgba() {
        // A 50%-opacity red fill: if we returned tiny-skia's *premultiplied*
        // buffer, the red channel would be ~128. Demultiplied (straight) it must
        // stay 255 with the alpha carrying the ~50% coverage.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#ff0000" fill-opacity="0.5"/></svg>"##;
        let decoded = ImageCodec::new().decode(svg).unwrap();
        let idx = ((2 * 4) + 2) * 4;
        let px = &decoded.rgba[idx..idx + 4];
        assert_eq!(
            px[0], 255,
            "red must be straight, not premultiplied: {px:?}"
        );
        assert_eq!((px[1], px[2]), (0, 0));
        assert!((px[3] as i32 - 128).abs() <= 2, "alpha ~128, got {}", px[3]);
    }

    #[test]
    fn caps_large_svg_to_max_dim() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="400"><rect width="800" height="400" fill="#00ff00"/></svg>"##;
        let decoded = ImageCodec::with_max_dim(100).decode(svg).unwrap();
        assert_eq!(
            decoded.size,
            Size::new(100, 50),
            "longest side capped, aspect kept"
        );
    }
}

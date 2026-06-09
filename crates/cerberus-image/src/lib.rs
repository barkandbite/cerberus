//! Image decoding adapter (ADR-0005): `ImageDecoder` over the `image` crate for
//! the common web formats (PNG, JPEG, GIF, WebP, BMP).
//!
//! No `image` type crosses the boundary — `decode` returns our
//! `cerberus_paint::DecodedImage`. Large images are downscaled to a memory cap so
//! a single image can't blow the RSS budget (memory is priority #1).

use cerberus_paint::{DecodedImage, ImageDecoder, PaintError};
use cerberus_types::Size;

/// Decoder over the `image` crate.
pub struct ImageCodec {
    /// Longest-side cap in pixels; larger images are downscaled.
    max_dim: u32,
}

impl ImageCodec {
    /// A decoder capping decoded images at 1600px on the longest side.
    pub fn new() -> Self {
        Self { max_dim: 1600 }
    }

    /// A decoder with an explicit longest-side pixel cap.
    pub fn with_max_dim(max_dim: u32) -> Self {
        Self {
            max_dim: max_dim.max(1),
        }
    }
}

impl Default for ImageCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageDecoder for ImageCodec {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, PaintError> {
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
}

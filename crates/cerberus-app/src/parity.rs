//! Parity measurement: a perceptual pixel-diff between a reference render
//! (headless Chrome) and Cerberus's own output, so "closer to Chrome" is a
//! number instead of a vibe (RENDERING_PARITY_PLAN.md, Workstream 0).
//!
//! The core [`diff`] is a pure function over two RGBA buffers — no I/O, fully
//! unit-tested. [`load_png`] and [`crop_top`] are thin wrappers the `diff`
//! subcommand uses to feed it real screenshots. Cerberus draws a 36px toolbar
//! that Chrome's page screenshot does not, so the caller crops that band off the
//! Cerberus image before diffing (`--crop-top 36`); the two are then aligned at
//! the top-left and compared over their overlapping region.

use cerberus_types::Size;

/// A decoded, straight-alpha RGBA image (row-major, 4 bytes/pixel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub size: Size,
    pub pixels: Vec<u8>,
}

impl Rgba {
    /// Build from raw parts, validating the buffer length matches `w*h*4`.
    pub fn new(w: u32, h: u32, pixels: Vec<u8>) -> Result<Self, String> {
        let need = w as usize * h as usize * 4;
        if pixels.len() != need {
            return Err(format!(
                "rgba buffer is {} bytes, expected {need} for {w}x{h}",
                pixels.len()
            ));
        }
        Ok(Self {
            size: Size::new(w, h),
            pixels,
        })
    }
}

/// The outcome of comparing two images over their overlapping region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffReport {
    /// Overlap width/height actually compared (the min of the two sizes).
    pub compared: Size,
    /// Total pixels compared (`compared.w * compared.h`).
    pub compared_px: u64,
    /// Pixels whose largest per-channel RGB delta exceeds `tolerance`.
    pub mismatched_px: u64,
    /// Root-mean-square error over RGB channels, normalized to `0.0..=1.0`
    /// (0 = identical, 1 = every channel maximally off). This is the headline
    /// "distance to Chrome" number: lower is closer.
    pub rmse: f64,
    /// Fraction of compared pixels that mismatched (`0.0..=1.0`).
    pub mismatch_fraction: f64,
}

impl DiffReport {
    /// Whether the two images are byte-identical over the overlap *and* the same
    /// size (a perfect match, the ideal parity outcome).
    pub fn is_perfect(&self, a: &Rgba, b: &Rgba) -> bool {
        a.size == b.size && self.mismatched_px == 0 && self.rmse == 0.0
    }
}

/// Compare `a` (reference) and `b` at the top-left over their overlapping
/// region. `tolerance` is the per-channel delta (0..=255) below which a pixel
/// counts as matching — a small value (e.g. 8) absorbs anti-aliasing and font
/// hinting jitter so only real differences register. RGB only; alpha is ignored
/// (page screenshots are opaque).
pub fn diff(a: &Rgba, b: &Rgba, tolerance: u8) -> DiffReport {
    let cw = a.size.w.min(b.size.w);
    let ch = a.size.h.min(b.size.h);
    let compared = Size::new(cw, ch);
    let compared_px = cw as u64 * ch as u64;
    if compared_px == 0 {
        return DiffReport {
            compared,
            compared_px: 0,
            mismatched_px: 0,
            rmse: 0.0,
            mismatch_fraction: 0.0,
        };
    }
    let (aw, bw) = (a.size.w as usize, b.size.w as usize);
    let mut sq_sum: u64 = 0; // sum of squared channel deltas
    let mut mismatched: u64 = 0;
    for y in 0..ch as usize {
        let arow = y * aw * 4;
        let brow = y * bw * 4;
        for x in 0..cw as usize {
            let ai = arow + x * 4;
            let bi = brow + x * 4;
            let mut worst = 0u8;
            for c in 0..3 {
                let d = a.pixels[ai + c].abs_diff(b.pixels[bi + c]);
                worst = worst.max(d);
                sq_sum += d as u64 * d as u64;
            }
            if worst > tolerance {
                mismatched += 1;
            }
        }
    }
    // RMSE over the RGB channels of every compared pixel, normalized by 255.
    let mean_sq = sq_sum as f64 / (compared_px as f64 * 3.0);
    let rmse = mean_sq.sqrt() / 255.0;
    DiffReport {
        compared,
        compared_px,
        mismatched_px: mismatched,
        rmse,
        mismatch_fraction: mismatched as f64 / compared_px as f64,
    }
}

/// Drop the top `rows` pixel rows (e.g. Cerberus's toolbar band) so the page
/// content aligns with a Chrome page screenshot. Rows beyond the image height
/// yield an empty image.
pub fn crop_top(img: &Rgba, rows: u32) -> Rgba {
    let rows = rows.min(img.size.h);
    let new_h = img.size.h - rows;
    let stride = img.size.w as usize * 4;
    let start = rows as usize * stride;
    Rgba {
        size: Size::new(img.size.w, new_h),
        pixels: img.pixels[start..].to_vec(),
    }
}

/// Decode a PNG file into straight-alpha RGBA through the sanctioned
/// `cerberus-image` adapter (no direct `image`-crate dependency in this crate's
/// production path). The decode caps are raised to effectively unlimited so a
/// reference or Cerberus screenshot is compared at its native resolution rather
/// than the adapter's default downscale.
pub fn load_png(path: &str) -> Result<Rgba, String> {
    use cerberus_image::ImageCodec;
    use cerberus_paint::ImageDecoder;
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let decoded = ImageCodec::with_limits(u32::MAX, u64::MAX)
        .decode(&bytes)
        .map_err(|e| format!("decode {path}: {e:?}"))?;
    Rgba::new(decoded.size.w, decoded.size.h, decoded.rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Rgba {
        let mut px = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..(w * h) {
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        Rgba::new(w, h, px).unwrap()
    }

    #[test]
    fn identical_images_score_zero() {
        let a = solid(4, 4, [10, 20, 30]);
        let r = diff(&a, &a, 0);
        assert_eq!(r.mismatched_px, 0);
        assert_eq!(r.rmse, 0.0);
        assert_eq!(r.mismatch_fraction, 0.0);
        assert!(r.is_perfect(&a, &a));
    }

    #[test]
    fn full_channel_difference_scores_one() {
        // Black vs white: every channel off by 255 → RMSE normalizes to 1.0.
        let black = solid(8, 8, [0, 0, 0]);
        let white = solid(8, 8, [255, 255, 255]);
        let r = diff(&black, &white, 0);
        assert_eq!(r.mismatched_px, 64);
        assert_eq!(r.mismatch_fraction, 1.0);
        assert!((r.rmse - 1.0).abs() < 1e-9, "rmse={}", r.rmse);
        assert!(!r.is_perfect(&black, &white));
    }

    #[test]
    fn tolerance_absorbs_small_jitter() {
        let a = solid(4, 4, [100, 100, 100]);
        let b = solid(4, 4, [104, 100, 100]); // +4 on red only
                                              // Under a tolerance of 8 the 4-level delta is not a mismatch...
        assert_eq!(diff(&a, &b, 8).mismatched_px, 0);
        // ...but RMSE still reflects the real (small) error.
        assert!(diff(&a, &b, 8).rmse > 0.0);
        // With zero tolerance every pixel counts as changed.
        assert_eq!(diff(&a, &b, 0).mismatched_px, 16);
    }

    #[test]
    fn mismatched_sizes_compare_the_overlap() {
        let a = solid(10, 10, [0, 0, 0]);
        let b = solid(6, 8, [0, 0, 0]);
        let r = diff(&a, &b, 0);
        assert_eq!(r.compared, Size::new(6, 8));
        assert_eq!(r.compared_px, 48);
        assert_eq!(r.mismatched_px, 0, "overlap is identical black");
        // Same content over the overlap, but the sizes differ, so not "perfect".
        assert!(!r.is_perfect(&a, &b));
    }

    #[test]
    fn crop_top_drops_the_leading_band() {
        // 3 rows: red, green, blue. Crop the first row → green then blue remain.
        let mut px = Vec::new();
        for rgb in [[255, 0, 0], [0, 255, 0], [0, 0, 255]] {
            for _ in 0..2 {
                px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
        let img = Rgba::new(2, 3, px).unwrap();
        let cropped = crop_top(&img, 1);
        assert_eq!(cropped.size, Size::new(2, 2));
        assert_eq!(
            &cropped.pixels[0..4],
            &[0, 255, 0, 255],
            "first row is green"
        );
    }

    #[test]
    fn crop_beyond_height_is_empty() {
        let img = solid(3, 2, [1, 2, 3]);
        let cropped = crop_top(&img, 5);
        assert_eq!(cropped.size, Size::new(3, 0));
        assert!(cropped.pixels.is_empty());
    }
}

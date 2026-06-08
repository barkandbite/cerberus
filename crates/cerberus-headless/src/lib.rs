//! Headless rendering: drive layout + paint into a framebuffer and serialize it.
//!
//! Scoped to rendering and automated tests (M8). It inherits farbling and
//! defaults third-party storage to deny — those policies are applied by the
//! caller (the app composition root) via the consent/farbling seams; this crate
//! is just the pixel pipeline. Output is PPM today; PNG/PDF arrive with the
//! approved image-encoder crate (M2/M8).

use cerberus_dom::Document;
use cerberus_layout::LayoutEngine;
use cerberus_paint::{Framebuffer, Rasterizer, TextShaper};
use cerberus_types::{Color, Size};
use std::io::{self, Write};
use std::path::Path;

/// Render a document to a framebuffer: lay out, clear to `background`, paint.
pub fn render_document(
    doc: &Document,
    viewport: Size,
    background: Color,
    layout: &mut dyn LayoutEngine,
    shaper: &dyn TextShaper,
    rasterizer: &dyn Rasterizer,
) -> Framebuffer {
    let list = layout.layout(doc, viewport, shaper);
    let mut fb = Framebuffer::new(viewport);
    fb.clear(background);
    rasterizer.rasterize(&list, &mut fb);
    fb
}

/// Write a framebuffer as a binary PPM (P6) file. PPM keeps the scaffold
/// dependency-free; PNG/PDF come with the approved image crate.
pub fn write_ppm(path: impl AsRef<Path>, fb: &Framebuffer) -> io::Result<()> {
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    write_ppm_to(&mut out, fb)?;
    out.flush()
}

/// Serialize a framebuffer as PPM to any writer.
pub fn write_ppm_to(out: &mut impl Write, fb: &Framebuffer) -> io::Result<()> {
    write!(out, "P6\n{} {}\n255\n", fb.size.w, fb.size.h)?;
    for px in fb.rgba.chunks_exact(4) {
        out.write_all(&px[..3])?; // drop alpha
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_trivial;
    use cerberus_layout::BlockLayout;
    use cerberus_paint::{BoxRasterizer, MonoShaper};

    #[test]
    fn renders_a_nonempty_frame_with_background() {
        let doc = parse_trivial("<h1>Cerberus</h1>");
        let viewport = Size::new(200, 80);
        let fb = render_document(
            &doc,
            viewport,
            Color::WHITE,
            &mut BlockLayout::default(),
            &MonoShaper,
            &BoxRasterizer,
        );
        assert_eq!(fb.size, viewport);
        // Background present...
        assert_eq!(fb.pixel(199, 79), Some(Color::WHITE));
        // ...and some text was painted (at least one non-white pixel).
        let painted = fb.rgba.chunks_exact(4).any(|px| px[..3] != [255, 255, 255]);
        assert!(painted, "expected painted glyph boxes");
    }

    #[test]
    fn ppm_has_valid_header() {
        let fb = Framebuffer::new(Size::new(2, 3));
        let mut buf = Vec::new();
        write_ppm_to(&mut buf, &fb).unwrap();
        assert!(buf.starts_with(b"P6\n2 3\n255\n"));
        assert_eq!(buf.len(), "P6\n2 3\n255\n".len() + 2 * 3 * 3);
    }
}

//! PNG and single-page PDF serialization for framebuffers — ours, no deps.
//!
//! Both formats ride on the same bootstrapped zlib *stored-block* stream
//! (RFC 1950/1951: no compression, just framing + Adler-32). Stored blocks
//! keep the encoder ~40 lines and dependency-free; the cost is file size,
//! which is acceptable for automation artifacts. The PNG path is also a
//! correctness proof: `cerberus-image` (the decode adapter) round-trips it
//! in tests.

use cerberus_paint::Framebuffer;
use std::io::{self, Write};
use std::path::Path;

/// zlib-wrap `raw` using stored (uncompressed) deflate blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let blocks = raw.chunks(65535);
    let n_blocks = blocks.len().max(1);
    let mut out = Vec::with_capacity(2 + raw.len() + 5 * n_blocks + 4);
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG: 32K window, no preset dict
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]); // final empty block
    }
    let last = n_blocks - 1;
    for (i, block) in raw.chunks(65535).enumerate() {
        let len = block.len() as u16;
        out.push(u8::from(i == last)); // BFINAL, BTYPE=00 (stored)
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    // Adler-32 of the raw stream.
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&(((b << 16) | a).to_be_bytes()));
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Serialize a framebuffer as a PNG (RGBA8, filter 0, stored-block zlib).
pub fn png_bytes(fb: &Framebuffer) -> Vec<u8> {
    let (w, h) = (fb.size.w, fb.size.h);
    // Raw scanlines: one filter byte (0 = None) + RGBA per row.
    let stride = w as usize * 4 + 1;
    let mut raw = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        raw[y * stride] = 0;
        let row = &fb.rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        raw[y * stride + 1..(y + 1) * stride].copy_from_slice(row);
    }

    let mut out = Vec::with_capacity(raw.len() + 1024);
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, no interlace
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// Write a framebuffer as a PNG file.
pub fn write_png(path: impl AsRef<Path>, fb: &Framebuffer) -> io::Result<()> {
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(&png_bytes(fb))?;
    out.flush()
}

/// Serialize a framebuffer as a single-page PDF: the frame as a full-page
/// RGB image XObject (alpha dropped — pages are composited opaque), one
/// pixel = one PDF point.
pub fn pdf_bytes(fb: &Framebuffer) -> Vec<u8> {
    let (w, h) = (fb.size.w, fb.size.h);
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for px in fb.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    let image = zlib_stored(&rgb);

    // Objects: 1 catalog, 2 pages, 3 page, 4 content stream, 5 image.
    let content = format!("q\n{w} 0 0 {h} 0 0 cm\n/Im0 Do\nQ\n");
    let mut objects: Vec<Vec<u8>> = Vec::new();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    objects.push(
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
        )
        .into_bytes(),
    );
    objects.push(
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        )
        .into_bytes(),
    );
    let mut image_obj = format!(
        "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} \
         /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode \
         /Length {} >>\nstream\n",
        image.len()
    )
    .into_bytes();
    image_obj.extend_from_slice(&image);
    image_obj.extend_from_slice(b"\nendstream");
    objects.push(image_obj);

    let mut out = b"%PDF-1.4\n%\xC2\xB5\xC2\xB6\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Write a framebuffer as a single-page PDF file.
pub fn write_pdf(path: impl AsRef<Path>, fb: &Framebuffer) -> io::Result<()> {
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(&pdf_bytes(fb))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_types::{Color, Size};

    fn checker(w: u32, h: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(Size::new(w, h));
        for y in 0..h {
            for x in 0..w {
                let c = if (x + y) % 2 == 0 {
                    Color::rgb(255, 0, 0)
                } else {
                    Color::rgb(0, 0, 255)
                };
                fb.fill_rect(cerberus_types::Rect::new(x as i32, y as i32, 1, 1), c);
            }
        }
        fb
    }

    #[test]
    fn png_has_signature_ihdr_and_correct_dims() {
        let bytes = png_bytes(&checker(5, 3));
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(&bytes[16..20], 5u32.to_be_bytes());
        assert_eq!(&bytes[20..24], 3u32.to_be_bytes());
        assert!(bytes.windows(4).any(|w| w == b"IEND"));
    }

    #[test]
    fn zlib_stored_stream_self_describes_its_length() {
        let raw = vec![0xABu8; 70000]; // forces two stored blocks
        let z = zlib_stored(&raw);
        // Header + first block (non-final, 65535) + second (final, 4465) + adler.
        assert_eq!(z[0], 0x78);
        assert_eq!(z[2], 0); // BFINAL=0 on the first block
        let len1 = u16::from_le_bytes([z[3], z[4]]);
        assert_eq!(len1, 65535);
        assert_eq!(z.len(), 2 + 5 + 65535 + 5 + 4465 + 4);
    }

    #[test]
    fn png_round_trips_through_the_real_decoder() {
        use cerberus_paint::ImageDecoder as _;
        let fb = checker(7, 5);
        let decoded = cerberus_image::ImageCodec::new()
            .decode(&png_bytes(&fb))
            .expect("our PNG must decode");
        assert_eq!((decoded.size.w, decoded.size.h), (7, 5));
        // Pixel-exact: stored blocks are lossless and filter 0 is identity.
        assert_eq!(decoded.rgba, fb.rgba);
    }

    #[test]
    fn pdf_structure_is_parseable() {
        let bytes = pdf_bytes(&checker(4, 4));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("%PDF-1.4"));
        assert!(text.contains("/Type /Catalog"));
        assert!(text.contains("/MediaBox [0 0 4 4]"));
        assert!(text.contains("/Filter /FlateDecode"));
        assert!(text.ends_with("%%EOF\n"));

        // startxref points at the actual xref table.
        let sx = text.rfind("startxref\n").unwrap();
        let offset: usize = text[sx + 10..]
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(&bytes[offset..offset + 4], b"xref");

        // Each xref entry points at "N 0 obj" (entry i describes object i;
        // entry 0 is the free head).
        let xref_text = String::from_utf8_lossy(&bytes[offset..]).into_owned();
        for (i, line) in xref_text
            .lines()
            .skip(2) // "xref" + "0 N"
            .take(6)
            .enumerate()
        {
            if i == 0 {
                continue; // the free object 0
            }
            let obj_off: usize = line.split_whitespace().next().unwrap().parse().unwrap();
            let expect = format!("{i} 0 obj");
            assert_eq!(
                String::from_utf8_lossy(&bytes[obj_off..obj_off + expect.len()]),
                expect,
                "xref entry {i} does not point at its object"
            );
        }
    }
}

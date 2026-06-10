# ADR-0005: Rendering stack — text shaping, font rasterization, image decoding

- Status: Accepted
- Date: 2026-06-09
- Deciders: bbarker@barkbite.org (approved "full stack"), engineering

## Context

"Properly" rendering real pages means turning text into shaped, rasterized
glyphs and decoding images — all historically large CVE surfaces. Per ADR-0003
these are exactly the places we lean on audited crates, each behind one of our
paint traits (`TextShaper`, `Rasterizer`, `ImageDecoder` in `cerberus-paint`).

## Decision

Approve the full visual stack, each wrapped in an adapter crate behind its trait:

- **Text shaping:** `rustybuzz` (pure-Rust HarfBuzz port) + `ttf-parser` for font
  parsing → `TextShaper`.
- **Glyph rasterization:** `swash` (scaling, hinting, rasterization, incl. color/
  emoji) → the glyph path of `Rasterizer`. `ab_glyph`/`fontdue` are the lighter
  fallbacks if swash proves heavy.
- **Image decoding:** the `image` crate facade for breadth now; revisit the
  leaner `zune-*` decoders if footprint/RSS demands → `ImageDecoder`.

### Bundle fonts; do NOT enumerate system fonts

We ship a fixed, **bundled** libre font set and do not read or enumerate the
user's installed fonts. Two reasons:

1. **Anti-fingerprinting.** Installed-font lists and metrics are a major
   fingerprinting vector; enumerating system fonts would directly undercut the
   farbling/anti-tracking goal. Font-metric farbling (M6) perturbs measurements
   *on top of* the fixed bundle.
2. **Reproducible rendering** across machines (and for headless PNG output).

## Consequences

- **Easier:** legible real text and images; complex-script shaping; deterministic
  output.
- **Costs:** a meaningfully larger dependency tree and higher RSS — **recalibrate
  the memory budget** when these land (M2). Each adapter confines its `unsafe`
  and is individually swappable (e.g. swash → ab_glyph) with no caller changes.

## Alternatives considered

- **`cosmic-text`:** bundles shaping + layout + rasterization, but overlaps our
  own `LayoutEngine` and would blur that boundary. Rejected for now.
- **System font discovery (`font-kit`/`fontdb` over installed fonts):** rejected
  for the fingerprinting reason above — a curated bundle is a feature, not a gap.

## Update — 2026-06-09: first adapter wired

Shipped `cerberus-text`: `ab_glyph` + a **bundled Roboto Regular** (Apache-2.0,
in `crates/cerberus-text/assets/`, license preserved alongside). Chosen over
swash as the leaner first rasterizer — only 4 transitive crates (ab_glyph,
ab_glyph_rasterizer, ttf-parser, owned_ttf_parser) — and sufficient for Latin
text. It implements both `TextShaper` and `Rasterizer` over the fixed font.
`rustybuzz` (complex-script shaping) and `image` (decoding) remain to wire
behind the same traits when needed. Verified: anti-aliased output, ~6 MB RSS.

## Update — 2026-06-09: image decoder wired

Shipped `cerberus-image`: the `image` crate (default features off; `png`, `jpeg`,
`gif`, `webp`, `bmp`) behind `ImageDecoder`. No `image` type crosses the seam —
`decode` returns `cerberus_paint::DecodedImage`. A **1600px long-edge cap**
downscales oversized images at decode time so a single asset can't blow the RSS
budget (memory is priority #1). The composition root fetches `<img>`
sub-resources (on the network worker for the interactive browser, synchronously
for the one-shot `render`) into a **per-page** store cleared on every
navigation, and `cerberus-text`'s `Rasterizer` paints them with a
nearest-neighbor alpha blend. Live-verified end-to-end (kernel.org 7/8,
Wikipedia 8–11/N decoded); RSS 15–32 MB on image-heavy pages, within the 64 MB
gate. SVG is vector, not a raster format `image` decodes, so SVG `<img>` are
skipped (a resvg-based vector path is a later, separately-approved adapter).
`rustybuzz` (complex-script shaping) is still the remaining piece.

## Update — 2026-06-10: SVG wired (see ADR-0009)

The deferred vector path landed: `resvg`/`usvg`/`tiny-skia` (text feature off)
behind the same `ImageDecoder`, sniffed in `ImageCodec::decode` and rasterized
under the same 1600px cap. Real-site decode jumped (rust-lang 0/9 → 9/9,
Wikipedia 6/12 → 9/12). Details and the anti-fingerprinting rationale for
disabling SVG text are in ADR-0009.

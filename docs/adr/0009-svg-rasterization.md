# ADR-0009: SVG image rasterization via resvg / usvg / tiny-skia

- Status: Accepted
- Date: 2026-06-10
- Deciders: bbarker@barkbite.org (approved "SVG image support"), engineering

## Context

Real-site hardening showed SVG is no longer a niche format: it is how modern
pages ship logos and icons. On a live sweep the `image`-crate decoder (ADR-0005)
returned **rust-lang.org 0/9**, **Wikipedia 6/12**, **Hacker News 1/2** — the
misses were almost all SVG. `image` is a raster-only facade and cannot decode a
vector document, so those `<img>` silently fell back to nothing.

ADR-0005 explicitly deferred this: *"SVG is vector, not a raster format `image`
decodes, so SVG `<img>` are skipped (a resvg-based vector path is a later,
separately-approved adapter)."* This ADR is that approval.

## Decision

Rasterize SVG behind the **existing `ImageDecoder` trait** in `cerberus-image` —
no new seam, no new adapter crate. `ImageCodec::decode` sniffs the bytes; an SVG
document is rendered to RGBA, anything else takes the unchanged `image` path.

Add three crates (all already license-clean per the ADR-0003 allow-list):

| Dependency | Role | License |
| --- | --- | --- |
| `usvg` | parse + normalize SVG into a render tree | Apache-2.0 OR MIT |
| `resvg` | render the tree into a pixmap | Apache-2.0 OR MIT |
| `tiny-skia` | the CPU raster backend (RGBA pixmap) | BSD-3-Clause |

(plus their small support crates: `roxmltree`, `simplecss`, `svgtypes`, `kurbo`,
`euclid`, … — 18 in total, no new license, no new duplicate version).

### Rasterize at intrinsic size, under the same memory cap

SVG has no inherent pixel size, so we render at the document's intrinsic
`width`/`height` (or `viewBox`), **scaled down so the longest side fits the same
1600px cap** ADR-0005 set for rasters. A vector image therefore cannot blow the
RSS budget any more than a bitmap can (memory is priority #1). Layout then scales
the resulting bitmap into the `<img>` box as for any other image.

`tiny-skia` paints **premultiplied** alpha; we **demultiply** to straight RGBA so
the result matches the unassociated-alpha convention the rest of the paint path
already assumes (`cerberus-text` treats RGB as straight colour and the A byte as
coverage). A regression test pins this — a 50%-opacity fill must keep its straight
channel value, not the premultiplied one.

### Text feature OFF — leanness *and* anti-fingerprinting

`resvg`/`usvg` are pulled with `default-features = false`, which drops the SVG
**text** stack (`fontdb`, `rustybuzz`, `ttf-parser`, system-font enumeration).
Two reasons, both load-bearing:

1. **Leanness / memory.** Disabling text keeps the addition to 18 small crates
   with no font machinery and no font-table allocations.
2. **Anti-fingerprinting.** System-font enumeration is exactly what ADR-0005
   forbids ("do NOT enumerate system fonts"). Letting an SVG renderer reach for
   installed fonts would reopen that vector.

Consequence: `<text>` inside an SVG does not render (shapes, paths, gradients,
clips, masks, patterns all do). The overwhelming majority of logos/icons are
path geometry, so this is the right lean default; if a real need appears we can
revisit using the *bundled* font only.

## Consequences

- **Easier:** real logos/icons render. Live re-sweep after wiring: **rust-lang
  0/9 → 9/9**, **Wikipedia 6/12 → 9/12**, **Hacker News 1/2 → 2/2**; RSS stayed
  within the 64 MB gate (19–41 MB on those pages).
- **Costs:** +18 crates (small, audited, single-version). SVG `<text>` is not
  drawn (deliberate). `.svgz` (gzip-compressed SVG) is not yet sniffed — a later
  add if it shows up in practice.
- The dependency stays confined behind `ImageDecoder`; no `resvg`/`tiny-skia`
  type crosses the boundary (`decode` still returns `cerberus_paint::DecodedImage`).

## Alternatives considered

- **`librsvg`:** the GNOME reference renderer, but C/GObject + cairo — FFI and a
  heavy non-Rust runtime, against ADR-0003's memory-safe posture. Rejected.
- **Bootstrapping an SVG renderer:** SVG is a large spec (path grammar, CSS,
  gradients, masks) and a historical CVE surface — squarely in ADR-0003's
  "delegate behind a trait" category. Rejected.
- **Keep skipping SVG:** rejected — it leaves most modern pages visibly broken.

//! Hinted glyph rasterization via `skrifa` (pure Rust — ADR-0005 compliant).
//!
//! The reference Chrome rasterizes through FreeType with **light hinting**
//! (fontconfig `hintslight`): TrueType bytecode runs with horizontal moves
//! discarded, grid-fitting stem edges to the pixel grid *vertically* so glyph
//! cores are solid ink. Unhinted outlines (the previous `ab_glyph` path) spread
//! that coverage across two rows/columns, which measured as the dominant
//! remaining glyph-AA gap (~35% of ink pixels >32 gray levels off) even with
//! Blink-exact layout, an integer baseline, and sub-pixel pen advances.
//!
//! `skrifa` (Google Fonts' pure-Rust scaler) reimplements FreeType's hinting:
//! [`SmoothMode::Light`] corresponds to `FT_LOAD_TARGET_LIGHT`. Only the
//! outline+fill step changes — glyph ids and advances still come from
//! `rustybuzz`, the baseline stays integer, and pens keep sub-pixel x (light
//! hinting never moves outlines horizontally, so a fractional pen x composes
//! cleanly with a vertically grid-fit outline, exactly as Chrome positions
//! text).
//!
//! The engine is the **auto-hinter** ([`Engine::Auto`]), not the TrueType
//! bytecode interpreter — measured against the reference Chrome on the
//! calibration page (black 13/16/24px Arial+Times on white, pixel-aligned
//! runs, ink pixels with |Δgray|>32): unhinted ab_glyph 34.9%, interpreter
//! light 26.5%, **auto light 20.1%** (mean |Δ| 36.3 → 24.8). The autohinter
//! also reproduces Chrome's stem-edge rounding direction (e.g. the H crossbar
//! snaps its top edge, spilling AA below, where the interpreter spilled
//! above). Auto+Normal measured far worse (43.8%) — Chrome really is in a
//! light, vertical-only mode.
//!
//! Anti-fingerprinting stance (ADR-0005) is unchanged: hinting instructions
//! live in the **bundled** font bytes and the interpreter is deterministic —
//! no system fonts, no system FreeType, no per-host state feeds the raster.

use std::collections::HashMap;
use std::sync::Mutex;

use ab_glyph_rasterizer::{point as rpoint, Rasterizer as CoverageRaster};
use cerberus_paint::Framebuffer;
use cerberus_types::{Color, FontStyle};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{
    DrawSettings, Engine, HintingInstance, HintingOptions, OutlineGlyph, OutlineGlyphCollection,
    OutlinePen, SmoothMode, Target,
};
use skrifa::GlyphId;

/// Largest raster a single glyph may claim (defense against a corrupt bundled
/// outline; matches the framebuffer scale of anything we draw).
const MAX_GLYPH_RASTER: usize = 4096;

/// Sub-pixel x positions cached per pixel. The outline is grid-fit vertically
/// (integer baseline) but positioned at a fractional pen x, so its coverage
/// depends on that fraction. Quantizing to 1/64 px keeps the render visually
/// identical to exact positioning (≤1/128 px shift, far below perception or the
/// AA measurement floor) while making the key discrete — and crucially, a page
/// re-rasterized on scroll hits every glyph at the *same* fraction, so the cache
/// is a full hit frame-to-frame.
const SUBPX: u32 = 64;

/// Upper bound on the rendered-glyph cache before it's dropped wholesale. Glyph
/// bitmaps are tiny (a 16 px glyph is ~200 B), so this holds many thousands of
/// (face, glyph, size, sub-pixel) variants — well past any page's working set —
/// while staying a small fraction of the 64 MB memory budget.
const GLYPH_CACHE_CAP_BYTES: usize = 4 * 1024 * 1024;

/// One rasterized glyph's coverage, positioned relative to its pen. `cov` is a
/// `w`×`h` row-major alpha map (0–255); `rel_x`/`rel_y` place its top-left
/// relative to `(floor(pen_x), baseline)`. Style (bold smear / italic shear) and
/// colour are applied at blit time, so this is the plain, reusable glyph.
struct CachedGlyph {
    w: u16,
    h: u16,
    rel_x: i32,
    rel_y: i32,
    cov: Vec<u8>,
}

/// Rendered-glyph cache with a byte budget. On overflow it clears wholesale
/// (simpler than eviction and cheap — the working set repopulates in one frame).
struct GlyphCache {
    map: HashMap<(usize, u16, u32, u16), CachedGlyph>,
    bytes: usize,
}

impl GlyphCache {
    fn insert(&mut self, key: (usize, u16, u32, u16), val: CachedGlyph) {
        let sz = val.cov.len() + 48;
        if self.bytes + sz > GLYPH_CACHE_CAP_BYTES {
            self.map.clear();
            self.bytes = 0;
        }
        self.bytes += sz;
        self.map.insert(key, val);
    }
}

/// A cache of per-(face, px) skrifa [`HintingInstance`]s. Building one runs the
/// font's `fpgm`/`prep` programs, so it is done once per face+size, not per
/// glyph. `None` records a face+size whose hinting failed to initialize (the
/// caller falls back to the unhinted `ab_glyph` path). Also holds the
/// rendered-glyph cache so a repaint (the common case: scrolling) blits cached
/// coverage instead of re-outlining and re-rasterizing every glyph.
pub(crate) struct HintCache {
    cache: Mutex<HashMap<(usize, u32), Option<HintingInstance>>>,
    glyphs: Mutex<GlyphCache>,
}

impl HintCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            glyphs: Mutex::new(GlyphCache {
                map: HashMap::new(),
                bytes: 0,
            }),
        }
    }

    /// Rasterize glyph `id` of `outlines` at `px` with FreeType-style light
    /// hinting, blending into `target` at (`pen_x`, `baseline`). `pen_x` is
    /// fractional (sub-pixel positioning); `baseline` is the integer-rounded
    /// baseline the caller derived. Returns `false` if this glyph could not be
    /// drawn hinted (caller falls back to the unhinted path); an empty glyph
    /// (space) returns `true` with no ink.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_glyph(
        &self,
        outlines: &OutlineGlyphCollection<'static>,
        face_key: usize,
        id: u16,
        px: u32,
        pen_x: f32,
        baseline: f32,
        color: Color,
        residual: FontStyle,
        target: &mut Framebuffer,
    ) -> bool {
        // Quantize the sub-pixel pen x so the glyph's coverage has a discrete
        // key; a fraction that rounds up to a whole pixel carries into the
        // integer part.
        let mut floor_x = pen_x.floor();
        let mut bucket = ((pen_x - floor_x) * SUBPX as f32).round() as u32;
        if bucket >= SUBPX {
            bucket -= SUBPX;
            floor_x += 1.0;
        }
        let floor_xi = floor_x as i32;
        let base_i = baseline.round() as i32;
        let key = (face_key, id, px, bucket as u16);

        // Fast path: this glyph, at this size and sub-pixel phase, is already
        // rasterized — blit its cached coverage. On a repaint (the common case:
        // scrolling re-rasterizes the same glyphs at the same fractions) this is
        // the whole win: a blit instead of re-outlining and re-filling.
        {
            let glyphs = self.glyphs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = glyphs.map.get(&key) {
                Self::blit(cached, target, floor_xi, base_i, baseline, color, residual);
                return true;
            }
        }

        // Miss: build (or reuse) the face+size hinter and rasterize the glyph at
        // this bucket's canonical sub-pixel position, then cache it.
        let Some(glyph) = outlines.get(GlyphId::new(id as u32)) else {
            return false;
        };
        let canon_pen_x = floor_x + bucket as f32 / SUBPX as f32;
        let cached = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let hinter = cache.entry((face_key, px)).or_insert_with(|| {
                HintingInstance::new(
                    outlines,
                    Size::new(px.max(1) as f32),
                    LocationRef::default(),
                    HintingOptions {
                        // Auto-hinter in light mode — the measured best match for
                        // the reference Chrome (see module docs); light means
                        // vertical-only grid-fitting, horizontal spacing
                        // untouched (FT_LOAD_TARGET_LIGHT).
                        engine: Engine::Auto(None),
                        target: Target::Smooth {
                            mode: SmoothMode::Light,
                            // FreeType always renders as if this is set.
                            symmetric_rendering: true,
                            // FreeType behaves as if disabled; advances stay
                            // rustybuzz's regardless (only outlines change).
                            preserve_linear_metrics: false,
                        },
                    },
                )
                .ok()
            });
            let Some(hinter) = hinter.as_ref() else {
                return false;
            };
            match Self::compute_coverage(&glyph, hinter, canon_pen_x, baseline) {
                Some(c) => c,
                None => return false,
            }
        };

        Self::blit(&cached, target, floor_xi, base_i, baseline, color, residual);
        self.glyphs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, cached);
        true
    }

    /// Rasterize one glyph's hinted outline into a reusable coverage map
    /// positioned at `canon_pen_x` / `baseline`. Colour and residual style are
    /// deliberately NOT applied here — they're per-draw and would break sharing.
    /// `None` means the outline couldn't be drawn (caller falls back to the
    /// unhinted path); an empty glyph (a space) is `Some` with a zero-size map.
    fn compute_coverage(
        glyph: &OutlineGlyph<'_>,
        hinter: &HintingInstance,
        canon_pen_x: f32,
        baseline: f32,
    ) -> Option<CachedGlyph> {
        // Collect the hinted outline in device space: skrifa draws y-up around
        // the glyph origin; the framebuffer is y-down with the origin at
        // (canon_pen_x, baseline).
        let mut pen = DevicePen::new(canon_pen_x, baseline);
        if glyph
            .draw(DrawSettings::hinted(hinter, false), &mut pen)
            .is_err()
        {
            return None;
        }
        let empty = CachedGlyph {
            w: 0,
            h: 0,
            rel_x: 0,
            rel_y: 0,
            cov: Vec::new(),
        };
        if pen.cmds.is_empty() {
            return Some(empty); // empty glyph (space): nothing to ink
        }

        // Pixel grid the outline touches (conservative: includes control points).
        let x0 = pen.min_x.floor();
        let y0 = pen.min_y.floor();
        let w = (pen.max_x.ceil() - x0) as usize;
        let h = (pen.max_y.ceil() - y0) as usize;
        if w == 0 || h == 0 {
            return Some(empty);
        }
        if w > MAX_GLYPH_RASTER || h > MAX_GLYPH_RASTER {
            return None;
        }
        let mut ras = CoverageRaster::new(w, h);
        let t = |p: (f32, f32)| rpoint(p.0 - x0, p.1 - y0);
        let (mut cur, mut start) = ((0.0f32, 0.0f32), (0.0f32, 0.0f32));
        let mut open = false;
        for cmd in &pen.cmds {
            match *cmd {
                Cmd::Move(p) => {
                    if open && cur != start {
                        ras.draw_line(t(cur), t(start)); // implicit close
                    }
                    cur = p;
                    start = p;
                    open = true;
                }
                Cmd::Line(p) => {
                    ras.draw_line(t(cur), t(p));
                    cur = p;
                }
                Cmd::Quad(c, p) => {
                    ras.draw_quad(t(cur), t(c), t(p));
                    cur = p;
                }
                Cmd::Curve(c0, c1, p) => {
                    ras.draw_cubic(t(cur), t(c0), t(c1), t(p));
                    cur = p;
                }
                Cmd::Close => {
                    if cur != start {
                        ras.draw_line(t(cur), t(start));
                    }
                    cur = start;
                    open = false;
                }
            }
        }
        if open && cur != start {
            ras.draw_line(t(cur), t(start));
        }

        // Bake coverage into a compact u8 alpha map (the ≤1/255 quantization is
        // imperceptible and standard for glyph caches).
        let mut cov = vec![0u8; w * h];
        ras.for_each_pixel_2d(|gx, gy, coverage| {
            if coverage > 0.0 {
                cov[gy as usize * w + gx as usize] = (coverage.min(1.0) * 255.0).round() as u8;
            }
        });
        Some(CachedGlyph {
            w: w as u16,
            h: h as u16,
            rel_x: x0 as i32 - canon_pen_x.floor() as i32,
            rel_y: y0 as i32 - baseline.round() as i32,
            cov,
        })
    }

    /// Composite a cached coverage map into `target` with `(floor_xi, base_i)` as
    /// the pen origin, synthesizing residual style exactly as the raster path
    /// does: faux-bold smears one pixel right, faux-italic shears scanlines above
    /// the baseline (~12°).
    fn blit(
        cached: &CachedGlyph,
        target: &mut Framebuffer,
        floor_xi: i32,
        base_i: i32,
        baseline: f32,
        color: Color,
        residual: FontStyle,
    ) {
        let (w, h) = (cached.w as i32, cached.h as i32);
        let slant = if residual.italic { 0.21f32 } else { 0.0 };
        for gy in 0..h {
            let y = base_i + cached.rel_y + gy;
            let shear = if slant != 0.0 {
                (slant * (baseline - y as f32)) as i32
            } else {
                0
            };
            let row = (gy * w) as usize;
            for gx in 0..w {
                let c = cached.cov[row + gx as usize];
                if c == 0 {
                    continue;
                }
                let x = floor_xi + cached.rel_x + gx + shear;
                let cov = c as f32 / 255.0;
                target.blend_pixel(x, y, color, cov);
                if residual.bold {
                    target.blend_pixel(x + 1, y, color, cov);
                }
            }
        }
    }
}

/// One outline path command in device (framebuffer) space.
enum Cmd {
    Move((f32, f32)),
    Line((f32, f32)),
    Quad((f32, f32), (f32, f32)),
    Curve((f32, f32), (f32, f32), (f32, f32)),
    Close,
}

/// An [`OutlinePen`] that records the outline transformed into device space
/// (y-down, origin at the glyph's pen position) and tracks its bounds.
struct DevicePen {
    pen_x: f32,
    baseline: f32,
    cmds: Vec<Cmd>,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl DevicePen {
    fn new(pen_x: f32, baseline: f32) -> Self {
        Self {
            pen_x,
            baseline,
            cmds: Vec::with_capacity(32),
            min_x: f32::MAX,
            min_y: f32::MAX,
            max_x: f32::MIN,
            max_y: f32::MIN,
        }
    }

    /// Glyph space (y-up at origin) → device space (y-down at the pen).
    fn dev(&mut self, x: f32, y: f32) -> (f32, f32) {
        let p = (self.pen_x + x, self.baseline - y);
        self.min_x = self.min_x.min(p.0);
        self.min_y = self.min_y.min(p.1);
        self.max_x = self.max_x.max(p.0);
        self.max_y = self.max_y.max(p.1);
        p
    }
}

impl OutlinePen for DevicePen {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.dev(x, y);
        self.cmds.push(Cmd::Move(p));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.dev(x, y);
        self.cmds.push(Cmd::Line(p));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let c = self.dev(cx0, cy0);
        let p = self.dev(x, y);
        self.cmds.push(Cmd::Quad(c, p));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let c0 = self.dev(cx0, cy0);
        let c1 = self.dev(cx1, cy1);
        let p = self.dev(x, y);
        self.cmds.push(Cmd::Curve(c0, c1, p));
    }

    fn close(&mut self) {
        self.cmds.push(Cmd::Close);
    }
}

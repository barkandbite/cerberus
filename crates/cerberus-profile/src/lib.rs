//! Coherent per-window ("per-head") browser fingerprint profiles.
//!
//! Every browser window Cerberus opens presents *one* consistent, plausible,
//! mainstream device to the page — never an impossible mash-up like an iOS user
//! agent atop an NVIDIA desktop GPU. A profile is derived **deterministically**
//! from a single per-head `u64` seed, so the same head always looks like the
//! same machine, while distinct heads look like distinct machines.
//!
//! Coherence is structural: every profile is grown from a hard-linked
//! [`Archetype`] (a real device class — "Windows 11 laptop, Chrome, Intel UHD
//! 630", say). The archetype pins the fields that *must* agree (OS ⟂ GPU family
//! ⟂ platform string ⟂ user-agent template) and offers small allowed-sets for
//! the fields that legitimately vary between two copies of the same device class
//! (screen resolution, core count). Per-field sampling draws from those sets via
//! independent seed sub-streams, so no draw can wander outside a coherent value.
//! [`ProfileOverrides`] then layers sparse operator overrides on top, and
//! [`Profile::validate`] repairs any override that would break coherence by
//! snapping it back to a legal value.
//!
//! This is not impersonation of a *specific* victim device; it is presenting a
//! believable member of a popular device class so the head blends into the
//! crowd (see the threat model). The derivation is pure — no `Date.now`, no
//! `Math.random`, no ambient state — matching the workspace determinism rule.
//!
//! The crate is a leaf: pure data model + derivation + a JS-prologue renderer
//! ([`Profile::profile_prologue`]) that a future prelude will read. It has no
//! integration wiring yet.

/// The font-enumeration surface: the catalog of font names a head can present
/// and the per-head sampling that makes the report privacy-safe.
pub mod fonts;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Operating-system family. Pins the platform string and the GPU backend family
/// (Direct3D/ANGLE on Windows, Metal/ANGLE on macOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Os {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    MacOs,
    /// A desktop Linux distribution.
    Linux,
}

/// CPU instruction-set family, as reported to UA client hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CpuArch {
    /// 64-bit x86 (`x86` / bitness 64 in UA-CH terms).
    X86_64,
    /// 64-bit ARM (`arm` / bitness 64), e.g. Apple silicon.
    Arm64,
}

/// Browser engine/brand family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Browser {
    /// Google Chrome (Chromium, `Google Chrome` brand).
    Chrome,
    /// Microsoft Edge (Chromium, `Microsoft Edge` brand).
    Edge,
    /// Mozilla Firefox (Gecko). No UA client hints, no `navigator.deviceMemory`.
    Firefox,
}

impl Browser {
    /// Chromium-family browsers expose UA client hints, `navigator.deviceMemory`
    /// and a bundled PDF plugin; Gecko (Firefox) exposes none of these.
    fn is_chromium(self) -> bool {
        matches!(self, Browser::Chrome | Browser::Edge)
    }
}

// ---------------------------------------------------------------------------
// Sub-structures
// ---------------------------------------------------------------------------

/// The `navigator.userAgentData` (UA client hints) surface. `None` on Firefox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UaData {
    /// Ordered `(brand, significant version)` pairs, including a GREASE brand.
    pub brands: Vec<(String, String)>,
    /// Always `false` here — Cerberus only models desktop device classes.
    pub mobile: bool,
    /// High-entropy platform name: `"Windows"` / `"macOS"` / `"Linux"`.
    pub platform: String,
    /// High-entropy CPU architecture: `"x86"` / `"arm"`.
    pub architecture: String,
    /// High-entropy bitness: `"64"`.
    pub bitness: String,
    /// High-entropy platform version (e.g. `"15.0.0"` for Windows 11).
    pub platform_version: String,
    /// High-entropy full browser version (e.g. `"142.0.7444.135"`).
    pub ua_full_version: String,
}

/// The WebGL identity strings. `vendor`/`renderer` are the masked values every
/// context returns; `unmasked_*` come from `WEBGL_debug_renderer_info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    /// Masked `gl.getParameter(gl.VENDOR)` — `"WebKit"` on Chromium.
    pub vendor: String,
    /// Masked `gl.getParameter(gl.RENDERER)` — `"WebKit WebGL"` on Chromium.
    pub renderer: String,
    /// Unmasked vendor, e.g. `"Google Inc. (Intel)"`.
    pub unmasked_vendor: String,
    /// Unmasked renderer, e.g. the full ANGLE/D3D11 or ANGLE/Metal string.
    pub unmasked_renderer: String,
}

/// The `window.screen` / `devicePixelRatio` surface. Widths/heights are in CSS
/// pixels (logical), matching what `screen.width` reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    /// Total logical screen width (`screen.width`).
    pub width: u32,
    /// Total logical screen height (`screen.height`).
    pub height: u32,
    /// Work-area width (`screen.availWidth`).
    pub avail_width: u32,
    /// Work-area height (`screen.availHeight`) — height minus reserved OS chrome.
    pub avail_height: u32,
    /// Work-area left inset (`screen.availLeft`).
    pub avail_left: u32,
    /// Work-area top inset (`screen.availTop`) — e.g. the macOS menu bar.
    pub avail_top: u32,
    /// Color depth in bits (`screen.colorDepth`).
    pub color_depth: u32,
    /// Pixel depth in bits (`screen.pixelDepth`) — equals `color_depth`.
    pub pixel_depth: u32,
    /// `window.devicePixelRatio` — `2.0` only on Retina/Apple device classes.
    pub device_pixel_ratio: f32,
}

/// A fully-resolved, internally-coherent browser fingerprint for one head.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// OS family.
    pub os: Os,
    /// Human OS version tag (e.g. `"11"`, `"14"`).
    pub os_version: String,
    /// `navigator.platform` — `"Win32"` / `"MacIntel"` / `"Linux x86_64"`.
    pub platform: &'static str,
    /// CPU architecture family.
    pub cpu_arch: CpuArch,
    /// Browser family.
    pub browser: Browser,
    /// Marketing major version (e.g. `142`).
    pub browser_major: u16,
    /// Full version string (e.g. `"142.0.7444.135"`).
    pub full_version: String,
    /// The full `navigator.userAgent` string.
    pub user_agent: String,
    /// UA client hints, or `None` on Firefox.
    pub ua_data: Option<UaData>,
    /// WebGL identity.
    pub gpu: Gpu,
    /// Screen / DPR surface.
    pub screen: Screen,
    /// Inner viewport `(innerWidth, innerHeight)` in CSS pixels.
    pub viewport: (u32, u32),
    /// `navigator.hardwareConcurrency`.
    pub hardware_concurrency: u8,
    /// `navigator.deviceMemory` (GiB, capped at 8), or `None` on Firefox.
    pub device_memory: Option<u8>,
    /// IANA timezone name (e.g. `"America/New_York"`).
    pub timezone: String,
    /// Minutes east of UTC for `timezone` (e.g. `-300`). Matches `timezone`.
    pub tz_offset_minutes: i16,
    /// `navigator.languages`, most-preferred first. `languages[0] == language`.
    pub languages: Vec<String>,
    /// `navigator.language` — always equal to `languages[0]`.
    pub language: String,
    /// The (bundled) font set the OS is presented as having.
    pub fonts: Vec<&'static str>,
    /// Whether a PDF plugin is advertised: Chromium `true`, Firefox `false`.
    pub plugins_has_pdf: bool,
    /// `navigator.maxTouchPoints` — `0` for the desktop classes modelled here.
    pub max_touch_points: u8,
    /// Per-head farbling noise seed (an independent sub-stream of the seed).
    pub noise_seed: u64,
}

// ---------------------------------------------------------------------------
// Archetype table
// ---------------------------------------------------------------------------

/// A real, mainstream device class. Hard-linked fields pin coherence; the
/// allowed-sets enumerate the values that legitimately vary within the class.
pub struct Archetype {
    /// Stable identifier for the class (used in tests / diagnostics).
    pub name: &'static str,
    /// Relative market-share weight for the weighted archetype draw.
    pub weight: u32,

    // ---- hard-linked (pinned) fields ----
    /// OS family.
    pub os: Os,
    /// CPU architecture family.
    pub cpu_arch: CpuArch,
    /// Browser family.
    pub browser: Browser,
    /// Marketing major version.
    pub browser_major: u16,
    /// Full version string.
    pub full_version: &'static str,
    /// Masked WebGL vendor.
    pub gpu_vendor: &'static str,
    /// Masked WebGL renderer.
    pub gpu_renderer: &'static str,
    /// Unmasked WebGL vendor.
    pub gpu_unmasked_vendor: &'static str,
    /// Unmasked WebGL renderer (coherent with `os`: D3D11 on Windows, Metal on
    /// macOS).
    pub gpu_unmasked_renderer: &'static str,
    /// Reported color depth (bits).
    pub color_depth: u32,
    /// Whether UA client hints are exposed (Chromium only).
    pub has_ua_data: bool,
    /// Whether `navigator.deviceMemory` is exposed (Chromium only).
    pub has_device_memory: bool,
    /// `navigator.maxTouchPoints`.
    pub max_touch_points: u8,

    // ---- allowed-sets (free, sampled) fields ----
    /// Allowed OS version tags.
    pub os_versions: &'static [&'static str],
    /// Allowed `(width, height, device_pixel_ratio)` triples.
    pub resolutions: &'static [(u32, u32, f32)],
    /// Allowed `hardwareConcurrency` values.
    pub cores: &'static [u8],
    /// Allowed `deviceMemory` values (GiB, already capped at 8).
    pub memory: &'static [u8],
}

/// The device-class table, market-share weighted (Windows Chrome dominates).
///
/// Every entry is internally coherent by construction: the GPU renderer's
/// backend matches the OS (Direct3D11 on Windows, Metal on macOS), the platform
/// string follows the OS, and the UA template follows the browser.
pub static ARCHETYPES: &[Archetype] = &[
    // The single most common desktop: a mainstream Intel-iGPU Windows laptop
    // running Chrome.
    Archetype {
        name: "WinChromeIntel1080",
        weight: 40,
        os: Os::Windows,
        cpu_arch: CpuArch::X86_64,
        browser: Browser::Chrome,
        browser_major: 142,
        full_version: "142.0.7444.135",
        gpu_vendor: "WebKit",
        gpu_renderer: "WebKit WebGL",
        gpu_unmasked_vendor: "Google Inc. (Intel)",
        gpu_unmasked_renderer:
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)",
        color_depth: 24,
        has_ua_data: true,
        has_device_memory: true,
        max_touch_points: 0,
        os_versions: &["10", "11"],
        resolutions: &[(1920, 1080, 1.0), (1366, 768, 1.0)],
        cores: &[4, 8],
        memory: &[8],
    },
    // A gaming/creator Windows 11 desktop with a discrete NVIDIA GPU running
    // Chrome. (16 GiB machines still report deviceMemory 8 — Chrome caps it.)
    Archetype {
        name: "WinChromeNvidia1440",
        weight: 20,
        os: Os::Windows,
        cpu_arch: CpuArch::X86_64,
        browser: Browser::Chrome,
        browser_major: 142,
        full_version: "142.0.7444.135",
        gpu_vendor: "WebKit",
        gpu_renderer: "WebKit WebGL",
        gpu_unmasked_vendor: "Google Inc. (NVIDIA)",
        gpu_unmasked_renderer:
            "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 (0x00002504) Direct3D11 vs_5_0 ps_5_0, D3D11)",
        color_depth: 24,
        has_ua_data: true,
        has_device_memory: true,
        max_touch_points: 0,
        os_versions: &["11"],
        resolutions: &[(2560, 1440, 1.0), (1920, 1080, 1.0)],
        cores: &[8, 12, 16],
        memory: &[8],
    },
    // A Windows 11 desktop with an AMD Radeon GPU running Edge.
    Archetype {
        name: "WinEdgeAmd1080",
        weight: 12,
        os: Os::Windows,
        cpu_arch: CpuArch::X86_64,
        browser: Browser::Edge,
        browser_major: 142,
        full_version: "142.0.3595.94",
        gpu_vendor: "WebKit",
        gpu_renderer: "WebKit WebGL",
        gpu_unmasked_vendor: "Google Inc. (AMD)",
        gpu_unmasked_renderer: "ANGLE (AMD, AMD Radeon RX 6600 Direct3D11 vs_5_0 ps_5_0, D3D11)",
        color_depth: 24,
        has_ua_data: true,
        has_device_memory: true,
        max_touch_points: 0,
        os_versions: &["11"],
        resolutions: &[(1920, 1080, 1.0), (2560, 1440, 1.0)],
        cores: &[6, 12],
        memory: &[8],
    },
    // An Apple-silicon MacBook running Chrome on a Retina display.
    Archetype {
        name: "MacChromeAppleRetina",
        weight: 15,
        os: Os::MacOs,
        cpu_arch: CpuArch::Arm64,
        browser: Browser::Chrome,
        browser_major: 142,
        full_version: "142.0.7444.135",
        gpu_vendor: "WebKit",
        gpu_renderer: "WebKit WebGL",
        gpu_unmasked_vendor: "Google Inc. (Apple)",
        gpu_unmasked_renderer: "ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)",
        color_depth: 30,
        has_ua_data: true,
        has_device_memory: true,
        max_touch_points: 0,
        os_versions: &["14"],
        resolutions: &[(1512, 982, 2.0), (1470, 956, 2.0)],
        cores: &[8],
        memory: &[8],
    },
    // A mainstream Intel-iGPU Windows laptop running Firefox — exercises the
    // Gecko path (no UA-CH, no deviceMemory, no PDF plugin). Firefox on Windows
    // also drives WebGL through ANGLE/D3D11, so the GPU stays coherent.
    Archetype {
        name: "WinFirefoxIntel1080",
        weight: 13,
        os: Os::Windows,
        cpu_arch: CpuArch::X86_64,
        browser: Browser::Firefox,
        browser_major: 143,
        full_version: "143.0",
        gpu_vendor: "WebKit",
        gpu_renderer: "WebKit WebGL",
        gpu_unmasked_vendor: "Google Inc. (Intel)",
        gpu_unmasked_renderer:
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)",
        color_depth: 24,
        has_ua_data: false,
        has_device_memory: false,
        max_touch_points: 0,
        os_versions: &["10", "11"],
        resolutions: &[(1920, 1080, 1.0), (1366, 768, 1.0)],
        cores: &[4, 8],
        memory: &[8],
    },
];

// ---------------------------------------------------------------------------
// Locale pool
// ---------------------------------------------------------------------------

/// `(timezone, offset-minutes-east-of-UTC, languages)` tuples, each a coherent
/// unit. A profile always draws one whole tuple, so the timezone, its numeric
/// offset, and the language list never desync.
pub static LOCALE_POOL: &[(&str, i16, &[&str])] = &[
    ("America/New_York", -300, &["en-US", "en"]),
    ("America/Chicago", -360, &["en-US", "en"]),
    ("America/Los_Angeles", -480, &["en-US", "en"]),
    ("Europe/London", 0, &["en-GB", "en"]),
    ("Europe/Berlin", 60, &["de-DE", "de", "en"]),
    ("Europe/Paris", 60, &["fr-FR", "fr", "en"]),
    ("Europe/Madrid", 60, &["es-ES", "es", "en"]),
    ("Asia/Tokyo", 540, &["ja-JP", "ja"]),
    ("Australia/Sydney", 600, &["en-AU", "en"]),
];

/// Look up the canonical offset for a pool timezone, if known. Used by
/// [`Profile::validate`] to resync an overridden timezone's offset.
fn offset_for_timezone(tz: &str) -> Option<i16> {
    LOCALE_POOL
        .iter()
        .find(|(name, _, _)| *name == tz)
        .map(|(_, off, _)| *off)
}

// ---------------------------------------------------------------------------
// Overrides
// ---------------------------------------------------------------------------

/// Sparse per-field operator overrides. Every field is optional; unset fields
/// keep their seed-derived value. Overrides that would break coherence are
/// repaired by [`Profile::validate`] rather than trusted blindly.
#[derive(Clone, Debug, Default)]
pub struct ProfileOverrides {
    /// Force a whole locale tuple from [`LOCALE_POOL`] by index (applied first).
    pub locale_index: Option<usize>,
    /// Force the timezone name (its offset is resynced if the zone is known).
    pub timezone: Option<String>,
    /// Force the numeric UTC offset (minutes east).
    pub tz_offset_minutes: Option<i16>,
    /// Force the language list.
    pub languages: Option<Vec<String>>,
    /// Force the primary language (also becomes `languages[0]`).
    pub language: Option<String>,
    /// Force `hardwareConcurrency` (clamped to at least 1).
    pub hardware_concurrency: Option<u8>,
    /// Force `deviceMemory` (`Some(None)` clears it; values are clamped/capped).
    pub device_memory: Option<Option<u8>>,
    /// Force `devicePixelRatio` (snapped to the value legal for the OS).
    pub device_pixel_ratio: Option<f32>,
    /// Force the inner viewport.
    pub viewport: Option<(u32, u32)>,
    /// Force the farbling noise seed.
    pub noise_seed: Option<u64>,
}

// ---------------------------------------------------------------------------
// Derivation
// ---------------------------------------------------------------------------

/// The golden-ratio odd constant used to decorrelate per-field sub-streams.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

// Distinct per-field stream tags. Each field samples from an independent
// sub-stream `splitmix64(seed ^ tag*GOLDEN)`, so fields do not correlate.
const TAG_ARCH: u64 = 1;
const TAG_RES: u64 = 2;
const TAG_CORES: u64 = 3;
const TAG_MEM: u64 = 4;
const TAG_OSVER: u64 = 5;
const TAG_LOCALE: u64 = 6;
const TAG_NOISE: u64 = 7;
const TAG_FONTS: u64 = 8;

/// Approximate pixels of OS chrome reserved at the bottom (Windows taskbar).
const WINDOWS_TASKBAR: u32 = 40;
/// Approximate pixels of OS chrome reserved at the top (macOS menu bar).
const MACOS_MENUBAR: u32 = 25;
/// Approximate pixels of browser chrome above the content viewport.
const BROWSER_TOOLBAR: u32 = 79;

/// Derive the coherent [`Profile`] for a head `seed`, then apply `overrides`.
///
/// Pure and deterministic: identical `(seed, overrides)` always yield an
/// identical profile. No ambient time or randomness is consulted. The archetype
/// is picked by market-share weight, each free field is sampled from its
/// archetype's allowed-set via an independent seed sub-stream, overrides are
/// layered on, and [`Profile::validate`] repairs anything incoherent.
pub fn derive_profile(seed: u64, overrides: &ProfileOverrides) -> Profile {
    let arch = pick_archetype(seed);

    // Free fields, each from an independent sub-stream.
    let (width, height, dpr) = *pick(arch.resolutions, sub_stream(seed, TAG_RES));
    let cores = *pick(arch.cores, sub_stream(seed, TAG_CORES));
    let mem = *pick(arch.memory, sub_stream(seed, TAG_MEM));
    let os_version = (*pick(arch.os_versions, sub_stream(seed, TAG_OSVER))).to_string();
    let (tz, tz_off, langs) = *pick(LOCALE_POOL, sub_stream(seed, TAG_LOCALE));
    let noise_seed = splitmix64(seed ^ TAG_NOISE.wrapping_mul(GOLDEN));

    let user_agent = build_user_agent(arch);
    let ua_data = if arch.has_ua_data {
        Some(build_ua_data(arch, &os_version))
    } else {
        None
    };
    let device_memory = if arch.has_device_memory {
        Some(mem)
    } else {
        None
    };

    let languages: Vec<String> = langs.iter().map(|s| (*s).to_string()).collect();
    let language = languages[0].clone();

    let mut profile = Profile {
        os: arch.os,
        os_version,
        platform: platform_for_os(arch.os),
        cpu_arch: arch.cpu_arch,
        browser: arch.browser,
        browser_major: arch.browser_major,
        full_version: arch.full_version.to_string(),
        user_agent,
        ua_data,
        gpu: Gpu {
            vendor: arch.gpu_vendor.to_string(),
            renderer: arch.gpu_renderer.to_string(),
            unmasked_vendor: arch.gpu_unmasked_vendor.to_string(),
            unmasked_renderer: arch.gpu_unmasked_renderer.to_string(),
        },
        screen: Screen {
            width,
            height,
            avail_width: width,
            avail_height: height,
            avail_left: 0,
            avail_top: 0,
            color_depth: arch.color_depth,
            pixel_depth: arch.color_depth,
            device_pixel_ratio: dpr,
        },
        viewport: (width, height),
        hardware_concurrency: cores,
        device_memory,
        timezone: tz.to_string(),
        tz_offset_minutes: tz_off,
        languages,
        language,
        // A per-head-random, OS-coherent font set drawn from the catalog, so the
        // three heads present different enumerations and no stable cross-site
        // font-fingerprint forms (see the `fonts` module).
        fonts: fonts::derive_fonts(sub_stream(seed, TAG_FONTS), arch.os),
        plugins_has_pdf: arch.browser.is_chromium(),
        max_touch_points: arch.max_touch_points,
        noise_seed,
    };

    apply_overrides(&mut profile, overrides);
    profile.validate();
    profile
}

/// Weighted archetype selection from the seed's dedicated sub-stream.
fn pick_archetype(seed: u64) -> &'static Archetype {
    let total: u64 = ARCHETYPES.iter().map(|a| u64::from(a.weight)).sum();
    let mut r = sub_stream(seed, TAG_ARCH) % total;
    for a in ARCHETYPES {
        let w = u64::from(a.weight);
        if r < w {
            return a;
        }
        r -= w;
    }
    // Unreachable: r < total and the weights sum to total.
    &ARCHETYPES[ARCHETYPES.len() - 1]
}

/// Pick an element of `slice` (non-empty) using a sub-stream value.
fn pick<T>(slice: &[T], stream: u64) -> &T {
    &slice[(stream % slice.len() as u64) as usize]
}

/// Derive an independent sub-stream value for `tag` from `seed`.
fn sub_stream(seed: u64, tag: u64) -> u64 {
    splitmix64(seed ^ tag.wrapping_mul(GOLDEN))
}

// ---------------------------------------------------------------------------
// Override application
// ---------------------------------------------------------------------------

fn apply_overrides(p: &mut Profile, o: &ProfileOverrides) {
    if let Some(i) = o.locale_index {
        let (tz, off, langs) = LOCALE_POOL[i % LOCALE_POOL.len()];
        p.timezone = tz.to_string();
        p.tz_offset_minutes = off;
        p.languages = langs.iter().map(|s| (*s).to_string()).collect();
        p.language = p.languages[0].clone();
    }
    if let Some(tz) = &o.timezone {
        p.timezone = tz.clone();
    }
    if let Some(off) = o.tz_offset_minutes {
        p.tz_offset_minutes = off;
    }
    if let Some(langs) = &o.languages {
        if !langs.is_empty() {
            p.languages = langs.clone();
        }
    }
    if let Some(lang) = &o.language {
        // A lone language override becomes the head of the language list, so the
        // `languages[0] == language` invariant survives.
        p.language = lang.clone();
        if p.languages.is_empty() {
            p.languages.push(lang.clone());
        } else {
            p.languages[0] = lang.clone();
        }
    }
    if let Some(hc) = o.hardware_concurrency {
        p.hardware_concurrency = hc;
    }
    if let Some(dm) = o.device_memory {
        p.device_memory = dm;
    }
    if let Some(dpr) = o.device_pixel_ratio {
        p.screen.device_pixel_ratio = dpr;
    }
    if let Some(vp) = o.viewport {
        p.viewport = vp;
    }
    if let Some(ns) = o.noise_seed {
        p.noise_seed = ns;
    }
}

// ---------------------------------------------------------------------------
// Validation / coherence repair
// ---------------------------------------------------------------------------

impl Profile {
    /// Repair the profile toward coherence, snapping any field (typically one an
    /// override just set) that would produce an impossible device back to a
    /// legal value. Idempotent: validating a coherent profile is a no-op.
    pub fn validate(&mut self) {
        // Platform string is a pure function of the OS.
        self.platform = platform_for_os(self.os);

        // Pixel depth always mirrors color depth in real browsers.
        self.screen.pixel_depth = self.screen.color_depth;

        // devicePixelRatio: 2.0 is a Retina/Apple trait only. Snap to the value
        // legal for this OS (macOS → 2.0, everything else → 1.0).
        self.screen.device_pixel_ratio = match self.os {
            Os::MacOs => 2.0,
            Os::Windows | Os::Linux => 1.0,
        };

        // Work area = screen minus reserved OS chrome, recomputed from the OS so
        // availHeight is always < height on the desktop classes modelled here.
        self.screen.avail_left = 0;
        match self.os {
            Os::Windows => {
                self.screen.avail_top = 0;
                self.screen.avail_width = self.screen.width;
                self.screen.avail_height = self.screen.height.saturating_sub(WINDOWS_TASKBAR);
            }
            Os::MacOs => {
                self.screen.avail_top = MACOS_MENUBAR;
                self.screen.avail_width = self.screen.width;
                self.screen.avail_height = self.screen.height.saturating_sub(MACOS_MENUBAR);
            }
            Os::Linux => {
                self.screen.avail_top = 0;
                self.screen.avail_width = self.screen.width;
                self.screen.avail_height = self.screen.height;
            }
        }

        // Viewport must fit inside the work area.
        let max_h = self
            .screen
            .avail_height
            .saturating_sub(BROWSER_TOOLBAR)
            .max(1);
        if self.viewport.0 == 0 || self.viewport.0 > self.screen.avail_width {
            self.viewport.0 = self.screen.avail_width;
        }
        if self.viewport.1 == 0 || self.viewport.1 > max_h {
            self.viewport.1 = max_h;
        }

        // Languages must be non-empty and `language` must equal `languages[0]`.
        if self.languages.is_empty() {
            self.languages.push(self.language.clone());
        } else {
            self.language = self.languages[0].clone();
        }

        // A known timezone forces its canonical offset (tz ⟂ offset).
        if let Some(off) = offset_for_timezone(&self.timezone) {
            self.tz_offset_minutes = off;
        }

        // hardwareConcurrency is at least 1.
        self.hardware_concurrency = self.hardware_concurrency.max(1);

        // Chromium-only surfaces: Firefox never exposes UA-CH, deviceMemory, or a
        // PDF plugin; Chromium always advertises the PDF plugin.
        if self.browser.is_chromium() {
            self.plugins_has_pdf = true;
            if let Some(m) = self.device_memory {
                self.device_memory = Some(clamp_device_memory(m));
            }
            // Keep UA-CH platform/arch/bitness coherent with the OS/CPU.
            if let Some(ua) = self.ua_data.as_mut() {
                ua.platform = ua_platform(self.os).to_string();
                ua.architecture = ua_arch(self.cpu_arch).to_string();
                ua.bitness = "64".to_string();
                ua.mobile = false;
            }
        } else {
            self.plugins_has_pdf = false;
            self.device_memory = None;
            self.ua_data = None;
        }
    }

    /// True when every coherence invariant holds. Used by the test-suite to
    /// prove derivation never emits an impossible device; also a handy
    /// post-condition after external mutation.
    pub fn is_coherent(&self) -> bool {
        // platform ⟂ os
        if self.platform != platform_for_os(self.os) {
            return false;
        }
        // GPU backend ⟂ os (D3D11 on Windows, Metal on macOS).
        let r = &self.gpu.unmasked_renderer;
        let gpu_ok = match self.os {
            Os::Windows => r.contains("Direct3D11") && !r.contains("Metal"),
            Os::MacOs => r.contains("Metal") && !r.contains("Direct3D11"),
            Os::Linux => true,
        };
        if !gpu_ok {
            return false;
        }
        // Work area strictly inside the screen for the desktop classes here.
        if self.screen.avail_height >= self.screen.height && self.os != Os::Linux {
            return false;
        }
        if self.screen.avail_width > self.screen.width {
            return false;
        }
        // devicePixelRatio rule.
        let dpr_ok = match self.os {
            Os::MacOs => feq(self.screen.device_pixel_ratio, 2.0),
            Os::Windows | Os::Linux => feq(self.screen.device_pixel_ratio, 1.0),
        };
        if !dpr_ok {
            return false;
        }
        // pixelDepth mirrors colorDepth.
        if self.screen.pixel_depth != self.screen.color_depth {
            return false;
        }
        // Languages coherence.
        if self.languages.is_empty() || self.languages[0] != self.language {
            return false;
        }
        // Timezone ⟂ offset for known zones.
        if let Some(off) = offset_for_timezone(&self.timezone) {
            if off != self.tz_offset_minutes {
                return false;
            }
        }
        // Browser-family surfaces.
        match self.browser {
            Browser::Firefox => {
                if self.ua_data.is_some() || self.device_memory.is_some() || self.plugins_has_pdf {
                    return false;
                }
            }
            Browser::Chrome | Browser::Edge => {
                if !self.plugins_has_pdf {
                    return false;
                }
            }
        }
        // deviceMemory must be a legal quantized value if present.
        if let Some(m) = self.device_memory {
            if !matches!(m, 1 | 2 | 4 | 8) {
                return false;
            }
        }
        if self.hardware_concurrency == 0 {
            return false;
        }
        true
    }

    /// The full `navigator.userAgent` string.
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The `Accept-Language` header value derived from `languages`, with the
    /// conventional descending `q` weights (`en-US,en;q=0.9`).
    pub fn accept_language(&self) -> String {
        let mut out = String::new();
        for (i, lang) in self.languages.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            if i == 0 {
                out.push_str(lang);
            } else {
                // q = 1.0 - i/10, floored at 0.1.
                let q = (10 - (i as i32).min(9)) as f32 / 10.0;
                out.push_str(lang);
                out.push_str(";q=");
                out.push_str(&format_q(q));
            }
        }
        out
    }

    /// A JS prologue installing `globalThis.__CERBERUS_PROFILE__` — a plain
    /// object literal (valid JSON, hence valid JS) that a future prelude reads to
    /// answer `navigator`/`screen`/WebGL/timezone/language queries with these
    /// coherent values. All strings are escaped.
    pub fn profile_prologue(&self) -> String {
        let mut o = String::with_capacity(1536);
        o.push_str("globalThis.__CERBERUS_PROFILE__ = {");

        // ---- navigator ----
        o.push_str("\"navigator\":{");
        push_field(&mut o, "userAgent", &esc(&self.user_agent), true);
        push_field(&mut o, "platform", &esc(self.platform), false);
        push_field(
            &mut o,
            "vendor",
            &esc(navigator_vendor(self.browser)),
            false,
        );
        push_field(
            &mut o,
            "hardwareConcurrency",
            &self.hardware_concurrency.to_string(),
            false,
        );
        let dm = match self.device_memory {
            Some(m) => m.to_string(),
            None => "null".to_string(),
        };
        push_field(&mut o, "deviceMemory", &dm, false);
        push_field(&mut o, "language", &esc(&self.language), false);
        o.push_str(",\"languages\":");
        push_str_array(&mut o, &self.languages);
        push_field(
            &mut o,
            "maxTouchPoints",
            &self.max_touch_points.to_string(),
            false,
        );
        o.push_str(",\"userAgentData\":");
        match &self.ua_data {
            Some(ua) => push_ua_data(&mut o, ua),
            None => o.push_str("null"),
        }
        o.push('}');

        // ---- screen ----
        o.push_str(",\"screen\":{");
        let s = &self.screen;
        push_field(&mut o, "width", &s.width.to_string(), true);
        push_field(&mut o, "height", &s.height.to_string(), false);
        push_field(&mut o, "availWidth", &s.avail_width.to_string(), false);
        push_field(&mut o, "availHeight", &s.avail_height.to_string(), false);
        push_field(&mut o, "availLeft", &s.avail_left.to_string(), false);
        push_field(&mut o, "availTop", &s.avail_top.to_string(), false);
        push_field(&mut o, "colorDepth", &s.color_depth.to_string(), false);
        push_field(&mut o, "pixelDepth", &s.pixel_depth.to_string(), false);
        push_field(
            &mut o,
            "devicePixelRatio",
            &format_dpr(s.device_pixel_ratio),
            false,
        );
        o.push('}');

        // ---- viewport ----
        o.push_str(",\"viewport\":{");
        push_field(&mut o, "innerWidth", &self.viewport.0.to_string(), true);
        push_field(&mut o, "innerHeight", &self.viewport.1.to_string(), false);
        o.push('}');

        // ---- gpu ----
        o.push_str(",\"gpu\":{");
        push_field(&mut o, "vendor", &esc(&self.gpu.vendor), true);
        push_field(&mut o, "renderer", &esc(&self.gpu.renderer), false);
        push_field(
            &mut o,
            "unmaskedVendor",
            &esc(&self.gpu.unmasked_vendor),
            false,
        );
        push_field(
            &mut o,
            "unmaskedRenderer",
            &esc(&self.gpu.unmasked_renderer),
            false,
        );
        o.push('}');

        // ---- top-level scalars ----
        push_field(&mut o, "timezone", &esc(&self.timezone), false);
        push_field(
            &mut o,
            "tzOffsetMinutes",
            &self.tz_offset_minutes.to_string(),
            false,
        );
        push_field(&mut o, "language", &esc(&self.language), false);
        o.push_str(",\"languages\":");
        push_str_array(&mut o, &self.languages);
        o.push_str(",\"fonts\":");
        push_str_array(&mut o, &self.fonts);
        push_field(
            &mut o,
            "pluginsHasPdf",
            if self.plugins_has_pdf {
                "true"
            } else {
                "false"
            },
            false,
        );
        push_field(&mut o, "noiseSeed", &self.noise_seed.to_string(), false);

        o.push_str("};");
        o
    }
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

fn platform_for_os(os: Os) -> &'static str {
    match os {
        Os::Windows => "Win32",
        Os::MacOs => "MacIntel",
        Os::Linux => "Linux x86_64",
    }
}

fn ua_platform(os: Os) -> &'static str {
    match os {
        Os::Windows => "Windows",
        Os::MacOs => "macOS",
        Os::Linux => "Linux",
    }
}

fn ua_arch(arch: CpuArch) -> &'static str {
    match arch {
        CpuArch::X86_64 => "x86",
        CpuArch::Arm64 => "arm",
    }
}

fn navigator_vendor(browser: Browser) -> &'static str {
    match browser {
        // Chromium reports "Google Inc." for navigator.vendor; Firefox reports "".
        Browser::Chrome | Browser::Edge => "Google Inc.",
        Browser::Firefox => "",
    }
}

/// UA-CH high-entropy platform version for the OS version tag.
fn platform_version(os: Os, os_version: &str) -> String {
    match os {
        Os::Windows => {
            // Windows 11 reports 15.x; Windows 10 reports 10.x (UA-CH mapping).
            if os_version == "11" {
                "15.0.0".to_string()
            } else {
                "10.0.0".to_string()
            }
        }
        Os::MacOs => format!("{os_version}.5.0"),
        Os::Linux => "6.8.0".to_string(),
    }
}

/// Chromium GREASE + brand list for UA-CH.
fn brands(browser: Browser, major: u16) -> Vec<(String, String)> {
    let m = major.to_string();
    match browser {
        Browser::Chrome => vec![
            ("Not)A;Brand".to_string(), "99".to_string()),
            ("Google Chrome".to_string(), m.clone()),
            ("Chromium".to_string(), m),
        ],
        Browser::Edge => vec![
            ("Not)A;Brand".to_string(), "99".to_string()),
            ("Microsoft Edge".to_string(), m.clone()),
            ("Chromium".to_string(), m),
        ],
        // Firefox has no UA-CH; this is never reached (has_ua_data == false).
        Browser::Firefox => Vec::new(),
    }
}

fn build_ua_data(a: &Archetype, os_version: &str) -> UaData {
    UaData {
        brands: brands(a.browser, a.browser_major),
        mobile: false,
        platform: ua_platform(a.os).to_string(),
        architecture: ua_arch(a.cpu_arch).to_string(),
        bitness: "64".to_string(),
        platform_version: platform_version(a.os, os_version),
        ua_full_version: a.full_version.to_string(),
    }
}

/// Build the `navigator.userAgent` string for an archetype. Chromium uses the
/// reduced (`MAJOR.0.0.0`) product token; the real build lives in UA-CH.
fn build_user_agent(a: &Archetype) -> String {
    let maj = a.browser_major;
    match (a.os, a.browser) {
        (Os::Windows, Browser::Chrome) => format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36"
        ),
        (Os::Windows, Browser::Edge) => format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36 Edg/{}",
            a.full_version
        ),
        (Os::MacOs, Browser::Chrome) => format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36"
        ),
        (Os::MacOs, Browser::Edge) => format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36 Edg/{}",
            a.full_version
        ),
        (Os::Linux, Browser::Chrome) => format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36"
        ),
        (Os::Linux, Browser::Edge) => format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/{maj}.0.0.0 Safari/537.36 Edg/{}",
            a.full_version
        ),
        (os, Browser::Firefox) => {
            let token = match os {
                Os::Windows => "Windows NT 10.0; Win64; x64",
                Os::MacOs => "Macintosh; Intel Mac OS X 10.15",
                Os::Linux => "X11; Linux x86_64",
            };
            format!("Mozilla/5.0 ({token}; rv:{maj}.0) Gecko/20100101 Firefox/{maj}.0")
        }
    }
}

fn clamp_device_memory(m: u8) -> u8 {
    // Chrome quantizes to a power of two and caps at 8 for privacy.
    if m >= 8 {
        8
    } else if m >= 4 {
        4
    } else if m >= 2 {
        2
    } else {
        1
    }
}

fn feq(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-6
}

/// Format a `q` weight as `0.9`, `0.8`, … (one decimal place).
fn format_q(q: f32) -> String {
    let tenths = (q * 10.0).round() as i32;
    format!("0.{}", tenths.rem_euclid(10))
}

/// Format a devicePixelRatio: integral ratios stay bare (`1`, `2`), otherwise
/// two decimals (`1.50`). Always a valid JS number literal.
fn format_dpr(dpr: f32) -> String {
    if (dpr - dpr.round()).abs() < 1e-6 {
        format!("{}", dpr.round() as i64)
    } else {
        format!("{dpr:.2}")
    }
}

// ---------------------------------------------------------------------------
// JSON / JS-literal emission
// ---------------------------------------------------------------------------

/// Append `,"key":value` (or `"key":value` when `first`) to a JS object body.
/// `value` is inserted verbatim, so pre-escape strings with [`esc`].
fn push_field(out: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(value);
}

/// Append a JSON array of strings, escaping each element.
fn push_str_array<S: AsRef<str>>(out: &mut String, items: &[S]) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(item.as_ref()));
    }
    out.push(']');
}

fn push_ua_data(out: &mut String, ua: &UaData) {
    out.push_str("{\"brands\":[");
    for (i, (brand, version)) in ua.brands.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"brand\":");
        out.push_str(&esc(brand));
        out.push_str(",\"version\":");
        out.push_str(&esc(version));
        out.push('}');
    }
    out.push(']');
    push_field(
        out,
        "mobile",
        if ua.mobile { "true" } else { "false" },
        false,
    );
    push_field(out, "platform", &esc(&ua.platform), false);
    push_field(out, "architecture", &esc(&ua.architecture), false);
    push_field(out, "bitness", &esc(&ua.bitness), false);
    push_field(out, "platformVersion", &esc(&ua.platform_version), false);
    push_field(out, "uaFullVersion", &esc(&ua.ua_full_version), false);
    out.push('}');
}

/// Return `s` as a quoted, escaped JSON/JS string literal. Minimal std-only
/// escaper mirroring the workspace's `write_json_string`.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let n = c as u32;
                out.push_str("\\u00");
                out.push(char::from_digit((n >> 4) & 0xf, 16).unwrap());
                out.push(char::from_digit(n & 0xf, 16).unwrap());
            }
            // JS-only line terminators that are legal inside JSON strings but
            // break a bare <script>; escape them defensively.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// SplitMix64 finalizer — small, fast, well-distributed, and a bijection over
/// `u64` (so distinct seeds give distinct sub-streams). Fingerprint derivation
/// only; never anything security-sensitive.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SAMPLE_SEEDS: [u64; 3] = [1, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0];

    #[test]
    fn derivation_is_deterministic() {
        for seed in SAMPLE_SEEDS {
            let a = derive_profile(seed, &ProfileOverrides::default());
            let b = derive_profile(seed, &ProfileOverrides::default());
            assert_eq!(a, b, "seed {seed:#x} derived two different profiles");
        }
        // Determinism holds with overrides applied, too.
        let ov = ProfileOverrides {
            locale_index: Some(4),
            hardware_concurrency: Some(6),
            ..Default::default()
        };
        for seed in SAMPLE_SEEDS {
            assert_eq!(derive_profile(seed, &ov), derive_profile(seed, &ov));
        }
    }

    #[test]
    fn coherent_over_many_seeds() {
        for seed in 0u64..100_000 {
            let p = derive_profile(seed, &ProfileOverrides::default());
            assert!(p.is_coherent(), "incoherent profile at seed {seed}");

            // Spell out the headline invariants explicitly as well.
            assert!(
                p.screen.avail_height < p.screen.height,
                "availHeight !< height at seed {seed}"
            );
            match p.os {
                Os::MacOs => assert!(feq(p.screen.device_pixel_ratio, 2.0)),
                Os::Windows | Os::Linux => assert!(feq(p.screen.device_pixel_ratio, 1.0)),
            }
            assert_eq!(p.languages[0], p.language, "lang desync at seed {seed}");
            assert_eq!(
                offset_for_timezone(&p.timezone),
                Some(p.tz_offset_minutes),
                "tz/offset desync at seed {seed}"
            );
            // No impossible OS/GPU pairing.
            match p.os {
                Os::Windows => {
                    assert!(p.gpu.unmasked_renderer.contains("Direct3D11"));
                    assert!(!p.gpu.unmasked_renderer.contains("Metal"));
                }
                Os::MacOs => {
                    assert!(p.gpu.unmasked_renderer.contains("Metal"));
                    assert!(!p.gpu.unmasked_renderer.contains("Direct3D11"));
                }
                Os::Linux => {}
            }
        }
    }

    #[test]
    fn noise_seed_is_distinct_across_seeds() {
        let seeds: HashSet<u64> = (0u64..20_000)
            .map(|s| derive_profile(s, &ProfileOverrides::default()).noise_seed)
            .collect();
        assert_eq!(seeds.len(), 20_000, "noise_seed collided across head seeds");
    }

    #[test]
    fn weighted_distribution_favours_windows_chrome() {
        let mut win_chrome = 0u32;
        let mut win_edge = 0u32;
        let mut win_firefox = 0u32;
        let mut mac_chrome = 0u32;
        let n = 20_000u64;
        for seed in 0..n {
            let p = derive_profile(seed, &ProfileOverrides::default());
            match (p.os, p.browser) {
                (Os::Windows, Browser::Chrome) => win_chrome += 1,
                (Os::Windows, Browser::Edge) => win_edge += 1,
                (Os::Windows, Browser::Firefox) => win_firefox += 1,
                (Os::MacOs, Browser::Chrome) => mac_chrome += 1,
                _ => {}
            }
        }
        // Windows+Chrome (weights 40+20=60%) must dominate every other class.
        assert!(win_chrome > win_edge, "{win_chrome} !> {win_edge}");
        assert!(win_chrome > win_firefox, "{win_chrome} !> {win_firefox}");
        assert!(win_chrome > mac_chrome, "{win_chrome} !> {mac_chrome}");
        // Sanity: it should be a clear majority of the sample.
        assert!(
            u64::from(win_chrome) > n / 2,
            "windows-chrome share too low: {win_chrome}/{n}"
        );
        // Every modelled minority class should still appear.
        assert!(win_edge > 0 && win_firefox > 0 && mac_chrome > 0);
    }

    #[test]
    fn prologue_emits_parseable_looking_js() {
        for seed in SAMPLE_SEEDS {
            let p = derive_profile(seed, &ProfileOverrides::default());
            let js = p.profile_prologue();
            assert!(js.contains("__CERBERUS_PROFILE__"));
            // The GPU renderer string (parens, commas — but no quotes) survives
            // verbatim inside its JSON string.
            assert!(
                js.contains(&p.gpu.unmasked_renderer),
                "renderer missing from prologue"
            );
            assert!(js.trim_end().ends_with("};"));
            // No stray unescaped quote breaks the literal.
            assert!(
                unescaped_quotes_balanced(&js),
                "unbalanced quotes in prologue for seed {seed:#x}"
            );
            // The object literal must open cleanly.
            assert!(js.contains("globalThis.__CERBERUS_PROFILE__ = {"));
        }
    }

    #[test]
    fn firefox_has_no_chromium_only_surfaces() {
        // Find a Firefox head and confirm the Gecko path is coherent even when an
        // operator tries to force Chromium-only fields.
        let ov = ProfileOverrides {
            device_memory: Some(Some(16)),
            ..Default::default()
        };
        let mut seen = false;
        for seed in 0u64..1000 {
            let p = derive_profile(seed, &ov);
            if p.browser == Browser::Firefox {
                seen = true;
                assert!(p.ua_data.is_none(), "firefox must not expose UA-CH");
                assert!(p.device_memory.is_none(), "firefox deviceMemory forced off");
                assert!(!p.plugins_has_pdf, "firefox advertises no PDF plugin");
                assert!(p.accept_language().starts_with(&p.language));
            }
        }
        assert!(seen, "no Firefox head appeared in 1000 seeds");
    }

    #[test]
    fn override_snaps_incoherent_dpr_back() {
        // A Windows head cannot be Retina: a forced dpr of 2.0 must snap to 1.0.
        let ov = ProfileOverrides {
            device_pixel_ratio: Some(2.0),
            ..Default::default()
        };
        let mut checked = false;
        for seed in 0u64..1000 {
            let p = derive_profile(seed, &ov);
            if p.os == Os::Windows {
                assert!(feq(p.screen.device_pixel_ratio, 1.0));
                checked = true;
            } else if p.os == Os::MacOs {
                assert!(feq(p.screen.device_pixel_ratio, 2.0));
            }
        }
        assert!(checked);

        // Conversely, forcing 1.0 on a Mac head snaps back to 2.0.
        let ov2 = ProfileOverrides {
            device_pixel_ratio: Some(1.0),
            ..Default::default()
        };
        for seed in 0u64..1000 {
            let p = derive_profile(seed, &ov2);
            if p.os == Os::MacOs {
                assert!(feq(p.screen.device_pixel_ratio, 2.0));
            }
        }
    }

    #[test]
    fn device_memory_override_is_capped_and_quantized() {
        let ov = ProfileOverrides {
            device_memory: Some(Some(16)),
            ..Default::default()
        };
        for seed in 0u64..1000 {
            let p = derive_profile(seed, &ov);
            if p.browser.is_chromium() {
                assert_eq!(p.device_memory, Some(8), "16 GiB must cap to 8");
            }
        }
        let ov3 = ProfileOverrides {
            device_memory: Some(Some(3)),
            ..Default::default()
        };
        for seed in 0u64..1000 {
            let p = derive_profile(seed, &ov3);
            if p.browser.is_chromium() {
                assert_eq!(p.device_memory, Some(2), "3 must quantize down to 2");
            }
        }
    }

    #[test]
    fn locale_override_keeps_tz_and_lang_synced() {
        // Force the Berlin locale on every head; tz, offset and languages must all
        // agree, whatever the archetype.
        let berlin = LOCALE_POOL
            .iter()
            .position(|(tz, _, _)| *tz == "Europe/Berlin")
            .unwrap();
        let ov = ProfileOverrides {
            locale_index: Some(berlin),
            ..Default::default()
        };
        for seed in 0u64..500 {
            let p = derive_profile(seed, &ov);
            assert_eq!(p.timezone, "Europe/Berlin");
            assert_eq!(p.tz_offset_minutes, 60);
            assert_eq!(p.language, "de-DE");
            assert_eq!(p.languages, vec!["de-DE", "de", "en"]);
            assert!(p.is_coherent());
        }
    }

    #[test]
    fn language_override_resyncs_primary_language() {
        // A lone `language` override becomes languages[0]; the invariant holds.
        let ov = ProfileOverrides {
            language: Some("pt-BR".to_string()),
            ..Default::default()
        };
        let p = derive_profile(7, &ov);
        assert_eq!(p.language, "pt-BR");
        assert_eq!(p.languages[0], "pt-BR");
        assert!(p.is_coherent());
    }

    #[test]
    fn accept_language_has_descending_q_weights() {
        let ov = ProfileOverrides {
            locale_index: LOCALE_POOL
                .iter()
                .position(|(tz, _, _)| *tz == "Europe/Berlin"),
            ..Default::default()
        };
        let p = derive_profile(3, &ov);
        assert_eq!(p.accept_language(), "de-DE,de;q=0.9,en;q=0.8");
    }

    #[test]
    fn user_agent_accessor_matches_field_and_is_plausible() {
        for seed in SAMPLE_SEEDS {
            let p = derive_profile(seed, &ProfileOverrides::default());
            assert_eq!(p.user_agent(), p.user_agent.as_str());
            assert!(p.user_agent.starts_with("Mozilla/5.0"));
            match p.browser {
                Browser::Chrome => {
                    assert!(p.user_agent.contains("Chrome/"));
                    assert!(!p.user_agent.contains("Edg/"));
                    assert!(!p.user_agent.contains("Firefox/"));
                }
                Browser::Edge => assert!(p.user_agent.contains("Edg/")),
                Browser::Firefox => {
                    assert!(p.user_agent.contains("Firefox/"));
                    assert!(p.user_agent.contains("Gecko"));
                }
            }
            // Chromium UA carries the reduced product token.
            if p.browser.is_chromium() {
                assert!(p
                    .user_agent
                    .contains(&format!("Chrome/{}.0.0.0", p.browser_major)));
            }
        }
    }

    #[test]
    fn every_archetype_is_self_coherent() {
        // Independent of sampling: each raw archetype's pinned fields are legal.
        for a in ARCHETYPES {
            match a.os {
                Os::Windows => assert!(a.gpu_unmasked_renderer.contains("Direct3D11")),
                Os::MacOs => assert!(a.gpu_unmasked_renderer.contains("Metal")),
                Os::Linux => {}
            }
            // Chromium ⟺ UA-CH + deviceMemory.
            assert_eq!(a.has_ua_data, a.browser.is_chromium());
            assert_eq!(a.has_device_memory, a.browser.is_chromium());
            assert!(!a.resolutions.is_empty());
            assert!(!a.cores.is_empty());
            assert!(!a.memory.is_empty());
            assert!(!a.os_versions.is_empty());
            // Apple/Retina resolutions carry dpr 2.0; everything else 1.0.
            for &(_, _, dpr) in a.resolutions {
                match a.os {
                    Os::MacOs => assert!(feq(dpr, 2.0)),
                    _ => assert!(feq(dpr, 1.0)),
                }
            }
        }
    }

    /// A `"` is unescaped when not immediately preceded by an odd run of `\`.
    fn unescaped_quotes_balanced(s: &str) -> bool {
        let mut count = 0u32;
        let mut prev_backslash = false;
        for c in s.chars() {
            if c == '"' && !prev_backslash {
                count += 1;
            }
            prev_backslash = c == '\\' && !prev_backslash;
        }
        count.is_multiple_of(2)
    }
}

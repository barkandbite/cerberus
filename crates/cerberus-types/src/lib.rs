//! Shared, dependency-free domain types used across the Cerberus workspace.
//!
//! This crate depends only on `std` and holds no policy or subsystem behavior —
//! just small value types: identifiers, geometry, color, and web origins.
//! Keeping it tiny means every other crate can depend on it freely.

use std::fmt;

/// A 128-bit opaque identifier, rendered as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id128([u8; 16]);

impl Id128 {
    /// Construct from raw bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Construct from two `u64` halves. Convenient for deterministic tests.
    pub fn from_u64_pair(hi: u64, lo: u64) -> Self {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&hi.to_be_bytes());
        bytes[8..].copy_from_slice(&lo.to_be_bytes());
        Self(bytes)
    }

    /// Borrow the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Parse the 32-char lowercase-hex form produced by `Display`.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            bytes[i] = ((hi << 4) | lo) as u8;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id128({self})")
    }
}

/// Defines a distinct, non-interchangeable id newtype so that, e.g., an
/// `InstanceId` can never be passed where a `HeadId` is expected.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(pub Id128);

        impl $name {
            /// Construct from two `u64` halves. Convenient for deterministic tests.
            pub fn from_u64_pair(hi: u64, lo: u64) -> Self {
                Self(Id128::from_u64_pair(hi, lo))
            }

            /// Parse the 32-char hex form produced by `Display`.
            pub fn from_hex(s: &str) -> Option<Self> {
                Id128::from_hex(s).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

id_newtype!(
    /// Identifies a sealed storage instance (a cookie partition). Cookies are
    /// hard-partitioned by `InstanceId`; see `cerberus-storage`.
    InstanceId
);
id_newtype!(
    /// Identifies an identity ("head"). Each head owns one `InstanceId` and one
    /// farbling seed.
    HeadId
);
id_newtype!(
    /// Identifies a tab (a realm within a head).
    TabId
);
id_newtype!(
    /// Identifies a JS realm/context.
    RealmId
);

/// Integer pixel dimensions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    /// Construct a new size.
    pub const fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }

    /// Total pixel count (`w * h`).
    pub const fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// An integer point in device pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    /// The origin, `(0, 0)`.
    pub const ZERO: Point = Point { x: 0, y: 0 };

    /// Construct a new point.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle in device pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Construct a new rectangle.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
}

/// How an image fills its box (`object-fit` / `background-size`) — ADR-0044.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ImageFit {
    /// Stretch to fill the box (the `object-fit: fill` default, and
    /// `background-size: 100% 100%`).
    #[default]
    Fill,
    /// Scale to cover the box, cropping overflow, preserving aspect ratio.
    Cover,
    /// Scale to fit inside the box, letterboxing, preserving aspect ratio.
    Contain,
    /// Draw at the image's natural pixel size, no scaling (`background-size: auto`,
    /// the CSS initial value). This is what CSS sprites rely on: the intrinsic
    /// bitmap is placed by `background-position` and clipped to the box.
    Auto,
}

/// Where a scaled image sits in its box (`object-position` /
/// `background-position`) — ADR-0045. Each axis is a fraction: `0.0` =
/// left/top, `0.5` = center, `1.0` = right/bottom. Only meaningful with
/// `ImageFit::Cover`/`Contain` (a `Fill` image already fills the box exactly).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ImagePos {
    pub x: f32,
    pub y: f32,
}

impl ImagePos {
    /// `50% 50%` — the `object-position` initial value.
    pub const CENTER: ImagePos = ImagePos { x: 0.5, y: 0.5 };
    /// `0% 0%` — the `background-position` initial value.
    pub const TOP_LEFT: ImagePos = ImagePos { x: 0.0, y: 0.0 };
}

/// A straight RGBA color, 8 bits per channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Opaque color from RGB.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Color from RGBA.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Opaque white.
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    /// Opaque black.
    pub const BLACK: Color = Color::rgb(0, 0, 0);
}

/// Font style for a run of text. Bold renders today (faux-bold); italic is
/// tracked for the cascade and a future bold/italic font swap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
    /// Render from the bundled icon font rather than the text font.
    pub icon: bool,
}

impl FontStyle {
    /// Regular (non-bold, non-italic).
    pub const REGULAR: FontStyle = FontStyle {
        bold: false,
        italic: false,
        icon: false,
    };

    /// A glyph from the icon font (the rasterizer selects that font).
    pub const ICON: FontStyle = FontStyle {
        bold: false,
        italic: false,
        icon: true,
    };

    /// A bold style.
    pub const fn bold() -> Self {
        Self {
            bold: true,
            italic: false,
            icon: false,
        }
    }
}

/// The CSS generic font family a run of text resolves to, after mapping the
/// `font-family` list (named faces included) to one of the five generics. The
/// renderer bundles one metric-compatible face per generic (a serif ≈ Times, a
/// monospace ≈ Courier, a sans ≈ Arial/Roboto), so text presents the right shape
/// class without shipping — or fingerprinting against — the actual named fonts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum GenericFamily {
    /// Times-class serif: generic `serif`, named Times New Roman (and its
    /// metric aliases) → the bundled Liberation Serif. Also the reference
    /// browser's STANDARD font — the fallback for a wholly unresolvable
    /// `font-family` stack.
    Serif,
    /// Generic `sans-serif` → the Arial-metric bundled sans (the reference's
    /// generic sans requests Arial, substituted with Liberation Sans).
    #[default]
    SansSerif,
    /// Named Arial/Helvetica (and metric aliases) — same face as the generic
    /// sans on this persona, kept distinct for farbling and future personas.
    SansArial,
    /// The `system-ui` UI-font class → the bundled DejaVu Sans (what the
    /// reference resolves its system font to).
    SansSystem,
    /// Generic `monospace` (`<pre>`/`<code>` default) → the bundled
    /// DejaVu Sans Mono, the reference's fixed-font resolution.
    Monospace,
    /// Named Courier New (and metric aliases) → the bundled Liberation Mono —
    /// distinct from the generic monospace face.
    MonoCourier,
    /// Handwriting/script; the reference's cursive preference is uninstalled,
    /// so it falls back to the standard (serif) face.
    Cursive,
    /// Decorative/display; falls back to the standard (serif) face likewise.
    Fantasy,
}

/// A web origin (scheme, host, optional port) used for site-boundary checks.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl Origin {
    /// Construct a new origin.
    pub fn new(scheme: impl Into<String>, host: impl Into<String>, port: Option<u16>) -> Self {
        Self {
            scheme: scheme.into(),
            host: host.into(),
            port,
        }
    }

    /// A "site" key for first-party vs third-party comparisons: scheme plus
    /// the registrable domain (eTLD+1).
    ///
    /// Uses the Public Suffix List matcher installed by the composition root
    /// via [`install_registrable_domain`]; until one is installed (e.g. in
    /// leaf-crate unit tests) a conservative last-two-labels fallback applies.
    pub fn site(&self) -> String {
        format!("{}://{}", self.scheme, registrable_domain(&self.host))
    }

    /// True when `self` belongs to a different site than `other` (third-party).
    pub fn is_third_party_to(&self, other: &Origin) -> bool {
        self.site() != other.site()
    }
}

/// The installed PSL-backed registrable-domain function (set once at startup).
///
/// Lives here as a function pointer so `cerberus-types` (the dependency root,
/// which must stay data-free) can serve `Origin::site()` to *both* storage
/// partitioning and consent policy without a dependency cycle on the crate
/// that embeds the PSL snapshot (`cerberus-consent`).
static REGISTRABLE_DOMAIN: std::sync::OnceLock<fn(&str) -> String> = std::sync::OnceLock::new();

/// Install the real eTLD+1 implementation. Idempotent; first install wins.
/// Called by the composition root before any `Origin::site()` comparison.
pub fn install_registrable_domain(f: fn(&str) -> String) {
    let _ = REGISTRABLE_DOMAIN.set(f);
}

/// Registrable-domain extraction: the installed PSL matcher, or a conservative
/// last-two-labels fallback when none is installed.
pub fn registrable_domain(host: &str) -> String {
    if let Some(f) = REGISTRABLE_DOMAIN.get() {
        return f(host);
    }
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    let n = labels.len();
    if n >= 2 {
        format!("{}.{}", labels[n - 2], labels[n - 1])
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_render_as_hex_and_are_distinct_types() {
        let a = InstanceId::from_u64_pair(0, 1);
        assert_eq!(a.to_string(), "00000000000000000000000000000001");
    }

    #[test]
    fn third_party_detection_uses_registrable_domain() {
        let fp = Origin::new("https", "shop.example.com", None);
        let same = Origin::new("https", "cdn.example.com", None);
        let other = Origin::new("https", "tracker.net", None);
        assert!(!same.is_third_party_to(&fp));
        assert!(other.is_third_party_to(&fp));
    }
}

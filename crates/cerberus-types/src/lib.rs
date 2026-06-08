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

    /// A coarse "site" key for first-party vs third-party comparisons.
    ///
    /// NOTE: placeholder. Real registrable-domain (eTLD+1) handling via the
    /// Public Suffix List arrives with the consent engine (M5).
    pub fn site(&self) -> String {
        format!("{}://{}", self.scheme, registrable_domain(&self.host))
    }

    /// True when `self` belongs to a different site than `other` (third-party).
    pub fn is_third_party_to(&self, other: &Origin) -> bool {
        self.site() != other.site()
    }
}

/// Placeholder registrable-domain extraction: the last two dot-labels.
fn registrable_domain(host: &str) -> String {
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

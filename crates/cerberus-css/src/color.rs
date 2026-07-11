//! CSS color parsing: `#hex` (3/4/6/8-digit), `rgb()/rgba()`, `hsl()/hsla()`,
//! `oklch()`/`oklab()`, and named colors.

use cerberus_types::Color;

/// Parse a CSS color value. Returns a `Color`; `a == 0` means transparent.
pub fn parse_color(input: &str) -> Option<Color> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = s.to_ascii_lowercase();
    // `rgb()` and `rgba()` are synonyms (CSS Color 4); both accept an optional
    // alpha in either legacy or modern syntax.
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|x| x.strip_suffix(')')) {
        return parse_rgb(inner);
    }
    if let Some(inner) = lower
        .strip_prefix("rgba(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return parse_rgb(inner);
    }
    if let Some(inner) = lower.strip_prefix("hsl(").and_then(|x| x.strip_suffix(')')) {
        return parse_hsl(inner);
    }
    if let Some(inner) = lower
        .strip_prefix("hsla(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return parse_hsl(inner);
    }
    // Modern-toolchain default (Tailwind v4-era design systems emit whole
    // palettes in OKLCH); without these an entire site's colors silently drop
    // to UA defaults — measured as the iana grey-band regression.
    if let Some(inner) = lower
        .strip_prefix("oklch(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return parse_oklch(inner);
    }
    if let Some(inner) = lower
        .strip_prefix("oklab(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return parse_oklab(inner);
    }
    named(&lower)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let hex = hex.trim();
    match hex.len() {
        // `#rgb` and `#rgba`: each nibble is doubled (`f` → `ff`).
        3 | 4 => {
            let r = dup(hex.get(0..1)?)?;
            let g = dup(hex.get(1..2)?)?;
            let b = dup(hex.get(2..3)?)?;
            let a = match hex.get(3..4) {
                Some(n) => dup(n)?,
                None => 255,
            };
            Some(Color::rgba(r, g, b, a))
        }
        // `#rrggbb` and `#rrggbbaa` (the trailing pair is alpha).
        6 | 8 => {
            let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
            let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
            let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
            let a = match hex.get(6..8) {
                Some(p) => u8::from_str_radix(p, 16).ok()?,
                None => 255,
            };
            Some(Color::rgba(r, g, b, a))
        }
        _ => None,
    }
}

fn dup(nibble: &str) -> Option<u8> {
    let v = u8::from_str_radix(nibble, 16).ok()?;
    Some(v * 16 + v)
}

/// `hsl()`/`hsla()` in either syntax (they are synonyms, CSS Color 4):
/// - legacy comma: `h, s%, l%` / `h, s%, l%, a`
/// - modern space: `h s% l%`   / `h s% l% / a`
///
/// Hue is in degrees (an optional `deg` unit, wrapped mod 360), saturation and
/// lightness are percentages, alpha is a 0..1 number or a percentage. Modern
/// sites lean on HSL heavily; without this every such color silently dropped to
/// the inherited/initial value.
fn parse_hsl(inner: &str) -> Option<Color> {
    let (parts, alpha) = if inner.contains(',') {
        let p: Vec<&str> = inner.split(',').map(str::trim).collect();
        if p.len() < 3 {
            return None;
        }
        ([p[0], p[1], p[2]], p.get(3).copied())
    } else {
        let (hsl, alpha) = match inner.split_once('/') {
            Some((h, a)) => (h, Some(a)),
            None => (inner, None),
        };
        let n: Vec<&str> = hsl.split_whitespace().collect();
        if n.len() < 3 {
            return None;
        }
        ([n[0], n[1], n[2]], alpha)
    };
    let h = parts[0]
        .trim()
        .trim_end_matches("deg")
        .trim()
        .parse::<f32>()
        .ok()?;
    let s = parts[1]
        .trim()
        .strip_suffix('%')?
        .trim()
        .parse::<f32>()
        .ok()?
        / 100.0;
    let l = parts[2]
        .trim()
        .strip_suffix('%')?
        .trim()
        .parse::<f32>()
        .ok()?
        / 100.0;
    let a = match alpha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => parse_alpha(a)?,
        None => 255,
    };
    let (r, g, b) = hsl_to_rgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
    Some(Color::rgba(r, g, b, a))
}

/// Convert HSL (hue degrees, sat 0..1, light 0..1) to 8-bit RGB.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0) / 360.0;
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let c = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (c * 255.0).round() as u8
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// `oklch(L C H [/ A])` (CSS Color 4, modern space syntax only). `L` is a
/// 0..1 number or percentage; `C` a non-negative number or a percentage of 0.4;
/// `H` degrees (optional `deg`); each may be the keyword `none` (= 0).
fn parse_oklch(inner: &str) -> Option<Color> {
    let (parts, alpha) = split_modern3(inner)?;
    let l = num_or_pct(parts[0], 1.0)?;
    let c = num_or_pct(parts[1], 0.4)?.max(0.0);
    let h = parts[2]
        .trim_end_matches("deg")
        .trim()
        .parse::<f32>()
        .ok()
        .or_else(|| (parts[2] == "none").then_some(0.0))?;
    let a = match alpha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => parse_alpha(a)?,
        None => 255,
    };
    let hr = h.to_radians();
    let (r, g, b) = oklab_to_srgb(l, c * hr.cos(), c * hr.sin());
    Some(Color::rgba(r, g, b, a))
}

/// `oklab(L a b [/ A])`: `L` as in `oklch`; `a`/`b` are signed numbers or
/// percentages of ±0.4.
fn parse_oklab(inner: &str) -> Option<Color> {
    let (parts, alpha) = split_modern3(inner)?;
    let l = num_or_pct(parts[0], 1.0)?;
    let a_ax = num_or_pct(parts[1], 0.4)?;
    let b_ax = num_or_pct(parts[2], 0.4)?;
    let a = match alpha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => parse_alpha(a)?,
        None => 255,
    };
    let (r, g, b) = oklab_to_srgb(l, a_ax, b_ax);
    Some(Color::rgba(r, g, b, a))
}

/// Split a modern space-syntax function body into exactly three components and
/// an optional `/ alpha` tail.
fn split_modern3(inner: &str) -> Option<([&str; 3], Option<&str>)> {
    let (body, alpha) = match inner.split_once('/') {
        Some((b, a)) => (b, Some(a)),
        None => (inner, None),
    };
    let n: Vec<&str> = body.split_whitespace().collect();
    if n.len() != 3 {
        return None;
    }
    Some(([n[0], n[1], n[2]], alpha))
}

/// A signed number, a percentage of `scale` (100% ⇒ `scale`), or `none` (0).
fn num_or_pct(s: &str, scale: f32) -> Option<f32> {
    let s = s.trim();
    if s == "none" {
        return Some(0.0);
    }
    if let Some(p) = s.strip_suffix('%') {
        return Some(p.trim().parse::<f32>().ok()? / 100.0 * scale);
    }
    s.parse::<f32>().ok()
}

/// OKLab → sRGB (Björn Ottosson's reference matrices), gamut-clipped per
/// channel — matching how Chromium renders out-of-gamut OKLCH in an sRGB
/// context closely enough for the parity tolerance.
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> (u8, u8, u8) {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;
    (srgb_encode(r), srgb_encode(g), srgb_encode(b))
}

/// Linear-light component → gamma-encoded 8-bit sRGB (clamped).
fn srgb_encode(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let e = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (e * 255.0).round() as u8
}

/// Parse the inside of `rgb(...)`/`rgba(...)` in either syntax:
/// - legacy comma:  `r, g, b` / `r, g, b, a`
/// - modern space:  `r g b`   / `r g b / a`   (CSS Color 4)
///
/// Channels are 0–255 integers or percentages; alpha is a 0–1 number or a
/// percentage. The two forms are told apart by the presence of a comma.
fn parse_rgb(inner: &str) -> Option<Color> {
    let (rgb, alpha) = if inner.contains(',') {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            return None;
        }
        ([parts[0], parts[1], parts[2]], parts.get(3).copied())
    } else {
        // Modern syntax: optional `/ alpha` after the three channels.
        let (chans, alpha) = match inner.split_once('/') {
            Some((c, a)) => (c, Some(a)),
            None => (inner, None),
        };
        let nums: Vec<&str> = chans.split_whitespace().collect();
        if nums.len() < 3 {
            return None;
        }
        ([nums[0], nums[1], nums[2]], alpha)
    };
    let r = channel(rgb[0].trim())?;
    let g = channel(rgb[1].trim())?;
    let b = channel(rgb[2].trim())?;
    let a = match alpha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => parse_alpha(a)?,
        None => 255,
    };
    Some(Color::rgba(r, g, b, a))
}

/// Parse an alpha value: a `0..=1` number or a percentage. Clamped to `[0, 1]`
/// and scaled to a 0–255 byte.
fn parse_alpha(s: &str) -> Option<u8> {
    let frac = if let Some(p) = s.strip_suffix('%') {
        p.trim().parse::<f32>().ok()? / 100.0
    } else {
        s.parse::<f32>().ok()?
    };
    Some((frac.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn channel(s: &str) -> Option<u8> {
    if let Some(pct) = s.strip_suffix('%') {
        let v: f32 = pct.trim().parse().ok()?;
        Some((v.clamp(0.0, 100.0) / 100.0 * 255.0).round() as u8)
    } else {
        let v: i32 = s.parse().ok()?;
        Some(v.clamp(0, 255) as u8)
    }
}

/// A practical subset of the CSS named colors.
fn named(name: &str) -> Option<Color> {
    let rgb = match name {
        "transparent" => return Some(Color::rgba(0, 0, 0, 0)),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "lime" => (0, 255, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "silver" => (192, 192, 192),
        "gray" | "grey" => (128, 128, 128),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "navy" => (0, 0, 128),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        "gold" => (255, 215, 0),
        "indigo" => (75, 0, 130),
        "violet" => (238, 130, 238),
        "crimson" => (220, 20, 60),
        "tomato" => (255, 99, 71),
        "coral" => (255, 127, 80),
        "salmon" => (250, 128, 114),
        "khaki" => (240, 230, 140),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "dimgray" | "dimgrey" => (105, 105, 105),
        "slategray" | "slategrey" => (112, 128, 144),
        "gainsboro" => (220, 220, 220),
        "whitesmoke" => (245, 245, 245),
        "lightblue" => (173, 216, 230),
        "skyblue" => (135, 206, 235),
        "steelblue" => (70, 130, 180),
        "dodgerblue" => (30, 144, 255),
        "royalblue" => (65, 105, 225),
        "darkblue" => (0, 0, 139),
        "darkgreen" => (0, 100, 0),
        "darkred" => (139, 0, 0),
        "rebeccapurple" => (102, 51, 153),
        "beige" => (245, 245, 220),
        "ivory" => (255, 255, 240),
        // The remainder of the CSS named-color set. Values are written as the
        // spec's hex triplets (`0xRR, 0xGG, 0xBB`) rather than decimal so each
        // maps 1:1 to the color table and can't drift through a hand conversion.
        // Without these, a page using e.g. `color: lightgreen` parses to `None`
        // and the declaration is silently dropped — rendering the default color.
        "aliceblue" => (0xf0, 0xf8, 0xff),
        "antiquewhite" => (0xfa, 0xeb, 0xd7),
        "aquamarine" => (0x7f, 0xff, 0xd4),
        "azure" => (0xf0, 0xff, 0xff),
        "bisque" => (0xff, 0xe4, 0xc4),
        "blanchedalmond" => (0xff, 0xeb, 0xcd),
        "blueviolet" => (0x8a, 0x2b, 0xe2),
        "burlywood" => (0xde, 0xb8, 0x87),
        "cadetblue" => (0x5f, 0x9e, 0xa0),
        "chartreuse" => (0x7f, 0xff, 0x00),
        "chocolate" => (0xd2, 0x69, 0x1e),
        "cornflowerblue" => (0x64, 0x95, 0xed),
        "cornsilk" => (0xff, 0xf8, 0xdc),
        "darkcyan" => (0x00, 0x8b, 0x8b),
        "darkgoldenrod" => (0xb8, 0x86, 0x0b),
        "darkkhaki" => (0xbd, 0xb7, 0x6b),
        "darkmagenta" => (0x8b, 0x00, 0x8b),
        "darkolivegreen" => (0x55, 0x6b, 0x2f),
        "darkorange" => (0xff, 0x8c, 0x00),
        "darkorchid" => (0x99, 0x32, 0xcc),
        "darksalmon" => (0xe9, 0x96, 0x7a),
        "darkseagreen" => (0x8f, 0xbc, 0x8f),
        "darkslateblue" => (0x48, 0x3d, 0x8b),
        "darkslategray" | "darkslategrey" => (0x2f, 0x4f, 0x4f),
        "darkturquoise" => (0x00, 0xce, 0xd1),
        "darkviolet" => (0x94, 0x00, 0xd3),
        "deeppink" => (0xff, 0x14, 0x93),
        "deepskyblue" => (0x00, 0xbf, 0xff),
        "firebrick" => (0xb2, 0x22, 0x22),
        "floralwhite" => (0xff, 0xfa, 0xf0),
        "forestgreen" => (0x22, 0x8b, 0x22),
        "ghostwhite" => (0xf8, 0xf8, 0xff),
        "goldenrod" => (0xda, 0xa5, 0x20),
        "greenyellow" => (0xad, 0xff, 0x2f),
        "honeydew" => (0xf0, 0xff, 0xf0),
        "hotpink" => (0xff, 0x69, 0xb4),
        "indianred" => (0xcd, 0x5c, 0x5c),
        "lavender" => (0xe6, 0xe6, 0xfa),
        "lavenderblush" => (0xff, 0xf0, 0xf5),
        "lawngreen" => (0x7c, 0xfc, 0x00),
        "lemonchiffon" => (0xff, 0xfa, 0xcd),
        "lightcoral" => (0xf0, 0x80, 0x80),
        "lightcyan" => (0xe0, 0xff, 0xff),
        "lightgoldenrodyellow" => (0xfa, 0xfa, 0xd2),
        "lightgreen" => (0x90, 0xee, 0x90),
        "lightpink" => (0xff, 0xb6, 0xc1),
        "lightsalmon" => (0xff, 0xa0, 0x7a),
        "lightseagreen" => (0x20, 0xb2, 0xaa),
        "lightskyblue" => (0x87, 0xce, 0xfa),
        "lightslategray" | "lightslategrey" => (0x77, 0x88, 0x99),
        "lightsteelblue" => (0xb0, 0xc4, 0xde),
        "lightyellow" => (0xff, 0xff, 0xe0),
        "limegreen" => (0x32, 0xcd, 0x32),
        "linen" => (0xfa, 0xf0, 0xe6),
        "mediumaquamarine" => (0x66, 0xcd, 0xaa),
        "mediumblue" => (0x00, 0x00, 0xcd),
        "mediumorchid" => (0xba, 0x55, 0xd3),
        "mediumpurple" => (0x93, 0x70, 0xdb),
        "mediumseagreen" => (0x3c, 0xb3, 0x71),
        "mediumslateblue" => (0x7b, 0x68, 0xee),
        "mediumspringgreen" => (0x00, 0xfa, 0x9a),
        "mediumturquoise" => (0x48, 0xd1, 0xcc),
        "mediumvioletred" => (0xc7, 0x15, 0x85),
        "midnightblue" => (0x19, 0x19, 0x70),
        "mintcream" => (0xf5, 0xff, 0xfa),
        "mistyrose" => (0xff, 0xe4, 0xe1),
        "moccasin" => (0xff, 0xe4, 0xb5),
        "navajowhite" => (0xff, 0xde, 0xad),
        "oldlace" => (0xfd, 0xf5, 0xe6),
        "olivedrab" => (0x6b, 0x8e, 0x23),
        "orangered" => (0xff, 0x45, 0x00),
        "orchid" => (0xda, 0x70, 0xd6),
        "palegoldenrod" => (0xee, 0xe8, 0xaa),
        "palegreen" => (0x98, 0xfb, 0x98),
        "paleturquoise" => (0xaf, 0xee, 0xee),
        "palevioletred" => (0xdb, 0x70, 0x93),
        "papayawhip" => (0xff, 0xef, 0xd5),
        "peachpuff" => (0xff, 0xda, 0xb9),
        "peru" => (0xcd, 0x85, 0x3f),
        "plum" => (0xdd, 0xa0, 0xdd),
        "powderblue" => (0xb0, 0xe0, 0xe6),
        "rosybrown" => (0xbc, 0x8f, 0x8f),
        "saddlebrown" => (0x8b, 0x45, 0x13),
        "sandybrown" => (0xf4, 0xa4, 0x60),
        "seagreen" => (0x2e, 0x8b, 0x57),
        "seashell" => (0xff, 0xf5, 0xee),
        "sienna" => (0xa0, 0x52, 0x2d),
        "slateblue" => (0x6a, 0x5a, 0xcd),
        "snow" => (0xff, 0xfa, 0xfa),
        "springgreen" => (0x00, 0xff, 0x7f),
        "tan" => (0xd2, 0xb4, 0x8c),
        "thistle" => (0xd8, 0xbf, 0xd8),
        "turquoise" => (0x40, 0xe0, 0xd0),
        "wheat" => (0xf5, 0xde, 0xb3),
        "yellowgreen" => (0x9a, 0xcd, 0x32),
        _ => return None,
    };
    Some(Color::rgb(rgb.0, rgb.1, rgb.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_rgb_named() {
        assert_eq!(parse_color("#fff"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(parse_color("#1e90ff"), Some(Color::rgb(30, 144, 255)));
        assert_eq!(parse_color("rgb(10, 20, 30)"), Some(Color::rgb(10, 20, 30)));
        assert_eq!(parse_color("RoyalBlue"), Some(Color::rgb(65, 105, 225)));
        assert_eq!(parse_color("transparent").unwrap().a, 0);
        assert_eq!(parse_color("not-a-color"), None);
    }

    #[test]
    fn parses_modern_and_legacy_rgb_syntax() {
        // Legacy comma and modern space forms are equivalent; rgb()/rgba() are
        // synonyms and both accept an optional alpha.
        assert_eq!(
            parse_color("rgb(255 0 0)"),
            Some(Color::rgba(255, 0, 0, 255))
        );
        assert_eq!(
            parse_color("rgb(10, 20, 30)"),
            parse_color("rgb(10 20 30)"),
            "comma and space forms agree"
        );
        // Modern slash alpha, as a number and as a percentage.
        assert_eq!(
            parse_color("rgb(0 128 255 / 0.5)"),
            Some(Color::rgba(0, 128, 255, 128))
        );
        assert_eq!(
            parse_color("rgb(0 0 0 / 50%)").unwrap().a,
            128,
            "percent alpha"
        );
        // `rgb(...)` (not just rgba) honors alpha; legacy 4-value form still works.
        assert_eq!(parse_color("rgba(255, 0, 0, 0)").unwrap().a, 0);
        assert_eq!(parse_color("rgb(1 2 3 / 100%)").unwrap().a, 255);
        // Percentage channels resolve in both forms.
        assert_eq!(
            parse_color("rgb(100% 0% 0%)"),
            Some(Color::rgba(255, 0, 0, 255))
        );
        // Too few channels → no color.
        assert_eq!(parse_color("rgb(1 2)"), None);
    }

    #[test]
    fn parses_extended_named_colors() {
        // Colors from the completed CSS named-color set that previously parsed
        // to `None` (and so rendered nothing). Case-insensitive, and the
        // gray/grey spelling variants both resolve.
        assert_eq!(
            parse_color("lightgreen"),
            Some(Color::rgb(0x90, 0xee, 0x90))
        );
        assert_eq!(parse_color("OrangeRed"), Some(Color::rgb(0xff, 0x45, 0x00)));
        assert_eq!(parse_color("turquoise"), Some(Color::rgb(0x40, 0xe0, 0xd0)));
        assert_eq!(parse_color("rebeccapurple"), Some(Color::rgb(102, 51, 153)));
        assert_eq!(
            parse_color("darkslategrey"),
            parse_color("darkslategray"),
            "gray/grey aliases resolve identically"
        );
        assert_eq!(
            parse_color("lightslategray"),
            Some(Color::rgb(0x77, 0x88, 0x99))
        );
    }

    #[test]
    fn parses_hex_with_alpha() {
        // 4-digit `#rgba`: each nibble doubled, last is alpha.
        assert_eq!(parse_color("#f008"), Some(Color::rgba(255, 0, 0, 0x88)));
        // 8-digit `#rrggbbaa`: trailing pair is alpha.
        assert_eq!(
            parse_color("#1e90ff80"),
            Some(Color::rgba(30, 144, 255, 0x80))
        );
        // Opaque forms are unchanged (a == 255).
        assert_eq!(parse_color("#00ff00ff"), Some(Color::rgba(0, 255, 0, 255)));
        // 5/7-digit hex is invalid.
        assert_eq!(parse_color("#12345"), None);
    }

    #[test]
    fn parses_hsl_and_hsla() {
        // Primary hues round-trip to their RGB equivalents.
        assert_eq!(
            parse_color("hsl(0, 100%, 50%)"),
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(
            parse_color("hsl(120, 100%, 50%)"),
            Some(Color::rgb(0, 255, 0))
        );
        assert_eq!(
            parse_color("hsl(240, 100%, 50%)"),
            Some(Color::rgb(0, 0, 255))
        );
        // 0% saturation is a pure grey at the given lightness.
        assert_eq!(
            parse_color("hsl(0, 0%, 50%)"),
            Some(Color::rgb(128, 128, 128))
        );
        // Alpha and a `deg` hue unit, with hue wrapping past 360.
        assert_eq!(
            parse_color("hsla(360deg, 100%, 50%, 0.5)"),
            Some(Color::rgba(255, 0, 0, 128))
        );
        // Malformed (missing % units) is rejected, not mis-parsed.
        assert_eq!(parse_color("hsl(120, 100, 50)"), None);
    }

    /// Channel-wise closeness for the float conversions (rounding drift ≤ 2).
    fn close(got: Option<Color>, want: (u8, u8, u8, u8)) -> bool {
        let Some(c) = got else { return false };
        (c.r as i32 - want.0 as i32).abs() <= 2
            && (c.g as i32 - want.1 as i32).abs() <= 2
            && (c.b as i32 - want.2 as i32).abs() <= 2
            && (c.a as i32 - want.3 as i32).abs() <= 2
    }

    #[test]
    fn parses_oklch_and_oklab() {
        // Ottosson's reference: sRGB red = oklab(0.62796, 0.22486, 0.12585),
        // i.e. oklch(0.62796 0.25768 29.234).
        assert!(close(
            parse_color("oklch(0.62796 0.25768 29.234)"),
            (255, 0, 0, 255)
        ));
        assert!(close(
            parse_color("oklab(0.62796 0.22486 0.12585)"),
            (255, 0, 0, 255)
        ));
        // Extremes and greys (chroma 0 ⇒ pure grey ramp).
        assert!(close(parse_color("oklch(1 0 0)"), (255, 255, 255, 255)));
        assert!(close(parse_color("oklch(0 0 0)"), (0, 0, 0, 255)));
        // Percentage L and C, `deg` hue, slash alpha.
        assert!(close(
            parse_color("oklch(62.796% 64.42% 29.234deg / 0.5)"),
            (255, 0, 0, 128)
        ));
        // `none` components are zero.
        assert!(close(parse_color("oklch(none 0 none)"), (0, 0, 0, 255)));
        // Wrong arity is rejected, not guessed.
        assert_eq!(parse_color("oklch(0.5 0.1)"), None);
    }

    #[test]
    fn parses_modern_hsl_syntax() {
        // Modern space form matches the legacy comma form.
        assert_eq!(
            parse_color("hsl(120 100% 50%)"),
            parse_color("hsl(120, 100%, 50%)")
        );
        // Slash alpha as a number and as a percentage; `deg` unit still accepted.
        assert_eq!(
            parse_color("hsl(0deg 100% 50% / 0.5)"),
            Some(Color::rgba(255, 0, 0, 128))
        );
        assert_eq!(parse_color("hsl(240 100% 50% / 50%)").unwrap().a, 128);
        // Too few components → no color.
        assert_eq!(parse_color("hsl(120 100%)"), None);
    }
}

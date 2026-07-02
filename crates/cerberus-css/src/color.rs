//! CSS color parsing: `#hex` (3/4/6/8-digit), `rgb()/rgba()`, `hsl()/hsla()`,
//! and named colors.

use cerberus_types::Color;

/// Parse a CSS color value. Returns a `Color`; `a == 0` means transparent.
pub fn parse_color(input: &str) -> Option<Color> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|x| x.strip_suffix(')')) {
        return parse_rgb(inner, false);
    }
    if let Some(inner) = lower
        .strip_prefix("rgba(")
        .and_then(|x| x.strip_suffix(')'))
    {
        return parse_rgb(inner, true);
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

/// `hsl(h, s%, l%)` / `hsla(h, s%, l%, a)` (comma syntax). Hue is in degrees
/// (wrapped mod 360), saturation and lightness are percentages, alpha is 0..1.
/// Modern sites lean on HSL heavily; without this every such color silently
/// dropped to the inherited/initial value.
fn parse_hsl(inner: &str) -> Option<Color> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let h = parts[0]
        .trim_end_matches("deg")
        .trim()
        .parse::<f32>()
        .ok()?;
    let s = parts[1].strip_suffix('%')?.trim().parse::<f32>().ok()? / 100.0;
    let l = parts[2].strip_suffix('%')?.trim().parse::<f32>().ok()? / 100.0;
    let a = if parts.len() >= 4 {
        (parts[3].parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        255
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

fn parse_rgb(inner: &str, with_alpha: bool) -> Option<Color> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let r = channel(parts[0])?;
    let g = channel(parts[1])?;
    let b = channel(parts[2])?;
    let a = if with_alpha && parts.len() >= 4 {
        let alpha: f32 = parts[3].parse().ok()?;
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        255
    };
    Some(Color::rgba(r, g, b, a))
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
}

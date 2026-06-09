//! CSS color parsing: `#hex`, `rgb()/rgba()`, and named colors.

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
    named(&lower)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let hex = hex.trim();
    match hex.len() {
        3 => {
            let r = dup(hex.get(0..1)?)?;
            let g = dup(hex.get(1..2)?)?;
            let b = dup(hex.get(2..3)?)?;
            Some(Color::rgb(r, g, b))
        }
        6 => {
            let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
            let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
            let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
            Some(Color::rgb(r, g, b))
        }
        _ => None,
    }
}

fn dup(nibble: &str) -> Option<u8> {
    let v = u8::from_str_radix(nibble, 16).ok()?;
    Some(v * 16 + v)
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
}

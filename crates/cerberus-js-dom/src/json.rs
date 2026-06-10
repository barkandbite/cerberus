//! A deliberately tiny JSON layer — just enough for the DOM wire format.
//!
//! The bridge owns both ends of its wire protocol (a Rust emitter and a Rust
//! parser, mirrored by `JSON.stringify`/`JSON.parse` on the JS side), so we do
//! not pull in `serde`. We need exactly: objects, arrays, JSON strings (with
//! full escape handling, including `\uXXXX` surrogate pairs), and non-negative
//! integers. Booleans, `null`, and floating-point numbers are intentionally
//! *not* supported — the wire format never emits them, and rejecting them keeps
//! the parser small and its failure modes obvious.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Append `s` as a JSON string literal (including the surrounding quotes) to
/// `out`, escaping the characters JSON requires.
///
/// We emit the short escapes `\"`, `\\`, `\n`, `\r`, `\t` for those specific
/// characters, `\uXXXX` for any other C0 control character (`< 0x20`), and pass
/// every other `char` through verbatim as UTF-8. Forward slashes and non-ASCII
/// text (e.g. `café`, `日本語`) are emitted literally — valid JSON and far more
/// readable than `\u`-escaping everything.
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Other control chars: \u00XX (always two leading zeros here,
                // since c < 0x20 fits in one byte).
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append `n` as a bare JSON integer literal to `out`. The wire format's `id`s,
/// `root`, and `children` entries are all non-negative integers, so no escaping
/// or sign handling is needed.
pub fn write_u64(out: &mut String, n: u64) {
    let _ = write!(out, "{n}");
}

// ---------------------------------------------------------------------------
// Parser (recursive descent)
// ---------------------------------------------------------------------------

/// A parsed JSON value, restricted to the shapes the DOM wire format uses.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    /// A non-negative integer. The wire format only ever carries node ids and
    /// indices, all of which are non-negative; we store them as `u64`.
    Int(u64),
    /// A UTF-8 string with all escapes already resolved.
    Str(String),
    /// An array of values, in order.
    Array(Vec<Json>),
    /// An object, key/value pairs in source order.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Borrow this value as an object's field list, or `None` if it is not an
    /// object.
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(fields) => Some(fields),
            _ => None,
        }
    }

    /// Borrow this value as an array, or `None` if it is not one.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Borrow this value as a string, or `None` if it is not one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// This value as a `u64`, or `None` if it is not an integer.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Look up a field by key in an object (first match), `None` otherwise.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

/// Parse a complete JSON document from `input`, rejecting trailing junk.
///
/// On failure returns a short human-readable message (the kind a developer
/// wants when the wire format is wrong), never a panic.
pub fn parse(input: &str) -> Result<Json, String> {
    let mut p = Parser::new(input);
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing characters at byte {}", p.pos));
    }
    Ok(value)
}

/// A cursor over the input bytes. We operate on bytes (the JSON grammar is
/// ASCII apart from inside string literals, where we decode UTF-8 explicitly).
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    /// The byte at the cursor, if any.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Advance past JSON insignificant whitespace.
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Parse any value at the cursor (whitespace already skipped by the caller).
    fn parse_value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(Json::Str),
            Some(b) if b.is_ascii_digit() => self.parse_int(),
            Some(b) => Err(format!("unexpected byte {:?} at {}", b as char, self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    /// Parse `{ "k": v, ... }`. Keys must be strings; duplicate keys are kept
    /// (the consumer takes the first via [`Json::get`]).
    fn parse_object(&mut self) -> Result<Json, String> {
        self.pos += 1; // consume '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!("expected object key string at {}", self.pos));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("expected ':' after key at {}", self.pos));
            }
            self.pos += 1; // consume ':'
            self.skip_ws();
            let value = self.parse_value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(format!("expected ',' or '}}' in object at {}", self.pos)),
            }
        }
    }

    /// Parse `[ v, ... ]`.
    fn parse_array(&mut self) -> Result<Json, String> {
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' in array at {}", self.pos)),
            }
        }
    }

    /// Parse a non-negative integer (a run of ASCII digits). We reject a lone
    /// leading zero followed by more digits (`007`) the way JSON does, accept a
    /// single `0`, and surface overflow rather than wrapping.
    fn parse_int(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let digits = &self.bytes[start..self.pos];
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(format!("leading zero in integer at {start}"));
        }
        // Reject `.`/`e` floats and bare `-`: those are values the wire format
        // never uses, so anything other than a clean digit run is an error.
        if matches!(self.peek(), Some(b'.') | Some(b'e') | Some(b'E')) {
            return Err(format!(
                "floating-point numbers are not supported at {start}"
            ));
        }
        // SAFETY-free: digits are ASCII by construction.
        let s = std::str::from_utf8(digits).expect("ascii digits");
        s.parse::<u64>()
            .map(Json::Int)
            .map_err(|_| format!("integer out of range at {start}"))
    }

    /// Parse a JSON string literal at the cursor (which must be `"`), resolving
    /// all escapes including `\uXXXX` with surrogate-pair combining.
    fn parse_string(&mut self) -> Result<String, String> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        loop {
            let b = self
                .peek()
                .ok_or_else(|| "unterminated string".to_string())?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                // Raw control characters are not allowed unescaped in JSON.
                0x00..=0x1F => {
                    return Err(format!("unescaped control character at {}", self.pos));
                }
                _ => {
                    // A UTF-8 sequence: copy the whole code point through. We
                    // decode it so we advance by the right number of bytes and
                    // reject invalid UTF-8.
                    let ch = self.next_utf8_char()?;
                    out.push(ch);
                }
            }
        }
    }

    /// Decode and push a single escape sequence (the leading `\` already
    /// consumed). Handles the JSON short escapes plus `\uXXXX`, combining a
    /// high+low surrogate pair into one `char`.
    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        let b = self
            .peek()
            .ok_or_else(|| "unterminated escape".to_string())?;
        match b {
            b'"' => {
                out.push('"');
                self.pos += 1;
            }
            b'\\' => {
                out.push('\\');
                self.pos += 1;
            }
            b'/' => {
                out.push('/');
                self.pos += 1;
            }
            b'b' => {
                out.push('\u{0008}');
                self.pos += 1;
            }
            b'f' => {
                out.push('\u{000C}');
                self.pos += 1;
            }
            b'n' => {
                out.push('\n');
                self.pos += 1;
            }
            b'r' => {
                out.push('\r');
                self.pos += 1;
            }
            b't' => {
                out.push('\t');
                self.pos += 1;
            }
            b'u' => {
                self.pos += 1; // consume 'u'
                let unit = self.parse_hex4()?;
                if (0xD800..=0xDBFF).contains(&unit) {
                    // High surrogate: must be followed by `\uXXXX` low surrogate.
                    if self.peek() != Some(b'\\') {
                        return Err(format!("lone high surrogate at {}", self.pos));
                    }
                    self.pos += 1; // consume '\'
                    if self.peek() != Some(b'u') {
                        return Err(format!("lone high surrogate at {}", self.pos));
                    }
                    self.pos += 1; // consume 'u'
                    let low = self.parse_hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(format!("invalid low surrogate at {}", self.pos));
                    }
                    let combined =
                        0x10000 + (((unit - 0xD800) as u32) << 10) + (low - 0xDC00) as u32;
                    match char::from_u32(combined) {
                        Some(c) => out.push(c),
                        None => return Err(format!("invalid surrogate pair at {}", self.pos)),
                    }
                } else if (0xDC00..=0xDFFF).contains(&unit) {
                    return Err(format!("lone low surrogate at {}", self.pos));
                } else {
                    // BMP code point.
                    match char::from_u32(unit as u32) {
                        Some(c) => out.push(c),
                        None => return Err(format!("invalid \\u escape at {}", self.pos)),
                    }
                }
            }
            other => {
                return Err(format!(
                    "invalid escape '\\{}' at {}",
                    other as char, self.pos
                ));
            }
        }
        Ok(())
    }

    /// Read exactly four hex digits as a `u16` code unit.
    fn parse_hex4(&mut self) -> Result<u16, String> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self
                .peek()
                .ok_or_else(|| "truncated \\u escape".to_string())?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(format!("invalid hex digit in \\u escape at {}", self.pos)),
            };
            value = value * 16 + digit as u16;
            self.pos += 1;
        }
        Ok(value)
    }

    /// Decode the UTF-8 code point starting at the cursor, advancing past it.
    /// Errors on malformed UTF-8.
    fn next_utf8_char(&mut self) -> Result<char, String> {
        let rest = &self.bytes[self.pos..];
        // Determine the sequence length from the lead byte.
        let len = match rest[0] {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return Err(format!("invalid UTF-8 lead byte at {}", self.pos)),
        };
        if rest.len() < len {
            return Err(format!("truncated UTF-8 sequence at {}", self.pos));
        }
        let s = std::str::from_utf8(&rest[..len])
            .map_err(|_| format!("invalid UTF-8 at {}", self.pos))?;
        let ch = s.chars().next().ok_or("empty UTF-8 decode")?;
        self.pos += len;
        Ok(ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitter_escapes_the_required_characters() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\re\tf");
        assert_eq!(out, r#""a\"b\\c\nd\re\tf""#);
    }

    #[test]
    fn emitter_uses_u_escape_for_other_controls_and_passes_unicode() {
        let mut out = String::new();
        write_json_string(&mut out, "x\u{0001}y café 日本語");
        assert_eq!(out, "\"x\\u0001y café 日本語\"");
    }

    #[test]
    fn parses_object_array_string_int() {
        let v = parse(r#"{"root":3,"nodes":["a",12,[]]}"#).unwrap();
        assert_eq!(v.get("root").and_then(Json::as_u64), Some(3));
        let nodes = v.get("nodes").and_then(Json::as_array).unwrap();
        assert_eq!(nodes[0].as_str(), Some("a"));
        assert_eq!(nodes[1].as_u64(), Some(12));
        assert!(nodes[2].as_array().unwrap().is_empty());
    }

    #[test]
    fn round_trips_string_escapes() {
        // Emit then parse: the decoded string must equal the original.
        let original = "quote \" backslash \\ newline \n tab \t ctrl \u{0007} café 日本語";
        let mut emitted = String::new();
        write_json_string(&mut emitted, original);
        let parsed = parse(&emitted).unwrap();
        assert_eq!(parsed.as_str(), Some(original));
    }

    #[test]
    fn decodes_surrogate_pair_escape() {
        // U+1F600 GRINNING FACE encoded as a UTF-16 surrogate pair.
        let v = parse(r#""😀""#).unwrap();
        assert_eq!(v.as_str(), Some("\u{1F600}"));
    }

    #[test]
    fn decodes_bmp_u_escape() {
        let v = parse(r#""é""#).unwrap(); // é
        assert_eq!(v.as_str(), Some("é"));
    }

    #[test]
    fn rejects_lone_high_surrogate() {
        assert!(parse(r#""\uD83D""#).is_err());
    }

    #[test]
    fn rejects_lone_low_surrogate() {
        assert!(parse(r#""\uDE00""#).is_err());
    }

    #[test]
    fn rejects_floats_and_leading_zeros() {
        assert!(parse("1.5").is_err());
        assert!(parse("007").is_err());
        assert!(parse("1e3").is_err());
    }

    #[test]
    fn rejects_trailing_junk_and_unterminated() {
        assert!(parse(r#"{"a":1} extra"#).is_err());
        assert!(parse(r#"{"a":1"#).is_err());
        assert!(parse(r#""no end"#).is_err());
    }

    #[test]
    fn accepts_zero() {
        assert_eq!(parse("0").unwrap().as_u64(), Some(0));
    }
}

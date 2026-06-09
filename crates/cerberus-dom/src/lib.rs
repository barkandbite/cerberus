//! DOM node types and a deliberately minimal HTML parser.
//!
//! The real, spec-shaped tokenizer + tree builder (with arena allocation for
//! the parse tree — see PLAN.md memory budget) is M2 and is ours to write. This
//! placeholder handles the simple built-in pages so the M0 render path is whole;
//! it is not standards-compliant and is clearly labeled as such.

/// A DOM node: either an element or a text run.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// An element with a tag name, attributes, and children.
#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    /// A new childless, attribute-less element.
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// A new childless element with attributes.
    pub fn with_attrs(tag: impl Into<String>, attrs: Vec<(String, String)>) -> Self {
        Self {
            tag: tag.into(),
            attrs,
            children: Vec::new(),
        }
    }

    /// The value of attribute `name` (lowercased keys), if present.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Concatenate the text of this element and its descendants.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for child in &self.children {
            match child {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => out.push_str(&e.text_content()),
            }
        }
        out
    }
}

/// A parsed document, rooted at a synthetic `#root` element.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub root: Element,
}

impl Document {
    /// The page `<title>` text, if any.
    pub fn title(&self) -> Option<String> {
        find_title(&self.root)
    }
}

fn find_title(el: &Element) -> Option<String> {
    if el.tag == "title" {
        let text = el.text_content();
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    el.children.iter().find_map(|child| match child {
        Node::Element(e) => find_title(e),
        Node::Text(_) => None,
    })
}

/// Void elements that never have a close tag.
const VOID: &[&str] = &["br", "hr", "img", "meta", "link", "input"];

/// Parse a minimal subset of HTML: tags with attributes, text with entity
/// decoding + whitespace collapsing, comment skipping, and `<script>`/`<style>`
/// rawtext dropping. Not standards-compliant — placeholder until M2.
pub fn parse_trivial(input: &str) -> Document {
    let mut stack: Vec<Element> = vec![Element::new("#root")];

    for token in tokenize(input) {
        match token {
            Token::Text(text) => {
                let cleaned = clean_text(&text);
                if !cleaned.is_empty() {
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Text(cleaned));
                    }
                }
            }
            Token::Open(name, attrs) => {
                let el = Element::with_attrs(name, attrs);
                if VOID.contains(&el.tag.as_str()) {
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Element(el));
                    }
                } else {
                    stack.push(el);
                }
            }
            Token::SelfClose(name, attrs) => {
                if let Some(top) = stack.last_mut() {
                    top.children
                        .push(Node::Element(Element::with_attrs(name, attrs)));
                }
            }
            Token::Close(name) => {
                // Tolerant close: pop until we find a match, attaching popped
                // elements to their parents as we go.
                if let Some(pos) = stack.iter().rposition(|e| e.tag == name) {
                    while stack.len() > pos {
                        let closed = stack.pop().expect("stack non-empty above pos");
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(Node::Element(closed));
                        } else {
                            // Closed the root's only frame; restore it.
                            stack.push(closed);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Collapse any unclosed elements into their parents.
    while stack.len() > 1 {
        let closed = stack.pop().expect("len > 1");
        stack
            .last_mut()
            .expect("parent exists")
            .children
            .push(Node::Element(closed));
    }

    Document {
        root: stack.pop().expect("root frame"),
    }
}

enum Token {
    Text(String),
    Open(String, Vec<(String, String)>),
    Close(String),
    SelfClose(String, Vec<(String, String)>),
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = input;

    while !rest.is_empty() {
        match rest.find('<') {
            Some(0) => {}
            Some(lt) => {
                tokens.push(Token::Text(rest[..lt].to_string()));
                rest = &rest[lt..];
            }
            None => {
                tokens.push(Token::Text(rest.to_string()));
                break;
            }
        }

        // `rest` now starts at '<'. Skip HTML comments wholesale (their body may
        // contain '>' and is never page text).
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = match after.find("-->") {
                Some(end) => &after[end + 3..],
                None => "",
            };
            continue;
        }

        let Some(gt) = rest.find('>') else {
            tokens.push(Token::Text(rest.to_string()));
            break;
        };
        let inner = &rest[1..gt];
        rest = &rest[gt + 1..];

        if let Some(name) = inner.strip_prefix('/') {
            tokens.push(Token::Close(tag_name(name)));
        } else if let Some(stripped) = inner.strip_suffix('/') {
            let (name, attrs) = parse_tag(stripped);
            tokens.push(Token::SelfClose(name, attrs));
        } else {
            let (name, attrs) = parse_tag(inner);
            // Rawtext elements: their content is not markup and must not be shown
            // as page text. Skip to the matching close tag and drop the body.
            if name == "script" || name == "style" {
                let close = format!("</{name}");
                match find_ci(rest, &close) {
                    Some(pos) => {
                        let after = &rest[pos..];
                        rest = match after.find('>') {
                            Some(gt2) => &after[gt2 + 1..],
                            None => "",
                        };
                    }
                    None => rest = "",
                }
                tokens.push(Token::Open(name.clone(), attrs));
                tokens.push(Token::Close(name));
            } else {
                tokens.push(Token::Open(name, attrs));
            }
        }
    }

    tokens
}

/// Split a tag's inner text into a lowercased name and its attributes.
fn parse_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let inner = inner.trim();
    let (name, rest) = match inner.find(char::is_whitespace) {
        Some(i) => (&inner[..i], &inner[i..]),
        None => (inner, ""),
    };
    (name.to_ascii_lowercase(), parse_attrs(rest))
}

/// Parse `name="value"` / `name='value'` / `name=value` / `name` attributes.
fn parse_attrs(mut rest: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    rest = rest.trim_start();
    while !rest.is_empty() {
        let before = rest.len();
        let name_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();

        let mut value = String::new();
        if let Some(after_eq) = rest.strip_prefix('=') {
            rest = after_eq.trim_start();
            match rest.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    rest = &rest[1..];
                    if let Some(end) = rest.find(q) {
                        value = rest[..end].to_string();
                        rest = &rest[end + 1..];
                    } else {
                        value = std::mem::take(&mut value) + rest;
                        rest = "";
                    }
                }
                _ => {
                    let v_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    value = rest[..v_end].to_string();
                    rest = &rest[v_end..];
                }
            }
        }
        if !name.is_empty() {
            attrs.push((name, value));
        }
        rest = rest.trim_start();
        if rest.len() == before {
            break; // guarantee progress
        }
    }
    attrs
}

/// Case-insensitive byte-offset search (ASCII-fold). `needle` must be lowercase.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

/// Decode HTML entities, collapse runs of whitespace to single spaces, and trim.
fn clean_text(raw: &str) -> String {
    let decoded = decode_entities(raw);
    let mut out = String::with_capacity(decoded.len());
    let mut prev_space = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match rest.find(';') {
            Some(semi) if semi <= 10 => match decode_one(&rest[1..semi]) {
                Some(ch) => {
                    out.push(ch);
                    rest = &rest[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            },
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_one(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

/// Take the tag name (first whitespace-delimited token), lowercased.
fn tag_name(raw: &str) -> String {
    raw.split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtin_home() {
        let doc = parse_trivial(
            "<html><body><h1>Cerberus</h1><p>three heads, one process</p></body></html>",
        );
        assert_eq!(doc.root.text_content(), "Cerberusthree heads, one process");
        // Structure: #root > html > body > {h1, p}
        let html = match &doc.root.children[0] {
            Node::Element(e) => e,
            _ => panic!("expected html element"),
        };
        assert_eq!(html.tag, "html");
    }

    #[test]
    fn handles_void_and_unclosed_tags() {
        let doc = parse_trivial("<p>a<br>b");
        let p = match &doc.root.children[0] {
            Node::Element(e) => e,
            _ => panic!("expected p"),
        };
        assert_eq!(p.tag, "p");
        assert_eq!(p.text_content(), "ab");
    }

    #[test]
    fn skips_script_and_style_and_decodes_entities() {
        let doc = parse_trivial(
            "<body><style>p{color:red}</style>\
             <!-- hidden > comment -->\
             <p>a &amp; b &lt;c&gt; &#65;</p>\
             <script>if (x<1){ y }</script>done</body>",
        );
        let text = doc.root.text_content();
        assert!(
            text.contains("a & b <c> A"),
            "entities decoded; got {text:?}"
        );
        assert!(!text.contains("color:red"), "style content dropped");
        assert!(!text.contains("if ("), "script content dropped");
        assert!(!text.contains("hidden"), "comment dropped");
        assert!(text.contains("done"));
    }

    #[test]
    fn collapses_whitespace() {
        let doc = parse_trivial("<p>one\n\n   two\t three</p>");
        assert_eq!(doc.root.text_content(), "one two three");
    }

    fn find_tag<'a>(el: &'a Element, tag: &str) -> Option<&'a Element> {
        if el.tag == tag {
            return Some(el);
        }
        el.children.iter().find_map(|c| match c {
            Node::Element(e) => find_tag(e, tag),
            Node::Text(_) => None,
        })
    }

    #[test]
    fn parses_attributes_and_title() {
        let doc = parse_trivial(
            "<head><title>Hi &amp; Bye</title></head>\
             <body><a href=\"/x\" class='c' download>link</a></body>",
        );
        assert_eq!(doc.title().as_deref(), Some("Hi & Bye"));
        let a = find_tag(&doc.root, "a").expect("a element");
        assert_eq!(a.attr("href"), Some("/x"));
        assert_eq!(a.attr("class"), Some("c"));
        assert_eq!(a.attr("download"), Some(""), "boolean attribute");
    }
}

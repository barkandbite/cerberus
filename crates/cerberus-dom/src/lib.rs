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

/// Void elements that never have a close tag.
const VOID: &[&str] = &["br", "hr", "img", "meta", "link", "input"];

/// Parse a minimal subset of HTML: tags (attributes discarded), text with
/// entity decoding + whitespace collapsing, comment skipping, and `<script>`/
/// `<style>` rawtext dropping. Not standards-compliant — placeholder until M2.
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
            Token::Open(name) => {
                if VOID.contains(&name.as_str()) {
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Element(Element::new(name)));
                    }
                } else {
                    stack.push(Element::new(name));
                }
            }
            Token::SelfClose(name) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Element(Element::new(name)));
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
    Open(String),
    Close(String),
    SelfClose(String),
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
        } else if let Some(name) = inner.strip_suffix('/') {
            tokens.push(Token::SelfClose(tag_name(name)));
        } else {
            let name = tag_name(inner);
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
                tokens.push(Token::Open(name.clone()));
                tokens.push(Token::Close(name));
            } else {
                tokens.push(Token::Open(name));
            }
        }
    }

    tokens
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
}

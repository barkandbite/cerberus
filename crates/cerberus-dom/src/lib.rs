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

/// Parse a minimal subset of HTML. Attributes are discarded; comments and
/// entities are not handled. Placeholder until the M2 parser.
pub fn parse_trivial(input: &str) -> Document {
    let mut stack: Vec<Element> = vec![Element::new("#root")];

    for token in tokenize(input) {
        match token {
            Token::Text(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Text(trimmed.to_string()));
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

        // `rest` now starts at '<'.
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
            tokens.push(Token::Open(tag_name(inner)));
        }
    }

    tokens
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
}

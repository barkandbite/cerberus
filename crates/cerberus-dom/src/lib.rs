//! DOM node types and a pragmatic, tolerant HTML parser.
//!
//! `parse_html` is a real (if not fully spec-complete) parser: a **quote-aware
//! tokenizer** so `>` inside an attribute value is safe, rawtext/RCDATA handling
//! for `<script>`/`<style>`/`<title>`/`<textarea>`, comment and doctype skipping,
//! HTML entity decoding, and a **tree builder with optional-end-tag rules** (a
//! new `<li>`/`<tr>`/`<td>`/`<option>` closes the previous one; block elements
//! close an open `<p>`). It is deliberately not the full HTML5 tree-construction
//! algorithm — no foster parenting, adoption agency, or implicit html/head/body —
//! but it parses real-world pages without mis-nesting. Swapping the recursive
//! node tree for an arena (PLAN.md memory budget) is a later, behind-the-API
//! change.

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

/// Void elements: no children, no close tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Whether `tag`'s content is raw text (not parsed as markup), and if so whether
/// to keep that text. `<style>`/`<title>`/`<textarea>` keep it (the CSS engine /
/// page reads it); scripts and embedded frames are dropped.
fn rawtext_keep(tag: &str) -> Option<bool> {
    match tag {
        "style" | "title" | "textarea" => Some(true),
        "script" | "noscript" | "iframe" | "noembed" | "noframes" | "template" => Some(false),
        _ => None,
    }
}

/// Parse HTML into a [`Document`]. Tolerant of malformed and unclosed markup;
/// see the module docs for the supported subset.
pub fn parse_html(input: &str) -> Document {
    build_tree(tokenize(input))
}

/// Build the DOM tree from a token stream, applying void-element and
/// optional-end-tag rules.
fn build_tree(tokens: Vec<Token>) -> Document {
    let mut stack: Vec<Element> = vec![Element::new("#root")];

    for token in tokens {
        match token {
            Token::Text(text) => {
                let top = stack.last_mut().expect("root frame is always present");
                if top.tag == "style" {
                    // Keep CSS verbatim for the style engine.
                    top.children.push(Node::Text(text));
                } else {
                    let cleaned = clean_text(&text);
                    if !cleaned.is_empty() {
                        top.children.push(Node::Text(cleaned));
                    }
                }
            }
            Token::Open(name, attrs) => {
                close_implied(&mut stack, &name);
                if VOID.contains(&name.as_str()) {
                    push_child(&mut stack, Element::with_attrs(name, attrs));
                } else {
                    stack.push(Element::with_attrs(name, attrs));
                }
            }
            Token::SelfClose(name, attrs) => {
                close_implied(&mut stack, &name);
                push_child(&mut stack, Element::with_attrs(name, attrs));
            }
            Token::Close(name) => close_element(&mut stack, &name),
        }
    }

    // Collapse any unclosed elements into their parents.
    while stack.len() > 1 {
        let closed = stack.pop().expect("len > 1");
        push_child(&mut stack, closed);
    }

    Document {
        root: stack.pop().expect("root frame"),
    }
}

/// Append `child` to the current open element.
fn push_child(stack: &mut [Element], child: Element) {
    stack
        .last_mut()
        .expect("root frame is always present")
        .children
        .push(Node::Element(child));
}

/// Tolerant end tag: if a matching element is open, pop down to it (attaching
/// popped elements to their parents); otherwise ignore the stray close.
fn close_element(stack: &mut Vec<Element>, name: &str) {
    if let Some(pos) = stack.iter().rposition(|e| e.tag == name) {
        if pos == 0 {
            return; // never pop the synthetic root
        }
        while stack.len() > pos {
            let closed = stack.pop().expect("above pos");
            push_child(stack, closed);
        }
    }
}

/// Apply optional-end-tag rules before opening `opening`: a new list item /
/// table row / cell / option closes the previous one, and a block-level element
/// closes an open `<p>`.
fn close_implied(stack: &mut Vec<Element>, opening: &str) {
    while stack.len() > 1 {
        let top = stack.last().expect("non-root frame").tag.as_str();
        let close = match opening {
            "li" => top == "li",
            "dt" | "dd" => top == "dt" || top == "dd",
            "option" => top == "option",
            "optgroup" => top == "option" || top == "optgroup",
            "tr" => matches!(top, "tr" | "td" | "th"),
            "td" | "th" => matches!(top, "td" | "th"),
            "thead" | "tbody" | "tfoot" => {
                matches!(top, "td" | "th" | "tr" | "thead" | "tbody" | "tfoot")
            }
            _ => closes_paragraph(opening) && top == "p",
        };
        if !close {
            break;
        }
        let closed = stack.pop().expect("non-root frame");
        push_child(stack, closed);
    }
}

/// Block-level elements whose start implies `</p>`.
fn closes_paragraph(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "details"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "ul"
    )
}

enum Token {
    Text(String),
    Open(String, Vec<(String, String)>),
    SelfClose(String, Vec<(String, String)>),
    Close(String),
}

/// Split raw HTML into tokens. The scanner is quote-aware (so `>` inside an
/// attribute value never ends a tag early) and handles rawtext/RCDATA elements,
/// comments, and doctype/PI declarations.
fn tokenize(input: &str) -> Vec<Token> {
    let b = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < b.len() {
        if b[i] != b'<' {
            let start = i;
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
            tokens.push(Token::Text(input[start..i].to_string()));
            continue;
        }

        // `i` is at '<'.
        let tail = &input[i..];
        if let Some(rest) = tail.strip_prefix("<!--") {
            // Comment: skip to "-->".
            i += 4 + rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            continue;
        }
        if tail.starts_with("<!") || tail.starts_with("<?") {
            // Doctype / processing instruction / bogus comment: skip to '>'.
            i += tail.find('>').map(|e| e + 1).unwrap_or(tail.len());
            continue;
        }
        if b.get(i + 1) == Some(&b'/') {
            let (name, next) = read_end_tag(input, i);
            i = next;
            if !name.is_empty() {
                tokens.push(Token::Close(name));
            }
            continue;
        }
        if b.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
            let (name, attrs, self_close, next) = read_start_tag(input, i);
            i = next;
            match rawtext_keep(&name) {
                Some(keep) => {
                    // Content is raw: consume to the matching close tag.
                    tokens.push(Token::Open(name.clone(), attrs));
                    let (content, after) = read_rawtext(input, i, &name);
                    i = after;
                    if keep {
                        tokens.push(Token::Text(content.to_string()));
                    }
                    tokens.push(Token::Close(name));
                }
                None if self_close => tokens.push(Token::SelfClose(name, attrs)),
                None => tokens.push(Token::Open(name, attrs)),
            }
            continue;
        }

        // A literal '<' that does not start a tag — treat as text.
        tokens.push(Token::Text("<".to_string()));
        i += 1;
    }

    tokens
}

/// Read a start tag at `start` (`b[start] == '<'`): lowercased name, decoded
/// attributes, whether it self-closes, and the index just past the closing `>`.
fn read_start_tag(input: &str, start: usize) -> (String, Vec<(String, String)>, bool, usize) {
    let b = input.as_bytes();
    let mut i = start + 1; // past '<'
    let name_start = i;
    while i < b.len() && is_name_byte(b[i]) {
        i += 1;
    }
    let name = input[name_start..i].to_ascii_lowercase();

    let mut attrs = Vec::new();
    let mut self_close = false;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => break,
            Some(b'>') => {
                i += 1;
                break;
            }
            Some(b'/') => {
                self_close = true;
                i += 1;
            }
            _ => {
                let an_start = i;
                while i < b.len()
                    && !b[i].is_ascii_whitespace()
                    && b[i] != b'='
                    && b[i] != b'>'
                    && b[i] != b'/'
                {
                    i += 1;
                }
                let aname = input[an_start..i].to_ascii_lowercase();

                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                let mut aval = String::new();
                if b.get(i) == Some(&b'=') {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    match b.get(i) {
                        Some(&q) if q == b'"' || q == b'\'' => {
                            i += 1;
                            let v_start = i;
                            while i < b.len() && b[i] != q {
                                i += 1;
                            }
                            aval = decode_entities(&input[v_start..i]);
                            if i < b.len() {
                                i += 1; // past the closing quote
                            }
                        }
                        _ => {
                            let v_start = i;
                            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                                i += 1;
                            }
                            aval = decode_entities(&input[v_start..i]);
                        }
                    }
                }
                if !aname.is_empty() {
                    attrs.push((aname, aval));
                }
            }
        }
    }
    (name, attrs, self_close, i)
}

/// Read an end tag at `start` (`b[start] == '<'`, `b[start+1] == '/'`): its
/// lowercased name and the index just past the closing `>`.
fn read_end_tag(input: &str, start: usize) -> (String, usize) {
    let b = input.as_bytes();
    let mut i = start + 2; // past '</'
    let name_start = i;
    while i < b.len() && is_name_byte(b[i]) {
        i += 1;
    }
    let name = input[name_start..i].to_ascii_lowercase();
    while i < b.len() && b[i] != b'>' {
        i += 1;
    }
    if i < b.len() {
        i += 1; // past '>'
    }
    (name, i)
}

/// Read a rawtext/RCDATA element's content starting at `i`, up to its matching
/// `</name>`. Returns the inner text and the index just past the close tag.
fn read_rawtext<'a>(input: &'a str, i: usize, name: &str) -> (&'a str, usize) {
    let close = format!("</{name}");
    let hay = &input[i..];
    match find_ci(hay, &close) {
        Some(rel) => {
            let content = &hay[..rel];
            let after = &hay[rel..];
            let end = after.find('>').map(|g| g + 1).unwrap_or(after.len());
            (content, i + rel + end)
        }
        None => (hay, input.len()),
    }
}

/// Bytes allowed in a tag/attribute name.
fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_'
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parses_builtin_home() {
        let doc = parse_html(
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
        let doc = parse_html("<p>a<br>b");
        let p = match &doc.root.children[0] {
            Node::Element(e) => e,
            _ => panic!("expected p"),
        };
        assert_eq!(p.tag, "p");
        assert_eq!(p.text_content(), "ab");
    }

    #[test]
    fn skips_script_and_style_and_decodes_entities() {
        let doc = parse_html(
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
        assert!(
            text.contains("color:red"),
            "style text kept for the CSS engine"
        );
        assert!(!text.contains("if ("), "script content dropped");
        assert!(!text.contains("hidden"), "comment dropped");
        assert!(text.contains("done"));
    }

    #[test]
    fn collapses_whitespace() {
        let doc = parse_html("<p>one\n\n   two\t three</p>");
        assert_eq!(doc.root.text_content(), "one two three");
    }

    #[test]
    fn parses_attributes_and_title() {
        let doc = parse_html(
            "<head><title>Hi &amp; Bye</title></head>\
             <body><a href=\"/x\" class='c' download>link</a></body>",
        );
        assert_eq!(doc.title().as_deref(), Some("Hi & Bye"));
        let a = find_tag(&doc.root, "a").expect("a element");
        assert_eq!(a.attr("href"), Some("/x"));
        assert_eq!(a.attr("class"), Some("c"));
        assert_eq!(a.attr("download"), Some(""), "boolean attribute");
    }

    #[test]
    fn gt_inside_a_quoted_attribute_does_not_end_the_tag() {
        // The quote-aware scanner must not treat the '>' in title="a>b" as the
        // tag's end (the old line-based tokenizer did).
        let doc = parse_html("<a title=\"a>b\" href='/p'>hi</a>");
        let a = find_tag(&doc.root, "a").expect("a element");
        assert_eq!(a.attr("title"), Some("a>b"));
        assert_eq!(a.attr("href"), Some("/p"));
        assert_eq!(a.text_content(), "hi");
    }

    #[test]
    fn list_items_auto_close() {
        // `<li>a<li>b` are siblings, not nested.
        let doc = parse_html("<ul><li>a<li>b</ul>");
        let ul = find_tag(&doc.root, "ul").expect("ul");
        let lis: Vec<_> = ul
            .children
            .iter()
            .filter_map(|c| match c {
                Node::Element(e) if e.tag == "li" => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(lis.len(), 2, "two sibling <li> items");
        assert_eq!(lis[0].text_content(), "a");
        assert_eq!(lis[1].text_content(), "b");
    }

    #[test]
    fn table_cells_and_rows_auto_close() {
        let doc = parse_html("<table><tr><td>a<td>b<tr><td>c</table>");
        let rows: Vec<_> = find_tag(&doc.root, "table")
            .expect("table")
            .children
            .iter()
            .filter_map(|c| match c {
                Node::Element(e) if e.tag == "tr" => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 2, "two rows");
        let first_cells = rows[0]
            .children
            .iter()
            .filter(|c| matches!(c, Node::Element(e) if e.tag == "td"))
            .count();
        assert_eq!(first_cells, 2, "first row has two cells");
    }

    #[test]
    fn block_element_closes_open_paragraph() {
        // `<p>a<div>b</div>` — the div closes the p (they are siblings).
        let doc = parse_html("<p>a<div>b</div>");
        let p = find_tag(&doc.root, "p").expect("p");
        assert_eq!(p.text_content(), "a", "div is not inside the p");
        assert!(find_tag(&doc.root, "div").is_some());
    }

    #[test]
    fn textarea_content_is_rawtext() {
        // The '<' in the textarea body must not start a tag.
        let doc = parse_html("<textarea>1 < 2 && 3 > 0</textarea>");
        let ta = find_tag(&doc.root, "textarea").expect("textarea");
        assert!(ta.text_content().contains("1 < 2"), "raw text preserved");
        assert!(find_tag(&doc.root, "2").is_none(), "no phantom element");
    }

    #[test]
    fn doctype_is_skipped() {
        let doc = parse_html("<!DOCTYPE html><html><body><h1>Hi</h1></body></html>");
        assert!(find_tag(&doc.root, "!doctype").is_none());
        assert!(find_tag(&doc.root, "h1").is_some());
    }

    #[test]
    fn entities_in_attribute_values_are_decoded() {
        let doc = parse_html("<a href='/s?a=1&amp;b=2'>q</a>");
        let a = find_tag(&doc.root, "a").expect("a");
        assert_eq!(a.attr("href"), Some("/s?a=1&b=2"));
    }
}

//! DOM node types and a pragmatic, tolerant HTML parser.
//!
//! `parse_html` is a real (if not fully spec-complete) parser: a **quote-aware
//! tokenizer** so `>` inside an attribute value is safe, rawtext/RCDATA handling
//! for `<script>`/`<style>`/`<title>`/`<textarea>`, comment and doctype skipping,
//! HTML entity decoding, and a **tree builder with optional-end-tag rules** (a
//! new `<li>`/`<tr>`/`<td>`/`<option>` closes the previous one; block elements
//! close an open `<p>`). It is deliberately not the full HTML5 tree-construction
//! algorithm — no foster parenting, adoption agency, or implicit html/head/body —
//! but it parses real-world pages without mis-nesting.
//!
//! Storage is an **arena** (PLAN.md memory budget): a [`Document`] owns one flat
//! `Vec<NodeData>` and children are referenced by [`NodeId`] index, never by
//! owned subtrees. Reads go through [`NodeRef`], a `Copy` cursor borrowing the
//! document; programmatic construction goes through [`DocumentBuilder`]. The
//! tokenizer and tree-construction *behavior* are unchanged — only the backing
//! store and the read/write API differ.

/// Index of a node within a [`Document`]'s arena.
pub type NodeId = u32;

/// A node stored in the arena: either an element or a text run. Children are
/// referenced by [`NodeId`], so the structure is flat rather than recursive.
#[derive(Clone, Debug, PartialEq)]
enum NodeData {
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<NodeId>,
    },
    Text(String),
}

/// A parsed document, rooted at a synthetic `#root` element. Owns the arena of
/// every node; navigate it with [`Document::root`] and [`NodeRef`].
///
/// Inline `<script>` sources are collected out-of-band in [`scripts`] (in
/// document order) rather than as render-tree nodes, so the JS engine can run
/// them while their text never appears as visible page content.
///
/// [`scripts`]: Document::scripts
#[derive(Clone, Debug)]
pub struct Document {
    nodes: Vec<NodeData>,
    root: NodeId,
    /// Inline `<script>` bodies (no `src`), in document order. Kept separate
    /// from `nodes` so script source is never rendered.
    scripts: Vec<String>,
}

impl Document {
    /// A cursor at the synthetic `#root` element.
    pub fn root(&self) -> NodeRef<'_> {
        NodeRef {
            doc: self,
            id: self.root,
        }
    }

    /// Inline `<script>` sources (elements without a `src` attribute), in
    /// document order. The text is the raw script body, undecoded — it is *not*
    /// part of the render tree and never appears in [`NodeRef::text_content`].
    pub fn scripts(&self) -> &[String] {
        &self.scripts
    }

    /// The page `<title>` text (trimmed), if any.
    pub fn title(&self) -> Option<String> {
        find_title(self.root())
    }
}

/// Depth-first search for the first `<title>` with non-empty trimmed text.
fn find_title(node: NodeRef<'_>) -> Option<String> {
    if node.tag() == "title" {
        let text = node.text_content();
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    node.children().find_map(find_title)
}

/// A lightweight cursor into a [`Document`]'s arena. `Copy`: cheap to pass
/// around and clone; it borrows the document and resolves nodes on demand.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    doc: &'a Document,
    id: NodeId,
}

impl<'a> NodeRef<'a> {
    /// The node's arena id — its stable identity within the document, suitable
    /// for ancestor or equality checks.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The backing arena entry for this cursor.
    fn data(&self) -> &'a NodeData {
        &self.doc.nodes[self.id as usize]
    }

    /// Whether this node is an element.
    pub fn is_element(&self) -> bool {
        matches!(self.data(), NodeData::Element { .. })
    }

    /// Whether this node is a text run.
    pub fn is_text(&self) -> bool {
        matches!(self.data(), NodeData::Text(_))
    }

    /// The element tag (lowercased), or `""` for text nodes.
    pub fn tag(&self) -> &'a str {
        match self.data() {
            NodeData::Element { tag, .. } => tag,
            NodeData::Text(_) => "",
        }
    }

    /// The text of a text node, or `None` for elements.
    pub fn text(&self) -> Option<&'a str> {
        match self.data() {
            NodeData::Text(t) => Some(t),
            NodeData::Element { .. } => None,
        }
    }

    /// The value of attribute `name` (lowercased keys), if present. Always
    /// `None` for text nodes.
    pub fn attr(&self, name: &str) -> Option<&'a str> {
        self.attrs()
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// This element's attributes; an empty slice for text nodes.
    pub fn attrs(&self) -> &'a [(String, String)] {
        match self.data() {
            NodeData::Element { attrs, .. } => attrs,
            NodeData::Text(_) => &[],
        }
    }

    /// The node's children, in document order. Empty for text nodes.
    pub fn children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let doc = self.doc;
        let ids: &'a [NodeId] = match self.data() {
            NodeData::Element { children, .. } => children,
            NodeData::Text(_) => &[],
        };
        ids.iter().map(move |&id| NodeRef { doc, id })
    }

    /// Recursive concatenation of all descendant text (the old
    /// `Element::text_content`).
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    /// Append this node's text and that of its descendants to `out`.
    fn collect_text(&self, out: &mut String) {
        match self.data() {
            NodeData::Text(t) => out.push_str(t),
            NodeData::Element { .. } => {
                for child in self.children() {
                    child.collect_text(out);
                }
            }
        }
    }
}

/// Builds a [`Document`] programmatically (for app-generated pages). Children
/// are created first (post-order), then their ids are handed to their parent;
/// [`finish`](DocumentBuilder::finish) names the root.
pub struct DocumentBuilder {
    nodes: Vec<NodeData>,
}

impl DocumentBuilder {
    /// A new, empty builder.
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Allocate a text node, returning its id.
    pub fn text(&mut self, s: impl Into<String>) -> NodeId {
        self.push(NodeData::Text(s.into()))
    }

    /// Allocate an attribute-less element over the given (already-created)
    /// children, returning its id.
    pub fn element(
        &mut self,
        tag: impl Into<String>,
        children: impl IntoIterator<Item = NodeId>,
    ) -> NodeId {
        self.element_attrs(tag, Vec::new(), children)
    }

    /// Allocate an element with attributes over the given (already-created)
    /// children, returning its id.
    pub fn element_attrs(
        &mut self,
        tag: impl Into<String>,
        attrs: Vec<(String, String)>,
        children: impl IntoIterator<Item = NodeId>,
    ) -> NodeId {
        self.push(NodeData::Element {
            tag: tag.into(),
            attrs,
            children: children.into_iter().collect(),
        })
    }

    /// Finish, with `root` as the document root. App-generated documents carry
    /// no inline scripts.
    pub fn finish(self, root: NodeId) -> Document {
        Document {
            nodes: self.nodes,
            root,
            scripts: Vec::new(),
        }
    }

    /// Push a node into the arena and return its freshly assigned id.
    fn push(&mut self, node: NodeData) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        id
    }
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
    }
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
    let (tokens, scripts) = tokenize(input);
    build_tree(tokens, scripts)
}

/// An open element on the builder stack: its tag, attributes, and the ids of the
/// children accumulated so far. Materialized into a [`NodeData::Element`] in the
/// arena when the element closes.
struct OpenElement {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<NodeId>,
}

impl OpenElement {
    fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    fn with_attrs(tag: impl Into<String>, attrs: Vec<(String, String)>) -> Self {
        Self {
            tag: tag.into(),
            attrs,
            children: Vec::new(),
        }
    }
}

/// Build the DOM tree from a token stream into a flat arena, applying
/// void-element and optional-end-tag rules. The stack holds open elements; on
/// close (or void/self-close) the element is allocated in the arena and its id
/// pushed into its parent's child list. `scripts` holds the inline `<script>`
/// sources already gathered (in document order) by the tokenizer; they are
/// stored on the [`Document`] but never enter the render tree.
fn build_tree(tokens: Vec<Token>, scripts: Vec<String>) -> Document {
    let mut nodes: Vec<NodeData> = Vec::new();
    let mut stack: Vec<OpenElement> = vec![OpenElement::new("#root")];

    for token in tokens {
        match token {
            Token::Text(text) => {
                let top = stack.last_mut().expect("root frame is always present");
                if top.tag == "style" {
                    // Keep CSS verbatim for the style engine.
                    let id = alloc(&mut nodes, NodeData::Text(text));
                    top.children.push(id);
                } else {
                    let cleaned = clean_text(&text);
                    if !cleaned.is_empty() {
                        let id = alloc(&mut nodes, NodeData::Text(cleaned));
                        stack
                            .last_mut()
                            .expect("root frame is always present")
                            .children
                            .push(id);
                    }
                }
            }
            Token::Open(name, attrs) => {
                close_implied(&mut nodes, &mut stack, &name);
                if VOID.contains(&name.as_str()) {
                    push_element(&mut nodes, &mut stack, OpenElement::with_attrs(name, attrs));
                } else {
                    stack.push(OpenElement::with_attrs(name, attrs));
                }
            }
            Token::SelfClose(name, attrs) => {
                close_implied(&mut nodes, &mut stack, &name);
                push_element(&mut nodes, &mut stack, OpenElement::with_attrs(name, attrs));
            }
            Token::Close(name) => close_element(&mut nodes, &mut stack, &name),
        }
    }

    // Collapse any unclosed elements into their parents.
    while stack.len() > 1 {
        let closed = stack.pop().expect("len > 1");
        push_element(&mut nodes, &mut stack, closed);
    }

    let root_frame = stack.pop().expect("root frame");
    let root = alloc(
        &mut nodes,
        NodeData::Element {
            tag: root_frame.tag,
            attrs: root_frame.attrs,
            children: root_frame.children,
        },
    );

    Document {
        nodes,
        root,
        scripts,
    }
}

/// Push a node into the arena and return its freshly assigned id.
fn alloc(nodes: &mut Vec<NodeData>, node: NodeData) -> NodeId {
    let id = nodes.len() as NodeId;
    nodes.push(node);
    id
}

/// Materialize `child` into the arena and append its id to the current open
/// element.
fn push_element(nodes: &mut Vec<NodeData>, stack: &mut [OpenElement], child: OpenElement) {
    let id = alloc(
        nodes,
        NodeData::Element {
            tag: child.tag,
            attrs: child.attrs,
            children: child.children,
        },
    );
    stack
        .last_mut()
        .expect("root frame is always present")
        .children
        .push(id);
}

/// Tolerant end tag: if a matching element is open, pop down to it (allocating
/// popped elements and attaching them to their parents); otherwise ignore the
/// stray close.
fn close_element(nodes: &mut Vec<NodeData>, stack: &mut Vec<OpenElement>, name: &str) {
    if let Some(pos) = stack.iter().rposition(|e| e.tag == name) {
        if pos == 0 {
            return; // never pop the synthetic root
        }
        while stack.len() > pos {
            let closed = stack.pop().expect("above pos");
            push_element(nodes, stack, closed);
        }
    }
}

/// Apply optional-end-tag rules before opening `opening`: a new list item /
/// table row / cell / option closes the previous one, and a block-level element
/// closes an open `<p>`.
fn close_implied(nodes: &mut Vec<NodeData>, stack: &mut Vec<OpenElement>, opening: &str) {
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
        push_element(nodes, stack, closed);
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
///
/// Returns the token stream plus the inline `<script>` sources (no `src`
/// attribute) in document order. Script bodies are captured here so they never
/// become render-tree text.
fn tokenize(input: &str) -> (Vec<Token>, Vec<String>) {
    let b = input.as_bytes();
    let mut tokens = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
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
                    // Inline scripts (no `src`) are collected out-of-band for
                    // the JS engine — capture before `attrs` is moved into the
                    // Open token.
                    let is_inline_script =
                        name == "script" && !attrs.iter().any(|(k, _)| k == "src");
                    tokens.push(Token::Open(name.clone(), attrs));
                    let (content, after) = read_rawtext(input, i, &name);
                    i = after;
                    if is_inline_script {
                        scripts.push(content.to_string());
                    }
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

    (tokens, scripts)
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

    /// Depth-first search for the first descendant (or `node` itself) whose tag
    /// matches, returning a cursor at it.
    fn find_tag<'a>(node: NodeRef<'a>, tag: &str) -> Option<NodeRef<'a>> {
        if node.is_element() && node.tag() == tag {
            return Some(node);
        }
        node.children().find_map(|c| find_tag(c, tag))
    }

    #[test]
    fn parses_builtin_home() {
        let doc = parse_html(
            "<html><body><h1>Cerberus</h1><p>three heads, one process</p></body></html>",
        );
        assert_eq!(
            doc.root().text_content(),
            "Cerberusthree heads, one process"
        );
        // Structure: #root > html > body > {h1, p}
        let html = doc.root().children().next().expect("html element");
        assert!(html.is_element());
        assert_eq!(html.tag(), "html");
    }

    #[test]
    fn handles_void_and_unclosed_tags() {
        let doc = parse_html("<p>a<br>b");
        let p = doc.root().children().next().expect("p");
        assert_eq!(p.tag(), "p");
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
        let text = doc.root().text_content();
        assert!(
            text.contains("a & b <c> A"),
            "entities decoded; got {text:?}"
        );
        assert!(
            text.contains("color:red"),
            "style text kept for the CSS engine"
        );
        // Script source is now *collected* for the JS engine but must still not
        // *render*: it never appears in the page's text content.
        assert!(!text.contains("if ("), "script content not rendered");
        assert!(!text.contains("hidden"), "comment dropped");
        assert!(text.contains("done"));
        // The inline script body is captured separately, rawtext (undecoded).
        assert_eq!(doc.scripts(), &["if (x<1){ y }".to_string()]);
    }

    #[test]
    fn inline_scripts_are_collected_in_order() {
        let doc = parse_html(
            "<head><script>A=1</script></head><body><p>hi</p><script>B=2</script></body>",
        );
        assert_eq!(doc.scripts(), &["A=1".to_string(), "B=2".to_string()]);
    }

    #[test]
    fn external_script_contributes_no_inline_source() {
        let doc = parse_html("<script src=\"x.js\"></script>");
        assert!(
            doc.scripts().is_empty(),
            "external scripts contribute no inline source; got {:?}",
            doc.scripts()
        );
    }

    #[test]
    fn script_with_src_and_body_ignores_body() {
        // A script with both `src` and a body is treated as external: its body
        // is ignored (matches browser behavior).
        let doc = parse_html("<script src=\"x.js\">SHOULD_BE_IGNORED</script>");
        assert!(
            doc.scripts().is_empty(),
            "body of a src'd script is ignored; got {:?}",
            doc.scripts()
        );
    }

    #[test]
    fn script_text_does_not_render() {
        let doc = parse_html("<body><p>visible</p><script>document.x='SECRET'</script></body>");
        let body = find_tag(doc.root(), "body").expect("body");
        let text = body.text_content();
        assert!(
            text.contains("visible"),
            "visible text renders; got {text:?}"
        );
        assert!(
            !text.contains("SECRET"),
            "script source must never render; got {text:?}"
        );
        // It is, however, available to the JS engine.
        assert_eq!(doc.scripts(), &["document.x='SECRET'".to_string()]);
    }

    #[test]
    fn collapses_whitespace() {
        let doc = parse_html("<p>one\n\n   two\t three</p>");
        assert_eq!(doc.root().text_content(), "one two three");
    }

    #[test]
    fn parses_attributes_and_title() {
        let doc = parse_html(
            "<head><title>Hi &amp; Bye</title></head>\
             <body><a href=\"/x\" class='c' download>link</a></body>",
        );
        assert_eq!(doc.title().as_deref(), Some("Hi & Bye"));
        let a = find_tag(doc.root(), "a").expect("a element");
        assert_eq!(a.attr("href"), Some("/x"));
        assert_eq!(a.attr("class"), Some("c"));
        assert_eq!(a.attr("download"), Some(""), "boolean attribute");
    }

    #[test]
    fn gt_inside_a_quoted_attribute_does_not_end_the_tag() {
        // The quote-aware scanner must not treat the '>' in title="a>b" as the
        // tag's end (the old line-based tokenizer did).
        let doc = parse_html("<a title=\"a>b\" href='/p'>hi</a>");
        let a = find_tag(doc.root(), "a").expect("a element");
        assert_eq!(a.attr("title"), Some("a>b"));
        assert_eq!(a.attr("href"), Some("/p"));
        assert_eq!(a.text_content(), "hi");
    }

    #[test]
    fn list_items_auto_close() {
        // `<li>a<li>b` are siblings, not nested.
        let doc = parse_html("<ul><li>a<li>b</ul>");
        let ul = find_tag(doc.root(), "ul").expect("ul");
        let lis: Vec<_> = ul
            .children()
            .filter(|c| c.is_element() && c.tag() == "li")
            .collect();
        assert_eq!(lis.len(), 2, "two sibling <li> items");
        assert_eq!(lis[0].text_content(), "a");
        assert_eq!(lis[1].text_content(), "b");
    }

    #[test]
    fn table_cells_and_rows_auto_close() {
        let doc = parse_html("<table><tr><td>a<td>b<tr><td>c</table>");
        let table = find_tag(doc.root(), "table").expect("table");
        let rows: Vec<_> = table
            .children()
            .filter(|c| c.is_element() && c.tag() == "tr")
            .collect();
        assert_eq!(rows.len(), 2, "two rows");
        let first_cells = rows[0]
            .children()
            .filter(|c| c.is_element() && c.tag() == "td")
            .count();
        assert_eq!(first_cells, 2, "first row has two cells");
    }

    #[test]
    fn block_element_closes_open_paragraph() {
        // `<p>a<div>b</div>` — the div closes the p (they are siblings).
        let doc = parse_html("<p>a<div>b</div>");
        let p = find_tag(doc.root(), "p").expect("p");
        assert_eq!(p.text_content(), "a", "div is not inside the p");
        assert!(find_tag(doc.root(), "div").is_some());
    }

    #[test]
    fn textarea_content_is_rawtext() {
        // The '<' in the textarea body must not start a tag.
        let doc = parse_html("<textarea>1 < 2 && 3 > 0</textarea>");
        let ta = find_tag(doc.root(), "textarea").expect("textarea");
        assert!(ta.text_content().contains("1 < 2"), "raw text preserved");
        assert!(find_tag(doc.root(), "2").is_none(), "no phantom element");
    }

    #[test]
    fn doctype_is_skipped() {
        let doc = parse_html("<!DOCTYPE html><html><body><h1>Hi</h1></body></html>");
        assert!(find_tag(doc.root(), "!doctype").is_none());
        assert!(find_tag(doc.root(), "h1").is_some());
    }

    #[test]
    fn entities_in_attribute_values_are_decoded() {
        let doc = parse_html("<a href='/s?a=1&amp;b=2'>q</a>");
        let a = find_tag(doc.root(), "a").expect("a");
        assert_eq!(a.attr("href"), Some("/s?a=1&b=2"));
    }

    #[test]
    fn builder_round_trips() {
        // Build `<a href="/x">hi <b>there</b></a>` post-order, then read back.
        let mut b = DocumentBuilder::new();
        let hi = b.text("hi ");
        let there = b.text("there");
        let bold = b.element("b", [there]);
        let a = b.element_attrs("a", vec![("href".into(), "/x".into())], [hi, bold]);
        let doc = b.finish(a);

        let root = doc.root();
        assert_eq!(root.tag(), "a");
        assert_eq!(root.attr("href"), Some("/x"));
        assert_eq!(root.text_content(), "hi there");
        assert!(doc.scripts().is_empty(), "builder docs have no scripts");

        let kids: Vec<_> = root.children().collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].text(), Some("hi "));
        assert!(kids[1].is_element());
        assert_eq!(kids[1].tag(), "b");
        assert_eq!(kids[1].text_content(), "there");
    }

    #[test]
    fn node_ids_are_distinct_and_children_ordered() {
        // Two distinct children with distinct ids, yielded in insertion order.
        let mut b = DocumentBuilder::new();
        let first = b.text("first");
        let second = b.text("second");
        let parent = b.element("p", [first, second]);
        let doc = b.finish(parent);

        let kids: Vec<_> = doc.root().children().collect();
        assert_eq!(kids.len(), 2);
        assert_ne!(kids[0].id(), kids[1].id(), "different nodes, different ids");
        assert_eq!(kids[0].id(), first);
        assert_eq!(kids[1].id(), second);
        assert_eq!(kids[0].text(), Some("first"));
        assert_eq!(kids[1].text(), Some("second"));
    }
}

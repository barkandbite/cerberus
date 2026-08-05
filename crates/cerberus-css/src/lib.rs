//! Our CSS engine (`CssEngine: StyleEngine`): parse the UA + author stylesheets,
//! run the cascade, and produce a `StyledDom`. Bootstrapped — no dependencies.
//!
//! `visibility` and `opacity` are honored (a hidden element is laid out but not
//! painted; a transparent one is composited). Time-based effects —
//! `animation*`, `transition*`, `transform` — are still ignored (no
//! compositor/timeline), so content they would reveal renders immediately.

mod color;
mod parser;

pub use color::parse_color;

use cerberus_dom::{Document, NodeRef};
use cerberus_style::{
    AlignItems, AlignSelf, BoxShadow, BoxSizing, Clear, ComputedStyle, Display, ExternalSheets,
    FlexBasis, FlexDirection, Float, Gradient, JustifyContent, Len, LineHeight, ListStyleType,
    Position, StyleEngine, StyledChild, StyledDom, StyledNode, TextAlign, TextTransform, Track,
    TrackMax, VerticalAlign, Visibility, WhiteSpace,
};
use cerberus_types::{Color, GenericFamily, ImageFit, ImagePos, Point};
use parser::{
    parse_declaration_block, parse_stylesheet, BucketKey, ElemRef, MediaContext, PseudoElement,
    SiblingRef, Specificity, Stylesheet,
};
use std::collections::HashMap;
use std::rc::Rc;

/// Rules indexed by their subject selector's key (id/class/tag/universal), so a
/// given element only tests the rules it could actually match instead of the
/// whole sheet. Values are indices into the stylesheet's `rules`, preserving
/// source order for the cascade tie-break.
#[derive(Default)]
struct RuleIndex {
    id: HashMap<String, Vec<usize>>,
    class: HashMap<String, Vec<usize>>,
    tag: HashMap<String, Vec<usize>>,
    /// Rules whose subject has no id/class/tag (`*`, attribute-only, pseudo-only):
    /// they could match any element, so every element must consider them.
    universal: Vec<usize>,
}

impl RuleIndex {
    /// Build an index over `sheet` using `keys` to read each rule's bucket keys
    /// (normal vs pseudo-element selectors are indexed separately).
    fn build(sheet: &Stylesheet, keys: impl Fn(&parser::Rule) -> Vec<BucketKey>) -> Self {
        let mut idx = RuleIndex::default();
        for (i, rule) in sheet.rules.iter().enumerate() {
            for key in keys(rule) {
                match key {
                    BucketKey::Id(s) => idx.id.entry(s).or_default().push(i),
                    BucketKey::Class(s) => idx.class.entry(s).or_default().push(i),
                    BucketKey::Tag(s) => idx.tag.entry(s).or_default().push(i),
                    BucketKey::Universal => idx.universal.push(i),
                }
            }
        }
        idx
    }

    /// The rule indices an element with this `tag`/`id`/`classes` could match,
    /// ascending and de-duplicated (a rule reachable via several of its selectors
    /// — e.g. `.a, .b` for an element with both classes — appears once so its
    /// declarations aren't applied twice).
    fn candidates(&self, tag: &str, id: Option<&str>, classes: &[String]) -> Vec<usize> {
        let mut out = self.universal.clone();
        if let Some(v) = self.tag.get(tag) {
            out.extend_from_slice(v);
        }
        if let Some(v) = id.and_then(|i| self.id.get(i)) {
            out.extend_from_slice(v);
        }
        for c in classes {
            if let Some(v) = self.class.get(c) {
                out.extend_from_slice(v);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// A stylesheet's two cascade indices: the element cascade and the generated
/// `::before`/`::after` cascade (built from disjoint sets of the sheet's
/// selectors, since a rule can feed either or both).
struct SheetIndex {
    normal: RuleIndex,
    pseudo: RuleIndex,
}

impl SheetIndex {
    fn build(sheet: &Stylesheet) -> Self {
        Self {
            normal: RuleIndex::build(sheet, parser::Rule::bucket_keys_normal),
            pseudo: RuleIndex::build(sheet, parser::Rule::bucket_keys_pseudo),
        }
    }
}

/// The cascaded CSS custom properties (`--name` → raw value) in scope for an
/// element. Inherited down the tree and shared by `Rc` so elements that declare
/// no custom properties (the common case) reuse their parent's map for free
/// (ADR-0035). Keys are lowercased to match the parser's property-name folding.
type Vars = Rc<HashMap<String, String>>;

/// A matched rule during the cascade: (origin, specificity, source-order, decls).
type MatchedRule<'a> = (u8, Specificity, usize, &'a Vec<(String, String, bool)>);

/// The user-agent default stylesheet.
const UA_CSS: &str = r#"
html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
header, footer, nav, main, aside, blockquote, pre, figure, figcaption, form,
table, tr, hr, dl, dt, dd, fieldset, address, center,
details, summary { display: block; }
/* Legacy presentational elements still seen on older pages. */
center { text-align: -webkit-center; }
nobr { white-space: nowrap; }
head, title, meta, link, style, script, base, template { display: none; }
/* Inline SVG renders: the app pre-rasterizes every <svg> subtree
   (cerberus-app::inline_svg, resvg) and the element KEEPS its tag with a
   synthetic `src`, so author tag selectors (`.logo svg{width:84px}`,
   media-query variant hiding) size and toggle it exactly as in Chrome. No
   display:none guard here — layout renders a src-carrying <svg> as a
   replaced image and skips a raw (unconverted) <svg> subtree entirely, so
   its <text>/<title> can never leak as page text. */
li { display: list-item; }
ol { list-style-type: decimal; }
/* The `type` attribute selects the ordered-list marker (HTML UA stylesheet). */
ol[type="a"] { list-style-type: lower-alpha; }
ol[type="A"] { list-style-type: upper-alpha; }
ol[type="i"] { list-style-type: lower-roman; }
ol[type="I"] { list-style-type: upper-roman; }
ol[type="1"] { list-style-type: decimal; }
/* The page inset is the BODY's margin (Chrome UA: body{margin:8px}), not an
   engine constant — so `body{margin:0}` pages (most modern sites) really start
   at the edge, and the body margin collapses with its first child's like any
   other margin. */
body { margin: 8px; }
/* Heading sizes and margins mirror Chrome's UA sheet (em values computed
   against each heading's own size: h1 0.67em of 32px = 21px, h4 1.33em of
   16px = 21px, …) so unstyled pages keep Chrome's vertical rhythm. */
h1 { font-size: 32px; font-weight: bold; margin-top: 21px; margin-bottom: 21px; }
h2 { font-size: 24px; font-weight: bold; margin-top: 20px; margin-bottom: 20px; }
h3 { font-size: 19px; font-weight: bold; margin-top: 19px; margin-bottom: 19px; }
h4 { font-size: 16px; font-weight: bold; margin-top: 21px; margin-bottom: 21px; }
h5 { font-size: 13px; font-weight: bold; margin-top: 22px; margin-bottom: 22px; }
h6 { font-size: 11px; font-weight: bold; margin-top: 25px; margin-bottom: 25px; }
p { margin-top: 16px; margin-bottom: 16px; }
/* Chrome indents lists with 40px of padding (marker outside), not margin. */
ul, ol { margin-top: 16px; margin-bottom: 16px; padding-left: 40px; }
blockquote { margin-top: 16px; margin-bottom: 16px; margin-left: 40px; margin-right: 40px; }
figure { margin-top: 16px; margin-bottom: 16px; margin-left: 40px; margin-right: 40px; }
dd { margin-left: 40px; }
pre { white-space: pre; margin-top: 16px; margin-bottom: 16px; font-family: monospace; }
code, kbd, samp, tt { white-space: pre; font-family: monospace; }
/* Only anchors with an href are links (`:any-link`); a bare `<a name=…>`
   placeholder is not styled blue/underlined. */
a[href] { color: #154fd2; text-decoration: underline; }
b, strong { font-weight: bold; }
i, em, cite, var, dfn, address { font-style: italic; }
del, s, strike { text-decoration: line-through; }
ins, u { text-decoration: underline; }
mark { background-color: yellow; color: black; }
small { font-size: smaller; }
sub { vertical-align: sub; font-size: smaller; }
sup { vertical-align: super; font-size: smaller; }
/* The `hidden` boolean attribute hides the element (HTML UA stylesheet). Low
   specificity (one attribute selector), so an author `display` still wins. */
[hidden] { display: none; }
"#;

/// CSS engine built on our parser + cascade.
pub struct CssEngine {
    ua: Stylesheet,
    /// Cascade index over the (fixed) UA sheet, built once at construction.
    ua_index: SheetIndex,
    media: MediaContext,
}

impl CssEngine {
    /// Build an engine with the bundled UA stylesheet parsed, for a default
    /// desktop viewport (used where `@media` width does not matter).
    pub fn new() -> Self {
        Self::with_media(1280, 800)
    }

    /// Build an engine that evaluates `@media` queries against `width`×`height`.
    pub fn with_media(width: u32, height: u32) -> Self {
        let ua = parse_stylesheet(UA_CSS);
        let ua_index = SheetIndex::build(&ua);
        Self {
            ua,
            ua_index,
            media: MediaContext { width, height },
        }
    }

    // The cascade walker threads per-element context (siblings/index/parent/
    // custom-properties) plus the shared path & author sheet; these vary per call,
    // so bundling them wouldn't aid readability.
    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        node: NodeRef<'_>,
        siblings: Rc<[SiblingRef]>,
        index: usize,
        parent: &ComputedStyle,
        parent_vars: &Vars,
        path: &mut Vec<ElemRef>,
        author: &Stylesheet,
        author_index: &SheetIndex,
        // Root element's computed font-size in px — the base for `rem`.
        // `html { font-size: 62.5% }` → 1rem = 10px; a hardcoded 16 would size
        // every rem-based box 1.6x too large.
        root_font_size: u32,
    ) -> StyledNode {
        let is_root = node.tag() == "#root";
        let mut style = if is_root {
            ComputedStyle::initial()
        } else {
            parent.inherit()
        };

        path.push(ElemRef {
            siblings: siblings.clone(),
            index,
        });

        // The custom-property registry in scope for this element (inherited,
        // augmented by any `--*` it declares) — see `collect_vars` (ADR-0035).
        let mut vars = parent_vars.clone();

        if !is_root {
            // Legacy HTML presentational attributes (`width`/`bgcolor`/…) act as
            // the lowest-priority author style (HTML §15 presentational hints), so
            // apply them before the cascade — any real CSS rule overrides them, but
            // old table-driven pages (Hacker News, forums) still get their intended
            // widths and colors.
            apply_presentational_hints(&mut style, node);

            // Collect matching declarations: (origin, specificity, source-order),
            // honoring @media against the engine's viewport. Only rules whose
            // subject key this element carries are tested (see `RuleIndex`) — the
            // rest cannot match, so scanning them was pure overhead on big pages.
            let el = &siblings[index];
            let el_id = el.id.as_deref();
            let mut matched: Vec<MatchedRule<'_>> = Vec::new();
            for order in self.ua_index.normal.candidates(&el.tag, el_id, &el.classes) {
                let rule = &self.ua.rules[order];
                if rule.applies(self.media) {
                    if let Some(spec) = rule.matches(path) {
                        matched.push((0, spec, order, &rule.declarations));
                    }
                }
            }
            for order in author_index.normal.candidates(&el.tag, el_id, &el.classes) {
                let rule = &author.rules[order];
                if rule.applies(self.media) {
                    if let Some(spec) = rule.matches(path) {
                        matched.push((1, spec, order, &rule.declarations));
                    }
                }
            }
            matched.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));

            // Inline `style=` has the highest priority; parse it once and reuse
            // it for both custom-property collection and normal application.
            let inline = node.attr("style").map(parse_declaration_block);

            // Custom properties first (full cascade), so a `var()` in any rule
            // resolves against the winning value regardless of declaration order.
            vars = collect_vars(parent_vars, &matched, inline.as_deref());

            // Two cascade passes: all *normal* declarations first (UA, author,
            // then inline — each already sorted by origin/specificity/order), then
            // all *important* declarations in the same order. Because the important
            // pass runs last and last-wins, any `!important` value overrides every
            // normal one regardless of specificity, and among important values the
            // higher origin/specificity/order still wins (inline `!important`,
            // applied last, tops author `!important`). The UA sheet declares no
            // `!important`, so UA-important is not separately elevated.
            let viewport = (self.media.width as f32, self.media.height as f32);
            let mut pending = PendingHidden::default();
            for (_, _, _, decls) in &matched {
                apply_declarations(
                    &mut style,
                    decls,
                    parent.font_size,
                    root_font_size,
                    &vars,
                    false,
                    viewport,
                    &mut pending,
                );
            }
            if let Some(decls) = &inline {
                apply_declarations(
                    &mut style,
                    decls,
                    parent.font_size,
                    root_font_size,
                    &vars,
                    false,
                    viewport,
                    &mut pending,
                );
            }
            for (_, _, _, decls) in &matched {
                apply_declarations(
                    &mut style,
                    decls,
                    parent.font_size,
                    root_font_size,
                    &vars,
                    true,
                    viewport,
                    &mut pending,
                );
            }
            if let Some(decls) = &inline {
                apply_declarations(
                    &mut style,
                    decls,
                    parent.font_size,
                    root_font_size,
                    &vars,
                    true,
                    viewport,
                    &mut pending,
                );
            }
            // An all-hiding `clip`/`clip-path` (sr-only patterns) folds into
            // `visibility: hidden` once every pass has run — `clip` needs the
            // final `position`, which any of the passes may have set.
            pending.finalize(&mut style);
        }

        // The monospace-size quirk: an unspecified (`medium`) font-size resolves
        // to 13px for the monospace generic and 16px otherwise, matching Chrome —
        // so `<pre>`/`<code>` render smaller than surrounding proportional text.
        // Applied once here, after both font-size and font-family are known, so
        // children inherit the resolved px.
        if style.font_size_medium {
            style.font_size = if style.font_family == GenericFamily::Monospace {
                13
            } else {
                16
            };
        }

        let child_root_font_size = if node.tag() == "html" {
            style.font_size
        } else {
            root_font_size
        };

        // The element children, reduced for sibling / :nth-child matching, shared
        // across this level via `Rc` so the cascade stays O(n).
        let child_siblings: Rc<[SiblingRef]> = node
            .children()
            .filter(|c| c.is_element())
            .map(sibling_ref)
            .collect::<Vec<_>>()
            .into();
        let mut elem_index = 0usize;
        let mut children: Vec<StyledChild> = node
            .children()
            .filter_map(|child| match child.text() {
                Some(t) => Some(StyledChild::Text(t.to_string())),
                // A RAW inline `<svg>` subtree (no synthetic `src` — a path
                // that skipped the app's pre-raster rewrite, e.g. styling a
                // bare document) is pruned here: styling its (often huge)
                // subtree would only leak `<text>`/`<title>` as page content
                // and burn memory. A REWRITTEN svg is childless and carries
                // `src`, so it styles normally and author `svg{…}` tag
                // selectors keep matching it.
                None if child.tag() == "svg" && child.attr("src").is_none() => {
                    elem_index += 1;
                    None
                }
                None => {
                    let styled = self.build(
                        child,
                        child_siblings.clone(),
                        elem_index,
                        &style,
                        &vars,
                        path,
                        author,
                        author_index,
                        child_root_font_size,
                    );
                    elem_index += 1;
                    Some(StyledChild::Element(Box::new(styled)))
                }
            })
            .collect();

        // Generated content (`::before`/`::after`): a matching pseudo rule with
        // a real `content` value synthesizes a styled child at the front/back —
        // the box then flows, paints, and inherits exactly like markup. This is
        // how sites draw icons, decorative bands, and clearfix spacers.
        if !is_root {
            if let Some(b) = self.pseudo_child(
                PseudoElement::Before,
                path,
                author,
                author_index,
                &style,
                &vars,
                child_root_font_size,
                node,
            ) {
                children.insert(0, StyledChild::Element(Box::new(b)));
            }
            if let Some(a) = self.pseudo_child(
                PseudoElement::After,
                path,
                author,
                author_index,
                &style,
                &vars,
                child_root_font_size,
                node,
            ) {
                children.push(StyledChild::Element(Box::new(a)));
            }
        }

        path.pop();
        StyledNode {
            tag: node.tag().to_string(),
            attrs: node.attrs().to_vec(),
            style,
            children,
            node_id: node.id(),
        }
    }

    /// Build the `::before`/`::after` box for the element at `path`, if any
    /// rule targets it with a renderable `content`. The pseudo inherits from
    /// its originating element and its declarations cascade in the usual
    /// (origin, specificity, order) sequence; `content: none|normal` (or no
    /// content declaration at all) generates nothing.
    #[allow(clippy::too_many_arguments)]
    fn pseudo_child(
        &self,
        which: PseudoElement,
        path: &[ElemRef],
        author: &Stylesheet,
        author_index: &SheetIndex,
        elem_style: &ComputedStyle,
        vars: &Vars,
        root_font_size: u32,
        node: NodeRef<'_>,
    ) -> Option<StyledNode> {
        // The originating element (subject of the ::before/::after selector); the
        // pseudo index is keyed by its subject compound, same as the element one.
        let owner = path.last()?;
        let el = &owner.siblings[owner.index];
        let el_id = el.id.as_deref();
        let mut matched: Vec<MatchedRule<'_>> = Vec::new();
        for order in self.ua_index.pseudo.candidates(&el.tag, el_id, &el.classes) {
            let rule = &self.ua.rules[order];
            if rule.applies(self.media) {
                if let Some(spec) = rule.matches_pseudo(path, which) {
                    matched.push((0, spec, order, &rule.declarations));
                }
            }
        }
        for order in author_index.pseudo.candidates(&el.tag, el_id, &el.classes) {
            let rule = &author.rules[order];
            if rule.applies(self.media) {
                if let Some(spec) = rule.matches_pseudo(path, which) {
                    matched.push((1, spec, order, &rule.declarations));
                }
            }
        }
        if matched.is_empty() {
            return None;
        }
        matched.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));

        // The winning `content` (last in cascade order; important simplified to
        // last-wins alongside — content is rarely !important-fought).
        let mut content_raw: Option<String> = None;
        for (_, _, _, decls) in &matched {
            for (prop, value, _) in decls.iter() {
                if prop == "content" {
                    content_raw = Some(value.clone());
                }
            }
        }
        let em = elem_style.font_size as f32;
        let viewport = (self.media.width as f32, self.media.height as f32);
        let resolved = resolve_value(
            &content_raw?,
            vars,
            CalcCtx {
                em,
                vw: viewport.0,
                vh: viewport.1,
                pct_base: Some(em),
            },
        );
        let text = parse_content_value(&resolved, node)?;

        let mut style = elem_style.inherit();
        let mut pending = PendingHidden::default();
        for (_, _, _, decls) in &matched {
            apply_declarations(
                &mut style,
                decls,
                elem_style.font_size,
                root_font_size,
                vars,
                false,
                viewport,
                &mut pending,
            );
        }
        for (_, _, _, decls) in &matched {
            apply_declarations(
                &mut style,
                decls,
                elem_style.font_size,
                root_font_size,
                vars,
                true,
                viewport,
                &mut pending,
            );
        }
        pending.finalize(&mut style);
        if style.display == Display::None {
            return None;
        }
        let children = if text.is_empty() {
            Vec::new()
        } else {
            vec![StyledChild::Text(text)]
        };
        Some(StyledNode {
            tag: match which {
                PseudoElement::Before => "::before".to_string(),
                PseudoElement::After => "::after".to_string(),
            },
            attrs: Vec::new(),
            style,
            children,
            // The generated box belongs to its originating element for
            // hit-testing/dispatch purposes.
            node_id: node.id(),
        })
    }
}

impl Default for CssEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleEngine for CssEngine {
    fn style(&self, doc: &Document) -> StyledDom {
        self.style_with_sheets(doc, &ExternalSheets::new())
    }

    fn style_with_sheets(&self, doc: &Document, sheets: &ExternalSheets) -> StyledDom {
        let mut css = String::new();
        collect_author_css(doc.root(), sheets, &mut css);
        let author = parse_stylesheet(&css);
        let author_index = SheetIndex::build(&author);
        let mut path = Vec::new();
        let root = doc.root();
        let root_siblings: Rc<[SiblingRef]> = vec![sibling_ref(root)].into();
        let no_vars: Vars = Rc::new(HashMap::new());
        let styled = self.build(
            root,
            root_siblings,
            0,
            &ComputedStyle::initial(),
            &no_vars,
            &mut path,
            &author,
            &author_index,
            INITIAL_ROOT_FONT_PX,
        );
        StyledDom {
            root: styled,
            font_face_families: author.font_face_families,
        }
    }
}

fn sibling_ref(node: NodeRef<'_>) -> SiblingRef {
    let mut r = shallow_sibling_ref(node);
    // One level of element children so `:has(...)` can check direct children
    // during the cascade. The children's own lists stay empty — `:has` is a
    // documented direct-child subset, so nothing looks deeper.
    r.children = node
        .children()
        .filter(|c| c.is_element())
        .map(shallow_sibling_ref)
        .collect::<Vec<_>>()
        .into();
    r
}

/// A [`SiblingRef`] without children (the leaf form; `sibling_ref` fills them).
fn shallow_sibling_ref(node: NodeRef<'_>) -> SiblingRef {
    SiblingRef {
        tag: node.tag().to_string(),
        id: node.attr("id").map(str::to_string),
        classes: node
            .attr("class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        attrs: node.attrs().to_vec(),
        children: Rc::from([]),
    }
}

/// Append author CSS in document order: each `<style>` element's text, and each
/// `<link rel="stylesheet">`'s externally-fetched body (from `sheets`) spliced in
/// at the link's position so the cascade order is faithful (ADR-0037).
fn collect_author_css(node: NodeRef<'_>, sheets: &ExternalSheets, out: &mut String) {
    match node.tag() {
        "style" => {
            out.push_str(&node.text_content());
            out.push('\n');
        }
        "link" if link_is_stylesheet(node) => {
            if let Some(css) = node.attr("href").and_then(|href| sheets.get(href)) {
                out.push_str(css);
                out.push('\n');
            }
        }
        _ => {}
    }
    for child in node.children() {
        if child.is_element() {
            collect_author_css(child, sheets, out);
        }
    }
}

/// The `font-family` names declared by the page's `@font-face` rules (inline
/// `<style>` + fetched external sheets), lowercased. The app injects these into
/// the JS realm so `document.fonts.check()` reports a page's own web fonts as
/// available — matching a real browser that loaded them — without ever fetching
/// the bytes (ADR-0005). Available before the full cascade, so it can be injected
/// ahead of page scripts.
pub fn page_font_families(doc: &Document, sheets: &ExternalSheets) -> Vec<String> {
    let mut css = String::new();
    collect_author_css(doc.root(), sheets, &mut css);
    parse_stylesheet(&css).font_face_families
}

/// Whether a `<link>` carries `rel="stylesheet"` (rel is a space-separated,
/// case-insensitive token list).
fn link_is_stylesheet(node: NodeRef<'_>) -> bool {
    node.attr("rel").is_some_and(|rel| {
        rel.split_whitespace()
            .any(|t| t.eq_ignore_ascii_case("stylesheet"))
    })
}

/// Build the custom-property registry in scope for an element: its parent's map
/// plus any `--*` it declares (in cascade order, so later wins). Reuses the
/// parent's `Rc` when the element declares none — the overwhelmingly common case
/// — so only declaring elements pay for a clone (ADR-0035).
fn collect_vars(
    parent: &Vars,
    matched: &[MatchedRule<'_>],
    inline: Option<&[(String, String, bool)]>,
) -> Vars {
    let is_custom = |d: &(String, String, bool)| d.0.starts_with("--");
    let declares = matched.iter().any(|(_, _, _, d)| d.iter().any(is_custom))
        || inline.is_some_and(|d| d.iter().any(is_custom));
    if !declares {
        return parent.clone();
    }
    let mut map = (**parent).clone();
    let mut insert = |decls: &[(String, String, bool)]| {
        for (p, v, _) in decls {
            if p.starts_with("--") {
                // Keys are already lowercased by the parser; store the raw value
                // (its own `var()`s resolve lazily, at use, in `substitute_vars`).
                map.insert(p.clone(), v.trim().to_string());
            }
        }
    };
    for (_, _, _, decls) in matched {
        insert(decls);
    }
    if let Some(decls) = inline {
        insert(decls);
    }
    Rc::new(map)
}

/// Resolve `var()` substitutions and `calc()` math in a raw declaration value,
/// yielding a plain CSS value the existing property parsers can consume. Most
/// values contain neither, so the fast path returns them untouched.
fn resolve_value(value: &str, vars: &Vars, ctx: CalcCtx) -> String {
    let has_var = value.contains("var(");
    let has_math = find_math_fn(value).is_some();
    let has_ld = value.contains("light-dark(");
    if !has_var && !has_math && !has_ld {
        return value.to_string();
    }
    let substituted = if has_var {
        substitute_vars(value, vars, 0)
    } else {
        value.to_string()
    };
    // `light-dark(a, b)` picks by the used color-scheme. The head is a fixed
    // light persona (see the `@media prefers-color-scheme` handling), so it
    // always resolves to the first argument — exactly what Chrome computes here
    // for a light user. Modern design systems (MDN, and any site built with
    // `@csstools/postcss-light-dark-function`) drive their whole theme through
    // this; without it the function was an unknown value, so the declaration
    // was dropped and the theme fell back to its dark branch (measured: MDN's
    // header painted `--color-gray-10` instead of `--color-gray-90`).
    let substituted = if substituted.contains("light-dark(") {
        resolve_light_dark(&substituted, 0)
    } else {
        substituted
    };
    if find_math_fn(&substituted).is_some() {
        eval_calcs(&substituted, ctx)
    } else {
        substituted
    }
}

/// Replace every `light-dark(light, dark)` with its `light` argument (the head's
/// fixed light persona). Paren/quote-aware so the two arguments split at the
/// top-level comma only (an argument may itself be `rgb(…)`, `var(…)` fallback,
/// or a nested `light-dark(…)`, which the chosen argument resolves in turn).
fn resolve_light_dark(input: &str, depth: usize) -> String {
    if depth > 16 {
        return input.to_string();
    }
    let Some(start) = input.find("light-dark(") else {
        return input.to_string();
    };
    let open = start + "light-dark(".len() - 1; // index of '('
    let Some(close) = matching_paren(input, open) else {
        return input.to_string();
    };
    let inner = &input[open + 1..close];
    // First top-level argument (before the first top-level comma).
    let light_arg = split_top_commas(inner)
        .into_iter()
        .next()
        .unwrap_or_default();
    let light_arg = resolve_light_dark(light_arg.trim(), depth + 1);
    let mut out = String::with_capacity(input.len());
    out.push_str(&input[..start]);
    out.push_str(&light_arg);
    out.push_str(&input[close + 1..]);
    // Resolve any further `light-dark(...)` that followed this one.
    resolve_light_dark(&out, depth + 1)
}

/// Index of the `)` matching the `(` at `open`, honoring nested parens and
/// skipping over quoted strings.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Replace the `currentColor` keyword (case-insensitive) with `color` rendered
/// as `rgba(...)`, which every color parser here already understands. CSS
/// resolves `currentColor` to the element's own computed `color`; we use the
/// value cascaded so far — the inherited color unless an earlier declaration in
/// this element set it. Most values never mention it (fast path returns as-is).
fn substitute_current_color(value: &str, color: cerberus_types::Color) -> String {
    const KW: &str = "currentcolor";
    let lower = value.to_ascii_lowercase();
    if !lower.contains(KW) {
        return value.to_string();
    }
    // No spaces inside the function: some property parsers grab the first
    // whitespace-delimited token (e.g. `border-color`), so a spaced `rgba(…)`
    // would be split apart.
    let repl = format!(
        "rgba({},{},{},{})",
        color.r,
        color.g,
        color.b,
        color.a as f32 / 255.0
    );
    // Splice from the ORIGINAL string (not the lowercased copy) so any
    // case-sensitive neighbours — e.g. a `url(Path.png)` in a `background`
    // shorthand — keep their casing.
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while let Some(rel) = lower[i..].find(KW) {
        let start = i + rel;
        out.push_str(&value[i..start]);
        out.push_str(&repl);
        i = start + KW.len();
    }
    out.push_str(&value[i..]);
    out
}

/// The CSS **guaranteed-invalid value** (CSS Variables §3), as a sentinel token
/// that can't occur in real CSS. A `var()` that references an undefined custom
/// property (or one explicitly set to `initial`) with no fallback resolves to
/// this; any value that ends up containing it is *invalid at computed-value
/// time* and is dropped, so a wrapping `var(--x, fallback)` takes its fallback
/// and a real declaration is ignored (keeping the prior cascade value). Treating
/// the case as empty string instead silently kept the wrong branch of the
/// custom-property "light-dark toggle" every modern design system compiles to
/// (measured: MDN's header inverted to its dark palette on a light persona).
const IACVT: &str = "\u{1}iacvt\u{1}";

/// Whether a custom-property value is the guaranteed-invalid value — either the
/// sentinel already, or the literal `initial` (which resets a custom property
/// TO the guaranteed-invalid value, unlike its meaning on normal properties).
fn is_iacvt(v: &str) -> bool {
    let t = v.trim();
    t.contains(IACVT) || t.eq_ignore_ascii_case("initial")
}

/// Replace every `var(--name[, fallback])` in `input` with the custom property's
/// value (resolved recursively, since a custom property may itself reference
/// others). An undefined/`initial` property with no fallback yields the
/// guaranteed-invalid value ([`IACVT`]); a resolved value that turns out invalid
/// makes the reference take its fallback. Guarded against cycles and depth.
fn substitute_vars(input: &str, vars: &Vars, depth: usize) -> String {
    if depth > 32 {
        return String::new();
    }
    let mut out = String::new();
    let mut rest = input;
    while let Some(pos) = rest.find("var(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        let Some((inner, tail)) = take_group(after) else {
            // Unbalanced parens: emit the remainder verbatim and stop.
            out.push_str(&rest[pos..]);
            return out;
        };
        let (name, fallback) = split_top_comma(inner);
        let key = name.trim().to_ascii_lowercase();
        // A property's substituted value, or `None` when it resolves to the
        // guaranteed-invalid value (undefined / `initial` / resolves-to-IACVT).
        let resolved: Option<String> = match vars.get(&key) {
            Some(v) if is_iacvt(v) => None,
            Some(v) => {
                let r = substitute_vars(v, vars, depth + 1);
                if r.contains(IACVT) {
                    None
                } else {
                    Some(r)
                }
            }
            None => None,
        };
        let replacement = match resolved {
            Some(r) => r,
            // Invalid reference: take the fallback (itself possibly invalid), or
            // propagate the guaranteed-invalid value up.
            None => match fallback {
                Some(fb) => {
                    let r = substitute_vars(fb.trim(), vars, depth + 1);
                    if r.contains(IACVT) {
                        IACVT.to_string()
                    } else {
                        r
                    }
                }
                None => IACVT.to_string(),
            },
        };
        out.push_str(replacement.trim());
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Given the text just after an opening `(`, return `(inner, tail)` where `inner`
/// is the balanced group's contents and `tail` is what follows the matching `)`.
fn take_group(after: &str) -> Option<(&str, &str)> {
    let mut depth = 1;
    for (i, c) in after.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&after[..i], &after[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Split `var()` arguments at the first top-level comma: `(name, fallback?)`.
/// Depth is clamped at zero so an unbalanced `)` can't drive it negative and
/// miss the separating comma.
fn split_top_comma(s: &str) -> (&str, Option<&str>) {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => return (&s[..i], Some(&s[i + 1..])),
            _ => {}
        }
    }
    (s, None)
}

/// The math functions the value resolver evaluates.
const MATH_FNS: [&str; 4] = ["calc(", "min(", "max(", "clamp("];

/// Find the earliest math-function call in `s`, at a word boundary so a
/// substring like the `max(` inside `minmax(...)` is never mistaken for one.
/// Returns `(byte offset, matched name incl. '(')`.
fn find_math_fn(s: &str) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for name in MATH_FNS {
        let mut from = 0;
        while let Some(rel) = s[from..].find(name) {
            let pos = from + rel;
            let bounded = pos == 0 || {
                let prev = s.as_bytes()[pos - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_')
            };
            if bounded {
                if best.is_none_or(|(b, _)| pos < b) {
                    best = Some((pos, name));
                }
                break;
            }
            from = pos + 1;
        }
    }
    best
}

/// The bases a `calc()`/`min()`/`max()`/`clamp()` expression resolves its
/// relative units against.
#[derive(Clone, Copy)]
struct CalcCtx {
    /// The element's font size in px (the `em` base).
    em: f32,
    /// Viewport width/height in px (the `vw`/`vh`/`vmin`/`vmax` bases).
    vw: f32,
    vh: f32,
    /// How `%` resolves for the property being parsed. `Some(base)` folds it to
    /// px against `base` — correct only for the font-relative properties
    /// (`font-size`/`line-height`/`vertical-align`). `None` keeps `%` symbolic:
    /// the expression reduces to the canonical `a% + b·px`, and only a pure
    /// form (`a == 0` or `b == 0`) resolves. A mixed result (e.g.
    /// `calc(100% - 32px)` for a width) is left unresolved so the declaration
    /// falls back to the prior/initial value instead of mis-resolving `%`
    /// against the font size — Chrome resolves such `%` against the containing
    /// block, which isn't known until layout, and `Len` has no combined
    /// `%+px` variant (layout/taffy match on `Len` exhaustively, and
    /// cerberus-layout must not be edited here).
    pct_base: Option<f32>,
}

/// A partially-evaluated calc value in the canonical linear form `px + pct%`
/// (`%` can't be folded into px without the containing block).
#[derive(Clone, Copy, PartialEq, Debug)]
struct CalcVal {
    px: f32,
    pct: f32,
}

impl CalcVal {
    fn add(self, o: Self) -> Self {
        CalcVal {
            px: self.px + o.px,
            pct: self.pct + o.pct,
        }
    }

    fn sub(self, o: Self) -> Self {
        CalcVal {
            px: self.px - o.px,
            pct: self.pct - o.pct,
        }
    }

    /// Multiply; one side must be a plain number (no `%` component — unitless
    /// numbers tokenize with their value in `px`, matching the old behavior).
    fn mul(self, o: Self) -> Option<Self> {
        if o.pct == 0.0 {
            Some(CalcVal {
                px: self.px * o.px,
                pct: self.pct * o.px,
            })
        } else if self.pct == 0.0 {
            Some(CalcVal {
                px: o.px * self.px,
                pct: o.pct * self.px,
            })
        } else {
            None
        }
    }

    /// Divide by a plain non-zero number.
    fn div(self, o: Self) -> Option<Self> {
        (o.pct == 0.0 && o.px != 0.0).then(|| CalcVal {
            px: self.px / o.px,
            pct: self.pct / o.px,
        })
    }
}

/// Print a resolved calc result as a plain CSS value: `px` when there is no
/// `%` component, `N%` when it is purely a percentage (the property parsers
/// then treat it exactly like a literal percentage), `None` when mixed.
fn format_calc_val(v: CalcVal) -> Option<String> {
    // Integer-ish results print without a trailing `.0`.
    let fmt = |n: f32, unit: &str| {
        if (n.round() - n).abs() < 1e-4 {
            format!("{}{unit}", n.round() as i64)
        } else {
            format!("{n}{unit}")
        }
    };
    if v.pct == 0.0 {
        Some(fmt(v.px, "px"))
    } else if v.px == 0.0 {
        Some(fmt(v.pct, "%"))
    } else {
        None
    }
}

/// Replace every math function ([`MATH_FNS`]) in `input` with its evaluated
/// length/percentage; leave one we cannot evaluate untouched so the
/// declaration fails to parse and falls back.
fn eval_calcs(input: &str, ctx: CalcCtx) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some((pos, name)) = find_math_fn(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + name.len()..];
        let Some((inner, tail)) = take_group(after) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        // Nested math inside this group resolves first.
        let inner = eval_calcs(inner, ctx);
        let val = if name == "calc(" {
            eval_calc_expr(&inner, ctx)
        } else {
            eval_min_max_clamp(name, &inner, ctx)
        };
        match val.and_then(format_calc_val) {
            Some(s) => out.push_str(&s),
            None => {
                out.push_str(name);
                out.push_str(&inner);
                out.push(')');
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Evaluate `min(...)`/`max(...)`/`clamp(lo, mid, hi)` over comma-separated
/// calc expressions. The arguments are only mutually comparable without a
/// containing block when they are all pure px or all pure `%`; anything mixed
/// (`min(100%, 500px)`) is left unresolved — the same safety rule as `calc()`
/// itself (FIX 1), so `%` is never compared against a font-size-derived px.
fn eval_min_max_clamp(name: &str, inner: &str, ctx: CalcCtx) -> Option<CalcVal> {
    let args: Vec<CalcVal> = split_top_commas(inner)
        .iter()
        .map(|a| eval_calc_expr(a, ctx))
        .collect::<Option<_>>()?;
    let all_px = args.iter().all(|a| a.pct == 0.0);
    let all_pct = args.iter().all(|a| a.px == 0.0);
    if args.is_empty() || !(all_px || all_pct) {
        return None;
    }
    // `%` compares monotonically because its (containing-block) base is ≥ 0.
    let key = |a: &CalcVal| if all_px { a.px } else { a.pct };
    let n = match name {
        "min(" => args.iter().map(key).fold(f32::INFINITY, f32::min),
        "max(" => args.iter().map(key).fold(f32::NEG_INFINITY, f32::max),
        // clamp(lo, mid, hi) = max(lo, min(mid, hi)); exactly three arguments.
        "clamp(" if args.len() == 3 => key(&args[0]).max(key(&args[1]).min(key(&args[2]))),
        _ => return None,
    };
    Some(if all_px {
        CalcVal { px: n, pct: 0.0 }
    } else {
        CalcVal { px: 0.0, pct: n }
    })
}

/// Evaluate a `calc()` expression body to the canonical `px + %` form,
/// supporting `+ - * /`, parentheses, and px/em/rem/pt/vw/vh/vmin/vmax/%
/// units. Returns `None` if it cannot be reduced.
fn eval_calc_expr(expr: &str, ctx: CalcCtx) -> Option<CalcVal> {
    let tokens = tokenize_calc(expr, ctx)?;
    let mut p = CalcParser {
        tokens: &tokens,
        i: 0,
    };
    let v = p.expr()?;
    if p.i == p.tokens.len() {
        Some(v)
    } else {
        None
    }
}

/// A `calc()` token: a resolved value or an operator/paren.
#[derive(Clone, Copy, PartialEq)]
enum CalcTok {
    Num(CalcVal),
    Plus,
    Minus,
    Mul,
    Div,
    Open,
    Close,
}

fn tokenize_calc(expr: &str, ctx: CalcCtx) -> Option<Vec<CalcTok>> {
    let mut toks = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match c {
            b'+' => {
                toks.push(CalcTok::Plus);
                i += 1;
            }
            b'-' if !at_number_start(&toks) => {
                toks.push(CalcTok::Minus);
                i += 1;
            }
            b'*' => {
                toks.push(CalcTok::Mul);
                i += 1;
            }
            b'/' => {
                toks.push(CalcTok::Div);
                i += 1;
            }
            b'(' => {
                toks.push(CalcTok::Open);
                i += 1;
            }
            b')' => {
                toks.push(CalcTok::Close);
                i += 1;
            }
            _ => {
                // A number with an optional unit (a leading '-' is a sign here).
                let start = i;
                if bytes[i] == b'-' || bytes[i] == b'+' {
                    i += 1;
                }
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let num: f32 = expr[start..i].parse().ok()?;
                let unit_start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'%') {
                    i += 1;
                }
                let unit = &expr[unit_start..i];
                let px = |px: f32| CalcVal { px, pct: 0.0 };
                let val = match unit.to_ascii_lowercase().as_str() {
                    "" | "px" => px(num),
                    "em" => px(num * ctx.em),
                    "rem" => px(num * 16.0),
                    "pt" => px(num * 96.0 / 72.0),
                    "vw" => px(num / 100.0 * ctx.vw),
                    "vh" => px(num / 100.0 * ctx.vh),
                    "vmin" => px(num / 100.0 * ctx.vw.min(ctx.vh)),
                    "vmax" => px(num / 100.0 * ctx.vw.max(ctx.vh)),
                    // See `CalcCtx::pct_base`: fold against the font-relative
                    // base, or keep the `%` symbolic for later reduction.
                    "%" => match ctx.pct_base {
                        Some(base) => px(num / 100.0 * base),
                        None => CalcVal { px: 0.0, pct: num },
                    },
                    _ => return None,
                };
                toks.push(CalcTok::Num(val));
            }
        }
    }
    Some(toks)
}

/// Whether the next token would start a number (so a `-` is a sign, not minus).
fn at_number_start(toks: &[CalcTok]) -> bool {
    !matches!(toks.last(), Some(CalcTok::Num(_)) | Some(CalcTok::Close))
}

struct CalcParser<'a> {
    tokens: &'a [CalcTok],
    i: usize,
}

impl CalcParser<'_> {
    fn peek(&self) -> Option<CalcTok> {
        self.tokens.get(self.i).copied()
    }

    fn expr(&mut self) -> Option<CalcVal> {
        let mut v = self.term()?;
        while let Some(op @ (CalcTok::Plus | CalcTok::Minus)) = self.peek() {
            self.i += 1;
            let rhs = self.term()?;
            v = if op == CalcTok::Plus {
                v.add(rhs)
            } else {
                v.sub(rhs)
            };
        }
        Some(v)
    }

    fn term(&mut self) -> Option<CalcVal> {
        let mut v = self.factor()?;
        while let Some(op @ (CalcTok::Mul | CalcTok::Div)) = self.peek() {
            self.i += 1;
            let rhs = self.factor()?;
            v = if op == CalcTok::Mul {
                v.mul(rhs)?
            } else {
                v.div(rhs)?
            };
        }
        Some(v)
    }

    fn factor(&mut self) -> Option<CalcVal> {
        match self.peek()? {
            CalcTok::Num(n) => {
                self.i += 1;
                Some(n)
            }
            CalcTok::Open => {
                self.i += 1;
                let v = self.expr()?;
                matches!(self.peek(), Some(CalcTok::Close)).then(|| self.i += 1)?;
                Some(v)
            }
            _ => None,
        }
    }
}

/// Initial root font-size in px (the base for `rem` before any `html {font-size}`).
const INITIAL_ROOT_FONT_PX: u32 = 16;

/// Resolve a `content` value to the text the generated box carries:
/// `none`/`normal` → no box (`None`); quoted strings concatenate; `attr(x)`
/// reads the originating element; `counter()`/`url()`/unknown functions
/// contribute nothing but still allow an (empty) box, which is how decorative
/// bands and clearfix spacers render.
fn parse_content_value(v: &str, node: NodeRef<'_>) -> Option<String> {
    let t = v.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("normal") {
        return None;
    }
    let mut out = String::new();
    let mut rest = t;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let c = rest.chars().next().unwrap();
        if c == '"' || c == '\'' {
            // A quoted string piece (no escape handling — content strings on
            // real pages are plain glyph runs).
            if let Some(end) = rest[1..].find(c) {
                out.push_str(&rest[1..1 + end]);
                rest = &rest[end + 2..];
                continue;
            }
            break;
        }
        // A function or keyword token up to whitespace (tracking parens so
        // `attr(data-x)` stays one token).
        let mut depth = 0i32;
        let mut split = rest.len();
        for (i, ch) in rest.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ch if ch.is_whitespace() && depth == 0 => {
                    split = i;
                    break;
                }
                _ => {}
            }
        }
        let tok = &rest[..split];
        rest = &rest[split..];
        let low = tok.to_ascii_lowercase();
        if let Some(name) = low.strip_prefix("attr(").and_then(|x| x.strip_suffix(')')) {
            if let Some(val) = node.attr(name.trim()) {
                out.push_str(val);
            }
        }
        // counter()/url()/open-quote/… contribute nothing (box still forms).
    }
    Some(out)
}

/// Cross-declaration state accumulated over one element's cascade passes and
/// finalized in `build` after all of them: whether the last `clip` /
/// `clip-path` declaration collapses the element to nothing (the `sr-only`
/// accessibility-hiding patterns). Deferred because `clip` only applies to
/// absolutely positioned boxes and `position` may be declared in any rule /
/// order relative to the `clip` (alphabetized blocks put `clip` first).
#[derive(Default)]
struct PendingHidden {
    /// `clip: rect(...)` left an empty visible region.
    clip: bool,
    /// `clip-path: inset(...)` insets away the whole box.
    clip_path: bool,
}

impl PendingHidden {
    /// Fold into the computed style: an all-hiding clip behaves like
    /// `visibility: hidden` (laid out, not painted, inherited by children) —
    /// reusing that mechanism means layout/paint need no changes. Real
    /// (partial) clipping is not modeled; only the invisible case applies.
    fn finalize(&self, style: &mut ComputedStyle) {
        let clip_applies = matches!(style.position, Position::Absolute | Position::Fixed);
        if self.clip_path || (self.clip && clip_applies) {
            style.visibility = Visibility::Hidden;
        }
    }
}

// The cascade threads per-element bases (parent/root font size, viewport) that
// vary per call; bundling them would not aid readability (same as `build`).
#[allow(clippy::too_many_arguments)]
fn apply_declarations(
    style: &mut ComputedStyle,
    decls: &[(String, String, bool)],
    parent_font_size: u32,
    root_font_size: u32,
    vars: &Vars,
    important: bool,
    viewport: (f32, f32),
    pending: &mut PendingHidden,
) {
    for (prop, value, is_important) in decls {
        // This pass only applies declarations of the matching importance, so the
        // caller can run all normal declarations before all important ones.
        if *is_important != important {
            continue;
        }
        // Custom properties are collected separately (`collect_vars`); they do
        // not set any computed value here.
        if prop.starts_with("--") {
            continue;
        }
        // Fold `rem` to px against the root font-size up front (`em` stays for
        // per-element resolution), so downstream length parsers see px.
        let value = &substitute_rem(value, root_font_size as f32);
        // Resolve `var()` references and calc()/min()/max()/clamp() math before
        // parsing the value. `em` uses the element's current font size; `%` only
        // folds to px for the font-relative properties (see `CalcCtx`).
        let ctx = CalcCtx {
            em: style.font_size as f32,
            vw: viewport.0,
            vh: viewport.1,
            pct_base: matches!(
                prop.as_str(),
                "font-size" | "line-height" | "vertical-align"
            )
            .then_some(style.font_size as f32),
        };
        let resolved = resolve_value(value, vars, ctx);
        // A value that resolved to the guaranteed-invalid value (an undefined /
        // `initial` custom property reached through `var()` with no usable
        // fallback) is invalid at computed-value time — drop the declaration so
        // the property keeps its prior cascade value, exactly as a browser does.
        if resolved.contains(IACVT) {
            continue;
        }
        // Then resolve the `currentColor` keyword against the color cascaded so
        // far, so it works anywhere a color appears (borders, backgrounds,
        // shadows, gradients) — not just as an unresolved literal.
        let resolved = substitute_current_color(&resolved, style.color);
        let v = resolved.trim();
        match prop.as_str() {
            "color" => {
                if let Some(c) = parse_color(v) {
                    style.color = c;
                }
            }
            // SVG paint: computed here so the app can inject it into the
            // pre-rasterized inline-svg payload (resvg never sees author CSS).
            "fill" => {
                let lv = v.to_ascii_lowercase();
                if lv == "currentcolor" {
                    style.fill = Some(style.color);
                } else if lv == "none" {
                    style.fill = None;
                } else if let Some(c) = parse_color(v) {
                    style.fill = Some(c);
                }
            }
            "background" | "background-color" => {
                if prop == "background" {
                    // The shorthand resets *every* longhand it can set to its
                    // initial value, then applies what the value specifies — so
                    // `background: url(x)` / `background: none` (neither parses
                    // as a color) clears a previously-cascaded color to
                    // transparent (`None`), not just the image/gradient. Color,
                    // image, gradient, position, and size are all recomputed
                    // from the value here (ADR-0044/0045).
                    style.background = parse_bg_color(v).filter(|c| c.a != 0);
                    style.background_image = parse_url_value(v);
                    style.background_gradient = parse_gradient(v).map(Box::new);
                    style.background_position = ImagePos::TOP_LEFT;
                    style.background_position_px = Point::ZERO;
                    style.background_size = ImageFit::Auto;
                    apply_background_shorthand_geometry(style, v);
                } else if let Some(c) = parse_bg_color(v) {
                    // Standalone `background-color` longhand: additive — only
                    // overwrite when the value parses, so an unparseable value
                    // leaves a prior color intact.
                    style.background = (c.a != 0).then_some(c);
                }
            }
            "background-image" => {
                style.background_image = parse_url_value(v);
                style.background_gradient = parse_gradient(v).map(Box::new);
            }
            // `cover`/`contain` are the only keywords that change scaling; explicit
            // sizes and `auto`/`100%` fall through to `Fill` (stretch) — ADR-0044.
            "object-fit" => style.object_fit = parse_image_fit(v),
            "background-size" => style.background_size = parse_image_fit(v),
            // `object-position`/`background-position` (keywords + percentages;
            // lengths are ignored — they only matter for sprites we don't tile) —
            // ADR-0045.
            "object-position" => {
                if let Some(p) = parse_image_pos(v) {
                    style.object_position = p;
                }
            }
            "background-position" => {
                if let Some(p) = parse_image_pos(v) {
                    style.background_position = p;
                }
                // Length components (the `-304px` in `0 -304px`) crop CSS sprites;
                // percentages/keywords are folded into the fraction above.
                style.background_position_px = parse_bg_position_px(v, style.font_size as f32);
            }
            "border-radius" => {
                // Uniform radius (first value of the 1–4 corner shorthand).
                style.border_radius = v
                    .split_whitespace()
                    .next()
                    .and_then(|t| parse_len(t, style.font_size as f32))
                    .map(|n| n.clamp(0, u16::MAX as i32) as u16)
                    .unwrap_or(0);
            }
            "box-shadow" => {
                style.box_shadow = parse_box_shadow(v, style.font_size as f32).map(Box::new);
            }
            "font-size" => {
                if let Some(px) = parse_size(v, parent_font_size) {
                    style.font_size = px;
                    // Track whether the value is still the initial `medium` keyword
                    // (vs an explicit length/keyword), for the monospace-size quirk
                    // resolved post-cascade.
                    style.font_size_medium = v.trim().eq_ignore_ascii_case("medium");
                }
            }
            "font-weight" => style.font.bold = is_bold(v),
            "font-style" => {
                let low = v.to_ascii_lowercase();
                style.font.italic = low == "italic" || low == "oblique";
            }
            "font" => apply_font_shorthand(style, v, parent_font_size),
            // `font-family` resolves to a generic class (serif / sans-serif /
            // monospace / …), which selects one of the bundled metric-compatible
            // faces at rasterization. The *named* fonts are never shipped or read
            // (a privacy/anti-fingerprinting property — no system or downloadable
            // fonts), so e.g. `Georgia, serif` and `Consolas, monospace` render in
            // the bundled serif/mono face rather than the literal named font.
            "font-family" => {
                if let Some(g) = parse_font_family(v) {
                    style.font_family = g;
                }
            }
            "text-align" => {
                style.text_align = match v.to_ascii_lowercase().as_str() {
                    "center" => TextAlign::Center,
                    "-webkit-center" | "-moz-center" => TextAlign::WebkitCenter,
                    "right" | "end" => TextAlign::Right,
                    "left" | "start" => TextAlign::Left,
                    _ => style.text_align,
                }
            }
            "line-height" => style.line_height = parse_line_height(v, style.font_size),
            "text-transform" => {
                style.text_transform = match v.trim().to_ascii_lowercase().as_str() {
                    "uppercase" => TextTransform::Uppercase,
                    "lowercase" => TextTransform::Lowercase,
                    "capitalize" => TextTransform::Capitalize,
                    _ => TextTransform::None,
                }
            }
            "letter-spacing" => {
                if v.trim().eq_ignore_ascii_case("normal") {
                    style.letter_spacing = 0;
                } else if let Some(px) = parse_len(v, style.font_size as f32) {
                    style.letter_spacing = px;
                }
            }
            "word-spacing" => {
                if v.trim().eq_ignore_ascii_case("normal") {
                    style.word_spacing = 0;
                } else if let Some(px) = parse_len(v, style.font_size as f32) {
                    style.word_spacing = px;
                }
            }
            "text-indent" => {
                if let Some(px) = parse_len(v, style.font_size as f32) {
                    style.text_indent = px;
                }
            }
            "vertical-align" => {
                style.vertical_align = match v.trim().to_ascii_lowercase().as_str() {
                    "sub" => VerticalAlign::Sub,
                    "super" => VerticalAlign::Super,
                    "top" | "middle" | "bottom" | "text-top" | "text-bottom" => {
                        VerticalAlign::OffBaseline
                    }
                    _ => VerticalAlign::Baseline,
                };
            }
            "list-style-type" => {
                if let Some(t) = parse_list_style_type(v) {
                    style.list_style_type = t;
                }
            }
            "list-style" => {
                // Shorthand: we model only the `type` component; scan the tokens
                // for a recognized keyword (ignoring position/image parts).
                for tok in v.split_whitespace() {
                    if let Some(t) = parse_list_style_type(tok) {
                        style.list_style_type = t;
                        break;
                    }
                }
            }
            "text-decoration" | "text-decoration-line" => {
                let low = v.to_ascii_lowercase();
                // `none` clears both lines; otherwise each named line is applied
                // independently (a shorthand may list several, e.g.
                // `underline line-through`).
                if low.contains("none") {
                    style.underline = false;
                    style.line_through = false;
                } else {
                    if low.contains("underline") {
                        style.underline = true;
                    }
                    if low.contains("line-through") {
                        style.line_through = true;
                    }
                }
            }
            "display" => {
                if let Some(d) = parse_display(v) {
                    style.display = d;
                    // inline-flex / inline-grid: Flex/Grid INSIDE, atomic
                    // inline OUTSIDE (block-level promotion broke the
                    // surrounding line, putting each such box on its own row).
                    style.display_inline_level = matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "inline-flex" | "inline-grid"
                    );
                }
            }
            "margin" => apply_margin_shorthand(style, v, style.font_size as f32),
            "margin-top" => {
                if let Some(m) = parse_inset(v, style.font_size as f32) {
                    style.margin_top = m;
                }
            }
            "margin-bottom" => {
                if let Some(m) = parse_inset(v, style.font_size as f32) {
                    style.margin_bottom = m;
                }
            }
            "margin-left" => {
                if let Some(m) = parse_inset(v, style.font_size as f32) {
                    style.margin_left_auto = m == Len::Auto;
                    style.margin_left = m;
                }
            }
            "margin-right" => {
                if let Some(m) = parse_inset(v, style.font_size as f32) {
                    style.margin_right_auto = m == Len::Auto;
                    style.margin_right = m;
                }
            }
            "width" => style.width = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto),
            "max-width" => {
                style.max_width = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto)
            }
            "min-width" => {
                style.min_width = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto)
            }
            "height" => style.height = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto),
            "max-height" => {
                style.max_height = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto)
            }
            "min-height" => {
                style.min_height = parse_inset(v, style.font_size as f32).unwrap_or(Len::Auto)
            }
            "float" => {
                style.float = match v.trim().to_ascii_lowercase().as_str() {
                    "left" => Float::Left,
                    "right" => Float::Right,
                    _ => Float::None,
                }
            }
            "clear" => {
                style.clear = match v.trim().to_ascii_lowercase().as_str() {
                    "left" => Clear::Left,
                    "right" => Clear::Right,
                    "both" => Clear::Both,
                    _ => Clear::None,
                }
            }
            "box-sizing" => {
                style.box_sizing = if v.trim().eq_ignore_ascii_case("border-box") {
                    BoxSizing::BorderBox
                } else {
                    BoxSizing::ContentBox
                }
            }
            "overflow" | "overflow-x" | "overflow-y" => {
                // hidden/clip/scroll/auto all clip (we don't scroll); visible doesn't.
                if matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "hidden" | "clip" | "scroll" | "auto"
                ) {
                    style.overflow_clip = true;
                } else if v.trim().eq_ignore_ascii_case("visible") {
                    style.overflow_clip = false;
                }
            }
            "text-overflow" => {
                // `ellipsis` truncates a clipped non-wrapping line with `…`;
                // `clip` (the default) hard-cuts.
                style.text_overflow_ellipsis = v.trim().eq_ignore_ascii_case("ellipsis");
            }
            "padding" => apply_box_shorthand(
                v,
                style.font_size as f32,
                &mut [
                    &mut style.padding_top,
                    &mut style.padding_right,
                    &mut style.padding_bottom,
                    &mut style.padding_left,
                ],
            ),
            "padding-top" => set_len(&mut style.padding_top, v, style.font_size as f32),
            "padding-right" => set_len(&mut style.padding_right, v, style.font_size as f32),
            "padding-bottom" => set_len(&mut style.padding_bottom, v, style.font_size as f32),
            "padding-left" => set_len(&mut style.padding_left, v, style.font_size as f32),
            "border" => apply_border(style, v, [true, true, true, true]),
            "border-top" => apply_border(style, v, [true, false, false, false]),
            "border-right" => apply_border(style, v, [false, true, false, false]),
            "border-bottom" => apply_border(style, v, [false, false, true, false]),
            "border-left" => apply_border(style, v, [false, false, false, true]),
            "border-width" => apply_box_shorthand(
                v,
                style.font_size as f32,
                &mut [
                    &mut style.border_top,
                    &mut style.border_right,
                    &mut style.border_bottom,
                    &mut style.border_left,
                ],
            ),
            "border-color" => {
                if let Some(c) = parse_color(v.split_whitespace().next().unwrap_or(v)) {
                    style.border_color = c;
                }
            }
            "border-style" => {
                // `none`/`hidden` removes the border; any other style keeps width.
                if matches!(v.trim().to_ascii_lowercase().as_str(), "none" | "hidden") {
                    style.border_top = 0;
                    style.border_right = 0;
                    style.border_bottom = 0;
                    style.border_left = 0;
                }
            }
            "border-top-width" => set_len(&mut style.border_top, v, style.font_size as f32),
            "border-right-width" => set_len(&mut style.border_right, v, style.font_size as f32),
            "border-bottom-width" => set_len(&mut style.border_bottom, v, style.font_size as f32),
            "border-left-width" => set_len(&mut style.border_left, v, style.font_size as f32),
            "white-space" => {
                style.white_space = match v.trim().to_ascii_lowercase().as_str() {
                    "pre" => WhiteSpace::Pre,
                    "pre-wrap" => WhiteSpace::PreWrap,
                    "pre-line" => WhiteSpace::PreLine,
                    "nowrap" => WhiteSpace::Nowrap,
                    _ => WhiteSpace::Normal,
                };
            }
            "visibility" => {
                style.visibility = match v.to_ascii_lowercase().as_str() {
                    "hidden" | "collapse" => Visibility::Hidden,
                    "visible" => Visibility::Visible,
                    _ => style.visibility,
                }
            }
            // Accessibility hiding: only the *all-hiding* clip forms are
            // modeled (finalized via `PendingHidden` after the cascade); a
            // partially-clipping value is treated as visible — we never
            // attempt real clipping. The last declaration wins, so a visible
            // `auto`/`none`/partial value overrides an earlier hiding one.
            "clip" => pending.clip = clip_rect_hides(v, style.font_size as f32),
            "clip-path" => {
                pending.clip_path = clip_path_inset_hides(v);
                // A `polygon(...)` clip paints the background as that shape (an
                // angled/stepped divider); any other value clears it.
                style.clip_polygon = parse_clip_polygon(v, style.font_size as f32);
            }
            "opacity" => {
                if let Some(o) = parse_opacity(v) {
                    style.opacity = o;
                }
            }
            "position" => {
                style.position = match v.trim().to_ascii_lowercase().as_str() {
                    "relative" => Position::Relative,
                    "absolute" => Position::Absolute,
                    "fixed" => Position::Fixed,
                    "sticky" => Position::Sticky,
                    "static" => Position::Static,
                    _ => style.position,
                }
            }
            "top" => {
                if let Some(l) = parse_inset(v, style.font_size as f32) {
                    style.inset_top = l;
                }
            }
            "right" => {
                if let Some(l) = parse_inset(v, style.font_size as f32) {
                    style.inset_right = l;
                }
            }
            "bottom" => {
                if let Some(l) = parse_inset(v, style.font_size as f32) {
                    style.inset_bottom = l;
                }
            }
            "left" => {
                if let Some(l) = parse_inset(v, style.font_size as f32) {
                    style.inset_left = l;
                }
            }
            "inset" => apply_inset_shorthand(style, v, style.font_size as f32),
            "z-index" => {
                let t = v.trim().to_ascii_lowercase();
                if t == "auto" {
                    style.z_index = None;
                } else if let Ok(n) = t.parse::<i32>() {
                    style.z_index = Some(n);
                }
            }
            "flex-direction" => {
                let low = v.to_ascii_lowercase();
                style.flex_direction = match low.as_str() {
                    "column" | "column-reverse" => FlexDirection::Column,
                    _ => FlexDirection::Row,
                };
                style.flex_reverse = low.ends_with("-reverse");
            }
            "flex-wrap" => style.flex_wrap = v.to_ascii_lowercase().starts_with("wrap"),
            "justify-content" => {
                style.justify_content = match v.to_ascii_lowercase().as_str() {
                    "center" => JustifyContent::Center,
                    "flex-end" | "end" | "right" => JustifyContent::End,
                    "space-between" => JustifyContent::SpaceBetween,
                    "space-around" => JustifyContent::SpaceAround,
                    "space-evenly" => JustifyContent::SpaceEvenly,
                    _ => JustifyContent::Start,
                }
            }
            "align-items" => {
                style.align_items = match v.to_ascii_lowercase().as_str() {
                    "center" => AlignItems::Center,
                    "flex-end" | "end" => AlignItems::End,
                    "flex-start" | "start" => AlignItems::Start,
                    _ => AlignItems::Stretch,
                }
            }
            "align-self" => {
                style.align_self = match v.to_ascii_lowercase().as_str() {
                    "center" => AlignSelf::Center,
                    "flex-end" | "end" => AlignSelf::End,
                    "flex-start" | "start" => AlignSelf::Start,
                    "stretch" => AlignSelf::Stretch,
                    _ => AlignSelf::Auto,
                }
            }
            "flex" => apply_flex_shorthand(style, v, style.font_size as f32),
            "flex-grow" => {
                if let Ok(n) = v.trim().parse::<f32>() {
                    style.flex_grow = n.max(0.0);
                }
            }
            "flex-shrink" => {
                if let Ok(n) = v.trim().parse::<f32>() {
                    style.flex_shrink = n.max(0.0);
                }
            }
            "flex-basis" => style.flex_basis = parse_flex_basis(v, style.font_size as f32),
            "order" => {
                if let Ok(n) = v.trim().parse::<i32>() {
                    style.order = n;
                }
            }
            "gap" | "grid-gap" | "column-gap" | "row-gap" => {
                if let Some(g) = parse_len(v, style.font_size as f32) {
                    style.gap = g.max(0) as u32;
                }
            }
            "grid-template-columns" => {
                let (tracks, auto_fill) = parse_grid_template(v, style.font_size as f32);
                style.grid_template_columns = tracks;
                style.grid_auto_fill = auto_fill;
                // A named-line template (full-bleed centering pattern) is collapsed
                // to one full-width column at layout (we don't resolve named lines).
                style.grid_cols_named = auto_fill.is_none() && v.contains('[');
            }
            "grid-template-rows" => {
                style.grid_template_rows = parse_tracks(v, style.font_size as f32);
            }
            "grid-auto-rows" => {
                style.grid_auto_rows = split_top(v.trim())
                    .first()
                    .map(|t| parse_one_track(t, style.font_size as f32));
            }
            "grid-column" => {
                let (start, end, span) = parse_grid_placement(v);
                style.grid_column_start = start;
                style.grid_column_end = end;
                style.grid_column_span = span;
                if grid_line_is_named(v) {
                    style.grid_named_place = true;
                }
            }
            "grid-row" => {
                let (start, end, span) = parse_grid_placement(v);
                style.grid_row_start = start;
                style.grid_row_end = end;
                style.grid_row_span = span;
            }
            "grid-area" => {
                // `grid-area: name` (a named area/line) — we don't resolve areas,
                // so flag it for content-track placement (ADR-0038).
                if grid_line_is_named(v) {
                    style.grid_named_place = true;
                }
            }
            // Still ignored (no compositor/timeline): animation*, transition*,
            // transform, and everything else.
            _ => {}
        }
    }
}

/// Whether a `clip` value is a `rect(...)` that leaves (essentially) nothing
/// visible: all four edges parse as lengths ≤ 1px — the screen-reader-only
/// patterns `rect(0, 0, 0, 0)` and `rect(1px, 1px, 1px, 1px)` (the visible
/// region is `left..right × top..bottom`, so those are empty). `auto` edges or
/// larger rects leave content visible and return false.
fn clip_rect_hides(v: &str, em: f32) -> bool {
    let t = v.trim().to_ascii_lowercase();
    let Some(inner) = t.strip_prefix("rect(").and_then(|s| s.strip_suffix(')')) else {
        return false;
    };
    // Both the comma and the legacy space-separated forms are valid.
    let edges: Vec<&str> = inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    edges.len() == 4
        && edges
            .iter()
            .all(|e| parse_css_px(e, em).is_some_and(|px| px <= 1.0))
}

/// Whether a `clip-path` value is an `inset(...)` that insets away the whole
/// box: opposite percentage insets summing to ≥ 100% on either axis (e.g. the
/// sr-only `inset(50%)`, or `inset(100%)`). Pixel insets can't be judged
/// without the box size and count as 0 (conservative: never hide unless the
/// percentages alone guarantee it) — Chrome shows `inset(0 0 50% 0)` half-
/// visible, and so do we (visible).
fn clip_path_inset_hides(v: &str) -> bool {
    let t = v.trim().to_ascii_lowercase();
    let Some(inner) = t.strip_prefix("inset(").and_then(|s| s.strip_suffix(')')) else {
        return false;
    };
    // 1–4 values (top/right/bottom/left, CSS order), before any `round` radius.
    let vals: Vec<&str> = inner
        .split_whitespace()
        .take_while(|tok| *tok != "round")
        .collect();
    let pct = |i: usize| -> f32 {
        vals.get(i)
            .and_then(|tok| tok.strip_suffix('%'))
            .and_then(|n| n.trim().parse::<f32>().ok())
            .unwrap_or(0.0)
    };
    let (top, right, bottom, left) = match vals.len() {
        1 => (pct(0), pct(0), pct(0), pct(0)),
        2 => (pct(0), pct(1), pct(0), pct(1)),
        3 => (pct(0), pct(1), pct(2), pct(1)),
        4 => (pct(0), pct(1), pct(2), pct(3)),
        _ => return false,
    };
    top + bottom >= 100.0 || left + right >= 100.0
}

fn parse_bg_color(v: &str) -> Option<Color> {
    parse_color(v).or_else(|| v.split_whitespace().find_map(parse_color))
}

/// Parse an HTML attribute color: a CSS color, or a bare hex triplet/sextet the
/// `bgcolor`/`color` attributes allow without a leading `#` (e.g. `ff6600`).
fn parse_attr_color(v: &str) -> Option<Color> {
    let v = v.trim();
    parse_color(v).or_else(|| {
        let hex = v.trim_start_matches('#');
        ((hex.len() == 3 || hex.len() == 6) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| parse_color(&format!("#{hex}")))
            .flatten()
    })
}

/// Apply the legacy HTML presentational attributes we support as computed style:
/// `width`/`height` (table model + `<hr>`), `bgcolor` (table model + `<body>`),
/// `align` (cell text-align; a table centers via auto margins), and `nowrap`
/// (cells). These are the HTML UA stylesheet's presentational hints (HTML §15) —
/// specificity 0, so the caller applies them before the author cascade.
fn apply_presentational_hints(style: &mut ComputedStyle, node: NodeRef<'_>) {
    let tag = node.tag();
    let em = style.font_size as f32;

    if matches!(
        tag,
        "table" | "td" | "th" | "tr" | "col" | "colgroup" | "thead" | "tbody" | "tfoot" | "hr"
    ) {
        if let Some(w) = node.attr("width").and_then(|v| parse_inset(v, em)) {
            style.width = w;
        }
        if let Some(h) = node.attr("height").and_then(|v| parse_inset(v, em)) {
            style.height = h;
        }
    }

    if matches!(
        tag,
        "table" | "td" | "th" | "tr" | "thead" | "tbody" | "tfoot" | "body"
    ) {
        if let Some(c) = node.attr("bgcolor").and_then(parse_attr_color) {
            style.background = Some(c);
        }
    }

    if let Some(a) = node.attr("align") {
        match a.trim().to_ascii_lowercase().as_str() {
            "center" if tag == "table" => {
                style.margin_left_auto = true;
                style.margin_right_auto = true;
            }
            "center" => style.text_align = TextAlign::Center,
            "right" if tag != "table" => style.text_align = TextAlign::Right,
            "left" if tag != "table" => style.text_align = TextAlign::Left,
            _ => {}
        }
    }

    if matches!(tag, "td" | "th") && node.attr("nowrap").is_some() {
        style.white_space = WhiteSpace::Nowrap;
    }
}

fn is_bold(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    if v == "bold" || v == "bolder" {
        return true;
    }
    v.parse::<u32>().map(|n| n >= 600).unwrap_or(false)
}

fn parse_opacity(v: &str) -> Option<f32> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        return p
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| (n / 100.0).clamp(0.0, 1.0));
    }
    v.parse::<f32>().ok().map(|n| n.clamp(0.0, 1.0))
}

fn parse_display(v: &str) -> Option<Display> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "none" => Display::None,
        "list-item" => Display::ListItem,
        "inline" | "contents" => Display::Inline,
        "inline-block" => Display::InlineBlock,
        "flex" | "inline-flex" => Display::Flex,
        "grid" | "inline-grid" => Display::Grid,
        "block" | "table" | "table-row" | "flow-root" => Display::Block,
        // A CSS table cell sits on a row beside its siblings; the shrink-to-fit
        // atomic inline-block is the closest box we model (stacking them as
        // full-width blocks put every "cell" on its own line).
        "table-cell" => Display::InlineBlock,
        _ => return None,
    })
}

/// The recognized `list-style-type` keywords. `decimal-leading-zero` and the
/// alphabetic/roman families collapse to `decimal` (still numbered, just not
/// styled), so an `<ol>` never falls back to a bullet.
fn parse_list_style_type(v: &str) -> Option<ListStyleType> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        "none" => ListStyleType::None,
        "decimal" | "decimal-leading-zero" => ListStyleType::Decimal,
        // `latin` is a synonym for `alpha`.
        "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
        "lower-roman" => ListStyleType::LowerRoman,
        "upper-roman" => ListStyleType::UpperRoman,
        _ => return None,
    })
}

/// Parse a `grid-template-columns`/`-rows` track list (px / fr / auto / repeat()).
fn parse_tracks(v: &str, em_base: f32) -> Vec<Track> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut tracks = Vec::new();
    for tok in split_top(v) {
        expand_track(&tok, em_base, &mut tracks);
    }
    tracks
}

/// Split on top-level whitespace, keeping `repeat(…)` groups (which contain
/// spaces) intact.
/// Split `v` on every top-level char matched by `is_delim`, respecting `(...)`
/// nesting so a delimiter inside `rgb(…)`/`calc(…)`/`repeat(…)` never splits.
///
/// One primitive behind [`split_top`] (whitespace) and [`split_top_commas`]
/// (comma). `keep_empty` distinguishes their list semantics: whitespace-splitting
/// collapses empty fields (runs of spaces are one separator), while
/// comma-splitting keeps them (an empty field is a real, if invalid, list slot) —
/// except a purely-whitespace trailing field, which both drop. Paren depth is
/// clamped at zero so a stray `)` in malformed input can't drive it negative and
/// silently swallow a later top-level delimiter.
fn split_top_by(v: &str, is_delim: impl Fn(char) -> bool, keep_empty: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in v.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            c if depth == 0 && is_delim(c) => {
                if keep_empty || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    // Final field: whitespace-splitting keeps it if non-empty; comma-splitting
    // drops a trailing all-whitespace field (a trailing `,` yields nothing).
    if (keep_empty && !cur.trim().is_empty()) || (!keep_empty && !cur.is_empty()) {
        out.push(cur);
    }
    out
}

/// Split a CSS value on top-level whitespace (respecting parentheses).
fn split_top(v: &str) -> Vec<String> {
    split_top_by(v, char::is_whitespace, false)
}

/// Parse a `grid-template-columns` value into `(explicit tracks, auto-fill
/// track)`. A whole-value `repeat(auto-fill|auto-fit, <track>)` yields no
/// explicit tracks and an auto-fill track whose count layout derives from the
/// container width (ADR-0038); anything else expands to explicit tracks.
fn parse_grid_template(v: &str, em_base: f32) -> (Vec<Track>, Option<Track>) {
    let toks = split_top(v.trim());
    if toks.len() == 1 {
        let t = toks[0].trim().to_ascii_lowercase();
        if let Some(inner) = t.strip_prefix("repeat(").and_then(|s| s.strip_suffix(')')) {
            let mut parts = inner.splitn(2, ',');
            if let (Some(kw), Some(group)) = (parts.next(), parts.next()) {
                let kw = kw.trim();
                if kw == "auto-fill" || kw == "auto-fit" {
                    // The repeated group is a single track in practice.
                    return (Vec::new(), Some(parse_one_track(group.trim(), em_base)));
                }
            }
        }
    }
    (parse_tracks(v, em_base), None)
}

/// Expand one track token (`200px`, `1fr`, `auto`, `minmax(…)`, or `repeat(N,…)`)
/// into `out`.
fn expand_track(tok: &str, em_base: f32, out: &mut Vec<Track>) {
    let t = tok.trim().to_ascii_lowercase();
    // `[line-name]` tokens name grid lines; they are not tracks (skipping them is
    // essential — otherwise a named-line template inflates the column count and
    // squeezes the real content track to near-zero width).
    if t.starts_with('[') {
        return;
    }
    if let Some(inner) = t.strip_prefix("repeat(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.splitn(2, ',');
        if let (Some(n), Some(group_src)) = (parts.next(), parts.next()) {
            if let Ok(count) = n.trim().parse::<usize>() {
                let mut group = Vec::new();
                for sub in split_top(group_src.trim()) {
                    expand_track(&sub, em_base, &mut group);
                }
                for _ in 0..count.min(1000) {
                    out.extend(group.iter().copied());
                }
            }
        }
        return;
    }
    out.push(parse_one_track(tok, em_base));
}

/// Parse a single grid track (`200px` / `1fr` / `auto` / `minmax(min, max)`).
fn parse_one_track(tok: &str, em_base: f32) -> Track {
    let t = tok.trim().to_ascii_lowercase();
    if let Some(inner) = t.strip_prefix("minmax(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.splitn(2, ',');
        if let (Some(min), Some(max)) = (parts.next(), parts.next()) {
            let min_px = parse_len(min.trim(), em_base).unwrap_or(0).max(0) as u32;
            let max = parse_track_max(max.trim(), em_base);
            return Track::MinMax(min_px, max);
        }
    }
    if let Some(fr) = t.strip_suffix("fr") {
        if let Ok(n) = fr.trim().parse::<f32>() {
            return Track::Fr(n.max(0.0));
        }
    }
    if t == "auto" || t == "min-content" || t == "max-content" || t == "fit-content" {
        return Track::Auto;
    }
    match parse_len(&t, em_base) {
        Some(px) => Track::Px(px.max(0) as u32),
        None => Track::Auto,
    }
}

/// The `max` side of a `minmax()`.
fn parse_track_max(tok: &str, em_base: f32) -> TrackMax {
    let t = tok.trim().to_ascii_lowercase();
    if let Some(fr) = t.strip_suffix("fr") {
        if let Ok(n) = fr.trim().parse::<f32>() {
            return TrackMax::Fr(n.max(0.0));
        }
    }
    if t == "auto" || t == "min-content" || t == "max-content" {
        return TrackMax::Auto;
    }
    match parse_len(&t, em_base) {
        Some(px) => TrackMax::Px(px.max(0) as u32),
        None => TrackMax::Auto,
    }
}

/// Split on top-level commas, keeping `func(…)` groups (which contain commas,
/// e.g. `rgba(…)`) intact.
/// Split a CSS value on top-level commas (respecting parentheses), keeping empty
/// interior fields — used for comma-separated layers (gradient stops, shadows).
fn split_top_commas(v: &str) -> Vec<String> {
    split_top_by(v, |c| c == ',', true)
}

/// Parse a `linear-gradient`/`radial-gradient` (incl. vendor prefixes) into a
/// two-stop [`Gradient`] — first/last stop colors, vertical unless the direction
/// is horizontal (ADR-0041). Returns `None` if there's no gradient or no colors.
fn parse_gradient(v: &str) -> Option<Gradient> {
    let low = v.to_ascii_lowercase();
    let start_idx = low
        .find("linear-gradient(")
        .or_else(|| low.find("radial-gradient("))?;
    let open = low[start_idx..].find('(')? + start_idx;
    let is_radial = low[..open].ends_with("radial-gradient");
    // Match the gradient's closing paren.
    let inner = {
        let after = &v[open + 1..];
        let mut depth = 1i32;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        &after[..end]
    };
    let parts = split_top_commas(inner);
    if parts.is_empty() {
        return None;
    }
    // A leading direction (`to …`/angle) is not a stop.
    let first = parts[0].trim().to_ascii_lowercase();
    let (vertical, stops) = if first.starts_with("to ")
        || first.ends_with("deg")
        || first.ends_with("turn")
        || first.ends_with("rad")
    {
        (is_radial || !direction_is_horizontal(&first), &parts[1..])
    } else {
        (true, &parts[..])
    };
    // The color is the first whitespace token of a stop (the rest is a position).
    let color_of = |stop: &str| parse_color(split_top(stop.trim()).first().map(String::as_str)?);
    let start = color_of(stops.first()?)?;
    let end = color_of(stops.last()?).unwrap_or(start);
    Some(Gradient {
        start,
        end,
        vertical: if is_radial { true } else { vertical },
    })
}

/// Whether a `linear-gradient` direction points mostly horizontally (`to
/// left/right`, or an angle near 90°/270°).
fn direction_is_horizontal(dir: &str) -> bool {
    if dir.contains("right") || dir.contains("left") {
        return !dir.contains("top") && !dir.contains("bottom");
    }
    if let Some(deg) = dir
        .strip_suffix("deg")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        let m = deg.rem_euclid(180.0);
        return (45.0..135.0).contains(&m);
    }
    false
}

/// Parse a `box-shadow` (outer, first layer): leading lengths are dx/dy/blur, a
/// color anywhere; `inset`/`none` → `None` (ADR-0041).
fn parse_box_shadow(v: &str, em: f32) -> Option<BoxShadow> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return None;
    }
    let first = split_top_commas(v).into_iter().next()?;
    let mut lens: Vec<i32> = Vec::new();
    let mut color = None;
    for tok in split_top(first.trim()) {
        if tok.eq_ignore_ascii_case("inset") {
            return None; // inner shadows aren't modeled
        }
        if let Some(c) = parse_color(&tok) {
            color = Some(c);
        } else if let Some(n) = parse_len(&tok, em) {
            lens.push(n);
        }
    }
    Some(BoxShadow {
        dx: lens.first().copied().unwrap_or(0),
        dy: lens.get(1).copied().unwrap_or(0),
        blur: lens.get(2).copied().unwrap_or(0).max(0),
        color: color.unwrap_or(cerberus_types::Color::rgba(0, 0, 0, 64)),
    })
}

/// Map `object-fit`/`background-size` to an [`ImageFit`]. Only the aspect-ratio
/// keywords matter to the rasterizer: `cover` crops to fill, `contain` (and the
/// no-upscale `scale-down`) letterbox. `fill`, `none`, explicit sizes (`100%`,
/// `200px 100px`), `auto`, and anything else stretch (`Fill`) — ADR-0044.
fn parse_image_fit(v: &str) -> ImageFit {
    match v.trim().to_ascii_lowercase().as_str() {
        "cover" => ImageFit::Cover,
        "contain" | "scale-down" => ImageFit::Contain,
        // `auto` (the `background-size` initial value) and `object-fit: none` both
        // draw at natural size — the mode CSS sprites depend on.
        "auto" | "none" => ImageFit::Auto,
        _ => ImageFit::Fill,
    }
}

/// Classify one `object-position`/`background-position` token into an axis hint
/// and a fraction: `0` horizontal, `1` vertical, `2` either (percentage/center).
/// Lengths and unknown tokens return `None` (ignored) — ADR-0045.
fn classify_pos_tok(t: &str) -> Option<(u8, f32)> {
    match t.trim().to_ascii_lowercase().as_str() {
        "left" => Some((0, 0.0)),
        "right" => Some((0, 1.0)),
        "top" => Some((1, 0.0)),
        "bottom" => Some((1, 1.0)),
        "center" => Some((2, 0.5)),
        s if s.ends_with('%') => s[..s.len() - 1]
            .trim()
            .parse::<f32>()
            .ok()
            .map(|p| (2, p / 100.0)),
        _ => None,
    }
}

/// Combine up to two classified position tokens into an [`ImagePos`], honoring the
/// keyword-order swap (`top left` == `left top`) — ADR-0045.
fn combine_pos(parts: &[(u8, f32)]) -> Option<ImagePos> {
    match parts {
        [] => None,
        [(a, f)] => Some(if *a == 1 {
            ImagePos { x: 0.5, y: *f }
        } else {
            ImagePos { x: *f, y: 0.5 }
        }),
        [(a0, f0), (a1, f1), ..] => {
            // Same-axis keyword pairs (two horizontal or two vertical) are invalid CSS.
            if (*a0 == 0 && *a1 == 0) || (*a0 == 1 && *a1 == 1) {
                return None;
            }
            // Swap when the order is vertical-first or horizontal-second.
            if *a0 == 1 || *a1 == 0 {
                Some(ImagePos { x: *f1, y: *f0 })
            } else {
                Some(ImagePos { x: *f0, y: *f1 })
            }
        }
    }
}

/// Extract the pixel (length) components of a `background-position` value into a
/// `(x, y)` offset, in the token order x-then-y. Percentages and keywords yield
/// `0` on their axis (they're carried by the fractional [`ImagePos`] instead); a
/// single token sets only x. This is what crops CSS sprites (`0 -304px`).
fn parse_bg_position_px(v: &str, em_base: f32) -> Point {
    let mut out = Point::ZERO;
    for (i, tok) in v.split_whitespace().take(2).enumerate() {
        // A length is a number with a non-`%` unit (or unitless 0). `split_num_unit`
        // rejects keywords (no leading digit), so those fall through to 0.
        let px = match split_num_unit(tok) {
            Some((_, unit)) if unit == "%" => continue,
            _ => parse_css_px(&tok.to_ascii_lowercase(), em_base),
        };
        if let Some(px) = px {
            let px = px.round() as i32;
            if i == 0 {
                out.x = px;
            } else {
                out.y = px;
            }
        }
    }
    out
}

/// Parse a standalone `object-position`/`background-position` value.
fn parse_image_pos(v: &str) -> Option<ImagePos> {
    let parts: Vec<(u8, f32)> = v
        .split_whitespace()
        .filter_map(classify_pos_tok)
        .take(2)
        .collect();
    combine_pos(&parts)
}

/// Pull a `<position> / <size>` group (or a bare `cover`/`contain`) out of the
/// `background` shorthand. Parenthesized spans (`url(...)`, `linear-gradient(...)`)
/// are masked first, so a `/` inside a URL or a `%` in a gradient stop is never
/// read as geometry — ADR-0045.
fn apply_background_shorthand_geometry(style: &mut ComputedStyle, v: &str) {
    let masked = mask_parens(v);
    // The position (fraction + any px) lives before the `/size`, or is the whole
    // value when there's no slash.
    let (pos_span, size_span) = match masked.find('/') {
        Some(slash) => (&masked[..slash], Some(&masked[slash + 1..])),
        None => (masked.as_str(), None),
    };
    if let Some(size_span) = size_span {
        if let Some(tok) = size_span.split_whitespace().next() {
            style.background_size = parse_image_fit(tok);
        }
    } else if let Some(f) = masked
        .split_whitespace()
        .map(parse_image_fit)
        .find(|f| !matches!(f, ImageFit::Fill | ImageFit::Auto))
    {
        style.background_size = f;
    }
    let parts: Vec<(u8, f32)> = pos_span
        .split_whitespace()
        .filter_map(classify_pos_tok)
        .collect();
    if let Some(p) = combine_pos(&parts[parts.len().saturating_sub(2)..]) {
        style.background_position = p;
    }
}

/// Replace parenthesized spans with spaces (keeping the rest) so top-level tokens
/// can be scanned without tripping on `url(...)`/`gradient(...)` innards.
fn mask_parens(v: &str) -> String {
    let mut depth = 0i32;
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '(' => {
                depth += 1;
                out.push(' ');
            }
            ')' => {
                depth = (depth - 1).max(0);
                out.push(' ');
            }
            _ if depth > 0 => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Extract the URL from the first `url(...)` in a value (e.g. a `background` /
/// `background-image`), stripping quotes. Returns `None` for `none`, gradients,
/// or a missing/`data:` url — i.e. only fetchable image URLs (ADR-0038).
fn parse_url_value(v: &str) -> Option<String> {
    let start = v.find("url(")? + 4;
    let rest = &v[start..];
    let end = rest.find(')')?;
    let url = rest[..end]
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    if url.is_empty() || url.starts_with("data:") {
        return None;
    }
    Some(url.to_string())
}

/// Whether a `grid-column`/`grid-row`/`grid-area` value references a *named* line
/// or area (a letter-led token that isn't `span`/`auto`) — which we don't resolve
/// to a track index, so layout uses content-track placement instead (ADR-0038).
fn grid_line_is_named(v: &str) -> bool {
    v.split(['/', ' '])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .any(|t| {
            let low = t.to_ascii_lowercase();
            low != "span"
                && low != "auto"
                && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        })
}

/// Parse a `grid-column`/`grid-row` placement into `(start line, end line,
/// span)`. Numeric lines are kept as CSS line numbers (1-based, negative from
/// the end — `1 / -1` means full width) and resolved against the REAL track
/// count at layout; the span is a fallback for the forms that imply one
/// (`span N`, `a / span N`, positive `a / b`). Named lines/areas yield no
/// numeric placement (the `grid_named_place` content-track path handles them).
fn parse_grid_placement(v: &str) -> (Option<i32>, Option<i32>, u32) {
    let v = v.trim().to_ascii_lowercase();
    if let Some(rest) = v.strip_prefix("span") {
        return (None, None, rest.trim().parse::<u32>().unwrap_or(1).max(1));
    }
    if let Some((a, b)) = v.split_once('/') {
        let (a, b) = (a.trim(), b.trim());
        let start = a.parse::<i32>().ok().filter(|n| *n != 0);
        if let Some(n) = b.strip_prefix("span") {
            return (start, None, n.trim().parse::<u32>().unwrap_or(1).max(1));
        }
        let end = b.parse::<i32>().ok().filter(|n| *n != 0);
        let span = match (start, end) {
            (Some(ai), Some(bi)) if ai > 0 && bi > 0 => (bi - ai).unsigned_abs().max(1),
            _ => 1,
        };
        return (start, end, span);
    }
    (v.parse::<i32>().ok().filter(|n| *n != 0), None, 1)
}

fn apply_font_shorthand(style: &mut ComputedStyle, v: &str, parent_font_size: u32) {
    for token in v.split_whitespace() {
        let t = token.to_ascii_lowercase();
        if t == "bold" {
            style.font.bold = true;
        } else if t == "italic" || t == "oblique" {
            style.font.italic = true;
        } else if t.chars().any(|c| c.is_ascii_digit()) {
            if let Some(px) = parse_size(&t, parent_font_size) {
                style.font_size = px;
                style.font_size_medium = false; // the shorthand set an explicit size
            }
        }
    }
    // The trailing family list (best-effort: size/style tokens don't classify).
    if let Some(g) = parse_font_family(v) {
        style.font_family = g;
    }
}

/// Classify one `font-family` entry into a generic class, or `None` if the name
/// is unknown (so the caller falls through to the next family in the list). CSS
/// generic keywords resolve directly; named faces resolve by well-known keywords
/// in the name (`mono`, `serif`, script/handwriting cues), defaulting a plain
/// named face to sans-serif only when nothing else matches.
fn classify_font_family(name: &str) -> Option<GenericFamily> {
    let n = name
        .trim()
        .trim_matches(['"', '\''])
        .trim()
        .to_ascii_lowercase();
    // Generic keywords resolve directly. `cursive`/`fantasy` render the
    // standard (serif) face: the reference browser's preferences for them are
    // uninstalled fonts, so it falls back to its standard font (measured).
    match n.as_str() {
        "serif" => return Some(GenericFamily::Serif),
        "sans-serif" => return Some(GenericFamily::SansSerif),
        "system-ui" | "ui-sans-serif" | "ui-rounded" => return Some(GenericFamily::SansSystem),
        "monospace" | "ui-monospace" => return Some(GenericFamily::Monospace),
        "cursive" => return Some(GenericFamily::Cursive),
        "fantasy" => return Some(GenericFamily::Fantasy),
        _ => {}
    }
    // Named faces resolve ONLY when the reference actually resolves them — the
    // fontconfig strong (metric) aliases and the faces installed there. Any
    // other name returns None so the stack falls through to its next entry
    // (measured: Chrome renders uninstalled names — Verdana, Georgia, Menlo,
    // Roboto, Segoe UI — as the NEXT resolvable entry, or as the standard
    // serif when nothing in the stack resolves; fontconfig's weak best-match
    // is not used).
    let has = |kw: &str| n.contains(kw);
    if has("arial") || has("helvetica") || has("liberation sans") || has("arimo") {
        Some(GenericFamily::SansArial)
    } else if has("times") || has("tinos") || has("liberation serif") || has("nimbus roman") {
        Some(GenericFamily::Serif)
    } else if has("courier") || has("cousine") || has("liberation mono") || has("nimbus mono") {
        Some(GenericFamily::MonoCourier)
    } else if n == "dejavu sans mono" {
        Some(GenericFamily::Monospace)
    } else if n == "dejavu sans" {
        Some(GenericFamily::SansSystem)
    } else if n == "dejavu serif" {
        Some(GenericFamily::Serif)
    } else {
        None
    }
}

/// Resolve a `font-family` value (a comma-separated list) to one generic class:
/// the first entry that resolves on the reference persona wins (real "first
/// available font" behavior), so `"MyBrand", Georgia, sans-serif` → sans-serif
/// (neither named face is installed). `None` if nothing in the list resolves
/// (the caller keeps the inherited family, whose root default is the standard
/// serif — the reference's unresolvable-stack fallback).
fn parse_font_family(v: &str) -> Option<GenericFamily> {
    v.split(',').find_map(classify_font_family)
}

fn apply_margin_shorthand(style: &mut ComputedStyle, v: &str, em_base: f32) {
    let toks: Vec<&str> = v.split_whitespace().collect();
    let parts: Vec<Len> = toks
        .iter()
        .map(|p| parse_inset(p, em_base).unwrap_or(Len::Px(0)))
        .collect();
    // Track which sides are `auto` (for centering): horizontal sides are index 1
    // (right) and 3 (left) in the 4-value form, or index 1 in the 2/3-value form.
    let is_auto = |i: usize| toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("auto"));
    // Horizontal sides: in the 1-value form both are index 0; in the 2/3-value
    // form both are index 1; in the 4-value form right is index 1, left index 3.
    let (top, bottom, left, right, l_auto, r_auto) = match parts.len() {
        1 => (
            parts[0],
            parts[0],
            parts[0],
            parts[0],
            is_auto(0),
            is_auto(0),
        ),
        2 | 3 => (
            parts[0],
            if parts.len() == 3 { parts[2] } else { parts[0] },
            parts[1],
            parts[1],
            is_auto(1),
            is_auto(1),
        ),
        n if n >= 4 => (
            parts[0],
            parts[2],
            parts[3],
            parts[1],
            is_auto(3),
            is_auto(1),
        ),
        _ => return,
    };
    style.margin_top = top;
    style.margin_bottom = bottom;
    style.margin_left = left;
    style.margin_right = right;
    style.margin_left_auto = l_auto;
    style.margin_right_auto = r_auto;
}

fn parse_size(v: &str, parent: u32) -> Option<u32> {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "xx-small" => return Some(9),
        "x-small" => return Some(11),
        "small" => return Some(13),
        "medium" => return Some(16),
        "large" => return Some(18),
        "x-large" => return Some(24),
        "xx-large" => return Some(32),
        "smaller" => return Some((parent as f32 * 0.85).round() as u32),
        "larger" => return Some((parent as f32 * 1.15).round() as u32),
        "inherit" => return Some(parent),
        _ => {}
    }
    let px = parse_css_px(&v, parent as f32)?;
    Some(px.round().max(1.0) as u32)
}

/// Replace every `<number>rem` in a value with its px equivalent against
/// `root_em`. Only a numeric prefix triggers a match, so `rem` inside an
/// identifier/function/URL is untouched, and `em`/other units are left for
/// per-element resolution. Handles signs and decimals (`-1.5rem`).
fn substitute_rem(value: &str, root_em: f32) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        let mut j = i;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let num_start = j;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
            j += 1;
        }
        let has_digit = value[num_start..j].bytes().any(|c| c.is_ascii_digit());
        // Byte-compare the unit ('rem' is ASCII); guarantees `j + 3` is a char
        // boundary so the slices below never split a multi-byte char.
        let is_rem = j + 3 <= bytes.len()
            && bytes[j].eq_ignore_ascii_case(&b'r')
            && bytes[j + 1].eq_ignore_ascii_case(&b'e')
            && bytes[j + 2].eq_ignore_ascii_case(&b'm')
            && !value[j + 3..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
        if has_digit && is_rem {
            if let Ok(n) = value[i..j].parse::<f32>() {
                out.push_str(&((n * root_em).round() as i32).to_string());
                out.push_str("px");
                i = j + 3;
                continue;
            }
        }
        let ch = value[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn parse_len(v: &str, em_base: f32) -> Option<i32> {
    let v = v.trim().to_ascii_lowercase();
    if v == "auto" || v == "inherit" {
        return None;
    }
    Some(parse_css_px(&v, em_base)?.round() as i32)
}

fn parse_css_px(v: &str, em_base: f32) -> Option<f32> {
    let (num, unit) = split_num_unit(v.trim())?;
    Some(match unit.as_str() {
        "px" | "" => num,
        "em" => num * em_base,
        "rem" => num * 16.0,
        "pt" => num * 96.0 / 72.0,
        "%" => num / 100.0 * em_base,
        "ex" => num * em_base * 0.5,
        _ => return None,
    })
}

/// Parse a `top`/`right`/`bottom`/`left` inset: `auto`, a px length, or a `%`
/// (kept as a percentage for resolution against the containing block at layout).
fn parse_inset(v: &str, em_base: f32) -> Option<Len> {
    let v = v.trim().to_ascii_lowercase();
    if v == "auto" {
        return Some(Len::Auto);
    }
    if v == "inherit" || v == "initial" {
        return None;
    }
    let (num, unit) = split_num_unit(&v)?;
    Some(match unit.as_str() {
        "%" => Len::Pct(num),
        "px" | "" => Len::Px(num.round() as i32),
        "em" => Len::Px((num * em_base).round() as i32),
        "rem" => Len::Px((num * 16.0).round() as i32),
        "pt" => Len::Px((num * 96.0 / 72.0).round() as i32),
        "vw" => Len::Vw(num),
        "vh" => Len::Vh(num),
        "vmin" => Len::Vmin(num),
        "vmax" => Len::Vmax(num),
        _ => return None,
    })
}

/// Parse `clip-path: polygon(x y, x y, …)` into its vertices as `(x, y)` lengths
/// against the border box. `None` for any other value (`none`/`inset()`/`circle()`
/// /`url()`…), which also clears an earlier polygon. Needs ≥3 points to be a fill.
fn parse_clip_polygon(v: &str, em_base: f32) -> Option<Vec<(Len, Len)>> {
    let t = v.trim().to_ascii_lowercase();
    // Allow an optional `<geometry-box>` prefix before the shape (e.g.
    // `border-box polygon(...)`); we ignore the box and take the polygon.
    let inner = t
        .split_once("polygon(")
        .and_then(|(_, rest)| rest.strip_suffix(')'))?;
    let mut pts = Vec::new();
    for pair in inner.split(',') {
        let mut it = pair.split_whitespace();
        let x = parse_inset(it.next()?, em_base)?;
        let y = parse_inset(it.next()?, em_base)?;
        pts.push((x, y));
    }
    (pts.len() >= 3).then_some(pts)
}

/// Parse a `flex-basis` value: `auto`, `content` (and friends), a px length, or
/// a percentage of the container's main size (kept symbolic for layout).
fn parse_flex_basis(v: &str, em: f32) -> FlexBasis {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "auto" | "" | "inherit" | "initial" => FlexBasis::Auto,
        "content" | "max-content" | "min-content" | "fit-content" => FlexBasis::Content,
        _ => {
            if let Some(num) = v
                .strip_suffix('%')
                .and_then(|n| n.trim().parse::<f32>().ok())
            {
                FlexBasis::Pct(num)
            } else if let Some(px) = parse_len(&v, em) {
                FlexBasis::Px(px)
            } else {
                FlexBasis::Auto
            }
        }
    }
}

/// Apply the `flex` shorthand: `none`/`auto`/`initial`, a unitless grow (and
/// optional shrink), and/or a basis — defaulting omitted parts per CSS.
fn apply_flex_shorthand(style: &mut ComputedStyle, v: &str, em: f32) {
    let low = v.trim().to_ascii_lowercase();
    match low.as_str() {
        "none" => {
            style.flex_grow = 0.0;
            style.flex_shrink = 0.0;
            style.flex_basis = FlexBasis::Auto;
            return;
        }
        "auto" => {
            style.flex_grow = 1.0;
            style.flex_shrink = 1.0;
            style.flex_basis = FlexBasis::Auto;
            return;
        }
        "initial" | "" => {
            style.flex_grow = 0.0;
            style.flex_shrink = 1.0;
            style.flex_basis = FlexBasis::Auto;
            return;
        }
        _ => {}
    }
    // Up to two leading unitless numbers are grow/shrink; any remaining token is
    // the basis.
    let mut nums: Vec<f32> = Vec::new();
    let mut basis_tok: Option<&str> = None;
    for p in low.split_whitespace() {
        match p.parse::<f32>() {
            Ok(n) if basis_tok.is_none() && nums.len() < 2 => nums.push(n.max(0.0)),
            _ => {
                if basis_tok.is_none() {
                    basis_tok = Some(p);
                }
            }
        }
    }
    style.flex_grow = nums.first().copied().unwrap_or(1.0);
    style.flex_shrink = nums.get(1).copied().unwrap_or(1.0);
    // A numeric form (`flex: 1`) implies basis 0; a basis-only form keeps it.
    style.flex_basis = match basis_tok {
        Some(b) => parse_flex_basis(b, em),
        None if nums.is_empty() => FlexBasis::Auto,
        None => FlexBasis::Px(0),
    };
}

/// Parse `line-height` into a [`LineHeight`]: `normal` → `Normal`, a unitless
/// number → `Factor(n)` (kept as a multiplier so it inherits and re-resolves per
/// element), `%` → `Px(pct × font-size)`, else a length → `Px` (ADR-0041).
fn parse_line_height(v: &str, font_size: u32) -> LineHeight {
    let t = v.trim().to_ascii_lowercase();
    if t == "normal" || t.is_empty() {
        return LineHeight::Normal;
    }
    // A percentage resolves against the element's own font-size and inherits as
    // that absolute px (unlike a unitless number, which inherits as the factor).
    if let Some(pct) = t
        .strip_suffix('%')
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return LineHeight::Px((pct / 100.0 * font_size as f32).round().max(0.0) as i32);
    }
    // A bare `<number>` is kept as a factor so each element re-multiplies it by
    // its own font-size (correct inheritance across differently-sized elements).
    if let Ok(n) = t.parse::<f32>() {
        return LineHeight::Factor(n.max(0.0));
    }
    parse_len(&t, font_size as f32)
        .map(|px| LineHeight::Px(px.max(0)))
        .unwrap_or(LineHeight::Normal)
}

/// Set a px field from a length value (no-op if it doesn't parse), clamped ≥ 0.
fn set_len(field: &mut i32, v: &str, em: f32) {
    if let Some(n) = parse_len(v, em) {
        *field = n.max(0);
    }
}

/// Apply a 1–4 value box shorthand (top/right/bottom/left, CSS order) to four px
/// fields — used for `padding` and `border-width` (ADR-0040).
fn apply_box_shorthand(v: &str, em: f32, sides: &mut [&mut i32; 4]) {
    let p: Vec<i32> = v
        .split_whitespace()
        .map(|t| parse_len(t, em).unwrap_or(0).max(0))
        .collect();
    let (t, r, b, l) = match p.len() {
        1 => (p[0], p[0], p[0], p[0]),
        2 => (p[0], p[1], p[0], p[1]),
        3 => (p[0], p[1], p[2], p[1]),
        n if n >= 4 => (p[0], p[1], p[2], p[3]),
        _ => return,
    };
    *sides[0] = t;
    *sides[1] = r;
    *sides[2] = b;
    *sides[3] = l;
}

/// A `border-width` keyword (`thin`/`medium`/`thick`) or length, in px.
fn parse_border_width(tok: &str, em: f32) -> Option<i32> {
    match tok {
        "thin" => Some(1),
        "medium" => Some(3),
        "thick" => Some(5),
        _ => parse_len(tok, em).map(|w| w.max(0)),
    }
}

/// Apply a `border` (or per-side `border-*`) shorthand: a width, an optional
/// style keyword (`none`/`hidden` removes it), and a color, in any order. `sides`
/// selects which of top/right/bottom/left to set (ADR-0040).
fn apply_border(style: &mut ComputedStyle, v: &str, sides: [bool; 4]) {
    let em = style.font_size as f32;
    let mut width: Option<i32> = None;
    let mut color: Option<cerberus_types::Color> = None;
    let mut style_none = false;
    let mut saw_style = false;
    for tok in v.split_whitespace() {
        match tok.to_ascii_lowercase().as_str() {
            "none" | "hidden" => {
                style_none = true;
                saw_style = true;
            }
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => {
                saw_style = true
            }
            low => {
                if let Some(w) = parse_border_width(low, em) {
                    width = Some(w);
                } else if let Some(c) = parse_color(tok) {
                    color = Some(c);
                }
            }
        }
    }
    // Width: explicit wins; `none` is 0; a bare style/color implies the default
    // medium width so the border is visible.
    let w = if style_none {
        0
    } else {
        width.unwrap_or(if saw_style || color.is_some() { 3 } else { 0 })
    };
    let fields = [
        &mut style.border_top,
        &mut style.border_right,
        &mut style.border_bottom,
        &mut style.border_left,
    ];
    for (i, f) in fields.into_iter().enumerate() {
        if sides[i] {
            *f = w;
        }
    }
    if let Some(c) = color {
        style.border_color = c;
    }
}

/// Apply the `inset` shorthand (1–4 values: top/right/bottom/left, CSS order).
fn apply_inset_shorthand(style: &mut ComputedStyle, v: &str, em: f32) {
    let raw: Vec<&str> = v.split_whitespace().collect();
    let Some(p): Option<Vec<Len>> = raw.iter().map(|s| parse_inset(s, em)).collect() else {
        return;
    };
    let (t, r, b, l) = match p.len() {
        1 => (p[0], p[0], p[0], p[0]),
        2 => (p[0], p[1], p[0], p[1]),
        3 => (p[0], p[1], p[2], p[1]),
        4 => (p[0], p[1], p[2], p[3]),
        _ => return,
    };
    style.inset_top = t;
    style.inset_right = r;
    style.inset_bottom = b;
    style.inset_left = l;
}

fn split_num_unit(v: &str) -> Option<(f32, String)> {
    let end = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(v.len());
    let num: f32 = v[..end].parse().ok()?;
    Some((num, v[end..].trim().to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_html;

    #[test]
    fn top_level_splitters_respect_parens_and_clamp_depth() {
        // Whitespace split ignores spaces inside a function; comma split ignores
        // commas inside one and keeps empty interior fields.
        assert_eq!(
            split_top("1px calc(2px + 3px) 4px"),
            vec!["1px", "calc(2px + 3px)", "4px"]
        );
        assert_eq!(
            split_top_commas("rgb(1, 2, 3), , blue"),
            vec!["rgb(1, 2, 3)", " ", " blue"],
            "interior empty/space fields are kept"
        );
        // A trailing comma yields no empty final field.
        assert_eq!(split_top_commas("a,"), vec!["a"]);

        // Depth clamp: an unbalanced `)` must not drive depth negative and swallow
        // a later top-level delimiter. With clamping, the comma still splits.
        assert_eq!(split_top_commas("a) , b"), vec!["a) ", " b"]);
        assert_eq!(split_top("a) b"), vec!["a)", "b"]);
        // var()-style first-comma split is likewise clamp-protected.
        assert_eq!(split_top_comma("x), y"), ("x)", Some(" y")));
    }

    fn first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
        if node.tag == tag {
            return Some(node);
        }
        node.children.iter().find_map(|c| match c {
            StyledChild::Element(e) => first(e, tag),
            StyledChild::Text(_) => None,
        })
    }

    #[test]
    fn cascade_bucketing_keeps_every_matchable_rule() {
        // Regression guard for the key-selector index: an element must still see
        // a rule keyed on *any* of its classes (not just the first), a bare
        // attribute selector (universal bucket), a tag rule, and a comma rule
        // reachable via two of its keys (deduped, applied once).
        let dom = CssEngine::new().style(&parse_html(
            "<style>\
               .c { color: #ff0000 }\
               [data-k] { text-decoration: underline }\
               p { font-weight: bold }\
               .a, p { color: #00ff00 }\
             </style>\
             <p id='t' class='a b c' data-k='v'>x</p>",
        ));
        let p = first(&dom.root, "p").expect("p");
        // `.c` is the element's THIRD class — the index must bucket by it and the
        // element must probe all its classes, or this rule would be dropped.
        // `.a, p` is a lower-specificity earlier rule, so `.c` (0,1,0 vs 0,1,0
        // but later source order) wins red only over `.a,p`'s green... both are
        // (0,1,0)/(0,0,1); the class rules tie on specificity so source order
        // decides: `.c` precedes `.a,p`, so green (from `.a, p`) wins last.
        assert_eq!(
            p.style.color,
            Color::rgb(0, 255, 0),
            "later equal-specificity class rule (reached via the bucket) wins"
        );
        // Universal-bucket attribute rule still applies.
        assert!(p.style.underline, "[data-k] underline applied");
        // Tag-bucket rule still applies.
        assert!(p.style.font.bold, "p{{font-weight:bold}} applied");
    }

    #[test]
    fn font_family_resolves_to_generic() {
        let dom = CssEngine::new().style(&parse_html(
            "<p id=a style='font-family:Georgia, serif'>a</p>\
             <p id=b style='font-family:Arial, sans-serif'>b</p>\
             <p id=c style='font-family:Consolas, monospace'>c</p>\
             <p id=d style='font-family:\"Brush Script MT\", cursive'>d</p>\
             <p id=e style='font-family:\"Segoe UI\", sans-serif'>e</p>",
        ));
        let fam = |id: &str| {
            fn by_id<'a>(n: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
                if n.attr("id") == Some(id) {
                    return Some(n);
                }
                n.children.iter().find_map(|c| match c {
                    StyledChild::Element(e) => by_id(e, id),
                    StyledChild::Text(_) => None,
                })
            }
            by_id(&dom.root, id).unwrap().style.font_family
        };
        assert_eq!(fam("a"), GenericFamily::Serif, "Georgia → serif");
        assert_eq!(
            fam("b"),
            GenericFamily::SansArial,
            "Arial → Arial-metric sans"
        );
        assert_eq!(fam("c"), GenericFamily::Monospace, "Consolas → monospace");
        assert_eq!(fam("d"), GenericFamily::Cursive, "Brush Script → cursive");
        // A non-Arial named sans (and the generic) fall to the Roboto default.
        assert_eq!(
            fam("e"),
            GenericFamily::SansSerif,
            "Segoe UI → default sans"
        );
    }

    #[test]
    fn monospace_uses_chrome_default_13px_size() {
        // `<pre>`/`<code>` inherit the UA monospace family; with an unspecified
        // (`medium`) size they resolve to 13px, matching Chrome's smaller default
        // for the fixed font. Proportional text stays 16px. An explicit size wins.
        let dom = CssEngine::new().style(&parse_html(
            "<p>prose</p><pre>code block</pre>\
             <code style='font-size:20px'>big</code>",
        ));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.font_size,
            16,
            "proportional medium → 16"
        );
        assert_eq!(
            first(&dom.root, "pre").unwrap().style.font_size,
            13,
            "monospace medium → 13"
        );
        assert_eq!(
            first(&dom.root, "code").unwrap().style.font_size,
            20,
            "explicit monospace size is honored"
        );
    }

    #[test]
    fn text_decoration_line_through_and_combos() {
        let strike = CssEngine::new().style(&parse_html(
            "<span style='text-decoration:line-through'>x</span>",
        ));
        let s = first(&strike.root, "span").unwrap();
        assert!(s.style.line_through && !s.style.underline);
        // A shorthand may name both lines.
        let both = CssEngine::new().style(&parse_html(
            "<span style='text-decoration:underline line-through'>x</span>",
        ));
        let b = first(&both.root, "span").unwrap();
        assert!(b.style.underline && b.style.line_through);
        // `none` clears the UA underline on an <a>.
        let cleared = CssEngine::new().style(&parse_html(
            "<a href='/x' style='text-decoration:none'>x</a>",
        ));
        assert!(!first(&cleared.root, "a").unwrap().style.underline);
    }

    #[test]
    fn clip_path_polygon_parses_to_vertices() {
        use crate::Len;
        // A four-point divider polygon: `%` → Pct, `px` → Px, both resolved
        // against the border box at paint time.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='clip-path:polygon(0 0, 100% 0, 100% 70%, 0 100%)'>x</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        let poly = d.style.clip_polygon.as_ref().expect("polygon parsed");
        assert_eq!(
            poly,
            &vec![
                (Len::Px(0), Len::Px(0)),
                (Len::Pct(100.0), Len::Px(0)),
                (Len::Pct(100.0), Len::Pct(70.0)),
                (Len::Px(0), Len::Pct(100.0)),
            ]
        );
        // A non-polygon clip form leaves no fill shape.
        let inset =
            CssEngine::new().style(&parse_html("<div style='clip-path:inset(0 0 0 0)'>x</div>"));
        assert!(first(&inset.root, "div")
            .unwrap()
            .style
            .clip_polygon
            .is_none());
    }

    #[test]
    fn ua_styles_headings_and_links() {
        let dom = CssEngine::new().style(&parse_html("<body><h1>T</h1><a href='/x'>l</a></body>"));
        let h1 = first(&dom.root, "h1").unwrap();
        assert!(h1.style.font.bold);
        assert_eq!(h1.style.font_size, 32);
        let a = first(&dom.root, "a").unwrap();
        assert!(a.style.underline);
        assert_eq!(a.style.color, Color::rgb(0x15, 0x4f, 0xd2));
    }

    #[test]
    fn ua_indents_dd() {
        // A description-list definition is indented 40px by the UA stylesheet.
        let dom = CssEngine::new().style(&parse_html("<dl><dt>t</dt><dd>d</dd></dl>"));
        assert_eq!(
            first(&dom.root, "dd").unwrap().style.margin_left,
            Len::Px(40)
        );
    }

    #[test]
    fn ua_gives_figure_default_margins() {
        let dom = CssEngine::new().style(&parse_html("<figure>f</figure>"));
        let f = first(&dom.root, "figure").unwrap();
        assert_eq!(f.style.margin_left, Len::Px(40));
        assert_eq!(f.style.margin_right, Len::Px(40));
        assert_eq!(f.style.margin_top, Len::Px(16));
    }

    #[test]
    fn ua_styles_details_summary_block() {
        let dom =
            CssEngine::new().style(&parse_html("<details><summary>s</summary>body</details>"));
        assert_eq!(
            first(&dom.root, "details").unwrap().style.display,
            Display::Block
        );
        assert_eq!(
            first(&dom.root, "summary").unwrap().style.display,
            Display::Block
        );
    }

    #[test]
    fn ua_styles_dfn_and_address_italic() {
        let dom = CssEngine::new().style(&parse_html("<dfn>d</dfn><address>a</address>"));
        assert!(first(&dom.root, "dfn").unwrap().style.font.italic);
        let addr = first(&dom.root, "address").unwrap();
        assert!(addr.style.font.italic);
        assert_eq!(addr.style.display, Display::Block, "<address> is a block");
    }

    #[test]
    fn ua_styles_legacy_center_and_nobr() {
        // `<center>` is a centered block carrying the legacy `-webkit-center`
        // value (centers child table boxes, not table-cell text); `<nobr>`
        // prevents wrapping.
        let dom = CssEngine::new().style(&parse_html("<center>c</center><nobr>n</nobr>"));
        let c = first(&dom.root, "center").unwrap();
        assert_eq!(c.style.display, Display::Block);
        assert_eq!(c.style.text_align, TextAlign::WebkitCenter);
        assert_eq!(
            first(&dom.root, "nobr").unwrap().style.white_space,
            cerberus_style::WhiteSpace::Nowrap
        );
    }

    #[test]
    fn ua_styles_text_level_semantics() {
        // <del>/<s> strike through, <ins>/<u> underline, <mark> highlights.
        let dom = CssEngine::new().style(&parse_html(
            "<del>d</del><s>s</s><ins>i</ins><u>u</u><mark>m</mark>",
        ));
        assert!(first(&dom.root, "del").unwrap().style.line_through);
        assert!(first(&dom.root, "s").unwrap().style.line_through);
        assert!(first(&dom.root, "ins").unwrap().style.underline);
        assert!(first(&dom.root, "u").unwrap().style.underline);
        let mark = first(&dom.root, "mark").unwrap();
        assert_eq!(mark.style.background, Some(Color::rgb(255, 255, 0)));
        assert_eq!(mark.style.color, Color::BLACK);
    }

    #[test]
    fn only_anchors_with_href_get_link_styling() {
        // An `<a>` without href is a placeholder/named anchor, not a link, so it
        // keeps the default text color and no underline; one with href is styled.
        let dom =
            CssEngine::new().style(&parse_html("<a name='top'>anchor</a><a href='/x'>link</a>"));
        let anchors: Vec<_> = {
            fn collect<'a>(n: &'a StyledNode, out: &mut Vec<&'a StyledNode>) {
                if n.tag == "a" {
                    out.push(n);
                }
                for c in &n.children {
                    if let StyledChild::Element(e) = c {
                        collect(e, out);
                    }
                }
            }
            let mut v = Vec::new();
            collect(&dom.root, &mut v);
            v
        };
        assert_eq!(anchors.len(), 2);
        assert!(
            !anchors[0].style.underline && anchors[0].style.color == Color::BLACK,
            "the bare <a name> anchor is not styled as a link"
        );
        assert!(
            anchors[1].style.underline && anchors[1].style.color == Color::rgb(0x15, 0x4f, 0xd2),
            "the <a href> is styled as a link"
        );
    }

    #[test]
    fn author_and_inline_cascade() {
        let html = "<style>p{color:green} #x{color:red}</style>\
                    <p id='x' style='color:#0000ff'>hi</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        // inline beats #id beats type.
        assert_eq!(p.style.color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn presentational_attributes_map_to_style() {
        // width/bgcolor/align HTML attributes become style (HTML §15 hints).
        let dom = CssEngine::new().style(&parse_html(
            "<table width='85%' bgcolor='#ff6600' align='center'>\
             <tr><td align='right' bgcolor='eee'>x</td></tr></table>",
        ));
        let table = first(&dom.root, "table").unwrap();
        assert_eq!(table.style.width, Len::Pct(85.0), "width='85%' → 85%");
        assert_eq!(
            table.style.background,
            Some(Color::rgb(255, 102, 0)),
            "bgcolor"
        );
        assert!(
            table.style.margin_left_auto && table.style.margin_right_auto,
            "align=center on a table centers it"
        );
        let td = first(&dom.root, "td").unwrap();
        assert_eq!(td.style.text_align, TextAlign::Right, "td align=right");
        // Bare hex (no '#') is accepted for attribute colors.
        assert_eq!(td.style.background, Some(Color::rgb(238, 238, 238)));
    }

    #[test]
    fn author_css_overrides_presentational_hint() {
        // A hint sits at specificity 0, so any author rule wins.
        let dom = CssEngine::new().style(&parse_html(
            "<style>table{width:400px}</style><table width='85%'><tr><td>x</td></tr></table>",
        ));
        let table = first(&dom.root, "table").unwrap();
        assert_eq!(
            table.style.width,
            Len::Px(400),
            "author width wins over attr"
        );
    }

    #[test]
    fn hidden_attribute_computes_display_none() {
        // The `hidden` boolean attribute hides the element via the UA sheet's
        // `[hidden] { display: none }`, but an author `display` overrides it
        // (higher/equal specificity + later origin).
        let dom = CssEngine::new().style(&parse_html(
            "<p hidden>a</p><p>b</p><p hidden style='display:block'>c</p>",
        ));
        let ps: Vec<_> = {
            fn collect<'a>(n: &'a StyledNode, out: &mut Vec<&'a StyledNode>) {
                if n.tag == "p" {
                    out.push(n);
                }
                for c in &n.children {
                    if let StyledChild::Element(e) = c {
                        collect(e, out);
                    }
                }
            }
            let mut v = Vec::new();
            collect(&dom.root, &mut v);
            v
        };
        assert_eq!(ps[0].style.display, Display::None, "[hidden] hides");
        assert_eq!(ps[1].style.display, Display::Block, "no hidden → block");
        assert_eq!(
            ps[2].style.display,
            Display::Block,
            "author display:block overrides [hidden]"
        );
    }

    #[test]
    fn display_none_and_background() {
        let html =
            "<div style='display:none'>x</div><section style='background:#ffffff'>y</section>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "div").unwrap().style.display,
            Display::None
        );
        assert_eq!(
            first(&dom.root, "section").unwrap().style.background,
            Some(Color::rgb(255, 255, 255))
        );
    }

    #[test]
    fn opacity_honored_animation_ignored() {
        let html = "<p style='opacity:0; animation: fade 3s; transition: all 2s'>x</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(p.style.opacity, 0.0, "opacity is now read");
        // animation/transition remain ignored — still a normal block.
        assert_eq!(p.style.display, Display::Block);
    }

    #[test]
    fn visibility_hidden_is_computed() {
        let dom = CssEngine::new().style(&parse_html("<p style='visibility:hidden'>x</p>"));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.visibility,
            Visibility::Hidden
        );
    }

    // ---- sr-only hiding via clip / clip-path (FIX 3) ----

    #[test]
    fn sr_only_clip_rect_hides_positioned_element() {
        // The Bootstrap `.visually-hidden` / classic sr-only pattern: the
        // "Skip to content" link must not paint (bbc/mozilla/iana/apple).
        let html = "<style>.sr{position:absolute;width:1px;height:1px;clip:rect(0,0,0,0)}\
                    </style><a class='sr' href='#main'>Skip to content</a><p>body</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "a").unwrap().style.visibility,
            Visibility::Hidden
        );
        assert_eq!(
            first(&dom.root, "p").unwrap().style.visibility,
            Visibility::Visible
        );
        // The legacy space-separated 1px rect hides too (right − left = 0),
        // and the hidden visibility reaches children by inheritance.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='position:absolute;clip:rect(1px 1px 1px 1px)'><span>x</span></div>",
        ));
        assert_eq!(
            first(&dom2.root, "span").unwrap().style.visibility,
            Visibility::Hidden
        );
    }

    #[test]
    fn clip_requires_absolute_positioning_but_not_declaration_order() {
        // Per CSS (Chrome-verified), `clip` applies only to absolutely
        // positioned boxes: a static element keeps painting.
        let stat = CssEngine::new().style(&parse_html("<div style='clip:rect(0,0,0,0)'>x</div>"));
        assert_eq!(
            first(&stat.root, "div").unwrap().style.visibility,
            Visibility::Visible
        );
        // Alphabetized blocks declare `clip` before `position`, and `position`
        // may come from a different rule entirely; the check runs after the
        // whole cascade, so both still hide.
        let html = "<style>.a{clip:rect(0,0,0,0)} .b{position:absolute}</style>\
                    <i class='a b'>x</i>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "i").unwrap().style.visibility,
            Visibility::Hidden
        );
        let fixed = CssEngine::new().style(&parse_html(
            "<div style='clip:rect(0,0,0,0);position:fixed'>x</div>",
        ));
        assert_eq!(
            first(&fixed.root, "div").unwrap().style.visibility,
            Visibility::Hidden
        );
    }

    #[test]
    fn visible_clip_values_do_not_hide_and_can_override() {
        // The last declaration wins: `auto` restores a previously-hidden clip.
        let auto = CssEngine::new().style(&parse_html(
            "<div style='position:absolute;clip:rect(0,0,0,0);clip:auto'>x</div>",
        ));
        assert_eq!(
            first(&auto.root, "div").unwrap().style.visibility,
            Visibility::Visible
        );
        // A rect that leaves a visible region is ignored (no real clipping).
        let partial = CssEngine::new().style(&parse_html(
            "<div style='position:absolute;clip:rect(0, 100px, 100px, 0)'>x</div>",
        ));
        assert_eq!(
            first(&partial.root, "div").unwrap().style.visibility,
            Visibility::Visible
        );
    }

    #[test]
    fn clip_path_inset_hides_only_when_fully_inset() {
        let vis = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.visibility
        };
        // Fully-insetting values hide — no positioning requirement (unlike clip).
        assert_eq!(vis("clip-path:inset(50%)"), Visibility::Hidden);
        assert_eq!(vis("clip-path:inset(100%)"), Visibility::Hidden);
        assert_eq!(vis("clip-path:inset(50% 50%)"), Visibility::Hidden);
        // A `round` radius suffix doesn't confuse the parse.
        assert_eq!(vis("clip-path:inset(50% round 8px)"), Visibility::Hidden);
        // Chrome shows half the box for a one-sided 50% inset — stay visible.
        assert_eq!(vis("clip-path:inset(0 0 50% 0)"), Visibility::Visible);
        // px insets can't be judged without the box; other shapes are ignored.
        assert_eq!(vis("clip-path:inset(10px)"), Visibility::Visible);
        assert_eq!(vis("clip-path:circle(40%)"), Visibility::Visible);
        // Last declaration wins: `none` restores.
        assert_eq!(
            vis("clip-path:inset(50%);clip-path:none"),
            Visibility::Visible
        );
    }

    #[test]
    fn flex_and_grid_parse() {
        let html = "<div style='display:flex; flex-direction:column; \
                    justify-content:space-between; align-items:center; gap:8px'>x</div>\
                    <section style='display:grid; \
                    grid-template-columns: 100px 1fr repeat(2, 2fr)'>y</section>";
        let dom = CssEngine::new().style(&parse_html(html));
        let d = first(&dom.root, "div").unwrap();
        assert_eq!(d.style.display, Display::Flex);
        assert_eq!(d.style.flex_direction, FlexDirection::Column);
        assert_eq!(d.style.justify_content, JustifyContent::SpaceBetween);
        assert_eq!(d.style.align_items, AlignItems::Center);
        assert_eq!(d.style.gap, 8);
        let g = first(&dom.root, "section").unwrap();
        assert_eq!(g.style.display, Display::Grid);
        assert_eq!(
            g.style.grid_template_columns,
            vec![
                Track::Px(100),
                Track::Fr(1.0),
                Track::Fr(2.0),
                Track::Fr(2.0)
            ]
        );
    }

    #[test]
    fn combinators_pseudos_and_attrs_cascade() {
        // Child combinator, :nth-child, and an attribute selector through the
        // full cascade (author CSS + DOM).
        let html = "<style>\
            ul > li:nth-child(2) { color: #ff0000 }\
            input[type=\"text\"] { color: #00ff00 }\
            </style>\
            <ul><li>a</li><li>b</li></ul><input type='text'>";
        let dom = CssEngine::new().style(&parse_html(html));
        // The second <li> is red; the first is not.
        let ul = first(&dom.root, "ul").unwrap();
        let lis: Vec<_> = ul
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if e.tag == "li" => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(lis[0].style.color, Color::BLACK);
        assert_eq!(lis[1].style.color, Color::rgb(0xff, 0, 0));
        // The text input is green via the attribute selector.
        assert_eq!(
            first(&dom.root, "input").unwrap().style.color,
            Color::rgb(0, 0xff, 0)
        );
    }

    #[test]
    fn has_is_where_cascade_end_to_end() {
        // (FIX 4) `:has(> a)` through the real cascade: only the div with a
        // direct <a> child turns red; `:is`/`:where` match the element itself.
        let html = "<style>\
            div:has(> a) { color: #ff0000 }\
            section:has(> a) { color: #0000ff }\
            p:is(.hero, .lead) { color: #00ff00 }\
            span:where([data-x]) { color: #00ffff }\
            </style>\
            <div><a href='/x'>l</a></div>\
            <section><b><a href='/y'>m</a></b></section>\
            <p class='lead'>t</p><span data-x='1'>s</span>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "div").unwrap().style.color,
            Color::rgb(0xff, 0, 0),
            "div has a direct <a> child"
        );
        assert_eq!(
            first(&dom.root, "section").unwrap().style.color,
            Color::BLACK,
            "the section's <a> is nested, not a direct child"
        );
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0)
        );
        assert_eq!(
            first(&dom.root, "span").unwrap().style.color,
            Color::rgb(0, 0xff, 0xff)
        );
    }

    #[test]
    fn nth_of_type_cascades_over_mixed_siblings() {
        // Among a mix of tags, `p:first-of-type` targets the first <p> even though
        // it isn't the first child, and `:nth-of-type(2)` the second <p>.
        let html = "<style>\
            p:first-of-type { color: #ff0000 }\
            p:nth-of-type(2) { color: #00ff00 }\
            </style>\
            <div><h2>t</h2><p>a</p><span>s</span><p>b</p></div>";
        let dom = CssEngine::new().style(&parse_html(html));
        let div = first(&dom.root, "div").unwrap();
        let ps: Vec<_> = div
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if e.tag == "p" => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(
            ps[0].style.color,
            Color::rgb(0xff, 0, 0),
            "first-of-type <p>"
        );
        assert_eq!(
            ps[1].style.color,
            Color::rgb(0, 0xff, 0),
            "nth-of-type(2) <p>"
        );
    }

    #[test]
    fn nth_last_child_cascades_from_the_end() {
        // `li:nth-last-child(1)` is the last item; `:nth-last-child(2)` the
        // second-to-last — counted from the end through the full cascade.
        let html = "<style>\
            li:nth-last-child(1) { color: #ff0000 }\
            li:nth-last-child(2) { color: #00ff00 }\
            </style>\
            <ul><li>a</li><li>b</li><li>c</li></ul>";
        let dom = CssEngine::new().style(&parse_html(html));
        let lis: Vec<_> = first(&dom.root, "ul")
            .unwrap()
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if e.tag == "li" => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(lis[0].style.color, Color::BLACK, "first li unaffected");
        assert_eq!(lis[1].style.color, Color::rgb(0, 0xff, 0), "2nd-from-last");
        assert_eq!(lis[2].style.color, Color::rgb(0xff, 0, 0), "last");
    }

    #[test]
    fn generated_content_builds_before_and_after_boxes() {
        // ::before prepends, ::after appends; the box inherits from its
        // originating element and carries the content text; attr() reads the
        // element; content:none (or no content) generates nothing.
        let dom = CssEngine::new().style(&parse_html(
            "<style>               .x::before { content: '-> '; color: #ff0000 }               .x:after { content: attr(data-n) '!'; }               .y::before { content: none; background: #00ff00 }               .z::before { background: #0000ff }             </style>             <p class='x' data-n='42'>mid</p><p class='y'>y</p><p class='z'>z</p>",
        ));
        fn by_class<'a>(n: &'a StyledNode, class: &str) -> Option<&'a StyledNode> {
            if n.attr("class") == Some(class) {
                return Some(n);
            }
            n.children.iter().find_map(|c| match c {
                StyledChild::Element(e) => by_class(e, class),
                _ => None,
            })
        }
        let x = by_class(&dom.root, "x").unwrap();
        let ps = [
            x,
            by_class(&dom.root, "y").unwrap(),
            by_class(&dom.root, "z").unwrap(),
        ];
        let first = match &x.children[0] {
            StyledChild::Element(e) => e,
            other => panic!("expected ::before element, got {other:?}"),
        };
        assert_eq!(first.tag, "::before");
        assert_eq!(first.text(), "-> ");
        assert_eq!(first.style.color, Color::rgb(0xff, 0, 0));
        assert_eq!(first.node_id, x.node_id, "pseudo belongs to its element");
        let last = match x.children.last().unwrap() {
            StyledChild::Element(e) => e,
            other => panic!("expected ::after element, got {other:?}"),
        };
        assert_eq!(last.tag, "::after");
        assert_eq!(last.text(), "42!", "attr() + string concatenation");
        // content:none and MISSING content both suppress the box.
        assert!(ps[1]
            .children
            .iter()
            .all(|c| !matches!(c, StyledChild::Element(e) if e.tag.starts_with("::"))));
        assert!(ps[2]
            .children
            .iter()
            .all(|c| !matches!(c, StyledChild::Element(e) if e.tag.starts_with("::"))));
    }

    #[test]
    fn generated_content_empty_string_still_makes_a_box() {
        // content:"" + a background is the decorative-band/clearfix pattern:
        // the box exists (and its styles apply) with no text.
        let dom = CssEngine::new().style(&parse_html(
            "<style>.band::before { content: ''; background: #112233 }</style>             <div class='band'>t</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        let b = match &d.children[0] {
            StyledChild::Element(e) => e,
            other => panic!("expected ::before, got {other:?}"),
        };
        assert_eq!(b.tag, "::before");
        assert!(b.children.is_empty(), "empty content, no text child");
        assert_eq!(b.style.background, Some(Color::rgb(0x11, 0x22, 0x33)));
    }

    #[test]
    fn media_query_respects_viewport() {
        let html = "<style>@media (max-width: 600px) { p { color: #ff0000 } }</style><p>x</p>";
        // Narrow viewport: the rule applies.
        let narrow = CssEngine::with_media(480, 800).style(&parse_html(html));
        assert_eq!(
            first(&narrow.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0)
        );
        // Wide viewport: it does not.
        let wide = CssEngine::with_media(1200, 800).style(&parse_html(html));
        assert_eq!(first(&wide.root, "p").unwrap().style.color, Color::BLACK);
    }

    // ---- CSS custom properties + var()/calc() (ADR-0035) ----

    #[test]
    fn var_resolves_from_root_and_inherits() {
        // A `:root` custom property is visible to a descendant via inheritance.
        let html = "<html><head><style>:root{--brand:#ff0000} p{color:var(--brand)}\
                    </style></head><body><p>x</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0)
        );
    }

    #[test]
    fn var_fallback_used_when_undefined() {
        let html = "<p style='color:var(--missing, #00ff00)'>x</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0)
        );
    }

    #[test]
    fn var_is_overridable_in_scope() {
        // The nearer declaration wins: the inner div redefines --c for itself.
        let html = "<html><head><style>:root{--c:#ff0000} div{color:var(--c)}</style></head>\
                    <body><div>outer<div style='--c:#0000ff'>inner</div></div></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        let outer = first(&dom.root, "div").unwrap();
        assert_eq!(outer.style.color, Color::rgb(0xff, 0, 0));
        let inner = match outer.children.iter().find_map(|c| match c {
            StyledChild::Element(e) if e.tag == "div" => Some(e),
            _ => None,
        }) {
            Some(e) => e,
            None => panic!("inner div"),
        };
        assert_eq!(inner.style.color, Color::rgb(0, 0, 0xff));
    }

    #[test]
    fn var_resolves_nested_references() {
        // --a -> var(--b) -> a concrete color, regardless of declaration order.
        let html = "<html><head><style>:root{--a:var(--b);--b:#abcdef} p{color:var(--a)}\
                    </style></head><body><p>x</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xab, 0xcd, 0xef)
        );
    }

    #[test]
    fn media_query_overrides_root_custom_property() {
        // mozilla.org drives its whole type/spacing scale through `:root` custom
        // properties that a wider `@media` redefines (`--text-title-2xl` goes
        // 48px → 80px → 128px). At a width where the query matches, the override
        // must win so a `var()`-driven font-size scales up. Regression for the
        // hero `<h1>` rendering at the base 48px instead of Chrome's 80px.
        let html = "<html><head><style>\
            :root{--t:48px}\
            @media(min-width:768px){:root{--t:80px}}\
            h1{font-size:var(--t)}\
            </style></head><body><h1>x</h1></body></html>";
        // Narrow viewport: the base value wins.
        let narrow = CssEngine::with_media(500, 800).style(&parse_html(html));
        assert_eq!(first(&narrow.root, "h1").unwrap().style.font_size, 48);
        // Wide viewport: the `@media` override wins.
        let wide = CssEngine::with_media(1200, 800).style(&parse_html(html));
        assert_eq!(first(&wide.root, "h1").unwrap().style.font_size, 80);
    }

    #[test]
    fn margin_right_from_longhand_and_shorthand() {
        // The `margin-right` longhand is honored (previously only its `auto` flag
        // was), and each shorthand arity fills the right side correctly.
        let one = CssEngine::new().style(&parse_html("<p style='margin:5px'>x</p>"));
        assert_eq!(
            first(&one.root, "p").unwrap().style.margin_right,
            Len::Px(5)
        );
        let two = CssEngine::new().style(&parse_html("<p style='margin:5px 10px'>x</p>"));
        assert_eq!(
            first(&two.root, "p").unwrap().style.margin_right,
            Len::Px(10)
        );
        let four = CssEngine::new().style(&parse_html("<p style='margin:1px 2px 3px 4px'>x</p>"));
        let p = first(&four.root, "p").unwrap();
        assert_eq!(
            p.style.margin_right,
            Len::Px(2),
            "right is the 2nd of four values"
        );
        assert_eq!(p.style.margin_left, Len::Px(4), "left is the 4th");
        let long = CssEngine::new().style(&parse_html("<p style='margin-right:12px'>x</p>"));
        assert_eq!(
            first(&long.root, "p").unwrap().style.margin_right,
            Len::Px(12)
        );
    }

    #[test]
    fn calc_evaluates_length_math() {
        // calc with mixed absolute units, precedence, and a var() operand (the
        // element references its own custom property).
        let html =
            "<p style='--g:8px;margin-top:calc(2 * 4px + 1rem);margin-left:calc(var(--g) * 3)'>x</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(p.style.margin_top, Len::Px(24), "2*4 + 16(rem) = 24");
        assert_eq!(p.style.margin_left, Len::Px(24), "8 * 3 = 24");
    }

    // ---- calc() percentage base (FIX 1) ----

    #[test]
    fn calc_mixed_pct_px_fails_instead_of_resolving_against_font_size() {
        // `width: calc(100% - 32px)` must not resolve `%` against the font size
        // (that gave 16 - 32 = a negative width). The containing block isn't
        // known at style time, so the declaration is dropped and the width
        // falls back (auto) — Chrome on an 800px block computes 768px.
        let dom =
            CssEngine::new().style(&parse_html("<div style='width:calc(100% - 32px)'>x</div>"));
        assert_eq!(first(&dom.root, "div").unwrap().style.width, Len::Auto);
        // A longhand that only sets on parse success keeps its prior value.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='margin-left:8px;margin-left:calc(100% - 32px)'>x</div>",
        ));
        assert_eq!(
            first(&dom2.root, "div").unwrap().style.margin_left,
            Len::Px(8)
        );
    }

    #[test]
    fn calc_pure_pct_reduces_to_a_percentage() {
        // A %-only calc reduces to a plain percentage, which resolves against
        // the containing block at layout — exactly like a literal `50%`.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='width:calc(100%);margin-left:calc(25% + 25%)'>x</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        assert_eq!(d.style.width, Len::Pct(100.0));
        assert_eq!(d.style.margin_left, Len::Pct(50.0));
        // % terms that cancel leave a plain px value.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='width:calc(50% - 50% + 24px)'>x</div>",
        ));
        assert_eq!(first(&dom2.root, "div").unwrap().style.width, Len::Px(24));
    }

    #[test]
    fn calc_pct_still_folds_for_font_relative_properties() {
        // For font-size, `%` IS font-relative (of the inherited size), so a
        // mixed calc still resolves: 100% of 16px + 2px = 18px (Chrome agrees).
        let dom =
            CssEngine::new().style(&parse_html("<p style='font-size:calc(100% + 2px)'>x</p>"));
        assert_eq!(first(&dom.root, "p").unwrap().style.font_size, 18);
    }

    #[test]
    fn calc_viewport_units_resolve_against_the_engine_viewport() {
        // The default engine viewport is 1280×800.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='width:calc(50vw - 40px);height:calc(25vh)'>x</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        assert_eq!(d.style.width, Len::Px(600), "50vw - 40px = 600px");
        assert_eq!(d.style.height, Len::Px(200), "25vh = 200px");
    }

    // ---- min()/max()/clamp() (FIX 2) ----

    #[test]
    fn min_max_evaluate_over_calc_units() {
        // min/max over px-reducible units (em here: 30em = 480px < 500px).
        let dom = CssEngine::new().style(&parse_html(
            "<div style='width:min(500px, 30em);height:max(100px, 10em)'>x</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        assert_eq!(d.style.width, Len::Px(480));
        assert_eq!(d.style.height, Len::Px(160));
        // Pure percentages compare symbolically and stay a percentage.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='width:min(50%, 80%);height:200px;max-height:max(25%, 10%)'>x</div>",
        ));
        let d2 = first(&dom2.root, "div").unwrap();
        assert_eq!(d2.style.width, Len::Pct(50.0));
        assert_eq!(d2.style.max_height, Len::Pct(25.0));
    }

    #[test]
    fn clamp_picks_lo_mid_hi() {
        let clamp = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.width
        };
        assert_eq!(clamp("width:clamp(10px, 5px, 20px)"), Len::Px(10), "lo");
        assert_eq!(clamp("width:clamp(10px, 15px, 20px)"), Len::Px(15), "mid");
        assert_eq!(clamp("width:clamp(10px, 25px, 20px)"), Len::Px(20), "hi");
        // Wrong arity is invalid and falls back.
        assert_eq!(clamp("width:clamp(10px, 20px)"), Len::Auto);
    }

    #[test]
    fn min_max_mixed_pct_px_falls_back_like_calc() {
        // `%` and px can't be compared without the containing block; the
        // declaration is dropped rather than mis-resolved (FIX 1 safety rule).
        let dom =
            CssEngine::new().style(&parse_html("<div style='width:min(100%, 500px)'>x</div>"));
        assert_eq!(first(&dom.root, "div").unwrap().style.width, Len::Auto);
    }

    #[test]
    fn math_functions_nest_and_leave_minmax_alone() {
        // min() inside calc() (and vice versa) resolve innermost-first.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='width:calc(min(10px, 20px) * 2);height:min(calc(3px * 4), 50px)'>x</div>",
        ));
        let d = first(&dom.root, "div").unwrap();
        assert_eq!(d.style.width, Len::Px(20));
        assert_eq!(d.style.height, Len::Px(12));
        // The `max(` substring inside grid `minmax(` is not a math function.
        let grid = CssEngine::new().style(&parse_html(
            "<div style='display:grid;grid-template-columns:minmax(100px, 1fr) 2fr'>x</div>",
        ));
        assert_eq!(
            first(&grid.root, "div")
                .unwrap()
                .style
                .grid_template_columns,
            vec![Track::MinMax(100, TrackMax::Fr(1.0)), Track::Fr(2.0)]
        );
    }

    #[test]
    fn var_cycle_does_not_hang() {
        // A self-referential cycle resolves to empty (no infinite recursion); the
        // property is left at its initial value rather than crashing.
        let html = "<html><head><style>:root{--a:var(--b);--b:var(--a)} p{color:var(--a)}\
                    </style></head><body><p>x</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(first(&dom.root, "p").unwrap().style.color, Color::BLACK);
    }

    // ---- External <link> stylesheets (ADR-0037) ----

    #[test]
    fn external_link_stylesheet_applies() {
        let html = "<html><head><link rel='stylesheet' href='/site.css'></head>\
                    <body><p>x</p></body></html>";
        let mut sheets = ExternalSheets::new();
        sheets.insert("/site.css".to_string(), "p{color:#ff0000}".to_string());
        let dom = CssEngine::new().style_with_sheets(&parse_html(html), &sheets);
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0)
        );
        // Without the fetched body, the rule is absent (the link contributes
        // nothing) — `style()` is the no-sheets path.
        let plain = CssEngine::new().style(&parse_html(html));
        assert_eq!(first(&plain.root, "p").unwrap().style.color, Color::BLACK);
    }

    #[test]
    fn external_sheet_respects_cascade_source_order() {
        // The <link> precedes the inline <style>, so the (equal-specificity)
        // inline rule wins by source order — proving the sheet is spliced at the
        // link's document position, not appended at the end.
        let html = "<html><head><link rel='stylesheet' href='/a.css'>\
                    <style>p{color:#00ff00}</style></head><body><p>x</p></body></html>";
        let mut sheets = ExternalSheets::new();
        sheets.insert("/a.css".to_string(), "p{color:#ff0000}".to_string());
        let dom = CssEngine::new().style_with_sheets(&parse_html(html), &sheets);
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0),
            "later inline <style> wins over the earlier <link>"
        );
    }

    #[test]
    fn external_sheet_can_define_variables() {
        // A design-token sheet on :root resolves for the whole page (ADR-0035 +
        // ADR-0037 together — the common real-world setup).
        let html = "<html><head><link rel='stylesheet' href='/tokens.css'></head>\
                    <body><p>x</p></body></html>";
        let mut sheets = ExternalSheets::new();
        sheets.insert(
            "/tokens.css".to_string(),
            ":root{--fg:#3366cc} p{color:var(--fg)}".to_string(),
        );
        let dom = CssEngine::new().style_with_sheets(&parse_html(html), &sheets);
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0x33, 0x66, 0xcc)
        );
    }

    #[test]
    fn important_overrides_higher_specificity_and_source_order() {
        // A low-specificity `!important` beats a later, higher-specificity normal
        // rule — the whole point of `!important` (previously the flag was dropped,
        // so specificity/order alone decided and this read green).
        let html = "<html><head><style>\
            p { color: #ff0000 !important }\
            #x { color: #00ff00 }\
            </style></head><body><p id='x'>hi</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0),
            "the !important type rule wins over the normal #id rule"
        );
    }

    #[test]
    fn important_beats_inline_but_inline_important_wins() {
        // Author `!important` overrides an inline (normal) style, but an inline
        // `!important` still tops the author `!important` (inline is applied last
        // in the important pass).
        let beats_inline = CssEngine::new().style(&parse_html(
            "<html><head><style>p{color:#ff0000 !important}</style></head>\
             <body><p style='color:#0000ff'>x</p></body></html>",
        ));
        assert_eq!(
            first(&beats_inline.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0),
            "author !important beats inline normal"
        );
        let inline_wins = CssEngine::new().style(&parse_html(
            "<html><head><style>p{color:#ff0000 !important}</style></head>\
             <body><p style='color:#0000ff !important'>x</p></body></html>",
        ));
        assert_eq!(
            first(&inline_wins.root, "p").unwrap().style.color,
            Color::rgb(0, 0, 0xff),
            "inline !important beats author !important"
        );
    }

    #[test]
    fn important_among_rules_still_respects_specificity() {
        // Between two `!important` declarations the normal cascade still decides:
        // the higher-specificity `#x` wins over the type selector.
        let html = "<html><head><style>\
            p { color: #ff0000 !important }\
            #x { color: #00ff00 !important }\
            </style></head><body><p id='x'>hi</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0),
            "higher-specificity !important wins among important declarations"
        );
    }

    #[test]
    fn current_color_resolves_to_the_elements_color() {
        // `currentColor` in a non-`color` slot resolves to the element's own
        // cascaded `color` (here set red earlier in the same rule).
        let html = "<html><head><style>\
            p { color: #ff0000; border-color: currentColor }\
            </style></head><body><p>x</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(p.style.color, Color::rgb(0xff, 0, 0));
        assert_eq!(
            p.style.border_color,
            Color::rgb(0xff, 0, 0),
            "border-color: currentColor picks up the element color"
        );
    }

    #[test]
    fn current_color_uses_inherited_color() {
        // With no `color` on the element, `currentColor` resolves to the
        // inherited color from the parent.
        let html = "<html><head><style>\
            div { color: #00ff00 }\
            span { border-color: currentColor }\
            </style></head><body><div><span>x</span></div></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        let span = first(&dom.root, "span").unwrap();
        assert_eq!(
            span.style.border_color,
            Color::rgb(0, 0xff, 0),
            "currentColor inherits the parent's color"
        );
    }

    #[test]
    fn list_style_type_from_ua_and_author() {
        // `<ol>` gets `decimal` from the UA sheet and its `<li>` inherits it;
        // `<ul>`/`<li>` default to `disc`.
        let dom = CssEngine::new().style(&parse_html("<ol><li>a</li></ol><ul><li>b</li></ul>"));
        let oli = first(&dom.root, "ol")
            .unwrap()
            .children
            .iter()
            .find_map(|c| {
                if let StyledChild::Element(e) = c {
                    Some(&**e)
                } else {
                    None
                }
            });
        assert_eq!(oli.unwrap().style.list_style_type, ListStyleType::Decimal);
        // Author override on a list container reaches its items via inheritance.
        let dom2 = CssEngine::new().style(&parse_html(
            "<ul style='list-style-type:square'><li>x</li></ul>",
        ));
        let uli = first(&dom2.root, "li").unwrap();
        assert_eq!(uli.style.list_style_type, ListStyleType::Square);
        // The `list-style` shorthand's type token is honored.
        let dom3 =
            CssEngine::new().style(&parse_html("<li style='list-style: none inside'>x</li>"));
        assert_eq!(
            first(&dom3.root, "li").unwrap().style.list_style_type,
            ListStyleType::None
        );
    }

    #[test]
    fn alpha_and_roman_list_types_parse_and_map_from_ol_type_attr() {
        // The `lower-alpha`/`upper-roman`/… keywords now map to their own types
        // (previously all collapsed to decimal); `latin` is a synonym for `alpha`.
        let author = CssEngine::new().style(&parse_html(
            "<ol style='list-style-type:lower-roman'><li>x</li></ol>",
        ));
        assert_eq!(
            first(&author.root, "ol").unwrap().style.list_style_type,
            ListStyleType::LowerRoman
        );
        let latin = CssEngine::new().style(&parse_html(
            "<ol style='list-style-type:upper-latin'><li>x</li></ol>",
        ));
        assert_eq!(
            first(&latin.root, "ol").unwrap().style.list_style_type,
            ListStyleType::UpperAlpha
        );

        // The HTML `type` attribute selects the marker via UA rules, and the value
        // match is case-sensitive: `a` → lower-alpha, `A` → upper-alpha.
        let lower = CssEngine::new().style(&parse_html("<ol type='a'><li>x</li></ol>"));
        assert_eq!(
            first(&lower.root, "ol").unwrap().style.list_style_type,
            ListStyleType::LowerAlpha
        );
        let upper = CssEngine::new().style(&parse_html("<ol type='I'><li>x</li></ol>"));
        assert_eq!(
            first(&upper.root, "ol").unwrap().style.list_style_type,
            ListStyleType::UpperRoman
        );
    }

    #[test]
    fn text_indent_parses_and_inherits() {
        let dom =
            CssEngine::new().style(&parse_html("<div style='text-indent:2em'><p>x</p></div>"));
        // 2em at the default 16px font = 32px; inherited by the child.
        assert_eq!(first(&dom.root, "div").unwrap().style.text_indent, 32);
        assert_eq!(first(&dom.root, "p").unwrap().style.text_indent, 32);
    }

    #[test]
    fn rem_resolves_against_the_root_font_size() {
        // `html { font-size: 62.5% }` → root = 10px, so 1rem = 10px everywhere,
        // not the hardcoded 16. Both a dimension (width) and a font-size scale.
        let dom = CssEngine::new().style(&parse_html(
            "<html style='font-size:62.5%'><body>\
               <div style='width:15.6rem;font-size:1.3rem'>x</div>\
             </body></html>",
        ));
        let div = first(&dom.root, "div").unwrap();
        assert_eq!(div.style.width.resolve(1000), Some(156));
        assert_eq!(div.style.font_size, 13);
        // Without any html font-size, rem stays 16px (the initial base).
        let plain = CssEngine::new().style(&parse_html("<div style='width:2rem'>x</div>"));
        assert_eq!(
            first(&plain.root, "div").unwrap().style.width.resolve(1000),
            Some(32)
        );
    }

    #[test]
    fn line_height_unitless_inherits_as_a_factor() {
        use cerberus_style::LineHeight;
        // A unitless line-height inherits as the factor, so a differently-sized
        // child re-resolves it against its own font-size (not the parent's).
        let dom = CssEngine::new().style(&parse_html(
            "<div style='line-height:2;font-size:10px'>\
             <p style='font-size:30px'>x</p></div>",
        ));
        let div = first(&dom.root, "div").unwrap();
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(div.style.line_height, LineHeight::Factor(2.0));
        assert_eq!(
            p.style.line_height,
            LineHeight::Factor(2.0),
            "factor inherits"
        );
        assert_eq!(div.style.line_height.resolve(10, 0), 20, "2 * 10px");
        assert_eq!(
            p.style.line_height.resolve(30, 0),
            60,
            "2 * 30px, its own size"
        );

        // A px/percentage line-height inherits as the resolved absolute length.
        // (font-size is declared first so the percentage resolves against 20px.)
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='font-size:20px;line-height:150%'>\
             <p style='font-size:40px'>x</p></div>",
        ));
        // 150% of 20px = 30px, inherited verbatim (not re-scaled to the child).
        assert_eq!(
            first(&dom2.root, "p").unwrap().style.line_height,
            LineHeight::Px(30)
        );
    }

    #[test]
    fn vertical_align_sub_sup_from_ua_and_not_inherited() {
        use cerberus_style::VerticalAlign;
        // `<sub>`/`<sup>` pick up their alignment (and a smaller size) from the UA
        // sheet; the surrounding text stays on the baseline.
        let dom = CssEngine::new().style(&parse_html("<p>x<sub>1</sub><sup>2</sup></p>"));
        assert_eq!(
            first(&dom.root, "sub").unwrap().style.vertical_align,
            VerticalAlign::Sub
        );
        assert_eq!(
            first(&dom.root, "sup").unwrap().style.vertical_align,
            VerticalAlign::Super
        );
        assert_eq!(
            first(&dom.root, "p").unwrap().style.vertical_align,
            VerticalAlign::Baseline
        );
        // `smaller` shrinks the sub/sup font below the 16px default.
        assert!(first(&dom.root, "sup").unwrap().style.font_size < 16);
        // Not inherited: a value on a parent does not reach the child.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='vertical-align:super'><span>y</span></div>",
        ));
        assert_eq!(
            first(&dom2.root, "div").unwrap().style.vertical_align,
            VerticalAlign::Super
        );
        assert_eq!(
            first(&dom2.root, "span").unwrap().style.vertical_align,
            VerticalAlign::Baseline
        );
    }

    #[test]
    fn white_space_keywords_parse() {
        use cerberus_style::WhiteSpace;
        let cases = [
            ("nowrap", WhiteSpace::Nowrap),
            ("pre-wrap", WhiteSpace::PreWrap),
            ("pre-line", WhiteSpace::PreLine),
            ("normal", WhiteSpace::Normal),
        ];
        for (kw, want) in cases {
            let dom =
                CssEngine::new().style(&parse_html(&format!("<p style='white-space:{kw}'>x</p>")));
            assert_eq!(
                first(&dom.root, "p").unwrap().style.white_space,
                want,
                "{kw}"
            );
        }
        // `<pre>` gets `pre` from the UA sheet; it wraps neither spaces nor lines.
        let pre = CssEngine::new().style(&parse_html("<pre>x</pre>"));
        let el = first(&pre.root, "pre").unwrap();
        assert_eq!(el.style.white_space, WhiteSpace::Pre);
        assert!(el.style.white_space.preserves_spaces() && !el.style.white_space.wraps());
        // `pre-wrap` preserves spaces but still wraps; `pre-line` collapses spaces
        // yet preserves newlines.
        assert!(WhiteSpace::PreWrap.preserves_spaces() && WhiteSpace::PreWrap.wraps());
        assert!(
            !WhiteSpace::PreLine.preserves_spaces() && WhiteSpace::PreLine.preserves_newlines()
        );
    }

    #[test]
    fn word_spacing_parses_and_inherits() {
        let dom = CssEngine::new().style(&parse_html(
            "<div style='word-spacing:12px'><span>x</span></div>",
        ));
        assert_eq!(first(&dom.root, "div").unwrap().style.word_spacing, 12);
        // word-spacing is inherited, so the child sees it too.
        assert_eq!(first(&dom.root, "span").unwrap().style.word_spacing, 12);
        // `normal` resets to 0.
        let dom2 = CssEngine::new().style(&parse_html("<div style='word-spacing:normal'>x</div>"));
        assert_eq!(first(&dom2.root, "div").unwrap().style.word_spacing, 0);
    }

    #[test]
    fn supports_block_rules_are_applied_and_font_face_is_skipped() {
        // A supported `@supports` condition applies its rules; @font-face never
        // injects rules; text still renders (bundled font).
        let html = "<html><head><style>\
            @font-face { font-family: 'X'; src: url(x.woff2); }\
            @supports (display: grid) { p { color: #ff0000 } }\
            </style></head><body><p style=\"font-family:'X', sans-serif\">hi</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0),
            "@supports rule applied; @font-face caused no breakage"
        );
    }

    #[test]
    fn font_face_families_are_collected_for_document_fonts() {
        // The page's own @font-face families are surfaced (lowercased, de-quoted)
        // so document.fonts.check() can report them loaded — without ever fetching
        // the bytes (ADR-0005). Also catches @font-face nested in @media.
        let html = "<html><head><style>\
            @font-face { font-family: 'Mozilla Text'; src: url(a.woff2); font-weight: 400 }\
            @font-face { font-family: \"Mozilla Headline\"; src: url(b.woff2) }\
            @media (min-width: 100px) { @font-face { font-family: Zilla; src: url(c.woff2) } }\
            p { color: #000 }\
            </style></head><body><p>hi</p></body></html>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            dom.font_face_families,
            vec![
                "mozilla text".to_string(),
                "mozilla headline".to_string(),
                "zilla".to_string()
            ]
        );
    }

    #[test]
    fn supports_not_of_a_supported_feature_is_skipped() {
        // `not (display: grid)` is false because we support grid, so its block —
        // a legacy fallback that would otherwise win on source order — must not
        // apply. The plain `@supports (display: grid)` block still does.
        let html = "<style>\
            @supports (display: grid) { p { color: #00ff00 } }\
            @supports not (display: grid) { p { color: #ff0000 } }\
            </style><p>hi</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0),
            "the not(grid) fallback is dropped; the grid rule wins"
        );
    }

    #[test]
    fn supports_not_of_an_undecidable_feature_still_applies() {
        // We can't decide support for an unknown property, so `not(...)` falls back
        // to the historical default of applying the block (never dropping rules we
        // can't prove unnecessary).
        let html = "<style>\
            @supports not (nonexistent-prop: 1) { p { color: #ff0000 } }\
            </style><p>hi</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0),
            "undecidable not(...) still applies"
        );
    }

    #[test]
    fn background_image_url_is_parsed() {
        let dom = CssEngine::new().style(&parse_html(
            "<div style=\"background-image: url('hero.jpg')\">x</div>",
        ));
        assert_eq!(
            first(&dom.root, "div")
                .unwrap()
                .style
                .background_image
                .as_deref(),
            Some("hero.jpg")
        );
        // The `background` shorthand carries both color and image.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='background: #fff url(bg.png) no-repeat'>x</div>",
        ));
        let s = &first(&dom2.root, "div").unwrap().style;
        assert_eq!(s.background_image.as_deref(), Some("bg.png"));
        assert_eq!(s.background, Some(Color::rgb(255, 255, 255)));
    }

    #[test]
    fn object_fit_and_background_size_parse() {
        let fit = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<img style='{css}'>")));
            first(&dom.root, "img").unwrap().style.object_fit
        };
        assert_eq!(fit("object-fit: cover"), ImageFit::Cover);
        assert_eq!(fit("object-fit: contain"), ImageFit::Contain);
        assert_eq!(fit("object-fit: scale-down"), ImageFit::Contain);
        assert_eq!(fit("object-fit: fill"), ImageFit::Fill);
        assert_eq!(
            fit("object-fit: none"),
            ImageFit::Auto,
            "object-fit: none draws at natural size"
        );

        let bg = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.background_size
        };
        assert_eq!(bg("background-size: cover"), ImageFit::Cover);
        assert_eq!(bg("background-size: contain"), ImageFit::Contain);
        assert_eq!(
            bg("background-size: 100% 50%"),
            ImageFit::Fill,
            "explicit sizes stretch (Fill)"
        );
        // The two are independent properties on one element.
        let dom = CssEngine::new().style(&parse_html(
            "<img style='object-fit: cover; background-size: contain'>",
        ));
        let s = &first(&dom.root, "img").unwrap().style;
        assert_eq!(s.object_fit, ImageFit::Cover);
        assert_eq!(s.background_size, ImageFit::Contain);
    }

    #[test]
    fn background_position_px_crops_sprites() {
        let sty = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.clone()
        };
        // Wikipedia's wordmark sprite: a pixel offset into a no-scale background.
        let s = sty("background-position: 0 -304px");
        assert_eq!(s.background_position_px, Point::new(0, -304));
        // Absent background-size defaults to `auto` (natural size) — the sprite mode.
        assert_eq!(s.background_size, ImageFit::Auto);
        // Two lengths, x then y.
        assert_eq!(
            sty("background-position: -10px 20px").background_position_px,
            Point::new(-10, 20)
        );
        // A keyword/percentage carries no pixel component (it's in the fraction).
        assert_eq!(
            sty("background-position: center").background_position_px,
            Point::ZERO
        );
        // A keyword on x, a length on y: the length lands on the y axis.
        assert_eq!(
            sty("background-position: center -260px").background_position_px,
            Point::new(0, -260)
        );
    }

    #[test]
    fn object_position_parse() {
        let pos = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<img style='{css}'>")));
            first(&dom.root, "img").unwrap().style.object_position
        };
        // Default is center.
        assert_eq!(pos(""), ImagePos::CENTER);
        // One value: the other axis stays center.
        assert_eq!(pos("object-position: right"), ImagePos { x: 1.0, y: 0.5 });
        assert_eq!(pos("object-position: top"), ImagePos { x: 0.5, y: 0.0 });
        assert_eq!(pos("object-position: 25%"), ImagePos { x: 0.25, y: 0.5 });
        // Two values, and the keyword-order swap.
        assert_eq!(
            pos("object-position: 25% 75%"),
            ImagePos { x: 0.25, y: 0.75 }
        );
        assert_eq!(
            pos("object-position: bottom right"),
            ImagePos { x: 1.0, y: 1.0 },
            "vertical-first keywords swap to (x,y)"
        );
        assert_eq!(
            pos("object-position: top left"),
            ImagePos { x: 0.0, y: 0.0 },
            "top left"
        );
        assert_eq!(
            pos("object-position: left top"),
            ImagePos { x: 0.0, y: 0.0 },
            "left top swaps the same as top left"
        );
        assert_eq!(
            pos("object-position: center top"),
            ImagePos { x: 0.5, y: 0.0 }
        );
        assert_eq!(
            pos("object-position: right center"),
            ImagePos { x: 1.0, y: 0.5 }
        );
        assert_eq!(pos("object-position: 30% 40%"), ImagePos { x: 0.3, y: 0.4 });
        // Lengths are ignored (no box at parse time) — falls back to default.
        assert_eq!(pos("object-position: 10px 20px"), ImagePos::CENTER);
    }

    #[test]
    fn object_position_same_axis_keywords_are_invalid() {
        let pos = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<img style='{css}'>")));
            first(&dom.root, "img").unwrap().style.object_position
        };
        // Two keywords on the same axis are invalid CSS; the declaration is
        // ignored and the initial value (center) is kept.
        assert_eq!(pos("object-position: left right"), ImagePos::CENTER);
        assert_eq!(pos("object-position: top bottom"), ImagePos::CENTER);
        assert_eq!(pos("object-position: left left"), ImagePos::CENTER);
    }

    #[test]
    fn background_shorthand_position_and_size() {
        let s = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.clone()
        };
        // The ubiquitous `<position> / cover` group, masked past url()/keywords.
        let a = s("background: #fff url(bg.png) center / cover no-repeat");
        assert_eq!(a.background_size, ImageFit::Cover);
        assert_eq!(a.background_position, ImagePos::CENTER);
        assert_eq!(a.background_image.as_deref(), Some("bg.png"));

        let b = s("background: url(/img/a.png) left top / contain");
        assert_eq!(b.background_size, ImageFit::Contain);
        assert_eq!(b.background_position, ImagePos { x: 0.0, y: 0.0 });
        assert_eq!(
            b.background_image.as_deref(),
            Some("/img/a.png"),
            "the slash inside url() is not mistaken for a size separator"
        );

        // A bare `cover` keyword (no slash) still sets the size.
        assert_eq!(
            s("background: url(x.png) cover").background_size,
            ImageFit::Cover
        );

        // A gradient's internal `%` stops are masked, not read as a position; with
        // no slash the size stays the `auto` initial value.
        let g = s("background: linear-gradient(red 50%, blue)");
        assert_eq!(g.background_size, ImageFit::Auto);
        assert_eq!(g.background_position, ImagePos::TOP_LEFT);
        assert!(g.background_gradient.is_some());

        // The shorthand resets position/size longhands to their initial value
        // even when the shorthand's own value carries no geometry group.
        let reset_size = s("background-size: cover; background: url(x)");
        assert_eq!(
            reset_size.background_size,
            ImageFit::Auto,
            "background shorthand resets a prior background-size longhand to auto"
        );
        let reset_pos = s("background-position: right; background: url(x)");
        assert_eq!(
            reset_pos.background_position,
            ImagePos::TOP_LEFT,
            "background shorthand resets a prior background-position longhand"
        );
        // Positive case: geometry present in the shorthand still applies.
        let both =
            s("background-position: right; background-size: cover; background: center / cover");
        assert_eq!(both.background_position, ImagePos::CENTER);
        assert_eq!(both.background_size, ImageFit::Cover);
        // `background-color` alone is not the shorthand and must not reset geometry.
        let color_only =
            s("background-position: right; background-size: cover; background-color: red");
        assert_eq!(color_only.background_position, ImagePos { x: 1.0, y: 0.5 });
        assert_eq!(color_only.background_size, ImageFit::Cover);
    }

    #[test]
    fn background_shorthand_resets_a_prior_color() {
        let s = |css: &str| {
            let dom = CssEngine::new().style(&parse_html(&format!("<div style='{css}'>x</div>")));
            first(&dom.root, "div").unwrap().style.clone()
        };
        // A shorthand with no color component clears a previously-set color to
        // transparent (issue #78) — it must not leak behind the image.
        assert_eq!(
            s("background: red; background: url(x)").background,
            None,
            "a later `background: url(x)` resets the color to transparent"
        );
        // `background: none` clears both the image and any prior color.
        assert_eq!(s("background: red; background: none").background, None);
        // Positive case: a color in the shorthand is still applied.
        assert_eq!(
            s("background: #fff url(x)").background,
            Some(Color::rgb(255, 255, 255)),
            "a color present in the shorthand is kept"
        );
        // The standalone `background-color` longhand stays additive: an
        // unparseable value does not wipe a prior color.
        assert_eq!(
            s("background-color: red; background-color: nonsense").background,
            Some(Color::rgb(255, 0, 0)),
            "a bad background-color longhand leaves the prior color intact"
        );
    }

    #[test]
    fn gradient_radius_shadow_parse() {
        let dom = CssEngine::new().style(&parse_html(
            "<div style='background:linear-gradient(to right, #ff0000, #0000ff);\
             border-radius:8px; box-shadow:0 2px 6px rgba(0,0,0,0.3)'>x</div>",
        ));
        let s = &first(&dom.root, "div").unwrap().style;
        let g = s.background_gradient.as_deref().expect("gradient parsed");
        assert_eq!(g.start, Color::rgb(0xff, 0, 0));
        assert_eq!(g.end, Color::rgb(0, 0, 0xff));
        assert!(!g.vertical, "`to right` is horizontal");
        assert_eq!(s.border_radius, 8);
        let sh = s.box_shadow.as_deref().expect("shadow parsed");
        assert_eq!((sh.dx, sh.dy, sh.blur), (0, 2, 6));
        assert_eq!(sh.color.a, 77); // rgba(...,0.3) -> round(0.3*255)
                                    // A vertical default-direction gradient.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='background:linear-gradient(#fff,#000)'>x</div>",
        ));
        assert!(
            first(&dom2.root, "div")
                .unwrap()
                .style
                .background_gradient
                .as_deref()
                .unwrap()
                .vertical
        );
    }

    #[test]
    fn light_dark_picks_the_light_argument() {
        // `light-dark(a, b)` → a on the fixed light persona, matching Chrome for
        // a light user. Args may be nested vars/functions.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='background-color: light-dark(#f7f7f8, #212426)'>x</div>",
        ));
        assert_eq!(
            first(&dom.root, "div").unwrap().style.background,
            Some(Color::rgb(0xf7, 0xf7, 0xf8)),
            "light-dark takes the light (first) argument"
        );
    }

    #[test]
    fn guaranteed_invalid_var_takes_the_outer_fallback() {
        // The custom-property "light-dark toggle" every modern design system
        // compiles to: an `initial` custom property reached through a var()
        // with no fallback is the GUARANTEED-INVALID value, so the wrapping
        // `var(--toggle, LIGHT)` must take its LIGHT fallback — not resolve the
        // inner to empty and keep the dark branch (the MDN regression).
        let css = "<style>div{\
            --cs-light:initial;\
            --toggle:var(--cs-light) #212426;\
            --bg:var(--toggle,#f7f7f8);\
            background-color:var(--bg);}\
            </style><div>x</div>";
        let dom = CssEngine::new().style(&parse_html(css));
        assert_eq!(
            first(&dom.root, "div").unwrap().style.background,
            Some(Color::rgb(0xf7, 0xf7, 0xf8)),
            "invalid toggle falls back to the light value"
        );
    }

    #[test]
    fn undefined_var_without_fallback_drops_the_declaration() {
        // `color: var(--nope)` with no fallback is invalid at computed-value
        // time → the declaration is dropped and the inherited color kept, not
        // reset to a default.
        let css = "<style>p{color:#112233;color:var(--nope)}</style><p>x</p>";
        let dom = CssEngine::new().style(&parse_html(css));
        assert_eq!(
            first(&dom.root, "p").unwrap().style.color,
            Color::rgb(0x11, 0x22, 0x33),
            "invalid var() declaration dropped, prior value kept"
        );
    }

    #[test]
    fn data_uri_and_gradient_backgrounds_are_not_fetchable_urls() {
        // We only surface fetchable image URLs; data: and gradients yield None.
        let dom = CssEngine::new().style(&parse_html(
            "<div style='background-image: linear-gradient(red, blue)'>x</div>",
        ));
        assert!(first(&dom.root, "div")
            .unwrap()
            .style
            .background_image
            .is_none());
    }

    #[test]
    fn non_stylesheet_link_is_ignored() {
        // A non-stylesheet rel (e.g. icon) never contributes CSS even if a body
        // is somehow present for its href.
        let html = "<html><head><link rel='icon' href='/x'></head><body><p>y</p></body></html>";
        let mut sheets = ExternalSheets::new();
        sheets.insert("/x".to_string(), "p{color:#ff0000}".to_string());
        let dom = CssEngine::new().style_with_sheets(&parse_html(html), &sheets);
        assert_eq!(first(&dom.root, "p").unwrap().style.color, Color::BLACK);
    }
}

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
    FlexBasis, FlexDirection, Float, Gradient, JustifyContent, Len, Position, StyleEngine,
    StyledChild, StyledDom, StyledNode, TextAlign, TextTransform, Track, TrackMax, Visibility,
};
use cerberus_types::{Color, ImageFit, ImagePos};
use parser::{
    parse_declaration_block, parse_stylesheet, ElemRef, MediaContext, RuleIndex, SiblingRef,
    Specificity, Stylesheet,
};
use std::collections::HashMap;
use std::rc::Rc;

/// The cascaded CSS custom properties (`--name` → raw value) in scope for an
/// element. Inherited down the tree and shared by `Rc` so elements that declare
/// no custom properties (the common case) reuse their parent's map for free
/// (ADR-0035). Keys are lowercased to match the parser's property-name folding.
type Vars = Rc<HashMap<String, String>>;

/// A matched rule during the cascade: (origin, specificity, source-order, decls).
type MatchedRule<'a> = (u8, Specificity, usize, &'a Vec<(String, String)>);

/// The user-agent default stylesheet.
const UA_CSS: &str = r#"
html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
header, footer, nav, main, aside, blockquote, pre, figure, figcaption, form,
table, tr, hr, dl, dt, dd, fieldset, address { display: block; }
head, title, meta, link, style, script, base, template { display: none; }
/* The HTML `hidden` attribute removes the element from rendering (ADR-0058). */
[hidden] { display: none; }
/* We don't paint SVG graphics; hiding it avoids flowing its <text>/markup as
   stray page text (e.g. decorative symbol grids). Icons render as nothing,
   which is what unpainted SVG already was. */
svg { display: none; }
li { display: list-item; }
h1 { font-size: 32px; font-weight: bold; margin-top: 16px; margin-bottom: 16px; }
h2 { font-size: 24px; font-weight: bold; margin-top: 14px; margin-bottom: 14px; }
h3 { font-size: 20px; font-weight: bold; margin-top: 12px; margin-bottom: 12px; }
h4 { font-size: 17px; font-weight: bold; margin-top: 10px; margin-bottom: 10px; }
h5 { font-size: 15px; font-weight: bold; margin-top: 10px; margin-bottom: 10px; }
h6 { font-size: 13px; font-weight: bold; margin-top: 10px; margin-bottom: 10px; }
p { margin-top: 8px; margin-bottom: 8px; }
ul, ol { margin-top: 8px; margin-bottom: 8px; margin-left: 24px; }
blockquote { margin-left: 24px; margin-top: 8px; margin-bottom: 8px; }
pre { white-space: pre; margin-top: 8px; margin-bottom: 8px; }
code, kbd, samp { white-space: pre; }
a { color: #154fd2; text-decoration: underline; }
b, strong { font-weight: bold; }
i, em, cite, var { font-style: italic; }
"#;

/// CSS engine built on our parser + cascade.
pub struct CssEngine {
    ua: Stylesheet,
    /// Subject index over the UA sheet, built once (the UA sheet never changes).
    ua_index: RuleIndex,
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
        let ua_index = RuleIndex::build(&ua);
        Self {
            ua,
            ua_index,
            media: MediaContext { width, height },
        }
    }

    /// Point `@media` evaluation at a new viewport (e.g. on window resize), so
    /// responsive rules re-resolve against the real width — cheap, keeps the UA
    /// index. The viewport must track the actual layout width, not a fixed guess.
    pub fn set_media(&mut self, width: u32, height: u32) {
        self.media = MediaContext { width, height };
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
        author_index: &RuleIndex,
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
            // Collect matching declarations: (origin, specificity, source-order),
            // honoring @media against the engine's viewport. The subject index
            // narrows each sheet to the rules that can match this element, instead
            // of scanning every rule (ADR-0047) — `order` stays the rule's true
            // source index so the cascade tiebreak is unchanged.
            let el = &path[path.len() - 1];
            let el = &el.siblings[el.index];
            let mut matched: Vec<MatchedRule<'_>> = Vec::new();
            for order in self.ua_index.candidates(el) {
                let rule = &self.ua.rules[order];
                if rule.applies(self.media) {
                    if let Some(spec) = rule.matches(path) {
                        matched.push((0, spec, order, &rule.declarations));
                    }
                }
            }
            for order in author_index.candidates(el) {
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

            for (_, _, _, decls) in &matched {
                apply_declarations(&mut style, decls, parent.font_size, &vars);
            }
            if let Some(decls) = &inline {
                apply_declarations(&mut style, decls, parent.font_size, &vars);
            }
        }

        // The element children, reduced for sibling / :nth-child matching, shared
        // across this level via `Rc` so the cascade stays O(n).
        let child_siblings: Rc<[SiblingRef]> = node
            .children()
            .filter(|c| c.is_element())
            .map(sibling_ref)
            .collect::<Vec<_>>()
            .into();
        let mut elem_index = 0usize;
        let children = node
            .children()
            .map(|child| match child.text() {
                Some(t) => StyledChild::Text(t.to_string()),
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
                    );
                    elem_index += 1;
                    StyledChild::Element(Box::new(styled))
                }
            })
            .collect();

        path.pop();
        StyledNode {
            tag: node.tag().to_string(),
            attrs: node.attrs().to_vec(),
            style,
            children,
            node_id: node.id(),
        }
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
        let author_index = RuleIndex::build(&author);
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
        );
        StyledDom { root: styled }
    }
}

fn sibling_ref(node: NodeRef<'_>) -> SiblingRef {
    SiblingRef {
        tag: node.tag().to_string(),
        id: node.attr("id").map(str::to_string),
        classes: node
            .attr("class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        attrs: node.attrs().to_vec(),
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
    inline: Option<&[(String, String)]>,
) -> Vars {
    let is_custom = |d: &(String, String)| d.0.starts_with("--");
    let declares = matched.iter().any(|(_, _, _, d)| d.iter().any(is_custom))
        || inline.is_some_and(|d| d.iter().any(is_custom));
    if !declares {
        return parent.clone();
    }
    let mut map = (**parent).clone();
    let mut insert = |decls: &[(String, String)]| {
        for (p, v) in decls {
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
fn resolve_value(value: &str, vars: &Vars, em: f32) -> String {
    let has_var = value.contains("var(");
    let has_calc = value.contains("calc(");
    if !has_var && !has_calc {
        return value.to_string();
    }
    let substituted = if has_var {
        substitute_vars(value, vars, 0)
    } else {
        value.to_string()
    };
    if substituted.contains("calc(") {
        eval_calcs(&substituted, em)
    } else {
        substituted
    }
}

/// Replace every `var(--name[, fallback])` in `input` with the custom property's
/// value (resolved recursively, since a custom property may itself reference
/// others), falling back to the comma fallback or empty. Guarded against cycles
/// and runaway depth.
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
        let replacement = match vars.get(&key) {
            Some(v) => substitute_vars(v, vars, depth + 1),
            None => match fallback {
                Some(fb) => substitute_vars(fb.trim(), vars, depth + 1),
                None => String::new(),
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
fn split_top_comma(s: &str) -> (&str, Option<&str>) {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return (&s[..i], Some(&s[i + 1..])),
            _ => {}
        }
    }
    (s, None)
}

/// Replace every `calc(...)` in `input` with its evaluated length/number (in px
/// where a unit is involved); leave a `calc()` we cannot evaluate untouched.
fn eval_calcs(input: &str, em: f32) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(pos) = rest.find("calc(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 5..];
        let Some((inner, tail)) = take_group(after) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        // Nested calc() inside this group resolves first.
        let inner = eval_calcs(inner, em);
        match eval_calc_expr(&inner, em) {
            Some(px) => {
                // Integer-ish results print without a trailing `.0`.
                if (px.round() - px).abs() < 1e-4 {
                    out.push_str(&format!("{}px", px.round() as i64));
                } else {
                    out.push_str(&format!("{px}px"));
                }
            }
            None => {
                out.push_str("calc(");
                out.push_str(&inner);
                out.push(')');
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Evaluate a `calc()` expression body to a px value, supporting `+ - * /`,
/// parentheses, and px/em/rem/pt/% units (others convert via the same rules the
/// length parser uses). Returns `None` if it cannot be reduced to a number.
fn eval_calc_expr(expr: &str, em: f32) -> Option<f32> {
    let tokens = tokenize_calc(expr, em)?;
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

/// A `calc()` token: a resolved px/number value or an operator/paren.
#[derive(Clone, Copy, PartialEq)]
enum CalcTok {
    Num(f32),
    Plus,
    Minus,
    Mul,
    Div,
    Open,
    Close,
}

fn tokenize_calc(expr: &str, em: f32) -> Option<Vec<CalcTok>> {
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
                let px = match unit.to_ascii_lowercase().as_str() {
                    "" | "px" => num,
                    "em" => num * em,
                    "rem" => num * 16.0,
                    "pt" => num * 96.0 / 72.0,
                    "%" => num / 100.0 * em,
                    _ => return None,
                };
                toks.push(CalcTok::Num(px));
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

    fn expr(&mut self) -> Option<f32> {
        let mut v = self.term()?;
        while let Some(op @ (CalcTok::Plus | CalcTok::Minus)) = self.peek() {
            self.i += 1;
            let rhs = self.term()?;
            v = if op == CalcTok::Plus {
                v + rhs
            } else {
                v - rhs
            };
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f32> {
        let mut v = self.factor()?;
        while let Some(op @ (CalcTok::Mul | CalcTok::Div)) = self.peek() {
            self.i += 1;
            let rhs = self.factor()?;
            if op == CalcTok::Mul {
                v *= rhs;
            } else {
                if rhs == 0.0 {
                    return None;
                }
                v /= rhs;
            }
        }
        Some(v)
    }

    fn factor(&mut self) -> Option<f32> {
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

fn apply_declarations(
    style: &mut ComputedStyle,
    decls: &[(String, String)],
    parent_font_size: u32,
    vars: &Vars,
) {
    for (prop, value) in decls {
        // Custom properties are collected separately (`collect_vars`); they do
        // not set any computed value here.
        if prop.starts_with("--") {
            continue;
        }
        // Resolve `var()` references and `calc()` math before parsing the value.
        // `em` for `calc()` uses the element's current font size.
        let resolved = resolve_value(value, vars, style.font_size as f32);
        let v = resolved.trim();
        match prop.as_str() {
            "color" => {
                if let Some(c) = parse_color(v) {
                    style.color = c;
                }
            }
            "background" | "background-color" => {
                if let Some(c) = parse_bg_color(v) {
                    style.background = (c.a != 0).then_some(c);
                }
                // The `background` shorthand may also carry an image/gradient and,
                // crucially for real sites, a `<position> / <size>` group such as
                // `center / cover` (ADR-0044/0045).
                if prop == "background" {
                    style.background_image = parse_url_value(v);
                    style.background_gradient = parse_gradient(v).map(Box::new);
                    apply_background_shorthand_geometry(style, v);
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
                }
            }
            "font-weight" => style.font.bold = is_bold(v),
            "font-style" => {
                let low = v.to_ascii_lowercase();
                style.font.italic = low == "italic" || low == "oblique";
            }
            "font" => apply_font_shorthand(style, v, parent_font_size),
            // `font-family` is intentionally not honored: the font set is fixed
            // to the bundled face (a privacy/anti-fingerprinting property — no
            // system or downloadable fonts are read), so families and `@font-face`
            // text render in the bundled font. Consumed here so the decision is
            // explicit rather than a silent fall-through (ADR-0038).
            "font-family" => {}
            "text-align" => {
                style.text_align = match v.to_ascii_lowercase().as_str() {
                    "center" => TextAlign::Center,
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
            "text-decoration" | "text-decoration-line" => {
                let low = v.to_ascii_lowercase();
                if low.contains("underline") {
                    style.underline = true;
                } else if low.contains("none") {
                    style.underline = false;
                }
            }
            "display" => {
                if let Some(d) = parse_display(v) {
                    style.display = d;
                }
            }
            "margin" => apply_margin_shorthand(style, v, style.font_size as f32),
            "margin-top" => {
                if let Some(m) = parse_len(v, style.font_size as f32) {
                    style.margin_top = m;
                }
            }
            "margin-bottom" => {
                if let Some(m) = parse_len(v, style.font_size as f32) {
                    style.margin_bottom = m;
                }
            }
            "margin-left" => {
                style.margin_left_auto = v.trim().eq_ignore_ascii_case("auto");
                if let Some(m) = parse_len(v, style.font_size as f32) {
                    style.margin_left = m;
                }
            }
            "margin-right" => {
                style.margin_right_auto = v.trim().eq_ignore_ascii_case("auto");
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
            "white-space" => style.preformatted = v.to_ascii_lowercase().starts_with("pre"),
            "visibility" => {
                style.visibility = match v.to_ascii_lowercase().as_str() {
                    "hidden" | "collapse" => Visibility::Hidden,
                    "visible" => Visibility::Visible,
                    _ => style.visibility,
                }
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
            "grid-auto-columns" => {
                style.grid_auto_columns = split_top(v.trim())
                    .first()
                    .map(|t| parse_one_track(t, style.font_size as f32));
            }
            "grid-auto-flow" => {
                // `column` / `column dense` → column-major auto-placement.
                style.grid_auto_flow_column = v.to_ascii_lowercase().contains("column");
            }
            "grid-column" => {
                style.grid_column_span = parse_grid_span(v);
                if grid_line_is_named(v) {
                    style.grid_named_place = true;
                }
            }
            "grid-row" => style.grid_row_span = parse_grid_span(v),
            "grid-area" => {
                // `grid-area: <name>` places the item into the container's matching
                // `grid-template-areas` rectangle (ADR-0051). A line-number form
                // (`a / b / c / d`) falls back to the content-track heuristic.
                let t = v.trim();
                if is_grid_area_name(t) {
                    style.grid_area = Some(t.to_string());
                } else if grid_line_is_named(v) {
                    style.grid_named_place = true;
                }
            }
            "grid-template-areas" => {
                style.grid_template_areas = parse_grid_template_areas(v);
            }
            "grid-template" | "grid" => {
                // `grid-template: <rows> / <cols>` (and the inline-areas form). The
                // common shells use this plus a separate `grid-template-areas`.
                if v.contains('\'') || v.contains('"') {
                    style.grid_template_areas = parse_grid_template_areas(v);
                    if let Some((_, cols)) = split_top_once(v, '/') {
                        let (tracks, auto_fill) = parse_grid_template(cols, style.font_size as f32);
                        style.grid_template_columns = tracks;
                        style.grid_auto_fill = auto_fill;
                    }
                } else if let Some((rows, cols)) = split_top_once(v, '/') {
                    style.grid_template_rows = parse_tracks(rows, style.font_size as f32);
                    let (tracks, auto_fill) = parse_grid_template(cols, style.font_size as f32);
                    style.grid_template_columns = tracks;
                    style.grid_auto_fill = auto_fill;
                }
            }
            // Still ignored (no compositor/timeline): animation*, transition*,
            // transform, and everything else.
            _ => {}
        }
    }
}

fn parse_bg_color(v: &str) -> Option<Color> {
    parse_color(v).or_else(|| v.split_whitespace().find_map(parse_color))
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
        "block" | "table" | "table-row" | "table-cell" | "flow-root" => Display::Block,
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
fn split_top(v: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in v.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
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
fn split_top_commas(v: &str) -> Vec<String> {
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
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
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
            // Swap when the order is vertical-first or horizontal-second.
            if *a0 == 1 || *a1 == 0 {
                Some(ImagePos { x: *f1, y: *f0 })
            } else {
                Some(ImagePos { x: *f0, y: *f1 })
            }
        }
    }
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
    if let Some(slash) = masked.find('/') {
        if let Some(tok) = masked[slash + 1..].split_whitespace().next() {
            style.background_size = parse_image_fit(tok);
        }
        let parts: Vec<(u8, f32)> = masked[..slash]
            .split_whitespace()
            .filter_map(classify_pos_tok)
            .collect();
        if let Some(p) = combine_pos(&parts[parts.len().saturating_sub(2)..]) {
            style.background_position = p;
        }
    } else if let Some(f) = masked
        .split_whitespace()
        .map(parse_image_fit)
        .find(|f| *f != ImageFit::Fill)
    {
        style.background_size = f;
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

/// Whether `t` is a single `grid-area` *name* (an identifier), vs. a line-number
/// placement (`1 / 2 / 3 / 4`) or `auto`.
fn is_grid_area_name(t: &str) -> bool {
    !t.is_empty()
        && !t.eq_ignore_ascii_case("auto")
        && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse `grid-template-areas` into rows of cell names; a `.`-only cell is empty
/// (`String::new`). e.g. `'a a' 'b c'` → `[[a,a],[b,c]]` (ADR-0051).
fn parse_grid_template_areas(v: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut chars = v.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '\'' || c == '"' {
            chars.next();
            let mut s = String::new();
            for ch in chars.by_ref() {
                if ch == c {
                    break;
                }
                s.push(ch);
            }
            let cells: Vec<String> = s
                .split_whitespace()
                .map(|t| {
                    if t.chars().all(|c| c == '.') {
                        String::new()
                    } else {
                        t.to_string()
                    }
                })
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        } else {
            chars.next();
        }
    }
    rows
}

/// Split `v` once on the first top-level `sep` (not nested in `()`), trimming both
/// sides — for `grid-template: <rows> / <cols>` where `<cols>` may hold `minmax()`.
fn split_top_once(v: &str, sep: char) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, c) in v.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if c == sep && depth == 0 => {
                return Some((v[..i].trim(), v[i + c.len_utf8()..].trim()));
            }
            _ => {}
        }
    }
    None
}

/// Parse a `grid-column`/`grid-row` placement into a track *span* count:
/// `span N`, `a / b` (→ b−a), `a / span N`, else 1 (ADR-0038).
fn parse_grid_span(v: &str) -> u32 {
    let v = v.trim().to_ascii_lowercase();
    if let Some(rest) = v.strip_prefix("span") {
        return rest.trim().parse::<u32>().unwrap_or(1).max(1);
    }
    if let Some((a, b)) = v.split_once('/') {
        let b = b.trim();
        if let Some(n) = b.strip_prefix("span") {
            return n.trim().parse::<u32>().unwrap_or(1).max(1);
        }
        if let (Ok(ai), Ok(bi)) = (a.trim().parse::<i32>(), b.parse::<i32>()) {
            return (bi - ai).unsigned_abs().max(1);
        }
    }
    1
}

fn apply_font_shorthand(style: &mut ComputedStyle, v: &str, parent_font_size: u32) {
    for token in v.split_whitespace() {
        let t = token.to_ascii_lowercase();
        if t == "bold" || t == "bolder" {
            style.font.bold = true;
        } else if t == "italic" || t == "oblique" {
            style.font.italic = true;
        } else if t.starts_with(|c: char| c.is_ascii_digit()) {
            // In the `font` shorthand the size carries a unit (or is
            // `size/line-height`); a *bare* number is the font **weight**
            // (100–900), never the size — e.g. `font: 600 16px/1.4 Arial` must not
            // set font-size to 600 (ADR-0057).
            if t.contains('/') || t.bytes().any(|b| b.is_ascii_alphabetic() || b == b'%') {
                let size_tok = t.split('/').next().unwrap_or(&t);
                if let Some(px) = parse_size(size_tok, parent_font_size) {
                    style.font_size = px;
                }
            } else if let Ok(weight) = t.parse::<u32>() {
                style.font.bold = weight >= 600;
            }
        }
    }
}

fn apply_margin_shorthand(style: &mut ComputedStyle, v: &str, em_base: f32) {
    let toks: Vec<&str> = v.split_whitespace().collect();
    let parts: Vec<i32> = toks
        .iter()
        .map(|p| parse_len(p, em_base).unwrap_or(0))
        .collect();
    // Track which sides are `auto` (for centering): horizontal sides are index 1
    // (right) and 3 (left) in the 4-value form, or index 1 in the 2/3-value form.
    let is_auto = |i: usize| toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("auto"));
    let (top, bottom, left, l_auto, r_auto) = match parts.len() {
        1 => (parts[0], parts[0], parts[0], is_auto(0), is_auto(0)),
        2 | 3 => (
            parts[0],
            if parts.len() == 3 { parts[2] } else { parts[0] },
            parts[1],
            is_auto(1),
            is_auto(1),
        ),
        n if n >= 4 => (parts[0], parts[2], parts[3], is_auto(3), is_auto(1)),
        _ => return,
    };
    style.margin_top = top;
    style.margin_bottom = bottom;
    style.margin_left = left;
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
        "ch" => num * em_base * 0.5, // ~width of '0'; 0.5em is a good approximation
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
    // Intrinsic sizing keywords (resolved by measuring content at layout). The
    // `fit-content(<len>)` function clamps to its content; we approximate it as
    // max-content (ADR-0055).
    if v == "max-content" || v == "-webkit-max-content" || v.starts_with("fit-content") {
        return Some(Len::MaxContent);
    }
    if v == "min-content" || v == "-webkit-min-content" {
        return Some(Len::MinContent);
    }
    let (num, unit) = split_num_unit(&v)?;
    Some(match unit.as_str() {
        "%" => Len::Pct(num),
        "px" | "" => Len::Px(num.round() as i32),
        "em" => Len::Px((num * em_base).round() as i32),
        "rem" => Len::Px((num * 16.0).round() as i32),
        "pt" => Len::Px((num * 96.0 / 72.0).round() as i32),
        "vw" | "vmax" => Len::Vw(num),
        "vh" | "vmin" => Len::Vh(num),
        _ => return None,
    })
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

/// Resolve `line-height` to px against `font_size`: `normal` → `None` (engine
/// default), a unitless number → `n × font-size`, `%` → `pct × font-size`, else a
/// length (ADR-0041).
fn parse_line_height(v: &str, font_size: u32) -> Option<i32> {
    let t = v.trim().to_ascii_lowercase();
    if t == "normal" || t.is_empty() {
        return None;
    }
    if let Some(pct) = t
        .strip_suffix('%')
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some((pct / 100.0 * font_size as f32).round().max(0.0) as i32);
    }
    if let Ok(n) = t.parse::<f32>() {
        return Some((n * font_size as f32).round().max(0.0) as i32);
    }
    parse_len(&t, font_size as f32).map(|px| px.max(0))
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
    fn author_and_inline_cascade() {
        let html = "<style>p{color:green} #x{color:red}</style>\
                    <p id='x' style='color:#0000ff'>hi</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        // inline beats #id beats type.
        assert_eq!(p.style.color, Color::rgb(0, 0, 255));
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

    #[test]
    fn media_query_calc_breakpoint_does_not_vacuously_match() {
        // `max-width: calc(640px - 1px)` (=639) must parse, not fail-and-match —
        // otherwise a mobile rule leaks into desktop (ADR-0054).
        let html = "<style>@media screen and (max-width: calc(640px - 1px)) \
                    { p { color: #ff0000 } }</style><p>x</p>";
        // 1920 > 639: the mobile rule must NOT apply.
        let wide = CssEngine::with_media(1920, 1080).style(&parse_html(html));
        assert_eq!(first(&wide.root, "p").unwrap().style.color, Color::BLACK);
        // 480 <= 639: it does.
        let narrow = CssEngine::with_media(480, 800).style(&parse_html(html));
        assert_eq!(
            first(&narrow.root, "p").unwrap().style.color,
            Color::rgb(0xff, 0, 0)
        );
    }

    #[test]
    fn media_type_and_feature_combined_track_the_real_viewport() {
        // `screen and (min-width: 1680px)` — media type + feature, the form the
        // Vector skin uses for its desktop grid header. Must resolve against the
        // engine's real viewport, not a fixed guess (ADR-0052).
        let html =
            "<style>@media screen and (min-width: 1680px) { p { color: #00ff00 } }</style><p>x</p>";
        let small = CssEngine::with_media(1280, 800).style(&parse_html(html));
        assert_eq!(first(&small.root, "p").unwrap().style.color, Color::BLACK);
        let big = CssEngine::with_media(1920, 1080).style(&parse_html(html));
        assert_eq!(
            first(&big.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0)
        );
        // `set_media` re-points evaluation (the resize path).
        let mut eng = CssEngine::with_media(1280, 800);
        eng.set_media(1920, 1080);
        let re = eng.style(&parse_html(html));
        assert_eq!(
            first(&re.root, "p").unwrap().style.color,
            Color::rgb(0, 0xff, 0)
        );
    }

    #[test]
    fn hidden_attribute_is_display_none() {
        // The HTML `hidden` attribute removes the element (ADR-0058) — common for
        // screen-reader / aria-live regions that otherwise pollute the render.
        let dom = CssEngine::new().style(&parse_html("<p hidden>x</p>"));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(p.style.display, Display::None);
        // A normal <p> is unaffected.
        let shown = CssEngine::new().style(&parse_html("<p>x</p>"));
        assert_ne!(
            first(&shown.root, "p").unwrap().style.display,
            Display::None
        );
    }

    #[test]
    fn font_shorthand_weight_is_not_size() {
        // `font: 600 16px/1.4 Arial` → size 16 (from the unit token); the bare 600
        // is the weight (semibold), not a 600px font-size (ADR-0057).
        let dom = CssEngine::new().style(&parse_html("<p style='font: 600 16px/1.4 Arial'>x</p>"));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(
            p.style.font_size, 16,
            "size comes from 16px, not the weight 600"
        );
        assert!(p.style.font.bold, "600 is a semibold weight");
        // A bare `font: italic 700 1.2em serif` still finds the em size.
        let dom2 = CssEngine::new().style(&parse_html(
            "<div style='font-size:20px'><p style='font: italic 700 1.5em serif'>y</p></div>",
        ));
        let p2 = first(&dom2.root, "p").unwrap();
        assert_eq!(p2.style.font_size, 30, "1.5em of the 20px parent");
        assert!(p2.style.font.italic && p2.style.font.bold);
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
    fn calc_evaluates_length_math() {
        // calc with mixed absolute units, precedence, and a var() operand (the
        // element references its own custom property).
        let html =
            "<p style='--g:8px;margin-top:calc(2 * 4px + 1rem);margin-left:calc(var(--g) * 3)'>x</p>";
        let dom = CssEngine::new().style(&parse_html(html));
        let p = first(&dom.root, "p").unwrap();
        assert_eq!(p.style.margin_top, 24, "2*4 + 16(rem) = 24");
        assert_eq!(p.style.margin_left, 24, "8 * 3 = 24");
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
    fn supports_block_rules_are_applied_and_font_face_is_skipped() {
        // @supports content is applied (condition not evaluated); @font-face never
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
        assert_eq!(fit("object-fit: none"), ImageFit::Fill);

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
        // Lengths are ignored (no box at parse time) — falls back to default.
        assert_eq!(pos("object-position: 10px 20px"), ImagePos::CENTER);
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
        // no slash the size stays the Fill default.
        let g = s("background: linear-gradient(red 50%, blue)");
        assert_eq!(g.background_size, ImageFit::Fill);
        assert_eq!(g.background_position, ImagePos::TOP_LEFT);
        assert!(g.background_gradient.is_some());
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

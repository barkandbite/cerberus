//! Our CSS engine (`CssEngine: StyleEngine`): parse the UA + author stylesheets,
//! run the cascade, and produce a `StyledDom`. Bootstrapped — no dependencies.
//!
//! **Speed-first / raw render (per product directive):** properties that exist
//! only to *delay* or *animate* the appearance of content — `opacity`,
//! `animation*`, `transition*`, `transform`, `visibility` — are deliberately
//! **ignored**. Content that a site hides behind a fade/scroll-in is therefore
//! shown immediately, at full visibility.

mod color;
mod parser;

pub use color::parse_color;

use cerberus_dom::{Document, Element, Node};
use cerberus_style::{
    ComputedStyle, Display, StyleEngine, StyledChild, StyledDom, StyledNode, TextAlign,
};
use cerberus_types::Color;
use parser::{parse_declaration_block, parse_stylesheet, ElemRef, Specificity, Stylesheet};

/// A matched rule during the cascade: (origin, specificity, source-order, decls).
type MatchedRule<'a> = (u8, Specificity, usize, &'a Vec<(String, String)>);

/// The user-agent default stylesheet.
const UA_CSS: &str = r#"
html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, li, section, article,
header, footer, nav, main, aside, blockquote, pre, figure, figcaption, form,
table, tr, hr, dl, dt, dd, fieldset, address { display: block; }
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
}

impl CssEngine {
    /// Build an engine with the bundled UA stylesheet parsed.
    pub fn new() -> Self {
        Self {
            ua: parse_stylesheet(UA_CSS),
        }
    }

    fn build(
        &self,
        element: &Element,
        parent: &ComputedStyle,
        path: &mut Vec<ElemRef>,
        author: &Stylesheet,
    ) -> StyledNode {
        let is_root = element.tag == "#root";
        let mut style = if is_root {
            ComputedStyle::initial()
        } else {
            parent.inherit()
        };

        path.push(elem_ref(element));

        if !is_root {
            // Collect matching declarations: (origin, specificity, source-order).
            let mut matched: Vec<MatchedRule<'_>> = Vec::new();
            for (order, rule) in self.ua.rules.iter().enumerate() {
                if let Some(spec) = rule.matches(path) {
                    matched.push((0, spec, order, &rule.declarations));
                }
            }
            for (order, rule) in author.rules.iter().enumerate() {
                if let Some(spec) = rule.matches(path) {
                    matched.push((1, spec, order, &rule.declarations));
                }
            }
            matched.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
            for (_, _, _, decls) in &matched {
                apply_declarations(&mut style, decls, parent.font_size);
            }

            // Inline `style=` has the highest priority.
            if let Some(inline) = element.attr("style") {
                let decls = parse_declaration_block(inline);
                apply_declarations(&mut style, &decls, parent.font_size);
            }
        }

        let children = element
            .children
            .iter()
            .map(|child| match child {
                Node::Text(t) => StyledChild::Text(t.clone()),
                Node::Element(e) => StyledChild::Element(self.build(e, &style, path, author)),
            })
            .collect();

        path.pop();
        StyledNode {
            tag: element.tag.clone(),
            attrs: element.attrs.clone(),
            style,
            children,
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
        let author = parse_stylesheet(&collect_author_css(&doc.root));
        let mut path = Vec::new();
        let root = self.build(&doc.root, &ComputedStyle::initial(), &mut path, &author);
        StyledDom { root }
    }
}

fn elem_ref(element: &Element) -> ElemRef {
    ElemRef {
        tag: element.tag.clone(),
        id: element.attr("id").map(str::to_string),
        classes: element
            .attr("class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
    }
}

/// Concatenate the text of every `<style>` element (the HTML parser keeps it raw).
fn collect_author_css(element: &Element) -> String {
    let mut css = String::new();
    if element.tag == "style" {
        css.push_str(&element.text_content());
        css.push('\n');
    }
    for child in &element.children {
        if let Node::Element(e) = child {
            css.push_str(&collect_author_css(e));
        }
    }
    css
}

fn apply_declarations(
    style: &mut ComputedStyle,
    decls: &[(String, String)],
    parent_font_size: u32,
) {
    for (prop, value) in decls {
        let v = value.trim();
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
            "text-align" => {
                style.text_align = match v.to_ascii_lowercase().as_str() {
                    "center" => TextAlign::Center,
                    "right" | "end" => TextAlign::Right,
                    "left" | "start" => TextAlign::Left,
                    _ => style.text_align,
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
                if let Some(m) = parse_len(v, style.font_size as f32) {
                    style.margin_left = m;
                }
            }
            "white-space" => style.preformatted = v.to_ascii_lowercase().starts_with("pre"),
            // Deliberately ignored (speed-first / raw render): opacity, animation*,
            // transition*, transform, visibility, and everything else.
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

fn parse_display(v: &str) -> Option<Display> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "none" => Display::None,
        "list-item" => Display::ListItem,
        "inline" | "inline-block" | "inline-flex" | "inline-grid" | "contents" => Display::Inline,
        "block" | "flex" | "grid" | "table" | "table-row" | "table-cell" | "flow-root" => {
            Display::Block
        }
        _ => return None,
    })
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
            }
        }
    }
}

fn apply_margin_shorthand(style: &mut ComputedStyle, v: &str, em_base: f32) {
    let parts: Vec<i32> = v
        .split_whitespace()
        .map(|p| parse_len(p, em_base).unwrap_or(0))
        .collect();
    let (top, bottom, left) = match parts.len() {
        1 => (parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[0], parts[1]),
        3 => (parts[0], parts[2], parts[1]),
        n if n >= 4 => (parts[0], parts[2], parts[3]),
        _ => return,
    };
    style.margin_top = top;
    style.margin_bottom = bottom;
    style.margin_left = left;
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
        _ => return None,
    })
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
    use cerberus_dom::parse_trivial;

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
        let dom =
            CssEngine::new().style(&parse_trivial("<body><h1>T</h1><a href='/x'>l</a></body>"));
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
        let dom = CssEngine::new().style(&parse_trivial(html));
        let p = first(&dom.root, "p").unwrap();
        // inline beats #id beats type.
        assert_eq!(p.style.color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn display_none_and_background() {
        let html =
            "<div style='display:none'>x</div><section style='background:#ffffff'>y</section>";
        let dom = CssEngine::new().style(&parse_trivial(html));
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
    fn opacity_and_animation_are_ignored_for_raw_render() {
        // Content hidden behind a fade/scroll-in still renders normally.
        let html = "<p style='opacity:0; animation: fade 3s; transition: all 2s'>visible</p>";
        let dom = CssEngine::new().style(&parse_trivial(html));
        let p = first(&dom.root, "p").unwrap();
        // We simply never read opacity/animation; the element is a normal block.
        assert_eq!(p.style.display, Display::Block);
    }
}

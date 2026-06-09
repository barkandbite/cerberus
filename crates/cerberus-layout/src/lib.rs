//! Layout: turn a `Document` into a `LaidOut` (a display list plus link boxes).
//!
//! `LayoutEngine` is the seam so we can recode layout freely. `BlockLayout` is a
//! small **block/inline flow** engine: block elements (`p`, `div`, `h1`-`h6`,
//! `li`, …) stack vertically; inline content (text, `<a>`, `<b>`, …) flows and
//! word-wraps within a block. `<head>`/metadata is skipped, links are styled and
//! emitted as clickable boxes. Real CSS + a real box model are still ahead (M2).

use cerberus_dom::{Document, Element, Node};
use cerberus_paint::{DisplayItem, DisplayList, TextShaper};
use cerberus_types::{Color, Point, Rect, Size};

/// A clickable link region produced by layout (in layout-local coordinates).
#[derive(Clone, Debug, PartialEq)]
pub struct LinkBox {
    pub rect: Rect,
    pub href: String,
}

/// The result of laying out a document: what to paint, and where the links are.
#[derive(Clone, Debug, Default)]
pub struct LaidOut {
    pub display: DisplayList,
    pub links: Vec<LinkBox>,
}

/// Produces a `LaidOut` from a `Document` for a given viewport.
pub trait LayoutEngine: Send {
    /// Lay out `doc` into `viewport`, shaping text with `shaper`.
    fn layout(&mut self, doc: &Document, viewport: Size, shaper: &dyn TextShaper) -> LaidOut;
}

/// Minimal block/inline flow layout.
#[derive(Clone, Copy, Debug)]
pub struct BlockLayout {
    /// Left margin in pixels.
    pub margin: i32,
    /// Body text size in pixels.
    pub body_px: u32,
    /// Heading text size in pixels.
    pub heading_px: u32,
    /// Body text color.
    pub color: Color,
    /// Link text/underline color.
    pub link_color: Color,
}

impl Default for BlockLayout {
    fn default() -> Self {
        Self {
            margin: 8,
            body_px: 16,
            heading_px: 28,
            color: Color::BLACK,
            link_color: Color::rgb(0x15, 0x4f, 0xd2),
        }
    }
}

const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
const SKIP: &[&str] = &[
    "head", "title", "meta", "link", "base", "script", "style", "noscript",
];
const INLINE: &[&str] = &[
    "a", "b", "i", "span", "em", "strong", "code", "small", "sub", "sup", "u", "label", "abbr",
    "mark", "time", "cite", "q", "s", "big", "tt", "kbd", "samp", "var",
];

fn is_inline(tag: &str) -> bool {
    INLINE.contains(&tag)
}

impl LayoutEngine for BlockLayout {
    fn layout(&mut self, doc: &Document, viewport: Size, shaper: &dyn TextShaper) -> LaidOut {
        let max_width = viewport
            .w
            .saturating_sub(2 * self.margin.max(0) as u32)
            .max(16);
        let mut ctx = Ctx::new(self.margin, max_width);
        self.walk(&doc.root, self.body_px, None, &mut ctx, shaper);
        ctx.flush_line();
        LaidOut {
            display: ctx.display,
            links: ctx.links,
        }
    }
}

impl BlockLayout {
    fn walk(
        &self,
        element: &Element,
        inherited_px: u32,
        in_link: Option<String>,
        ctx: &mut Ctx,
        shaper: &dyn TextShaper,
    ) {
        let tag = element.tag.as_str();
        if SKIP.contains(&tag) {
            return;
        }
        if tag == "br" {
            ctx.line_break(inherited_px);
            return;
        }
        if tag == "hr" {
            ctx.flush_line();
            ctx.rule();
            return;
        }

        let px = if HEADINGS.contains(&tag) {
            self.heading_px
        } else {
            inherited_px
        };
        let block = !is_inline(tag) && tag != "#root";

        // `<a href>` opens a link span; nested inline keeps the enclosing href.
        let link = if tag == "a" {
            element.attr("href").map(str::to_string).or(in_link)
        } else {
            in_link
        };

        if block {
            ctx.flush_line();
            if tag == "li" {
                ctx.bullet(px, self, shaper);
            }
        }

        for child in &element.children {
            match child {
                Node::Text(text) => ctx.add_text(text, px, link.as_deref(), self, shaper),
                Node::Element(child_el) => self.walk(child_el, px, link.clone(), ctx, shaper),
            }
        }

        if block {
            ctx.flush_line();
            ctx.block_gap(px);
        }
    }
}

/// Flow state: a pen position plus the line's tallest text size.
struct Ctx {
    display: DisplayList,
    links: Vec<LinkBox>,
    margin: i32,
    max_width: u32,
    x: i32,
    y: i32,
    line_px: u32,
}

impl Ctx {
    fn new(margin: i32, max_width: u32) -> Self {
        Self {
            display: DisplayList::new(),
            links: Vec::new(),
            margin,
            max_width,
            x: margin,
            y: margin,
            line_px: 0,
        }
    }

    fn newline(&mut self) {
        self.y += line_height(self.line_px.max(1));
        self.x = self.margin;
        self.line_px = 0;
    }

    /// End the current line if it has content.
    fn flush_line(&mut self) {
        if self.x != self.margin {
            self.newline();
        }
    }

    /// Force a line break (e.g. `<br>`), even on an empty line.
    fn line_break(&mut self, px: u32) {
        self.line_px = self.line_px.max(px);
        self.newline();
    }

    fn block_gap(&mut self, px: u32) {
        self.y += (px / 2) as i32;
    }

    fn rule(&mut self) {
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(self.margin, self.y, self.max_width, 1),
            color: Color::rgb(0xCC, 0xCC, 0xCC),
        });
        self.y += 8;
    }

    fn bullet(&mut self, px: u32, layout: &BlockLayout, shaper: &dyn TextShaper) {
        self.add_word("\u{2022}", px, None, layout, shaper);
        self.x += space_width(shaper, px) as i32;
    }

    fn add_text(
        &mut self,
        text: &str,
        px: u32,
        link: Option<&str>,
        layout: &BlockLayout,
        shaper: &dyn TextShaper,
    ) {
        for word in text.split_whitespace() {
            self.add_word(word, px, link, layout, shaper);
        }
    }

    fn add_word(
        &mut self,
        word: &str,
        px: u32,
        link: Option<&str>,
        layout: &BlockLayout,
        shaper: &dyn TextShaper,
    ) {
        let glyphs = shaper.shape(word, px);
        let word_w: u32 = glyphs.iter().map(|g| g.advance).sum();
        let gap = if self.x == self.margin {
            0
        } else {
            space_width(shaper, px) as i32
        };

        // Wrap before this word if it would overflow the current line.
        if self.x != self.margin
            && (self.x - self.margin) as u32 + gap as u32 + word_w > self.max_width
        {
            self.newline();
        } else {
            self.x += gap;
        }

        let color = if link.is_some() {
            layout.link_color
        } else {
            layout.color
        };
        self.display.push(DisplayItem::Glyphs {
            origin: Point::new(self.x, self.y),
            glyphs,
            color,
        });
        if let Some(href) = link {
            let h = px as i32;
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(self.x, self.y + h, word_w, 1),
                color: layout.link_color,
            });
            self.links.push(LinkBox {
                rect: Rect::new(self.x, self.y, word_w, (h + h / 3).max(1) as u32),
                href: href.to_string(),
            });
        }

        self.x += word_w as i32;
        self.line_px = self.line_px.max(px);
    }
}

fn line_height(px: u32) -> i32 {
    px as i32 + px as i32 / 2
}

fn space_width(shaper: &dyn TextShaper, px: u32) -> u32 {
    shaper.shape(" ", px).iter().map(|g| g.advance).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_trivial;
    use cerberus_paint::MonoShaper;

    fn glyph_ys(laid: &LaidOut) -> Vec<i32> {
        laid.display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some(origin.y),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn inline_content_flows_on_one_line() {
        let doc = parse_trivial("<p>Hello <b>brave</b> world</p>");
        let laid = BlockLayout::default().layout(&doc, Size::new(400, 200), &MonoShaper);
        let ys = glyph_ys(&laid);
        assert_eq!(ys.len(), 3, "three words");
        assert!(ys.iter().all(|&y| y == ys[0]), "inline flows on one line");
    }

    #[test]
    fn blocks_stack_vertically() {
        let doc = parse_trivial("<h1>Title</h1><p>body</p>");
        let laid = BlockLayout::default().layout(&doc, Size::new(400, 200), &MonoShaper);
        let ys = glyph_ys(&laid);
        assert_eq!(ys.len(), 2);
        assert!(ys[1] > ys[0], "second block is below the first");
    }

    #[test]
    fn links_are_collected_with_href() {
        let doc = parse_trivial("<p><a href=\"/page\">click me</a></p>");
        let laid = BlockLayout::default().layout(&doc, Size::new(400, 200), &MonoShaper);
        assert!(!laid.links.is_empty(), "link boxes emitted");
        assert!(laid.links.iter().all(|l| l.href == "/page"));
    }

    #[test]
    fn skips_head_metadata() {
        let doc = parse_trivial("<head><title>Hidden</title><meta></head>");
        let laid = BlockLayout::default().layout(&doc, Size::new(400, 200), &MonoShaper);
        let runs = glyph_ys(&laid).len();
        assert_eq!(runs, 0, "head content is not rendered");
    }

    #[test]
    fn wraps_long_text_into_multiple_lines() {
        let words = "word ".repeat(60);
        let doc = parse_trivial(&format!("<p>{words}</p>"));
        let laid = BlockLayout::default().layout(&doc, Size::new(200, 800), &MonoShaper);
        let distinct: std::collections::BTreeSet<i32> = glyph_ys(&laid).into_iter().collect();
        assert!(distinct.len() > 1, "wrapped into multiple lines");
    }
}

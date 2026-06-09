//! Layout: turn a `Document` into a `DisplayList`.
//!
//! `LayoutEngine` is the seam so we can recode layout freely (the brief treats
//! rendering as undifferentiated heavy lifting). `BlockLayout` is a deliberately
//! minimal vertical block stacker — enough to place the built-in page's text —
//! and uses the injected `TextShaper` so it never depends on a concrete shaper.
//! The real, arena-allocated layout (see PLAN.md memory budget) is M2.

use cerberus_dom::{Document, Element, Node};
use cerberus_paint::{DisplayItem, DisplayList, GlyphBox, TextShaper};
use cerberus_types::{Color, Point, Size};

/// Produces a `DisplayList` from a `Document` for a given viewport.
pub trait LayoutEngine: Send {
    /// Lay out `doc` into `viewport`, shaping text with `shaper`.
    fn layout(&mut self, doc: &Document, viewport: Size, shaper: &dyn TextShaper) -> DisplayList;
}

/// Minimal block layout: walk the DOM, stack text runs top-to-bottom, sizing
/// headings larger than body text.
#[derive(Clone, Copy, Debug)]
pub struct BlockLayout {
    /// Left margin in pixels.
    pub margin: i32,
    /// Body text size in pixels.
    pub body_px: u32,
    /// Heading text size in pixels.
    pub heading_px: u32,
    /// Text color.
    pub color: Color,
}

impl Default for BlockLayout {
    fn default() -> Self {
        Self {
            margin: 8,
            body_px: 16,
            heading_px: 28,
            color: Color::BLACK,
        }
    }
}

impl LayoutEngine for BlockLayout {
    fn layout(&mut self, doc: &Document, viewport: Size, shaper: &dyn TextShaper) -> DisplayList {
        let mut list = DisplayList::new();
        let mut cursor_y = self.margin;
        let max_width = viewport
            .w
            .saturating_sub(2 * self.margin.max(0) as u32)
            .max(16);
        self.lay_element(
            &doc.root,
            self.body_px,
            max_width,
            shaper,
            &mut list,
            &mut cursor_y,
        );
        list
    }
}

impl BlockLayout {
    fn lay_element(
        &self,
        element: &Element,
        inherited_px: u32,
        max_width: u32,
        shaper: &dyn TextShaper,
        list: &mut DisplayList,
        cursor_y: &mut i32,
    ) {
        let px = match element.tag.as_str() {
            "h1" | "h2" | "h3" => self.heading_px,
            _ => inherited_px,
        };

        for child in &element.children {
            match child {
                Node::Text(text) => self.lay_text(text, px, max_width, shaper, list, cursor_y),
                Node::Element(child_el) => {
                    self.lay_element(child_el, px, max_width, shaper, list, cursor_y);
                }
            }
        }
    }

    /// Word-wrap `text` to `max_width`, emitting one glyph run per line.
    fn lay_text(
        &self,
        text: &str,
        px: u32,
        max_width: u32,
        shaper: &dyn TextShaper,
        list: &mut DisplayList,
        cursor_y: &mut i32,
    ) {
        let line_height = px as i32 + px as i32 / 2;
        let space = shaper.shape(" ", px);
        let space_w: u32 = space.iter().map(|g| g.advance).sum();

        let mut line: Vec<GlyphBox> = Vec::new();
        let mut line_w: u32 = 0;

        for word in text.split_whitespace() {
            let word_glyphs = shaper.shape(word, px);
            let word_w: u32 = word_glyphs.iter().map(|g| g.advance).sum();

            // Wrap before this word if it would overflow the current line.
            if !line.is_empty() && line_w + space_w + word_w > max_width {
                self.emit_line(std::mem::take(&mut line), line_height, list, cursor_y);
                line_w = 0;
            }
            if line.is_empty() {
                line.extend(word_glyphs);
                line_w = word_w;
            } else {
                line.extend(space.iter().copied());
                line.extend(word_glyphs);
                line_w += space_w + word_w;
            }
        }
        if !line.is_empty() {
            self.emit_line(line, line_height, list, cursor_y);
        }
    }

    fn emit_line(
        &self,
        glyphs: Vec<GlyphBox>,
        line_height: i32,
        list: &mut DisplayList,
        cursor_y: &mut i32,
    ) {
        list.push(DisplayItem::Glyphs {
            origin: Point::new(self.margin, *cursor_y),
            glyphs,
            color: self.color,
        });
        *cursor_y += line_height;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_trivial;
    use cerberus_paint::MonoShaper;

    #[test]
    fn lays_out_text_runs_top_to_bottom() {
        let doc = parse_trivial("<h1>Title</h1><p>body</p>");
        let list = BlockLayout::default().layout(&doc, Size::new(320, 240), &MonoShaper);

        let glyph_runs: Vec<&DisplayItem> = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .collect();
        assert_eq!(
            glyph_runs.len(),
            2,
            "one run for the heading, one for the body"
        );

        // Runs are stacked: the second starts below the first.
        let ys: Vec<i32> = list
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some(origin.y),
                _ => None,
            })
            .collect();
        assert!(ys[1] > ys[0]);
    }

    #[test]
    fn wraps_long_text_into_multiple_lines() {
        let words = "word ".repeat(60);
        let doc = parse_trivial(&format!("<p>{words}</p>"));
        let list = BlockLayout::default().layout(&doc, Size::new(200, 600), &MonoShaper);

        let runs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        assert!(
            runs > 1,
            "expected wrapping into multiple lines, got {runs}"
        );

        // Every line starts at the left margin.
        for item in &list.items {
            if let DisplayItem::Glyphs { origin, .. } = item {
                assert_eq!(origin.x, BlockLayout::default().margin);
            }
        }
    }
}

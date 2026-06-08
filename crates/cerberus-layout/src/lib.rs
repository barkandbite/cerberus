//! Layout: turn a `Document` into a `DisplayList`.
//!
//! `LayoutEngine` is the seam so we can recode layout freely (the brief treats
//! rendering as undifferentiated heavy lifting). `BlockLayout` is a deliberately
//! minimal vertical block stacker — enough to place the built-in page's text —
//! and uses the injected `TextShaper` so it never depends on a concrete shaper.
//! The real, arena-allocated layout (see PLAN.md memory budget) is M2.

use cerberus_dom::{Document, Element, Node};
use cerberus_paint::{DisplayItem, DisplayList, TextShaper};
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
    fn layout(&mut self, doc: &Document, _viewport: Size, shaper: &dyn TextShaper) -> DisplayList {
        let mut list = DisplayList::new();
        let mut cursor_y = self.margin;
        self.lay_element(&doc.root, self.body_px, shaper, &mut list, &mut cursor_y);
        list
    }
}

impl BlockLayout {
    fn lay_element(
        &self,
        element: &Element,
        inherited_px: u32,
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
                Node::Text(text) => {
                    let glyphs = shaper.shape(text, px);
                    list.push(DisplayItem::Glyphs {
                        origin: Point::new(self.margin, *cursor_y),
                        glyphs,
                        color: self.color,
                    });
                    *cursor_y += px as i32 + px as i32 / 2; // line + leading
                }
                Node::Element(child_el) => {
                    self.lay_element(child_el, px, shaper, list, cursor_y);
                }
            }
        }
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
}

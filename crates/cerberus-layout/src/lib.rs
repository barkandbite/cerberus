//! Layout: flow a styled tree into a `LaidOut` (display list + link boxes).
//!
//! `BlockLayout` is a small **block/inline flow** engine driven entirely by the
//! `ComputedStyle` on each node (from `cerberus-css`): blocks stack with their
//! margins and optional background, inline content flows and word-wraps, text
//! uses the cascaded color/size/weight/underline, `text-align` shifts lines, and
//! `display:none` is skipped. `<a href>` text also emits clickable link boxes.
//! Real box widths, floats, and positioning are still ahead.

use cerberus_paint::{DisplayItem, DisplayList, GlyphBox, TextShaper};
use cerberus_style::{ComputedStyle, Display, StyledChild, StyledDom, StyledNode, TextAlign};
use cerberus_types::{Color, FontStyle, Point, Rect, Size};

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

/// Produces a `LaidOut` from a styled document for a given viewport.
pub trait LayoutEngine: Send {
    /// Lay out `styled` into `viewport`, shaping text with `shaper`.
    fn layout(&mut self, styled: &StyledDom, viewport: Size, shaper: &dyn TextShaper) -> LaidOut;
}

/// Block/inline flow layout. The only knob is the page margin; everything else
/// comes from the cascade.
#[derive(Clone, Copy, Debug)]
pub struct BlockLayout {
    /// Page margin in pixels.
    pub margin: i32,
}

impl Default for BlockLayout {
    fn default() -> Self {
        Self { margin: 8 }
    }
}

impl LayoutEngine for BlockLayout {
    fn layout(&mut self, styled: &StyledDom, viewport: Size, shaper: &dyn TextShaper) -> LaidOut {
        let max_width = viewport
            .w
            .saturating_sub(2 * self.margin.max(0) as u32)
            .max(16) as i32;
        let mut ctx = Ctx::new(self.margin, max_width, shaper);
        ctx.walk(&styled.root, None);
        ctx.flush_line();
        LaidOut {
            display: ctx.display,
            links: ctx.links,
        }
    }
}

/// One placed run of text on the current (not-yet-aligned) line.
struct LinePiece {
    x: i32,
    y: i32,
    w: u32,
    px: u32,
    glyphs: Vec<GlyphBox>,
    color: Color,
    font: FontStyle,
    underline: bool,
    href: Option<String>,
}

/// Flow state.
struct Ctx<'a> {
    shaper: &'a dyn TextShaper,
    display: DisplayList,
    links: Vec<LinkBox>,
    left0: i32,
    right: i32,
    left: i32,
    x: i32,
    y: i32,
    line_px: u32,
    line: Vec<LinePiece>,
    line_align: TextAlign,
}

impl<'a> Ctx<'a> {
    fn new(margin: i32, max_width: i32, shaper: &'a dyn TextShaper) -> Self {
        Self {
            shaper,
            display: DisplayList::new(),
            links: Vec::new(),
            left0: margin,
            right: margin + max_width,
            left: margin,
            x: margin,
            y: margin,
            line_px: 0,
            line: Vec::new(),
            line_align: TextAlign::Left,
        }
    }

    fn walk(&mut self, node: &StyledNode, in_link: Option<&str>) {
        let style = &node.style;
        if style.display == Display::None {
            return;
        }
        match node.tag.as_str() {
            "br" => {
                self.line_break(style.font_size.max(1));
                return;
            }
            "hr" => {
                self.flush_line();
                self.rule();
                return;
            }
            "img" => {
                // Replaced element: show the alt text (real decoding is a later
                // slice; lazy-loading is ignored — speed-first, raw render).
                if let Some(alt) = node.attr("alt").map(str::trim) {
                    if !alt.is_empty() {
                        self.add_text(&format!("[{alt}]"), style, in_link);
                    }
                }
                return;
            }
            _ => {}
        }

        let href = if node.tag == "a" {
            node.attr("href").or(in_link)
        } else {
            in_link
        };

        let is_block = matches!(style.display, Display::Block | Display::ListItem);
        let saved_left = self.left;
        let (bg_index, bg_start_y) = (self.display.items.len(), self.y);

        if is_block {
            self.flush_line();
            self.y += style.margin_top;
            self.line_align = style.text_align;
            self.left += style.margin_left;
            self.x = self.left;
            if style.display == Display::ListItem {
                self.add_run("\u{2022}", style, None);
                self.x += space_width(self.shaper, style.font_size.max(1)) as i32;
            }
        }

        for child in &node.children {
            match child {
                StyledChild::Text(t) => self.add_text(t, style, href),
                StyledChild::Element(e) => self.walk(e, href),
            }
        }

        if is_block {
            self.flush_line();
            if let Some(color) = style.background {
                let h = (self.y - bg_start_y).max(0) as u32;
                if h > 0 {
                    self.display.items.insert(
                        bg_index,
                        DisplayItem::Rect {
                            rect: Rect::new(
                                self.left0,
                                bg_start_y,
                                (self.right - self.left0) as u32,
                                h,
                            ),
                            color,
                        },
                    );
                }
            }
            self.y += style.margin_bottom;
            self.left = saved_left;
            self.x = self.left;
        }
    }

    fn add_text(&mut self, text: &str, style: &ComputedStyle, href: Option<&str>) {
        if style.preformatted {
            let mut first = true;
            for line in text.split('\n') {
                if !first {
                    self.line_break(style.font_size.max(1));
                }
                first = false;
                if !line.is_empty() {
                    self.add_run(line, style, href);
                }
            }
        } else {
            for word in text.split_whitespace() {
                self.add_word(word, style, href);
            }
        }
    }

    fn add_word(&mut self, word: &str, style: &ComputedStyle, href: Option<&str>) {
        let px = style.font_size.max(1);
        let glyphs = self.shaper.shape(word, px);
        let w: u32 = glyphs.iter().map(|g| g.advance).sum();
        let gap = if self.x == self.left {
            0
        } else {
            space_width(self.shaper, px) as i32
        };
        if self.x != self.left && self.x + gap + w as i32 > self.right {
            self.newline();
        } else {
            self.x += gap;
        }
        self.push_piece(px, w, glyphs, style, href);
    }

    fn add_run(&mut self, text: &str, style: &ComputedStyle, href: Option<&str>) {
        let px = style.font_size.max(1);
        let glyphs = self.shaper.shape(text, px);
        let w: u32 = glyphs.iter().map(|g| g.advance).sum();
        self.push_piece(px, w, glyphs, style, href);
    }

    fn push_piece(
        &mut self,
        px: u32,
        w: u32,
        glyphs: Vec<GlyphBox>,
        style: &ComputedStyle,
        href: Option<&str>,
    ) {
        self.line.push(LinePiece {
            x: self.x,
            y: self.y,
            w,
            px,
            glyphs,
            color: style.color,
            font: style.font,
            underline: style.underline,
            href: href.map(str::to_string),
        });
        self.x += w as i32;
        self.line_px = self.line_px.max(px);
    }

    fn flush_line(&mut self) {
        if self.x != self.left || !self.line.is_empty() {
            self.newline();
        }
    }

    fn line_break(&mut self, px: u32) {
        self.line_px = self.line_px.max(px);
        self.newline();
    }

    fn newline(&mut self) {
        self.commit_line();
        self.y += line_height(self.line_px.max(1));
        self.x = self.left;
        self.line_px = 0;
    }

    /// Apply text-align to the buffered line, then emit it.
    fn commit_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let used = self.x - self.left;
        let available = ((self.right - self.left) - used).max(0);
        let offset = match self.line_align {
            TextAlign::Left => 0,
            TextAlign::Center => available / 2,
            TextAlign::Right => available,
        };
        for piece in std::mem::take(&mut self.line) {
            let x = piece.x + offset;
            self.display.push(DisplayItem::Glyphs {
                origin: Point::new(x, piece.y),
                glyphs: piece.glyphs,
                color: piece.color,
                style: piece.font,
            });
            if piece.underline {
                self.display.push(DisplayItem::Rect {
                    rect: Rect::new(x, piece.y + piece.px as i32, piece.w, 1),
                    color: piece.color,
                });
            }
            if let Some(href) = piece.href {
                let h = (piece.px as i32 + piece.px as i32 / 3).max(1) as u32;
                self.links.push(LinkBox {
                    rect: Rect::new(x, piece.y, piece.w, h),
                    href,
                });
            }
        }
    }

    fn rule(&mut self) {
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(self.left, self.y, (self.right - self.left).max(0) as u32, 1),
            color: Color::rgb(0xCC, 0xCC, 0xCC),
        });
        self.y += 8;
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
    use cerberus_css::CssEngine;
    use cerberus_dom::parse_trivial;
    use cerberus_paint::MonoShaper;
    use cerberus_style::StyleEngine;

    fn lay(html: &str, width: u32) -> LaidOut {
        let styled = CssEngine::new().style(&parse_trivial(html));
        BlockLayout::default().layout(&styled, Size::new(width, 2000), &MonoShaper)
    }

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
    fn inline_flows_blocks_stack() {
        let laid = lay("<p>Hello <b>brave</b> world</p><p>next</p>", 400);
        let ys = glyph_ys(&laid);
        // First paragraph's three words share a line; "next" is lower.
        assert_eq!(ys.iter().filter(|&&y| y == ys[0]).count(), 3);
        assert!(*ys.last().unwrap() > ys[0]);
    }

    #[test]
    fn display_none_is_skipped() {
        let laid = lay("<p style='display:none'>hidden</p><p>shown</p>", 400);
        assert_eq!(glyph_ys(&laid).iter().filter(|_| true).count(), 1);
    }

    #[test]
    fn opacity_zero_still_renders() {
        // Speed-first: content hidden behind a fade-in shows anyway.
        let laid = lay("<p style='opacity:0'>fade-in text</p>", 400);
        assert!(!glyph_ys(&laid).is_empty(), "opacity is ignored");
    }

    #[test]
    fn links_emit_boxes_with_href() {
        let laid = lay("<a href=\"/x\">click me</a>", 400);
        assert!(!laid.links.is_empty());
        assert!(laid.links.iter().all(|l| l.href == "/x"));
    }

    #[test]
    fn background_paints_a_rect_behind_a_block() {
        let laid = lay("<div style='background:#ff0000'>hi</div>", 400);
        let has_red = laid.display.items.iter().any(
            |i| matches!(i, DisplayItem::Rect { color, .. } if *color == Color::rgb(255, 0, 0)),
        );
        assert!(has_red, "block background rect emitted");
    }

    #[test]
    fn centered_text_is_shifted_right() {
        let left = lay("<p>hi</p>", 400);
        let center = lay("<p style='text-align:center'>hi</p>", 400);
        let lx = match &left.display.items[0] {
            DisplayItem::Glyphs { origin, .. } => origin.x,
            _ => panic!(),
        };
        let cx = match &center.display.items[0] {
            DisplayItem::Glyphs { origin, .. } => origin.x,
            _ => panic!(),
        };
        assert!(cx > lx, "centered line starts further right");
    }
}

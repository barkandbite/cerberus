//! Layout: flow a styled tree into a `LaidOut` (display list + link boxes).
//!
//! `BlockLayout` is a small **block/inline flow** engine driven entirely by the
//! `ComputedStyle` on each node (from `cerberus-css`): blocks stack with their
//! margins and optional background, inline content flows and word-wraps, text
//! uses the cascaded color/size/weight/underline, `text-align` shifts lines, and
//! `display:none` is skipped. `<a href>` text also emits clickable link boxes,
//! `<img>` emits decoded images (or a sized placeholder / `[alt]`), and form
//! controls (`<input>`, `<button>`, `<textarea>`, `<select>`) render as bordered
//! inline-block boxes. Real box widths, floats, and positioning are still ahead.

use cerberus_paint::{DecodedImage, DisplayItem, DisplayList, GlyphBox, TextShaper};
use cerberus_style::{ComputedStyle, Display, StyledChild, StyledDom, StyledNode, TextAlign};
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use std::sync::Arc;

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

/// Supplies decoded images to layout, keyed by an element's `src`/`data-src`.
/// Resolution/fetching/decoding all happen inside the implementation.
pub trait ImageProvider {
    /// The decoded image for `src`, if available.
    fn get(&self, src: &str) -> Option<Arc<DecodedImage>>;
}

/// An image provider with nothing (placeholders / alt text only).
pub struct NoImages;

impl ImageProvider for NoImages {
    fn get(&self, _src: &str) -> Option<Arc<DecodedImage>> {
        None
    }
}

/// Produces a `LaidOut` from a styled document for a given viewport.
pub trait LayoutEngine: Send {
    /// Lay out `styled` into `viewport`, shaping with `shaper`, images via `images`.
    fn layout(
        &mut self,
        styled: &StyledDom,
        viewport: Size,
        shaper: &dyn TextShaper,
        images: &dyn ImageProvider,
    ) -> LaidOut;
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
    fn layout(
        &mut self,
        styled: &StyledDom,
        viewport: Size,
        shaper: &dyn TextShaper,
        images: &dyn ImageProvider,
    ) -> LaidOut {
        let max_width = viewport
            .w
            .saturating_sub(2 * self.margin.max(0) as u32)
            .max(16) as i32;
        let mut ctx = Ctx::new(self.margin, max_width, shaper, images);
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
    images: &'a dyn ImageProvider,
    display: DisplayList,
    links: Vec<LinkBox>,
    left0: i32,
    right: i32,
    left: i32,
    x: i32,
    y: i32,
    /// Tallest content on the current line (text or image), in pixels.
    line_h: i32,
    line: Vec<LinePiece>,
    line_align: TextAlign,
}

impl<'a> Ctx<'a> {
    fn new(
        margin: i32,
        max_width: i32,
        shaper: &'a dyn TextShaper,
        images: &'a dyn ImageProvider,
    ) -> Self {
        Self {
            shaper,
            images,
            display: DisplayList::new(),
            links: Vec::new(),
            left0: margin,
            right: margin + max_width,
            left: margin,
            x: margin,
            y: margin,
            line_h: 0,
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
                self.image(node, in_link);
                return;
            }
            "input" => {
                self.form_input(node);
                return;
            }
            "button" => {
                self.form_button(node);
                return;
            }
            "textarea" => {
                self.form_textarea(node);
                return;
            }
            "select" => {
                self.form_select(node);
                return;
            }
            // Options are rendered by their <select>; loose ones never flow.
            "option" | "optgroup" => return,
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
        self.line_h = self.line_h.max(line_height(px));
    }

    /// Lay out an `<img>`: draw the decoded image if ready, else a sized
    /// placeholder, else the alt text. Lazy-loading is ignored (raw render).
    fn image(&mut self, node: &StyledNode, in_link: Option<&str>) {
        // Prefer data-src (the real URL behind lazy-loaders) over a placeholder src.
        let Some(src) = node.attr("data-src").or_else(|| node.attr("src")) else {
            self.image_alt(node, in_link);
            return;
        };
        let attr_w = node.attr("width").and_then(parse_dim);
        let attr_h = node.attr("height").and_then(parse_dim);

        if let Some(image) = self.images.get(src) {
            let (mut w, mut h) = (
                attr_w.filter(|v| *v > 0).unwrap_or(image.size.w),
                attr_h.filter(|v| *v > 0).unwrap_or(image.size.h),
            );
            let max_w = (self.right - self.left).max(1) as u32;
            if w > max_w {
                h = (h as f32 * max_w as f32 / w as f32).round() as u32;
                w = max_w;
            }
            self.place_box(w, h.max(1));
            let rect = Rect::new(self.x, self.y, w, h.max(1));
            self.display.push(DisplayItem::Image { rect, image });
            self.advance_box(w, h);
        } else if let (Some(w), Some(h)) = (attr_w, attr_h) {
            // Not decoded yet: reserve the declared box so layout doesn't reflow.
            self.place_box(w, h.max(1));
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(self.x, self.y, w, h.max(1)),
                color: Color::rgb(0xDD, 0xDD, 0xDD),
            });
            self.advance_box(w, h);
        } else {
            self.image_alt(node, in_link);
        }
    }

    fn image_alt(&mut self, node: &StyledNode, in_link: Option<&str>) {
        if let Some(alt) = node.attr("alt").map(str::trim) {
            if !alt.is_empty() {
                self.add_text(&format!("[{alt}]"), &node.style, in_link);
            }
        }
    }

    /// Lay out an `<input>` as the right inline-block control for its `type`.
    fn form_input(&mut self, node: &StyledNode) {
        let kind = node
            .attr("type")
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string());
        match kind.as_str() {
            "hidden" => {}
            "checkbox" | "radio" => self.toggle_control(node, kind == "radio"),
            "submit" | "reset" | "button" | "image" => {
                let label =
                    node.attr("value")
                        .map(str::to_string)
                        .unwrap_or_else(|| match kind.as_str() {
                            "reset" => "Reset".to_string(),
                            _ => "Submit".to_string(),
                        });
                self.push_button(&node.style, &label);
            }
            // text, search, email, url, tel, password, number, date, … all render
            // as a single-line text field.
            _ => self.text_field(node, kind == "password"),
        }
    }

    /// A `<button>`: a button box labelled with its text (or `value`).
    fn form_button(&mut self, node: &StyledNode) {
        let text = node.text();
        let label = if text.trim().is_empty() {
            node.attr("value").unwrap_or("Button")
        } else {
            text.trim()
        };
        self.push_button(&node.style, label);
    }

    /// A `<textarea>`: a multi-row bordered box showing its text content.
    fn form_textarea(&mut self, node: &StyledNode) {
        let px = node.style.font_size.max(1);
        let rows = node
            .attr("rows")
            .and_then(parse_dim)
            .unwrap_or(2)
            .clamp(1, 20);
        let cols = node.attr("cols").and_then(parse_dim).unwrap_or(30).max(1);
        let w = self.fit_width(cols as i32 * self.char_w(px) + 2 * FIELD_PAD);
        let h = rows * line_height(px) as u32 + 6;
        self.control_box(w, h, FIELD_BG);
        let value = node.text();
        let value = value.trim_end_matches('\n');
        if !value.is_empty() {
            self.box_label(px, value, node.style.color, FIELD_PAD, FIELD_PAD);
        }
        self.advance_box(w, h);
    }

    /// A `<select>`: a bordered box showing the chosen option plus a caret.
    fn form_select(&mut self, node: &StyledNode) {
        let px = node.style.font_size.max(1);
        let label = selected_option(node).unwrap_or_default();
        let text_w = self.text_width(&label, px);
        let w = self.fit_width(text_w + self.char_w(px) + 3 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, FIELD_BG);
        if !label.is_empty() {
            self.box_label(px, &label, node.style.color, FIELD_PAD, FIELD_PAD);
        }
        // A down caret at the right edge marks it as a dropdown.
        self.box_label(
            px,
            "\u{25BE}",
            Color::rgb(0x55, 0x55, 0x55),
            w as i32 - self.char_w(px) - FIELD_PAD,
            FIELD_PAD,
        );
        self.advance_box(w, h as u32);
    }

    /// A single-line text field of width from the `size` attr (or a default),
    /// showing the `value`, else the `placeholder` (greyed).
    fn text_field(&mut self, node: &StyledNode, password: bool) {
        let px = node.style.font_size.max(1);
        let cols = node.attr("size").and_then(parse_dim).unwrap_or(20).max(1);
        let w = self.fit_width(cols as i32 * self.char_w(px) + 2 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, FIELD_BG);

        let (text, color) = match node.attr("value").filter(|v| !v.is_empty()) {
            Some(v) if password => ("\u{2022}".repeat(v.chars().count()), node.style.color),
            Some(v) => (v.to_string(), node.style.color),
            None => (
                node.attr("placeholder").unwrap_or("").to_string(),
                Color::rgb(0x75, 0x75, 0x75),
            ),
        };
        if !text.is_empty() {
            self.box_label(px, &text, color, FIELD_PAD, FIELD_PAD);
        }
        self.advance_box(w, h as u32);
    }

    /// A checkbox/radio: a small box, filled when `checked`.
    fn toggle_control(&mut self, node: &StyledNode, _radio: bool) {
        let px = node.style.font_size.max(1);
        let s = px + 2;
        self.control_box(s, s, Color::WHITE);
        if node.attr("checked").is_some() {
            let inset = (s / 4).max(1) as i32;
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(
                    self.x + inset,
                    self.y + inset,
                    s - 2 * inset as u32,
                    s - 2 * inset as u32,
                ),
                color: Color::rgb(0x33, 0x33, 0x33),
            });
        }
        self.advance_box(s, s);
    }

    /// A button-styled box (grey fill) labelled `label`, centred horizontally.
    fn push_button(&mut self, style: &ComputedStyle, label: &str) {
        let px = style.font_size.max(1);
        let text_w = self.text_width(label, px);
        let w = self.fit_width(text_w + 4 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, BUTTON_BG);
        let pad_x = ((w as i32 - text_w) / 2).max(FIELD_PAD);
        self.box_label(px, label, style.color, pad_x, FIELD_PAD);
        self.advance_box(w, h as u32);
    }

    /// Emit a bordered, filled control box at the current pen, wrapping first if
    /// it wouldn't fit. Does **not** advance the pen (the caller does, after
    /// drawing any label, so the label sits on top of the box).
    fn control_box(&mut self, w: u32, h: u32, fill: Color) {
        self.place_box(w, h);
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(self.x, self.y, w, h),
            color: CONTROL_BORDER,
        });
        if w > 2 && h > 2 {
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(self.x + 1, self.y + 1, w - 2, h - 2),
                color: fill,
            });
        }
    }

    /// Draw one line of label text inside the current control box at the given
    /// padding offsets from the box's top-left.
    fn box_label(&mut self, px: u32, text: &str, color: Color, pad_x: i32, pad_y: i32) {
        let glyphs = self.shaper.shape(text, px);
        self.display.push(DisplayItem::Glyphs {
            origin: Point::new(self.x + pad_x, self.y + pad_y),
            glyphs,
            color,
            style: FontStyle::REGULAR,
        });
    }

    /// Total advance width of `text` at size `px`.
    fn text_width(&self, text: &str, px: u32) -> i32 {
        self.shaper
            .shape(text, px)
            .iter()
            .map(|g| g.advance)
            .sum::<u32>() as i32
    }

    /// Approximate width of one character at size `px`.
    fn char_w(&self, px: u32) -> i32 {
        self.text_width("n", px).max(1)
    }

    /// Clamp a desired control width to the content box.
    fn fit_width(&self, w: i32) -> u32 {
        w.clamp(1, (self.right - self.left).max(1)) as u32
    }

    /// Wrap to a new line if a `w`-wide box wouldn't fit on the current one.
    fn place_box(&mut self, w: u32, _h: u32) {
        if self.x != self.left && self.x + w as i32 > self.right {
            self.newline();
        }
    }

    fn advance_box(&mut self, w: u32, h: u32) {
        self.x += w as i32;
        self.line_h = self.line_h.max(h as i32);
    }

    fn flush_line(&mut self) {
        if self.x != self.left || !self.line.is_empty() {
            self.newline();
        }
    }

    fn line_break(&mut self, px: u32) {
        self.line_h = self.line_h.max(line_height(px));
        self.newline();
    }

    fn newline(&mut self) {
        self.commit_line();
        self.y += self.line_h.max(1);
        self.x = self.left;
        self.line_h = 0;
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

/// Parse an `<img width/height>` attribute (a bare number or `Npx`).
fn parse_dim(v: &str) -> Option<u32> {
    v.trim().trim_end_matches("px").trim().parse().ok()
}

// --- Form-control styling (UA defaults; CSS overrides are a later slice). ---
/// Inner padding for form controls, in pixels.
const FIELD_PAD: i32 = 4;
/// Form-control border colour (≈ `#767676`, the typical UA control border).
const CONTROL_BORDER: Color = Color::rgb(0x76, 0x76, 0x76);
/// Fill for text fields, selects, and textareas.
const FIELD_BG: Color = Color::WHITE;
/// Fill for buttons.
const BUTTON_BG: Color = Color::rgb(0xE9, 0xE9, 0xED);

/// The label of a `<select>`'s selected option, falling back to its first.
fn selected_option(node: &StyledNode) -> Option<String> {
    let mut options: Vec<(String, bool)> = Vec::new();
    collect_options(node, &mut options);
    options
        .iter()
        .find(|(_, selected)| *selected)
        .or_else(|| options.first())
        .map(|(label, _)| label.clone())
}

fn collect_options(node: &StyledNode, out: &mut Vec<(String, bool)>) {
    for child in &node.children {
        if let StyledChild::Element(e) = child {
            match e.tag.as_str() {
                "option" => out.push((e.text().trim().to_string(), e.attr("selected").is_some())),
                "optgroup" => collect_options(e, out),
                _ => {}
            }
        }
    }
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
        BlockLayout::default().layout(&styled, Size::new(width, 2000), &MonoShaper, &NoImages)
    }

    struct OneImage(Arc<DecodedImage>);
    impl ImageProvider for OneImage {
        fn get(&self, _src: &str) -> Option<Arc<DecodedImage>> {
            Some(self.0.clone())
        }
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

    fn rect_count(laid: &LaidOut) -> usize {
        laid.display
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Rect { .. }))
            .count()
    }

    fn total_glyphs(laid: &LaidOut) -> usize {
        laid.display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { glyphs, .. } => Some(glyphs.len()),
                _ => None,
            })
            .sum()
    }

    fn has_rect_color(laid: &LaidOut, c: Color) -> bool {
        laid.display
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Rect { color, .. } if *color == c))
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

    #[test]
    fn img_with_provider_emits_image_item() {
        let styled = CssEngine::new().style(&parse_trivial("<img src='pic.png' alt='x'>"));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(400, 2000),
            &MonoShaper,
            &OneImage(img),
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "decoded image emitted"
        );
    }

    #[test]
    fn img_without_provider_shows_alt() {
        let laid = lay("<img src='pic.png' alt='a cat'>", 400);
        assert!(!glyph_ys(&laid).is_empty(), "alt text laid out");
        assert!(!laid
            .display
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Image { .. })));
    }

    #[test]
    fn text_input_renders_a_bordered_box_with_placeholder() {
        let laid = lay("<input placeholder='Search'>", 400);
        // A border rect and an inset fill rect.
        assert!(rect_count(&laid) >= 2);
        assert!(
            has_rect_color(&laid, CONTROL_BORDER),
            "control border drawn"
        );
        assert!(has_rect_color(&laid, FIELD_BG), "field fill drawn");
        // The placeholder text is laid out.
        assert!(total_glyphs(&laid) > 0, "placeholder shown");
    }

    #[test]
    fn button_renders_a_filled_box_and_label() {
        let laid = lay("<button>Go</button>", 400);
        assert!(has_rect_color(&laid, BUTTON_BG), "button fill drawn");
        assert_eq!(total_glyphs(&laid), 2, "two-glyph label 'Go'");
    }

    #[test]
    fn submit_input_uses_its_value_as_label() {
        let laid = lay("<input type='submit' value='Send'>", 400);
        assert!(has_rect_color(&laid, BUTTON_BG));
        assert_eq!(total_glyphs(&laid), 4, "'Send' label");
    }

    #[test]
    fn checkbox_fills_when_checked() {
        let off = lay("<input type='checkbox'>", 400);
        let on = lay("<input type='checkbox' checked>", 400);
        // The checked mark is an extra rect over the empty box.
        assert!(rect_count(&on) > rect_count(&off), "checked mark drawn");
    }

    #[test]
    fn select_shows_only_the_selected_option_plus_a_caret() {
        let laid = lay(
            "<select><option>AAAA</option><option selected>BBBB</option></select>",
            400,
        );
        // Only "BBBB" (4) is shown, plus the dropdown caret (1) — "AAAA" is not.
        assert_eq!(total_glyphs(&laid), 5);
    }

    #[test]
    fn textarea_renders_a_box_with_its_text() {
        let laid = lay("<textarea>hello</textarea>", 400);
        assert!(has_rect_color(&laid, CONTROL_BORDER));
        assert_eq!(total_glyphs(&laid), 5, "'hello' shown inside the box");
    }

    #[test]
    fn hidden_input_renders_nothing() {
        let laid = lay("<input type='hidden' value='secret'>", 400);
        assert!(
            laid.display.items.is_empty(),
            "type=hidden produces no paint"
        );
    }
}

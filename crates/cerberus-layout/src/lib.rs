//! Layout: flow a styled tree into a `LaidOut` (display list + link boxes).
//!
//! `BlockLayout` is a small **block/inline flow** engine driven entirely by the
//! `ComputedStyle` on each node (from `cerberus-css`): blocks stack with their
//! margins and optional background, inline content flows and word-wraps, text
//! uses the cascaded color/size/weight/underline, `text-align` shifts lines, and
//! `display:none` is skipped. `<a href>` text also emits clickable link boxes,
//! `<img>` emits decoded images (or a sized placeholder / `[alt]`), form
//! controls (`<input>`, `<button>`, `<textarea>`, `<select>`) render as bordered
//! inline-block boxes, and `<table>` lays out as an equal-width bordered grid
//! (each cell's content flowed into its own box). Real box widths, floats, and
//! positioning are still ahead.

use cerberus_dom::NodeId;
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

/// The kind of an interactive form control, used to route input and rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Textarea,
    Checkbox,
    Radio,
    Select,
    Button,
}

/// A hit region for one interactive form control (in layout-local coordinates).
///
/// The `id` is the control's 0-based index in a pre-order traversal of the tree
/// counting **every** `<input>`, `<textarea>`, `<select>`, and `<button>` —
/// including `type=hidden`, which consumes an id but paints nothing. The app
/// assigns the same ids while walking the DOM, so a box the user clicked maps to
/// the right control's name/value.
#[derive(Clone, Debug, PartialEq)]
pub struct FormFieldBox {
    pub rect: Rect,
    pub id: u32,
    pub kind: FieldKind,
}

/// A generic element hit region (layout-local coords) tagging a painted block
/// element's box with the `NodeId` it came from. Unlike [`LinkBox`]/
/// [`FormFieldBox`] (which drive a *default* action), these let the app dispatch
/// a real DOM event at whatever element was clicked and let it bubble — so
/// handlers on arbitrary elements, and event delegation, work (M12b). Boxes
/// nest (a parent contains its children); the app picks the smallest one
/// containing the point.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementBox {
    pub rect: Rect,
    pub node: NodeId,
}

/// The result of laying out a document: what to paint, where the links are, the
/// interactive form-control hit boxes, and the generic element hit map.
#[derive(Clone, Debug, Default)]
pub struct LaidOut {
    pub display: DisplayList,
    pub links: Vec<LinkBox>,
    pub fields: Vec<FormFieldBox>,
    pub elements: Vec<ElementBox>,
}

/// Supplies the live state of form controls to layout, keyed by field id (the
/// same pre-order index layout assigns). An implementation returns `Some`/`true`
/// only for fields the user has actually touched; layout falls back to the DOM
/// attributes otherwise.
pub trait FormState {
    /// The current text of a text field/textarea, if the user has edited it.
    fn value(&self, id: u32) -> Option<&str>;
    /// Whether a checkbox/radio is currently checked (the live override).
    fn checked(&self, id: u32) -> bool;
    /// The chosen option index of a `<select>`, if the user has changed it.
    fn select_index(&self, id: u32) -> Option<usize>;
}

/// A form state that knows nothing: every control renders from its DOM defaults.
pub struct NoForms;

impl FormState for NoForms {
    fn value(&self, _id: u32) -> Option<&str> {
        None
    }
    fn checked(&self, _id: u32) -> bool {
        false
    }
    fn select_index(&self, _id: u32) -> Option<usize> {
        None
    }
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
    /// Lay out `styled` into `viewport`, shaping with `shaper`, images via
    /// `images`, and rendering form controls from their live `forms` state.
    fn layout(
        &mut self,
        styled: &StyledDom,
        viewport: Size,
        shaper: &dyn TextShaper,
        images: &dyn ImageProvider,
        forms: &dyn FormState,
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
        forms: &dyn FormState,
    ) -> LaidOut {
        let max_width = viewport
            .w
            .saturating_sub(2 * self.margin.max(0) as u32)
            .max(16) as i32;
        let mut ctx = Ctx::new(self.margin, max_width, shaper, images, forms);
        ctx.walk(&styled.root, None);
        ctx.flush_line();
        LaidOut {
            display: ctx.display,
            links: ctx.links,
            fields: ctx.fields,
            elements: ctx.elements,
        }
    }
}

/// The output of flowing one table cell: its display items, link boxes, form
/// field boxes, and the content height (all in absolute coordinates).
type CellLayout = (Vec<DisplayItem>, Vec<LinkBox>, Vec<FormFieldBox>, i32);

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
    forms: &'a dyn FormState,
    display: DisplayList,
    links: Vec<LinkBox>,
    fields: Vec<FormFieldBox>,
    elements: Vec<ElementBox>,
    /// Next form-control id (0-based pre-order index across the whole document).
    field_id: u32,
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
        forms: &'a dyn FormState,
    ) -> Self {
        Self {
            shaper,
            images,
            forms,
            display: DisplayList::new(),
            links: Vec::new(),
            fields: Vec::new(),
            elements: Vec::new(),
            field_id: 0,
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

    /// A fresh flow context bounded to `left..right` and starting at `y`, used to
    /// lay a table cell's content into its own rectangle. It shares the parent's
    /// shaper/images/forms and produces absolute-coordinate items (no offset
    /// needed). The `field_id` is seeded from the parent so controls inside the
    /// cell continue the document-wide pre-order numbering; the parent reads the
    /// advanced counter back after the cell is flowed.
    fn sub(
        left: i32,
        right: i32,
        y: i32,
        shaper: &'a dyn TextShaper,
        images: &'a dyn ImageProvider,
        forms: &'a dyn FormState,
        field_id: u32,
    ) -> Self {
        Self {
            shaper,
            images,
            forms,
            display: DisplayList::new(),
            links: Vec::new(),
            fields: Vec::new(),
            elements: Vec::new(),
            field_id,
            left0: left,
            right: right.max(left + 1),
            left,
            x: left,
            y,
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
            "table" => {
                self.table(node);
                return;
            }
            // Options are rendered by their <select>; loose ones never flow.
            "option" | "optgroup" => return,
            // Table-internal tags only flow inside a <table> (see `table`); a
            // stray one in normal flow renders nothing.
            "tr" | "td" | "th" | "thead" | "tbody" | "tfoot" | "caption" => return,
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
            // Generic hit box for this block element (M12b): the content extent
            // it occupied. Boxes nest (a parent contains its children); the app
            // resolves a click to the smallest one and lets the event bubble.
            let elem_h = (self.y - bg_start_y).max(0) as u32;
            if elem_h > 0 {
                self.elements.push(ElementBox {
                    rect: Rect::new(
                        self.left0,
                        bg_start_y,
                        (self.right - self.left0).max(0) as u32,
                        elem_h,
                    ),
                    node: node.node_id,
                });
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
    ///
    /// Assigns this control's id first, *before* the `type=hidden` early-out, so
    /// a hidden input still consumes an id (keeping layout's pre-order numbering
    /// in lockstep with the app's DOM walk).
    fn form_input(&mut self, node: &StyledNode) {
        let id = self.field_id;
        self.field_id += 1;
        let kind = node
            .attr("type")
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string());
        match kind.as_str() {
            "hidden" => {}
            "checkbox" | "radio" => self.toggle_control(node, id, kind == "radio"),
            "submit" | "reset" | "button" | "image" => {
                let label =
                    node.attr("value")
                        .map(str::to_string)
                        .unwrap_or_else(|| match kind.as_str() {
                            "reset" => "Reset".to_string(),
                            _ => "Submit".to_string(),
                        });
                self.push_button(&node.style, id, &label);
            }
            // text, search, email, url, tel, password, number, date, … all render
            // as a single-line text field.
            _ => self.text_field(node, id, kind == "password"),
        }
    }

    /// A `<button>`: a button box labelled with its text (or `value`).
    fn form_button(&mut self, node: &StyledNode) {
        let id = self.field_id;
        self.field_id += 1;
        let text = node.text();
        let label = if text.trim().is_empty() {
            node.attr("value").unwrap_or("Button")
        } else {
            text.trim()
        };
        self.push_button(&node.style, id, label);
    }

    /// A `<textarea>`: a multi-row bordered box showing its text content (or the
    /// live edited value from `forms`).
    fn form_textarea(&mut self, node: &StyledNode) {
        let id = self.field_id;
        self.field_id += 1;
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
        self.push_field(id, FieldKind::Textarea, w, h);
        let dom_value = node.text();
        let value: &str = match self.forms.value(id) {
            Some(v) => v,
            None => dom_value.trim_end_matches('\n'),
        };
        if !value.is_empty() {
            self.box_label(px, value, node.style.color, FIELD_PAD, FIELD_PAD);
        }
        self.advance_box(w, h);
    }

    /// A `<select>`: a bordered box showing the chosen option plus a caret. The
    /// chosen option is `forms.select_index(id)` if set, else the DOM-selected
    /// option (or the first).
    fn form_select(&mut self, node: &StyledNode) {
        let id = self.field_id;
        self.field_id += 1;
        let px = node.style.font_size.max(1);
        let label = match self.forms.select_index(id) {
            Some(i) => option_at(node, i).unwrap_or_default(),
            None => selected_option(node).unwrap_or_default(),
        };
        let text_w = self.text_width(&label, px);
        let w = self.fit_width(text_w + self.char_w(px) + 3 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, FIELD_BG);
        self.push_field(id, FieldKind::Select, w, h as u32);
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
    /// showing the live edited value from `forms`, else the DOM `value`, else the
    /// `placeholder` (greyed). Passwords are masked.
    fn text_field(&mut self, node: &StyledNode, id: u32, password: bool) {
        let px = node.style.font_size.max(1);
        let cols = node.attr("size").and_then(parse_dim).unwrap_or(20).max(1);
        let w = self.fit_width(cols as i32 * self.char_w(px) + 2 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, FIELD_BG);
        self.push_field(id, FieldKind::Text, w, h as u32);

        let live = self.forms.value(id);
        let dom = node.attr("value").filter(|v| !v.is_empty());
        let (text, color) = match (live, dom) {
            (Some(v), _) if password => ("\u{2022}".repeat(v.chars().count()), node.style.color),
            (Some(v), _) => (v.to_string(), node.style.color),
            (None, Some(v)) if password => ("\u{2022}".repeat(v.chars().count()), node.style.color),
            (None, Some(v)) => (v.to_string(), node.style.color),
            (None, None) => (
                node.attr("placeholder").unwrap_or("").to_string(),
                Color::rgb(0x75, 0x75, 0x75),
            ),
        };
        if !text.is_empty() {
            self.box_label(px, &text, color, FIELD_PAD, FIELD_PAD);
        }
        self.advance_box(w, h as u32);
    }

    /// A checkbox/radio: a small box, filled when checked. It is checked when
    /// `forms.checked(id)` is set, or — when `forms` has no opinion — when the DOM
    /// carries the `checked` attribute.
    fn toggle_control(&mut self, node: &StyledNode, id: u32, radio: bool) {
        let px = node.style.font_size.max(1);
        let s = px + 2;
        self.control_box(s, s, Color::WHITE);
        self.push_field(
            id,
            if radio {
                FieldKind::Radio
            } else {
                FieldKind::Checkbox
            },
            s,
            s,
        );
        let checked = self.forms.checked(id) || node.attr("checked").is_some();
        if checked {
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

    /// A button-styled box (grey fill) labelled `label`, centred horizontally,
    /// recording a `Button` hit box under `id`.
    fn push_button(&mut self, style: &ComputedStyle, id: u32, label: &str) {
        let px = style.font_size.max(1);
        let text_w = self.text_width(label, px);
        let w = self.fit_width(text_w + 4 * FIELD_PAD);
        let h = px as i32 + 2 * FIELD_PAD;
        self.control_box(w, h as u32, BUTTON_BG);
        self.push_field(id, FieldKind::Button, w, h as u32);
        let pad_x = ((w as i32 - text_w) / 2).max(FIELD_PAD);
        self.box_label(px, label, style.color, pad_x, FIELD_PAD);
        self.advance_box(w, h as u32);
    }

    /// Record an interactive control's hit box at the current pen position. The
    /// rect is absolute (the pen is already in document/cell coordinates), so the
    /// box needs no later offset — the same as link boxes inside a cell.
    fn push_field(&mut self, id: u32, kind: FieldKind, w: u32, h: u32) {
        self.fields.push(FormFieldBox {
            rect: Rect::new(self.x, self.y, w, h),
            id,
            kind,
        });
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

    /// Lay out a `<table>` as an equal-width grid.
    ///
    /// Rows are the `<tr>`s directly under the table plus those inside
    /// `<thead>/<tbody>/<tfoot>` (flattened in source order); a row's cells are
    /// its `<td>`/`<th>` children. Columns are equal width across the content box
    /// (`col_w = (right - left) / cols`, last column takes the remainder). Each
    /// cell's content is flowed into its own rectangle by a sub-`Ctx`, the row
    /// height is the tallest cell, and a 1px border (plus optional fill) is drawn
    /// around every cell. `<th>` text is bold and centred; `<td>` is left-aligned.
    ///
    /// Pragmatic, not spec-perfect: equal columns only (no content-based sizing),
    /// and colspan/rowspan are ignored — every cell is one column by one row.
    // TODO: honour colspan/rowspan and content-based column widths.
    fn table(&mut self, node: &StyledNode) {
        self.flush_line();
        let left = self.left;
        let right = self.right.max(left + 1);
        self.line_align = node.style.text_align;

        // A caption (if any) renders as a single line above the grid.
        if let Some(caption) = find_child(node, "caption") {
            let text = caption.text();
            let text = text.trim();
            if !text.is_empty() {
                self.x = left;
                self.add_run(text, &node.style, None);
                self.flush_line();
            }
        }

        let rows = collect_rows(node);
        let num_cols = rows
            .iter()
            .map(|r| cell_children(r).count())
            .max()
            .unwrap_or(0);

        // Nothing to lay out: leave a small margin and bail (never panic).
        if num_cols == 0 || right - left < num_cols as i32 {
            self.y += TABLE_MARGIN;
            self.x = self.left;
            return;
        }

        let col_w = ((right - left) / num_cols as i32).max(1);
        let mut row_y = self.y;

        for row in rows {
            let cells: Vec<&StyledNode> = cell_children(row).collect();
            if cells.is_empty() {
                continue;
            }

            // Sub-lay every cell, capturing its items/links/fields and height.
            let mut laid: Vec<CellLayout> = Vec::with_capacity(cells.len());
            let mut row_h = line_height(node.style.font_size.max(1));
            for (col, cell) in cells.iter().enumerate() {
                let cell_x = left + col as i32 * col_w;
                let cell_w = if col + 1 == num_cols {
                    right - cell_x
                } else {
                    col_w
                }
                .max(1);
                let (items, links, fields, h) =
                    self.flow_cell(cell, cell_x, cell_x + cell_w, row_y);
                row_h = row_h.max(h);
                laid.push((items, links, fields, h));
            }
            row_h = (row_h + 2 * CELL_PAD).max(1);

            // Emit each cell's box (fill + border) under its content.
            for (col, cell) in cells.iter().enumerate() {
                let cell_x = left + col as i32 * col_w;
                let cell_w = if col + 1 == num_cols {
                    right - cell_x
                } else {
                    col_w
                }
                .max(1) as u32;
                let is_header = cell.tag == "th";
                let fill = cell
                    .style
                    .background
                    .or(if is_header { Some(TH_BG) } else { None });
                self.cell_box(cell_x, row_y, cell_w, row_h as u32, fill);
            }

            // Then the captured cell content, on top of the boxes.
            for (items, links, fields, _) in laid {
                self.display.items.extend(items);
                self.links.extend(links);
                self.fields.extend(fields);
            }

            row_y += row_h;
        }

        self.y = row_y + TABLE_MARGIN;
        self.x = self.left;
    }

    /// Flow one table cell's children into its own rectangle and read back the
    /// produced items, links, form-field boxes, and content height. The
    /// sub-context is positioned at the cell's absolute padded bounds, so its
    /// output (including hit rects) needs no offset.
    ///
    /// Field ids stay globally consistent: the sub-context starts its `field_id`
    /// at the parent's current value, and the parent's counter is advanced to the
    /// sub's final value here — so a control in a later cell (or after the table)
    /// continues the same pre-order numbering.
    fn flow_cell(
        &mut self,
        cell: &StyledNode,
        cell_x: i32,
        cell_right: i32,
        cell_y: i32,
    ) -> CellLayout {
        let content_left = cell_x + CELL_PAD;
        let content_right = (cell_right - CELL_PAD).max(content_left + 1);
        let content_top = cell_y + CELL_PAD;
        let mut sub = Ctx::sub(
            content_left,
            content_right,
            content_top,
            self.shaper,
            self.images,
            self.forms,
            self.field_id,
        );

        let is_header = cell.tag == "th";
        // Headers centre their text; cells inherit the cell's own alignment.
        sub.line_align = if is_header {
            TextAlign::Center
        } else {
            cell.style.text_align
        };
        // Direct text in a <th> is bold (nested elements keep their own style).
        let text_style = if is_header {
            let mut s = cell.style.clone();
            s.font.bold = true;
            s
        } else {
            cell.style.clone()
        };

        for child in &cell.children {
            match child {
                StyledChild::Text(t) => sub.add_text(t, &text_style, None),
                StyledChild::Element(e) => sub.walk(e, None),
            }
        }
        sub.flush_line();

        // Carry the advanced control counter back to the parent.
        self.field_id = sub.field_id;
        // After flush, `sub.y` already includes the last line; floor at one line.
        let height = (sub.y - content_top).max(line_height(cell.style.font_size.max(1)));
        (sub.display.items, sub.links, sub.fields, height)
    }

    /// Draw a cell's optional background fill and its 1px border outline.
    fn cell_box(&mut self, x: i32, y: i32, w: u32, h: u32, fill: Option<Color>) {
        if w == 0 || h == 0 {
            return;
        }
        if let Some(color) = fill {
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(x, y, w, h),
                color,
            });
        }
        // Four thin rects form a hollow border (so a fill stays visible inside).
        let b = TABLE_BORDER;
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(x, y, w, 1),
            color: b,
        });
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(x, y + h as i32 - 1, w, 1),
            color: b,
        });
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(x, y, 1, h),
            color: b,
        });
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(x + w as i32 - 1, y, 1, h),
            color: b,
        });
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

// --- Table styling (UA defaults; CSS overrides are a later slice). ---
/// Inner padding inside every table cell, in pixels.
const CELL_PAD: i32 = 4;
/// Space left below a table before the next block.
const TABLE_MARGIN: i32 = 8;
/// Table cell border colour.
const TABLE_BORDER: Color = Color::rgb(0xCC, 0xCC, 0xCC);
/// Default `<th>` header-cell fill (light grey) when none is set by the cascade.
const TH_BG: Color = Color::rgb(0xF0, 0xF0, 0xF0);

/// Rows of a table in source order: each `<tr>` directly under the table, and
/// each `<tr>` inside a `<thead>`/`<tbody>`/`<tfoot>` section (flattened).
fn collect_rows(table: &StyledNode) -> Vec<&StyledNode> {
    let mut rows = Vec::new();
    for child in &table.children {
        if let StyledChild::Element(e) = child {
            match e.tag.as_str() {
                "tr" => rows.push(e),
                "thead" | "tbody" | "tfoot" => {
                    for inner in &e.children {
                        if let StyledChild::Element(r) = inner {
                            if r.tag == "tr" {
                                rows.push(r);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    rows
}

/// The `<td>`/`<th>` element children of a row, in order.
fn cell_children(row: &StyledNode) -> impl Iterator<Item = &StyledNode> {
    row.children.iter().filter_map(|c| match c {
        StyledChild::Element(e) if e.tag == "td" || e.tag == "th" => Some(e),
        _ => None,
    })
}

/// The first direct element child of `node` whose tag is `tag`.
fn find_child<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
    node.children.iter().find_map(|c| match c {
        StyledChild::Element(e) if e.tag == tag => Some(e),
        _ => None,
    })
}

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

/// The label of a `<select>`'s option at index `i` (pre-order over options).
fn option_at(node: &StyledNode, i: usize) -> Option<String> {
    let mut options: Vec<(String, bool)> = Vec::new();
    collect_options(node, &mut options);
    options.get(i).map(|(label, _)| label.clone())
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
    use cerberus_dom::parse_html;
    use cerberus_paint::MonoShaper;
    use cerberus_style::StyleEngine;

    fn lay(html: &str, width: u32) -> LaidOut {
        let styled = CssEngine::new().style(&parse_html(html));
        BlockLayout::default().layout(
            &styled,
            Size::new(width, 2000),
            &MonoShaper,
            &NoImages,
            &NoForms,
        )
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

    fn glyph_xs(laid: &LaidOut) -> Vec<i32> {
        laid.display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some(origin.x),
                _ => None,
            })
            .collect()
    }

    /// Count of distinct values in a slice of coordinates.
    fn distinct(values: &[i32]) -> usize {
        let mut v = values.to_vec();
        v.sort_unstable();
        v.dedup();
        v.len()
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
        let styled = CssEngine::new().style(&parse_html("<img src='pic.png' alt='x'>"));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(400, 2000),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
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

    #[test]
    fn text_input_emits_one_field_box_with_sane_rect() {
        let laid = lay("<input name='q'>", 400);
        assert_eq!(laid.fields.len(), 1, "one form-field box");
        let f = &laid.fields[0];
        assert_eq!(f.kind, FieldKind::Text);
        assert_eq!(f.id, 0, "first control gets id 0");
        // The hit rect is positive-sized and lives inside the content box.
        assert!(f.rect.w > 0 && f.rect.h > 0, "field has a real size");
        assert!(f.rect.x >= 0 && f.rect.y >= 0);
        assert!(f.rect.x + f.rect.w as i32 <= 400, "field stays in viewport");
    }

    #[test]
    fn field_ids_increase_in_document_order() {
        // text(0), hidden(1, no box), checkbox(2), select(3), button(4)
        let laid = lay(
            "<input name='a'>\
             <input type='hidden' name='h'>\
             <input type='checkbox' name='c'>\
             <select name='s'><option>x</option></select>\
             <button>Go</button>",
            400,
        );
        let kinds: Vec<(u32, FieldKind)> = laid.fields.iter().map(|f| (f.id, f.kind)).collect();
        // Hidden consumes id 1 but emits no box, so the boxes carry ids 0,2,3,4.
        assert_eq!(
            kinds,
            vec![
                (0, FieldKind::Text),
                (2, FieldKind::Checkbox),
                (3, FieldKind::Select),
                (4, FieldKind::Button),
            ],
            "ids ascend in pre-order; hidden still consumes id 1"
        );
    }

    #[test]
    fn control_inside_a_table_cell_has_an_absolute_field_box() {
        // A control preceding the table takes id 0; the in-cell input takes id 1
        // and must carry an absolute hit-rect offset into the cell.
        let laid = lay(
            "<input name='before'>\
             <table><tr><td><input name='incell'></td></tr></table>",
            400,
        );
        assert_eq!(laid.fields.len(), 2, "outer + in-cell controls");
        let cell_field = laid.fields.iter().find(|f| f.id == 1).expect("id 1 box");
        assert_eq!(cell_field.kind, FieldKind::Text);
        // The cell sits below the first input, so the in-cell box is lower and
        // inset by the cell padding — i.e. a real absolute rect, not (0,0).
        assert!(cell_field.rect.x > 0, "inset by the cell's left padding");
        assert!(
            cell_field.rect.y > laid.fields[0].rect.y,
            "the cell control is below the leading control"
        );
    }

    #[test]
    fn table_draws_cell_borders_and_grids_text() {
        // A 2x2 grid of <td> cells. Each cell draws a hollow 1px border made of
        // four rects, so a table emits many more rects than the plain-text
        // baseline (which emits none).
        let baseline = lay("<p>aa bb cc dd</p>", 400);
        let laid = lay(
            "<table><tr><td>aa</td><td>bb</td></tr>\
             <tr><td>cc</td><td>dd</td></tr></table>",
            400,
        );
        assert_eq!(rect_count(&baseline), 0, "plain text draws no rects");
        assert!(
            rect_count(&laid) >= 4 * 4,
            "four cells each add a four-rect border: {}",
            rect_count(&laid)
        );

        // All four cell texts are laid out across two columns and two rows.
        assert_eq!(total_glyphs(&laid), 8, "aa bb cc dd, two glyphs each");
        let xs = glyph_xs(&laid);
        let ys = glyph_ys(&laid);
        assert_eq!(distinct(&xs), 2, "two distinct cell columns: {xs:?}");
        assert_eq!(distinct(&ys), 2, "two distinct cell rows: {ys:?}");
    }

    #[test]
    fn table_header_cells_render_bold() {
        let laid = lay(
            "<table><thead><tr><th>Name</th><th>Age</th></tr></thead>\
             <tbody><tr><td>Alice</td><td>30</td></tr></tbody></table>",
            400,
        );
        // The header row's two cells are laid out (4 + 3 glyphs).
        assert!(total_glyphs(&laid) >= 7, "header + body text laid out");
        // <th> text is shaped bold; <td> text is not, so both weights appear.
        let has_bold = laid
            .display
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Glyphs { style, .. } if style.bold));
        let has_regular = laid
            .display
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Glyphs { style, .. } if !style.bold));
        assert!(has_bold, "header text rendered bold");
        assert!(has_regular, "body text rendered regular");
        // The light-grey header fill is drawn behind the <th> cells.
        assert!(has_rect_color(&laid, TH_BG), "header cell fill drawn");
    }

    #[test]
    fn table_cell_link_emits_a_link_box() {
        let laid = lay(
            "<table><tr><td><a href=\"/dest\">go</a></td></tr></table>",
            400,
        );
        assert!(!laid.links.is_empty(), "link inside a cell is preserved");
        assert!(laid.links.iter().all(|l| l.href == "/dest"));
    }

    #[test]
    fn empty_table_does_not_panic() {
        let laid = lay("<table></table>", 400);
        // No rows means no cells: nothing painted, no links, no crash.
        assert!(laid.display.items.is_empty(), "empty table paints nothing");
        assert!(laid.links.is_empty());
    }

    #[test]
    fn malformed_table_does_not_panic() {
        // A stray <td> directly under <table> (no <tr>) must not panic and must
        // not produce absurd output.
        let laid = lay("<table><td>x</table>", 400);
        // Sane result: the loose cell is dropped (no row), so nothing is painted.
        assert!(rect_count(&laid) == 0);
        // A row of one bare cell is also tolerated and produces a real grid.
        let one = lay("<table><tr><td>x</td></tr></table>", 400);
        assert_eq!(total_glyphs(&one), 1, "single-cell table lays out its text");
        assert!(rect_count(&one) >= 4, "single cell has a four-rect border");
    }
}

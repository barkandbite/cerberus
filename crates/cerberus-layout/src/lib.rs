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
use cerberus_style::{
    AlignItems, ComputedStyle, Display, FlexDirection, JustifyContent, StyledChild, StyledDom,
    StyledNode, TextAlign, Track,
};
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
        let mut ctx = Ctx::new(self.margin, max_width, viewport, shaper, images, forms);
        ctx.walk(&styled.root, None);
        ctx.flush_line();
        ctx.finish_positioned();
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
    link_node: Option<NodeId>,
}

/// A containing block (px), for resolving positioned insets (ADR-0034).
#[derive(Clone, Copy)]
struct ContainingBlock {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// The flow state captured before laying a positioned element, so the tail can
/// translate (relative) or lift out (absolute/fixed) exactly its output.
struct PosBase {
    disp: usize,
    links: usize,
    fields: usize,
    elements: usize,
    /// The element's in-flow top and reference left (`left0`).
    y: i32,
    x: i32,
}

/// An out-of-flow (`absolute`/`fixed`) element's painted output, lifted from the
/// normal flow and painted on top in `z-index` then document order.
struct PositionedLayer {
    z: i32,
    order: usize,
    items: Vec<DisplayItem>,
    links: Vec<LinkBox>,
    fields: Vec<FormFieldBox>,
    elements: Vec<ElementBox>,
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
    /// The farthest right the inline cursor has reached — the content's intrinsic
    /// width, used to size flex/grid items.
    max_x: i32,
    /// Tallest content on the current line (text or image), in pixels.
    line_h: i32,
    line: Vec<LinePiece>,
    line_align: TextAlign,
    /// The `NodeId` of the enclosing `<a>` while flowing its inline content, so
    /// each link piece can carry it into a hit box for event dispatch (M12b).
    cur_link_node: Option<NodeId>,
    /// Effective `opacity: 0` from this element or an ancestor — its whole
    /// subtree is suppressed from paint (the opacity group). `visibility:hidden`
    /// is handled per element via the computed style.
    opacity_hidden: bool,
    /// A reusable scratch context for intrinsic-width measurement, kept across
    /// items so flex/grid sizing does not allocate a fresh `Ctx` (with its five
    /// output buffers) per item per render. Created lazily, cleared between uses.
    scratch: Option<Box<Ctx<'a>>>,
    /// Viewport size (px) — the `fixed` containing block and the initial
    /// containing block for `absolute`, plus the basis for `%` insets.
    vw: i32,
    vh: i32,
    /// Out-of-flow layers (`absolute`/`fixed`), painted on top after the flow.
    positioned: Vec<PositionedLayer>,
    /// Document-order counter for stable z-index tie-breaking.
    pos_order: usize,
    /// Whether positioning is active. Only the root flow positions; sub-flows
    /// (table cells, intrinsic measurement) keep elements in-flow (v1).
    pos_enabled: bool,
}

impl<'a> Ctx<'a> {
    fn new(
        margin: i32,
        max_width: i32,
        viewport: Size,
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
            max_x: margin,
            line_h: 0,
            line: Vec::new(),
            line_align: TextAlign::Left,
            cur_link_node: None,
            opacity_hidden: false,
            scratch: None,
            vw: viewport.w as i32,
            vh: viewport.h as i32,
            positioned: Vec::new(),
            pos_order: 0,
            pos_enabled: true,
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
            max_x: left,
            line_h: 0,
            line: Vec::new(),
            line_align: TextAlign::Left,
            cur_link_node: None,
            opacity_hidden: false,
            scratch: None,
            vw: 0,
            vh: 0,
            positioned: Vec::new(),
            pos_order: 0,
            pos_enabled: false,
        }
    }

    fn walk(&mut self, node: &StyledNode, in_link: Option<&str>) {
        let style = &node.style;
        if style.display == Display::None {
            return;
        }
        // `opacity: 0` (here or inherited via the group) and `visibility: hidden`
        // suppress this element's own paint; children are judged by their own
        // computed style (visibility inherits, so they are hidden unless they
        // override it).
        let subtree_hidden = self.opacity_hidden || style.opacity == 0.0;
        let visible = !subtree_hidden && style.visibility == cerberus_style::Visibility::Visible;
        match node.tag.as_str() {
            "br" => {
                self.line_break(style.font_size.max(1));
                return;
            }
            "hr" => {
                self.flush_line();
                if visible {
                    self.rule();
                }
                return;
            }
            "img" => {
                if visible {
                    self.image(node, in_link);
                }
                return;
            }
            "input" => {
                if visible {
                    self.form_input(node);
                }
                return;
            }
            "button" => {
                if visible {
                    self.form_button(node);
                }
                return;
            }
            "textarea" => {
                if visible {
                    self.form_textarea(node);
                }
                return;
            }
            "select" => {
                if visible {
                    self.form_select(node);
                }
                return;
            }
            "table" => {
                if visible {
                    self.table(node);
                }
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
        // While flowing an <a>'s inline content, tag each link piece with the
        // anchor's node so a click on the link dispatches at the <a> (M12b).
        let saved_link_node = self.cur_link_node;
        if node.tag == "a" {
            self.cur_link_node = Some(node.node_id);
        }

        // A positioned element (relative/absolute/fixed) is laid out in flow,
        // then the tail below either translates it (relative) or lifts it into a
        // paint-on-top layer (absolute/fixed) — ADR-0034. Flex/grid containers
        // return early, so positioning applies to the block/inline path (v1).
        let positioned = self.pos_enabled
            && matches!(
                style.position,
                cerberus_style::Position::Relative
                    | cerberus_style::Position::Absolute
                    | cerberus_style::Position::Fixed
            );
        let pos_base = if positioned {
            Some(PosBase {
                disp: self.display.items.len(),
                links: self.links.len(),
                fields: self.fields.len(),
                elements: self.elements.len(),
                y: self.y,
                x: self.left0,
            })
        } else {
            None
        };

        // An out-of-flow box (`absolute`/`fixed`) uses its **shrink-to-fit**
        // content width (or, when both left & right are set, the stretched width),
        // not the full flow width — so right/bottom anchoring lands correctly.
        let out_of_flow = pos_base.is_some()
            && matches!(
                style.position,
                cerberus_style::Position::Absolute | cerberus_style::Position::Fixed
            );
        let saved_right = if out_of_flow {
            let cb = self.containing_block(style.position);
            let used_w = match (
                style.inset_left.resolve(cb.w),
                style.inset_right.resolve(cb.w),
            ) {
                (Some(l), Some(r)) => (cb.w - l - r).max(1),
                _ => self.measure_intrinsic_width(node).clamp(1, cb.w.max(1)),
            };
            let saved = self.right;
            self.right = self.left0 + used_w;
            Some(saved)
        } else {
            None
        };

        // Flex/grid containers lay their items out and return; everything else
        // falls through to block/inline flow.
        match style.display {
            Display::Flex => {
                self.flex_layout(node);
                self.cur_link_node = saved_link_node;
                return;
            }
            Display::Grid => {
                self.grid_layout(node);
                self.cur_link_node = saved_link_node;
                return;
            }
            _ => {}
        }
        let is_block = matches!(style.display, Display::Block | Display::ListItem);
        let saved_left = self.left;
        let (bg_index, bg_start_y) = (self.display.items.len(), self.y);

        if is_block {
            self.flush_line();
            self.y += style.margin_top;
            self.line_align = style.text_align;
            self.left += style.margin_left;
            self.x = self.left;
            if visible && style.display == Display::ListItem {
                self.add_run("\u{2022}", style, None);
                self.x += space_width(self.shaper, style.font_size.max(1)) as i32;
            }
        }

        let saved_opacity_hidden = self.opacity_hidden;
        self.opacity_hidden = subtree_hidden;
        for child in &node.children {
            match child {
                StyledChild::Text(t) => {
                    if visible {
                        self.add_text(t, style, href);
                    }
                }
                StyledChild::Element(e) => self.walk(e, href),
            }
        }
        self.opacity_hidden = saved_opacity_hidden;

        if is_block {
            self.flush_line();
            if let Some(color) = style.background.filter(|_| visible) {
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
        if let Some(base) = pos_base {
            self.apply_positioning(&node.style, base);
        }
        if let Some(r) = saved_right {
            self.right = r;
        }
        self.cur_link_node = saved_link_node;
    }

    /// The containing block for a positioned element (v1): `fixed` resolves
    /// against the viewport; `absolute`/`relative` against the page content area
    /// (the initial containing block) — nearest-positioned-ancestor is a
    /// follow-up.
    fn containing_block(&self, position: cerberus_style::Position) -> ContainingBlock {
        match position {
            // No positioned-ancestor tracking yet (v1): `absolute` and `fixed`
            // both resolve against the viewport (the initial containing block).
            cerberus_style::Position::Absolute | cerberus_style::Position::Fixed => {
                ContainingBlock {
                    x: 0,
                    y: 0,
                    w: self.vw,
                    h: self.vh,
                }
            }
            // `relative` % insets are relative to the page content area.
            _ => ContainingBlock {
                x: self.left0,
                y: 0,
                w: (self.right - self.left0).max(0),
                h: self.vh,
            },
        }
    }

    /// Translate a `relative` element in place, or lift an `absolute`/`fixed`
    /// element out of flow into a paint-on-top layer (ADR-0034).
    fn apply_positioning(&mut self, style: &ComputedStyle, base: PosBase) {
        use cerberus_style::Position;
        let cb = self.containing_block(style.position);
        let elem_w = (self.right - self.left0).max(0);
        let elem_h = (self.y - base.y).max(0);

        let (dx, dy) = match style.position {
            Position::Relative => {
                // Offset from the in-flow position; left wins over right, top over
                // bottom. Flow space is preserved (the box keeps its slot).
                let dx = style
                    .inset_left
                    .resolve(cb.w)
                    .or_else(|| style.inset_right.resolve(cb.w).map(|r| -r))
                    .unwrap_or(0);
                let dy = style
                    .inset_top
                    .resolve(cb.h)
                    .or_else(|| style.inset_bottom.resolve(cb.h).map(|b| -b))
                    .unwrap_or(0);
                (dx, dy)
            }
            // absolute / fixed: resolve an absolute origin, then translate from
            // the in-flow reference origin to it.
            _ => {
                let ox = style
                    .inset_left
                    .resolve(cb.w)
                    .map(|l| cb.x + l)
                    .or_else(|| {
                        style
                            .inset_right
                            .resolve(cb.w)
                            .map(|r| cb.x + cb.w - r - elem_w)
                    })
                    .unwrap_or(base.x);
                let oy = style
                    .inset_top
                    .resolve(cb.h)
                    .map(|t| cb.y + t)
                    .or_else(|| {
                        style
                            .inset_bottom
                            .resolve(cb.h)
                            .map(|b| cb.y + cb.h - b - elem_h)
                    })
                    .unwrap_or(base.y);
                (ox - base.x, oy - base.y)
            }
        };

        if style.position == Position::Relative {
            for it in &mut self.display.items[base.disp..] {
                translate_item(it, dx, dy);
            }
            for l in &mut self.links[base.links..] {
                l.rect = offset_rect(l.rect, dx, dy);
            }
            for f in &mut self.fields[base.fields..] {
                f.rect = offset_rect(f.rect, dx, dy);
            }
            for e in &mut self.elements[base.elements..] {
                e.rect = offset_rect(e.rect, dx, dy);
            }
            return;
        }

        // Out of flow: drain this element's output, translate it, and stash it as
        // a layer; then rewind the flow so siblings ignore the removed box.
        let mut items: Vec<DisplayItem> = self.display.items.drain(base.disp..).collect();
        for it in &mut items {
            translate_item(it, dx, dy);
        }
        let links = drain_offset(&mut self.links, base.links, dx, dy, |l| &mut l.rect);
        let fields = drain_offset(&mut self.fields, base.fields, dx, dy, |f| &mut f.rect);
        let elements = drain_offset(&mut self.elements, base.elements, dx, dy, |e| &mut e.rect);
        self.positioned.push(PositionedLayer {
            z: style.z_index.unwrap_or(0),
            order: self.pos_order,
            items,
            links,
            fields,
            elements,
        });
        self.pos_order += 1;
        // Rewind flow to before the element (it occupies no normal-flow space).
        self.y = base.y;
        self.x = self.left;
    }

    /// Sort the out-of-flow layers by `z-index` (then document order) and append
    /// them after the in-flow content, so they paint on top (ADR-0034).
    fn finish_positioned(&mut self) {
        if self.positioned.is_empty() {
            return;
        }
        let mut layers = std::mem::take(&mut self.positioned);
        layers.sort_by(|a, b| a.z.cmp(&b.z).then(a.order.cmp(&b.order)));
        for layer in layers {
            self.display.items.extend(layer.items);
            self.links.extend(layer.links);
            self.fields.extend(layer.fields);
            self.elements.extend(layer.elements);
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
            link_node: self.cur_link_node,
        });
        self.x += w as i32;
        self.max_x = self.max_x.max(self.x);
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
        self.max_x = self.max_x.max(self.x);
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
        // Drain through a moved-out buffer so the line `Vec`'s capacity is kept
        // for the next line instead of being dropped each commit (`mem::take`
        // would leave a zero-capacity `Vec`).
        let mut line = std::mem::take(&mut self.line);
        for piece in line.drain(..) {
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
                let rect = Rect::new(x, piece.y, piece.w, h);
                // Tag the link's box with its <a> node so a click dispatches at
                // the anchor (and can preventDefault navigation) — M12b.
                if let Some(node) = piece.link_node {
                    self.elements.push(ElementBox { rect, node });
                }
                self.links.push(LinkBox { rect, href });
            }
        }
        self.line = line;
    }

    fn rule(&mut self) {
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(self.left, self.y, (self.right - self.left).max(0) as u32, 1),
            color: Color::rgb(0xCC, 0xCC, 0xCC),
        });
        self.y += 8;
    }

    /// Reset this context to the measure-only state of a fresh
    /// `Ctx::sub(0, 1_000_000, 0, …)` while **retaining** the allocated buffers,
    /// so a reused scratch measures identically to a freshly built one. The
    /// shaper/images/forms references and the nested `scratch` are left intact.
    fn reset_for_measure(&mut self, field_id: u32) {
        self.display.items.clear();
        self.links.clear();
        self.fields.clear();
        self.elements.clear();
        self.field_id = field_id;
        self.left0 = 0;
        self.right = 1_000_000;
        self.left = 0;
        self.x = 0;
        self.y = 0;
        self.max_x = 0;
        self.line_h = 0;
        self.line.clear();
        self.line_align = TextAlign::Left;
        self.cur_link_node = None;
        self.opacity_hidden = false;
    }

    /// Intrinsic content width of `node`: lay it into an effectively unbounded
    /// sub-context and read how far the inline cursor reached. Used to size flex
    /// items (and grid `auto` tracks).
    ///
    /// Reuses a single scratch [`Ctx`] across items (cleared, not dropped) so a
    /// flex/grid page does not allocate a fresh context and its five output
    /// buffers per item on every render — the dominant layout allocation, and a
    /// cost paid on every mirror render too.
    fn measure_intrinsic_width(&mut self, node: &StyledNode) -> i32 {
        let field_id = self.field_id;
        let mut scratch = self.scratch.take().unwrap_or_else(|| {
            Box::new(Ctx::sub(
                0,
                1_000_000,
                0,
                self.shaper,
                self.images,
                self.forms,
                field_id,
            ))
        });
        scratch.reset_for_measure(field_id);
        scratch.walk(node, None);
        scratch.flush_line();
        let width = scratch.max_x.max(1);
        self.scratch = Some(scratch);
        width
    }

    /// Min-content width of `node`: lay it into a 1px-wide sub so every breakable
    /// point wraps, then read the widest line — the longest unbreakable run (e.g.
    /// the longest word). Used as the floor when flex-shrinking an item (ADR-0036)
    /// so text wraps rather than clipping to nothing.
    fn measure_min_content_width(&mut self, node: &StyledNode) -> i32 {
        let field_id = self.field_id;
        let mut scratch = self.scratch.take().unwrap_or_else(|| {
            Box::new(Ctx::sub(
                0,
                1,
                0,
                self.shaper,
                self.images,
                self.forms,
                field_id,
            ))
        });
        scratch.reset_for_measure(field_id);
        scratch.right = 1; // force a wrap at every opportunity
        scratch.walk(node, None);
        scratch.flush_line();
        let width = scratch.max_x.max(1);
        self.scratch = Some(scratch);
        width
    }

    /// Merge a finished sub-context's output into this one, shifting it down by
    /// `dy` (cross-axis alignment). All sub items are already in absolute coords.
    fn merge_sub(&mut self, sub: Ctx<'a>, dx: i32, dy: i32) {
        for mut item in sub.display.items {
            if dx != 0 || dy != 0 {
                translate_item(&mut item, dx, dy);
            }
            self.display.items.push(item);
        }
        for mut l in sub.links {
            l.rect = offset_rect(l.rect, dx, dy);
            self.links.push(l);
        }
        for mut f in sub.fields {
            f.rect = offset_rect(f.rect, dx, dy);
            self.fields.push(f);
        }
        for mut e in sub.elements {
            e.rect = offset_rect(e.rect, dx, dy);
            self.elements.push(e);
        }
    }

    /// Lay out a flex container (ADR-0036): row/column (+ `-reverse`), `gap`,
    /// `justify-content` (incl. `space-evenly`), `align-items`/`align-self`,
    /// `order`, wrap, and flexible item sizing (`flex-grow`/`-shrink`/`-basis`).
    /// Free space along a row is distributed by grow; overflow is taken back by
    /// shrink (floored at each item's min-content); the cross axis aligns/stretches.
    fn flex_layout(&mut self, node: &StyledNode) {
        self.flush_line();
        let left = self.left;
        let right = self.right.max(left + 1);
        let start_y = self.y;
        let gap = node.style.gap as i32;
        let bg_index = self.display.items.len();

        // Flex items in `order` (stable sort keeps document order within a group).
        let mut items: Vec<&StyledNode> = node
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if e.style.display != Display::None => Some(e.as_ref()),
                _ => None,
            })
            .collect();
        items.sort_by_key(|e| e.style.order);

        if !items.is_empty() {
            match node.style.flex_direction {
                FlexDirection::Row => self.flex_row(&items, left, right, gap, start_y, &node.style),
                FlexDirection::Column => {
                    self.flex_column(&items, left, right, gap, start_y, &node.style)
                }
            }
        }

        let h = (self.y - start_y).max(0) as u32;
        if h > 0 {
            if let Some(color) = node.style.background {
                self.display.items.insert(
                    bg_index,
                    DisplayItem::Rect {
                        rect: Rect::new(left, start_y, (right - left).max(0) as u32, h),
                        color,
                    },
                );
            }
            self.elements.push(ElementBox {
                rect: Rect::new(left, start_y, (right - left).max(0) as u32, h),
                node: node.node_id,
            });
        }
        self.x = self.left;
    }

    #[allow(clippy::too_many_arguments)]
    /// One flex item's main-axis (width) base size for a row: the resolved
    /// `flex-basis`, falling back to the item's content (max-content) width.
    fn flex_base_main(&mut self, item: &StyledNode, avail: i32) -> i32 {
        match item.style.flex_basis {
            cerberus_style::FlexBasis::Px(p) => p.max(0),
            cerberus_style::FlexBasis::Pct(f) => {
                ((f / 100.0) * avail as f32).round().max(0.0) as i32
            }
            cerberus_style::FlexBasis::Content | cerberus_style::FlexBasis::Auto => {
                self.measure_intrinsic_width(item)
            }
        }
    }

    fn flex_row(
        &mut self,
        items: &[&StyledNode],
        left: i32,
        right: i32,
        gap: i32,
        start_y: i32,
        style: &ComputedStyle,
    ) {
        let avail = (right - left).max(1);
        let basis: Vec<i32> = items
            .iter()
            .map(|it| self.flex_base_main(it, avail))
            .collect();

        // Group into lines: wrap on `basis` widths, or a single line otherwise.
        let lines: Vec<Vec<usize>> = if style.flex_wrap {
            let mut lines = Vec::new();
            let mut cur: Vec<usize> = Vec::new();
            let mut used = 0;
            for (i, &w) in basis.iter().enumerate() {
                let add = if cur.is_empty() { w } else { gap + w };
                if !cur.is_empty() && used + add > avail {
                    lines.push(std::mem::take(&mut cur));
                    used = w;
                } else {
                    used += add;
                }
                cur.push(i);
            }
            if !cur.is_empty() {
                lines.push(cur);
            }
            lines
        } else {
            vec![(0..items.len()).collect()]
        };

        let mut y = start_y;
        for line in &lines {
            let n = line.len();
            let gaps = gap * (n as i32 - 1).max(0);
            let inner_avail = (avail - gaps).max(0) as f32;
            let line_basis: Vec<f32> = line.iter().map(|&i| basis[i] as f32).collect();
            let grow: Vec<f32> = line.iter().map(|&i| items[i].style.flex_grow).collect();
            let shrink: Vec<f32> = line.iter().map(|&i| items[i].style.flex_shrink).collect();

            // Min-content floors are only needed (and only measured) when the line
            // is shrinking — the common grow/fit cases skip the extra pass.
            let total_basis: f32 = line_basis.iter().sum();
            let mins: Vec<f32> = if total_basis > inner_avail {
                line.iter()
                    .map(|&i| {
                        self.measure_min_content_width(items[i])
                            .min(basis[i])
                            .max(0) as f32
                    })
                    .collect()
            } else {
                vec![0.0; n]
            };

            let sizes = resolve_flex(&line_basis, &grow, &shrink, &mins, inner_avail);
            let widths: Vec<i32> = sizes.iter().map(|s| s.round().max(1.0) as i32).collect();

            // Leftover after flexing (zero when something grew) is placed by
            // justify-content.
            let content: i32 = widths.iter().sum();
            let free = (avail - content - gaps).max(0);
            let count = n as i32;
            let (mut x, eff_gap) = match style.justify_content {
                JustifyContent::Start => (left, gap),
                JustifyContent::Center => (left + free / 2, gap),
                JustifyContent::End => (left + free, gap),
                JustifyContent::SpaceBetween if count > 1 => (left, gap + free / (count - 1)),
                JustifyContent::SpaceBetween => (left, gap),
                JustifyContent::SpaceAround => {
                    let around = if count > 0 { free / count } else { 0 };
                    (left + around / 2, gap + around)
                }
                JustifyContent::SpaceEvenly => {
                    let around = if count >= 0 { free / (count + 1) } else { 0 };
                    (left + around, gap + around)
                }
            };

            // Placement order (reversed for row-reverse), carrying each item's
            // resolved width.
            let mut placed: Vec<(usize, i32)> =
                line.iter().copied().zip(widths.iter().copied()).collect();
            if style.flex_reverse {
                placed.reverse();
            }

            let mut laid: Vec<(Ctx<'a>, i32, usize)> = Vec::new();
            let mut row_h = 0;
            for (i, w) in placed {
                let mut sub = Ctx::sub(
                    x,
                    x + w,
                    y,
                    self.shaper,
                    self.images,
                    self.forms,
                    self.field_id,
                );
                sub.walk(items[i], None);
                sub.flush_line();
                self.field_id = sub.field_id;
                let h = (sub.y - y).max(1);
                row_h = row_h.max(h);
                laid.push((sub, h, i));
                x += w + eff_gap;
            }
            for (sub, h, i) in laid {
                let align = resolve_align(items[i].style.align_self, style.align_items);
                let dy = match align {
                    // Stretch along a row is the cross (height) axis; treated as
                    // top-aligned for now (height stretch of item backgrounds is a
                    // later refinement).
                    AlignItems::Start | AlignItems::Stretch => 0,
                    AlignItems::Center => (row_h - h) / 2,
                    AlignItems::End => row_h - h,
                };
                self.merge_sub(sub, 0, dy);
            }
            y += row_h + gap;
        }
        self.y = (y - gap).max(start_y);
    }

    fn flex_column(
        &mut self,
        items: &[&StyledNode],
        left: i32,
        right: i32,
        gap: i32,
        start_y: i32,
        style: &ComputedStyle,
    ) {
        // The main axis (height) is content-sized (the container has no definite
        // height), so grow/shrink along it are no-ops; we stack items and align /
        // stretch each on the cross (width) axis. `column-reverse` flips the order.
        let avail_cross = (right - left).max(1);
        let order: Vec<&&StyledNode> = if style.flex_reverse {
            items.iter().rev().collect()
        } else {
            items.iter().collect()
        };
        let mut y = start_y;
        for (idx, it) in order.iter().enumerate() {
            if idx > 0 {
                y += gap;
            }
            let align = resolve_align(it.style.align_self, style.align_items);
            let (x0, w) = match align {
                AlignItems::Stretch => (left, avail_cross),
                _ => {
                    let iw = self.measure_intrinsic_width(it).clamp(1, avail_cross);
                    let dx = match align {
                        AlignItems::Center => (avail_cross - iw) / 2,
                        AlignItems::End => avail_cross - iw,
                        _ => 0,
                    };
                    (left + dx, iw)
                }
            };
            let mut sub = Ctx::sub(
                x0,
                x0 + w,
                y,
                self.shaper,
                self.images,
                self.forms,
                self.field_id,
            );
            sub.walk(it, None);
            sub.flush_line();
            self.field_id = sub.field_id;
            let h = (sub.y - y).max(1);
            self.merge_sub(sub, 0, 0);
            y += h;
        }
        self.y = y;
    }

    /// Lay out a grid container (explicit tracks v1): `grid-template-columns`
    /// (Px fixed; Fr/Auto share the leftover) defines the columns; children are
    /// placed row-major into cells with `gap`; each row's height is its tallest
    /// cell. `grid-template-rows` and auto-placement/spanning are not modeled —
    /// rows are content-sized.
    fn grid_layout(&mut self, node: &StyledNode) {
        self.flush_line();
        let left = self.left;
        let right = self.right.max(left + 1);
        let start_y = self.y;
        let gap = node.style.gap as i32;
        let bg_index = self.display.items.len();

        let cols = &node.style.grid_template_columns;
        let ncols = cols.len().max(1);
        let avail = (right - left).max(1);

        // Resolve column widths.
        let widths: Vec<i32> = if cols.is_empty() {
            vec![avail]
        } else {
            let total_gap = gap * (ncols as i32 - 1).max(0);
            let space = (avail - total_gap).max(ncols as i32);
            let mut fixed = 0i32;
            let mut fr_total = 0f32;
            for t in cols {
                match t {
                    Track::Px(p) => fixed += *p as i32,
                    Track::Fr(f) => fr_total += f.max(0.0),
                    Track::Auto => fr_total += 1.0,
                }
            }
            let leftover = (space - fixed).max(0) as f32;
            cols.iter()
                .map(|t| match t {
                    Track::Px(p) => (*p as i32).max(1),
                    Track::Fr(f) if fr_total > 0.0 => {
                        (leftover * f.max(0.0) / fr_total).floor().max(1.0) as i32
                    }
                    Track::Auto if fr_total > 0.0 => (leftover / fr_total).floor().max(1.0) as i32,
                    _ => 1,
                })
                .collect()
        };

        let mut col_x = Vec::with_capacity(widths.len());
        let mut cx = left;
        for &w in &widths {
            col_x.push(cx);
            cx += w + gap;
        }

        let items: Vec<&StyledNode> = node
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if e.style.display != Display::None => Some(e.as_ref()),
                _ => None,
            })
            .collect();

        let mut y = start_y;
        let mut start = 0;
        while start < items.len() {
            let end = (start + ncols).min(items.len());
            let mut row_h = 0;
            let mut laid: Vec<Ctx<'a>> = Vec::new();
            for (col, it) in items[start..end].iter().enumerate() {
                let x0 = col_x[col];
                let w = widths[col];
                let mut sub = Ctx::sub(
                    x0,
                    x0 + w,
                    y,
                    self.shaper,
                    self.images,
                    self.forms,
                    self.field_id,
                );
                sub.walk(it, None);
                sub.flush_line();
                self.field_id = sub.field_id;
                row_h = row_h.max((sub.y - y).max(1));
                laid.push(sub);
            }
            for sub in laid {
                self.merge_sub(sub, 0, 0);
            }
            y += row_h + gap;
            start = end;
        }
        self.y = if items.is_empty() {
            start_y
        } else {
            (y - gap).max(start_y)
        };

        let h = (self.y - start_y).max(0) as u32;
        if h > 0 {
            if let Some(color) = node.style.background {
                self.display.items.insert(
                    bg_index,
                    DisplayItem::Rect {
                        rect: Rect::new(left, start_y, (right - left).max(0) as u32, h),
                        color,
                    },
                );
            }
            self.elements.push(ElementBox {
                rect: Rect::new(left, start_y, (right - left).max(0) as u32, h),
                node: node.node_id,
            });
        }
        self.x = self.left;
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

/// Offset a rect by `(dx, dy)` (for translating positioned output).
fn offset_rect(r: Rect, dx: i32, dy: i32) -> Rect {
    Rect::new(r.x + dx, r.y + dy, r.w, r.h)
}

fn offset_point(p: Point, dx: i32, dy: i32) -> Point {
    Point::new(p.x + dx, p.y + dy)
}

/// Translate a display item in place by `(dx, dy)` (ADR-0034 positioning).
fn translate_item(item: &mut DisplayItem, dx: i32, dy: i32) {
    match item {
        DisplayItem::Rect { rect, .. } | DisplayItem::Image { rect, .. } => {
            *rect = offset_rect(*rect, dx, dy);
        }
        DisplayItem::Glyphs { origin, .. } => *origin = offset_point(*origin, dx, dy),
        DisplayItem::Line { a, b, .. } => {
            *a = offset_point(*a, dx, dy);
            *b = offset_point(*b, dx, dy);
        }
    }
}

/// Drain `v[from..]`, offsetting each drained element's rect by `(dx, dy)`.
fn drain_offset<T>(
    v: &mut Vec<T>,
    from: usize,
    dx: i32,
    dy: i32,
    rect_of: impl Fn(&mut T) -> &mut Rect,
) -> Vec<T> {
    let mut out: Vec<T> = v.drain(from..).collect();
    for item in &mut out {
        let r = rect_of(item);
        *r = offset_rect(*r, dx, dy);
    }
    out
}

/// A flex item's effective cross alignment: `align-self` if set, else the
/// container's `align-items` (ADR-0036).
fn resolve_align(
    s: cerberus_style::AlignSelf,
    container: cerberus_style::AlignItems,
) -> cerberus_style::AlignItems {
    use cerberus_style::{AlignItems, AlignSelf};
    match s {
        AlignSelf::Auto => container,
        AlignSelf::Start => AlignItems::Start,
        AlignSelf::Center => AlignItems::Center,
        AlignSelf::End => AlignItems::End,
        AlignSelf::Stretch => AlignItems::Stretch,
    }
}

/// Resolve flexible main-axis sizes for one flex line (CSS Flexbox §9.7,
/// pragmatic): distribute positive free space by `grow`, or negative free space
/// by `shrink × basis`, freezing items at their min-content floor (`mins`) and
/// redistributing the remainder. `avail` is the line's main size minus gaps.
fn resolve_flex(basis: &[f32], grow: &[f32], shrink: &[f32], mins: &[f32], avail: f32) -> Vec<f32> {
    let n = basis.len();
    if n == 0 {
        return Vec::new();
    }
    let total_basis: f32 = basis.iter().sum();
    let grow_mode = avail >= total_basis;
    let mut size = basis.to_vec();
    let mut frozen = vec![false; n];
    // Items that cannot flex in this direction are fixed at their basis.
    for i in 0..n {
        let factor = if grow_mode { grow[i] } else { shrink[i] };
        if factor <= 0.0 {
            frozen[i] = true;
        }
    }
    for _ in 0..=n {
        let used: f32 = size.iter().sum();
        let remaining = avail - used;
        if remaining.abs() < 0.5 {
            break;
        }
        let unfrozen: Vec<usize> = (0..n).filter(|&i| !frozen[i]).collect();
        if unfrozen.is_empty() {
            break;
        }
        let sum_factor: f32 = unfrozen
            .iter()
            .map(|&i| {
                if grow_mode {
                    grow[i]
                } else {
                    shrink[i] * basis[i]
                }
            })
            .sum();
        if sum_factor <= 0.0 {
            break;
        }
        let mut froze_any = false;
        for &i in &unfrozen {
            let factor = if grow_mode {
                grow[i]
            } else {
                shrink[i] * basis[i]
            };
            let mut new = size[i] + remaining * (factor / sum_factor);
            if !grow_mode && new < mins[i] {
                new = mins[i];
                frozen[i] = true;
                froze_any = true;
            }
            size[i] = new;
        }
        if !froze_any {
            break;
        }
    }
    size
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
                "tr" => rows.push(e.as_ref()),
                "thead" | "tbody" | "tfoot" => {
                    for inner in &e.children {
                        if let StyledChild::Element(r) = inner {
                            if r.tag == "tr" {
                                rows.push(r.as_ref());
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
        StyledChild::Element(e) if e.tag == "td" || e.tag == "th" => Some(e.as_ref()),
        _ => None,
    })
}

/// The first direct element child of `node` whose tag is `tag`.
fn find_child<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
    node.children.iter().find_map(|c| match c {
        StyledChild::Element(e) if e.tag == tag => Some(e.as_ref()),
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

    #[test]
    fn flex_row_lays_items_on_one_row() {
        let block = lay("<div><div>AAAA</div><div>BBBB</div></div>", 800);
        let flex = lay(
            "<div style='display:flex'><div>AAAA</div><div>BBBB</div></div>",
            800,
        );
        // Block: the two inner divs stack -> glyphs span at least two rows.
        assert!(distinct(&glyph_ys(&block)) >= 2);
        // Flex row: both items share a single row.
        assert_eq!(distinct(&glyph_ys(&flex)), 1);
        // ...and the second item is placed to the right of the first.
        let xs = glyph_xs(&flex);
        assert!(xs.iter().max() > xs.iter().min());
    }

    #[test]
    fn flex_justify_center_shifts_items_right() {
        let start = lay("<div style='display:flex'><div>AA</div></div>", 800);
        let center = lay(
            "<div style='display:flex; justify-content:center'><div>AA</div></div>",
            800,
        );
        let start_x = *glyph_xs(&start).iter().min().unwrap();
        let center_x = *glyph_xs(&center).iter().min().unwrap();
        assert!(
            center_x > start_x,
            "justify-center x {center_x} should exceed start x {start_x}"
        );
    }

    // ---- Flexbox v2: flexible item sizing + alignment (ADR-0036) ----

    /// Background-fill rects sorted left-to-right. Each flex item with a
    /// background paints one rect whose width is the item's laid main size, so
    /// these widths verify flex sizing precisely.
    fn fill_rects(laid: &LaidOut) -> Vec<Rect> {
        let mut rects: Vec<Rect> = laid
            .display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Rect { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        rects.sort_by_key(|r| r.x);
        rects
    }

    #[test]
    fn flex_grow_fills_free_space_equally() {
        // Two `flex:1` items split the container (800 - 2*8 margin = 784) evenly.
        let laid = lay(
            "<div style='display:flex'>\
             <div style='flex:1;background:#ff0000'>A</div>\
             <div style='flex:1;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2, "two item backgrounds");
        let (w0, w1) = (r[0].w as i32, r[1].w as i32);
        assert!(
            (w0 - w1).abs() <= 2,
            "equal grow -> equal widths: {w0} vs {w1}"
        );
        assert!(
            (w0 + w1 - 784).abs() <= 4,
            "items fill the container: {}",
            w0 + w1
        );
    }

    #[test]
    fn flex_grow_distributes_proportionally() {
        // flex:2 takes twice the free space of flex:1 (both basis 0).
        let laid = lay(
            "<div style='display:flex'>\
             <div style='flex:1;background:#ff0000'>A</div>\
             <div style='flex:2;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        let (w0, w1) = (r[0].w as f32, r[1].w as f32);
        assert!(
            (w1 / w0 - 2.0).abs() < 0.15,
            "flex:2 ~ 2x flex:1: {w0} vs {w1}"
        );
    }

    #[test]
    fn flex_shrink_prevents_overflow() {
        // Two 600px bases (1200 total) shrink to fit 784, splitting it evenly.
        let laid = lay(
            "<div style='display:flex'>\
             <div style='flex-basis:600px;background:#ff0000'>A</div>\
             <div style='flex-basis:600px;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        let total = r[0].w as i32 + r[1].w as i32;
        assert!(total <= 788, "shrunk to fit the container: total {total}");
        assert!((r[0].w as i32 - r[1].w as i32).abs() <= 3, "equal shrink");
    }

    #[test]
    fn flex_basis_percent_sizes_items() {
        let laid = lay(
            "<div style='display:flex'>\
             <div style='flex-basis:25%;flex-shrink:0;background:#ff0000'>A</div>\
             <div style='flex-basis:50%;flex-shrink:0;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].w as i32 - 196).abs() <= 3,
            "25% of 784 ~ 196: {}",
            r[0].w
        );
        assert!(
            (r[1].w as i32 - 392).abs() <= 3,
            "50% of 784 ~ 392: {}",
            r[1].w
        );
    }

    #[test]
    fn flex_order_reorders_items() {
        // order:1 comes before order:2 regardless of document order.
        let laid = lay(
            "<div style='display:flex'>\
             <div style='order:2;flex-basis:100px;flex-shrink:0;background:#ff0000'>A</div>\
             <div style='order:1;flex-basis:300px;flex-shrink:0;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].w as i32 - 300).abs() <= 3,
            "order:1 (300px) placed first"
        );
        assert!(
            (r[1].w as i32 - 100).abs() <= 3,
            "order:2 (100px) placed second"
        );
    }

    #[test]
    fn flex_justify_space_between_pushes_to_edges() {
        let laid = lay(
            "<div style='display:flex;justify-content:space-between'>\
             <div style='flex-basis:100px;flex-shrink:0;background:#ff0000'>A</div>\
             <div style='flex-basis:100px;flex-shrink:0;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(r[0].x <= 10, "first item at the start: {}", r[0].x);
        assert!(
            r[1].x >= 600,
            "second item pushed to the far edge: {}",
            r[1].x
        );
    }

    #[test]
    fn flex_row_reverse_flips_item_order() {
        let laid = lay(
            "<div style='display:flex;flex-direction:row-reverse'>\
             <div style='flex-basis:100px;flex-shrink:0;background:#ff0000'>A</div>\
             <div style='flex-basis:300px;flex-shrink:0;background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].w as i32 - 300).abs() <= 3,
            "reverse: B(300) ends up left of A"
        );
        assert!(
            (r[1].w as i32 - 100).abs() <= 3,
            "reverse: A(100) ends up on the right"
        );
    }

    #[test]
    fn flex_column_align_center_shifts_item_right() {
        let start = lay(
            "<div style='display:flex;flex-direction:column'><div>AA</div></div>",
            800,
        );
        let center = lay(
            "<div style='display:flex;flex-direction:column;align-items:center'><div>AA</div></div>",
            800,
        );
        let sx = *glyph_xs(&start).iter().min().unwrap();
        let cx = *glyph_xs(&center).iter().min().unwrap();
        assert!(
            cx > sx,
            "column align-items:center centers the item: {cx} vs {sx}"
        );
    }

    #[test]
    fn flex_align_self_overrides_container() {
        // align-self:center on one item shifts it down within a tall row, while
        // align-items:flex-start keeps the row top-aligned by default.
        let laid = lay(
            "<div style='display:flex;align-items:flex-start'>\
             <div>line one<br>line two<br>line three</div>\
             <div style='align-self:center'>X</div></div>",
            800,
        );
        let ys = glyph_ys(&laid);
        let min_y = *ys.iter().min().unwrap();
        let max_y = *ys.iter().max().unwrap();
        // The centered single-line item sits below the first line of the tall item.
        assert!(max_y > min_y, "align-self:center offsets the item downward");
    }

    #[test]
    fn grid_two_columns_side_by_side() {
        let grid = lay(
            "<div style='display:grid; grid-template-columns:1fr 1fr'>\
             <div>AAAA</div><div>BBBB</div></div>",
            800,
        );
        assert_eq!(distinct(&glyph_ys(&grid)), 1, "two cells on one row");
        let xs = glyph_xs(&grid);
        assert!(xs.iter().max() > xs.iter().min(), "cells side by side");
    }

    #[test]
    fn grid_one_column_stacks() {
        let grid = lay(
            "<div style='display:grid; grid-template-columns:1fr'>\
             <div>A</div><div>B</div></div>",
            800,
        );
        assert!(distinct(&glyph_ys(&grid)) >= 2, "one column -> two rows");
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
    fn opacity_zero_and_visibility_hidden_are_not_painted() {
        // opacity:0 and visibility:hidden suppress this element's paint.
        assert!(glyph_ys(&lay("<p style='opacity:0'>fade-in text</p>", 400)).is_empty());
        assert!(glyph_ys(&lay("<p style='visibility:hidden'>x</p>", 400)).is_empty());
        // A visible child of a visibility:hidden parent still shows.
        let laid = lay(
            "<div style='visibility:hidden'>hide<span style='visibility:visible'>show</span></div>",
            400,
        );
        assert!(
            !glyph_ys(&laid).is_empty(),
            "visible child overrides hidden parent"
        );
        // Partial opacity (>0) still paints (composited, not skipped).
        assert!(!glyph_ys(&lay("<p style='opacity:0.5'>x</p>", 400)).is_empty());
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

    // ---- CSS positioning (ADR-0034) ----

    /// Glyph (x, y) origins in display (paint) order.
    fn glyph_xy(laid: &LaidOut) -> Vec<(i32, i32)> {
        laid.display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some((origin.x, origin.y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn absolute_is_out_of_flow_and_placed_at_its_inset_origin() {
        // The absolute <div> takes no flow space (C rises to the 2nd line) and is
        // placed at top:100/left:40 against the viewport, painted last (on top).
        let laid = lay(
            "<p>A</p><div style=\"position:absolute;top:100px;left:40px\">B</div><p>C</p>",
            400,
        );
        let g = glyph_xy(&laid);
        assert_eq!(g.len(), 3, "A, C (in flow) + B (lifted out)");
        // In paint order the in-flow glyphs come first, the absolute one last.
        let (bx, by) = *g.last().unwrap();
        assert!(
            by >= 100 && (40..70).contains(&bx),
            "B at its inset origin: {bx},{by}"
        );
        // C (the 2nd in-flow glyph) stays near the top — B reserved no space.
        let in_flow_max_y = g[..2].iter().map(|(_, y)| *y).max().unwrap();
        assert!(
            in_flow_max_y < 100,
            "in-flow content not pushed down by abs"
        );
    }

    #[test]
    fn relative_shifts_in_place_and_keeps_its_flow_space() {
        // Glyphs are A, B, C in document order. `relative` translates B in place,
        // so C (index 2) keeps the exact y it has with no positioning.
        let plain = lay("<p>A</p><p>B</p><p>C</p>", 400);
        let rel = lay(
            "<p>A</p><p style=\"position:relative;left:25px;top:10px\">B</p><p>C</p>",
            400,
        );
        let (gp, gr) = (glyph_xy(&plain), glyph_xy(&rel));
        assert_eq!(gp.len(), 3);
        assert_eq!(gr.len(), 3);
        // C unchanged (relative reserved B's flow slot).
        assert_eq!(gr[2].1, gp[2].1, "following element's y is unchanged");
        // B shifted by the insets.
        assert_eq!(gr[1].0, gp[1].0 + 25, "B shifted right by left:25");
        assert_eq!(gr[1].1, gp[1].1 + 10, "B shifted down by top:10");
    }

    #[test]
    fn z_index_orders_positioned_layers_in_paint() {
        // Two absolutes; the higher z-index paints last (on top) regardless of
        // document order.
        let laid = lay(
            "<div style=\"position:absolute;z-index:5;left:100px\">P</div>\
             <div style=\"position:absolute;z-index:1;left:10px\">Q</div>",
            400,
        );
        let g = glyph_xy(&laid);
        assert_eq!(g.len(), 2);
        // Sorted by z: the left:10 (z1) paints first, the left:100 (z5) last.
        assert!(g[0].0 < 30, "low z first");
        assert!(g[1].0 >= 100, "high z on top (painted last)");
    }
}

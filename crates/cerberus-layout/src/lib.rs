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
    AlignItems, ComputedStyle, Display, FlexDirection, JustifyContent, Len, ListStyleType,
    StyledChild, StyledDom, StyledNode, TextAlign, TextTransform, Track, TrackMax,
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
    line_through: bool,
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
    /// Stack of positioned ancestors' (in-flow) border boxes, so `absolute`
    /// resolves against its nearest positioned ancestor, not the viewport
    /// (ADR-0042). Pushed/popped around a positioned block's children.
    cb_stack: Vec<ContainingBlock>,
    /// Whether positioning is active. Only the root flow positions; sub-flows
    /// (table cells, intrinsic measurement) keep elements in-flow (v1).
    pos_enabled: bool,
    /// Whether this context is measuring intrinsic width (laid at a huge probe
    /// width). Flex/grid then pack items at their base size, left-aligned, with no
    /// grow/justify/align offsets — otherwise grow fills the probe width and
    /// center/justify offsets place content at ~width/2, both wildly inflating the
    /// measured width and corrupting nested sizing (ADR-0038).
    measuring: bool,
    /// One-shot: treat the next walked element as a block (the inline-block atom
    /// laid into its own sub gets the full block box model) — ADR-0042.
    as_block_once: bool,
    /// Ordinal of the `list-item` about to be walked (set by the parent's child
    /// loop), used to render a `decimal` `<ol>` marker as "N.". Consumed when the
    /// marker is emitted, before descending, so nested lists number independently.
    list_ordinal: u32,
    /// Pending `text-indent` (px) for the current block's first line, consumed as
    /// the leading offset of the first word placed and then zeroed.
    pending_indent: i32,
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
            cb_stack: Vec::new(),
            pos_enabled: true,
            measuring: false,
            as_block_once: false,
            list_ordinal: 0,
            pending_indent: 0,
        }
    }

    /// A fresh flow context bounded to `left..right` and starting at `y`, used to
    /// lay a table cell's content into its own rectangle. It shares the parent's
    /// shaper/images/forms and produces absolute-coordinate items (no offset
    /// needed). The `field_id` is seeded from the parent so controls inside the
    /// cell continue the document-wide pre-order numbering; the parent reads the
    /// advanced counter back after the cell is flowed.
    // Folding these into a shared `LayoutEnv` (shaper/images/forms/viewport) is
    // the cleaner shape, but that belongs with the #20 walker decomposition; for
    // now the explicit params keep viewport propagation a compile-time obligation.
    #[allow(clippy::too_many_arguments)]
    fn sub(
        left: i32,
        right: i32,
        y: i32,
        shaper: &'a dyn TextShaper,
        images: &'a dyn ImageProvider,
        forms: &'a dyn FormState,
        field_id: u32,
        // The viewport is global to a layout pass; every sub-context must resolve
        // `vh`/`vw` (and viewport-relative insets) against the *real* viewport, so
        // it is a required parameter — making omission a compile error rather than
        // the silent `vw:0/vh:0` collapse that hit flex/grid/table/float children.
        vw: i32,
        vh: i32,
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
            vw,
            vh,
            positioned: Vec::new(),
            pos_order: 0,
            cb_stack: Vec::new(),
            pos_enabled: false,
            measuring: false,
            as_block_once: false,
            list_ordinal: 0,
            pending_indent: 0,
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
                style.inset_left.resolve_vp(cb.w, self.vw, self.vh),
                style.inset_right.resolve_vp(cb.w, self.vw, self.vh),
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
        // An inline-block flows inline but carries the block box model: lay it as
        // an atomic box on the current line (ADR-0042). `as_block_once` is the
        // atom's own sub-layout asking for the block path (avoids re-routing).
        if style.display == Display::InlineBlock && !self.as_block_once {
            self.add_inline_block(node, href);
            self.cur_link_node = saved_link_node;
            return;
        }
        let is_block =
            matches!(style.display, Display::Block | Display::ListItem) || self.as_block_once;
        self.as_block_once = false;
        let saved_left = self.left;
        let saved_right0 = self.right;
        let bg_index = self.display.items.len();
        // The border box (left/right/top + per-side border widths), recorded at
        // block-open and used to paint the background + border and the hit box at
        // block-close (ADR-0040).
        let mut bbox: Option<BorderBox> = None;

        if is_block {
            self.flush_line();
            self.y += style.margin_top;
            self.line_align = style.text_align;
            let avail = (self.right - self.left).max(1);
            let (pl, pr) = (style.padding_left, style.padding_right);
            let (bl, br) = (style.border_left, style.border_right);
            let h_extra = pl + pr + bl + br;
            // Border-box width from width/max-width (box-sizing aware); `margin:
            // auto` centers it, else margin-left offsets (ADR-0039/0040).
            let (box_left, box_w) =
                match resolve_border_box_width(style, avail, h_extra, self.vw, self.vh) {
                    Some(bw) => {
                        let bw = bw.clamp(1, avail);
                        let extra = (avail - bw).max(0);
                        let off = if style.margin_left_auto && style.margin_right_auto {
                            extra / 2
                        } else if style.margin_left_auto {
                            extra
                        } else if style.margin_right_auto {
                            0
                        } else {
                            style.margin_left.clamp(0, extra)
                        };
                        (self.left + off, bw)
                    }
                    None => {
                        let l = self.left + style.margin_left;
                        (l, (self.right - l).max(1))
                    }
                };
            bbox = Some(BorderBox {
                left: box_left,
                right: box_left + box_w,
                top: self.y,
                bt: style.border_top,
                br,
                bb: style.border_bottom,
                bl,
            });
            // A positioned block is the containing block for its descendants'
            // `absolute` (ADR-0042): push its (in-flow) border box; the whole
            // subtree is translated together if this element is later lifted.
            if positioned {
                self.cb_stack.push(ContainingBlock {
                    x: box_left,
                    y: self.y,
                    w: box_w,
                    h: self.vh,
                });
            }
            // Content box = border box inset by border + padding.
            self.left = box_left + bl + pl;
            self.right = (box_left + box_w - br - pr).max(self.left + 1);
            self.y += style.border_top + style.padding_top;
            self.x = self.left;
            if visible && style.display == Display::ListItem {
                if let Some(m) = list_marker(style.list_style_type, self.list_ordinal) {
                    self.add_run(&m, style, None);
                    self.x += space_width(self.shaper, style.font_size.max(1)) as i32;
                }
            }
            // Arm `text-indent` for this block's first line: it is consumed as the
            // leading offset of the first word (so `add_word`'s "first word of
            // line" test `x == left` still holds and no phantom space is inserted),
            // then cleared — only the first line is indented. Nested blocks re-arm
            // it from the inherited value when their own content starts.
            self.pending_indent = style.text_indent;
        }

        let saved_opacity_hidden = self.opacity_hidden;
        self.opacity_hidden = subtree_hidden;
        // Float band state: consecutive `float` children pack left-to-right and
        // wrap (the common column-grid pattern); a non-float child, text, or
        // `clear` drops below the band (ADR-0039). Text wrap-around is not modeled.
        let mut fb = FloatBand::new(self.left, self.right, self.y);
        // Counts the list-item children of this block so an ordered list numbers
        // 1, 2, 3…; each `<ol>`/`<ul>` restarts it, so nested lists are independent.
        let mut item_ordinal = 0u32;
        for child in &node.children {
            match child {
                StyledChild::Text(t) => {
                    self.flush_floats(&mut fb);
                    if visible {
                        self.add_text(t, style, href);
                    }
                }
                StyledChild::Element(e)
                    if e.style.float != cerberus_style::Float::None
                        && e.style.display != Display::None =>
                {
                    self.place_float(e, href, &mut fb);
                }
                StyledChild::Element(e) => {
                    if e.style.clear != cerberus_style::Clear::None {
                        self.flush_floats(&mut fb);
                    }
                    self.flush_floats(&mut fb);
                    if e.style.display == Display::ListItem {
                        item_ordinal += 1;
                        self.list_ordinal = item_ordinal;
                    }
                    self.walk(e, href);
                }
            }
        }
        self.flush_floats(&mut fb);
        self.opacity_hidden = saved_opacity_hidden;

        if is_block {
            self.flush_line();
            // Close the box: bottom padding + border, then apply height/min-height/
            // max-height (ADR-0042) before painting the border box (ADR-0040).
            self.y += style.padding_bottom + style.border_bottom;
            if let Some(bx) = bbox {
                let v_extra = style.padding_top
                    + style.padding_bottom
                    + style.border_top
                    + style.border_bottom;
                let natural = (self.y - bx.top).max(0);
                let sized = resolve_block_height(style, natural, v_extra, self.vw, self.vh);
                self.y = bx.top + sized;
                let h = (self.y - bx.top).max(0) as u32;
                if h > 0 {
                    let rect = Rect::new(bx.left, bx.top, (bx.right - bx.left).max(0) as u32, h);
                    if visible {
                        self.paint_box(bg_index, style, rect, &bx);
                    }
                    self.elements.push(ElementBox {
                        rect,
                        node: node.node_id,
                    });
                }
            }
            // Intrinsic width includes the box's right padding + border (the left
            // insets are already in the content's x). Only while measuring — in
            // real layout `max_x` is unused and a no-width box spans the line.
            if self.measuring {
                self.max_x += style.padding_right + style.border_right;
            }
            self.y += style.margin_bottom;
            self.left = saved_left;
            self.right = saved_right0;
            self.x = self.left;
            // Pop this element's CB before its own `apply_positioning`, so that
            // resolves against *its* containing block, not itself (ADR-0042).
            if positioned {
                self.cb_stack.pop();
            }
        }
        if let Some(base) = pos_base {
            self.apply_positioning(&node.style, base);
        }
        if let Some(r) = saved_right {
            self.right = r;
        }
        self.cur_link_node = saved_link_node;
    }

    /// The containing block for a positioned element (ADR-0034/0042): `fixed`
    /// resolves against the viewport; `absolute` against its nearest positioned
    /// ancestor (top of the CB stack) else the viewport; `relative` against that
    /// ancestor else the page content area.
    fn containing_block(&self, position: cerberus_style::Position) -> ContainingBlock {
        use cerberus_style::Position;
        let viewport = ContainingBlock {
            x: 0,
            y: 0,
            w: self.vw,
            h: self.vh,
        };
        match position {
            Position::Fixed => viewport,
            Position::Absolute => self.cb_stack.last().copied().unwrap_or(viewport),
            _ => self.cb_stack.last().copied().unwrap_or(ContainingBlock {
                x: self.left0,
                y: 0,
                w: (self.right - self.left0).max(0),
                h: self.vh,
            }),
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
                    .resolve_vp(cb.w, self.vw, self.vh)
                    .or_else(|| {
                        style
                            .inset_right
                            .resolve_vp(cb.w, self.vw, self.vh)
                            .map(|r| -r)
                    })
                    .unwrap_or(0);
                let dy = style
                    .inset_top
                    .resolve_vp(cb.h, self.vw, self.vh)
                    .or_else(|| {
                        style
                            .inset_bottom
                            .resolve_vp(cb.h, self.vw, self.vh)
                            .map(|b| -b)
                    })
                    .unwrap_or(0);
                (dx, dy)
            }
            // absolute / fixed: resolve an absolute origin, then translate from
            // the in-flow reference origin to it.
            _ => {
                let ox = style
                    .inset_left
                    .resolve_vp(cb.w, self.vw, self.vh)
                    .map(|l| cb.x + l)
                    .or_else(|| {
                        style
                            .inset_right
                            .resolve_vp(cb.w, self.vw, self.vh)
                            .map(|r| cb.x + cb.w - r - elem_w)
                    })
                    .unwrap_or(base.x);
                let oy = style
                    .inset_top
                    .resolve_vp(cb.h, self.vw, self.vh)
                    .map(|t| cb.y + t)
                    .or_else(|| {
                        style
                            .inset_bottom
                            .resolve_vp(cb.h, self.vw, self.vh)
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

    /// Shape a run and apply `letter-spacing` (px, may be negative) to advances,
    /// returning the glyphs and their total width (ADR-0041).
    fn shape_run(&self, text: &str, px: u32, style: &ComputedStyle) -> (Vec<GlyphBox>, u32) {
        let mut glyphs = self.shaper.shape(text, px);
        if style.letter_spacing != 0 {
            for g in &mut glyphs {
                g.advance = (g.advance as i32 + style.letter_spacing).max(0) as u32;
            }
        }
        let w = glyphs.iter().map(|g| g.advance).sum();
        (glyphs, w)
    }

    fn add_text(&mut self, text: &str, style: &ComputedStyle, href: Option<&str>) {
        // `text-transform` rewrites the run before shaping (transient String).
        let transformed;
        let text = match style.text_transform {
            TextTransform::None => text,
            TextTransform::Uppercase => {
                transformed = text.to_uppercase();
                transformed.as_str()
            }
            TextTransform::Lowercase => {
                transformed = text.to_lowercase();
                transformed.as_str()
            }
            TextTransform::Capitalize => {
                transformed = capitalize_words(text);
                transformed.as_str()
            }
        };
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
        } else if style.nowrap {
            // `white-space: nowrap`: collapse runs of whitespace to single spaces
            // like normal text, but place the whole thing as one atomic run so it
            // never wraps (it may overflow the container, per spec).
            let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !collapsed.is_empty() {
                self.add_run(&collapsed, style, href);
            }
        } else {
            for word in text.split_whitespace() {
                self.add_word(word, style, href);
            }
        }
    }

    fn add_word(&mut self, word: &str, style: &ComputedStyle, href: Option<&str>) {
        let px = style.font_size.max(1);
        let (glyphs, w) = self.shape_run(word, px, style);
        let at_line_start = self.x == self.left;
        // The leading offset before this word: at the start of a line it is the
        // one-shot `text-indent` (usually 0), otherwise the inter-word space
        // (widened/trimmed by `word-spacing`, clamped so a large negative can't
        // reverse the cursor).
        let gap = if at_line_start {
            std::mem::take(&mut self.pending_indent).max(0)
        } else {
            (space_width(self.shaper, px) as i32 + style.word_spacing).max(0)
        };
        if !at_line_start && self.x + gap + w as i32 > self.right {
            self.newline();
        } else {
            self.x += gap;
        }
        self.push_piece(px, w, glyphs, style, href);
    }

    fn add_run(&mut self, text: &str, style: &ComputedStyle, href: Option<&str>) {
        let px = style.font_size.max(1);
        let (glyphs, w) = self.shape_run(text, px, style);
        self.push_piece(px, w, glyphs, style, href);
    }

    /// Place an `inline-block` as an atomic box on the current line (ADR-0042): a
    /// `width`-sized (else shrink-to-fit) sub laid with the full block box model,
    /// positioned at the inline cursor and advancing it; wraps if it overflows.
    fn add_inline_block(&mut self, e: &StyledNode, in_link: Option<&str>) {
        let avail = (self.right - self.left).max(1);
        let h_extra = e.style.padding_left
            + e.style.padding_right
            + e.style.border_left
            + e.style.border_right;
        let w = resolve_border_box_width(&e.style, avail, h_extra, self.vw, self.vh)
            .unwrap_or_else(|| self.measure_intrinsic_width(e).min(avail))
            .clamp(1, avail);
        if self.x != self.left && self.x + w > self.right {
            self.newline();
        }
        let mut sub = Ctx::sub(
            self.x,
            self.x + w,
            self.y,
            self.shaper,
            self.images,
            self.forms,
            self.field_id,
            self.vw,
            self.vh,
        );
        sub.measuring = self.measuring;
        sub.as_block_once = true; // lay `e` with the block box model, filling [x, x+w]
        sub.walk(e, in_link);
        sub.flush_line();
        self.field_id = sub.field_id;
        let h = (sub.y - self.y).max(1);
        self.merge_sub(sub, 0, 0);
        self.x += w;
        self.max_x = self.max_x.max(self.x);
        self.line_h = self.line_h.max(h);
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
            line_through: style.line_through,
            href: href.map(str::to_string),
            link_node: self.cur_link_node,
        });
        self.x += w as i32;
        self.max_x = self.max_x.max(self.x);
        let lh = style.line_height.unwrap_or_else(|| line_height(px));
        self.line_h = self.line_h.max(lh);
    }

    /// Lay out an `<img>`: draw the decoded image if ready, else a sized
    /// placeholder, else the alt text. Lazy-loading is ignored (raw render).
    fn image(&mut self, node: &StyledNode, in_link: Option<&str>) {
        // Resolve srcset/sizes/data-src to one URL (ADR-0046), using the same
        // viewport width the fetch-time collector used, so the lookup hits.
        let Some(src) = pick_img_url(|n| node.attr(n), self.vw.max(0) as u32) else {
            self.image_alt(node, in_link);
            return;
        };
        let src = src.as_str();
        let attr_w = node.attr("width").and_then(parse_dim);
        let attr_h = node.attr("height").and_then(parse_dim);

        if let Some(image) = self.images.get(src) {
            let (mut w, mut h) = replaced_size(attr_w, attr_h, image.size);
            let max_w = (self.right - self.left).max(1) as u32;
            if w > max_w {
                h = (h as f32 * max_w as f32 / w as f32).round() as u32;
                w = max_w;
            }
            self.place_box(w, h.max(1));
            let rect = Rect::new(self.x, self.y, w, h.max(1));
            let fit = node.style.object_fit;
            let pos = node.style.object_position;
            self.display.push(DisplayItem::Image {
                rect,
                image,
                fit,
                pos,
            });
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
        self.control_box(w, h, &node.style, FIELD_BG);
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
        self.control_box(w, h as u32, &node.style, FIELD_BG);
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
        self.control_box(w, h as u32, &node.style, FIELD_BG);
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
        self.control_box(s, s, &node.style, Color::WHITE);
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
        self.control_box(w, h as u32, style, BUTTON_BG);
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
    /// Paint a form control's chrome. The UA `default_fill`/border are just
    /// defaults now: author CSS overrides them, so `<button style="background:
    /// #0a7">` (or an input with a custom `border`) renders as styled instead of
    /// the built-in grey — the control paths used to ignore the cascade entirely
    /// (#66). A `background` set by the page wins the fill; a border color wins
    /// only when the control actually has a border (border-color is always a
    /// value, so the border *width* is what signals author intent).
    fn control_box(&mut self, w: u32, h: u32, style: &ComputedStyle, default_fill: Color) {
        let fill = style.background.unwrap_or(default_fill);
        let has_border = style
            .border_top
            .max(style.border_right)
            .max(style.border_bottom)
            .max(style.border_left)
            > 0;
        let border = if has_border {
            style.border_color
        } else {
            CONTROL_BORDER
        };
        self.place_box(w, h);
        self.display.push(DisplayItem::Rect {
            rect: Rect::new(self.x, self.y, w, h),
            color: border,
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
        // text-indent is a first-line-only effect; once we wrap it no longer
        // applies (it is normally already consumed by the first word).
        self.pending_indent = 0;
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
            if piece.line_through {
                // A 1px rule through the vertical middle of the text (≈ half the
                // font size below the baseline-top), in the text color.
                self.display.push(DisplayItem::Rect {
                    rect: Rect::new(x, piece.y + piece.px as i32 / 2, piece.w, 1),
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
        self.measuring = true;
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
                self.vw,
                self.vh,
            ))
        });
        scratch.reset_for_measure(field_id);
        // Measure an inline-block by its block content (avoids re-routing into
        // `add_inline_block`, which would recurse here) — ADR-0042.
        scratch.as_block_once = matches!(node.style.display, Display::InlineBlock);
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
                self.vw,
                self.vh,
            ))
        });
        scratch.reset_for_measure(field_id);
        scratch.right = 1; // force a wrap at every opportunity
        scratch.as_block_once = matches!(node.style.display, Display::InlineBlock);
        scratch.walk(node, None);
        scratch.flush_line();
        let width = scratch.max_x.max(1);
        self.scratch = Some(scratch);
        width
    }

    /// Insert a box's fill at `idx` — a `linear-gradient`, else a solid color
    /// (rounded when `radius > 0`), then any `background-image` — returning the
    /// next index (ADR-0038/0041).
    fn fill_box(&mut self, idx0: usize, style: &ComputedStyle, rect: Rect, radius: u16) -> usize {
        let mut idx = idx0;
        if let Some(g) = style.background_gradient.as_deref() {
            self.display.items.insert(
                idx,
                DisplayItem::Gradient {
                    rect,
                    start: g.start,
                    end: g.end,
                    vertical: g.vertical,
                    radius,
                },
            );
            idx += 1;
        } else if let Some(color) = style.background {
            let item = if radius > 0 {
                DisplayItem::RoundRect {
                    rect,
                    color,
                    radius,
                }
            } else {
                DisplayItem::Rect { rect, color }
            };
            self.display.items.insert(idx, item);
            idx += 1;
        }
        if let Some(url) = &style.background_image {
            if let Some(img) = self.images.get(url) {
                self.display.items.insert(
                    idx,
                    DisplayItem::Image {
                        rect,
                        image: img,
                        fit: style.background_size,
                        pos: style.background_position,
                    },
                );
                idx += 1;
            }
        }
        idx
    }

    /// Paint a block's box behind content (ADR-0040/0041): the drop shadow, then
    /// the background (color/gradient/image), then the border. With a corner
    /// radius the border is an outer rounded rect under the inset rounded fill;
    /// otherwise it's four solid edge rects.
    fn paint_box(&mut self, bg_index: usize, style: &ComputedStyle, rect: Rect, bx: &BorderBox) {
        let mut idx = bg_index;
        if let Some(sh) = style.box_shadow.as_deref() {
            let srect = Rect::new(rect.x + sh.dx, rect.y + sh.dy, rect.w, rect.h);
            self.display.items.insert(
                idx,
                DisplayItem::Shadow {
                    rect: srect,
                    blur: sh.blur.max(0) as u16,
                    color: sh.color,
                },
            );
            idx += 1;
        }
        let radius = style.border_radius;
        let has_border = bx.bt > 0 || bx.br > 0 || bx.bb > 0 || bx.bl > 0;
        if radius > 0 {
            if has_border {
                self.display.items.insert(
                    idx,
                    DisplayItem::RoundRect {
                        rect,
                        color: style.border_color,
                        radius,
                    },
                );
                idx += 1;
            }
            let inner = Rect::new(
                rect.x + bx.bl,
                rect.y + bx.bt,
                (rect.w as i32 - bx.bl - bx.br).max(0) as u32,
                (rect.h as i32 - bx.bt - bx.bb).max(0) as u32,
            );
            let inner_r = (radius as i32 - bx.bl.max(bx.br).max(bx.bt).max(bx.bb)).max(0) as u16;
            idx = self.fill_box(idx, style, inner, inner_r);
        } else {
            idx = self.fill_box(idx, style, rect, 0);
            let col = style.border_color;
            let (l, t) = (rect.x, rect.y);
            let (w, h) = (rect.w as i32, rect.h as i32);
            if bx.bt > 0 {
                self.display.items.insert(
                    idx,
                    DisplayItem::Rect {
                        rect: Rect::new(l, t, w.max(0) as u32, bx.bt as u32),
                        color: col,
                    },
                );
                idx += 1;
            }
            if bx.bb > 0 {
                self.display.items.insert(
                    idx,
                    DisplayItem::Rect {
                        rect: Rect::new(l, t + h - bx.bb, w.max(0) as u32, bx.bb as u32),
                        color: col,
                    },
                );
                idx += 1;
            }
            if bx.bl > 0 {
                self.display.items.insert(
                    idx,
                    DisplayItem::Rect {
                        rect: Rect::new(l, t, bx.bl as u32, h.max(0) as u32),
                        color: col,
                    },
                );
                idx += 1;
            }
            if bx.br > 0 {
                self.display.items.insert(
                    idx,
                    DisplayItem::Rect {
                        rect: Rect::new(l + w - bx.br, t, bx.br as u32, h.max(0) as u32),
                        color: col,
                    },
                );
                idx += 1;
            }
        }
        // `overflow` clips the content (laid before this call, now at `idx..`) to
        // the padding box (ADR-0043).
        if style.overflow_clip && self.display.items.len() > idx {
            let clip = Rect::new(
                rect.x + bx.bl,
                rect.y + bx.bt,
                (rect.w as i32 - bx.bl - bx.br).max(0) as u32,
                (rect.h as i32 - bx.bt - bx.bb).max(0) as u32,
            );
            self.display
                .items
                .insert(idx, DisplayItem::ClipPush { rect: clip });
            self.display.items.push(DisplayItem::ClipPop);
        }
    }

    /// Place one `float` child into the current float band: pack left-to-right,
    /// wrapping to a new row when it doesn't fit, sizing it from its
    /// `width`/`max-width` (else shrink-to-fit) — ADR-0039/0043. `float:left`
    /// packs from the left, `float:right` from the right; text wrap-around is not
    /// modeled (following in-flow content drops below the band).
    fn place_float(&mut self, e: &StyledNode, in_link: Option<&str>, fb: &mut FloatBand) {
        let is_right = e.style.float == cerberus_style::Float::Right;
        let avail = (self.right - self.left).max(1);
        let explicit = resolve_block_width(&e.style, avail, self.vw, self.vh);
        let w = explicit
            .unwrap_or_else(|| self.measure_intrinsic_width(e).min(avail))
            .clamp(1, avail);
        if !fb.active {
            fb.active = true;
            fb.row_top = self.y;
            fb.x = fb.left;
            fb.right_x = fb.right;
            fb.row_h = 0;
            fb.bottom = self.y;
        }
        // Wrap to a new band row when the float won't fit between the left and
        // right cursors.
        let at_row_start = fb.x == fb.left && fb.right_x == fb.right;
        if !at_row_start && w > (fb.right_x - fb.x).max(0) {
            fb.row_top += fb.row_h;
            fb.x = fb.left;
            fb.right_x = fb.right;
            fb.row_h = 0;
        }
        let place_x = if is_right { fb.right_x - w } else { fb.x };
        // An explicit-width float gets the full avail (so `walk` resolves its
        // width% against the container); a shrink-to-fit float is bounded to its
        // content. Either way its box lands in `[place_x, place_x + w]`.
        let sub_right = if explicit.is_some() {
            place_x + avail
        } else {
            place_x + w
        };
        let mut sub = Ctx::sub(
            place_x,
            sub_right,
            fb.row_top,
            self.shaper,
            self.images,
            self.forms,
            self.field_id,
            self.vw,
            self.vh,
        );
        sub.measuring = self.measuring;
        sub.walk(e, in_link);
        sub.flush_line();
        self.field_id = sub.field_id;
        let h = (sub.y - fb.row_top).max(1);
        self.merge_sub(sub, 0, 0);
        fb.row_h = fb.row_h.max(h);
        if is_right {
            fb.right_x -= w;
        } else {
            fb.x += w;
        }
        fb.bottom = fb.bottom.max(fb.row_top + fb.row_h);
    }

    /// Close the float band: drop the flow below the floats (also handles `clear`
    /// and any in-flow content following floats) — ADR-0039.
    fn flush_floats(&mut self, fb: &mut FloatBand) {
        if fb.active {
            self.y = self.y.max(fb.bottom);
            self.x = self.left;
            *fb = FloatBand::new(fb.left, fb.right, self.y);
        }
    }

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
        // Bubble the sub's content extent up so flex/grid/table containers report a
        // real intrinsic width to `measure_intrinsic_width` (ADR-0038).
        self.max_x = self.max_x.max(sub.max_x + dx);
    }

    /// Lay out a flex container (ADR-0036): row/column (+ `-reverse`), `gap`,
    /// `justify-content` (incl. `space-evenly`), `align-items`/`align-self`,
    /// `order`, wrap, and flexible item sizing (`flex-grow`/`-shrink`/`-basis`).
    /// Free space along a row is distributed by grow; overflow is taken back by
    /// shrink (floored at each item's min-content); the cross axis aligns/stretches.
    fn flex_layout(&mut self, node: &StyledNode) {
        self.flush_line();
        let s = &node.style;
        // The container border box; items lay inside it inset by border + padding
        // (ADR-0040).
        let box_left = self.left;
        let box_right = self.right.max(box_left + 1);
        let box_top = self.y;
        let left = box_left + s.border_left + s.padding_left;
        let right = (box_right - s.border_right - s.padding_right).max(left + 1);
        let start_y = box_top + s.border_top + s.padding_top;
        self.y = start_y;
        let gap = s.gap as i32;
        let bg_index = self.display.items.len();

        // Flex items in `order` (stable sort keeps document order within a group).
        let mut items: Vec<&StyledNode> = node
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if is_flex_grid_item(e) => Some(e.as_ref()),
                _ => None,
            })
            .collect();
        items.sort_by_key(|e| e.style.order);

        let (ds, ls, fs, es) = (
            self.display.items.len(),
            self.links.len(),
            self.fields.len(),
            self.elements.len(),
        );
        if !items.is_empty() {
            match node.style.flex_direction {
                FlexDirection::Row => self.flex_row(&items, left, right, gap, start_y, &node.style),
                FlexDirection::Column => {
                    self.flex_column(&items, left, right, gap, start_y, &node.style)
                }
            }
        }

        self.y += s.padding_bottom + s.border_bottom;
        // Apply height/min-height/max-height (ADR-0042). When the box is taller
        // than its content, center/end-align the content along the block axis (the
        // common full-height hero) — row uses align-items, column uses
        // justify-content as the vertical proxy.
        let v_extra = s.padding_top + s.padding_bottom + s.border_top + s.border_bottom;
        let natural = (self.y - box_top).max(0);
        let sized = resolve_block_height(s, natural, v_extra, self.vw, self.vh);
        if sized > natural {
            let extra = sized - natural;
            let vrt = matches!(s.flex_direction, FlexDirection::Row);
            let dy = if vrt {
                match s.align_items {
                    AlignItems::Center => extra / 2,
                    AlignItems::End => extra,
                    _ => 0,
                }
            } else {
                match s.justify_content {
                    JustifyContent::Center => extra / 2,
                    JustifyContent::End => extra,
                    _ => 0,
                }
            };
            if dy != 0 {
                for item in &mut self.display.items[ds..] {
                    translate_item(item, 0, dy);
                }
                for l in &mut self.links[ls..] {
                    l.rect = offset_rect(l.rect, 0, dy);
                }
                for f in &mut self.fields[fs..] {
                    f.rect = offset_rect(f.rect, 0, dy);
                }
                for e in &mut self.elements[es..] {
                    e.rect = offset_rect(e.rect, 0, dy);
                }
            }
            self.y = box_top + sized;
        }
        let h = (self.y - box_top).max(0) as u32;
        if h > 0 {
            let rect = Rect::new(box_left, box_top, (box_right - box_left).max(0) as u32, h);
            let bx = BorderBox {
                left: box_left,
                right: box_right,
                top: box_top,
                bt: s.border_top,
                br: s.border_right,
                bb: s.border_bottom,
                bl: s.border_left,
            };
            self.paint_box(bg_index, &node.style, rect, &bx);
            self.elements.push(ElementBox {
                rect,
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

            // Probe width (intrinsic measurement): use base sizes only — no grow
            // (it would fill the huge probe width) and no shrink — so the measured
            // extent reflects content, not the probe (ADR-0038).
            let widths: Vec<i32> = if self.measuring {
                line_basis.iter().map(|b| (*b as i32).max(1)).collect()
            } else {
                let grow: Vec<f32> = line.iter().map(|&i| items[i].style.flex_grow).collect();
                let shrink: Vec<f32> = line.iter().map(|&i| items[i].style.flex_shrink).collect();
                // Min-content floors are only needed (and measured) when shrinking.
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
                sizes.iter().map(|s| s.round().max(1.0) as i32).collect()
            };

            // Leftover after flexing (zero when something grew) is placed by
            // justify-content; while measuring, pack at the start.
            let content: i32 = widths.iter().sum();
            let free = (avail - content - gaps).max(0);
            let count = n as i32;
            let (mut x, eff_gap) = if self.measuring {
                (left, gap)
            } else {
                match style.justify_content {
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
                    self.vw,
                    self.vh,
                );
                sub.measuring = self.measuring;
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
                let dy = if self.measuring {
                    0
                } else {
                    match align {
                        // Stretch along a row is the cross (height) axis; treated as
                        // top-aligned for now (height stretch of item backgrounds is
                        // a later refinement).
                        AlignItems::Start | AlignItems::Stretch => 0,
                        AlignItems::Center => (row_h - h) / 2,
                        AlignItems::End => row_h - h,
                    }
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
            // While measuring (huge probe width), don't stretch or center —
            // size each item to its content at the left, so the column's measured
            // width is the widest item's content, not the probe (ADR-0038).
            let (x0, w) = if self.measuring {
                (left, self.measure_intrinsic_width(it).max(1))
            } else {
                match align {
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
                self.vw,
                self.vh,
            );
            sub.measuring = self.measuring;
            sub.walk(it, None);
            sub.flush_line();
            self.field_id = sub.field_id;
            let h = (sub.y - y).max(1);
            self.merge_sub(sub, 0, 0);
            y += h;
        }
        self.y = y;
    }

    /// Lay out a grid container (ADR-0038): resolve column tracks
    /// (`px`/`fr`/`auto`/`minmax()`, or `repeat(auto-fill, …)` whose count comes
    /// from the container width), auto-place items into a 2-D occupancy grid
    /// honoring `grid-column`/`grid-row` spans, size rows from
    /// `grid-template-rows`/`grid-auto-rows` or content, and place each item
    /// (spanning the union of its tracks).
    fn grid_layout(&mut self, node: &StyledNode) {
        self.flush_line();
        let s = &node.style;
        // Container border box; cells lay inside it inset by border+padding (0040).
        let box_left = self.left;
        let box_right = self.right.max(box_left + 1);
        let box_top = self.y;
        let left = box_left + s.border_left + s.padding_left;
        let right = (box_right - s.border_right - s.padding_right).max(left + 1);
        let start_y = box_top + s.border_top + s.padding_top;
        let gap = s.gap as i32;
        let bg_index = self.display.items.len();
        let avail = (right - left).max(1);

        let items: Vec<&StyledNode> = node
            .children
            .iter()
            .filter_map(|c| match c {
                StyledChild::Element(e) if is_flex_grid_item(e) => Some(e.as_ref()),
                _ => None,
            })
            .collect();

        // While measuring (huge probe width), don't expand the template (auto-fill
        // would create thousands of columns); use one content-wide column so the
        // grid's measured width is the widest item's content (ADR-0038).
        let widths = if self.measuring {
            let w = items
                .iter()
                .map(|it| self.measure_intrinsic_width(it))
                .max()
                .unwrap_or(1)
                .max(1);
            vec![w]
        } else {
            resolve_grid_columns(&node.style, avail, gap)
        };
        let ncols = widths.len().max(1);
        let mut col_x = Vec::with_capacity(widths.len());
        let mut cx = left;
        for &w in &widths {
            col_x.push(cx);
            cx += w + gap;
        }

        // Auto-placement: scan row-major for the first free span_c × span_r block.
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut placed: Vec<GridPlacement> = Vec::with_capacity(items.len());
        let mut cursor = (0usize, 0usize); // (row, col) search hint
                                           // The content track: where items with unresolvable named placement land
                                           // (the common `1fr [content] minmax/content 1fr` centering pattern would
                                           // otherwise drop them into a leading gutter).
        let content_col = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
            .unwrap_or(0);
        for it in &items {
            let rs = (it.style.grid_row_span as usize).max(1);
            let (r0, c0, cs) = if it.style.grid_named_place {
                let c0 = content_col;
                (find_free_in_col(&mut occ, ncols, c0, rs), c0, 1)
            } else {
                let cs = (it.style.grid_column_span as usize).clamp(1, ncols);
                let (r0, c0) = find_free_cell(&mut occ, ncols, cs, rs, cursor);
                (r0, c0, cs)
            };
            for row in occ.iter_mut().take(r0 + rs).skip(r0) {
                for cell in row.iter_mut().take(c0 + cs).skip(c0) {
                    *cell = true;
                }
            }
            cursor = (r0, (c0 + cs) % ncols.max(1));
            placed.push(GridPlacement { r0, c0, cs, rs });
        }
        let nrows = occ.len();

        // Lay each item into its column-span width (starting at `start_y`), and
        // record its content height.
        let mut subs: Vec<(Ctx<'a>, i32)> = Vec::with_capacity(items.len());
        for (it, p) in items.iter().zip(&placed) {
            let x0 = col_x[p.c0];
            let span_w = span_extent(&widths, p.c0, p.cs, gap);
            let mut sub = Ctx::sub(
                x0,
                x0 + span_w,
                start_y,
                self.shaper,
                self.images,
                self.forms,
                self.field_id,
                self.vw,
                self.vh,
            );
            sub.measuring = self.measuring;
            sub.walk(it, None);
            sub.flush_line();
            self.field_id = sub.field_id;
            let h = (sub.y - start_y).max(1);
            subs.push((sub, h));
        }

        // Row heights: explicit (`grid-template-rows`/`grid-auto-rows`) or the
        // tallest single-row item; multi-row items push any deficit onto their
        // last spanned row so they never overlap the next row.
        let mut row_h = vec![0i32; nrows];
        for (r, h) in row_h.iter_mut().enumerate() {
            *h = explicit_row_height(&node.style, r);
        }
        for ((_, h), p) in subs.iter().zip(&placed) {
            if p.rs == 1 {
                row_h[p.r0] = row_h[p.r0].max(*h);
            }
        }
        for ((_, h), p) in subs.iter().zip(&placed) {
            if p.rs > 1 {
                let span: i32 =
                    row_h[p.r0..p.r0 + p.rs].iter().sum::<i32>() + gap * (p.rs as i32 - 1);
                if span < *h {
                    row_h[p.r0 + p.rs - 1] += *h - span;
                }
            }
        }
        let mut row_y = Vec::with_capacity(nrows);
        let mut ry = start_y;
        for &h in &row_h {
            row_y.push(ry);
            ry += h + gap;
        }

        for ((sub, _), p) in subs.into_iter().zip(&placed) {
            let dy = row_y.get(p.r0).copied().unwrap_or(start_y) - start_y;
            self.merge_sub(sub, 0, dy);
        }
        self.y = if items.is_empty() {
            start_y
        } else {
            (ry - gap).max(start_y)
        };
        self.y += s.padding_bottom + s.border_bottom;
        // Apply height/min-height/max-height to the grid container (ADR-0042).
        let v_extra = s.padding_top + s.padding_bottom + s.border_top + s.border_bottom;
        let natural = (self.y - box_top).max(0);
        self.y = box_top + resolve_block_height(s, natural, v_extra, self.vw, self.vh);

        let h = (self.y - box_top).max(0) as u32;
        if h > 0 {
            let rect = Rect::new(box_left, box_top, (box_right - box_left).max(0) as u32, h);
            let bx = BorderBox {
                left: box_left,
                right: box_right,
                top: box_top,
                bt: s.border_top,
                br: s.border_right,
                bb: s.border_bottom,
                bl: s.border_left,
            };
            self.paint_box(bg_index, &node.style, rect, &bx);
            self.elements.push(ElementBox {
                rect,
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
            self.vw,
            self.vh,
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

/// Uppercase the first letter of each whitespace-separated word (`text-transform:
/// capitalize`) — ADR-0041.
fn capitalize_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            at_word_start = true;
            out.push(c);
        } else if at_word_start {
            at_word_start = false;
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
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
        DisplayItem::Rect { rect, .. }
        | DisplayItem::RoundRect { rect, .. }
        | DisplayItem::Gradient { rect, .. }
        | DisplayItem::Shadow { rect, .. }
        | DisplayItem::ClipPush { rect }
        | DisplayItem::Image { rect, .. } => {
            *rect = offset_rect(*rect, dx, dy);
        }
        DisplayItem::Glyphs { origin, .. } => *origin = offset_point(*origin, dx, dy),
        DisplayItem::Line { a, b, .. } => {
            *a = offset_point(*a, dx, dy);
            *b = offset_point(*b, dx, dy);
        }
        DisplayItem::ClipPop => {}
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
/// A block's border box (edges + per-side border widths), recorded at block-open
/// to paint the background/border and the hit box at block-close (ADR-0040).
struct BorderBox {
    left: i32,
    right: i32,
    top: i32,
    bt: i32,
    br: i32,
    bb: i32,
    bl: i32,
}

/// The used **border-box** width of a block (box-sizing aware) from
/// `width`/`max-width`, or `None` when unconstrained — `h_extra` is the
/// horizontal padding+border (ADR-0040).
fn resolve_border_box_width(
    style: &ComputedStyle,
    avail: i32,
    h_extra: i32,
    vw: i32,
    vh: i32,
) -> Option<i32> {
    let raw = resolve_block_width(style, avail, vw, vh)?;
    Some(match style.box_sizing {
        cerberus_style::BoxSizing::BorderBox => raw,
        cerberus_style::BoxSizing::ContentBox => raw + h_extra,
    })
}

/// State for packing a run of `float` siblings into rows (ADR-0039).
struct FloatBand {
    /// The container's content-left (where each band row starts).
    left: i32,
    /// The container's content-right (where right floats start).
    right: i32,
    /// Next left-float x within the current row.
    x: i32,
    /// Next right-float right-edge within the current row.
    right_x: i32,
    /// Top y of the current band row.
    row_top: i32,
    /// Height of the tallest float in the current row.
    row_h: i32,
    /// Whether any float is currently placed (the band is open).
    active: bool,
    /// The lowest point reached by any float (where flow resumes).
    bottom: i32,
}

impl FloatBand {
    fn new(left: i32, right: i32, y: i32) -> Self {
        Self {
            left,
            right,
            x: left,
            right_x: right,
            row_top: y,
            row_h: 0,
            active: false,
            bottom: y,
        }
    }
}

/// The used content width of a block from `width`/`max-width`/`min-width`, or
/// `None` when unconstrained (fill the available width) — ADR-0039.
fn resolve_block_width(style: &ComputedStyle, avail: i32, vw: i32, vh: i32) -> Option<i32> {
    let res = |len: Len| len.resolve_vp(avail, vw, vh).map(|v| v.max(0));
    let mut w = res(style.width);
    if let Some(mw) = res(style.max_width) {
        w = Some(w.unwrap_or(avail).min(mw));
    }
    if let Some(mw) = res(style.min_width) {
        w = Some(w.unwrap_or(0).max(mw));
    }
    let w = w?;
    // Not a constraint if `width` is auto and it wouldn't narrow the box.
    if w >= avail && matches!(style.width, Len::Auto) {
        return None;
    }
    Some(w.clamp(1, avail))
}

/// The used **border-box** height of a block (box-sizing aware) from `height`/
/// `min-height`/`max-height`, given the content's natural border-box height.
/// `%` heights (indefinite parent) are ignored. `v_extra` is vertical
/// padding+border (ADR-0042).
fn resolve_block_height(
    style: &ComputedStyle,
    natural: i32,
    v_extra: i32,
    vw: i32,
    vh: i32,
) -> i32 {
    // Only px / vw / vh count; `%`/`auto` leave the box content-sized.
    let res = |len: Len| match len {
        Len::Px(p) => Some(p.max(0)),
        Len::Vw(f) => Some((f / 100.0 * vw as f32).round().max(0.0) as i32),
        Len::Vh(f) => Some((f / 100.0 * vh as f32).round().max(0.0) as i32),
        Len::Auto | Len::Pct(_) => None,
    };
    let adjust = |v: i32| match style.box_sizing {
        cerberus_style::BoxSizing::BorderBox => v,
        cerberus_style::BoxSizing::ContentBox => v + v_extra,
    };
    let mut h = res(style.height).map(adjust).unwrap_or(natural);
    if let Some(min) = res(style.min_height) {
        h = h.max(adjust(min));
    }
    if let Some(max) = res(style.max_height) {
        h = h.min(adjust(max));
    }
    h.max(0)
}

/// Whether an element participates in flex/grid layout: rendered and **in flow**.
/// Per CSS, `absolute`/`fixed` children are taken out of flex/grid flow (they
/// don't size or shift the tracks) — including them corrupts sizing, e.g. a
/// `width:100%` absolute overlay forcing its siblings to min-content (ADR-0038).
fn is_flex_grid_item(e: &StyledNode) -> bool {
    e.style.display != Display::None
        && !matches!(
            e.style.position,
            cerberus_style::Position::Absolute | cerberus_style::Position::Fixed
        )
}

/// One item's resolved grid placement: top-left cell + span (ADR-0038).
struct GridPlacement {
    r0: usize,
    c0: usize,
    cs: usize,
    rs: usize,
}

/// Find the first free `cs × rs` cell block scanning row-major from `cursor`,
/// growing the occupancy grid as needed.
fn find_free_cell(
    occ: &mut Vec<Vec<bool>>,
    ncols: usize,
    cs: usize,
    rs: usize,
    cursor: (usize, usize),
) -> (usize, usize) {
    let ncols = ncols.max(1);
    let (mut r, mut c) = cursor;
    if c + cs > ncols {
        c = 0;
        r += 1;
    }
    loop {
        while occ.len() < r + rs {
            occ.push(vec![false; ncols]);
        }
        let free = (r..r + rs).all(|rr| (c..c + cs).all(|cc| !occ[rr][cc]));
        if free {
            return (r, c);
        }
        c += 1;
        if c + cs > ncols {
            c = 0;
            r += 1;
        }
    }
}

/// The first row where column `col` is free for `rs` rows (content-track
/// placement for named items), growing the occupancy grid as needed.
fn find_free_in_col(occ: &mut Vec<Vec<bool>>, ncols: usize, col: usize, rs: usize) -> usize {
    let col = col.min(ncols.saturating_sub(1));
    let mut r = 0;
    loop {
        while occ.len() < r + rs {
            occ.push(vec![false; ncols]);
        }
        if (r..r + rs).all(|rr| !occ[rr][col]) {
            return r;
        }
        r += 1;
    }
}

/// The pixel width spanned by columns `[c0, c0+cs)` including internal gaps.
fn span_extent(widths: &[i32], c0: usize, cs: usize, gap: i32) -> i32 {
    let end = (c0 + cs).min(widths.len());
    let w: i32 = widths[c0..end].iter().sum();
    w + gap * ((end - c0) as i32 - 1).max(0)
}

/// A grid track's fixed base (px floor) and flex weight (`fr`).
fn track_base_fr(track: Track) -> (i32, f32) {
    match track {
        Track::Px(p) => (p as i32, 0.0),
        Track::Fr(f) => (0, f.max(0.0)),
        Track::Auto => (0, 1.0),
        Track::MinMax(min, max) => match max {
            TrackMax::Px(p) => ((min as i32).max(p as i32), 0.0),
            TrackMax::Fr(f) => (min as i32, f.max(0.0)),
            TrackMax::Auto => (min as i32, 1.0),
        },
    }
}

/// Resolve explicit grid tracks to pixel widths: fixed bases first, then `fr`
/// tracks share the leftover (ADR-0038).
fn resolve_tracks(tracks: &[Track], avail: i32, gap: i32) -> Vec<i32> {
    let n = tracks.len();
    if n == 0 {
        return vec![avail.max(1)];
    }
    let total_gap = gap * (n as i32 - 1).max(0);
    let mut bases = Vec::with_capacity(n);
    let mut frs = Vec::with_capacity(n);
    for &t in tracks {
        let (b, f) = track_base_fr(t);
        bases.push(b);
        frs.push(f);
    }
    let leftover = (avail - bases.iter().sum::<i32>() - total_gap).max(0) as f32;
    let fr_sum: f32 = frs.iter().sum();
    (0..n)
        .map(|i| {
            let extra = if fr_sum > 0.0 {
                leftover * frs[i] / fr_sum
            } else {
                0.0
            };
            (bases[i] as f32 + extra).round().max(1.0) as i32
        })
        .collect()
}

/// Resolve a grid container's column widths: a `repeat(auto-fill, …)` derives the
/// column count from the container width; otherwise the explicit template
/// (or one full-width column) is resolved (ADR-0038).
fn resolve_grid_columns(style: &ComputedStyle, avail: i32, gap: i32) -> Vec<i32> {
    if let Some(track) = style.grid_auto_fill {
        let (base, _) = track_base_fr(track);
        // A non-positive minimum (`minmax(0, 1fr)`) can't determine a count, so a
        // single full-width track is the only sane fit — otherwise the count is
        // floor((avail+gap)/(min+gap)), bounded so a tiny min can't explode into
        // hundreds of 1px columns (which collapses content to per-character).
        if base < 8 {
            return resolve_tracks(&[track], avail, gap);
        }
        let ncols = (((avail + gap) / (base + gap)).max(1) as usize).min(64);
        let tracks = vec![track; ncols];
        return resolve_tracks(&tracks, avail, gap);
    }
    // Named-line (full-bleed) templates collapse to one full-width column so
    // content stacks readably rather than landing in a gutter track (ADR-0038).
    if style.grid_cols_named || style.grid_template_columns.is_empty() {
        return vec![avail.max(1)];
    }
    resolve_tracks(&style.grid_template_columns, avail, gap)
}

/// The explicit pixel height of grid row `r` (`grid-template-rows` then
/// `grid-auto-rows`), or 0 to mean "size to content" (ADR-0038).
fn explicit_row_height(style: &ComputedStyle, r: usize) -> i32 {
    let track = style
        .grid_template_rows
        .get(r)
        .copied()
        .or(style.grid_auto_rows);
    match track {
        Some(Track::Px(p)) => p as i32,
        Some(Track::MinMax(min, TrackMax::Px(p))) => (min as i32).max(p as i32),
        Some(Track::MinMax(min, _)) => min as i32,
        // fr/auto rows are content-sized (the container height is indefinite).
        _ => 0,
    }
}

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

/// The marker text for a `list-item`, per `list-style-type`: a bullet glyph, or
/// the `1.`-style decimal ordinal for an `<ol>` (the parent's child loop set the
/// ordinal). `none` yields no marker (and the caller skips the gap too). Ordinals
/// floor at 1 so a stray zero can't produce `0.`.
fn list_marker(kind: ListStyleType, ordinal: u32) -> Option<String> {
    Some(match kind {
        ListStyleType::None => return None,
        ListStyleType::Decimal => format!("{}.", ordinal.max(1)),
        ListStyleType::Circle => "\u{25E6}".to_string(), // ◦
        ListStyleType::Square => "\u{25AA}".to_string(), // ▪
        ListStyleType::Disc => "\u{2022}".to_string(),   // •
    })
}

fn space_width(shaper: &dyn TextShaper, px: u32) -> u32 {
    // Delegates to the shaper's `space_advance`, which real shapers implement
    // without the per-call `Vec` allocation `shape(" ", …)` would incur — this
    // runs once per word in the inline loop.
    shaper.space_advance(px)
}

/// Parse an `<img width/height>` attribute (a bare number or `Npx`).
fn parse_dim(v: &str) -> Option<u32> {
    v.trim().trim_end_matches("px").trim().parse().ok()
}

/// Resolve a decoded `<img>`'s box from its `width`/`height` presentation
/// attributes and its intrinsic (decoded) size, per the CSS replaced-element
/// sizing rule.
///
/// The subtlety: when the author gives only ONE axis, the other must be derived
/// from the *intrinsic aspect ratio*, not carried over from the intrinsic pixel
/// size of that axis independently — otherwise a 400×300 image with `width="200"`
/// renders 200×300 (distorted) instead of the correct 200×150 (issue #34). Both
/// axes authored honors both exactly (an intentional override may break the
/// ratio); neither authored keeps the intrinsic size. A degenerate intrinsic
/// ratio (either dimension zero) falls back to the intrinsic axis, matching the
/// pre-fix behavior. The derived axis is computed in `f64` so large decoded
/// dimensions cannot overflow a `u32` multiply, and floored at 1 to match the
/// caller's `h.max(1)` convention.
fn replaced_size(attr_w: Option<u32>, attr_h: Option<u32>, intrinsic: Size) -> (u32, u32) {
    let aw = attr_w.filter(|v| *v > 0);
    let ah = attr_h.filter(|v| *v > 0);
    let ratio_ok = intrinsic.w > 0 && intrinsic.h > 0;
    let derive = |known: u32, num: u32, den: u32| {
        (known as f64 * num as f64 / den as f64).round().max(1.0) as u32
    };
    match (aw, ah) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) if ratio_ok => (w, derive(w, intrinsic.h, intrinsic.w)),
        (None, Some(h)) if ratio_ok => (derive(h, intrinsic.w, intrinsic.h), h),
        (Some(w), None) => (w, intrinsic.h),
        (None, Some(h)) => (intrinsic.w, h),
        (None, None) => (intrinsic.w, intrinsic.h),
    }
}

/// Choose the URL to fetch and draw for an `<img>` (ADR-0046): the explicit
/// `data-src` lazy alias wins; otherwise the best `srcset` candidate (honoring
/// `sizes`); otherwise plain `src`. Both the fetch-time collector and layout call
/// this with the same viewport width, so they agree on which candidate to use.
pub fn pick_img_url<'a>(attr: impl Fn(&str) -> Option<&'a str>, viewport_w: u32) -> Option<String> {
    if let Some(d) = attr("data-src") {
        return Some(d.to_string());
    }
    if let Some(ss) = attr("data-srcset").or_else(|| attr("srcset")) {
        if let Some(u) = select_srcset(ss, attr("sizes"), viewport_w) {
            return Some(u);
        }
    }
    attr("src").map(str::to_string)
}

/// Tokenize a `srcset` attribute value into `(url, descriptor)` candidates per the
/// WHATWG "parse a srcset attribute" algorithm. A bare `,` only separates candidates
/// when it terminates the URL or descriptor, not when it appears inside a URL (a
/// query string or a `data:` URI may legitimately contain commas).
fn srcset_candidates(input: &str) -> Vec<(&str, Option<&str>)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let is_ws = |c: u8| c.is_ascii_whitespace();
    while i < bytes.len() {
        while i < bytes.len() && (is_ws(bytes[i]) || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let url_start = i;
        while i < bytes.len() && !is_ws(bytes[i]) {
            i += 1;
        }
        let mut url = &input[url_start..i];
        // A URL ending in one or more commas has them stripped, and the comma(s)
        // end the candidate with no descriptor.
        let trimmed = url.trim_end_matches(',');
        if trimmed.len() != url.len() {
            url = trimmed;
            if !url.is_empty() {
                out.push((url, None));
            }
            continue;
        }
        while i < bytes.len() && is_ws(bytes[i]) {
            i += 1;
        }
        // Collect the descriptor up to the next top-level comma (depth 0), so a
        // parenthesized descriptor component isn't split early.
        let desc_start = i;
        let mut depth = 0i32;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth = (depth - 1).max(0),
                b',' if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        // Only the first descriptor token is meaningful to the selection logic
        // below (a width/density pair on one candidate is not something we act on).
        let descriptor = input[desc_start..i].split_whitespace().next();
        out.push((url, descriptor));
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }
    out
}

/// Pick one URL from an `srcset` candidate list (ADR-0046). Width (`w`) candidates
/// pick the smallest whose width covers the `sizes`-resolved target (bandwidth-
/// first, at device-pixel-ratio 1); density (`x`/bare) candidates pick `1x` (we
/// render at 1x). `None` for an empty/unparseable list (caller falls back to `src`).
pub fn select_srcset(srcset: &str, sizes: Option<&str>, viewport_w: u32) -> Option<String> {
    let mut width: Vec<(u32, &str)> = Vec::new();
    let mut density: Vec<(f32, &str)> = Vec::new();
    for (url, descriptor) in srcset_candidates(srcset) {
        match descriptor {
            Some(d) if d.ends_with('w') => {
                if let Ok(w) = d[..d.len() - 1].parse::<u32>() {
                    width.push((w, url));
                }
            }
            Some(d) if d.ends_with('x') => {
                if let Ok(x) = d[..d.len() - 1].parse::<f32>() {
                    density.push((x, url));
                }
            }
            Some(_) => continue,              // unknown descriptor
            None => density.push((1.0, url)), // a bare candidate is 1x
        }
    }
    if !width.is_empty() {
        let target = resolve_sizes(sizes, viewport_w);
        width.sort_by_key(|(w, _)| *w);
        // Smallest candidate that covers the target, else the largest available.
        let chosen = width
            .iter()
            .find(|(w, _)| *w >= target)
            .or_else(|| width.last())?;
        return Some(chosen.1.to_string());
    }
    if !density.is_empty() {
        density.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Smallest density >= 1 (1x preferred), else the largest.
        let chosen = density
            .iter()
            .find(|(x, _)| *x >= 1.0)
            .or_else(|| density.last())?;
        return Some(chosen.1.to_string());
    }
    None
}

/// Resolve a `sizes` attribute to a target width in CSS px. The first entry whose
/// media condition matches the viewport wins; a trailing entry with no condition is
/// the default. Absent/unparseable → the full viewport width (`100vw`).
fn resolve_sizes(sizes: Option<&str>, viewport_w: u32) -> u32 {
    let Some(sizes) = sizes else {
        return viewport_w;
    };
    for entry in sizes.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // The length is the final whitespace token; anything before it is the
        // (optional) media condition.
        let (cond, len) = match entry.rfind(char::is_whitespace) {
            Some(i) => (entry[..i].trim(), entry[i..].trim()),
            None => ("", entry),
        };
        if cond.is_empty() || sizes_media_matches(cond, viewport_w) {
            if let Some(px) = resolve_sizes_len(len, viewport_w) {
                return px;
            }
        }
    }
    viewport_w
}

/// Resolve one `sizes` length token (`px`/`vw`/`%`) to CSS px against the viewport.
/// `em`/`calc()`/etc. return `None` so the caller falls back.
fn resolve_sizes_len(len: &str, viewport_w: u32) -> Option<u32> {
    let len = len.trim();
    let vwf = viewport_w as f32;
    if let Some(n) = len.strip_suffix("px") {
        n.trim().parse::<f32>().ok().map(|v| v.max(0.0) as u32)
    } else if let Some(n) = len.strip_suffix("vw") {
        n.trim()
            .parse::<f32>()
            .ok()
            .map(|v| (v / 100.0 * vwf).max(0.0) as u32)
    } else if let Some(n) = len.strip_suffix('%') {
        n.trim()
            .parse::<f32>()
            .ok()
            .map(|v| (v / 100.0 * vwf).max(0.0) as u32)
    } else {
        None
    }
}

/// Match a `sizes` media condition (`(max-width: Npx)` / `(min-width: Npx)`, joined
/// by `and`) against the viewport width. Unrecognized conditions don't match.
fn sizes_media_matches(cond: &str, viewport_w: u32) -> bool {
    let mut matched_any = false;
    for clause in cond.split("and") {
        let c = clause.trim().trim_start_matches('(').trim_end_matches(')');
        let Some((k, v)) = c.split_once(':') else {
            continue; // e.g. a bare `screen` media type — ignore
        };
        let Some(px) = resolve_sizes_len(v.trim(), viewport_w) else {
            return false;
        };
        matched_any = true;
        match k.trim() {
            "max-width" if viewport_w > px => return false,
            "min-width" if viewport_w < px => return false,
            "max-width" | "min-width" => {}
            _ => return false,
        }
    }
    matched_any
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
    use cerberus_types::{ImageFit, ImagePos};

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

    /// Wraps `MonoShaper` but counts how many times a bare `" "` is shaped, and
    /// serves the inter-word gap through the allocation-free `space_advance`
    /// override — so a regression that re-introduces per-word `shape(" ")` in the
    /// inline loop is caught (issue #27).
    struct SpaceCountingShaper {
        space_shapes: std::sync::atomic::AtomicUsize,
    }
    impl TextShaper for SpaceCountingShaper {
        fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
            if text == " " {
                self.space_shapes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            MonoShaper.shape(text, px)
        }
        fn space_advance(&self, px: u32) -> u32 {
            // Matches MonoShaper's space advance without allocating a Vec.
            px.max(2) / 2
        }
    }

    #[test]
    fn inter_word_gaps_do_not_shape_a_space_per_word() {
        let shaper = SpaceCountingShaper {
            space_shapes: std::sync::atomic::AtomicUsize::new(0),
        };
        let styled = CssEngine::new().style(&parse_html("<p>one two three four five six</p>"));
        let _ = BlockLayout::default().layout(
            &styled,
            Size::new(800, 2000),
            &shaper,
            &NoImages,
            &NoForms,
        );
        assert_eq!(
            shaper
                .space_shapes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "inter-word spacing must route through space_advance, not shape(\" \")"
        );
    }

    struct OneImage(Arc<DecodedImage>);
    impl ImageProvider for OneImage {
        fn get(&self, _src: &str) -> Option<Arc<DecodedImage>> {
            Some(self.0.clone())
        }
    }

    /// Returns the image only for one exact URL — so an emitted `Image` proves the
    /// srcset selection resolved to that key.
    struct KeyedImage(&'static str, Arc<DecodedImage>);
    impl ImageProvider for KeyedImage {
        fn get(&self, src: &str) -> Option<Arc<DecodedImage>> {
            (src == self.0).then(|| self.1.clone())
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

    // ---- Grid v2: minmax / auto-fill / spans (ADR-0038) ----

    #[test]
    fn grid_auto_fill_minmax_derives_column_count_from_width() {
        // avail 784 with minmax(200px, 1fr) auto-fill -> 3 columns; the 4th item
        // wraps to a second row.
        let laid = lay(
            "<div style='display:grid;grid-template-columns:repeat(auto-fill, minmax(200px, 1fr))'>\
             <div style='background:#ff0000'>A</div><div style='background:#00ff00'>B</div>\
             <div style='background:#0000ff'>C</div><div style='background:#ffff00'>D</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 4);
        let xs: Vec<i32> = r.iter().map(|rc| rc.x).collect();
        assert_eq!(distinct(&xs), 3, "three columns at 800px wide");
        let ys: Vec<i32> = r.iter().map(|rc| rc.y).collect();
        assert!(distinct(&ys) >= 2, "the 4th item wraps to a second row");
        for rc in &r {
            assert!(
                (rc.w as i32 - 261).abs() <= 4,
                "column width ~261: {}",
                rc.w
            );
        }
    }

    #[test]
    fn grid_item_column_span_widens_the_cell() {
        // 4 equal columns (~196 each); the first item spans 2 (~392).
        let laid = lay(
            "<div style='display:grid;grid-template-columns:repeat(4, 1fr)'>\
             <div style='background:#ff0000;grid-column:span 2'>A</div>\
             <div style='background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].w as i32 - 392).abs() <= 6,
            "span-2 cell ~392: {}",
            r[0].w
        );
        assert!(
            (r[1].w as i32 - 196).abs() <= 4,
            "single cell ~196: {}",
            r[1].w
        );
        // B is placed in the third column (after the 2-col span), not overlapping.
        assert!(r[1].x >= r[0].x + 380, "B starts after the spanned cell");
    }

    #[test]
    fn absolute_flex_child_does_not_shrink_its_sibling() {
        // An absolutely-positioned flex child is out of flex flow; it must not
        // consume basis and shrink the in-flow sibling (ADR-0038).
        let laid = lay(
            "<div style='display:flex'>\
             <div style='position:absolute'>a very long absolute overlay heading that is wide</div>\
             <div style='flex:1;background:#ff0000'>hi</div></div>",
            400,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 1, "only the in-flow item paints a background");
        assert!(
            (r[0].w as i32 - 384).abs() <= 4,
            "flex:1 item fills the row despite the absolute sibling: {}",
            r[0].w
        );
    }

    #[test]
    fn nested_centered_flex_is_measured_by_content_not_probe_width() {
        // A flex item that is itself a centered flex column must be measured by its
        // content width; otherwise the probe-width centering inflates it and it
        // shrinks to per-word min-content (the Apple/MDN hero bug — ADR-0038).
        let laid = lay(
            "<div style='display:flex'>\
             <div style='display:flex;flex-direction:column;align-items:center;background:#ff0000'>\
               <div>Item One Heading</div></div>\
             <div style='display:flex;flex-direction:column;align-items:center;background:#00ff00'>\
               <div>Item Two Heading</div></div></div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        // Each measured to its content (well above a single-word min-content), laid
        // side by side rather than collapsed into a narrow column.
        assert!(r[0].w >= 70, "first item sized to content: {}", r[0].w);
        assert!(
            r[1].x >= r[0].x + 70,
            "items are side by side, not collapsed"
        );
    }

    #[test]
    fn max_width_with_auto_margins_centers_block() {
        // A 200px max-width block with margin:auto centers in the content area.
        let laid = lay(
            "<div style='max-width:200px;margin:0 auto;background:#ff0000'>hi</div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 1);
        assert!(
            (r[0].w as i32 - 200).abs() <= 2,
            "constrained to 200: {}",
            r[0].w
        );
        assert!(r[0].x > 100, "centered, not flush left: {}", r[0].x);
    }

    #[test]
    fn float_left_blocks_sit_side_by_side() {
        // Two float:left 50% columns share a row (the Bootstrap grid pattern).
        let laid = lay(
            "<div><div style='float:left;width:50%;background:#ff0000'>A</div>\
             <div style='float:left;width:50%;background:#00ff00'>B</div></div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].w as i32 - 292).abs() <= 4,
            "first column ~50%: {}",
            r[0].w
        );
        assert!(
            r[1].x > r[0].x + 200,
            "second float beside the first: {}",
            r[1].x
        );
        assert!((r[0].y - r[1].y).abs() <= 2, "floats share a row");
    }

    #[test]
    fn float_right_packs_from_the_right_edge() {
        let laid = lay(
            "<div><div style='float:right;width:100px;background:#ff0000'>R</div>\
             <div style='float:left;width:100px;background:#00ff00'>L</div></div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(r[0].x <= 12, "left float at the left edge: {}", r[0].x);
        assert!(r[1].x >= 480, "right float at the right edge: {}", r[1].x);
    }

    #[test]
    fn clear_drops_below_floats() {
        // A cleared block starts below the float row, not beside it.
        let laid = lay(
            "<div><div style='float:left;width:50%;background:#ff0000'>A</div>\
             <div style='clear:both;background:#0000ff'>below</div></div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        // The cleared block is full-width and below the float.
        assert!(
            r[1].y >= r[0].y + r[0].h as i32,
            "cleared block is below the float"
        );
        assert!(
            r[1].w as i32 > 400,
            "cleared block spans the full width: {}",
            r[1].w
        );
    }

    #[test]
    fn padding_insets_content_and_enlarges_box() {
        // Padding grows the background box and insets the text (ADR-0040).
        let plain = lay("<div style='background:#ff0000'>hi</div>", 400);
        let padded = lay("<div style='background:#ff0000;padding:20px'>hi</div>", 400);
        let (pr, dr) = (fill_rects(&plain), fill_rects(&padded));
        assert!(dr[0].h > pr[0].h + 30, "vertical padding grows the box");
        let min_plain = *glyph_xs(&plain).iter().min().unwrap();
        let min_pad = *glyph_xs(&padded).iter().min().unwrap();
        assert!(min_pad >= min_plain + 15, "text inset by left padding");
    }

    #[test]
    fn border_paints_four_edges() {
        let laid = lay("<div style='border:2px solid #000000'>hi</div>", 400);
        let rects = laid
            .display
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Rect { .. }))
            .count();
        assert!(rects >= 4, "border paints 4 edge rects, got {rects}");
    }

    #[test]
    fn box_sizing_border_box_includes_padding() {
        // border-box: the 200px width includes 20px padding each side (box=200);
        // content-box: width adds the padding (box=240).
        let bb = lay(
            "<div style='box-sizing:border-box;width:200px;padding:20px;background:#ff0000'>x</div>",
            600,
        );
        let cb = lay(
            "<div style='width:200px;padding:20px;background:#ff0000'>x</div>",
            600,
        );
        assert!(
            (fill_rects(&bb)[0].w as i32 - 200).abs() <= 2,
            "border-box width includes padding: {}",
            fill_rects(&bb)[0].w
        );
        assert!(
            (fill_rects(&cb)[0].w as i32 - 240).abs() <= 2,
            "content-box adds padding to width: {}",
            fill_rects(&cb)[0].w
        );
    }

    #[test]
    fn gradient_radius_shadow_emit_paint_items() {
        let laid = lay(
            "<div style='background:linear-gradient(#ff0000,#0000ff);border-radius:10px;\
             box-shadow:0 4px 8px rgba(0,0,0,0.3)'>hi</div>",
            400,
        );
        let items = &laid.display.items;
        assert!(
            items
                .iter()
                .any(|i| matches!(i, DisplayItem::Gradient { .. })),
            "gradient item"
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, DisplayItem::Shadow { .. })),
            "shadow item"
        );
    }

    #[test]
    fn rounded_border_emits_outer_round_rect() {
        let laid = lay(
            "<div style='border:2px solid #000000;border-radius:6px'>hi</div>",
            400,
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::RoundRect { .. })),
            "rounded border paints an outer round rect"
        );
    }

    #[test]
    fn overflow_hidden_emits_clip_markers() {
        let laid = lay(
            "<div style='overflow:hidden;height:40px'>a<br>b<br>c<br>d<br>e<br>f</div>",
            400,
        );
        let items = &laid.display.items;
        assert!(
            items
                .iter()
                .any(|i| matches!(i, DisplayItem::ClipPush { .. })),
            "overflow:hidden pushes a clip"
        );
        assert!(
            items.iter().any(|i| matches!(i, DisplayItem::ClipPop)),
            "and pops it"
        );
    }

    #[test]
    fn inline_block_gets_box_model_and_flows_inline() {
        // Two inline-block "buttons" with padding sit side by side, each
        // shrink-to-fit (content + padding), not full width (ADR-0042).
        let laid = lay(
            "<div><span style='display:inline-block;padding:10px;background:#ff0000'>A</span>\
             <span style='display:inline-block;padding:10px;background:#00ff00'>B</span></div>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2, "two inline-block backgrounds");
        assert!(r[0].w < 200, "shrink-to-fit, not full width: {}", r[0].w);
        assert!(r[0].w >= 20, "includes horizontal padding: {}", r[0].w);
        assert!((r[0].y - r[1].y).abs() <= 2, "flow on one line");
        assert!(r[1].x > r[0].x + 10, "second box follows the first");
    }

    #[test]
    fn absolute_resolves_against_positioned_ancestor() {
        // An absolute child of a relative parent lands at the PARENT's origin
        // (below the 100px spacer, indented by its margin), not the viewport's.
        let laid = lay(
            "<div style='height:100px'>spacer</div>\
             <div style='position:relative;margin-left:50px'>\
               <div style='position:absolute;top:0;left:0;background:#ff0000'>X</div>\
               parent</div>",
            400,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 1, "the absolute child's background");
        assert!(
            r[0].y >= 90,
            "resolves to the parent's top, not viewport: {}",
            r[0].y
        );
        assert!(
            r[0].x >= 50,
            "resolves to the parent's left (margin-left:50): {}",
            r[0].x
        );
    }

    #[test]
    fn min_height_and_vh_size_the_box() {
        let tall = lay(
            "<div style='background:#ff0000;min-height:200px'>hi</div>",
            400,
        );
        assert!(
            fill_rects(&tall)[0].h >= 200,
            "min-height extends: {}",
            fill_rects(&tall)[0].h
        );
        let plain = lay("<div style='background:#ff0000'>hi</div>", 400);
        assert!(
            fill_rects(&plain)[0].h < 60,
            "unconstrained block stays content-sized"
        );
        let vh = lay("<div style='background:#ff0000;height:50vh'>x</div>", 400);
        assert!(
            (fill_rects(&vh)[0].h as i32 - 1000).abs() <= 4,
            "50vh of 2000 ~ 1000: {}",
            fill_rects(&vh)[0].h
        );
    }

    #[test]
    fn viewport_units_resolve_inside_nested_contexts() {
        // Regression for #58: `vh`/`vw` used to collapse to 0 inside every nested
        // formatting context because `Ctx::sub` seeded vw:0/vh:0. With the real
        // viewport (2000 tall) propagated, a `height:50vh` element laid inside a
        // flex item, a grid cell, and a table cell now resolves to ~1000 (half the
        // viewport) instead of collapsing to its content height.
        let has_1000_tall = |laid: &LaidOut| {
            fill_rects(laid)
                .iter()
                .any(|r| (r.h as i32 - 1000).abs() <= 8)
        };

        let flex = lay(
            "<div style='display:flex'>\
               <div style='background:#00ff00;height:50vh'>x</div></div>",
            400,
        );
        assert!(has_1000_tall(&flex), "50vh resolves inside a flex item");

        let grid = lay(
            "<div style='display:grid'>\
               <div style='background:#00ff00;height:50vh'>x</div></div>",
            400,
        );
        assert!(has_1000_tall(&grid), "50vh resolves inside a grid cell");

        let table = lay(
            "<table><tr><td>\
               <div style='background:#00ff00;height:50vh'>x</div></td></tr></table>",
            400,
        );
        assert!(has_1000_tall(&table), "50vh resolves inside a table cell");
    }

    #[test]
    fn flex_min_height_centers_content_vertically() {
        let laid = lay(
            "<div style='display:flex;align-items:center;min-height:400px'><div>hi</div></div>",
            400,
        );
        let min_y = *glyph_ys(&laid).iter().min().unwrap();
        assert!(min_y > 100, "content centered within the tall box: {min_y}");
    }

    #[test]
    fn line_height_controls_row_pitch() {
        let normal = lay("<p>one</p><p>two</p>", 400);
        let tall = lay(
            "<p style='line-height:48px'>one</p><p style='line-height:48px'>two</p>",
            400,
        );
        assert!(
            *glyph_ys(&tall).iter().max().unwrap() > *glyph_ys(&normal).iter().max().unwrap(),
            "larger line-height increases row pitch"
        );
    }

    #[test]
    fn letter_spacing_widens_a_run() {
        let normal = lay(
            "<div style='display:flex'><div style='background:#ff0000'>iiiii</div></div>",
            800,
        );
        let spaced = lay(
            "<div style='display:flex'><div style='background:#ff0000;letter-spacing:8px'>iiiii</div></div>",
            800,
        );
        let (nw, sw) = (fill_rects(&normal)[0].w, fill_rects(&spaced)[0].w);
        assert!(sw > nw + 20, "letter-spacing widens the run: {sw} vs {nw}");
    }

    #[test]
    fn word_spacing_widens_inter_word_gaps() {
        // Four words → three inter-word gaps; word-spacing:30px adds 30 to each,
        // so the shrink-to-fit run grows by ~90px. Single-word runs are unaffected
        // by word-spacing (no gaps), unlike letter-spacing.
        let normal = lay(
            "<div style='display:flex'><div style='background:#ff0000'>a b c d</div></div>",
            800,
        );
        let spaced = lay(
            "<div style='display:flex'><div style='background:#ff0000;word-spacing:30px'>a b c d</div></div>",
            800,
        );
        let (nw, sw) = (fill_rects(&normal)[0].w, fill_rects(&spaced)[0].w);
        assert!(
            sw > nw + 60,
            "word-spacing widens the inter-word gaps: {sw} vs {nw}"
        );
    }

    #[test]
    fn list_marker_reflects_type_and_ordinal() {
        assert_eq!(
            list_marker(ListStyleType::Disc, 3).as_deref(),
            Some("\u{2022}")
        );
        assert_eq!(
            list_marker(ListStyleType::Circle, 3).as_deref(),
            Some("\u{25E6}")
        );
        assert_eq!(
            list_marker(ListStyleType::Square, 3).as_deref(),
            Some("\u{25AA}")
        );
        assert_eq!(list_marker(ListStyleType::None, 3), None);
        // Ordered items number by ordinal; a 0 floors to 1.
        assert_eq!(
            list_marker(ListStyleType::Decimal, 1).as_deref(),
            Some("1.")
        );
        assert_eq!(
            list_marker(ListStyleType::Decimal, 42).as_deref(),
            Some("42.")
        );
        assert_eq!(
            list_marker(ListStyleType::Decimal, 0).as_deref(),
            Some("1.")
        );
    }

    #[test]
    fn ordered_list_numbers_items_distinctly_from_bullets() {
        // A <ul> emits a single bullet glyph per item; an <ol> emits a decimal
        // marker ("1." = 2 glyphs, …), so the ordered list produces strictly more
        // marker glyphs across three items — proof it isn't rendering bullets.
        let glyph_count = |laid: &LaidOut| {
            laid.display
                .items
                .iter()
                .filter_map(|i| match i {
                    DisplayItem::Glyphs { glyphs, .. } => Some(glyphs.len()),
                    _ => None,
                })
                .sum::<usize>()
        };
        let ul = lay("<ul><li>a</li><li>a</li><li>a</li></ul>", 800);
        let ol = lay("<ol><li>a</li><li>a</li><li>a</li></ol>", 800);
        assert!(
            glyph_count(&ol) > glyph_count(&ul),
            "ordered markers (1. 2. 3.) add more glyphs than bullets: ol={} ul={}",
            glyph_count(&ol),
            glyph_count(&ul)
        );
    }

    #[test]
    fn capitalize_words_uppercases_each_word() {
        assert_eq!(capitalize_words("hello world foo"), "Hello World Foo");
        assert_eq!(capitalize_words("  spaced  out "), "  Spaced  Out ");
    }

    #[test]
    fn grid_minmax_floor_is_respected() {
        // minmax(300px,1fr) + 1fr at 800: col0 floored at 300 then shares; both fr.
        let laid = lay(
            "<div style='display:grid;grid-template-columns:minmax(300px,1fr) 1fr'>\
             <div style='background:#ff0000'>A</div><div style='background:#00ff00'>B</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 2);
        assert!(r[0].w >= 300, "minmax floor honored: {}", r[0].w);
        // col0 = 300 + half of (784-300); col1 = half of (784-300).
        assert!(r[0].w as i32 > r[1].w as i32, "floored track is wider");
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
    fn object_fit_and_background_size_reach_the_image_item() {
        // `<img object-fit:cover>` tags its Image item Cover; a block's
        // `background-size:contain` tags its background Image Contain (ADR-0044).
        let fit_of = |html: &str| {
            let styled = CssEngine::new().style(&parse_html(html));
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
            laid.display.items.iter().find_map(|i| match i {
                DisplayItem::Image { fit, .. } => Some(*fit),
                _ => None,
            })
        };
        assert_eq!(
            fit_of("<img src='pic.png' style='object-fit:cover'>"),
            Some(ImageFit::Cover)
        );
        assert_eq!(
            fit_of("<div style='background-image:url(bg.png); background-size:contain'>x</div>"),
            Some(ImageFit::Contain)
        );
        // Default (no property) stays Fill.
        assert_eq!(
            fit_of("<img src='pic.png'>"),
            Some(ImageFit::Fill),
            "default object-fit is Fill (stretch)"
        );
    }

    #[test]
    fn object_and_background_position_reach_the_image_item() {
        let pos_of = |html: &str| {
            let styled = CssEngine::new().style(&parse_html(html));
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
            laid.display.items.iter().find_map(|i| match i {
                DisplayItem::Image { pos, .. } => Some(*pos),
                _ => None,
            })
        };
        assert_eq!(
            pos_of("<img src='pic.png' style='object-position:right'>"),
            Some(ImagePos { x: 1.0, y: 0.5 })
        );
        // `<position>/<size>` in the background shorthand reaches the bg image.
        assert_eq!(
            pos_of("<div style='background:url(bg.png) left top / cover'>x</div>"),
            Some(ImagePos { x: 0.0, y: 0.0 })
        );
        // `<img>` default object-position is center.
        assert_eq!(pos_of("<img src='pic.png'>"), Some(ImagePos::CENTER));
    }

    #[test]
    fn srcset_density_and_width_selection() {
        // Density: 1x preferred (we render at 1x); bare candidate counts as 1x.
        assert_eq!(
            select_srcset("a.png 1x, b.png 2x", None, 1000).as_deref(),
            Some("a.png")
        );
        assert_eq!(
            select_srcset("a.png, b.png 2x", None, 1000).as_deref(),
            Some("a.png")
        );
        // All densities > 1 → the smallest.
        assert_eq!(
            select_srcset("b.png 2x, c.png 3x", None, 1000).as_deref(),
            Some("b.png")
        );
        // Width: smallest candidate covering the sizes target.
        let ss = "s.png 480w, m.png 800w, l.png 1200w";
        let sizes = Some("(max-width: 600px) 480px, 1000px");
        assert_eq!(
            select_srcset(ss, sizes, 500).as_deref(),
            Some("s.png"),
            "narrow viewport matches the 480px branch"
        );
        assert_eq!(
            select_srcset(ss, sizes, 1000).as_deref(),
            Some("l.png"),
            "wide viewport uses the 1000px default → 1200w"
        );
        // No sizes → 100vw default.
        assert_eq!(
            select_srcset("s.png 480w, l.png 1200w", None, 400).as_deref(),
            Some("s.png")
        );
        // None covers the target → the largest available.
        assert_eq!(
            select_srcset("s.png 480w", None, 2000).as_deref(),
            Some("s.png")
        );
        assert_eq!(select_srcset("", None, 800), None);
    }

    #[test]
    fn srcset_commas_inside_urls_do_not_shear_candidates() {
        // A query-string comma must not be mistaken for a candidate separator.
        assert_eq!(
            select_srcset("a.jpg?x=1,2 480w, b.jpg 800w", None, 1000).as_deref(),
            Some("b.jpg"),
            "the 800w candidate wins, not a sheared a.jpg?x=1"
        );
        // A `data:` URI's embedded commas stay part of one candidate.
        assert_eq!(
            select_srcset("data:image/png;base64,AAA,BBB 1x, b.png 2x", None, 1000).as_deref(),
            Some("data:image/png;base64,AAA,BBB")
        );
    }

    #[test]
    fn pick_img_url_precedence() {
        let pick = |pairs: &[(&'static str, &'static str)], vw: u32| {
            pick_img_url(|n| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| *v), vw)
        };
        // data-src wins outright.
        assert_eq!(
            pick(&[("data-src", "lazy.png"), ("srcset", "a.png 2x")], 800).as_deref(),
            Some("lazy.png")
        );
        // srcset chosen over plain src.
        assert_eq!(
            pick(&[("srcset", "a.png 1x, b.png 2x"), ("src", "x.png")], 800).as_deref(),
            Some("a.png")
        );
        // Plain src is the fallback.
        assert_eq!(
            pick(&[("src", "only.png")], 800).as_deref(),
            Some("only.png")
        );
        assert_eq!(pick(&[], 800), None);
    }

    #[test]
    fn img_srcset_resolves_to_the_selected_url() {
        // Laid out at viewport 800 with sizes:400px → target 400 → 480w "small.png".
        // The provider only serves "small.png", so an Image item proves selection.
        let styled = CssEngine::new().style(&parse_html(
            "<img src='fallback.png' srcset='small.png 480w, big.png 1200w' sizes='400px'>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(800, 2000),
            &MonoShaper,
            &KeyedImage("small.png", img),
            &NoForms,
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "srcset selected small.png (the only key the provider serves)"
        );
    }

    /// Lay out an `<img>` backed by a decoded image of `intrinsic` size and return
    /// the emitted `Image` rect (w, h). Used by the replaced-sizing tests below.
    fn img_box(html: &str, intrinsic: Size, container_w: u32) -> (u32, u32) {
        let styled = CssEngine::new().style(&parse_html(html));
        let img = Arc::new(DecodedImage {
            size: intrinsic,
            rgba: vec![255; (intrinsic.area() as usize) * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(container_w, 2000),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
        );
        laid.display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Image { rect, .. } => Some((rect.w, rect.h)),
                _ => None,
            })
            .expect("decoded image emitted")
    }

    #[test]
    fn img_width_only_derives_height_from_intrinsic_ratio() {
        // 400×300 with width=200 → height must follow the ratio (150), not the
        // intrinsic 300 (issue #34).
        assert_eq!(
            img_box("<img src='p.png' width='200'>", Size::new(400, 300), 800),
            (200, 150)
        );
    }

    #[test]
    fn img_height_only_derives_width_from_intrinsic_ratio() {
        // 400×300 with height=150 → width follows the ratio (200), not intrinsic 400.
        assert_eq!(
            img_box("<img src='p.png' height='150'>", Size::new(400, 300), 800),
            (200, 150)
        );
    }

    #[test]
    fn img_both_attrs_are_honored_even_against_the_ratio() {
        // Both authored → both honored exactly, even when they contradict the
        // intrinsic 4:3 ratio (an intentional distortion).
        assert_eq!(
            img_box(
                "<img src='p.png' width='200' height='80'>",
                Size::new(400, 300),
                800
            ),
            (200, 80)
        );
    }

    #[test]
    fn img_no_size_attrs_uses_intrinsic_size() {
        assert_eq!(
            img_box("<img src='p.png'>", Size::new(400, 300), 800),
            (400, 300)
        );
    }

    #[test]
    fn img_single_axis_ratio_still_clamps_on_container_overflow() {
        // width=600 on a 400×300 image derives height 450; a container whose content
        // area is 500px then clamps width→500 and scales height proportionally
        // (450 * 500/600 = 375). Proves the overflow clamp runs AFTER the ratio
        // derivation. Content area = container - 2*8px page margin, so 516 → 500.
        assert_eq!(
            img_box("<img src='p.png' width='600'>", Size::new(400, 300), 516),
            (500, 375)
        );
    }

    #[test]
    fn replaced_size_covers_the_four_cases() {
        let intr = Size::new(400, 300);
        // Both present → honored verbatim.
        assert_eq!(replaced_size(Some(200), Some(80), intr), (200, 80));
        // Width only → height from ratio.
        assert_eq!(replaced_size(Some(200), None, intr), (200, 150));
        // Height only → width from ratio.
        assert_eq!(replaced_size(None, Some(150), intr), (200, 150));
        // Neither → intrinsic.
        assert_eq!(replaced_size(None, None, intr), (400, 300));
        // Rounding: a 3:2 image (300×200) with width=100 → h = round(100*200/300)=67.
        assert_eq!(
            replaced_size(Some(100), None, Size::new(300, 200)),
            (100, 67)
        );
        // Degenerate intrinsic ratio → fall back to the intrinsic axis.
        assert_eq!(
            replaced_size(Some(200), None, Size::new(0, 300)),
            (200, 300)
        );
    }

    #[test]
    fn background_image_paints_behind_block_content() {
        // `background-image: url(...)` on a block paints a stretched image item
        // (ADR-0038), supplied by the provider keyed by the url.
        let styled = CssEngine::new().style(&parse_html(
            "<div style='background-image:url(bg.png)'>hello</div>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(8, 8),
            rgba: vec![200; 8 * 8 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(400, 2000),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
        );
        let bg = laid
            .display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("background image emitted");
        // The background fills the block width (here the full content area).
        assert!(bg.w >= 300, "bg image stretched to the box: {}", bg.w);
        assert_eq!(bg.x, 8, "bg starts at the page margin");
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
    fn white_space_nowrap_stays_on_one_line() {
        // In a narrow container, normal text wraps to several lines; the same text
        // with `white-space: nowrap` stays on a single line.
        let wrapped = lay("<p>aaaa bbbb cccc dddd eeee</p>", 60);
        let nowrap = lay(
            "<p style='white-space:nowrap'>aaaa bbbb cccc dddd eeee</p>",
            60,
        );
        assert!(
            distinct(&glyph_ys(&wrapped)) > 1,
            "normal text wraps to multiple lines"
        );
        assert_eq!(
            distinct(&glyph_ys(&nowrap)),
            1,
            "nowrap text stays on one line"
        );
    }

    #[test]
    fn line_through_draws_a_strike_rule() {
        // Plain text emits no rects; a line-through run adds a 1px rule (like an
        // underline, but through the middle of the text).
        let plain = lay("<p>struck</p>", 400);
        let strike = lay("<p style='text-decoration:line-through'>struck</p>", 400);
        assert_eq!(rect_count(&plain), 0, "plain text draws no rule");
        assert_eq!(rect_count(&strike), 1, "line-through draws one strike rule");
        // The strike sits above the baseline (underline would be at y+px).
        let (strike_y, px) = strike
            .display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Rect { rect, .. } => Some((rect.y, 16)),
                _ => None,
            })
            .unwrap();
        let (text_y, _) = strike
            .display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some((origin.y, ())),
                _ => None,
            })
            .unwrap();
        assert!(
            strike_y < text_y + px,
            "strike ({strike_y}) sits above the underline position"
        );
    }

    #[test]
    fn control_box_honors_author_background_and_border() {
        // A plain button uses the UA chrome; a styled one takes the author's
        // background, and its border color once it has a border (#66).
        let plain = lay("<button>Go</button>", 400);
        assert!(
            has_rect_color(&plain, BUTTON_BG),
            "UA button fill by default"
        );
        assert!(
            has_rect_color(&plain, CONTROL_BORDER),
            "UA border by default"
        );

        let styled = lay(
            "<button style='background:#00ff00;border:2px solid #0000ff'>Go</button>",
            400,
        );
        assert!(
            has_rect_color(&styled, Color::rgb(0, 0xff, 0)),
            "author background fills the control"
        );
        assert!(
            has_rect_color(&styled, Color::rgb(0, 0, 0xff)),
            "author border color drawn once the control has a border"
        );
        assert!(
            !has_rect_color(&styled, BUTTON_BG),
            "the UA grey no longer appears"
        );
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
    fn text_indent_offsets_only_the_first_line() {
        let min_x = |laid: &LaidOut| glyph_xy(laid).iter().map(|(x, _)| *x).min().unwrap();

        // Single line: the indented run starts exactly 40px further right.
        let plain = lay("<p>hello world</p>", 400);
        let indented = lay("<p style='text-indent:40px'>hello world</p>", 400);
        assert_eq!(
            min_x(&indented) - min_x(&plain),
            40,
            "the first line is indented by text-indent"
        );

        // Force a wrap: the first line (smallest y) is indented, later lines
        // reset to the left edge — text-indent is a first-line-only effect.
        let wrapped = lay(
            "<p style='text-indent:40px'>aaaa bbbb cccc dddd eeee ffff</p>",
            80,
        );
        let xy = glyph_xy(&wrapped);
        let first_y = xy.iter().map(|(_, y)| *y).min().unwrap();
        let first_line_x = xy
            .iter()
            .filter(|(_, y)| *y == first_y)
            .map(|(x, _)| *x)
            .min()
            .unwrap();
        let later_x = xy
            .iter()
            .filter(|(_, y)| *y != first_y)
            .map(|(x, _)| *x)
            .min()
            .expect("content wrapped to a second line");
        assert_eq!(
            first_line_x - later_x,
            40,
            "only the first line is indented; wrapped lines reset to the left"
        );
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

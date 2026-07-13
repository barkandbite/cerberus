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
    StyledChild, StyledDom, StyledNode, TextAlign, TextTransform, Track, TrackMax, VerticalAlign,
};
use cerberus_types::{Color, FontStyle, GenericFamily, Point, Rect, Size};
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

/// The result of flowing a run of inline-level content into a fixed-width box —
/// the output of [`flow_inline`], the leaf painter shared with the taffy engine.
/// All coordinates are absolute (the box's own origin), so the caller splices the
/// items straight into its display list.
#[derive(Clone, Debug, Default)]
pub struct InlineFlow {
    pub display: Vec<DisplayItem>,
    pub links: Vec<LinkBox>,
    pub fields: Vec<FormFieldBox>,
    pub elements: Vec<ElementBox>,
    /// Content width actually used (rightmost inline cursor − left edge).
    pub width: i32,
    /// Content height (final flow cursor − top edge), floored at one line.
    pub height: i32,
    /// The form-control id counter after flowing, so the caller continues the
    /// document-wide pre-order numbering across leaves.
    pub next_field_id: u32,
}

/// Flow a slice of styled children (text / inline / inline-block / images / form
/// controls / lists / tables) into a box of width `right − left` with its top-left
/// at `(left, top)`, reusing the block engine's inline machinery — line breaking,
/// shaping, `text-align`, atomic inline boxes, list markers, and replaced content.
///
/// This is the **inline formatting context leaf painter** the taffy engine uses
/// (`RENDERING_ARCHITECTURE_PLAN.md`, Stage 3): taffy owns the block/flex/grid box
/// geometry, and every inline run inside a box is measured and painted here, so the
/// existing shaping/inline flow stays the single source of truth below the box
/// level. Output is in absolute coordinates.
#[allow(clippy::too_many_arguments)]
pub fn flow_inline(
    children: &[StyledChild],
    text_style: &ComputedStyle,
    align: TextAlign,
    left: i32,
    right: i32,
    top: i32,
    shaper: &dyn TextShaper,
    images: &dyn ImageProvider,
    forms: &dyn FormState,
    field_id: u32,
    vw: i32,
    vh: i32,
) -> InlineFlow {
    let mut sub = Ctx::sub(left, right, top, shaper, images, forms, field_id, vw, vh);
    sub.line_align = align;
    for child in children {
        match child {
            // Bare text uses the containing element's inherited style (color, font,
            // size); the caller passes it as `text_style`. Nested inline elements
            // carry their own cascaded style through `walk`.
            StyledChild::Text(t) => sub.add_text(t, text_style, None),
            StyledChild::Element(e) => sub.walk(e, None),
        }
    }
    sub.flush_line();
    let width = (sub.max_x - left).max(0);
    let height = (sub.y - top).max(0);
    let next_field_id = sub.field_id;
    InlineFlow {
        display: sub.display.items,
        links: sub.links,
        fields: sub.fields,
        elements: sub.elements,
        width,
        height,
        next_field_id,
    }
}

/// Append a block box's decorations — drop shadow, background (color / gradient /
/// image), then border — for a border-box `rect`, in the same paint order as the
/// walker's own box painting. The taffy engine calls this for each element box
/// before flowing that box's content on top, so both engines decorate boxes
/// identically (`RENDERING_ARCHITECTURE_PLAN.md`, Stage 3). Content the caller
/// appends afterward therefore paints over the background, as in normal flow.
pub fn box_decorations(
    style: &ComputedStyle,
    rect: Rect,
    images: &dyn ImageProvider,
    out: &mut Vec<DisplayItem>,
) {
    if let Some(sh) = style.box_shadow.as_deref() {
        out.push(DisplayItem::Shadow {
            rect: Rect::new(rect.x + sh.dx, rect.y + sh.dy, rect.w, rect.h),
            blur: sh.blur.max(0) as u16,
            color: sh.color,
        });
    }
    let (bt, br, bb, bl) = (
        style.border_top.max(0),
        style.border_right.max(0),
        style.border_bottom.max(0),
        style.border_left.max(0),
    );
    let radius = style.border_radius;
    let has_border = bt > 0 || br > 0 || bb > 0 || bl > 0;
    // Background fill (gradient wins, else solid color), then background-image.
    let fill = |rect: Rect, radius: u16, out: &mut Vec<DisplayItem>| {
        if let Some(g) = style.background_gradient.as_deref() {
            out.push(DisplayItem::Gradient {
                rect,
                start: g.start,
                end: g.end,
                vertical: g.vertical,
                radius,
            });
        } else if let Some(color) = style.background {
            out.push(if radius > 0 {
                DisplayItem::RoundRect {
                    rect,
                    color,
                    radius,
                }
            } else {
                DisplayItem::Rect { rect, color }
            });
        }
        if let Some(url) = &style.background_image {
            if let Some(img) = images.get(url) {
                out.push(DisplayItem::Image {
                    rect,
                    image: img,
                    fit: style.background_size,
                    pos: style.background_position,
                    pos_px: style.background_position_px,
                });
            }
        }
    };
    if radius > 0 {
        if has_border {
            out.push(DisplayItem::RoundRect {
                rect,
                color: style.border_color,
                radius,
            });
        }
        let inner = Rect::new(
            rect.x + bl,
            rect.y + bt,
            (rect.w as i32 - bl - br).max(0) as u32,
            (rect.h as i32 - bt - bb).max(0) as u32,
        );
        let inner_r = (radius as i32 - bl.max(br).max(bt).max(bb)).max(0) as u16;
        fill(inner, inner_r, out);
    } else {
        fill(rect, 0, out);
        let col = style.border_color;
        let (l, t) = (rect.x, rect.y);
        let (w, h) = (rect.w as i32, rect.h as i32);
        if bt > 0 {
            out.push(DisplayItem::Rect {
                rect: Rect::new(l, t, w.max(0) as u32, bt as u32),
                color: col,
            });
        }
        if bb > 0 {
            out.push(DisplayItem::Rect {
                rect: Rect::new(l, t + h - bb, w.max(0) as u32, bb as u32),
                color: col,
            });
        }
        if bl > 0 {
            out.push(DisplayItem::Rect {
                rect: Rect::new(l, t, bl as u32, h.max(0) as u32),
                color: col,
            });
        }
        if br > 0 {
            out.push(DisplayItem::Rect {
                rect: Rect::new(l + w - br, t, br as u32, h.max(0) as u32),
                color: col,
            });
        }
    }
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

/// How an `<img>` is presented: as its decoded graphic, or as its text
/// alternative (alt/title/caption). Text-only saves memory, CPU, and network —
/// the bytes are never fetched or decoded — and is a per-image user option (the
/// app maps a global default plus per-image overrides onto the provider).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImageDisplayMode {
    /// Decode and paint the image (the browser default).
    #[default]
    Graphical,
    /// Render the image's text alternative instead of the graphic.
    TextOnly,
}

impl ImageDisplayMode {
    /// Parse a mode name (`graphical` / `text-only`, `text` accepted); unknown
    /// names fall back to `Graphical`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "text-only" | "text" | "textonly" | "caption" => ImageDisplayMode::TextOnly,
            _ => ImageDisplayMode::Graphical,
        }
    }

    /// Read the default from the `CERB_IMAGES` env var (default `Graphical`), so
    /// a session can run text-only without a rebuild.
    pub fn from_env() -> Self {
        std::env::var("CERB_IMAGES")
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// Supplies decoded images to layout, keyed by an element's `src`/`data-src`.
/// Resolution/fetching/decoding all happen inside the implementation.
pub trait ImageProvider {
    /// The decoded image for `src`, if available.
    fn get(&self, src: &str) -> Option<Arc<DecodedImage>>;

    /// Whether this image should render as its text alternative (alt/caption)
    /// instead of the graphic — the resource-saving text-only option. Default
    /// `false` (always graphical). When `true`, layout draws the text chip and
    /// never asks for the decoded bytes.
    fn render_as_text(&self, _src: &str) -> bool {
        false
    }
}

/// An image provider with nothing (placeholders / alt text only).
pub struct NoImages;

impl ImageProvider for NoImages {
    fn get(&self, _src: &str) -> Option<Arc<DecodedImage>> {
        None
    }
}

/// Which layout engine to use. `Block` is the current hand-rolled single-pass
/// walker; `Taffy` will be a spec-correct block/flex/grid box engine
/// (`RENDERING_ARCHITECTURE_PLAN.md`). During the strangler-fig migration both
/// are constructed via [`make_layout`] and A/B-compared on the parity corpus,
/// with `Block` the default until a page's `Taffy` RMSE is no worse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LayoutEngineKind {
    #[default]
    Block,
    Taffy,
}

impl LayoutEngineKind {
    /// Parse an engine name (`block` / `taffy`); unknown names fall back to
    /// `Block`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "taffy" => LayoutEngineKind::Taffy,
            _ => LayoutEngineKind::Block,
        }
    }

    /// Read the engine from the `CERB_LAYOUT` env var (default `Block`), so the
    /// corpus harness can A/B without a rebuild.
    pub fn from_env() -> Self {
        std::env::var("CERB_LAYOUT")
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// Construct the selected layout engine behind the [`LayoutEngine`] trait. Until
/// the taffy engine lands, `Taffy` aliases the walker so the selection seam and
/// A/B harness can be exercised with byte-identical output.
pub fn make_layout(kind: LayoutEngineKind) -> Box<dyn LayoutEngine> {
    match kind {
        LayoutEngineKind::Block | LayoutEngineKind::Taffy => Box::new(BlockLayout::default()),
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
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockLayout {
    /// Extra page margin in pixels. Defaults to 0: the page inset is the UA
    /// stylesheet's `body { margin: 8px }` (as in Chrome), so a page that sets
    /// `body{margin:0}` really reaches the viewport edge — a fixed engine inset
    /// shifted every such page by 8px on both axes.
    pub margin: i32,
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
        // Realize the last block's deferred bottom margin so the document height
        // still includes a trailing margin, as before collapsing was deferred.
        ctx.flush_vmargin();
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
    /// The current line's tallest *fractional* line-height. Chrome keeps used
    /// line-height fractional (16px Arial-metric `normal` is 18.398px, not 18)
    /// and rounds only where each line lands, so line N sits at
    /// `round(N × pitch)`. `newline` advances by this, carrying the sub-pixel
    /// remainder in `line_frac`; `line_h` (its rounding) still sizes boxes.
    line_hf: f32,
    /// Sub-pixel line-pitch debt carried across `newline`s (see `line_hf`).
    line_frac: f32,
    /// Sub-pixel inline-cursor debt within the current line. Inter-word gaps
    /// are fractional (a Liberation Sans space at 16px advances 4.453px, not
    /// 4); the flow carries the remainder across gaps and rounds per
    /// placement, and the wrap test compares the fractional total — otherwise
    /// a 20-space line runs ~9px narrow, fits one extra word, and flips the
    /// wrap point Chrome takes (shifting everything below). Reset at each
    /// line start; glyph runs already carry their own remainder internally.
    x_frac: f32,
    /// Flowing inside a table cell. Legacy `<center>` (`-webkit-center`)
    /// centering does NOT propagate across a cell boundary (measured on the
    /// reference: HN's `<center><table>…<td><table>` leaves the inner
    /// item-list table at the cell's LEFT edge, ~187px left of where
    /// re-centering would put it) — `table()` only honors an inherited
    /// WebkitCenter when this is false.
    in_cell: bool,
    /// A collapsible space from the SOURCE text is pending before the next
    /// word/run (issue #137). Whitespace state must cross inline-element
    /// boundaries: `<a>RFC 6761</a>, a` has NO space before the comma (the
    /// old x-position heuristic invented one → "6761 , a"), while
    /// `by <span nowrap>Public…` has a real one that the nowrap fast path
    /// dropped → "byPublic". Set by `add_text` from each text node's actual
    /// leading/inter-word/trailing whitespace; consumed by the next
    /// `add_word`/`add_run` placement.
    pending_space: bool,
    line: Vec<LinePiece>,
    line_align: TextAlign,
    /// Output-buffer lengths at the start of the current line, so `text-align`
    /// can shift the atomic inline boxes added mid-line (inline-blocks, form
    /// fields, buttons, inline images) — which go straight to these buffers
    /// rather than the buffered `line` of text pieces — by the same offset as the
    /// text. Without this, only text centered/right-aligned; boxes stayed left.
    line_disp0: usize,
    line_links0: usize,
    line_fields0: usize,
    line_elems0: usize,
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
    /// The `<source>` candidates of the `<picture>` currently being laid out, so
    /// its child `<img>` resolves its URL through them (type/media selection)
    /// rather than through its own `src`/`srcset` alone. Set while walking a
    /// `<picture>`, cleared immediately after its `<img>` — never leaks to a
    /// sibling image (ADR-0046: fetch and paint pick the same candidate).
    cur_picture: Option<Vec<OwnedPictureSource>>,
    /// The bottom margin of the most recently closed block, deferred so it can
    /// collapse with the NEXT block sibling's top margin (CSS 2.1 §8.3.1: the
    /// separation is max(positives)+min(negatives), not the sum — without this
    /// every `p+p` gap doubled and text pages drifted ever farther down).
    /// Realized as-is by `flush_vmargin` when non-block content follows.
    pending_vmargin: i32,
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
            line_hf: 0.0,
            line_frac: 0.0,
            x_frac: 0.0,
            in_cell: false,
            pending_space: false,
            line: Vec::new(),
            line_align: TextAlign::Left,
            line_disp0: 0,
            line_links0: 0,
            line_fields0: 0,
            line_elems0: 0,
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
            cur_picture: None,
            pending_vmargin: 0,
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
            line_hf: 0.0,
            line_frac: 0.0,
            x_frac: 0.0,
            in_cell: false,
            pending_space: false,
            line: Vec::new(),
            line_align: TextAlign::Left,
            line_disp0: 0,
            line_links0: 0,
            line_fields0: 0,
            line_elems0: 0,
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
            cur_picture: None,
            pending_vmargin: 0,
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
                self.line_break(style.font_size.max(1), style.font_family);
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
                    // A positioned <img> (e.g. Wikipedia's globe:
                    // position:absolute;top:158px inside the relative wordmark box)
                    // must resolve its insets like any positioned box, not lay in
                    // normal flow. Capture flow state, lay the replaced box, then
                    // translate/lift it against its containing block.
                    let base = self.positioned_base(style);
                    self.image(node, in_link);
                    if let Some(base) = base {
                        self.apply_positioning(style, base);
                    }
                }
                return;
            }
            // An inline `<svg>` the app pre-rasterized keeps its tag (so author
            // `svg{…}` tag selectors size/hide it) and carries a synthetic
            // `src` — treat it as the replaced image it now is. A RAW svg
            // subtree (no `src`; a path that skipped the rewrite) renders
            // nothing: its `<text>`/`<title>` must not leak as page text.
            "svg" => {
                if visible && node.attr("src").is_some() {
                    let base = self.positioned_base(style);
                    self.image(node, in_link);
                    if let Some(base) = base {
                        self.apply_positioning(style, base);
                    }
                }
                return;
            }
            "picture" => {
                // A <picture> with a direct <img> selects one URL from its
                // <source> children (by `type`/`media`) and renders that <img>
                // with it (WHATWG "select an image source"); its <source>/other
                // children paint nothing. With NO direct <img> (invalid, but
                // possible) fall through to normal container layout so any nested
                // content still renders — matching the fetch collector.
                if let Some(img) = node.children.iter().find_map(|c| match c {
                    StyledChild::Element(e) if e.tag == "img" => Some(e.as_ref()),
                    _ => None,
                }) {
                    // Honor the <img>'s OWN box-suppression, exactly as the bare
                    // "img" arm does: `display:none`, `visibility:hidden`, or an
                    // `opacity:0` (here or via the group) draws nothing.
                    let img_style = &img.style;
                    let img_hidden = self.opacity_hidden || img_style.opacity == 0.0;
                    let img_visible = !img_hidden
                        && img_style.visibility == cerberus_style::Visibility::Visible
                        && img_style.display != Display::None;
                    if visible && img_visible {
                        // Honor a block-level <picture> (e.g. `picture{display:block}`)
                        // by breaking the line around its image.
                        let block = style.display == Display::Block;
                        if block {
                            self.flush_line();
                        }
                        self.cur_picture = Some(collect_picture_sources(node));
                        let base = self.positioned_base(&img.style);
                        self.image(img, in_link);
                        if let Some(base) = base {
                            self.apply_positioning(&img.style, base);
                        }
                        self.cur_picture = None;
                        if block {
                            self.flush_line();
                        }
                    }
                    return;
                }
                // No direct <img>: fall through to the generic container path.
            }
            "input" => {
                if visible {
                    self.form_input(node);
                }
                return;
            }
            "button" => {
                // `as_block_once` means `form_button` is re-laying this button as a
                // content container (to render its icon/span children) — fall
                // through to block layout, which paints its box and walks children.
                // Otherwise handle it as a form control.
                if !self.as_block_once {
                    if visible {
                        self.form_button(node);
                    }
                    return;
                }
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
                // The element's in-flow origin — its container's content-left
                // (`self.left`), where its box is laid, NOT `self.left0` (an
                // ancestor reference). In a centered container these differ, and
                // the mismatch offset a left/top-anchored absolute box by exactly
                // (self.left − self.left0) — e.g. Wikipedia's `left:60%` language
                // columns landed far to the right.
                x: self.left,
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
            // An explicit `width` wins (CSS: a set width is the used width, even
            // out of flow — e.g. Wikipedia's `.central-featured-lang{width:15.6rem}`,
            // whose count text must not wrap). Otherwise: both insets set → the
            // stretched inset gap; else shrink-to-fit intrinsic content width.
            let used_w = if let Some(w) = style.width.resolve_vp(cb.w, self.vw, self.vh) {
                w.max(1)
            } else {
                match (
                    style.inset_left.resolve_vp(cb.w, self.vw, self.vh),
                    style.inset_right.resolve_vp(cb.w, self.vw, self.vh),
                ) {
                    (Some(l), Some(r)) => (cb.w - l - r).max(1),
                    _ => self.measure_intrinsic_width(node).clamp(1, cb.w.max(1)),
                }
            };
            let saved = self.right;
            // Extend the used-width box from the element's flow-start (`self.left`,
            // the current content-left of its container), NOT `self.left0` — the
            // latter is an ancestor reference and, inside a centered/indented
            // container (`margin:0 auto`), sits to the LEFT of `self.left`. With a
            // narrow container that makes `right < left`, collapsing avail to 1px
            // and wrapping the content (Wikipedia's language cells inside the
            // centered `.central-featured`). Was masked when the container was wide
            // enough to keep the (wrong) width positive.
            self.right = self.left + used_w;
            Some(saved)
        } else {
            None
        };

        // Flex/grid containers lay their items out and return; everything else
        // falls through to block/inline flow.
        match style.display {
            Display::Flex => {
                // `href` (not the raw in_link): an <a> wrapping the container
                // keeps its links clickable inside the items — the common
                // brand-page card pattern (<a><div class=grid>headline…).
                self.flex_layout(node, href);
                self.cur_link_node = saved_link_node;
                return;
            }
            Display::Grid => {
                self.grid_layout(node, href);
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
        // `as_block_once` marks an inline-block (or table cell / float) laying its
        // own box into a sub that `add_inline_block`/`place_float` already sized
        // to its resolved width. Such an atom must FILL that sub, not re-resolve
        // its own `width` — a percentage width would otherwise apply twice (e.g.
        // Wikipedia's `.search-input{width:73%}` became 73% of 73%, collapsing
        // the search field's containing block). Not while measuring, where the
        // sub is a huge probe and the box must shrink to its content instead.
        let fills_sub = self.as_block_once && !self.measuring;
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
            self.line_align = style.text_align;
            let avail = (self.right - self.left).max(1);
            // Adjacent-sibling margin collapsing (CSS 2.1 §8.3.1): this block's
            // top margin and the previous block sibling's deferred bottom margin
            // join as max(positives) + min(negatives), not their sum.
            let mt = self.resolve_margin(style.margin_top, avail);
            let prev = std::mem::take(&mut self.pending_vmargin);
            let joined = prev.max(mt).max(0) + prev.min(mt).min(0);
            if style.border_top + style.padding_top == 0 {
                // Parent/first-child collapse-through (§8.3.1): with no top
                // border/padding, this box's top margin keeps collapsing with
                // its first block child's — so DEFER it (the border-box top
                // below projects the gap known so far; a deeper child margin
                // that enlarges it shifts content but not this box's painted
                // top, an accepted v1 approximation).
                self.pending_vmargin = joined;
            } else {
                self.y += joined;
            }
            let (pl, pr) = (style.padding_left, style.padding_right);
            let (bl, br) = (style.border_left, style.border_right);
            let h_extra = pl + pr + bl + br;
            // Border-box width from width/max-width (box-sizing aware); `margin:
            // auto` centers it, else margin-left offsets (ADR-0039/0040). An atom
            // filling its pre-sized sub takes the whole `avail` (its width and
            // margins were already resolved by the caller).
            let (box_left, box_w) = if fills_sub {
                (self.left, avail)
            } else {
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
                            self.resolve_margin(style.margin_left, avail)
                                .clamp(0, extra)
                        };
                        (self.left + off, bw)
                    }
                    None => {
                        // Auto width fills the space left after both margins: a
                        // non-auto `margin-right` shrinks the box from the right
                        // (an `auto` right margin computes to 0 here, and its value
                        // is 0, so this is a no-op for centering).
                        let l = self.left + self.resolve_margin(style.margin_left, avail);
                        let r = (self.right - self.resolve_margin(style.margin_right, avail))
                            .max(l + 1);
                        (l, (r - l).max(1))
                    }
                }
            };
            bbox = Some(BorderBox {
                left: box_left,
                right: box_left + box_w,
                // Projected top: any still-deferred margin sits ABOVE this box
                // (it collapses through), so the painted box starts below it.
                top: self.y + self.pending_vmargin,
                bt: style.border_top,
                br,
                bb: style.border_bottom,
                bl,
            });
            // A positioned block is the containing block for its descendants'
            // `absolute` (ADR-0042): push its (in-flow) border box; the whole
            // subtree is translated together if this element is later lifted.
            // Height: absolute children resolve %-based `top`/`bottom` against
            // THIS block's height, so an explicit `height` must be used (e.g.
            // Wikipedia's `.central-featured{height:32.5rem}` positions its
            // languages by `top:20%…80%`). Auto height is only known after
            // layout, so fall back to the viewport height there.
            if positioned {
                let cb_h = style
                    .height
                    .resolve_vp(self.vh, self.vw, self.vh)
                    .map(|h| h.max(1))
                    .unwrap_or(self.vh);
                self.cb_stack.push(ContainingBlock {
                    x: box_left,
                    y: self.y + self.pending_vmargin,
                    w: box_w,
                    h: cb_h,
                });
            }
            // Content box = border box inset by border + padding.
            self.left = box_left + bl + pl;
            self.right = (box_left + box_w - br - pr).max(self.left + 1);
            self.y += style.border_top + style.padding_top;
            self.x = self.left;
            // This block's first line starts here — track its atomic boxes for
            // text-align (a fresh block may not have gone through commit_line).
            self.mark_line_start();
            if visible && style.display == Display::ListItem {
                if let Some(m) = list_marker(style.list_style_type, self.list_ordinal) {
                    self.add_run(&m, style, None);
                    self.x +=
                        space_width(self.shaper, style.font_size.max(1), style.font_family) as i32;
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
        // `<ol start="N">` seeds the count so the first item is N.
        let mut item_ordinal = ol_start_base(node);
        // Horizontal insets of an inline (non-replaced) box: its left
        // margin/border/padding push following content rightward at the box's
        // start, the right ones after its content. Block and inline-block boxes
        // model padding through their content box already; this is only for a
        // true inline element (e.g. a nav `<a>` with `padding:4px 6px`). Applied
        // on the current line — an inline box that soft-wraps is approximated
        // (the common styled-link/nav case does not wrap).
        let inline_box = !is_block && style.display == Display::Inline;
        let inline_cb = (self.right - self.left).max(1);
        if inline_box && visible {
            self.x += self.resolve_margin(style.margin_left, inline_cb)
                + style.border_left
                + style.padding_left;
            self.max_x = self.max_x.max(self.x);
        }
        for child in &node.children {
            match child {
                StyledChild::Text(t) => {
                    // A whitespace-only node (source indentation between
                    // elements) is not content: it only arms the
                    // inter-element space (#137). It must NOT close the float
                    // band — the newline between a float-left logo and a
                    // float-right nav would otherwise push the nav below the
                    // logo (iana's header/footer stacked exactly that way).
                    if t.trim().is_empty() {
                        if visible && !t.is_empty() {
                            self.pending_space = true;
                        }
                    } else {
                        self.flush_floats(&mut fb);
                        if visible {
                            self.add_text(t, style, href);
                        }
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
                        // `<li value="N">` sets this item's number to N; following
                        // items continue from it. Otherwise just advance by one.
                        item_ordinal = match e.attr("value").and_then(|v| v.trim().parse().ok()) {
                            Some(v) => v,
                            None => item_ordinal + 1,
                        };
                        self.list_ordinal = item_ordinal;
                    }
                    self.walk(e, href);
                }
            }
        }
        self.flush_floats(&mut fb);
        if inline_box && visible {
            self.x += style.padding_right
                + style.border_right
                + self.resolve_margin(style.margin_right, inline_cb);
            self.max_x = self.max_x.max(self.x);
        }
        self.opacity_hidden = saved_opacity_hidden;

        if is_block {
            self.flush_line();
            // A bottom margin still pending from our LAST block child is
            // contained by our bottom padding/border (it adds to our height);
            // with neither, it escapes and collapses with our own bottom margin
            // instead (CSS 2.1 §8.3.1 parent/last-child), contributing nothing
            // to this box's auto height.
            let escaped = if style.padding_bottom + style.border_bottom > 0 {
                self.flush_vmargin();
                0
            } else {
                std::mem::take(&mut self.pending_vmargin)
            };
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
            // Defer our bottom margin (joined with any escaped last-child
            // margin) so it can collapse with the next sibling's top margin.
            let mb = self.resolve_margin(style.margin_bottom, (saved_right0 - saved_left).max(1));
            self.pending_vmargin = escaped.max(mb).max(0) + escaped.min(mb).min(0);
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

    /// Capture the flow state before laying a `relative`/`absolute`/`fixed`
    /// element, so [`apply_positioning`](Self::apply_positioning) can translate or
    /// lift exactly its output. `None` for in-flow elements. Used by the replaced
    /// (`<img>`) path, which otherwise laid positioned images in normal flow and
    /// ignored their insets.
    fn positioned_base(&self, style: &ComputedStyle) -> Option<PosBase> {
        use cerberus_style::Position;
        let positioned = self.pos_enabled
            && matches!(
                style.position,
                Position::Relative | Position::Absolute | Position::Fixed
            );
        if positioned {
            Some(PosBase {
                disp: self.display.items.len(),
                links: self.links.len(),
                fields: self.fields.len(),
                elements: self.elements.len(),
                y: self.y,
                // The element's in-flow origin (`self.left`), not the ancestor
                // reference `self.left0` — see the block-path capture above.
                x: self.left,
            })
        } else {
            None
        }
    }

    /// Translate a `relative` element in place, or lift an `absolute`/`fixed`
    /// element out of flow into a paint-on-top layer (ADR-0034).
    fn apply_positioning(&mut self, style: &ComputedStyle, base: PosBase) {
        use cerberus_style::Position;
        let cb = self.containing_block(style.position);
        // The element's own border-box width, from its content box (`self.left`),
        // NOT `self.left0` (an ancestor reference that, in a centered container,
        // sits to the left). Used for right/bottom inset anchoring.
        let elem_w = (self.right - self.left).max(0);
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
        let mut glyphs = self.shaper.shape_with(text, px, style.font_family);
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
        let ws = style.white_space;
        // Collapsible whitespace state crosses element boundaries (#137):
        // leading whitespace arms `pending_space` for the first word, inter-word
        // whitespace re-arms it between words, and after the node it reflects
        // the node's TRAILING whitespace — so `provided by <a>…` keeps its real
        // space and `<a>RFC 6761</a>,` gets none. A whitespace-only node
        // (`</a> <a>`) arms the flag without placing anything.
        let starts_ws = text.starts_with(|c: char| c.is_ascii_whitespace());
        let ends_ws = text.ends_with(|c: char| c.is_ascii_whitespace());
        if ws.preserves_newlines() {
            // `pre`/`pre-wrap`/`pre-line`: an explicit `\n` is a hard break. Each
            // resulting line then either preserves its spaces (and maybe wraps) or
            // collapses them, per the keyword.
            let mut first = true;
            for line in text.split('\n') {
                if !first {
                    self.line_break(style.font_size.max(1), style.font_family);
                }
                first = false;
                if line.is_empty() {
                    continue;
                }
                if ws.preserves_spaces() {
                    if ws.wraps() {
                        // `pre-wrap`: keep the exact spaces but break between words
                        // when the line overflows.
                        self.add_pre_wrap_line(line, style, href);
                    } else {
                        // `pre`: one atomic run, spaces intact, never wrapping.
                        self.add_run(line, style, href);
                    }
                } else {
                    // `pre-line`: spaces collapse, but the surrounding `\n`s (above)
                    // are preserved and the collapsed words still wrap.
                    for word in line.split_ascii_whitespace() {
                        self.add_word(word, style, href);
                        self.pending_space = true;
                    }
                }
            }
            // Preserved-space content manages its own spacing literally.
            self.pending_space = false;
        } else if !ws.wraps() {
            // `white-space: nowrap`: collapse runs of whitespace to single spaces
            // like normal text, but place the whole thing as one atomic run so it
            // never wraps (it may overflow the container, per spec).
            if starts_ws {
                self.pending_space = true;
            }
            let collapsed = text.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
            if !collapsed.is_empty() {
                self.add_run(&collapsed, style, href);
                self.pending_space = ends_ws;
            } else if !text.is_empty() {
                self.pending_space = true;
            }
        } else {
            // Split on ASCII whitespace only, so a non-breaking space (`&nbsp;`,
            // U+00A0) keeps the words it joins on the same line rather than
            // becoming a wrap opportunity.
            if starts_ws {
                self.pending_space = true;
            }
            let mut placed_any = false;
            for word in text.split_ascii_whitespace() {
                self.add_word(word, style, href);
                self.pending_space = true;
                placed_any = true;
            }
            if placed_any {
                self.pending_space = ends_ws;
            }
        }
    }

    /// Lay one `white-space: pre-wrap` line: preserve every space run as literal
    /// advance (spaces draw no glyph, so width is `count × space_advance`) while
    /// still allowing a soft break between words on overflow. A break consumes the
    /// space run that would have preceded the wrapped word (it hangs at the old
    /// line's end); leading spaces on a line are kept as indentation. Tabs are
    /// counted as one space each — a small simplification, noted here.
    fn add_pre_wrap_line(&mut self, line: &str, style: &ComputedStyle, href: Option<&str>) {
        let px = style.font_size.max(1);
        let sw = space_width_f(self.shaper, px, style.font_family);
        // Accumulated leading-whitespace width for the next word (fractional —
        // a preserved space run's width is `count × exact advance`; see
        // `x_frac`).
        let mut lead = 0.0f32;
        let mut chars = line.chars().peekable();
        while let Some(&c) = chars.peek() {
            // Only ASCII whitespace is a break opportunity / collapsible gap; a
            // non-breaking space stays inside the word so it never wraps.
            if c.is_ascii_whitespace() {
                lead += sw;
                chars.next();
                continue;
            }
            // Gather the next word (run up to the next ASCII space).
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
            let (glyphs, w) = self.shape_run(&word, px, style);
            let at_line_start = self.x == self.left;
            if !at_line_start && self.x as f32 + self.x_frac + lead + w as f32 > self.right as f32 {
                // Wrap: the pending space run hangs at the end of the old line.
                self.newline();
            } else {
                self.advance_x_f(lead);
            }
            lead = 0.0;
            self.push_piece(px, w, glyphs, style, href);
        }
        // Trailing spaces hang past the last word (kept for width fidelity).
        if lead > 0.0 {
            self.advance_x_f(lead);
            self.max_x = self.max_x.max(self.x);
        }
    }

    fn add_word(&mut self, word: &str, style: &ComputedStyle, href: Option<&str>) {
        self.flush_vmargin();
        let px = style.font_size.max(1);
        let (glyphs, w) = self.shape_run(word, px, style);
        let at_line_start = self.x == self.left;
        // The leading offset before this word: at the start of a line it is the
        // one-shot `text-indent` (usually 0), otherwise the inter-word space
        // (widened/trimmed by `word-spacing`, clamped so a large negative can't
        // reverse the cursor). A negative `text-indent` is honored (it is the
        // classic image-replacement trick — `text-indent:-9999px` pushes the
        // fallback text off-screen so only the background sprite shows).
        // The gap is FRACTIONAL (`x_frac` carries the sub-pixel remainder) and
        // the wrap test compares the fractional total, matching how Chrome
        // accumulates advances across the line.
        // A gap only where the SOURCE text had whitespace (`pending_space`,
        // #137): `<a>RFC 6761</a>,` has no space before the comma, and the old
        // x-position heuristic invented one ("6761 , a"), misrendering and
        // flipping wrap points.
        let gap_f = if at_line_start {
            std::mem::take(&mut self.pending_indent) as f32
        } else if self.pending_space {
            (space_width_f(self.shaper, px, style.font_family) + style.word_spacing as f32).max(0.0)
        } else {
            0.0
        };
        self.pending_space = false;
        if !at_line_start && self.x as f32 + self.x_frac + gap_f + w as f32 > self.right as f32 {
            self.newline();
        } else {
            self.advance_x_f(gap_f);
        }
        self.push_piece(px, w, glyphs, style, href);
    }

    /// Advance the inline cursor by a fractional width: `x` gets the rounded
    /// position and `x_frac` keeps the sub-pixel remainder for the next gap.
    fn advance_x_f(&mut self, w: f32) {
        let t = self.x as f32 + self.x_frac + w;
        let xi = t.round();
        self.x = xi as i32;
        self.x_frac = t - xi;
    }

    fn add_run(&mut self, text: &str, style: &ComputedStyle, href: Option<&str>) {
        self.flush_vmargin();
        let px = style.font_size.max(1);
        let (glyphs, w) = self.shape_run(text, px, style);
        // Consume this block's one-shot `text-indent` at the start of a line, as
        // `add_word` does. A `white-space: nowrap` run (`.pure-button` sets it,
        // and it inherits) is placed atomically through here, so without this an
        // indent is dropped — notably the `text-indent:-9999px` image-replacement
        // trick that hides a button's fallback label behind its sprite icon.
        if self.x == self.left {
            self.x += std::mem::take(&mut self.pending_indent);
        } else if self.pending_space {
            // A real space precedes this run in the source (`by <span
            // nowrap>Public…` — the nowrap fast path used to eat it: #137).
            self.advance_x_f(
                (space_width_f(self.shaper, px, style.font_family) + style.word_spacing as f32)
                    .max(0.0),
            );
        }
        self.pending_space = false;
        // `text-overflow: ellipsis`: when this clipped, non-wrapping run would
        // overflow the box, drop the tail glyphs and append `…` so the visible
        // text ends within the box.
        if style.text_overflow_ellipsis && style.overflow_clip {
            let avail = self.right - self.x;
            if avail > 0 && w as i32 > avail {
                let (ell, ell_w) = self.shape_run("\u{2026}", px, style);
                let target = (avail - ell_w as i32).max(0);
                let mut acc = 0i32;
                let mut kept: Vec<GlyphBox> = Vec::with_capacity(glyphs.len());
                for g in glyphs {
                    if acc + g.advance as i32 > target {
                        break;
                    }
                    acc += g.advance as i32;
                    kept.push(g);
                }
                kept.extend(ell);
                let total = (acc + ell_w as i32).max(0) as u32;
                self.push_piece(px, total, kept, style, href);
                return;
            }
        }
        self.push_piece(px, w, glyphs, style, href);
    }

    /// Resolve a margin `Len` to px against the containing-block width `cb_w`
    /// (CSS resolves `%` margins — on every side — against the container's
    /// width). `auto` and other non-resolving values yield 0; horizontal `auto`
    /// for centering is handled separately via the `margin_*_auto` flags.
    fn resolve_margin(&self, len: Len, cb_w: i32) -> i32 {
        len.resolve_vp(cb_w, self.vw, self.vh).unwrap_or(0)
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
        let floor = min_border_box_width(&e.style, avail, h_extra, self.vw, self.vh);
        let w = resolve_border_box_width(&e.style, avail, h_extra, self.vw, self.vh)
            .unwrap_or_else(|| self.measure_intrinsic_width(e).min(avail))
            .max(floor)
            .clamp(1, avail);
        // Inline-block margins flow along the line: the left margin offsets the
        // box, the right margin advances the cursor for the next atom — and a
        // negative right margin pulls it back to overlap (Wikipedia's search box
        // uses `.search-input{margin-right:-6.6rem}` to seat the Search button
        // against the input's right edge). Auto margins compute to 0 in inline
        // flow (no centering).
        let ml = if e.style.margin_left_auto {
            0
        } else {
            self.resolve_margin(e.style.margin_left, avail)
        };
        let mr = if e.style.margin_right_auto {
            0
        } else {
            self.resolve_margin(e.style.margin_right, avail)
        };
        if self.x != self.left && self.x + ml + w > self.right {
            self.newline();
        }
        self.x += ml;
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
        // Enable out-of-flow positioning inside a real (non-probe) inline-block:
        // a `position:relative` inline-block is the containing block for its
        // `absolute` descendants (e.g. Wikipedia's `.styled-select` language
        // dropdown pinned to the right edge of the relatively-positioned
        // `.search-input`). `finish_positioned` below folds those layers onto the
        // sub's content before it merges up. Measurement probes stay flat.
        sub.pos_enabled = !self.measuring;
        sub.as_block_once = true; // lay `e` with the block box model, filling [x, x+w]
        sub.walk(e, in_link);
        sub.flush_line();
        sub.finish_positioned();
        self.field_id = sub.field_id;
        let h = (sub.y - self.y).max(1);
        self.merge_sub(sub, 0, 0);
        self.x += w;
        // Content extent is the box's right edge; a trailing margin (empty space,
        // or a negative pull-back) does not extend it.
        self.max_x = self.max_x.max(self.x);
        self.x += mr;
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
        // `vertical-align: sub`/`super` shifts the (already smaller) inline box
        // off the shared line top. Pieces are top-aligned, so raising `super`
        // (negative offset) lifts it toward the cap of the surrounding text and
        // lowering `sub` drops it toward the baseline — a fraction of the piece's
        // own font size, the usual crude-but-legible offset.
        let voffset = match style.vertical_align {
            VerticalAlign::Super => -(px as i32 / 3),
            VerticalAlign::Sub => px as i32 / 3,
            VerticalAlign::Baseline | VerticalAlign::OffBaseline => 0,
        };
        self.line.push(LinePiece {
            x: self.x,
            y: self.y + voffset,
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
        // Resolve `line-height` against this piece's own font size, so a unitless
        // factor inherited from an ancestor scales to this element (not the
        // ancestor's font size). `Normal` uses the face's real vertical metrics
        // via the shaper (~1.15× for the Times/Arial-metric faces, ~1.17× for
        // Roboto) — the flat 1.2 drifted a pixel every couple of lines.
        let lhf = style
            .line_height
            .resolve_f(px, self.shaper.natural_leading_f(px, style.font_family));
        self.line_hf = self.line_hf.max(lhf);
        self.line_h = self.line_h.max(lhf.round() as i32);
    }

    /// Lay out an `<img>`: draw the decoded image if ready, else a sized
    /// placeholder, else the alt text. Lazy-loading is ignored (raw render).
    fn image(&mut self, node: &StyledNode, in_link: Option<&str>) {
        // Resolve srcset/sizes/data-src to one URL (ADR-0046), using the same
        // viewport width the fetch-time collector used, so the lookup hits. Inside
        // a <picture>, resolve through its <source> candidates first (type/media),
        // falling back to this <img>'s own src.
        let vw = self.vw.max(0) as u32;
        let vh = self.vh.max(0) as u32;
        let picked = match &self.cur_picture {
            Some(sources) => {
                let borrowed: Vec<PictureSource<'_>> =
                    sources.iter().map(OwnedPictureSource::borrow).collect();
                pick_picture_url(&borrowed, |n| node.attr(n), vw, vh)
            }
            None => pick_img_url(|n| node.attr(n), vw),
        };
        let Some(src) = picked else {
            self.image_alt(node, in_link);
            return;
        };
        let src = src.as_str();
        // Text-only option: render the image's text alternative instead of the
        // graphic (its bytes were never fetched). Checked before any decoded-byte
        // lookup so it needs none.
        if self.images.render_as_text(src) {
            self.image_text_chip(node, in_link);
            return;
        }
        let attr_w = node.attr("width").and_then(parse_dim);
        let attr_h = node.attr("height").and_then(parse_dim);
        // CSS `width`/`height` (px, %, vw/vh) override the presentational
        // width/height *attributes* on a replaced element; whichever is set wins,
        // and a missing dimension is derived from the intrinsic aspect ratio by
        // `replaced_size`. Without this, a stylesheet rule like `img{width:60px}`
        // was ignored and every image rendered at full natural size, overflowing
        // its intended box.
        let cb_w = (self.right - self.left).max(1);
        let css_w = node
            .style
            .width
            .resolve_vp(cb_w, self.vw, self.vh)
            .map(|v| v.max(0) as u32);
        let css_h = node
            .style
            .height
            .resolve_vp(cb_w, self.vw, self.vh)
            .map(|v| v.max(0) as u32);
        let spec_w = css_w.or(attr_w);
        let spec_h = css_h.or(attr_h);

        if let Some(image) = self.images.get(src) {
            let (mut w, mut h) = replaced_size(spec_w, spec_h, image.size);
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
                pos_px: Point::ZERO,
            });
            self.link_image_box(rect, in_link);
            // The line box reserves the strut descent below a baseline-aligned
            // image; the painted rect stays `h`.
            let below = self.strut_descent_below(&node.style);
            self.advance_box(w, h.saturating_add(below.max(0) as u32));
        } else if let (Some(w), Some(h)) = (spec_w, spec_h) {
            // Not decoded yet: reserve the declared box so layout doesn't reflow.
            self.place_box(w, h.max(1));
            let rect = Rect::new(self.x, self.y, w, h.max(1));
            self.display.push(DisplayItem::Rect {
                rect,
                color: Color::rgb(0xDD, 0xDD, 0xDD),
            });
            self.link_image_box(rect, in_link);
            let below = self.strut_descent_below(&node.style);
            self.advance_box(w, h.saturating_add(below.max(0) as u32));
        } else {
            self.image_alt(node, in_link);
        }
    }

    /// How far the line box extends BELOW a baseline-aligned inline replaced
    /// box: the image's bottom edge sits on the text baseline, so the strut's
    /// descent plus the below-baseline share of the leading is reserved under
    /// it (the classic "gap below image"). Measured against Chrome: a 72px
    /// image in a 16px-Arial div makes a 76px line box (descent 3 + gap-share
    /// 1), 82px with `line-height:30px` (descent 3 + 7 of the 13px leading —
    /// the below share is the leading minus the floored above share). Zero for
    /// `display:block` images (no line box) and for `vertical-align:
    /// top/middle/bottom` (off the baseline — Chrome reports exactly 72px).
    fn strut_descent_below(&self, style: &ComputedStyle) -> i32 {
        if style.vertical_align != VerticalAlign::Baseline
            || matches!(style.display, Display::Block | Display::ListItem)
        {
            return 0;
        }
        let px = style.font_size.max(1);
        let (a, d) = self.shaper.ascent_descent(px, style.font_family);
        let lh = style
            .line_height
            .resolve_f(px, self.shaper.natural_leading_f(px, style.font_family));
        let leading = lh - (a + d) as f32;
        let above = (leading / 2.0).floor();
        let below = leading - above;
        ((d as f32 + below).round() as i32).max(0)
    }

    /// An image inside an `<a>` is itself the click target (logo links, product
    /// cards): emit a link hit box over the image rect — text pieces are boxed
    /// at line commit, but a replaced box never flows through that path.
    fn link_image_box(&mut self, rect: Rect, in_link: Option<&str>) {
        if let Some(href) = in_link {
            if let Some(node) = self.cur_link_node {
                self.elements.push(ElementBox { rect, node });
            }
            self.links.push(LinkBox {
                rect,
                href: href.to_string(),
            });
        }
    }

    fn image_alt(&mut self, node: &StyledNode, in_link: Option<&str>) {
        if let Some(alt) = node.attr("alt").map(str::trim) {
            if !alt.is_empty() {
                self.add_text(&format!("[{alt}]"), &node.style, in_link);
            }
        }
    }

    /// Render the text-only substitute for an image: its `alt` caption, else its
    /// `title` tooltip, else a label derived from the file name — bracketed as
    /// inline text so it flows, wraps, aligns, and positions like the surrounding
    /// content. Always emits something (never vanishes on an empty `alt`), so the
    /// user sees what the suppressed image was.
    fn image_text_chip(&mut self, node: &StyledNode, in_link: Option<&str>) {
        let label = image_text_label(node, self.vw.max(0) as u32);
        self.add_text(&format!("[{label}]"), &node.style, in_link);
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
    ///
    /// A button that contains child *elements* (icon `<i>`s, styled `<span>`s —
    /// e.g. Wikipedia's "Read Wikipedia in your language" pill with a translate
    /// icon + chevron) is a content container: lay its subtree as an inline-block
    /// so those children render (and its CSS box/border paints), then record the
    /// Button hit box over the result. A text-only button keeps the simple
    /// labelled-box path.
    fn form_button(&mut self, node: &StyledNode) {
        let id = self.field_id;
        self.field_id += 1;
        let has_elem_children = node
            .children
            .iter()
            .any(|c| matches!(c, StyledChild::Element(_)));
        if has_elem_children {
            let (x0, y0) = (self.x, self.y);
            self.add_inline_block(node, None);
            let w = (self.x - x0).max(1) as u32;
            let h = self.line_h.max(1) as u32;
            // Registered after layout; the line's `text-align` shift (which moves
            // fields added this line) then centers this hit box with its content.
            self.fields.push(FormFieldBox {
                rect: Rect::new(x0, y0, w, h),
                id,
                kind: FieldKind::Button,
            });
            return;
        }
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
        // An explicit CSS `width` wins over the `size` attribute (CSS: a set width
        // is the used width — e.g. a search box with `size="20"` but `width:100%`
        // must fill its container, as Chrome renders it). Fall back to `size` cols.
        let cb_w = (self.right - self.left).max(1);
        if std::env::var("CERB_DBG_SI").is_ok() && node.attr("id") == Some("searchInput") {
            eprintln!(
                "DBG searchInput width={:?} cb_w={} left={} right={} resolved={:?}",
                node.style.width,
                cb_w,
                self.left,
                self.right,
                node.style.width.resolve_vp(cb_w, self.vw, self.vh)
            );
        }
        let w = match node.style.width.resolve_vp(cb_w, self.vw, self.vh) {
            Some(css_w) => self.fit_width(css_w.max(1)),
            None => {
                let cols = node.attr("size").and_then(parse_dim).unwrap_or(20).max(1);
                self.fit_width(cols as i32 * self.char_w(px) + 2 * FIELD_PAD)
            }
        };
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
    /// Realize a block bottom margin still pending from a previous sibling,
    /// as-is: non-block content (text, atoms, rules, floats, tables, flex/grid)
    /// has no top margin for it to collapse with.
    fn flush_vmargin(&mut self) {
        self.y += std::mem::take(&mut self.pending_vmargin);
    }

    fn place_box(&mut self, w: u32, _h: u32) {
        self.flush_vmargin();
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

    fn line_break(&mut self, px: u32, family: GenericFamily) {
        let lf = self.shaper.natural_leading_f(px, family);
        self.line_hf = self.line_hf.max(lf);
        self.line_h = self.line_h.max(lf.round() as i32);
        self.newline();
    }

    fn newline(&mut self) {
        self.commit_line();
        // Advance by the *fractional* line pitch, carrying the sub-pixel
        // remainder into the next line — so line N lands at `round(N × pitch)`
        // exactly as Chrome places it, instead of drifting by the per-line
        // rounding error (0.4px/line ≈ a full line's offset 40 lines down).
        // Atomic boxes (images, inline-blocks) only feed integer `line_h`, so
        // the max with `line_hf` keeps them exact.
        let adv_f = self.line_hf.max(self.line_h as f32).max(1.0) + self.line_frac;
        let adv = (adv_f.round() as i32).max(1);
        self.line_frac = (adv_f - adv as f32).clamp(-1.0, 1.0);
        self.y += adv;
        self.x = self.left;
        self.x_frac = 0.0;
        self.line_h = 0;
        self.line_hf = 0.0;
        // text-indent is a first-line-only effect; once we wrap it no longer
        // applies (it is normally already consumed by the first word).
        self.pending_indent = 0;
    }

    /// Record the output-buffer lengths at the start of a line, so `commit_line`
    /// can shift the atomic boxes added during it by the `text-align` offset.
    fn mark_line_start(&mut self) {
        self.line_disp0 = self.display.items.len();
        self.line_links0 = self.links.len();
        self.line_fields0 = self.fields.len();
        self.line_elems0 = self.elements.len();
        // A fresh line starts at an integer x; sub-pixel debt never crosses
        // lines (each line accumulates its own, as Chrome does).
        self.x_frac = 0.0;
    }

    /// Apply text-align to the buffered line, then emit it.
    fn commit_line(&mut self) {
        let used = self.x - self.left;
        let available = ((self.right - self.left) - used).max(0);
        let offset = match self.line_align {
            TextAlign::Left => 0,
            TextAlign::Center | TextAlign::WebkitCenter => available / 2,
            TextAlign::Right => available,
        };
        // Shift the atomic inline boxes added during this line (inline-blocks,
        // form fields, buttons, inline images) — they went straight to the output
        // buffers rather than the buffered text `line`, so `text-align` must move
        // them by the same offset. Done before the text pieces are appended so the
        // range covers only this line's boxes.
        if offset != 0 {
            for it in &mut self.display.items[self.line_disp0..] {
                translate_item(it, offset, 0);
            }
            for l in &mut self.links[self.line_links0..] {
                l.rect = offset_rect(l.rect, offset, 0);
            }
            for f in &mut self.fields[self.line_fields0..] {
                f.rect = offset_rect(f.rect, offset, 0);
            }
            for e in &mut self.elements[self.line_elems0..] {
                e.rect = offset_rect(e.rect, offset, 0);
            }
        }
        // Underline continuity (#137): a multi-word link underlines its
        // inter-word gaps too — Chrome rules the whole anchor, not word
        // islands. Extend each underlined piece's rule to the start of the
        // next piece when it continues the same link on the same baseline.
        let mut under_w: Vec<u32> = Vec::with_capacity(self.line.len());
        for i in 0..self.line.len() {
            let p = &self.line[i];
            let mut w = p.w;
            if p.underline && p.href.is_some() {
                if let Some(n) = self.line.get(i + 1) {
                    if n.underline && n.y == p.y && n.href == p.href {
                        w = w.max((n.x - p.x).max(0) as u32);
                    }
                }
            }
            under_w.push(w);
        }
        // Drain through a moved-out buffer so the line `Vec`'s capacity is kept
        // for the next line instead of being dropped each commit (`mem::take`
        // would leave a zero-capacity `Vec`).
        let mut line = std::mem::take(&mut self.line);
        for (i, piece) in line.drain(..).enumerate() {
            let x = piece.x + offset;
            self.display.push(DisplayItem::Glyphs {
                origin: Point::new(x, piece.y),
                glyphs: piece.glyphs,
                color: piece.color,
                style: piece.font,
            });
            if piece.underline {
                self.display.push(DisplayItem::Rect {
                    rect: Rect::new(x, piece.y + piece.px as i32, under_w[i], 1),
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
        // The next line's atomic boxes start after everything emitted here.
        self.mark_line_start();
    }

    fn rule(&mut self) {
        self.flush_vmargin();
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
        self.line_hf = 0.0;
        self.line_frac = 0.0;
        self.x_frac = 0.0;
        self.pending_space = false;
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
        // `add_inline_block`, which would recurse here) — ADR-0042. An
        // element-children `<button>` takes the same block path, else the walk
        // re-dispatches to `form_button` and recurses back into measurement.
        scratch.as_block_once =
            matches!(node.style.display, Display::InlineBlock) || button_wants_block(node);
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
        scratch.as_block_once =
            matches!(node.style.display, Display::InlineBlock) || button_wants_block(node);
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
                        pos_px: style.background_position_px,
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
        self.flush_vmargin();
        let is_right = e.style.float == cerberus_style::Float::Right;
        let avail = (self.right - self.left).max(1);
        let explicit = resolve_block_width(&e.style, avail, self.vw, self.vh);
        let floor = e
            .style
            .min_width
            .resolve_vp(avail, self.vw, self.vh)
            .unwrap_or(0);
        let w = explicit
            .unwrap_or_else(|| self.measure_intrinsic_width(e).min(avail))
            .max(floor)
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
    fn flex_layout(&mut self, node: &StyledNode, in_link: Option<&str>) {
        self.flush_line();
        self.flush_vmargin();
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

        // Flex items in `order` (stable sort keeps document order within a
        // group); bare text children become anonymous items (Flexbox §4).
        let anon = anon_text_items(node);
        let mut items = collect_flex_grid_items(node, &anon);
        items.sort_by_key(|e| e.style.order);

        let (ds, ls, fs, es) = (
            self.display.items.len(),
            self.links.len(),
            self.fields.len(),
            self.elements.len(),
        );
        if !items.is_empty() {
            match node.style.flex_direction {
                FlexDirection::Row => {
                    self.flex_row(&items, left, right, gap, start_y, &node.style, in_link)
                }
                FlexDirection::Column => {
                    self.flex_column(&items, left, right, gap, start_y, &node.style, in_link)
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

    #[allow(clippy::too_many_arguments)]
    fn flex_row(
        &mut self,
        items: &[&StyledNode],
        left: i32,
        right: i32,
        gap: i32,
        start_y: i32,
        style: &ComputedStyle,
        in_link: Option<&str>,
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
                sub.walk(items[i], in_link);
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

    #[allow(clippy::too_many_arguments)]
    fn flex_column(
        &mut self,
        items: &[&StyledNode],
        left: i32,
        right: i32,
        gap: i32,
        start_y: i32,
        style: &ComputedStyle,
        in_link: Option<&str>,
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
            sub.walk(it, in_link);
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
    fn grid_layout(&mut self, node: &StyledNode, in_link: Option<&str>) {
        self.flush_line();
        self.flush_vmargin();
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

        // Element items plus anonymous items for bare text children (Grid §6.1).
        let anon = anon_text_items(node);
        let items = collect_flex_grid_items(node, &anon);

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
            let rs = match (it.style.grid_row_start, it.style.grid_row_end) {
                // Explicit numeric row lines fix the span too (`grid-row: 1/3`).
                (Some(a), Some(b)) => {
                    let a = resolve_grid_line(a, usize::MAX / 2);
                    let b = resolve_grid_line(b, usize::MAX / 2);
                    b.saturating_sub(a).max(1)
                }
                _ => (it.style.grid_row_span as usize).max(1),
            };
            let (r0, c0, cs) = if let Some(start) = it.style.grid_column_start {
                // Explicit numeric line placement (`grid-column: 2/9`, `1/-1`):
                // anchor the column to the resolved line — mozilla's hero
                // anchors its flag text at line 2 of a 12-track grid, a whole
                // track off under auto-placement. The row takes its own
                // explicit line when given, else the first row where the
                // column block is free.
                let c0 = resolve_grid_line(start, ncols).min(ncols - 1);
                let cs = match it.style.grid_column_end {
                    Some(end) => resolve_grid_line(end, ncols).saturating_sub(c0),
                    None => it.style.grid_column_span as usize,
                }
                .clamp(1, ncols - c0);
                let r0 = match it.style.grid_row_start {
                    Some(rn) => {
                        let r0 = resolve_grid_line(rn, usize::MAX / 2);
                        while occ.len() < r0 + rs {
                            occ.push(vec![false; ncols]);
                        }
                        r0
                    }
                    None => find_free_at(&mut occ, ncols, c0, cs, rs),
                };
                (r0, c0, cs)
            } else if it.style.grid_named_place {
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
            sub.walk(it, in_link);
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
    /// Intrinsic content width of a table cell: flow its children into a very
    /// wide scratch (nothing wraps) and read the widest extent. This walks the
    /// cell's *children*, not the cell node — `walk` returns early on a
    /// `<td>`/`<tr>` (they are laid only by `table`), so measuring the cell node
    /// directly reads nothing. Floated inline children (a common nav/link cell)
    /// pack horizontally in the wide scratch, so their real row width is counted.
    fn measure_cell_width(&mut self, cell: &StyledNode) -> i32 {
        let mut sub = Ctx::sub(
            0,
            1_000_000,
            0,
            self.shaper,
            self.images,
            self.forms,
            self.field_id,
            self.vw,
            self.vh,
        );
        sub.measuring = true;
        let mut fb = FloatBand::new(sub.left, sub.right, sub.y);
        for child in &cell.children {
            match child {
                StyledChild::Text(t) => sub.add_text(t, &cell.style, None),
                StyledChild::Element(e) if e.style.float != cerberus_style::Float::None => {
                    sub.place_float(e, None, &mut fb)
                }
                StyledChild::Element(e) => sub.walk(e, None),
            }
        }
        sub.flush_floats(&mut fb);
        sub.flush_line();
        sub.max_x.max(1)
    }

    fn table(&mut self, node: &StyledNode) {
        self.flush_line();
        self.flush_vmargin();
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
            .map(|r| cell_children(r).map(cell_colspan).sum::<usize>())
            .max()
            .unwrap_or(0);

        // Nothing to lay out: leave a small margin and bail (never panic).
        if num_cols == 0 || right - left < num_cols as i32 {
            self.y += TABLE_MARGIN;
            self.x = self.left;
            return;
        }

        // Content-proportional column widths (auto table layout) rather than an
        // equal split: size each column to its widest cell's intrinsic content
        // (plus padding). An auto-width table whose content fits shrinks to it
        // (Chrome's footer nav: a narrow label column beside a wide links
        // column); an explicit-width table — or content that overflows — fills
        // the target width proportionally. `col_x` holds the column left edges;
        // `col_x[num_cols]` is the table's right edge.
        let avail = (right - left).max(num_cols as i32);
        // `cellpadding` (HTML presentational) sets every cell's padding; the
        // engine default stands in for the UA's when absent.
        let pad = node
            .attr("cellpadding")
            .and_then(parse_dim)
            .map(|v| (v as i32).clamp(0, 40))
            .unwrap_or(CELL_PAD);
        let mut col_max = vec![1i32; num_cols];
        for row in &rows {
            let mut col = 0usize;
            for cell in cell_children(row) {
                if col >= num_cols {
                    break;
                }
                let span = cell_colspan(cell).min(num_cols - col);
                let w = (self.measure_cell_width(cell) + 2 * pad).max(1);
                // A spanning cell contributes its width divided across its
                // columns (adequate stand-in for the spec's proportional
                // distribution).
                let per = (w / span as i32).max(1);
                for m in col_max.iter_mut().skip(col).take(span) {
                    *m = (*m).max(per);
                }
                col += span;
            }
        }
        let total: i64 = col_max.iter().map(|&w| w as i64).sum();
        let explicit = resolve_block_width(&node.style, avail, self.vw, self.vh);
        let target = explicit.unwrap_or(avail).clamp(num_cols as i32, avail);
        let col_widths: Vec<i32> = if explicit.is_some() || total > target as i64 {
            col_max
                .iter()
                .map(|&c| ((c as i64 * target as i64) / total).max(1) as i32)
                .collect()
        } else {
            col_max
        };
        // A table narrower than its containing block centers when the legacy
        // `<center>` (`-webkit-center`) is in effect or `align=center` set auto
        // margins — the HN shell pattern (`<center><table width=85%>`): the
        // BOX centers while cell text stays left (see flow_cell).
        let table_w: i32 = col_widths.iter().sum();
        // Inherited `<center>` stops at a cell boundary (`in_cell`): the inner
        // table sits at the cell's left edge in the reference, even though the
        // outer table centered. `align=center`/auto margins still center.
        let centered = (matches!(node.style.text_align, TextAlign::WebkitCenter) && !self.in_cell)
            || (node.style.margin_left_auto && node.style.margin_right_auto);
        let x0 = if centered {
            left + ((avail - table_w) / 2).max(0)
        } else {
            left
        };
        let mut col_x = vec![x0; num_cols + 1];
        for col in 0..num_cols {
            col_x[col + 1] = col_x[col] + col_widths[col];
        }

        // HTML `border` attribute: cells get 1px rules only when it is present and
        // non-zero (HTML §15). Layout tables (`border="0"` or absent, as on Hacker
        // News) draw no grid lines — matching Chrome, which otherwise diverges by a
        // black line around every cell.
        let draw_border = node
            .attr("border")
            .and_then(parse_dim)
            .is_some_and(|b| b > 0);

        // The table's own background (e.g. `<table bgcolor=…>`, the HN beige)
        // paints under all rows: reserve its display slot now, fill it once the
        // total height is known.
        let table_top = self.y;
        let table_bg_index = self.display.items.len();
        let mut row_y = self.y;

        for row in rows {
            // Resolve each cell's column start + span once (colspan-aware), so
            // laying, box-painting, and advancing all agree — a `colspan=2`
            // title cell (the HN pattern) spans its columns instead of pushing
            // later cells into the wrong ones.
            let mut placed: Vec<(&StyledNode, usize, usize)> = Vec::new();
            let mut col = 0usize;
            for cell in cell_children(row) {
                if col >= num_cols {
                    break;
                }
                let span = cell_colspan(cell).min(num_cols - col);
                placed.push((cell, col, span));
                col += span;
            }
            if placed.is_empty() {
                // A cell-less spacer row (`<tr style="height:5px">` — HN
                // separates stories with these) still contributes its declared
                // height; it used to contribute nothing, compressing the list
                // by ~5px per item.
                let h = row
                    .style
                    .height
                    .resolve_vp(0, self.vw, self.vh)
                    .or_else(|| row.attr("height").and_then(parse_dim).map(|v| v as i32))
                    .unwrap_or(0);
                row_y += h.max(0);
                continue;
            }

            // Sub-lay every cell, capturing its items/links/fields and height.
            // The row is as tall as its tallest CELL (plus padding) — flooring
            // at the TABLE font's line height inflated small-print rows (HN's
            // 7pt subtext measures 10px in Chrome, not the table's 15px line).
            // An explicit `<tr height>` still sets a minimum.
            let mut laid: Vec<CellLayout> = Vec::with_capacity(placed.len());
            let mut row_h = 0;
            for &(cell, col, span) in &placed {
                let cell_x = col_x[col];
                let cell_w = (col_x[(col + span).min(num_cols)] - cell_x).max(1);
                let (items, links, fields, h) =
                    self.flow_cell(cell, cell_x, cell_x + cell_w, row_y, pad);
                row_h = row_h.max(h);
                laid.push((items, links, fields, h));
            }
            let tr_min = row
                .style
                .height
                .resolve_vp(0, self.vw, self.vh)
                .or_else(|| row.attr("height").and_then(parse_dim).map(|v| v as i32))
                .unwrap_or(0);
            row_h = (row_h + 2 * pad).max(tr_min).max(1);

            // Emit each cell's box (fill + border) under its content.
            for &(cell, col, span) in &placed {
                let cell_x = col_x[col];
                let cell_w = (col_x[(col + span).min(num_cols)] - cell_x).max(1) as u32;
                let is_header = cell.tag == "th";
                let fill = cell
                    .style
                    .background
                    .or(if is_header { Some(TH_BG) } else { None });
                self.cell_box(cell_x, row_y, cell_w, row_h as u32, fill, draw_border);
            }

            // Then the captured cell content, on top of the boxes.
            for (items, links, fields, _) in laid {
                self.display.items.extend(items);
                self.links.extend(links);
                self.fields.extend(fields);
            }

            row_y += row_h;
        }

        if let Some(bg) = node.style.background {
            let h = (row_y - table_top).max(0) as u32;
            if h > 0 && bg.a > 0 {
                self.display.items.insert(
                    table_bg_index,
                    DisplayItem::Rect {
                        rect: Rect::new(x0, table_top, table_w.max(0) as u32, h),
                        color: bg,
                    },
                );
            }
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
        pad: i32,
    ) -> CellLayout {
        let content_left = cell_x + pad;
        let content_right = (cell_right - pad).max(content_left + 1);
        let content_top = cell_y + pad;
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
        // Legacy `<center>` block-centering stops here (see `in_cell`).
        sub.in_cell = true;

        let is_header = cell.tag == "th";
        // Headers centre their text; cells take their own alignment — except
        // the legacy `<center>` value, which centers the TABLE BOX but not the
        // text inside its cells (measured against the reference: HN's titles
        // are left-aligned inside a `<center>`ed table).
        sub.line_align = if is_header {
            TextAlign::Center
        } else if cell.style.text_align == TextAlign::WebkitCenter {
            TextAlign::Left
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
        // A last child's deferred bottom margin is contained by the cell (a
        // table cell establishes its own formatting context; margins never
        // escape it) — HN's votearrow div (`margin: 3px 2px 6px`) lost its
        // bottom 6px here, shorting every item row.
        sub.flush_vmargin();

        // Carry the advanced control counter back to the parent.
        self.field_id = sub.field_id;
        // After flush, `sub.y` already includes the last line; a cell WITH
        // content is floored at one line of its own font, an empty cell
        // contributes nothing (Chrome: an empty row is padding-tall).
        let height = if sub.y > content_top || !sub.display.items.is_empty() {
            (sub.y - content_top).max(line_height(cell.style.font_size.max(1)))
        } else {
            0
        };
        (sub.display.items, sub.links, sub.fields, height)
    }

    /// Draw a cell's optional background fill and, when the table requested a
    /// border, its 1px outline.
    fn cell_box(&mut self, x: i32, y: i32, w: u32, h: u32, fill: Option<Color>, border: bool) {
        if w == 0 || h == 0 {
            return;
        }
        if let Some(color) = fill {
            self.display.push(DisplayItem::Rect {
                rect: Rect::new(x, y, w, h),
                color,
            });
        }
        if !border {
            return;
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

/// The `line-height: normal` leading for text of `px` font size. Browsers derive
/// this from the font's own metrics — typically ~1.15–1.2× the font size for the
/// common sans/serif faces; we approximate it as 1.2×. (Was 1.5×, which inflated
/// the vertical rhythm of every text block ~25% taller than Chrome, accumulating
/// into large below-the-fold misalignment on text-heavy pages.)
fn line_height(px: u32) -> i32 {
    (px as i32 * 6) / 5
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

/// The `min-width` floor as a **border-box** width (box-sizing aware), or 0 when
/// unset. Applied by shrink-to-fit callers after measuring content, since
/// `resolve_block_width` deliberately leaves an auto-width box unconstrained
/// rather than pinning it to `min-width`.
fn min_border_box_width(style: &ComputedStyle, avail: i32, h_extra: i32, vw: i32, vh: i32) -> i32 {
    match style.min_width.resolve_vp(avail, vw, vh) {
        Some(mw) => {
            let mw = mw.max(0);
            match style.box_sizing {
                cerberus_style::BoxSizing::BorderBox => mw,
                cerberus_style::BoxSizing::ContentBox => mw + h_extra,
            }
        }
        None => 0,
    }
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
    // `min-width` is a floor, not a definite width. It clamps a width already
    // fixed by `width`/`max-width`; but with `width:auto` and no `max-width`
    // the box is unconstrained (a block fills `avail`, an inline-block/float
    // shrink-fits its content) — flooring against 0 here would collapse a
    // shrink-to-fit box to `min-width` and suppress content measurement. Leave
    // it unconstrained then, unless `min-width` exceeds `avail` (an overflow the
    // floor genuinely widens); the shrink-to-fit caller re-applies the floor.
    if let Some(mw) = res(style.min_width) {
        match w {
            Some(cur) => w = Some(cur.max(mw)),
            None if mw > avail => w = Some(mw),
            None => {}
        }
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
    // Only px / viewport units count; `%`/`auto` leave the box content-sized.
    let res = |len: Len| match len {
        Len::Px(p) => Some(p.max(0)),
        Len::Vw(f) => Some((f / 100.0 * vw as f32).round().max(0.0) as i32),
        Len::Vh(f) => Some((f / 100.0 * vh as f32).round().max(0.0) as i32),
        Len::Vmin(f) => Some((f / 100.0 * vw.min(vh) as f32).round().max(0.0) as i32),
        Len::Vmax(f) => Some((f / 100.0 * vw.max(vh) as f32).round().max(0.0) as i32),
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

/// Bare text children of a flex/grid container form ANONYMOUS items (CSS
/// Flexbox §4 / Grid §6.1) — `<div style="display:flex">Products<span>…`
/// must lay "Products" out as its own item, not drop it. Each non-whitespace
/// text run is wrapped in a synthesized block inheriting the container's text
/// style; returns `(child_index, node)` storage that
/// [`collect_flex_grid_items`] interleaves back in document order.
fn anon_text_items(node: &StyledNode) -> Vec<(usize, StyledNode)> {
    node.children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match c {
            StyledChild::Text(t) if !t.trim().is_empty() => {
                let mut style = node.style.inherit();
                style.display = Display::Block;
                Some((
                    i,
                    StyledNode {
                        tag: String::new(),
                        attrs: Vec::new(),
                        style,
                        children: vec![StyledChild::Text(t.clone())],
                        node_id: node.node_id,
                    },
                ))
            }
            _ => None,
        })
        .collect()
}

/// The flex/grid items of `node` in document order: element children that are
/// in-flow items, with the anonymous text items from [`anon_text_items`]
/// spliced back at their original child positions.
fn collect_flex_grid_items<'n>(
    node: &'n StyledNode,
    anon: &'n [(usize, StyledNode)],
) -> Vec<&'n StyledNode> {
    let mut ai = anon.iter().peekable();
    let mut items = Vec::new();
    for (i, c) in node.children.iter().enumerate() {
        if let Some((j, n)) = ai.peek() {
            if *j == i {
                items.push(n);
                ai.next();
                continue;
            }
        }
        if let StyledChild::Element(e) = c {
            if is_flex_grid_item(e) {
                items.push(e.as_ref());
            }
        }
    }
    items
}

/// A `<button>` with element children (icon `<i>`/`<span>` sprites, not just a
/// text label) is laid as an atomic block-model box so its children paint —
/// `form_button` routes it through `add_inline_block`. During intrinsic-width
/// measurement the button must take the same block path; otherwise the scratch
/// walk re-dispatches to `form_button`, which re-enters measurement and
/// overflows the stack.
fn button_wants_block(node: &StyledNode) -> bool {
    node.tag == "button"
        && node
            .children
            .iter()
            .any(|c| matches!(c, StyledChild::Element(_)))
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

/// Resolve a CSS grid line number against the track count: line `n > 0` is
/// track index `n − 1`; a negative line counts from the END of the explicit
/// grid (`-1` is the line after the last track, so `1 / -1` spans all tracks).
fn resolve_grid_line(n: i32, ncols: usize) -> usize {
    if n > 0 {
        (n as usize).saturating_sub(1).min(ncols)
    } else {
        (ncols as i64 + 1 + n as i64).max(0) as usize
    }
}

/// The first row where columns `[c0, c0+cs)` are free for `rs` rows (row
/// auto-placement of an explicitly column-anchored item), growing the
/// occupancy grid as needed.
fn find_free_at(occ: &mut Vec<Vec<bool>>, ncols: usize, c0: usize, cs: usize, rs: usize) -> usize {
    let c0 = c0.min(ncols.saturating_sub(1));
    let end = (c0 + cs).min(ncols);
    let mut r = 0;
    loop {
        while occ.len() < r + rs {
            occ.push(vec![false; ncols]);
        }
        if (r..r + rs).all(|rr| (c0..end).all(|cc| !occ[rr][cc])) {
            return r;
        }
        r += 1;
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

/// The ordinal seed for a list block: `<ol start="N">` numbers its first item N,
/// so the child loop (which pre-increments) must start from `N - 1`. Any other
/// element (a `<ul>`, or an `<ol>` with no/invalid `start`) seeds 0. Only a
/// positive `start` is honored (the common case; `reversed`/negative starts are
/// not modeled).
fn ol_start_base(node: &StyledNode) -> u32 {
    if node.tag == "ol" {
        if let Some(n) = node
            .attr("start")
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            return n.saturating_sub(1);
        }
    }
    0
}

/// The marker text for a `list-item`, per `list-style-type`: a bullet glyph, or
/// the `1.`-style decimal ordinal for an `<ol>` (the parent's child loop set the
/// ordinal). `none` yields no marker (and the caller skips the gap too). Ordinals
/// floor at 1 so a stray zero can't produce `0.`.
fn list_marker(kind: ListStyleType, ordinal: u32) -> Option<String> {
    Some(match kind {
        ListStyleType::None => return None,
        ListStyleType::Decimal => format!("{}.", ordinal.max(1)),
        ListStyleType::LowerAlpha => format!("{}.", alpha_ordinal(ordinal.max(1), false)),
        ListStyleType::UpperAlpha => format!("{}.", alpha_ordinal(ordinal.max(1), true)),
        ListStyleType::LowerRoman => format!("{}.", roman_ordinal(ordinal.max(1), false)),
        ListStyleType::UpperRoman => format!("{}.", roman_ordinal(ordinal.max(1), true)),
        ListStyleType::Circle => "\u{25E6}".to_string(), // ◦
        ListStyleType::Square => "\u{25AA}".to_string(), // ▪
        ListStyleType::Disc => "\u{2022}".to_string(),   // •
    })
}

/// Bijective base-26 alphabetic ordinal: 1→a, 26→z, 27→aa, 28→ab, … (upper-cased
/// when `upper`). Used for `list-style-type: lower-alpha`/`upper-alpha`.
fn alpha_ordinal(mut n: u32, upper: bool) -> String {
    let base = if upper { b'A' } else { b'a' };
    let mut buf = Vec::new();
    while n > 0 {
        n -= 1; // 1-based → 0-based within each place
        buf.push(base + (n % 26) as u8);
        n /= 26;
    }
    buf.reverse();
    String::from_utf8(buf).expect("ascii letters")
}

/// Roman-numeral ordinal: 1→i, 4→iv, 9→ix, 40→xl, … (upper-cased when `upper`).
/// Used for `list-style-type: lower-roman`/`upper-roman`. Values are clamped to
/// the classic 1..=3999 range; anything larger falls back to the decimal number
/// so a huge list still numbers monotonically rather than overflowing the glyphs.
fn roman_ordinal(n: u32, upper: bool) -> String {
    if !(1..=3999).contains(&n) {
        return n.to_string();
    }
    const TABLE: &[(u32, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut rem = n;
    let mut out = String::new();
    for &(value, sym) in TABLE {
        while rem >= value {
            out.push_str(sym);
            rem -= value;
        }
    }
    if upper {
        out.to_ascii_uppercase()
    } else {
        out
    }
}

fn space_width(shaper: &dyn TextShaper, px: u32, family: GenericFamily) -> u32 {
    // Delegates to the shaper's `space_advance_with`, which real shapers implement
    // without the per-call `Vec` allocation `shape(" ", …)` would incur — this
    // runs once per word in the inline loop. The family matters for `<pre>`/
    // `<code>`: a monospace space is wider than a proportional one.
    shaper.space_advance_with(px, family)
}

/// [`space_width`] without the rounding — inter-word gaps accumulate
/// fractionally along a line (see `Ctx::x_frac`).
fn space_width_f(shaper: &dyn TextShaper, px: u32, family: GenericFamily) -> f32 {
    shaper.space_advance_with_f(px, family)
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

/// The label for an image rendered as text (the text-only option): its `alt`
/// caption, else its `title` tooltip, else the file name from its `src`, else the
/// generic word "image". Never empty, so a suppressed image is always visible as
/// text.
fn image_text_label(node: &StyledNode, viewport_w: u32) -> String {
    let attr_nonempty = |name: &str| node.attr(name).map(str::trim).filter(|s| !s.is_empty());
    if let Some(alt) = attr_nonempty("alt") {
        return alt.to_string();
    }
    if let Some(title) = attr_nonempty("title") {
        return title.to_string();
    }
    if let Some(src) = pick_img_url(|n| node.attr(n), viewport_w) {
        if let Some(name) = image_file_name(&src) {
            return name;
        }
    }
    "image".to_string()
}

/// The trailing file-name segment of an image URL (minus query/fragment), for a
/// text-only label. `None` for a `data:` URI or an implausible name.
fn image_file_name(src: &str) -> Option<String> {
    if src.starts_with("data:") {
        return None;
    }
    let path = src.split(['?', '#']).next().unwrap_or(src);
    let seg = path.rsplit('/').next().unwrap_or(path).trim();
    if seg.is_empty() || seg.len() > 64 {
        return None;
    }
    Some(seg.to_string())
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
        if let Some(u) = select_srcset_with_src(ss, attr("sizes"), viewport_w, attr("src")) {
            return Some(u);
        }
    }
    attr("src").map(str::to_string)
}

/// A `<source>` candidate inside a `<picture>`, borrowed from the DOM/styled
/// tree: its optional `type` (MIME) and `media` query, plus `srcset`/`sizes`. A
/// `<source>` without a `srcset` is invalid and never contributes a URL.
pub struct PictureSource<'a> {
    pub type_: Option<&'a str>,
    pub media: Option<&'a str>,
    pub srcset: Option<&'a str>,
    pub sizes: Option<&'a str>,
}

/// An owned [`PictureSource`], so the layout walker can stash a `<picture>`'s
/// candidates on the context across the recursion into its `<img>` child.
#[derive(Clone, Debug)]
pub struct OwnedPictureSource {
    pub type_: Option<String>,
    pub media: Option<String>,
    pub srcset: Option<String>,
    pub sizes: Option<String>,
}

impl OwnedPictureSource {
    fn borrow(&self) -> PictureSource<'_> {
        PictureSource {
            type_: self.type_.as_deref(),
            media: self.media.as_deref(),
            srcset: self.srcset.as_deref(),
            sizes: self.sizes.as_deref(),
        }
    }
}

/// Collect the `<source>` candidates of a `<picture>` in document order.
fn collect_picture_sources(picture: &StyledNode) -> Vec<OwnedPictureSource> {
    picture
        .children
        .iter()
        .filter_map(|c| match c {
            StyledChild::Element(e) if e.tag == "source" => Some(OwnedPictureSource {
                type_: e.attr("type").map(str::to_string),
                media: e.attr("media").map(str::to_string),
                srcset: e.attr("srcset").map(str::to_string),
                sizes: e.attr("sizes").map(str::to_string),
            }),
            _ => None,
        })
        .collect()
}

/// Whether the bundled image codec can decode `mime`. Mirrors the formats
/// enabled on the `image` crate in cerberus-image (png/jpeg/gif/webp/bmp) plus
/// SVG via resvg — a `<source type=...>` naming anything else (e.g. AVIF) is
/// skipped so selection falls through to a format we can actually paint, per the
/// WHATWG "picture" source-selection steps.
pub fn image_type_supported(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/svg+xml"
    )
}

/// Evaluate a `<source media>` query against a `vw`×`vh` viewport. Supports the
/// dimension/orientation features responsive `<picture>` art direction uses
/// (`min/max-width`, `min/max-height`, `orientation`) and the `screen`/`all`
/// media types; every other type or feature (`print`, `prefers-*`, …) does not
/// match, so that `<source>` is skipped and selection falls through to the next
/// candidate (ultimately the `<img>`). This tracks the CSS engine's fixed
/// desktop-screen persona: a `<source>` gated on a preference we don't advertise
/// simply yields to the plain `<img>`, which is the safe, visible default.
pub fn picture_media_matches(query: &str, vw: u32, vh: u32) -> bool {
    // OR over comma-separated queries; AND over ` and `-separated parts.
    query.split(',').any(|branch| {
        branch.split(" and ").all(|part| {
            let part = part.trim().trim_start_matches("only ").trim();
            if part.is_empty() {
                // An empty media attribute (or branch) matches everything.
                return true;
            }
            if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
                picture_media_feature(inner.trim(), vw, vh)
            } else {
                // A bare media type, optionally negated. Only screen/all match.
                let (ty, negated) = match part.strip_prefix("not ") {
                    Some(rest) => (rest.trim(), true),
                    None => (part, false),
                };
                let is_screen = ty.eq_ignore_ascii_case("screen") || ty.eq_ignore_ascii_case("all");
                is_screen != negated
            }
        })
    })
}

/// Evaluate one `(feature: value)` from a `<source media>` query. An unknown or
/// malformed feature is false (spec: it makes the query fail), so the candidate
/// is skipped rather than matched by accident.
fn picture_media_feature(feat: &str, vw: u32, vh: u32) -> bool {
    let (name, value) = match feat.split_once(':') {
        Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()),
        None => (feat.trim().to_ascii_lowercase(), String::new()),
    };
    let px = || -> Option<u32> {
        value
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    };
    match name.as_str() {
        "min-width" => px().is_some_and(|p| vw >= p),
        "max-width" => px().is_some_and(|p| vw <= p),
        "min-height" => px().is_some_and(|p| vh >= p),
        "max-height" => px().is_some_and(|p| vh <= p),
        "orientation" => match value.as_str() {
            "portrait" => vh >= vw,
            "landscape" => vw > vh,
            _ => false,
        },
        _ => false,
    }
}

/// Choose the URL a `<picture>`'s `<img>` should load (WHATWG "select an image
/// source"): the first `<source>`, in document order, whose `type` we can decode
/// and whose `media` matches, resolved through its `srcset`/`sizes`; otherwise
/// the `<img>`'s own [`pick_img_url`]. The fetch-time collector and layout both
/// call this with the same viewport, so they agree on the chosen candidate.
pub fn pick_picture_url<'a>(
    sources: &[PictureSource<'_>],
    img_attr: impl Fn(&str) -> Option<&'a str>,
    vw: u32,
    vh: u32,
) -> Option<String> {
    for s in sources {
        if s.type_.is_some_and(|t| !image_type_supported(t)) {
            continue;
        }
        if s.media.is_some_and(|m| !picture_media_matches(m, vw, vh)) {
            continue;
        }
        // A <source> must carry a srcset; without one it contributes nothing.
        let Some(ss) = s.srcset else { continue };
        if let Some(u) = select_srcset(ss, s.sizes, vw) {
            return Some(u);
        }
    }
    pick_img_url(img_attr, vw)
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
    select_srcset_with_src(srcset, sizes, viewport_w, None)
}

/// Like [`select_srcset`] but also considers the element's plain `src` as an
/// implicit 1x density candidate. Per the HTML "update the source set" algorithm,
/// when a `srcset` carries only density (`x`) descriptors, `src` participates as
/// the 1x source — so at device-pixel-ratio 1 a `src="a.png" srcset="a@2x.png 2x"`
/// resolves to `a.png`, not the 2x image (which a naive srcset-only pick returns
/// as the smallest density ≥ 1). `src` is ignored when width (`w`) descriptors are
/// present (the spec drops density selection entirely in that mode).
pub fn select_srcset_with_src(
    srcset: &str,
    sizes: Option<&str>,
    viewport_w: u32,
    src: Option<&str>,
) -> Option<String> {
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
    // A plain `src` joins the density pool as 1x, but only in density mode and
    // only if it isn't already listed as a candidate.
    if width.is_empty() {
        if let Some(s) = src {
            if !density.iter().any(|(_, u)| *u == s) {
                density.push((1.0, s));
            }
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

/// A cell's `colspan` (1 when absent/invalid; capped to keep col math sane).
fn cell_colspan(cell: &StyledNode) -> usize {
    cell.attr("colspan")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&s| s >= 1)
        .unwrap_or(1)
        .min(64)
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

    fn img_node(attrs: &[(&str, &str)]) -> StyledNode {
        StyledNode {
            tag: "img".into(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            style: ComputedStyle::initial(),
            children: Vec::new(),
            node_id: 0,
        }
    }

    #[test]
    fn adjacent_sibling_margins_collapse_to_the_larger() {
        // Two paragraphs with explicit 20px margins: the gap between their glyph
        // baselines must reflect ONE 20px margin (collapsed), not 40px stacked.
        // The control pair uses margin-bottom only, so its gap is the yardstick.
        let collapsed = lay(
            "<p style='margin:20px 0'>a</p><p style='margin:20px 0'>b</p>",
            600,
        );
        let control = lay(
            "<p style='margin:0 0 20px 0'>a</p><p style='margin:0'>b</p>",
            600,
        );
        let cy = glyph_ys(&collapsed);
        let ky = glyph_ys(&control);
        assert_eq!(cy.len(), 2);
        assert_eq!(ky.len(), 2);
        assert_eq!(
            cy[1] - cy[0],
            ky[1] - ky[0],
            "20/20 collapses to the same separation as a lone 20"
        );

        // Unequal margins: max(30, 10) = 30 — same separation as a lone 30.
        let unequal = lay(
            "<p style='margin:0 0 30px 0'>a</p><p style='margin:10px 0 0 0'>b</p>",
            600,
        );
        let lone30 = lay(
            "<p style='margin:0 0 30px 0'>a</p><p style='margin:0'>b</p>",
            600,
        );
        assert_eq!(
            glyph_ys(&unequal)[1] - glyph_ys(&unequal)[0],
            glyph_ys(&lone30)[1] - glyph_ys(&lone30)[0],
            "max(30,10) = 30"
        );
    }

    #[test]
    fn negative_margins_collapse_as_most_positive_plus_most_negative() {
        // 30px bottom then -10px top: separation = 30 + (-10) = 20 — identical
        // to a lone 20px margin between the same two paragraphs.
        let mixed = lay(
            "<p style='margin:0 0 30px 0'>a</p><p style='margin:-10px 0 0 0'>b</p>",
            600,
        );
        let lone20 = lay(
            "<p style='margin:0 0 20px 0'>a</p><p style='margin:0'>b</p>",
            600,
        );
        assert_eq!(
            glyph_ys(&mixed)[1] - glyph_ys(&mixed)[0],
            glyph_ys(&lone20)[1] - glyph_ys(&lone20)[0],
            "30 + (-10) = 20"
        );
    }

    #[test]
    fn block_then_text_realizes_the_pending_margin() {
        // A block followed by bare inline text at the same level: the block's
        // bottom margin has nothing to collapse with and applies in full.
        let with_margin = lay("<div><p style='margin:0 0 24px 0'>a</p>text</div>", 600);
        let without = lay("<div><p style='margin:0'>a</p>text</div>", 600);
        let wy = glyph_ys(&with_margin);
        let ny = glyph_ys(&without);
        assert_eq!(
            (wy[1] - wy[0]) - (ny[1] - ny[0]),
            24,
            "the full 24px margin separates block from following text"
        );
    }

    #[test]
    fn center_centers_a_narrow_table_box_but_not_its_cell_text() {
        // The HN shell: <center><table width=50%> — the table BOX centers in
        // the containing block, while text inside cells stays left-aligned
        // (measured against the reference).
        let laid = lay(
            "<center><table width='50%' bgcolor='#ffdddd' cellpadding='0'>             <tr><td>x</td></tr></table></center>",
            600,
        );
        let bg = laid
            .display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Rect { rect, color } if color.r > 240 && color.g < 240 => Some(*rect),
                _ => None,
            })
            .expect("table background painted");
        // 50% of 600 = 300 wide, centered → x ≈ 150.
        assert!(
            (bg.x - 150).abs() <= 8 && (bg.w as i32 - 300).abs() <= 8,
            "table box centered at ~150 width ~300, got x={} w={}",
            bg.x,
            bg.w
        );
        // The cell text is LEFT inside the box (near the box's left edge, not
        // centered within the 300px cell).
        let gx = glyph_xs(&laid);
        assert!(!gx.is_empty());
        assert!(
            gx[0] <= bg.x + 12,
            "cell text left-aligned at the cell edge, got x={} (box x={})",
            gx[0],
            bg.x
        );
    }

    #[test]
    fn table_colspan_spans_columns_and_keeps_later_rows_aligned() {
        // Row 1: a colspan=2 cell + one cell (3 columns total). Row 2: three
        // cells. The colspan cell must span columns 0-1, and row 2's cells must
        // land in columns 0/1/2 — not shifted (the HN rank/title interleave).
        let laid = lay(
            "<table cellpadding='0'>\
             <tr><td colspan='2' style='background:#ff0000'>wide</td>\
                 <td style='background:#00ff00'>c</td></tr>\
             <tr><td style='background:#0000ff'>a</td>\
                 <td style='background:#ffff00'>b</td>\
                 <td style='background:#ff00ff'>c</td></tr>\
             </table>",
            600,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 5, "five cell boxes painted");
        // The spanning cell covers exactly the width of columns 0+1, from col 0.
        let wide = r.iter().find(|rc| rc.w >= 30).expect("span box");
        let narrow: Vec<_> = r.iter().filter(|rc| rc.w < 30 && rc.x < 30).collect();
        assert_eq!(wide.x, 0, "span starts at column 0");
        assert_eq!(
            wide.w,
            narrow.iter().map(|rc| rc.w).sum::<u32>(),
            "span width = col0 + col1"
        );
        // Both rows' third-column cells share one x — later rows aren't shifted.
        let col2: Vec<_> = r.iter().filter(|rc| rc.x as u32 == wide.w).collect();
        assert_eq!(col2.len(), 2, "one column-2 cell per row");
        assert_ne!(col2[0].y, col2[1].y, "one per row");
    }

    #[test]
    fn links_inside_flex_and_grid_containers_keep_hit_boxes() {
        // The brand-page card pattern: an <a> WRAPPING a flex/grid container
        // (and links nested in items) must still emit link hit boxes — flex/
        // grid item sub-layouts previously dropped the enclosing href.
        let laid = lay(
            "<a href='/card'><div style='display:flex'>\
               <div>Headline</div><div>Blurb</div>\
             </div></a>\
             <div style='display:grid;grid-template-columns:1fr 1fr'>\
               <div><a href='/g1'>One</a></div><div><a href='/g2'>Two</a></div>\
             </div>",
            600,
        );
        let hrefs: Vec<&str> = laid.links.iter().map(|l| l.href.as_str()).collect();
        assert!(
            hrefs.contains(&"/card"),
            "anchor wrapping a flex container is clickable: {hrefs:?}"
        );
        assert!(hrefs.contains(&"/g1") && hrefs.contains(&"/g2"));
        assert!(
            laid.links.iter().all(|l| l.rect.w > 0 && l.rect.h > 0),
            "no degenerate link boxes"
        );
    }

    #[test]
    fn image_only_anchor_is_clickable() {
        // A logo link (<a> wrapping only an <img>) must emit a link hit box
        // over the image rect — there is no text piece to box at line commit.
        let styled = CssEngine::new().style(&parse_html(
            "<a href='/home'><img src='logo.png' alt='logo'></a>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(40, 20),
            rgba: vec![255; 40 * 20 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(600, 400),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
        );
        let link = laid
            .links
            .iter()
            .find(|l| l.href == "/home")
            .expect("image link boxed");
        assert!(
            link.rect.w >= 40 && link.rect.h >= 20,
            "box covers the image"
        );
    }

    #[test]
    fn table_cellpadding_zero_tightens_rows() {
        // cellpadding=0 rows are tighter than the engine-default padding.
        let padded = lay("<table><tr><td>a</td></tr><tr><td>b</td></tr></table>", 400);
        let tight = lay(
            "<table cellpadding='0'><tr><td>a</td></tr><tr><td>b</td></tr></table>",
            400,
        );
        let py = glyph_ys(&padded);
        let ty = glyph_ys(&tight);
        assert_eq!(py.len(), 2);
        assert_eq!(ty.len(), 2);
        assert!(
            ty[1] - ty[0] < py[1] - py[0],
            "cellpadding=0 row pitch {} < default {}",
            ty[1] - ty[0],
            py[1] - py[0]
        );
    }

    #[test]
    fn image_text_label_prefers_alt_then_title_then_filename() {
        // alt wins.
        assert_eq!(
            image_text_label(
                &img_node(&[("alt", "a lake"), ("title", "tip"), ("src", "/p.png")]),
                1000
            ),
            "a lake"
        );
        // title when alt is empty/missing.
        assert_eq!(
            image_text_label(&img_node(&[("title", "tip"), ("src", "/p.png")]), 1000),
            "tip"
        );
        // file name when neither alt nor title.
        assert_eq!(
            image_text_label(
                &img_node(&[("src", "https://x.test/a/b/logo-v2.png?q=1")]),
                1000
            ),
            "logo-v2.png"
        );
        // generic word when nothing usable.
        assert_eq!(image_text_label(&img_node(&[]), 1000), "image");
    }

    #[test]
    fn text_overflow_ellipsis_truncates_a_clipped_nowrap_line() {
        let long = "the quick brown fox jumps over the lazy dog again and again";
        let base = "<div style='width:120px;white-space:nowrap;overflow:hidden";
        let ellip = lay(&format!("{base};text-overflow:ellipsis'>{long}</div>"), 400);
        let clip = lay(&format!("{base}'>{long}</div>"), 400);
        // The clipped line keeps every glyph (clipping happens in paint); the
        // ellipsis line drops the overflowing tail, so it has strictly fewer.
        assert!(
            total_glyphs(&ellip) < total_glyphs(&clip),
            "ellipsis truncates: {} vs {}",
            total_glyphs(&ellip),
            total_glyphs(&clip)
        );
        // A short line that fits is untouched (no truncation).
        let short = lay(&format!("{base};text-overflow:ellipsis'>hi</div>"), 400);
        assert_eq!(total_glyphs(&short), 2, "short line keeps all glyphs");
    }

    #[test]
    fn image_file_name_extracts_the_segment() {
        assert_eq!(
            image_file_name("https://x/a/b/pic.jpg").as_deref(),
            Some("pic.jpg")
        );
        assert_eq!(
            image_file_name("/logo.png?v=2#x").as_deref(),
            Some("logo.png")
        );
        assert_eq!(image_file_name("data:image/png;base64,AAAA"), None);
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
            (w0 + w1 - 800).abs() <= 4,
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
        assert!(total <= 804, "shrunk to fit the container: total {total}");
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
            (r[0].w as i32 - 200).abs() <= 3,
            "25% of 800 ~ 200: {}",
            r[0].w
        );
        assert!(
            (r[1].w as i32 - 400).abs() <= 3,
            "50% of 800 ~ 400: {}",
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
        // avail 800 with minmax(200px, 1fr) auto-fill -> 4 columns fit exactly.
        let laid = lay(
            "<div style='display:grid;grid-template-columns:repeat(auto-fill, minmax(200px, 1fr))'>\
             <div style='background:#ff0000'>A</div><div style='background:#00ff00'>B</div>\
             <div style='background:#0000ff'>C</div><div style='background:#ffff00'>D</div></div>",
            800,
        );
        let r = fill_rects(&laid);
        assert_eq!(r.len(), 4);
        let xs: Vec<i32> = r.iter().map(|rc| rc.x).collect();
        assert_eq!(distinct(&xs), 4, "four columns fit at 800px content width");
        for rc in &r {
            assert!(
                (rc.w as i32 - 195).abs() <= 6,
                "column width ~195: {}",
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
            (r[0].w as i32 - 400).abs() <= 6,
            "span-2 cell ~400: {}",
            r[0].w
        );
        assert!(
            (r[1].w as i32 - 200).abs() <= 4,
            "single cell ~200: {}",
            r[1].w
        );
        // B is placed in the third column (after the 2-col span), not overlapping.
        assert!(r[1].x >= r[0].x + 388, "B starts after the spanned cell");
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
            (r[0].w as i32 - 400).abs() <= 4,
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
            (r[0].w as i32 - 300).abs() <= 4,
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
    fn table_columns_size_to_content_not_equal_split() {
        // Auto table layout: a short label column stays narrow beside a wide
        // content column, rather than each taking an equal half of the width.
        let laid = lay(
            "<table border=\"1\"><tr><td>Hi</td>\
             <td>a considerably longer stretch of cell content goes here</td></tr></table>",
            600,
        );
        // Cells draw horizontal border rects (height 1) spanning each column.
        let mut widths: Vec<u32> = laid
            .display
            .items
            .iter()
            .filter_map(|it| match it {
                DisplayItem::Rect { rect, .. } if rect.h == 1 && rect.w > 2 => Some(rect.w),
                _ => None,
            })
            .collect();
        widths.sort_unstable();
        widths.dedup();
        assert!(widths.len() >= 2, "two distinct column widths: {widths:?}");
        // An equal split would make each column ~300px; the label shrinks well
        // below that, and the content column is wider.
        assert!(widths[0] < 200, "label column sized to content: {widths:?}");
        assert!(
            *widths.last().unwrap() > widths[0],
            "content column is wider than the label: {widths:?}"
        );
    }

    #[test]
    fn percent_and_viewport_margins_resolve_at_layout() {
        // `%` margins resolve against the containing-block width (all sides), and
        // `vh`/`vw` against the viewport — both were dropped when margins were
        // stored as parse-time px. A 10% top margin in a 400px-wide container is
        // 40px; a following block sits that much lower.
        let pct = lay("<div style='margin-top:10%'>x</div><div>y</div>", 400);
        let plain = lay("<div>x</div><div>y</div>", 400);
        let first_y = |l: &LaidOut| glyph_ys(l)[0];
        assert!(
            first_y(&pct) - first_y(&plain) >= 38,
            "10% of 400px ~= 40px top margin: {} vs {}",
            first_y(&pct),
            first_y(&plain)
        );
    }

    #[test]
    fn normal_line_height_is_about_1_2x_font_size() {
        // `line-height: normal` leads ~1.2x the font size (browser-like), not the
        // old 1.5x that made every text block a quarter too tall.
        let laid = lay(
            "<p style='font-size:20px;margin:0'>one</p>\
             <p style='font-size:20px;margin:0'>two</p>",
            400,
        );
        let mut ys = glyph_ys(&laid);
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(ys.len(), 2, "two single-line paragraphs: {ys:?}");
        let pitch = ys[1] - ys[0];
        assert!(
            (pitch - 24).abs() <= 1,
            "row pitch ~= 1.2 * 20px = 24, got {pitch}"
        );
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
    fn fractional_line_pitch_accumulates_like_chrome() {
        // Chrome keeps line pitch fractional: 16px Arial-metric text advances
        // 18.398px per line, and line N is painted at round(N × pitch). A shaper
        // with 1.15× leading (18.4 @ 16px) must place five lines at cumulative
        // offsets 18, 37, 55, 74 — not the drifting 18, 36, 54, 72 an
        // integer-rounded pitch produces.
        struct FractionalShaper;
        impl TextShaper for FractionalShaper {
            fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
                MonoShaper.shape(text, px)
            }
            fn natural_leading_f(&self, px: u32, _family: GenericFamily) -> f32 {
                px.max(1) as f32 * 1.15
            }
        }
        let styled =
            CssEngine::new().style(&parse_html("<p style='margin:0'>a<br>b<br>c<br>d<br>e</p>"));
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(400, 2000),
            &FractionalShaper,
            &NoImages,
            &NoForms,
        );
        let mut ys = glyph_ys(&laid);
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(ys.len(), 5, "five lines: {ys:?}");
        let rel: Vec<i32> = ys.iter().map(|y| y - ys[0]).collect();
        let want: Vec<i32> = (0..5).map(|n| (n as f32 * 18.4).round() as i32).collect();
        assert_eq!(rel, want, "lines land at round(N × 18.4)");
    }

    #[test]
    fn fractional_space_advance_flips_wrap_point_like_chrome() {
        // Real faces have fractional space advances (Liberation Sans @16px is
        // 4.453px). Chrome accumulates them across the line, so a line that
        // fits with integer-rounded gaps can overflow with the true widths and
        // wrap a word earlier. Three 50px words + two 10.45px gaps = 170.9px
        // in a 170px box: integer gaps (10px → 170 total) kept it on one line;
        // the fractional total must wrap.
        struct FracSpaceShaper;
        impl TextShaper for FracSpaceShaper {
            fn shape(&self, text: &str, px: u32) -> Vec<GlyphBox> {
                MonoShaper.shape(text, px)
            }
            fn space_advance_with_f(&self, px: u32, _family: GenericFamily) -> f32 {
                px.max(2) as f32 + 0.45
            }
        }
        let styled = CssEngine::new().style(&parse_html(
            "<body style='margin:0'><p style='margin:0;font-size:10px'>\
             aaaaaaaaaa bbbbbbbbbb cccccccccc</p></body>",
        ));
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(170, 500),
            &FracSpaceShaper,
            &NoImages,
            &NoForms,
        );
        let mut ys = glyph_ys(&laid);
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(
            ys.len(),
            2,
            "fractional gaps (320.9px total) must wrap the third word: {ys:?}"
        );
    }

    #[test]
    fn inline_image_line_box_reserves_strut_descent() {
        // A baseline-aligned inline image sits with its bottom ON the text
        // baseline, so the line box extends descent + below-leading past it
        // (Chrome: 72px img in a 16px-Arial div → 76px box; 82px with
        // line-height:30px). The stub shaper's rounded metrics at 16px are
        // ascent 13 / descent 3; with line-height:30px the leading is
        // 30 − 16 = 14, splitting 7 above / 7 below → 3 + 7 = 10px under the
        // image: the box must be 82px tall.
        let with_lh = lay(
            "<div style='background:#ff0000;font-size:16px;line-height:30px;margin:0'>\
             <img src='x.png' width=72 height=72></div>",
            400,
        );
        let box_h = |laid: &LaidOut| {
            fill_rects(laid)
                .iter()
                .map(|r| r.h)
                .max()
                .expect("div paints a background")
        };
        assert_eq!(box_h(&with_lh), 82, "descent 3 + below-leading 7");
        // vertical-align: middle takes the image off the baseline — no strut
        // descent (Chrome reports exactly the image height).
        let middle = lay(
            "<div style='background:#ff0000;font-size:16px;line-height:30px;margin:0'>\
             <img src='x.png' width=72 height=72 style='vertical-align:middle'></div>",
            400,
        );
        assert_eq!(box_h(&middle), 72, "off-baseline image: no descent");
        // display:block has no line box at all.
        let block = lay(
            "<div style='background:#ff0000;font-size:16px;line-height:30px;margin:0'>\
             <img src='x.png' width=72 height=72 style='display:block'></div>",
            400,
        );
        assert_eq!(box_h(&block), 72, "block image: no strut");
    }

    #[test]
    fn flex_and_grid_lay_out_bare_text_children_as_anonymous_items() {
        // CSS Flexbox §4 / Grid §6.1: contiguous text directly inside a flex or
        // grid container wraps in an anonymous item. These were silently
        // dropped (mozilla.org's nav menu titles are `<div
        // style=display:flex>Products<svg…>` — the word vanished).
        let count_glyph_items = |laid: &LaidOut| {
            laid.display
                .items
                .iter()
                .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
                .count()
        };
        let flex = lay(
            "<div style='display:flex'>Products<span>About</span></div>",
            400,
        );
        assert!(
            count_glyph_items(&flex) >= 2,
            "flex: both the bare text and the span render"
        );
        let grid = lay(
            "<div style='display:grid;grid-template-columns:1fr 1fr'>\
             Cell-text<span>Elem</span></div>",
            400,
        );
        assert!(
            count_glyph_items(&grid) >= 2,
            "grid: both the bare text and the span render"
        );
    }

    #[test]
    fn whitespace_state_crosses_inline_boundaries() {
        // #137: spacing must come from the SOURCE text, not the x-position
        // heuristic. An inline element boundary adds nothing by itself, so
        // `<a>RFC 6761</a>, a` must render pixel-identically to the plain text
        // `RFC 6761, a` (no phantom space before the comma), and a real space
        // before a nowrap span must survive the atomic-run fast path.
        let glyph_xs = |laid: &LaidOut| {
            let mut xs: Vec<i32> = laid
                .display
                .items
                .iter()
                .filter_map(|i| match i {
                    DisplayItem::Glyphs { origin, .. } => Some(origin.x),
                    _ => None,
                })
                .collect();
            xs.sort_unstable();
            xs
        };
        let linked = lay("<p style='margin:0'><a href='#x'>RFC 6761</a>, a</p>", 600);
        let plain = lay("<p style='margin:0'>RFC 6761, a</p>", 600);
        // The linked version splits into more pieces, but every piece must sit
        // where the plain text's words sit: the union of x-origins of the
        // plain render must be a subset of the linked one at identical x.
        let (lx, px) = (glyph_xs(&linked), glyph_xs(&plain));
        assert_eq!(
            lx.first(),
            px.first(),
            "first word starts at the same x: {lx:?} vs {px:?}"
        );
        assert_eq!(
            lx.last(),
            px.last(),
            "no phantom space shifts the tail: {lx:?} vs {px:?}"
        );

        let nowrap = lay(
            "<p style='margin:0'>by <span style='white-space:nowrap'>Public</span></p>",
            600,
        );
        let nowrap_plain = lay("<p style='margin:0'>by Public</p>", 600);
        assert_eq!(
            glyph_xs(&nowrap).last(),
            glyph_xs(&nowrap_plain).last(),
            "the space before a nowrap span is kept (was eaten: 'byPublic')"
        );
    }

    #[test]
    fn multi_word_link_underline_is_continuous() {
        // #137 facet: Chrome underlines the whole anchor including inter-word
        // gaps; per-word rules left gaps ('RFC 2606' underlined only '2606').
        let laid = lay("<p style='margin:0'><a href='#x'>two words</a></p>", 600);
        let mut rules: Vec<Rect> = laid
            .display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Rect { rect, .. } if rect.h == 1 => Some(*rect),
                _ => None,
            })
            .collect();
        rules.sort_by_key(|r| r.x);
        assert_eq!(rules.len(), 2, "one rule per piece: {rules:?}");
        assert_eq!(
            rules[0].x + rules[0].w as i32,
            rules[1].x,
            "first word's rule extends across the gap to the second: {rules:?}"
        );
    }

    #[test]
    fn center_does_not_recenter_tables_inside_cells() {
        // The HN shell: `<center><table width=85%>` centers the OUTER table
        // box, but an auto-width table nested inside one of its cells stays at
        // the cell's LEFT edge in the reference — inherited -webkit-center
        // must not cross the cell boundary (it displaced HN's item list
        // ~187px right).
        let laid = lay(
            "<center><table width='500'><tr><td>\
             <table><tr><td>inner</td></tr></table>\
             </td></tr></table></center>",
            600,
        );
        let min_glyph_x = laid
            .display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Glyphs { origin, .. } => Some(origin.x),
                _ => None,
            })
            .min()
            .expect("inner text renders");
        // Outer table: centered 500px box in 600px → starts ~x=50; the inner
        // table must hug the cell's left padding (well under the ~300px a
        // re-centering would produce).
        assert!(
            min_glyph_x < 120,
            "inner table stays at the cell's left edge, got x={min_glyph_x}"
        );
    }

    #[test]
    fn grid_explicit_line_placement_anchors_columns() {
        // `grid-column: 3 / 5` anchors at track 3 (index 2) spanning 2 tracks;
        // `1 / -1` spans the whole explicit grid. Auto-placement put both in
        // the first free cell — mozilla's hero text (grid-column: 2/9 of 12)
        // sat a full track off.
        let laid = lay(
            "<div style='display:grid;grid-template-columns:100px 100px 100px 100px'>\
             <div style='grid-column:3/5;background:#ff0000'>a</div>\
             <div style='grid-column:1/-1;background:#00ff00'>b</div>\
             </div>",
            420,
        );
        let rects = fill_rects(&laid);
        let red = rects.iter().find(|r| r.w > 150 && r.w < 260).copied();
        let green = rects.iter().find(|r| r.w > 350).copied();
        let red = red.expect("3/5 spans two 100px tracks");
        assert!(
            red.x >= 200,
            "anchored at line 3 (x≥200 with two tracks before it): {red:?}"
        );
        let green = green.expect("1/-1 spans all four tracks");
        assert!(green.x <= 8, "full-bleed row starts at the left: {green:?}");
    }

    #[test]
    fn table_row_heights_follow_cells_spacers_and_margins() {
        // Three HN-measured behaviors in one table: (1) a cell-less spacer row
        // contributes its declared height; (2) a small-font row is its own
        // content height, not the table font's line; (3) a last child's
        // bottom margin is contained by its cell.
        let spaced = lay(
            "<table><tr><td>a</td></tr>\
             <tr style='height:20px'></tr>\
             <tr><td>b</td></tr></table>",
            400,
        );
        let flat = lay(
            "<table><tr><td>a</td></tr>\
             <tr><td>b</td></tr></table>",
            400,
        );
        let ys = |laid: &LaidOut| {
            let mut v = glyph_ys(laid);
            v.sort_unstable();
            v
        };
        let (sy, fy) = (ys(&spaced), ys(&flat));
        assert_eq!(
            sy[1] - sy[0],
            (fy[1] - fy[0]) + 20,
            "spacer row adds exactly its 20px: {sy:?} vs {fy:?}"
        );

        // Small-font row: the gap between two 10px-font rows is smaller than
        // between two default(16px)-font rows in the same table.
        let small = lay(
            "<table style='font-size:16px'>\
             <tr><td style='font-size:10px'>a</td></tr>\
             <tr><td style='font-size:10px'>b</td></tr></table>",
            400,
        );
        let big = lay(
            "<table style='font-size:16px'><tr><td>a</td></tr><tr><td>b</td></tr></table>",
            400,
        );
        assert!(
            ys(&small)[1] - ys(&small)[0] < ys(&big)[1] - ys(&big)[0],
            "a small-print row is shorter than the table font's line"
        );

        // Bottom margin of a cell's last block child extends the row.
        let margined = lay(
            "<table><tr><td><div style='margin:0 0 6px 0'>a</div></td></tr>\
             <tr><td>b</td></tr></table>",
            400,
        );
        let plain = lay(
            "<table><tr><td><div style='margin:0'>a</div></td></tr>\
             <tr><td>b</td></tr></table>",
            400,
        );
        assert_eq!(
            ys(&margined)[1] - ys(&margined)[0],
            (ys(&plain)[1] - ys(&plain)[0]) + 6,
            "trailing cell margin contained (adds 6px to the row)"
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
    fn ol_start_attribute_seeds_the_first_ordinal() {
        // `<ol start="10">` numbers its first item 10, so its "10." marker has one
        // more glyph than the default "1."; a plain `<ol>` and non-numeric starts
        // are unaffected.
        let default_ol = total_glyphs(&lay("<ol><li>x</li></ol>", 400));
        let started = total_glyphs(&lay("<ol start='10'><li>x</li></ol>", 400));
        assert_eq!(started - default_ol, 1, "'10.' adds one glyph over '1.'");

        // The count continues from the seed: `start=9` → "9." then "10." markers
        // (2 + 3 glyphs) plus the two item letters = 7.
        let two = total_glyphs(&lay("<ol start='9'><li>x</li><li>y</li></ol>", 400));
        assert_eq!(two, 7, "9. then 10. plus two content letters");

        // `start` on a `<ul>` (or a non-numeric value) is ignored.
        let ul = total_glyphs(&lay("<ul start='9'><li>x</li></ul>", 400));
        let ul_plain = total_glyphs(&lay("<ul><li>x</li></ul>", 400));
        assert_eq!(ul, ul_plain, "start has no effect on an unordered list");
    }

    #[test]
    fn alpha_and_roman_ordinals_are_correct() {
        assert_eq!(alpha_ordinal(1, false), "a");
        assert_eq!(alpha_ordinal(26, false), "z");
        assert_eq!(alpha_ordinal(27, false), "aa");
        assert_eq!(alpha_ordinal(28, false), "ab");
        assert_eq!(alpha_ordinal(53, false), "ba");
        assert_eq!(alpha_ordinal(1, true), "A");
        assert_eq!(alpha_ordinal(702, true), "ZZ"); // 26*26 + 26

        assert_eq!(roman_ordinal(1, false), "i");
        assert_eq!(roman_ordinal(4, false), "iv");
        assert_eq!(roman_ordinal(9, false), "ix");
        assert_eq!(roman_ordinal(40, false), "xl");
        assert_eq!(roman_ordinal(2024, false), "mmxxiv");
        assert_eq!(roman_ordinal(3999, false), "mmmcmxcix");
        assert_eq!(roman_ordinal(4, true), "IV");
        // Out of the classic range → decimal fallback (still monotonic).
        assert_eq!(roman_ordinal(4000, false), "4000");
    }

    #[test]
    fn li_value_overrides_and_continues_the_ordinal() {
        // `<li value="10">` sets that item to 10 and the next continues at 11:
        // markers 1. (2) + 10. (3) + 11. (3) plus three letters = 11 glyphs.
        let jumped = total_glyphs(&lay(
            "<ol><li>x</li><li value='10'>y</li><li>z</li></ol>",
            400,
        ));
        assert_eq!(jumped, 11, "1. then 10. then 11. plus x y z");
        // Without the override the markers are 1. 2. 3. (2 glyphs each): 9 total.
        let plain = total_glyphs(&lay("<ol><li>x</li><li>y</li><li>z</li></ol>", 400));
        assert_eq!(plain, 9);
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
    fn inline_block_in_absolute_cell_gets_the_cell_width() {
        // An inline-block inside an absolute cell (only `right` set + explicit
        // width) must see the cell's content width, not a collapsed ~1px box —
        // else its text wraps though it fits. This is Wikipedia's count
        // ("N articles", a `display:inline-block` <small>) in a 156px lang cell.
        let laid = lay(
            "<div style='position:relative;width:600px;height:300px;text-align:center'>\
               <div style='position:absolute;right:60%;width:200px'>\
                 <a style='display:block'>\
                   <strong style='display:block'>Name</strong>\
                   <span style='display:inline-block'>a b c</span>\
                 </a>\
               </div>\
             </div>",
            800,
        );
        // Two lines expected: the block <strong> and the inline-block below it —
        // NOT a third line from the inline-block's text wrapping in a collapsed box.
        let ys: std::collections::BTreeSet<i32> =
            glyph_xy(&laid).into_iter().map(|(_, y)| y).collect();
        assert_eq!(
            ys.len(),
            2,
            "strong on line 1, inline-block text on one line below: {ys:?}"
        );
    }

    #[test]
    fn absolutely_positioned_img_resolves_against_its_ancestor() {
        // An absolute <img> honors top/left against its positioned ancestor,
        // instead of being laid in normal flow with its insets ignored (the
        // Wikipedia-globe bug: position:absolute;top:158px inside a relative box).
        let styled = CssEngine::new().style(&parse_html(
            "<div style='height:100px'>spacer</div>\
             <div style='position:relative'>\
               <img src='g.png' style='position:absolute;top:40px;left:20px'>\
             </div>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(30, 30),
            rgba: vec![255; 30 * 30 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(400, 2000),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
        );
        let rect = laid
            .display
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .expect("image emitted");
        // Relative container sits at y≈100 (100px spacer; the fragment has no
        // <body>, so no UA body margin applies); top:40 → ~140, left:20 → ~20.
        // Without the fix the image would land at the flow origin (~top of the
        // relative box).
        assert!(
            (132..=150).contains(&rect.y),
            "img at ancestor.y + top:40px, got y={}",
            rect.y
        );
        assert!(
            (16..=26).contains(&rect.x),
            "img at left:20px, got x={}",
            rect.x
        );
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
        // `src` is the implicit 1x candidate when srcset lists only higher
        // densities (Wikipedia's globe: src=v2.png srcset="v2@2x.png 2x").
        assert_eq!(
            select_srcset_with_src("v2@2x.png 2x", None, 1000, Some("v2.png")).as_deref(),
            Some("v2.png"),
            "at DPR 1, plain src (1x) beats the only srcset entry (2x)"
        );
        // `src` does not override width-descriptor selection.
        assert_eq!(
            select_srcset_with_src("s.png 480w, l.png 1200w", None, 400, Some("orig.png"))
                .as_deref(),
            Some("s.png"),
            "width mode ignores src"
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

    #[test]
    fn image_type_supported_matches_bundled_codecs() {
        for ok in [
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "image/bmp",
            "image/svg+xml",
            "IMAGE/PNG",
        ] {
            assert!(image_type_supported(ok), "{ok} should decode");
        }
        for no in ["image/avif", "image/jxl", "image/tiff", "video/mp4", ""] {
            assert!(!image_type_supported(no), "{no} should not decode");
        }
    }

    #[test]
    fn picture_media_matches_dimensions_and_orientation() {
        // width features against a 500-wide viewport
        assert!(picture_media_matches("(max-width: 600px)", 500, 800));
        assert!(!picture_media_matches("(max-width: 400px)", 500, 800));
        assert!(picture_media_matches("(min-width: 500px)", 500, 800));
        // AND / OR, media type, and orientation
        assert!(picture_media_matches(
            "screen and (min-width: 400px)",
            500,
            800
        ));
        assert!(!picture_media_matches(
            "print and (min-width: 400px)",
            500,
            800
        ));
        assert!(picture_media_matches(
            "(max-width: 100px), (min-width: 400px)",
            500,
            800
        ));
        assert!(picture_media_matches("(orientation: portrait)", 500, 800));
        assert!(!picture_media_matches("(orientation: landscape)", 500, 800));
        // Empty query matches everything; unknown/preference features don't.
        assert!(picture_media_matches("", 500, 800));
        assert!(!picture_media_matches(
            "(prefers-color-scheme: dark)",
            500,
            800
        ));
    }

    #[test]
    fn pick_picture_url_skips_undecodable_types_then_falls_back() {
        let img = |n: &str| match n {
            "src" => Some("fallback.jpg"),
            _ => None,
        };
        // AVIF can't decode → skip; WebP can → win.
        let sources = [
            PictureSource {
                type_: Some("image/avif"),
                media: None,
                srcset: Some("a.avif"),
                sizes: None,
            },
            PictureSource {
                type_: Some("image/webp"),
                media: None,
                srcset: Some("a.webp"),
                sizes: None,
            },
        ];
        assert_eq!(
            pick_picture_url(&sources, img, 800, 600).as_deref(),
            Some("a.webp")
        );
        // Only an undecodable source → fall back to the <img>.
        let only_avif = [PictureSource {
            type_: Some("image/avif"),
            media: None,
            srcset: Some("a.avif"),
            sizes: None,
        }];
        assert_eq!(
            pick_picture_url(&only_avif, img, 800, 600).as_deref(),
            Some("fallback.jpg")
        );
    }

    #[test]
    fn picture_source_selected_by_media_is_drawn() {
        // A narrow (500px) viewport → the (max-width:600px) mobile source wins;
        // the provider serves only that key, so an Image item proves selection.
        let styled = CssEngine::new().style(&parse_html(
            "<picture>\
               <source media='(min-width: 900px)' srcset='desktop.png'>\
               <source media='(max-width: 600px)' srcset='mobile.png'>\
               <img src='fallback.png' alt='hero'>\
             </picture>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(500, 2000),
            &MonoShaper,
            &KeyedImage("mobile.png", img),
            &NoForms,
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "the (max-width:600px) source (mobile.png) was selected and drawn"
        );
    }

    #[test]
    fn picture_falls_back_to_img_when_no_source_matches() {
        // A wide (1000px) viewport: neither the undecodable AVIF nor the
        // max-width:600 source qualifies → the <img> fallback is drawn.
        let styled = CssEngine::new().style(&parse_html(
            "<picture>\
               <source type='image/avif' srcset='a.avif'>\
               <source media='(max-width: 600px)' srcset='mobile.png'>\
               <img src='fallback.png' alt='hero'>\
             </picture>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(1000, 2000),
            &MonoShaper,
            &KeyedImage("fallback.png", img),
            &NoForms,
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "no source qualified, so the <img> fallback (fallback.png) was drawn"
        );
    }

    #[test]
    fn picture_honors_the_inner_img_display_none() {
        // A display:none <img> inside a <picture> paints nothing, exactly as a
        // bare display:none <img> would (the picture arm must not override the
        // image's own box suppression).
        let styled = CssEngine::new().style(&parse_html(
            "<picture>\
               <source srcset='mobile.png'>\
               <img src='fallback.png' style='display:none' alt='hero'>\
             </picture>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(500, 2000),
            &MonoShaper,
            &OneImage(img),
            &NoForms,
        );
        assert!(
            !laid
                .display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "a display:none <img> in a <picture> draws nothing"
        );
    }

    #[test]
    fn picture_without_a_direct_img_still_renders_nested_content() {
        // Invalid nesting (<img> under a <figure> inside <picture>): with no
        // direct <img>, the picture arm falls through to normal container layout
        // so the nested image still draws, matching browsers (and the fetch
        // collector, which likewise falls through to collect it).
        let styled = CssEngine::new().style(&parse_html(
            "<picture><figure><img src='nested.png' alt='x'></figure></picture>",
        ));
        let img = Arc::new(DecodedImage {
            size: Size::new(20, 10),
            rgba: vec![255; 20 * 10 * 4],
        });
        let laid = BlockLayout::default().layout(
            &styled,
            Size::new(800, 2000),
            &MonoShaper,
            &KeyedImage("nested.png", img),
            &NoForms,
        );
        assert!(
            laid.display
                .items
                .iter()
                .any(|i| matches!(i, DisplayItem::Image { .. })),
            "the nested <img> renders even though it is not a direct <picture> child"
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
    fn css_width_height_size_the_image() {
        // A stylesheet rule sizes a replaced element. Regression: this was
        // ignored, so the image rendered at its full 400×300 intrinsic size.
        assert_eq!(
            img_box(
                "<style>img{width:60px;height:40px}</style><img src='p.png'>",
                Size::new(400, 300),
                800
            ),
            (60, 40)
        );
    }

    #[test]
    fn css_width_only_derives_height_from_ratio() {
        // CSS width alone → height follows the intrinsic 4:3 ratio.
        assert_eq!(
            img_box(
                "<style>img{width:200px}</style><img src='p.png'>",
                Size::new(400, 300),
                800
            ),
            (200, 150)
        );
    }

    #[test]
    fn css_width_overrides_the_presentational_size_attribute() {
        // CSS `width` (a real property) beats the `width` attribute (a
        // presentational hint); the untouched `height` attribute still applies.
        assert_eq!(
            img_box(
                "<style>img{width:100px}</style><img src='p.png' width='200' height='150'>",
                Size::new(400, 300),
                800
            ),
            (100, 150)
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
        // derivation. (No engine page margin: the content area IS the container.)
        assert_eq!(
            img_box("<img src='p.png' width='600'>", Size::new(400, 300), 500),
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
        assert_eq!(bg.x, 0, "bg starts at the content edge (no engine margin)");
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
    fn inline_horizontal_padding_spaces_surrounding_content() {
        // A true inline element's horizontal padding (and margin) pushes the
        // content after it rightward — the spacing that separates nav links like
        // iana.org's `#header .navigation li a { padding:4px 6px }`. Without it,
        // styled inline links render run-together.
        let padded = lay("<p>A<a style='padding:0 10px;margin:0 5px'>B</a>C</p>", 400);
        let plain = lay("<p>A<a>B</a>C</p>", 400);
        let last_x = |l: &LaidOut| *glyph_xs(l).iter().max().unwrap();
        // 'C' (the rightmost glyph) sits farther right with the inline box's
        // 10px+10px padding and 5px+5px margins around 'B'.
        assert!(
            last_x(&padded) >= last_x(&plain) + 28,
            "inline padding+margin pushed following content right: {} vs {}",
            last_x(&padded),
            last_x(&plain)
        );
    }

    #[test]
    fn inline_block_margins_advance_and_pull_back_the_cursor() {
        // Rects (left edges), in document order, of two inline-block boxes.
        fn rect_lefts(laid: &LaidOut) -> Vec<i32> {
            laid.display
                .items
                .iter()
                .filter_map(|i| match i {
                    DisplayItem::Rect { rect, .. } => Some(rect.x),
                    _ => None,
                })
                .collect()
        }
        // A positive right margin on the first box spaces the second box away.
        let spaced = lay(
            "<div><span style='display:inline-block;width:30px;height:10px;\
background:#111;margin-right:40px'></span>\
<span style='display:inline-block;width:30px;height:10px;background:#222'></span></div>",
            400,
        );
        let l = rect_lefts(&spaced);
        assert_eq!(l.len(), 2, "two inline-block backgrounds: {l:?}");
        // second.left = first.left(0) + width(30) + margin-right(40) = 70.
        assert_eq!(l[1] - l[0], 70, "positive right margin spaces the next box");

        // A negative right margin pulls the following box back to overlap.
        let overlap = lay(
            "<div><span style='display:inline-block;width:30px;height:10px;\
background:#111;margin-right:-10px'></span>\
<span style='display:inline-block;width:30px;height:10px;background:#222'></span></div>",
            400,
        );
        let o = rect_lefts(&overlap);
        assert_eq!(
            o[1] - o[0],
            20,
            "negative right margin overlaps the next box"
        );
    }

    #[test]
    fn text_indent_hides_a_nowrap_run_off_screen() {
        // The image-replacement trick: `text-indent:-9999px` on a `white-space:
        // nowrap` element (e.g. a `.pure-button` whose icon replaces its label)
        // pushes the fallback text off-screen so only the sprite shows. A nowrap
        // run is laid atomically, so it must honor text-indent like a wrapped
        // word does — otherwise the label paints on top of the icon.
        let hidden = lay(
            "<div style='white-space:nowrap;text-indent:-9999px'>Search</div>",
            400,
        );
        // Every glyph is shoved far to the left, off the visible canvas.
        assert!(
            glyph_xs(&hidden).iter().all(|&x| x < 0),
            "nowrap run honors text-indent: {:?}",
            glyph_xs(&hidden)
        );
        // Without the indent the same run sits at the content edge.
        let shown = lay("<div style='white-space:nowrap'>Search</div>", 400);
        assert!(glyph_xs(&shown).iter().all(|&x| x >= 0));
    }

    #[test]
    fn absolute_child_positions_against_a_relative_inline_block() {
        // An `absolute` element inside a `position:relative` inline-block uses
        // that inline-block as its containing block (Wikipedia's language dropdown
        // pinned to the right edge of the search input): a `right`-anchored child
        // lands on the right side of the 200px box, not at the flow's left edge.
        let laid = lay(
            "<div style='display:inline-block;position:relative;width:200px'>\
             <div style='position:absolute;top:0;right:4px;width:20px'>Z</div></div>",
            400,
        );
        let z_x = *glyph_xs(&laid).iter().max().unwrap();
        assert!(
            z_x > 150,
            "right-anchored absolute child sits near the box's right edge (~176), got x={z_x}"
        );
    }

    #[test]
    fn percent_width_inline_block_does_not_resolve_twice() {
        // An inline-block `width:50%` in a 400px container is 200px, and a
        // `width:100%` child fills that 200px — not 50%-of-50% (100px). Regression:
        // the percentage was applied once to size the atom's sub and again inside
        // it, collapsing Wikipedia's `.search-input{width:73%}` search field.
        let laid = lay(
            "<div style='width:400px'>\
             <span style='display:inline-block;width:50%'>\
             <div style='width:100%;background:#123456;height:10px'></div></span></div>",
            400,
        );
        let filled = laid.display.items.iter().any(|it| {
            matches!(
                it,
                DisplayItem::Rect { rect, color }
                    if *color == Color::rgb(0x12, 0x34, 0x56) && (190..=210).contains(&rect.w)
            )
        });
        assert!(
            filled,
            "inner width:100% fills the 200px atom, not 100px: {:?}",
            laid.display
                .items
                .iter()
                .filter_map(|it| match it {
                    DisplayItem::Rect { rect, color } if *color == Color::rgb(0x12, 0x34, 0x56) =>
                        Some(rect.w),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn min_width_floors_but_does_not_pin_a_shrink_to_fit_box() {
        // An icon-only button (Wikipedia search) sizes to its content — an
        // inline-block icon plus padding — even though `.pure-button` sets a
        // small `min-width`. min-width is a floor, not a fixed width: it must
        // not suppress content measurement and collapse the box.
        let html = "<style>.ib{display:inline-block;width:22px;height:22px}\
b{display:inline-block;min-width:16px;padding:8px 16px;box-sizing:border-box;\
background-color:#0645ad}</style><b><i class=\"ib\">x</i></b>";
        let laid = lay(html, 400);
        // 22px icon + 2*16px padding = 54px, well above the 16px min-width.
        let wide = laid.display.items.iter().any(|it| {
            matches!(
            it, DisplayItem::Rect { rect, .. } if rect.w >= 50 && rect.w <= 60)
        });
        assert!(
            wide,
            "shrink-to-fit box sizes to content (~54px), not min-width"
        );
    }

    #[test]
    fn button_with_element_children_lays_them_out_and_stays_a_button() {
        // Wikipedia's language/search buttons wrap icon `<i>` sprites and a
        // `<span>` label rather than bare text. Such a button is laid via the
        // block box model so its children paint, and still registers as a
        // clickable Button hit box. (Regression: routing the children through
        // `add_inline_block` used to recurse back into `form_button` during
        // intrinsic-width measurement and overflow the stack.)
        let laid = lay("<button><i></i><span>Read this</span><i></i></button>", 400);
        assert_eq!(total_glyphs(&laid), 8, "child span's 'Read this' laid out");
        let buttons: Vec<_> = laid
            .fields
            .iter()
            .filter(|f| f.kind == FieldKind::Button)
            .collect();
        assert_eq!(buttons.len(), 1, "one Button hit box registered");
        assert!(buttons[0].rect.w > 1, "hit box spans the laid content");
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
    fn pre_wrap_and_pre_line_wrap_but_pre_does_not() {
        // Long text in a narrow box: `pre` (from <pre>) never wraps, so it stays
        // on one line; `pre-wrap` and `pre-line` both wrap on overflow.
        let text = "aaaa bbbb cccc dddd eeee ffff";
        let pre = lay(&format!("<pre>{text}</pre>"), 60);
        assert_eq!(
            distinct(&glyph_ys(&pre)),
            1,
            "pre never wraps: {:?}",
            glyph_ys(&pre)
        );
        let pre_wrap = lay(&format!("<p style='white-space:pre-wrap'>{text}</p>"), 60);
        assert!(
            distinct(&glyph_ys(&pre_wrap)) > 1,
            "pre-wrap wraps on overflow: {:?}",
            glyph_ys(&pre_wrap)
        );
        let pre_line = lay(&format!("<p style='white-space:pre-line'>{text}</p>"), 60);
        assert!(
            distinct(&glyph_ys(&pre_line)) > 1,
            "pre-line wraps on overflow: {:?}",
            glyph_ys(&pre_line)
        );
    }

    #[test]
    fn pre_preserves_newlines_and_spaces() {
        // `<pre>` is a whitespace-preserving context (parser keeps its raw text),
        // so explicit newlines become hard breaks — unlike `normal`, which
        // collapses them onto one row.
        let pre = lay("<pre>ab\n    cd</pre>", 400);
        assert_eq!(
            distinct(&glyph_ys(&pre)),
            2,
            "newline makes two rows: {:?}",
            glyph_ys(&pre)
        );
        let norm = lay("<p>ab\n    cd</p>", 400);
        assert_eq!(
            distinct(&glyph_ys(&norm)),
            1,
            "normal collapses to one line"
        );

        // Space runs survive verbatim in `<pre>` (each space is a laid glyph in
        // the atomic run), where `normal` collapses the run and renders the single
        // inter-word space as an advance, not a glyph: "a    b" → 6 glyphs (a, four
        // spaces, b) under `pre` vs just 2 (a, b) under `normal`.
        let pre_sp = lay("<pre>a    b</pre>", 400);
        let norm_sp = lay("<p>a    b</p>", 400);
        assert_eq!(total_glyphs(&pre_sp), 6, "pre keeps the four spaces");
        assert_eq!(
            total_glyphs(&norm_sp),
            2,
            "normal collapses inter-word space"
        );
    }

    #[test]
    fn nbsp_does_not_break_the_line() {
        // A regular space between two words is a wrap opportunity, so in a narrow
        // box the words fall onto separate lines. A non-breaking space (`&nbsp;`)
        // holds them together on one line (overflowing rather than wrapping).
        let spaced = lay("<p>aaaa bbbb</p>", 60);
        assert!(
            distinct(&glyph_ys(&spaced)) > 1,
            "a normal space lets the words wrap: {:?}",
            glyph_ys(&spaced)
        );
        let nbsp = lay("<p>aaaa&nbsp;bbbb</p>", 60);
        assert_eq!(
            distinct(&glyph_ys(&nbsp)),
            1,
            "nbsp keeps both words on one line: {:?}",
            glyph_ys(&nbsp)
        );
    }

    #[test]
    fn inherited_unitless_line_height_scales_to_child_font() {
        // A unitless line-height on the ancestor scales to the child's *own* font
        // size: the child's wrapped rows are spaced factor × child-font-size
        // apart, not factor × ancestor-font-size.
        let laid = lay(
            "<div style='line-height:3;font-size:10px'>\
             <p style='font-size:20px'>aaaa bbbb cccc</p></div>",
            100,
        );
        let mut rows = glyph_ys(&laid);
        rows.sort_unstable();
        rows.dedup();
        assert!(rows.len() >= 2, "text wraps to multiple rows: {rows:?}");
        for pair in rows.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                60,
                "row spacing is 3 * 20px (the child's font), not 3 * 10px: {rows:?}"
            );
        }
    }

    #[test]
    fn margin_right_shrinks_the_auto_width_box() {
        // A non-auto `margin-right` narrows an auto-width block, so the same text
        // wraps into more rows than with no right margin. Both share the same left
        // edge, so this isolates the right-margin effect.
        let text = "aa bb cc dd ee ff gg hh";
        let plain = lay(&format!("<div>{text}</div>"), 200);
        let margined = lay(
            &format!("<div style='margin-right:140px'>{text}</div>"),
            200,
        );
        assert!(
            distinct(&glyph_ys(&margined)) > distinct(&glyph_ys(&plain)),
            "margin-right narrows the box so text wraps more: plain {:?} vs margined {:?}",
            glyph_ys(&plain),
            glyph_ys(&margined)
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
    fn sub_and_sup_shift_the_baseline() {
        // `<sup>` lifts its run above the surrounding text (smaller y); `<sub>`
        // drops it below. The base "x" piece is emitted before the shifted "2",
        // so ys[0] is the base line and ys[1] is the sup/sub run.
        let sup = lay("<p>x<sup>2</sup></p>", 400);
        let sup_ys = glyph_ys(&sup);
        assert_eq!(sup_ys.len(), 2, "base + superscript pieces");
        assert!(
            sup_ys[1] < sup_ys[0],
            "superscript ({}) sits above the baseline ({})",
            sup_ys[1],
            sup_ys[0]
        );

        let sub = lay("<p>x<sub>2</sub></p>", 400);
        let sub_ys = glyph_ys(&sub);
        assert_eq!(sub_ys.len(), 2, "base + subscript pieces");
        assert!(
            sub_ys[1] > sub_ys[0],
            "subscript ({}) sits below the baseline ({})",
            sub_ys[1],
            sub_ys[0]
        );

        // A plain inline run with no vertical-align keeps both pieces on one line.
        let flat = lay("<p>x<span>2</span></p>", 400);
        assert_eq!(distinct(&glyph_ys(&flat)), 1, "no shift without sub/sup");
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
        // A 2x2 grid of bordered <td> cells (border="1"). Each cell draws a hollow
        // 1px border made of four rects, so a table emits many more rects than the
        // plain-text baseline (which emits none).
        let baseline = lay("<p>aa bb cc dd</p>", 400);
        let laid = lay(
            "<table border=\"1\"><tr><td>aa</td><td>bb</td></tr>\
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
        let one = lay("<table border=\"1\"><tr><td>x</td></tr></table>", 400);
        assert_eq!(total_glyphs(&one), 1, "single-cell table lays out its text");
        assert!(rect_count(&one) >= 4, "single cell has a four-rect border");
    }

    #[test]
    fn borderless_table_draws_no_grid_lines() {
        // Without a `border` attribute (or `border="0"`) a table is a layout grid:
        // no cell rules, matching Chrome. Only text (and any fills) are painted.
        let none = lay("<table><tr><td>a</td><td>b</td></tr></table>", 400);
        let zero = lay(
            "<table border=\"0\"><tr><td>a</td><td>b</td></tr></table>",
            400,
        );
        assert_eq!(rect_count(&none), 0, "no border attr → no grid rects");
        assert_eq!(rect_count(&zero), 0, "border=0 → no grid rects");
        assert_eq!(total_glyphs(&none), 2, "cell text still laid out");
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

        // A large negative text-indent (the image-replacement trick) is honored,
        // pushing the text far off the left edge instead of being clamped to 0.
        let hidden = lay("<p style='text-indent:-9999px'>hello world</p>", 400);
        assert!(
            min_x(&hidden) <= -9000,
            "negative text-indent pushes text off-screen, got {}",
            min_x(&hidden)
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
    fn absolute_element_honors_its_explicit_width() {
        // An out-of-flow box with an explicit `width` uses it (not shrink-to-fit),
        // even with only one inset set — so its text wraps at that width, not at
        // its intrinsic content width. Wikipedia's `.central-featured-lang` sets
        // width:15.6rem with only `right`, and its count line must not wrap.
        let laid = lay(
            "<div style='position:relative;height:300px'>\
               <div style='position:absolute;top:0;right:0;width:250px'>\
                 one two three four five six</div>\
             </div>",
            600,
        );
        // With a 250px box the phrase fits on one line; a shrunk box would wrap it.
        let ys: std::collections::BTreeSet<i32> =
            glyph_xy(&laid).into_iter().map(|(_, y)| y).collect();
        assert_eq!(
            ys.len(),
            1,
            "explicit-width box keeps the run on one line: {ys:?}"
        );
    }

    #[test]
    fn percent_top_resolves_against_the_containing_blocks_explicit_height() {
        // A `top:%` absolute child resolves against its positioned ancestor's own
        // height when that height is explicit — NOT the viewport height. This is
        // Wikipedia's portal pattern (`.central-featured{height:32.5rem}` with
        // languages at `top:20%…80%`). With a 200px-tall relative box, top:50%
        // must land at ~100px, well short of the 600px viewport's 50% (=300px).
        let laid = lay(
            "<div style='position:relative;height:200px'>\
               <div style='position:absolute;top:50%;left:0'>X</div>\
             </div>",
            400,
        );
        let (_, y) = *glyph_xy(&laid).last().unwrap();
        assert!(
            (80..=140).contains(&y),
            "top:50% of the 200px container (~100px), not of the viewport: {y}"
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

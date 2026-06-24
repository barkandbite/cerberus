//! Neutral style types and the `StyleEngine` seam.
//!
//! This crate holds the *result* of styling — `ComputedStyle` and a `StyledDom`
//! tree — plus the `StyleEngine` trait. The actual CSS parsing/cascade lives in
//! an adapter (`cerberus-css`) behind this trait, so it can be swapped or
//! reimplemented without touching layout. Layout consumes only these types.

use cerberus_dom::{Document, NodeId};
use cerberus_types::{Color, FontStyle, ImageFit, ImagePos};

/// CSS `position`. `Static` is normal flow; the rest are positioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
    /// Parsed but laid out as normal flow until scroll containers exist (v1).
    Sticky,
}

/// A CSS inset value for `top`/`right`/`bottom`/`left`: `auto`, a px length, or a
/// percentage of the containing block (resolved at layout).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Len {
    #[default]
    Auto,
    Px(i32),
    Pct(f32),
    /// Viewport-relative: percent of viewport width / height (ADR-0042).
    Vw(f32),
    Vh(f32),
    /// `max-content` / `fit-content`: size to the content's max-content extent,
    /// resolved at layout where intrinsic measurement is available (ADR-0055).
    MaxContent,
    /// `min-content`: size to the content's min-content extent (ADR-0055).
    MinContent,
}

impl Len {
    /// Whether this is an intrinsic keyword (`max-content`/`min-content`) that
    /// must be resolved by measuring the content rather than the containing block.
    pub fn is_intrinsic(self) -> bool {
        matches!(self, Len::MaxContent | Len::MinContent)
    }

    /// Resolve against a containing-block `extent` in px; `auto`/viewport/intrinsic
    /// → `None` (use [`Len::resolve_vp`] when the viewport is known).
    pub fn resolve(self, extent: i32) -> Option<i32> {
        match self {
            Len::Px(p) => Some(p),
            Len::Pct(f) => Some((f / 100.0 * extent as f32).round() as i32),
            Len::Auto | Len::Vw(_) | Len::Vh(_) | Len::MaxContent | Len::MinContent => None,
        }
    }

    /// Resolve including viewport units (`vw`/`vh`) against `vw`×`vh` px.
    pub fn resolve_vp(self, extent: i32, vw: i32, vh: i32) -> Option<i32> {
        match self {
            Len::Vw(f) => Some((f / 100.0 * vw as f32).round() as i32),
            Len::Vh(f) => Some((f / 100.0 * vh as f32).round() as i32),
            other => other.resolve(extent),
        }
    }
}

/// A two-stop linear gradient background (`linear-gradient`) — start→end along
/// the vertical (default) or horizontal axis. Multi-stop gradients collapse to
/// their first/last stop (ADR-0041).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub start: Color,
    pub end: Color,
    pub vertical: bool,
}

/// A `box-shadow` (outer, single layer): offset, blur radius, color (ADR-0041).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxShadow {
    pub dx: i32,
    pub dy: i32,
    pub blur: i32,
    pub color: Color,
}

/// CSS `text-transform` (ADR-0041). Inherited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// CSS `box-sizing` — whether `width`/`height` include padding + border
/// (`border-box`) or just the content (`content-box`, the default) — ADR-0040.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

/// CSS `float` — take a box out of normal flow to the left/right, with siblings
/// flowing alongside (ADR-0039).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// CSS `clear` — drop below preceding floats on the named side(s).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

/// CSS `display` (the subset we flow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    /// An atomic inline-level box with the block box model (ADR-0042).
    InlineBlock,
    ListItem,
    /// Flex container (single-axis v1).
    Flex,
    /// Grid container (explicit tracks v1).
    Grid,
    /// Not rendered at all.
    None,
}

/// CSS `visibility`. A hidden element still occupies layout space but is not
/// painted (unlike `display: none`). Inherited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
}

/// CSS `text-align`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// CSS `flex-direction` (v1: the two main axes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
}

/// CSS `justify-content` — main-axis distribution of items/free space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum JustifyContent {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// CSS `align-items` — cross-axis alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlignItems {
    Start,
    Center,
    End,
    #[default]
    Stretch,
}

/// CSS `align-self` — a flex item's per-item cross-axis alignment. `Auto` defers
/// to the container's `align-items`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlignSelf {
    #[default]
    Auto,
    Start,
    Center,
    End,
    Stretch,
}

/// CSS `flex-basis` — a flex item's initial main size before grow/shrink.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum FlexBasis {
    /// `auto` (use the item's main size, falling back to its content size).
    #[default]
    Auto,
    /// `content` (always the content size).
    Content,
    /// A fixed px length.
    Px(i32),
    /// A percentage of the container's main size.
    Pct(f32),
}

/// A grid track size (one column/row in `grid-template-columns`/`-rows`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Track {
    /// A fixed length in CSS pixels.
    Px(u32),
    /// A share of the leftover space (`fr`).
    Fr(f32),
    /// Content-sized (v1: treated as one `fr`).
    Auto,
    /// `minmax(min, max)` — a floor in px and a max that flexes (ADR-0038).
    MinMax(u32, TrackMax),
}

/// The `max` side of a `minmax()` track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackMax {
    Px(u32),
    Fr(f32),
    Auto,
}

/// The computed style applied to one element (after the cascade).
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background: Option<Color>,
    /// `background-image: url(...)` — the (unresolved) URL, painted behind the
    /// element's content via the image pipeline (ADR-0038). Not inherited.
    pub background_image: Option<String>,
    pub font_size: u32,
    pub font: FontStyle,
    pub text_align: TextAlign,
    pub underline: bool,
    /// `line-height` resolved to px (`None` = `normal`, the 1.5× default);
    /// `text-transform`; `letter-spacing` in px (may be negative). Inherited text
    /// properties (ADR-0041).
    pub line_height: Option<i32>,
    pub text_transform: TextTransform,
    pub letter_spacing: i32,
    pub margin_top: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    /// `margin-left`/`-right: auto` — used to center a width-constrained block
    /// (ADR-0039). Not inherited.
    pub margin_left_auto: bool,
    pub margin_right_auto: bool,
    /// `width`/`max-width`/`min-width` for block boxes (ADR-0039). `Auto` means
    /// unconstrained (fill the available width). Not inherited.
    pub width: Len,
    pub max_width: Len,
    pub min_width: Len,
    /// `height`/`min-height`/`max-height` for block & flex/grid container boxes
    /// (ADR-0042). `Auto` = content-sized; `%` heights (indefinite parent) are
    /// treated as auto. Not inherited.
    pub height: Len,
    pub min_height: Len,
    pub max_height: Len,
    /// `float` / `clear` (ADR-0039). Not inherited.
    pub float: Float,
    pub clear: Clear,
    /// `padding` (px per side) — inner spacing between border and content
    /// (ADR-0040). Not inherited.
    pub padding_top: i32,
    pub padding_right: i32,
    pub padding_bottom: i32,
    pub padding_left: i32,
    /// `border-*-width` (px per side) and a single `border-color` — a solid
    /// border painted around the padding box (ADR-0040). Not inherited.
    pub border_top: i32,
    pub border_right: i32,
    pub border_bottom: i32,
    pub border_left: i32,
    pub border_color: Color,
    /// `box-sizing` (ADR-0040). Not inherited.
    pub box_sizing: BoxSizing,
    /// `overflow`(`-x`/`-y`): whether content past the box edges is clipped
    /// (`hidden`/`clip`/`scroll`/`auto` — we clip rather than scroll) — ADR-0043.
    /// Not inherited.
    pub overflow_clip: bool,
    /// `border-radius` (px, uniform), `background: linear-gradient(...)`, and
    /// `box-shadow` (ADR-0041). The rare gradient/shadow are boxed so the common
    /// element (neither) pays only a null pointer. Not inherited.
    pub border_radius: u16,
    pub background_gradient: Option<Box<Gradient>>,
    pub box_shadow: Option<Box<BoxShadow>>,
    /// `object-fit` (`<img>`) / `background-size` (cover/contain) — how an image
    /// scales into its box (ADR-0044). `Fill` (stretch) is the default. The two
    /// are separate CSS properties on the same element, so they're tracked apart.
    /// Not inherited.
    pub object_fit: ImageFit,
    pub background_size: ImageFit,
    /// `object-position` (default center) / `background-position` (default
    /// top-left) — where a `Cover`/`Contain` image anchors in its box
    /// (ADR-0045). Plain values (8 bytes each), not inherited.
    pub object_position: ImagePos,
    pub background_position: ImagePos,
    /// Preserve whitespace/newlines (`<pre>`); otherwise collapse + wrap.
    pub preformatted: bool,
    /// `visibility: hidden` — laid out but not painted. Inherited.
    pub visibility: Visibility,
    /// `opacity` in `[0.0, 1.0]`, composited in paint. Not inherited.
    pub opacity: f32,
    /// Flex/grid container properties (meaningful only when `display` is
    /// `Flex`/`Grid`); reset per element.
    pub flex_direction: FlexDirection,
    /// `flex-direction: row-reverse`/`column-reverse` — main axis is reversed.
    pub flex_reverse: bool,
    pub flex_wrap: bool,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Flex *item* properties (meaningful when this element is a flex child);
    /// reset per element (ADR-0036). `flex: grow shrink basis`, plus per-item
    /// cross alignment and reorder.
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: FlexBasis,
    pub align_self: AlignSelf,
    pub order: i32,
    /// `gap` between flex items / grid tracks, in CSS pixels.
    pub gap: u32,
    pub grid_template_columns: Vec<Track>,
    pub grid_template_rows: Vec<Track>,
    /// `repeat(auto-fill|auto-fit, <track>)` columns: the repeated track, with the
    /// count computed from the container width at layout (ADR-0038).
    pub grid_auto_fill: Option<Track>,
    /// `grid-auto-rows` — size of implicitly-created rows (else content-sized).
    pub grid_auto_rows: Option<Track>,
    /// `grid-auto-columns` — size of implicitly-created columns (ADR-0054).
    pub grid_auto_columns: Option<Track>,
    /// `grid-auto-flow: column` — auto-placed items flow down columns (horizontal
    /// toolbars/chip rows) rather than the default row-major flow (ADR-0054).
    pub grid_auto_flow_column: bool,
    /// The column template used named grid lines (`[name]`), i.e. the full-bleed
    /// centering pattern we don't resolve; layout collapses it to one full-width
    /// column so content stacks readably instead of landing in a gutter (ADR-0038).
    pub grid_cols_named: bool,
    /// Grid *item* placement spans (`grid-column`/`grid-row: span N` or `a / b`);
    /// 1 unless the item spans multiple tracks. Reset per element (ADR-0038).
    pub grid_column_span: u32,
    pub grid_row_span: u32,
    /// The item used named-line/area placement we don't resolve (e.g.
    /// `grid-column: content`); layout places it in the container's widest
    /// (content) track rather than dumping it into a leading gutter (ADR-0038).
    pub grid_named_place: bool,
    /// `grid-template-areas`: rows of cell names (an empty `.` cell is `String::new`).
    /// A named container maps each `grid-area` item to the rectangle its name spans
    /// — how modern page shells lay out sidebars + content (ADR-0051).
    pub grid_template_areas: Vec<Vec<String>>,
    /// The grid *item*'s `grid-area: <name>`, placed into the container's matching
    /// template area (ADR-0051).
    pub grid_area: Option<String>,
    /// `position` and its insets/`z-index` (ADR-0034). Insets resolve against the
    /// containing block at layout; `z_index` orders positioned layers in paint.
    pub position: Position,
    pub inset_top: Len,
    pub inset_right: Len,
    pub inset_bottom: Len,
    pub inset_left: Len,
    pub z_index: Option<i32>,
}

impl ComputedStyle {
    /// The initial (root) computed style.
    pub fn initial() -> Self {
        Self {
            display: Display::Block,
            color: Color::BLACK,
            background: None,
            background_image: None,
            font_size: 16,
            font: FontStyle::REGULAR,
            text_align: TextAlign::Left,
            underline: false,
            line_height: None,
            text_transform: TextTransform::None,
            letter_spacing: 0,
            margin_top: 0,
            margin_bottom: 0,
            margin_left: 0,
            margin_left_auto: false,
            margin_right_auto: false,
            width: Len::Auto,
            max_width: Len::Auto,
            min_width: Len::Auto,
            height: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::Auto,
            float: Float::None,
            clear: Clear::None,
            padding_top: 0,
            padding_right: 0,
            padding_bottom: 0,
            padding_left: 0,
            border_top: 0,
            border_right: 0,
            border_bottom: 0,
            border_left: 0,
            border_color: Color::BLACK,
            box_sizing: BoxSizing::ContentBox,
            overflow_clip: false,
            border_radius: 0,
            background_gradient: None,
            box_shadow: None,
            object_fit: ImageFit::Fill,
            background_size: ImageFit::Fill,
            object_position: ImagePos::CENTER,
            background_position: ImagePos::TOP_LEFT,
            preformatted: false,
            visibility: Visibility::Visible,
            opacity: 1.0,
            flex_direction: FlexDirection::Row,
            flex_reverse: false,
            flex_wrap: false,
            justify_content: JustifyContent::Start,
            align_items: AlignItems::Stretch,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Auto,
            align_self: AlignSelf::Auto,
            order: 0,
            gap: 0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_auto_fill: None,
            grid_auto_rows: None,
            grid_auto_columns: None,
            grid_auto_flow_column: false,
            grid_cols_named: false,
            grid_column_span: 1,
            grid_row_span: 1,
            grid_named_place: false,
            grid_template_areas: Vec::new(),
            grid_area: None,
            position: Position::Static,
            inset_top: Len::Auto,
            inset_right: Len::Auto,
            inset_bottom: Len::Auto,
            inset_left: Len::Auto,
            z_index: None,
        }
    }

    /// Inheritable properties pass to children; box/display properties reset.
    pub fn inherit(&self) -> Self {
        Self {
            // Inherited:
            color: self.color,
            font_size: self.font_size,
            font: self.font,
            text_align: self.text_align,
            underline: self.underline,
            line_height: self.line_height,
            text_transform: self.text_transform,
            letter_spacing: self.letter_spacing,
            preformatted: self.preformatted,
            visibility: self.visibility,
            // Reset per element:
            display: Display::Inline,
            background: None,
            background_image: None,
            opacity: 1.0,
            margin_top: 0,
            margin_bottom: 0,
            margin_left: 0,
            margin_left_auto: false,
            margin_right_auto: false,
            width: Len::Auto,
            max_width: Len::Auto,
            min_width: Len::Auto,
            height: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::Auto,
            float: Float::None,
            clear: Clear::None,
            padding_top: 0,
            padding_right: 0,
            padding_bottom: 0,
            padding_left: 0,
            border_top: 0,
            border_right: 0,
            border_bottom: 0,
            border_left: 0,
            border_color: Color::BLACK,
            box_sizing: BoxSizing::ContentBox,
            overflow_clip: false,
            border_radius: 0,
            background_gradient: None,
            box_shadow: None,
            // object-fit/-position and background-size/-position are not inherited.
            object_fit: ImageFit::Fill,
            background_size: ImageFit::Fill,
            object_position: ImagePos::CENTER,
            background_position: ImagePos::TOP_LEFT,
            flex_direction: FlexDirection::Row,
            flex_reverse: false,
            flex_wrap: false,
            justify_content: JustifyContent::Start,
            align_items: AlignItems::Stretch,
            // Flex-item properties are not inherited.
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Auto,
            align_self: AlignSelf::Auto,
            order: 0,
            gap: 0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_auto_fill: None,
            grid_auto_rows: None,
            grid_auto_columns: None,
            grid_auto_flow_column: false,
            grid_cols_named: false,
            grid_column_span: 1,
            grid_row_span: 1,
            grid_named_place: false,
            grid_template_areas: Vec::new(),
            grid_area: None,
            // Positioning is not inherited; every element starts in normal flow.
            position: Position::Static,
            inset_top: Len::Auto,
            inset_right: Len::Auto,
            inset_bottom: Len::Auto,
            inset_left: Len::Auto,
            z_index: None,
        }
    }
}

/// A child within the styled tree. The element node is boxed so a `Text` child
/// doesn't reserve a whole `StyledNode`'s worth of slack in the children vector
/// (which matters on text-heavy pages) and to keep the enum small.
#[derive(Clone, Debug)]
pub enum StyledChild {
    Text(String),
    Element(Box<StyledNode>),
}

/// An element with its computed style and styled children.
#[derive(Clone, Debug)]
pub struct StyledNode {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub style: ComputedStyle,
    pub children: Vec<StyledChild>,
    /// The id of the DOM node this was styled from, so layout can tag its hit
    /// boxes and event dispatch can correlate a rendered box back to the live
    /// DOM / JS node (M12b).
    pub node_id: NodeId,
}

impl StyledNode {
    /// The value of attribute `name`, if present.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Concatenate the text of this node and its descendants.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        for child in &self.children {
            match child {
                StyledChild::Text(t) => out.push_str(t),
                StyledChild::Element(e) => e.collect_text(out),
            }
        }
    }
}

/// A styled document.
#[derive(Clone, Debug)]
pub struct StyledDom {
    pub root: StyledNode,
}

/// Externally-fetched stylesheets (`<link rel="stylesheet">` bodies), keyed by
/// the link's `href` exactly as it appears in the document, so the cascade can
/// splice each sheet in at its `<link>`'s document position (ADR-0037).
pub type ExternalSheets = std::collections::HashMap<String, String>;

/// Turns a parsed `Document` into a `StyledDom` (UA + author CSS cascade).
pub trait StyleEngine: Send {
    /// Compute styles for `doc` (inline `<style>` + `style=` author CSS only).
    fn style(&self, doc: &Document) -> StyledDom;

    /// Compute styles for `doc`, splicing externally-fetched `<link>`
    /// stylesheets into the cascade at each link's position. The default ignores
    /// them (so non-CSS engines need not implement it).
    fn style_with_sheets(&self, doc: &Document, sheets: &ExternalSheets) -> StyledDom {
        let _ = sheets;
        self.style(doc)
    }
}

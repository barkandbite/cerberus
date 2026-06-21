//! Neutral style types and the `StyleEngine` seam.
//!
//! This crate holds the *result* of styling — `ComputedStyle` and a `StyledDom`
//! tree — plus the `StyleEngine` trait. The actual CSS parsing/cascade lives in
//! an adapter (`cerberus-css`) behind this trait, so it can be swapped or
//! reimplemented without touching layout. Layout consumes only these types.

use cerberus_dom::{Document, NodeId};
use cerberus_types::{Color, FontStyle};

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
}

impl Len {
    /// Resolve against a containing-block `extent` in px; `auto` → `None`.
    pub fn resolve(self, extent: i32) -> Option<i32> {
        match self {
            Len::Auto => None,
            Len::Px(p) => Some(p),
            Len::Pct(f) => Some((f / 100.0 * extent as f32).round() as i32),
        }
    }
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
    /// `float` / `clear` (ADR-0039). Not inherited.
    pub float: Float,
    pub clear: Clear,
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
            margin_top: 0,
            margin_bottom: 0,
            margin_left: 0,
            margin_left_auto: false,
            margin_right_auto: false,
            width: Len::Auto,
            max_width: Len::Auto,
            min_width: Len::Auto,
            float: Float::None,
            clear: Clear::None,
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
            grid_cols_named: false,
            grid_column_span: 1,
            grid_row_span: 1,
            grid_named_place: false,
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
            float: Float::None,
            clear: Clear::None,
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
            grid_cols_named: false,
            grid_column_span: 1,
            grid_row_span: 1,
            grid_named_place: false,
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

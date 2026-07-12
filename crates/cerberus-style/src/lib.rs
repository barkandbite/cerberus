//! Neutral style types and the `StyleEngine` seam.
//!
//! This crate holds the *result* of styling — `ComputedStyle` and a `StyledDom`
//! tree — plus the `StyleEngine` trait. The actual CSS parsing/cascade lives in
//! an adapter (`cerberus-css`) behind this trait, so it can be swapped or
//! reimplemented without touching layout. Layout consumes only these types.

use cerberus_dom::{Document, NodeId};
use cerberus_types::{Color, FontStyle, GenericFamily, ImageFit, ImagePos, Point};

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
    /// Percent of the smaller / larger viewport dimension (`vmin`/`vmax`).
    Vmin(f32),
    Vmax(f32),
}

impl Len {
    /// Resolve against a containing-block `extent` in px; `auto`/viewport units
    /// → `None` (use [`Len::resolve_vp`] when the viewport is known).
    pub fn resolve(self, extent: i32) -> Option<i32> {
        match self {
            Len::Px(p) => Some(p),
            Len::Pct(f) => Some((f / 100.0 * extent as f32).round() as i32),
            Len::Auto | Len::Vw(_) | Len::Vh(_) | Len::Vmin(_) | Len::Vmax(_) => None,
        }
    }

    /// Resolve including viewport units (`vw`/`vh`/`vmin`/`vmax`) against a
    /// `vw`×`vh` px viewport.
    pub fn resolve_vp(self, extent: i32, vw: i32, vh: i32) -> Option<i32> {
        let pct = |f: f32, basis: i32| Some((f / 100.0 * basis as f32).round() as i32);
        match self {
            Len::Vw(f) => pct(f, vw),
            Len::Vh(f) => pct(f, vh),
            Len::Vmin(f) => pct(f, vw.min(vh)),
            Len::Vmax(f) => pct(f, vw.max(vh)),
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

/// CSS `vertical-align` for inline content — the subset that shifts the baseline
/// (`sub`/`super`, as used by `<sub>`/`<sup>`). Not inherited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Sub,
    Super,
    /// `top` / `middle` / `bottom` / `text-top` / `text-bottom` — alignments
    /// that take a replaced box OFF the baseline. Not positioned distinctly
    /// (boxes stay top-aligned), but they must suppress the baseline strut
    /// descent an inline image otherwise reserves below itself.
    OffBaseline,
}

/// CSS `white-space`: how whitespace and newlines in inline content collapse,
/// and whether lines soft-wrap. Inherited. The three behaviors — preserving
/// space runs, preserving explicit `\n`, and wrapping on overflow — vary
/// independently across the keywords, so they're exposed as predicate methods
/// rather than encoded positionally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WhiteSpace {
    /// Collapse whitespace runs to a single space; wrap on overflow.
    #[default]
    Normal,
    /// Preserve spaces and newlines; never wrap (`<pre>`).
    Pre,
    /// Preserve spaces and newlines; still wrap on overflow.
    PreWrap,
    /// Collapse space runs but keep explicit newlines; wrap on overflow.
    PreLine,
    /// Collapse whitespace like `normal`, but never wrap.
    Nowrap,
}

impl WhiteSpace {
    /// Whether runs of spaces/tabs are kept verbatim rather than collapsed.
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }
    /// Whether an explicit `\n` in the source forces a hard line break.
    pub fn preserves_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }
    /// Whether long lines soft-wrap at word boundaries when they overflow.
    pub fn wraps(self) -> bool {
        matches!(
            self,
            WhiteSpace::Normal | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }
}

/// CSS `line-height`. A unitless `<number>` is special: it inherits as the
/// **factor**, and each element re-multiplies it by its *own* font-size — so
/// `body { line-height: 1.5 }` gives a larger heading proportionally taller
/// leading. A length/percentage instead inherits as the resolved px. Inherited.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LineHeight {
    /// `normal` — the layout's natural leading for the element's font size.
    #[default]
    Normal,
    /// A unitless multiplier of the element's own font-size.
    Factor(f32),
    /// An absolute px value (a length, or a percentage resolved at parse time).
    Px(i32),
}

impl LineHeight {
    /// Resolve to a px line-box height for an element of `font_size`, using
    /// `default_px` (the font's natural leading) for `Normal`.
    pub fn resolve(self, font_size: u32, default_px: i32) -> i32 {
        match self {
            LineHeight::Normal => default_px,
            LineHeight::Factor(f) => (f * font_size as f32).round().max(0.0) as i32,
            LineHeight::Px(px) => px,
        }
    }

    /// [`resolve`](Self::resolve) without the rounding. Chrome keeps used
    /// line-height fractional (`line-height: 1.15` on 16px text is 18.4px, and
    /// `normal` is the face's exact metric ratio); the inline flow accumulates
    /// this and rounds per line, so line N lands at `round(N × pitch)` instead
    /// of drifting by the rounding error each line.
    pub fn resolve_f(self, font_size: u32, default: f32) -> f32 {
        match self {
            LineHeight::Normal => default,
            LineHeight::Factor(f) => (f * font_size as f32).max(0.0),
            LineHeight::Px(px) => px as f32,
        }
    }
}

/// CSS `list-style-type`: the marker drawn before a `display: list-item`. A
/// practical subset — the bullet glyphs and decimal numbering that cover almost
/// all real lists. Inherited (so `<ol>`'s `decimal` reaches its `<li>` children).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ListStyleType {
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    /// `a, b, … z, aa, …` (bijective base-26); `LowerAlpha`/`UpperAlpha` differ
    /// only in case.
    LowerAlpha,
    UpperAlpha,
    /// `i, ii, iii, iv, …` roman numerals; `LowerRoman`/`UpperRoman` differ only
    /// in case.
    LowerRoman,
    UpperRoman,
    None,
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
    /// The legacy `<center>` value (`-webkit-center`): centers inline content
    /// like `center` AND centers child table boxes — but does NOT survive into
    /// table cells (their text stays left unless the cell sets its own
    /// alignment), matching how the reference renders `<center><table>`.
    WebkitCenter,
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
    /// Whether `font-size` is still the initial `medium` keyword (no explicit
    /// length/keyword set on this element or an ancestor). Inherited. Chrome
    /// resolves `medium` to 13px for the monospace generic and 16px otherwise
    /// (the "monospace renders smaller" quirk), so the cascade applies that once
    /// both `font-size` and `font-family` are known.
    pub font_size_medium: bool,
    /// The generic family this element's `font-family` resolves to (serif /
    /// sans-serif / monospace / …). Inherited. Selects the bundled face at
    /// rasterization; the named fonts themselves are never shipped.
    pub font_family: GenericFamily,
    pub text_align: TextAlign,
    pub underline: bool,
    /// `text-decoration: line-through` (strikethrough). Inherited alongside
    /// `underline`.
    pub line_through: bool,
    /// `line-height` (a factor, absolute px, or `normal`); `text-transform`;
    /// `letter-spacing` in px (may be negative). Inherited text properties
    /// (ADR-0041).
    pub line_height: LineHeight,
    pub text_transform: TextTransform,
    pub letter_spacing: i32,
    /// `word-spacing` in px (may be negative): extra space added to each
    /// inter-word gap on top of the normal space advance. Inherited.
    pub word_spacing: i32,
    /// `list-style-type`: the marker for a `display: list-item`. Inherited.
    pub list_style_type: ListStyleType,
    /// `vertical-align` (`sub`/`super`) — shifts inline content off the baseline.
    /// Not inherited.
    pub vertical_align: VerticalAlign,
    /// `text-indent` in px: the first-line indent of a block's inline content.
    /// Inherited.
    pub text_indent: i32,
    /// Margins as lengths, resolved against the containing-block **width** at
    /// layout (CSS resolves `%` margins — top/bottom included — against the
    /// container's width). `Auto` resolves to 0 here; horizontal `auto` for
    /// centering is tracked separately by `margin_{left,right}_auto`. Not
    /// inherited. Was `i32` px, which dropped `%`/`vw`/`vh` margins at parse.
    pub margin_top: Len,
    pub margin_bottom: Len,
    pub margin_left: Len,
    pub margin_right: Len,
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
    /// `text-overflow: ellipsis`: when a non-wrapping line is clipped by the box,
    /// truncate it and append an ellipsis (`…`) instead of a hard cut. Applies
    /// with `overflow` clipping and `white-space: nowrap`. Not inherited.
    pub text_overflow_ellipsis: bool,
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
    /// The pixel (length) component of `background-position` — e.g. the `-304px`
    /// in `background-position: 0 -304px` that crops a CSS sprite. Applied on top
    /// of the fractional `background_position`. `(0, 0)` when the position is
    /// keyword/percentage-only. Not inherited.
    pub background_position_px: Point,
    /// `white-space`: whitespace collapsing, newline preservation, and wrapping.
    /// Inherited.
    pub white_space: WhiteSpace,
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
            font_size_medium: true,
            // Chrome's UA default for a page that never sets `font-family` is a
            // serif (Times), not a sans — an unstyled page must render serif or
            // every wrap point and heading drifts from the reference.
            font_family: GenericFamily::Serif,
            text_align: TextAlign::Left,
            underline: false,
            line_through: false,
            line_height: LineHeight::Normal,
            text_transform: TextTransform::None,
            letter_spacing: 0,
            word_spacing: 0,
            list_style_type: ListStyleType::Disc,
            vertical_align: VerticalAlign::Baseline,
            text_indent: 0,
            margin_top: Len::Px(0),
            margin_bottom: Len::Px(0),
            margin_left: Len::Px(0),
            margin_right: Len::Px(0),
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
            text_overflow_ellipsis: false,
            border_radius: 0,
            background_gradient: None,
            box_shadow: None,
            object_fit: ImageFit::Fill,
            background_size: ImageFit::Auto,
            object_position: ImagePos::CENTER,
            background_position: ImagePos::TOP_LEFT,
            background_position_px: Point::ZERO,
            white_space: WhiteSpace::Normal,
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
            font_size_medium: self.font_size_medium,
            font_family: self.font_family,
            text_align: self.text_align,
            underline: self.underline,
            line_through: self.line_through,
            line_height: self.line_height,
            text_transform: self.text_transform,
            letter_spacing: self.letter_spacing,
            word_spacing: self.word_spacing,
            list_style_type: self.list_style_type,
            vertical_align: VerticalAlign::Baseline,
            text_indent: self.text_indent,
            white_space: self.white_space,
            visibility: self.visibility,
            // Reset per element:
            display: Display::Inline,
            background: None,
            background_image: None,
            opacity: 1.0,
            margin_top: Len::Px(0),
            margin_bottom: Len::Px(0),
            margin_left: Len::Px(0),
            margin_right: Len::Px(0),
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
            text_overflow_ellipsis: false,
            border_radius: 0,
            background_gradient: None,
            box_shadow: None,
            // object-fit/-position and background-size/-position are not inherited.
            object_fit: ImageFit::Fill,
            background_size: ImageFit::Auto,
            object_position: ImagePos::CENTER,
            background_position: ImagePos::TOP_LEFT,
            background_position_px: Point::ZERO,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn len_resolve_handles_px_pct_and_auto() {
        assert_eq!(Len::Px(20).resolve(1000), Some(20));
        // Percentage is of the containing extent, rounded to the nearest px.
        assert_eq!(Len::Pct(50.0).resolve(200), Some(100));
        assert_eq!(Len::Pct(33.0).resolve(100), Some(33));
        // Auto and viewport units cannot resolve without a viewport.
        assert_eq!(Len::Auto.resolve(500), None);
        assert_eq!(Len::Vw(50.0).resolve(500), None);
        assert_eq!(Len::Vh(50.0).resolve(500), None);
    }

    #[test]
    fn len_resolve_vp_resolves_viewport_units() {
        // vw/vh are percentages of the viewport axes.
        assert_eq!(Len::Vw(50.0).resolve_vp(0, 1280, 800), Some(640));
        assert_eq!(Len::Vh(25.0).resolve_vp(0, 1280, 800), Some(200));
        // Everything else delegates to `resolve` (extent-relative).
        assert_eq!(Len::Px(10).resolve_vp(500, 1280, 800), Some(10));
        assert_eq!(Len::Pct(10.0).resolve_vp(500, 1280, 800), Some(50));
        assert_eq!(Len::Auto.resolve_vp(500, 1280, 800), None);
    }

    #[test]
    fn white_space_predicate_matrix() {
        use WhiteSpace::*;
        // (preserves_spaces, preserves_newlines, wraps) for each keyword.
        assert_eq!(
            (
                Normal.preserves_spaces(),
                Normal.preserves_newlines(),
                Normal.wraps()
            ),
            (false, false, true)
        );
        assert_eq!(
            (
                Pre.preserves_spaces(),
                Pre.preserves_newlines(),
                Pre.wraps()
            ),
            (true, true, false)
        );
        assert_eq!(
            (
                PreWrap.preserves_spaces(),
                PreWrap.preserves_newlines(),
                PreWrap.wraps()
            ),
            (true, true, true)
        );
        assert_eq!(
            (
                PreLine.preserves_spaces(),
                PreLine.preserves_newlines(),
                PreLine.wraps()
            ),
            (false, true, true)
        );
        assert_eq!(
            (
                Nowrap.preserves_spaces(),
                Nowrap.preserves_newlines(),
                Nowrap.wraps()
            ),
            (false, false, false)
        );
    }

    #[test]
    fn viewport_units_resolve_against_the_right_basis() {
        // In portrait (w < h): vmin follows width, vmax follows height — the case
        // the old `vmax→Vw` / `vmin→Vh` aliasing got backwards.
        let (w, h) = (400, 800);
        assert_eq!(Len::Vw(50.0).resolve_vp(0, w, h), Some(200));
        assert_eq!(Len::Vh(50.0).resolve_vp(0, w, h), Some(400));
        assert_eq!(
            Len::Vmin(50.0).resolve_vp(0, w, h),
            Some(200),
            "vmin = min(w,h)"
        );
        assert_eq!(
            Len::Vmax(50.0).resolve_vp(0, w, h),
            Some(400),
            "vmax = max(w,h)"
        );
        // In landscape the min/max swap dimensions.
        assert_eq!(Len::Vmin(10.0).resolve_vp(0, 1000, 500), Some(50));
        assert_eq!(Len::Vmax(10.0).resolve_vp(0, 1000, 500), Some(100));
        // Without a viewport they don't resolve.
        assert_eq!(Len::Vmin(50.0).resolve(999), None);
        assert_eq!(Len::Vmax(50.0).resolve(999), None);
    }

    #[test]
    fn initial_has_sane_defaults() {
        let s = ComputedStyle::initial();
        assert_eq!(s.display, Display::Block);
        assert_eq!(s.color, Color::BLACK);
        assert_eq!(s.font_size, 16);
        assert_eq!(s.margin_top, Len::Px(0));
        assert_eq!(s.margin_right, Len::Px(0));
        assert_eq!(s.width, Len::Auto);
        assert_eq!(s.vertical_align, VerticalAlign::Baseline);
        assert_eq!(s.white_space, WhiteSpace::Normal);
    }

    #[test]
    fn inherit_copies_inherited_and_resets_the_rest() {
        // Set some inherited and some non-inherited properties away from initial,
        // plus `vertical-align` (an inline text property that is nonetheless NOT
        // inherited), then confirm which cross the parent→child boundary.
        let mut parent = ComputedStyle::initial();
        parent.font_size = 22; // inherited
        parent.color = Color::rgb(1, 2, 3); // inherited
        parent.text_indent = 12; // inherited
        parent.white_space = WhiteSpace::Pre; // inherited
        parent.margin_top = Len::Px(9); // not inherited
        parent.margin_right = Len::Px(7); // not inherited
        parent.width = Len::Px(300); // not inherited
        parent.display = Display::Flex; // not inherited (child resets to Inline)
        parent.vertical_align = VerticalAlign::Super; // not inherited

        let child = parent.inherit();
        let init = ComputedStyle::initial();

        // Inherited values flow down.
        assert_eq!(child.font_size, 22);
        assert_eq!(child.color, Color::rgb(1, 2, 3));
        assert_eq!(child.text_indent, 12);
        assert_eq!(child.white_space, WhiteSpace::Pre);

        // Non-inherited values reset to their initial value.
        assert_eq!(child.margin_top, init.margin_top);
        assert_eq!(child.margin_right, init.margin_right);
        assert_eq!(child.width, init.width);
        assert_eq!(
            child.display,
            Display::Inline,
            "a child box starts inline, not the parent's display"
        );
        assert_eq!(
            child.vertical_align,
            VerticalAlign::Baseline,
            "vertical-align does not inherit"
        );
    }
}

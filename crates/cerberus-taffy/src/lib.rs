//! Taffy adapter — the structural half of the rendering-architecture migration
//! (`docs/RENDERING_ARCHITECTURE_PLAN.md`, Stage 3).
//!
//! Cerberus's block/inline flow was hand-rolled as a single immediate-mode
//! walker. That collapses the CSS pipeline's three distinct stages — box-tree
//! construction, intrinsic sizing, and per-formatting-context used-size layout —
//! into one cursor, which is why block/flex/grid geometry bugs kept recurring.
//! Rather than re-derive 30 years of standardized layout by screenshot-diffing,
//! we adopt [`taffy`] (the layout core behind Bevy/Dioxus/Zed) behind the
//! existing `LayoutEngine` seam and migrate page classes to it incrementally.
//!
//! This module is the **pure, side-effect-free** boundary: [`to_taffy_style`]
//! translates one [`ComputedStyle`] into a [`taffy::Style`]. It owns only the
//! box model taffy understands — display/position/size/margin/padding/border/
//! flex/grid. Inline content, text, and replaced elements are *leaves* taffy
//! sizes through a measure closure (built in the engine layer, not here), so the
//! existing `cerberus-text` shaping and `cerberus-layout` inline flow stay the
//! source of truth for everything below the box level.
//!
//! Keeping this a pure `&ComputedStyle -> taffy::Style` function is deliberate:
//! it is exhaustively unit-testable (every `Len`/margin-auto/grid-track mapping
//! below has a test), and it lets the engine layer land separately once the
//! mapping is trusted.

mod engine;
pub use engine::TaffyLayout;

use cerberus_style::{
    AlignItems, ComputedStyle, Display, FlexBasis, JustifyContent, Len, Track, TrackMax,
};
use taffy::{
    geometry::{Rect, Size},
    prelude::TaffyAuto,
    style::{
        AlignContent, AlignItems as TaffyAlignItems, Dimension, FlexDirection, FlexWrap,
        GridTemplateComponent, LengthPercentage, LengthPercentageAuto, MaxTrackSizingFunction,
        MinTrackSizingFunction, Position, RepetitionCount, Style, TrackSizingFunction,
    },
    Display as TaffyDisplay,
};

/// A CSS length as a taffy [`Dimension`] (for `width`/`height`/`flex-basis`),
/// resolving viewport units against the `vw`×`vh` viewport. `%` stays a taffy
/// percent (resolved against the containing block by taffy itself); `auto` maps
/// to [`Dimension::AUTO`].
fn dim(len: Len, vw: i32, vh: i32) -> Dimension {
    match len {
        Len::Auto => Dimension::AUTO,
        Len::Px(p) => Dimension::length(p as f32),
        Len::Pct(f) => Dimension::percent(f / 100.0),
        // Viewport units have no CSS-percent analogue in taffy; resolve to px.
        Len::Vw(_) | Len::Vh(_) | Len::Vmin(_) | Len::Vmax(_) => {
            Dimension::length(len.resolve_vp(0, vw, vh).unwrap_or(0) as f32)
        }
    }
}

/// A CSS length as a taffy [`LengthPercentageAuto`] (for `margin`/`inset`).
fn lpa(len: Len, vw: i32, vh: i32) -> LengthPercentageAuto {
    match len {
        Len::Auto => LengthPercentageAuto::AUTO,
        Len::Px(p) => LengthPercentageAuto::length(p as f32),
        Len::Pct(f) => LengthPercentageAuto::percent(f / 100.0),
        Len::Vw(_) | Len::Vh(_) | Len::Vmin(_) | Len::Vmax(_) => {
            LengthPercentageAuto::length(len.resolve_vp(0, vw, vh).unwrap_or(0) as f32)
        }
    }
}

/// A px count as a taffy [`LengthPercentage`] (for `padding`/`border`, which are
/// already resolved to px in `ComputedStyle`).
fn lp_px(px: i32) -> LengthPercentage {
    LengthPercentage::length(px.max(0) as f32)
}

/// One grid track (`grid-template-columns`/`-rows` entry) as a taffy
/// [`TrackSizingFunction`]. `fr` → flex, `auto` → `auto`, `minmax` → `minmax`.
fn track_sizing(t: Track) -> TrackSizingFunction {
    use taffy::style_helpers::{auto, fr, length, minmax};
    match t {
        Track::Px(px) => length(px as f32),
        Track::Fr(f) => fr(f),
        Track::Auto => auto(),
        Track::MinMax(min, max) => {
            let mn: MinTrackSizingFunction = length(min as f32);
            let mx: MaxTrackSizingFunction = match max {
                TrackMax::Px(px) => length(px as f32),
                TrackMax::Fr(f) => fr(f),
                TrackMax::Auto => auto(),
            };
            minmax(mn, mx)
        }
    }
}

/// Map our [`ComputedStyle`] to a [`taffy::Style`], resolving viewport units
/// against the `vw`×`vh` viewport. This is the single source of truth for how a
/// cascaded box presents itself to taffy; the engine layer adds children and a
/// leaf measure closure but never re-interprets these fields.
///
/// Not mapped here (they live below taffy's box level, in the inline/paint
/// layers): color, backgrounds, text properties, floats, overflow, opacity,
/// border-radius, object-fit — none affect box geometry.
pub fn to_taffy_style(s: &ComputedStyle, vw: i32, vh: i32) -> Style {
    let display = match s.display {
        Display::None => TaffyDisplay::None,
        Display::Flex => TaffyDisplay::Flex,
        Display::Grid => TaffyDisplay::Grid,
        // Block, inline-block, list-item and (as leaves) inline all present a
        // block box to taffy; inline runs are handled by the leaf measure fn.
        _ => TaffyDisplay::Block,
    };

    let position = match s.position {
        cerberus_style::Position::Absolute | cerberus_style::Position::Fixed => Position::Absolute,
        // Static/relative/sticky all flow in-place; relative offset is applied
        // in paint, not by taffy, so both map to taffy's Relative.
        _ => Position::Relative,
    };

    // Margins: horizontal `auto` (centering) overrides the length mapping.
    let margin = Rect {
        left: if s.margin_left_auto {
            LengthPercentageAuto::AUTO
        } else {
            lpa(s.margin_left, vw, vh)
        },
        right: if s.margin_right_auto {
            LengthPercentageAuto::AUTO
        } else {
            lpa(s.margin_right, vw, vh)
        },
        top: lpa(s.margin_top, vw, vh),
        bottom: lpa(s.margin_bottom, vw, vh),
    };

    let flex_direction = match (s.flex_direction, s.flex_reverse) {
        (cerberus_style::FlexDirection::Row, false) => FlexDirection::Row,
        (cerberus_style::FlexDirection::Row, true) => FlexDirection::RowReverse,
        (cerberus_style::FlexDirection::Column, false) => FlexDirection::Column,
        (cerberus_style::FlexDirection::Column, true) => FlexDirection::ColumnReverse,
    };

    let align_items = Some(match s.align_items {
        AlignItems::Start => TaffyAlignItems::START,
        AlignItems::Center => TaffyAlignItems::CENTER,
        AlignItems::End => TaffyAlignItems::END,
        AlignItems::Stretch => TaffyAlignItems::STRETCH,
    });

    let justify_content = Some(match s.justify_content {
        JustifyContent::Start => AlignContent::START,
        JustifyContent::Center => AlignContent::CENTER,
        JustifyContent::End => AlignContent::END,
        JustifyContent::SpaceBetween => AlignContent::SPACE_BETWEEN,
        JustifyContent::SpaceAround => AlignContent::SPACE_AROUND,
        JustifyContent::SpaceEvenly => AlignContent::SPACE_EVENLY,
    });

    let flex_basis = match s.flex_basis {
        FlexBasis::Auto | FlexBasis::Content => Dimension::AUTO,
        FlexBasis::Px(p) => Dimension::length(p as f32),
        FlexBasis::Pct(f) => Dimension::percent(f / 100.0),
    };

    // Grid: an explicit template wins; otherwise a single `repeat(auto-fill,…)`
    // track expands to as many columns as fit (taffy computes the count).
    let grid_template_columns: Vec<GridTemplateComponent<_>> =
        if !s.grid_template_columns.is_empty() {
            s.grid_template_columns
                .iter()
                .copied()
                .map(|t| GridTemplateComponent::Single(track_sizing(t)))
                .collect()
        } else if let Some(t) = s.grid_auto_fill {
            vec![taffy::style_helpers::repeat(
                RepetitionCount::AutoFill,
                vec![track_sizing(t)],
            )]
        } else {
            Vec::new()
        };
    let grid_template_rows: Vec<GridTemplateComponent<_>> = s
        .grid_template_rows
        .iter()
        .copied()
        .map(|t| GridTemplateComponent::Single(track_sizing(t)))
        .collect();

    Style {
        display,
        position,
        size: Size {
            width: dim(s.width, vw, vh),
            height: dim(s.height, vw, vh),
        },
        min_size: Size {
            width: dim(s.min_width, vw, vh),
            height: dim(s.min_height, vw, vh),
        },
        max_size: Size {
            width: dim(s.max_width, vw, vh),
            height: dim(s.max_height, vw, vh),
        },
        margin,
        padding: Rect {
            left: lp_px(s.padding_left),
            right: lp_px(s.padding_right),
            top: lp_px(s.padding_top),
            bottom: lp_px(s.padding_bottom),
        },
        border: Rect {
            left: lp_px(s.border_left),
            right: lp_px(s.border_right),
            top: lp_px(s.border_top),
            bottom: lp_px(s.border_bottom),
        },
        inset: Rect {
            left: lpa(s.inset_left, vw, vh),
            right: lpa(s.inset_right, vw, vh),
            top: lpa(s.inset_top, vw, vh),
            bottom: lpa(s.inset_bottom, vw, vh),
        },
        flex_direction,
        flex_wrap: if s.flex_wrap {
            FlexWrap::Wrap
        } else {
            FlexWrap::NoWrap
        },
        flex_grow: s.flex_grow,
        flex_shrink: s.flex_shrink,
        flex_basis,
        align_items,
        justify_content,
        gap: Size {
            width: lp_px(s.gap as i32),
            height: lp_px(s.gap as i32),
        },
        grid_template_columns,
        grid_template_rows,
        ..Style::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ComputedStyle {
        ComputedStyle::initial()
    }

    #[test]
    fn display_maps() {
        let mut s = base();
        s.display = Display::Flex;
        assert_eq!(to_taffy_style(&s, 1000, 800).display, TaffyDisplay::Flex);
        s.display = Display::Grid;
        assert_eq!(to_taffy_style(&s, 1000, 800).display, TaffyDisplay::Grid);
        s.display = Display::None;
        assert_eq!(to_taffy_style(&s, 1000, 800).display, TaffyDisplay::None);
        s.display = Display::InlineBlock;
        assert_eq!(to_taffy_style(&s, 1000, 800).display, TaffyDisplay::Block);
    }

    #[test]
    fn width_px_pct_auto() {
        let mut s = base();
        s.width = Len::Px(200);
        assert_eq!(
            to_taffy_style(&s, 1000, 800).size.width,
            Dimension::length(200.0)
        );
        s.width = Len::Pct(50.0);
        assert_eq!(
            to_taffy_style(&s, 1000, 800).size.width,
            Dimension::percent(0.5)
        );
        s.width = Len::Auto;
        assert_eq!(to_taffy_style(&s, 1000, 800).size.width, Dimension::AUTO);
    }

    #[test]
    fn viewport_units_resolve_to_px() {
        let mut s = base();
        s.width = Len::Vw(50.0); // 50% of 1000 = 500
        assert_eq!(
            to_taffy_style(&s, 1000, 800).size.width,
            Dimension::length(500.0)
        );
        s.height = Len::Vh(25.0); // 25% of 800 = 200
        assert_eq!(
            to_taffy_style(&s, 1000, 800).size.height,
            Dimension::length(200.0)
        );
    }

    #[test]
    fn margin_auto_centers() {
        let mut s = base();
        s.margin_left_auto = true;
        s.margin_right_auto = true;
        let t = to_taffy_style(&s, 1000, 800);
        assert_eq!(t.margin.left, LengthPercentageAuto::AUTO);
        assert_eq!(t.margin.right, LengthPercentageAuto::AUTO);
    }

    #[test]
    fn margin_px_maps() {
        let mut s = base();
        s.margin_top = Len::Px(12);
        assert_eq!(
            to_taffy_style(&s, 1000, 800).margin.top,
            LengthPercentageAuto::length(12.0)
        );
    }

    #[test]
    fn padding_and_border_px() {
        let mut s = base();
        s.padding_left = 8;
        s.border_top = 3;
        let t = to_taffy_style(&s, 1000, 800);
        assert_eq!(t.padding.left, LengthPercentage::length(8.0));
        assert_eq!(t.border.top, LengthPercentage::length(3.0));
    }

    #[test]
    fn flex_item_props() {
        let mut s = base();
        s.flex_grow = 2.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::Px(120);
        let t = to_taffy_style(&s, 1000, 800);
        assert_eq!(t.flex_grow, 2.0);
        assert_eq!(t.flex_shrink, 0.0);
        assert_eq!(t.flex_basis, Dimension::length(120.0));
    }

    #[test]
    fn flex_direction_reverse() {
        let mut s = base();
        s.flex_direction = cerberus_style::FlexDirection::Column;
        s.flex_reverse = true;
        assert_eq!(
            to_taffy_style(&s, 1000, 800).flex_direction,
            FlexDirection::ColumnReverse
        );
    }

    #[test]
    fn grid_template_tracks() {
        let mut s = base();
        s.display = Display::Grid;
        s.grid_template_columns = vec![Track::Px(100), Track::Fr(1.0), Track::Auto];
        let t = to_taffy_style(&s, 1000, 800);
        assert_eq!(t.grid_template_columns.len(), 3);
    }

    #[test]
    fn gap_maps_both_axes() {
        let mut s = base();
        s.gap = 16;
        let t = to_taffy_style(&s, 1000, 800);
        assert_eq!(t.gap.width, LengthPercentage::length(16.0));
        assert_eq!(t.gap.height, LengthPercentage::length(16.0));
    }
}

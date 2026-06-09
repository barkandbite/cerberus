//! Neutral style types and the `StyleEngine` seam.
//!
//! This crate holds the *result* of styling — `ComputedStyle` and a `StyledDom`
//! tree — plus the `StyleEngine` trait. The actual CSS parsing/cascade lives in
//! an adapter (`cerberus-css`) behind this trait, so it can be swapped or
//! reimplemented without touching layout. Layout consumes only these types.

use cerberus_dom::Document;
use cerberus_types::{Color, FontStyle};

/// CSS `display` (the subset we flow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    ListItem,
    /// Not rendered at all.
    None,
}

/// CSS `text-align`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// The computed style applied to one element (after the cascade).
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background: Option<Color>,
    pub font_size: u32,
    pub font: FontStyle,
    pub text_align: TextAlign,
    pub underline: bool,
    pub margin_top: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    /// Preserve whitespace/newlines (`<pre>`); otherwise collapse + wrap.
    pub preformatted: bool,
}

impl ComputedStyle {
    /// The initial (root) computed style.
    pub fn initial() -> Self {
        Self {
            display: Display::Block,
            color: Color::BLACK,
            background: None,
            font_size: 16,
            font: FontStyle::REGULAR,
            text_align: TextAlign::Left,
            underline: false,
            margin_top: 0,
            margin_bottom: 0,
            margin_left: 0,
            preformatted: false,
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
            // Reset per element:
            display: Display::Inline,
            background: None,
            margin_top: 0,
            margin_bottom: 0,
            margin_left: 0,
        }
    }
}

/// A child within the styled tree.
#[derive(Clone, Debug)]
pub enum StyledChild {
    Text(String),
    Element(StyledNode),
}

/// An element with its computed style and styled children.
#[derive(Clone, Debug)]
pub struct StyledNode {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub style: ComputedStyle,
    pub children: Vec<StyledChild>,
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

/// Turns a parsed `Document` into a `StyledDom` (UA + author CSS cascade).
pub trait StyleEngine: Send {
    /// Compute styles for `doc`.
    fn style(&self, doc: &Document) -> StyledDom;
}

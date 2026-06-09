//! The minimal browser UI: a single toolbar.
//!
//! Exactly one fixed toolbar containing, left to right: Back, Forward, Refresh,
//! Stop, a URL box, a tiny head switcher, and a Settings button. No bookmarks,
//! no tab strip — the browser shows one page at a time.
//!
//! This crate is pure: it models the toolbar, lays it out for a window size,
//! paints it into a `DisplayList`, and maps a click to a [`ToolbarAction`]. It
//! knows nothing about windowing (that's a `PlatformSurface` adapter) or
//! networking (that's the session). Button glyphs are shaped via the injected
//! `TextShaper`, so they read correctly once a real font adapter lands.

use cerberus_paint::{DisplayItem, DisplayList, TextShaper};
use cerberus_types::{Color, Point, Rect, Size};

/// Height of the single toolbar, in device pixels.
pub const TOOLBAR_HEIGHT: u32 = 36;

const PAD: i32 = 4;
const BTN: u32 = 28;
const HEAD_W: u32 = 44;
const LABEL_PX: u32 = 16;

/// An action produced by clicking or typing in the toolbar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    /// Go back in history.
    Back,
    /// Go forward in history.
    Forward,
    /// Reload the current page.
    Reload,
    /// Stop the in-flight load.
    Stop,
    /// The URL box was focused (begin editing).
    FocusUrl,
    /// Navigate to this address (URL box submitted).
    Navigate(String),
    /// Cycle to the next identity ("head").
    SwitchHead,
    /// Open the settings panel.
    OpenSettings,
    /// The click hit no control (e.g. the page area).
    None,
}

/// The toolbar controls, in layout order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Back,
    Forward,
    Reload,
    Stop,
    UrlBox,
    Head,
    Settings,
}

/// The toolbar's current state.
#[derive(Clone, Debug)]
pub struct Toolbar {
    /// Text shown/edited in the URL box.
    pub url_text: String,
    /// Whether the URL box has keyboard focus.
    pub url_focused: bool,
    /// Whether Back is enabled.
    pub can_back: bool,
    /// Whether Forward is enabled.
    pub can_forward: bool,
    /// Whether a load is in progress (enables Stop, animates Reload later).
    pub loading: bool,
    /// Short label for the active head (e.g. "work").
    pub head_label: String,
}

impl Toolbar {
    /// A new toolbar for the given active-head label.
    pub fn new(head_label: impl Into<String>) -> Self {
        Self {
            url_text: String::new(),
            url_focused: false,
            can_back: false,
            can_forward: false,
            loading: false,
            head_label: head_label.into(),
        }
    }

    /// Top-left of the page content area (just below the toolbar).
    pub fn content_origin(&self) -> Point {
        Point::new(0, TOOLBAR_HEIGHT as i32)
    }

    /// Size of the page content area for a given window size.
    pub fn content_size(&self, window: Size) -> Size {
        Size::new(window.w, window.h.saturating_sub(TOOLBAR_HEIGHT))
    }

    /// Compute control rectangles for a window width.
    fn layout(&self, window: Size) -> Vec<(Control, Rect)> {
        let mut out = Vec::with_capacity(7);
        let mut x = PAD;
        for c in [
            Control::Back,
            Control::Forward,
            Control::Reload,
            Control::Stop,
        ] {
            out.push((c, Rect::new(x, PAD, BTN, BTN)));
            x += BTN as i32 + PAD;
        }

        // Right-anchored: Settings, then Head, laid out from the right edge.
        let w = window.w as i32;
        let settings_x = (w - PAD - BTN as i32).max(x);
        let head_x = (settings_x - PAD - HEAD_W as i32).max(x);

        // URL box fills the gap between the left group and the head switcher.
        let url_x = x;
        let url_w = (head_x - PAD - url_x).max(0) as u32;
        out.push((Control::UrlBox, Rect::new(url_x, PAD, url_w, BTN)));
        out.push((Control::Head, Rect::new(head_x, PAD, HEAD_W, BTN)));
        out.push((Control::Settings, Rect::new(settings_x, PAD, BTN, BTN)));
        out
    }

    /// Map a click at `(x, y)` to an action. Clicks below the toolbar (in the
    /// page) return [`ToolbarAction::None`].
    pub fn hit_test(&self, window: Size, x: i32, y: i32) -> ToolbarAction {
        if y < 0 || (y as u32) >= TOOLBAR_HEIGHT {
            return ToolbarAction::None;
        }
        for (control, rect) in self.layout(window) {
            if point_in(rect, x, y) {
                return self.action_for(control);
            }
        }
        ToolbarAction::None
    }

    fn action_for(&self, control: Control) -> ToolbarAction {
        match control {
            Control::Back if self.can_back => ToolbarAction::Back,
            Control::Forward if self.can_forward => ToolbarAction::Forward,
            Control::Reload => ToolbarAction::Reload,
            Control::Stop if self.loading => ToolbarAction::Stop,
            Control::UrlBox => ToolbarAction::FocusUrl,
            Control::Head => ToolbarAction::SwitchHead,
            Control::Settings => ToolbarAction::OpenSettings,
            // Disabled controls swallow the click.
            Control::Back | Control::Forward | Control::Stop => ToolbarAction::None,
        }
    }

    /// Append a character to the URL box (only when focused).
    pub fn type_char(&mut self, c: char) {
        if self.url_focused && !c.is_control() {
            self.url_text.push(c);
        }
    }

    /// Delete the last character of the URL box (only when focused).
    pub fn backspace(&mut self) {
        if self.url_focused {
            self.url_text.pop();
        }
    }

    /// Submit the URL box, producing a [`ToolbarAction::Navigate`].
    pub fn submit_url(&mut self) -> ToolbarAction {
        self.url_focused = false;
        ToolbarAction::Navigate(self.url_text.clone())
    }

    /// Paint the toolbar into a display list. The page is painted separately
    /// into the content area below.
    pub fn paint(&self, window: Size, shaper: &dyn TextShaper) -> DisplayList {
        let mut list = DisplayList::new();

        // Toolbar background + a hairline separator at the bottom.
        list.push(DisplayItem::Rect {
            rect: Rect::new(0, 0, window.w, TOOLBAR_HEIGHT),
            color: Color::rgb(0xEC, 0xEC, 0xEC),
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(0, TOOLBAR_HEIGHT as i32 - 1, window.w, 1),
            color: Color::rgb(0xC8, 0xC8, 0xC8),
        });

        for (control, rect) in self.layout(window) {
            let (bg, label, enabled) = self.style(control);
            list.push(DisplayItem::Rect { rect, color: bg });
            if !label.is_empty() {
                let color = if enabled {
                    Color::rgb(0x20, 0x20, 0x20)
                } else {
                    Color::rgb(0xA0, 0xA0, 0xA0)
                };
                let glyphs = shaper.shape(&label, LABEL_PX);
                list.push(DisplayItem::Glyphs {
                    origin: Point::new(rect.x + 6, rect.y + 6),
                    glyphs,
                    color,
                });
            }
        }
        list
    }

    /// Background color, label, and enabled-state for a control.
    fn style(&self, control: Control) -> (Color, String, bool) {
        let btn_bg = Color::rgb(0xDC, 0xDC, 0xDC);
        let box_bg = if self.url_focused {
            Color::rgb(0xFF, 0xFF, 0xFF)
        } else {
            Color::rgb(0xF6, 0xF6, 0xF6)
        };
        match control {
            Control::Back => (btn_bg, "<".into(), self.can_back),
            Control::Forward => (btn_bg, ">".into(), self.can_forward),
            Control::Reload => (btn_bg, "R".into(), true),
            Control::Stop => (btn_bg, "X".into(), self.loading),
            Control::UrlBox => (box_bg, self.url_display(), true),
            Control::Head => (Color::rgb(0xD0, 0xDC, 0xF0), self.head_label.clone(), true),
            Control::Settings => (btn_bg, "S".into(), true),
        }
    }

    fn url_display(&self) -> String {
        if self.url_text.is_empty() && !self.url_focused {
            "Search or enter address".to_string()
        } else {
            self.url_text.clone()
        }
    }
}

fn point_in(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && y >= rect.y && x < rect.x + rect.w as i32 && y < rect.y + rect.h as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    fn window() -> Size {
        Size::new(800, 600)
    }

    #[test]
    fn content_area_sits_below_the_toolbar() {
        let t = Toolbar::new("work");
        assert_eq!(t.content_origin(), Point::new(0, TOOLBAR_HEIGHT as i32));
        assert_eq!(
            t.content_size(window()),
            Size::new(800, 600 - TOOLBAR_HEIGHT)
        );
    }

    #[test]
    fn back_is_disabled_until_there_is_history() {
        let mut t = Toolbar::new("work");
        let (bx, by) = (PAD + (BTN as i32) / 2, PAD + (BTN as i32) / 2);
        assert_eq!(t.hit_test(window(), bx, by), ToolbarAction::None);
        t.can_back = true;
        assert_eq!(t.hit_test(window(), bx, by), ToolbarAction::Back);
    }

    #[test]
    fn settings_and_head_are_right_anchored() {
        let t = Toolbar::new("work");
        let w = window();
        let settings_x = w.w as i32 - PAD - (BTN as i32) / 2;
        assert_eq!(
            t.hit_test(w, settings_x, PAD + 2),
            ToolbarAction::OpenSettings
        );
        let head_x = w.w as i32 - PAD - BTN as i32 - PAD - (HEAD_W as i32) / 2;
        assert_eq!(t.hit_test(w, head_x, PAD + 2), ToolbarAction::SwitchHead);
    }

    #[test]
    fn clicking_the_middle_focuses_the_url_box() {
        let t = Toolbar::new("work");
        assert_eq!(t.hit_test(window(), 400, PAD + 2), ToolbarAction::FocusUrl);
    }

    #[test]
    fn clicks_in_the_page_area_are_not_toolbar() {
        let t = Toolbar::new("work");
        assert_eq!(
            t.hit_test(window(), 400, TOOLBAR_HEIGHT as i32 + 10),
            ToolbarAction::None
        );
    }

    #[test]
    fn url_editing_and_submit() {
        let mut t = Toolbar::new("work");
        t.url_focused = true;
        for ch in "cerberus:home".chars() {
            t.type_char(ch);
        }
        t.backspace();
        assert_eq!(t.url_text, "cerberus:hom");
        assert_eq!(
            t.submit_url(),
            ToolbarAction::Navigate("cerberus:hom".to_string())
        );
        assert!(!t.url_focused);
    }

    #[test]
    fn paint_produces_toolbar_and_controls() {
        let t = Toolbar::new("work");
        let list = t.paint(window(), &MonoShaper);
        let rects = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Rect { .. }))
            .count();
        assert!(rects >= 9, "got {rects} rects");
    }
}

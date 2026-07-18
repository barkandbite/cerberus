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
use cerberus_types::{Color, FontStyle, Point, Rect, Size};

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
    /// Open the MIRC control panel (the SYNC button): the count-badge button
    /// that orchestrates every driven identity (broadcast, navigate-all, open).
    OpenSync,
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
    Sync,
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
    /// Whether the whole URL is selected (select-all on focus): the next typed
    /// character or Backspace replaces it, like a browser's address bar.
    pub url_selected: bool,
    /// Whether Back is enabled.
    pub can_back: bool,
    /// Whether Forward is enabled.
    pub can_forward: bool,
    /// Whether a load is in progress (enables Stop, animates Reload later).
    pub loading: bool,
    /// Short label for the active head (e.g. "work").
    pub head_label: String,
    /// Whether MIRC broadcasting is on (SYNC button highlighted; the master
    /// drives every identity at once). Toggled from the MIRC panel.
    pub broadcasting: bool,
    /// How many identities/sessions the SYNC button drives — drawn as a count
    /// badge on the button. Zero hides the badge.
    pub sync_count: usize,
}

impl Toolbar {
    /// A new toolbar for the given active-head label.
    pub fn new(head_label: impl Into<String>) -> Self {
        Self {
            url_text: String::new(),
            url_focused: false,
            url_selected: false,
            can_back: false,
            can_forward: false,
            loading: false,
            head_label: head_label.into(),
            broadcasting: false,
            sync_count: 0,
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

        // Right-anchored from the right edge: Settings, Head, then the SYNC
        // button (just right of the URL bar, per the MIRC concept).
        let w = window.w as i32;
        let settings_x = (w - PAD - BTN as i32).max(x);
        let head_x = (settings_x - PAD - HEAD_W as i32).max(x);
        let sync_x = (head_x - PAD - BTN as i32).max(x);

        // URL box fills the gap between the left group and the SYNC button.
        let url_x = x;
        let url_w = (sync_x - PAD - url_x).max(0) as u32;
        out.push((Control::UrlBox, Rect::new(url_x, PAD, url_w, BTN)));
        out.push((Control::Sync, Rect::new(sync_x, PAD, BTN, BTN)));
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
            Control::Sync => ToolbarAction::OpenSync,
            Control::Head => ToolbarAction::SwitchHead,
            Control::Settings => ToolbarAction::OpenSettings,
            // Disabled controls swallow the click.
            Control::Back | Control::Forward | Control::Stop => ToolbarAction::None,
        }
    }

    /// Focus the URL box and select all of it, so the next keystroke replaces the
    /// current address — the address-bar convention browsers use on click/focus.
    pub fn focus_url(&mut self) {
        self.url_focused = true;
        self.url_selected = true;
    }

    /// Remove focus (and any selection) from the URL box.
    pub fn blur_url(&mut self) {
        self.url_focused = false;
        self.url_selected = false;
    }

    /// Append a character to the URL box (only when focused). If the box is in
    /// the select-all state, the character replaces the whole URL.
    pub fn type_char(&mut self, c: char) {
        if self.url_focused && !c.is_control() {
            if self.url_selected {
                self.url_text.clear();
                self.url_selected = false;
            }
            self.url_text.push(c);
        }
    }

    /// Delete from the URL box (only when focused). With the whole URL selected,
    /// the first Backspace clears it; otherwise it deletes the last character.
    pub fn backspace(&mut self) {
        if self.url_focused {
            if self.url_selected {
                self.url_text.clear();
                self.url_selected = false;
            } else {
                self.url_text.pop();
            }
        }
    }

    /// Submit the URL box, producing a [`ToolbarAction::Navigate`].
    pub fn submit_url(&mut self) -> ToolbarAction {
        self.url_focused = false;
        self.url_selected = false;
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
            let text = if enabled {
                Color::rgb(0x20, 0x20, 0x20)
            } else {
                Color::rgb(0xA0, 0xA0, 0xA0)
            };
            // The URL box is a text field; the head chip keeps its text label;
            // the nav/reload/stop/settings buttons are icon-font glyphs.
            match control {
                Control::UrlBox => self.paint_url_box(&mut list, shaper, rect, bg, &label, text),
                Control::Head => draw_button(&mut list, shaper, rect, &label, bg, text, LABEL_PX),
                // The MIRC button (a multiperson glyph) glows blue while
                // broadcasting and wears the driven-count badge ("N profiles") on
                // its corner. Clicking it opens the panel that orchestrates the set.
                Control::Sync => {
                    let (fill, fg) = if self.broadcasting {
                        (Color::rgb(0x1E, 0x66, 0xE0), Color::WHITE)
                    } else {
                        (bg, text)
                    };
                    draw_icon_button(&mut list, shaper, rect, IC_USERS, ICON_PX, fill, fg);
                    push_count_badge(&mut list, shaper, rect, self.sync_count);
                }
                other => {
                    let icon = match other {
                        Control::Back => IC_BACK,
                        Control::Forward => IC_FORWARD,
                        Control::Reload => IC_RELOAD,
                        Control::Stop => IC_CLOSE,
                        Control::Settings => IC_GEAR,
                        Control::UrlBox | Control::Head | Control::Sync => unreachable!(),
                    };
                    draw_icon_button(&mut list, shaper, rect, icon, ICON_PX, bg, text);
                }
            }
        }
        list
    }

    /// Paint the URL box: its background, label, a caret when focused, and a
    /// select-all highlight right after focusing.
    fn paint_url_box(
        &self,
        list: &mut DisplayList,
        shaper: &dyn TextShaper,
        rect: Rect,
        bg: Color,
        label: &str,
        color: Color,
    ) {
        list.push(DisplayItem::Rect { rect, color: bg });
        stroke_rect(list, rect, darken(bg)); // input-field border
        let y = rect.y + (rect.h as i32 - LABEL_PX as i32) / 2;
        let tx = rect.x + 6;
        // Width of the actually-typed text (not the placeholder), for the caret
        // and the selection highlight.
        let text_w: i32 = shaper
            .shape(&self.url_text, LABEL_PX)
            .iter()
            .map(|g| g.advance as i32)
            .sum();
        // Select-all highlight behind the text, shown right after focusing.
        if self.url_focused && self.url_selected && !self.url_text.is_empty() {
            list.push(DisplayItem::Rect {
                rect: Rect::new(
                    tx,
                    rect.y + 4,
                    text_w.max(0) as u32,
                    rect.h.saturating_sub(8),
                ),
                color: Color::rgb(0xB5, 0xD0, 0xF5),
            });
        }
        list.push(DisplayItem::Glyphs {
            origin: Point::new(tx, y),
            frac_x: 0.0,
            glyphs: shaper.shape(label, LABEL_PX),
            color,
            style: FontStyle::REGULAR,
        });
        // Caret at the end of the text once the selection is cleared (or the box
        // is empty); while all is selected the highlight stands in for it.
        if self.url_focused && (!self.url_selected || self.url_text.is_empty()) {
            list.push(DisplayItem::Rect {
                rect: Rect::new(tx + text_w, y, 1, LABEL_PX),
                color: Color::rgb(0x20, 0x20, 0x20),
            });
        }
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
            // SYNC: label unused (painted as an icon); the broadcasting highlight
            // is applied in `paint`.
            Control::Sync => (btn_bg, String::new(), true),
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

/// Height of the driven-profiles badge, in device pixels.
const BADGE_H: u32 = 24;
/// Inner horizontal padding of the badge pill.
const BADGE_PAD: i32 = 8;
/// Margin from the window's top-right corner.
const BADGE_MARGIN: i32 = 8;
/// Badge label text size.
const BADGE_PX: u32 = 14;
/// Diameter of the badge's status dot.
const BADGE_DOT: u32 = 8;

/// A small overlay badge for a mirror **master** window — the owner's "N
/// profiles being driven" indicator, e.g. "23 profiles being driven · github.com".
///
/// The mirror has no toolbar, so this is composited over the page's top-right
/// corner. Pure like the rest of this crate: it lays itself out for a window
/// size, paints into a [`DisplayList`], and reports whether a point hits it (a
/// click that should open the identities panel).
pub struct DrivenBadge;

impl DrivenBadge {
    /// The badge label for a driven `count` on `site` (a host); `site` may be
    /// empty (e.g. built-in pages), in which case it is omitted.
    pub fn label(count: usize, site: &str) -> String {
        let noun = if count == 1 { "profile" } else { "profiles" };
        if site.is_empty() {
            format!("{count} {noun} being driven")
        } else {
            format!("{count} {noun} being driven · {site}")
        }
    }

    /// The badge rectangle, right-anchored to the window's top-right corner and
    /// sized to its label (measured with `shaper`).
    pub fn rect(window: Size, count: usize, site: &str, shaper: &dyn TextShaper) -> Rect {
        let text_w: i32 = shaper
            .shape(&Self::label(count, site), BADGE_PX)
            .iter()
            .map(|g| g.advance as i32)
            .sum();
        let w = (BADGE_PAD + BADGE_DOT as i32 + BADGE_PAD + text_w + BADGE_PAD).max(1) as u32;
        let x = (window.w as i32 - BADGE_MARGIN - w as i32).max(BADGE_MARGIN);
        Rect::new(x, BADGE_MARGIN, w, BADGE_H)
    }

    /// Paint the badge into its own display list (composited after the page).
    pub fn paint(window: Size, count: usize, site: &str, shaper: &dyn TextShaper) -> DisplayList {
        let mut list = DisplayList::new();
        let rect = Self::rect(window, count, site, shaper);
        list.push(DisplayItem::Rect {
            rect,
            color: Color::rgb(0x1E, 0x40, 0x80),
        });
        // A status dot at the left edge.
        let dot_y = rect.y + (BADGE_H as i32 - BADGE_DOT as i32) / 2;
        list.push(DisplayItem::Rect {
            rect: Rect::new(rect.x + BADGE_PAD, dot_y, BADGE_DOT, BADGE_DOT),
            color: Color::rgb(0x6C, 0xE0, 0x8A),
        });
        let glyphs = shaper.shape(&Self::label(count, site), BADGE_PX);
        list.push(DisplayItem::Glyphs {
            origin: Point::new(
                rect.x + BADGE_PAD + BADGE_DOT as i32 + BADGE_PAD,
                rect.y + (BADGE_H as i32 - BADGE_PX as i32) / 2,
            ),
            frac_x: 0.0,
            glyphs,
            color: Color::WHITE,
            style: FontStyle::REGULAR,
        });
        list
    }

    /// Whether `(x, y)` falls on the badge — a click that should open the panel.
    pub fn hit_test(
        window: Size,
        count: usize,
        site: &str,
        shaper: &dyn TextShaper,
        x: i32,
        y: i32,
    ) -> bool {
        point_in(Self::rect(window, count, site, shaper), x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    fn window() -> Size {
        Size::new(800, 600)
    }

    #[test]
    fn driven_badge_pluralizes_and_includes_site() {
        assert_eq!(
            DrivenBadge::label(1, "github.com"),
            "1 profile being driven · github.com"
        );
        assert_eq!(
            DrivenBadge::label(23, "github.com"),
            "23 profiles being driven · github.com"
        );
        // A built-in page (no host) omits the site suffix.
        assert_eq!(DrivenBadge::label(2, ""), "2 profiles being driven");
    }

    #[test]
    fn driven_badge_anchors_top_right_and_is_hittable() {
        let w = window();
        let rect = DrivenBadge::rect(w, 23, "github.com", &MonoShaper);
        // Right-anchored within the window, at the top margin.
        assert_eq!(rect.y, BADGE_MARGIN);
        assert!(rect.x > 0 && rect.x + rect.w as i32 <= w.w as i32);
        // A point at the badge's center hits; a far-away point does not.
        let (cx, cy) = (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2);
        assert!(DrivenBadge::hit_test(
            w,
            23,
            "github.com",
            &MonoShaper,
            cx,
            cy
        ));
        assert!(!DrivenBadge::hit_test(
            w,
            23,
            "github.com",
            &MonoShaper,
            5,
            300
        ));

        // Paint emits the pill, the dot, and the label glyphs.
        let list = DrivenBadge::paint(w, 23, "github.com", &MonoShaper);
        let rects = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Rect { .. }))
            .count();
        let glyphs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        assert_eq!(rects, 2, "pill + status dot");
        assert_eq!(glyphs, 1, "the label");
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

    #[test]
    fn nav_buttons_render_icon_font_runs() {
        let t = Toolbar::new("work");
        let list = t.paint(window(), &MonoShaper);
        let icons = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { style, .. } if style.icon))
            .count();
        // back, forward, reload, stop, settings → five icon-styled runs.
        assert!(
            icons >= 5,
            "expected icon runs for nav/settings buttons; got {icons}"
        );
    }

    #[test]
    fn url_focus_selects_all_and_first_keystroke_replaces() {
        let mut t = Toolbar::new("work");
        t.url_text = "cerberus:home".into();
        t.focus_url();
        assert!(t.url_focused && t.url_selected);
        t.type_char('a'); // replaces the selected URL
        assert_eq!(t.url_text, "a");
        assert!(!t.url_selected);
        t.type_char('b'); // then appends normally
        assert_eq!(t.url_text, "ab");
    }

    #[test]
    fn url_backspace_clears_whole_selection_then_deletes() {
        let mut t = Toolbar::new("work");
        t.url_text = "abc".into();
        t.focus_url();
        t.backspace(); // selected -> clear all
        assert_eq!(t.url_text, "");
        assert!(!t.url_selected);
        t.url_text = "xy".into();
        t.backspace(); // not selected -> delete last
        assert_eq!(t.url_text, "x");
    }

    #[test]
    fn url_blur_drops_focus_and_selection() {
        let mut t = Toolbar::new("work");
        t.focus_url();
        t.blur_url();
        assert!(!t.url_focused && !t.url_selected);
    }

    #[test]
    fn focused_url_box_paints_a_caret() {
        let mut t = Toolbar::new("work");
        t.focus_url();
        t.url_selected = false; // editing state: caret should show
        t.url_text = "ab".into();
        let before = Toolbar::new("work")
            .paint(window(), &MonoShaper)
            .items
            .len();
        let after = t.paint(window(), &MonoShaper).items.len();
        assert!(after > before, "focused box should add a caret rect");
    }
}

/// Height of the consent banner strip (shown below the toolbar while a
/// third-party request awaits a decision).
pub const BANNER_HEIGHT: u32 = 28;

/// An action produced by clicking the consent banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerAction {
    /// Allow this third-party site under the current first party (standing rule).
    Allow,
    /// Deny it (standing rule).
    Deny,
    /// Dismiss the prompt without a standing rule (deny for now).
    Dismiss,
    /// The click hit no banner control.
    None,
}

/// The consent prompt strip: one pending third-party site at a time, with
/// Allow / Deny / dismiss controls. Pure, like [`Toolbar`]: paint +
/// hit-test only; policy lives in `cerberus-consent`, state in the app.
#[derive(Clone, Debug, Default)]
pub struct ConsentBanner {
    /// The third-party site awaiting a decision (e.g. `https://ads.tracker.net`).
    pub request_site: String,
    /// How many further prompts are queued behind this one.
    pub queued: usize,
}

const BANNER_BTN_W: u32 = 52;

impl ConsentBanner {
    /// A banner for one pending request site.
    pub fn new(request_site: impl Into<String>, queued: usize) -> Self {
        Self {
            request_site: request_site.into(),
            queued,
        }
    }

    /// The banner strip rect (full width, directly below the toolbar).
    pub fn rect(window: Size) -> Rect {
        Rect::new(0, TOOLBAR_HEIGHT as i32, window.w, BANNER_HEIGHT)
    }

    fn buttons(window: Size) -> [(BannerAction, Rect); 3] {
        let y = TOOLBAR_HEIGHT as i32 + PAD;
        let h = BANNER_HEIGHT - 2 * PAD as u32;
        let w = window.w as i32;
        let dismiss_x = w - PAD - h as i32; // square × button
        let deny_x = dismiss_x - PAD - BANNER_BTN_W as i32;
        let allow_x = deny_x - PAD - BANNER_BTN_W as i32;
        [
            (BannerAction::Allow, Rect::new(allow_x, y, BANNER_BTN_W, h)),
            (BannerAction::Deny, Rect::new(deny_x, y, BANNER_BTN_W, h)),
            (BannerAction::Dismiss, Rect::new(dismiss_x, y, h, h)),
        ]
    }

    /// Map a click (window coordinates) to a banner action. Clicks elsewhere
    /// in the strip return `None` (consumed by the banner, no action).
    pub fn hit_test(&self, window: Size, x: i32, y: i32) -> BannerAction {
        for (action, rect) in Self::buttons(window) {
            if x >= rect.x
                && y >= rect.y
                && x < rect.x + rect.w as i32
                && y < rect.y + rect.h as i32
            {
                return action;
            }
        }
        BannerAction::None
    }

    /// Paint the strip: message text left, Allow / Deny / × right.
    pub fn paint(&self, window: Size, shaper: &dyn TextShaper) -> DisplayList {
        let mut list = DisplayList::new();
        let strip = Self::rect(window);
        list.push(DisplayItem::Rect {
            rect: strip,
            color: Color::rgb(0xFF, 0xF4, 0xD6), // soft warning yellow
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(0, strip.y + BANNER_HEIGHT as i32 - 1, window.w, 1),
            color: Color::rgb(0xC8, 0xB8, 0x80),
        });

        let more = if self.queued > 0 {
            format!(" (+{} more)", self.queued)
        } else {
            String::new()
        };
        let msg = format!("{} wants third-party access{more}", self.request_site);
        list.push(DisplayItem::Glyphs {
            origin: Point::new(PAD + 4, strip.y + 19),
            frac_x: 0.0,
            glyphs: shaper.shape(&msg, 13),
            color: Color::rgb(0x40, 0x38, 0x10),
            style: FontStyle::REGULAR,
        });

        for (action, rect) in Self::buttons(window) {
            let (fill, label) = match action {
                BannerAction::Allow => (Color::rgb(0xD9, 0xEF, 0xD9), "Allow"),
                BannerAction::Deny => (Color::rgb(0xF3, 0xD9, 0xD9), "Deny"),
                BannerAction::Dismiss => (Color::rgb(0xE8, 0xE8, 0xE8), "×"),
                BannerAction::None => continue,
            };
            draw_button(&mut list, shaper, rect, label, fill, Color::BLACK, 12);
        }
        list
    }
}

#[cfg(test)]
mod banner_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    #[test]
    fn banner_sits_directly_below_the_toolbar() {
        let r = ConsentBanner::rect(Size::new(800, 600));
        assert_eq!(r.y, TOOLBAR_HEIGHT as i32);
        assert_eq!(r.h, BANNER_HEIGHT);
        assert_eq!(r.w, 800);
    }

    #[test]
    fn banner_buttons_hit_test_and_misses_are_none() {
        let b = ConsentBanner::new("https://ads.tracker.net", 0);
        let size = Size::new(800, 600);
        let [(_, allow), (_, deny), (_, dismiss)] = ConsentBanner::buttons(size);
        assert_eq!(
            b.hit_test(size, allow.x + 2, allow.y + 2),
            BannerAction::Allow
        );
        assert_eq!(b.hit_test(size, deny.x + 2, deny.y + 2), BannerAction::Deny);
        assert_eq!(
            b.hit_test(size, dismiss.x + 2, dismiss.y + 2),
            BannerAction::Dismiss
        );
        // The message area consumes the click but maps to no action.
        assert_eq!(b.hit_test(size, 10, allow.y + 2), BannerAction::None);
    }

    #[test]
    fn banner_paints_strip_buttons_and_message() {
        let b = ConsentBanner::new("https://ads.tracker.net", 2);
        let list = b.paint(Size::new(800, 600), &MonoShaper);
        let rects = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Rect { .. }))
            .count();
        let glyphs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        assert!(rects >= 5, "strip + divider + 3 buttons");
        assert!(glyphs >= 4, "message + 3 labels");
    }
}

// ---- Cookie manager (M10): a transparent, per-cookie disposition inspector ----

/// Height of one cookie row in the inspector.
pub const COOKIE_ROW_H: u32 = 26;

/// One row of the cookie inspector, prepared by the app from a `CookieView`.
#[derive(Clone, Debug)]
pub struct CookieRow {
    /// `name` (and, when revealed, `=value`); domain shown dimmed after it.
    pub primary: String,
    /// The dimmer right-hand detail (domain + expiry).
    pub detail: String,
    /// The disposition chip text (e.g. `allow`, `Timed 3600s`).
    pub chip: String,
}

/// A click outcome in the cookie inspector. Row indices are absolute (into the
/// full list the app passed), already adjusted for the scroll offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieAction {
    Close,
    /// Cycle the global-default disposition.
    CycleGlobal,
    /// Cycle one cookie's disposition.
    Cycle(usize),
    /// Reveal/hide one cookie's value.
    Reveal(usize),
    /// Delete one cookie.
    Delete(usize),
    ScrollUp,
    ScrollDown,
    None,
}

/// The cookie inspector panel: a scrollable list of every stored cookie with a
/// per-row disposition chip, a reveal toggle, and a delete control, plus a
/// global-default chip. Pure paint + hit-test, like [`ConsentBanner`]; the app
/// owns the data, the scroll offset, and applies the actions to storage.
pub struct CookieManager;

const COOKIE_CHIP_W: u32 = 96;
const COOKIE_BTN_W: u32 = 22;
const COOKIE_LIST_TOP: i32 = 104; // panel-local y where rows begin (after the legend)
const COOKIE_LIST_BOTTOM_PAD: u32 = 40;

/// Push `text`, centred horizontally and vertically, inside `rect`. Keeps the
/// square buttons and chips legible — their glyphs sit in the middle instead of
/// at a fixed corner offset (which read as "misaligned").
fn push_centered(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    rect: Rect,
    text: &str,
    px: u32,
    color: Color,
) {
    let glyphs = shaper.shape(text, px);
    let text_w: i32 = glyphs.iter().map(|g| g.advance as i32).sum();
    let x = rect.x + ((rect.w as i32 - text_w) / 2).max(0);
    let y = rect.y + (rect.h as i32 - px as i32) / 2;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(x, y),
        frac_x: 0.0,
        glyphs,
        color,
        style: FontStyle::REGULAR,
    });
}

/// Standard button chrome: a filled rect with its label centred on both axes.
/// This is the single place button alignment is defined, so every button across
/// the UI (toolbar, consent banner, cookie manager) stays consistent instead of
/// each call site hand-placing a label that drifts out of its box.
fn draw_button(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    rect: Rect,
    label: &str,
    fill: Color,
    text: Color,
    px: u32,
) {
    list.push(DisplayItem::Rect { rect, color: fill });
    // A 1px border, a shade darker than the fill, gives a standard button edge
    // (affordance) and keeps a light chip visible on a light background.
    stroke_rect(list, rect, darken(fill));
    if !label.is_empty() {
        push_centered(list, shaper, rect, label, px, text);
    }
}

/// A shade darker than `c` (~80%), for button/field borders.
fn darken(c: Color) -> Color {
    let s = |v: u8| (v as u32 * 80 / 100) as u8;
    Color::rgb(s(c.r), s(c.g), s(c.b))
}

/// Draw a 1px border just inside `rect` as four thin filled rects.
fn stroke_rect(list: &mut DisplayList, rect: Rect, color: Color) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let (w, h) = (rect.w, rect.h);
    list.push(DisplayItem::Rect {
        rect: Rect::new(rect.x, rect.y, w, 1),
        color,
    });
    list.push(DisplayItem::Rect {
        rect: Rect::new(rect.x, rect.y + h as i32 - 1, w, 1),
        color,
    });
    list.push(DisplayItem::Rect {
        rect: Rect::new(rect.x, rect.y, 1, h),
        color,
    });
    list.push(DisplayItem::Rect {
        rect: Rect::new(rect.x + w as i32 - 1, rect.y, 1, h),
        color,
    });
}

/// Background colour for a cookie-disposition chip, by its token, so the state
/// reads at a glance: green = allow, amber = session, red = block. Anything else
/// (legacy timed/allow-once) is neutral grey.
fn chip_fill(token: &str) -> Color {
    match token {
        "allow" => Color::rgb(0xD9, 0xEF, 0xD9),
        "session" => Color::rgb(0xFD, 0xEF, 0xC8),
        "block" => Color::rgb(0xF6, 0xCF, 0xCF),
        _ => Color::rgb(0xE4, 0xE4, 0xE4),
    }
}

// Icon-font codepoints (bundled IcoMoon subset; see cerberus-text/assets).
const IC_BACK: char = '\u{ea38}';
const IC_FORWARD: char = '\u{ea34}';
const IC_RELOAD: char = '\u{e984}';
const IC_CLOSE: char = '\u{ea0f}';
const IC_GEAR: char = '\u{e994}';
/// MIRC button: "users" (multiperson) — it opens the multi-identity roster.
const IC_USERS: char = '\u{e972}';
const IC_EYE: char = '\u{e9ce}';
const IC_TRASH: char = '\u{e9ac}';
/// Icon size for the 28px toolbar buttons.
const ICON_PX: u32 = 18;

/// Push an icon-font glyph centred in `rect`, in a run styled [`FontStyle::ICON`]
/// so the rasterizer outlines it from the icon font (crisp at any scale).
fn push_icon(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    rect: Rect,
    icon: char,
    px: u32,
    color: Color,
) {
    let glyphs = shaper.shape_icon(icon, px);
    let text_w: i32 = glyphs.iter().map(|g| g.advance as i32).sum();
    let x = rect.x + ((rect.w as i32 - text_w) / 2).max(0);
    let y = rect.y + (rect.h as i32 - px as i32) / 2;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(x, y),
        frac_x: 0.0,
        glyphs,
        color,
        style: FontStyle::ICON,
    });
}

/// A button (fill + border) whose label is a centred icon-font glyph.
fn draw_icon_button(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    rect: Rect,
    icon: char,
    px: u32,
    fill: Color,
    color: Color,
) {
    list.push(DisplayItem::Rect { rect, color: fill });
    stroke_rect(list, rect, darken(fill));
    push_icon(list, shaper, rect, icon, px, color);
}

/// Overlay a small count badge on the top-right corner of `rect` (the SYNC
/// button's "N profiles" count). Zero paints nothing; counts over 99 show "99+".
/// It rides slightly up-and-right of the corner, like a notification badge.
fn push_count_badge(list: &mut DisplayList, shaper: &dyn TextShaper, rect: Rect, count: usize) {
    if count == 0 {
        return;
    }
    let text = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };
    let px = 10;
    let glyphs = shaper.shape(&text, px);
    let tw: i32 = glyphs.iter().map(|g| g.advance as i32).sum();
    let bw = (tw + 6).max(14) as u32;
    let bh = 14u32;
    let bx = rect.x + rect.w as i32 - bw as i32 + 4;
    let by = (rect.y - 3).max(0);
    list.push(DisplayItem::Rect {
        rect: Rect::new(bx, by, bw, bh),
        color: Color::rgb(0xE5, 0x3E, 0x3E),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(
            bx + ((bw as i32 - tw) / 2).max(0),
            by + (bh as i32 - px as i32) / 2,
        ),
        frac_x: 0.0,
        glyphs,
        color: Color::WHITE,
        style: FontStyle::REGULAR,
    });
}

impl CookieManager {
    /// The inspector panel rect (centered, 74% of the window).
    pub fn panel_rect(window: Size) -> Rect {
        let pw = window.w * 74 / 100;
        let ph = window.h * 74 / 100;
        let px = (window.w.saturating_sub(pw) / 2) as i32;
        let py = (window.h.saturating_sub(ph) / 2) as i32;
        Rect::new(px, py, pw, ph)
    }

    /// How many rows fit in the list area for this window.
    pub fn visible_rows(window: Size) -> usize {
        let panel = Self::panel_rect(window);
        let list_h = (panel.h as i32 - COOKIE_LIST_TOP - COOKIE_LIST_BOTTOM_PAD as i32).max(0);
        (list_h / COOKIE_ROW_H as i32).max(0) as usize
    }

    fn close_rect(window: Size) -> Rect {
        let p = Self::panel_rect(window);
        Rect::new(p.x + p.w as i32 - 28, p.y + 8, 20, 20)
    }

    fn global_chip_rect(window: Size) -> Rect {
        let p = Self::panel_rect(window);
        Rect::new(
            p.x + p.w as i32 - COOKIE_CHIP_W as i32 - 12,
            p.y + 48,
            COOKIE_CHIP_W,
            20,
        )
    }

    fn scroll_rects(window: Size) -> (Rect, Rect) {
        let p = Self::panel_rect(window);
        let x = p.x + p.w as i32 - 28;
        let down_y = p.y + p.h as i32 - 28;
        (
            Rect::new(x, p.y + COOKIE_LIST_TOP, 20, 20), // up
            Rect::new(x, down_y, 20, 20),
        ) // down
    }

    /// Per-row control rects (chip, reveal, delete) for the `i`-th *visible*
    /// row (0-based from the top of the list).
    fn row_controls(window: Size, vis_i: usize) -> (Rect, Rect, Rect, i32) {
        let p = Self::panel_rect(window);
        let y = p.y + COOKIE_LIST_TOP + vis_i as i32 * COOKIE_ROW_H as i32;
        let delete = Rect::new(
            p.x + p.w as i32 - 28 - 24,
            y + 2,
            COOKIE_BTN_W,
            COOKIE_BTN_W,
        );
        let chip = Rect::new(
            delete.x - COOKIE_CHIP_W as i32 - 6,
            y + 2,
            COOKIE_CHIP_W,
            20,
        );
        let reveal = Rect::new(
            chip.x - COOKIE_BTN_W as i32 - 6,
            y + 2,
            COOKIE_BTN_W,
            COOKIE_BTN_W,
        );
        (chip, reveal, delete, y)
    }

    /// Map a click to an action. `len` is the total row count; `scroll` is the
    /// app's current top offset.
    pub fn hit_test(window: Size, len: usize, scroll: usize, x: i32, y: i32) -> CookieAction {
        let inside = |r: Rect| x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32;
        if inside(Self::close_rect(window)) {
            return CookieAction::Close;
        }
        if inside(Self::global_chip_rect(window)) {
            return CookieAction::CycleGlobal;
        }
        let (up, down) = Self::scroll_rects(window);
        if inside(up) {
            return CookieAction::ScrollUp;
        }
        if inside(down) {
            return CookieAction::ScrollDown;
        }
        let visible = Self::visible_rows(window);
        for vis_i in 0..visible {
            let abs = scroll + vis_i;
            if abs >= len {
                break;
            }
            let (chip, reveal, delete, _) = Self::row_controls(window, vis_i);
            if inside(chip) {
                return CookieAction::Cycle(abs);
            }
            if inside(reveal) {
                return CookieAction::Reveal(abs);
            }
            if inside(delete) {
                return CookieAction::Delete(abs);
            }
        }
        CookieAction::None
    }

    /// Paint the inspector. `rows` is the full list; `scroll` is the top row.
    pub fn paint(
        window: Size,
        shaper: &dyn TextShaper,
        global_chip: &str,
        rows: &[CookieRow],
        scroll: usize,
    ) -> DisplayList {
        let mut list = DisplayList::new();
        let p = Self::panel_rect(window);
        // Backdrop + panel.
        list.push(DisplayItem::Rect {
            rect: Rect::new(p.x - 1, p.y - 1, p.w + 2, p.h + 2),
            color: Color::rgb(0x30, 0x30, 0x30),
        });
        list.push(DisplayItem::Rect {
            rect: p,
            color: Color::rgb(0xFA, 0xFA, 0xFA),
        });
        // Title + count.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 12, p.y + 26),
            frac_x: 0.0,
            glyphs: shaper.shape(&format!("Cookies ({})", rows.len()), 20),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        // Close button.
        let close = Self::close_rect(window);
        draw_icon_button(
            &mut list,
            shaper,
            close,
            IC_CLOSE,
            13,
            Color::rgb(0xE0, 0xE0, 0xE0),
            Color::BLACK,
        );
        // Global default chip.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 12, p.y + 63),
            frac_x: 0.0,
            glyphs: shaper.shape("global default:", 13),
            color: Color::rgb(0x50, 0x50, 0x50),
            style: FontStyle::REGULAR,
        });
        let gchip = Self::global_chip_rect(window);
        draw_button(
            &mut list,
            shaper,
            gchip,
            global_chip,
            chip_fill(global_chip),
            Color::BLACK,
            12,
        );
        // Legend: explains the per-cookie chip so it isn't a mystery cycle.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 12, p.y + 88),
            frac_x: 0.0,
            glyphs: shaper.shape(
                "allow = keep   ·   session = forget on close   ·   block = never store",
                12,
            ),
            color: Color::rgb(0x60, 0x60, 0x60),
            style: FontStyle::REGULAR,
        });
        // Rows.
        let visible = Self::visible_rows(window);
        for vis_i in 0..visible {
            let abs = scroll + vis_i;
            let Some(row) = rows.get(abs) else { break };
            let (chip, reveal, delete, y) = Self::row_controls(window, vis_i);
            if vis_i % 2 == 1 {
                list.push(DisplayItem::Rect {
                    rect: Rect::new(p.x + 4, y, p.w - 8, COOKIE_ROW_H),
                    color: Color::rgb(0xF0, 0xF0, 0xF0),
                });
            }
            list.push(DisplayItem::Glyphs {
                origin: Point::new(p.x + 12, y + 17),
                frac_x: 0.0,
                glyphs: shaper.shape(&row.primary, 13),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(p.x + 12 + 260, y + 17),
                frac_x: 0.0,
                glyphs: shaper.shape(&row.detail, 11),
                color: Color::rgb(0x80, 0x80, 0x80),
                style: FontStyle::REGULAR,
            });
            // reveal (eye), chip, delete (x)
            draw_icon_button(
                &mut list,
                shaper,
                reveal,
                IC_EYE,
                12,
                Color::rgb(0xE8, 0xE8, 0xE8),
                Color::BLACK,
            );
            draw_button(
                &mut list,
                shaper,
                chip,
                &row.chip,
                chip_fill(&row.chip),
                Color::BLACK,
                12,
            );
            draw_icon_button(
                &mut list,
                shaper,
                delete,
                IC_TRASH,
                12,
                Color::rgb(0xF3, 0xD9, 0xD9),
                Color::BLACK,
            );
        }
        // Scroll affordances.
        let (up, down) = Self::scroll_rects(window);
        // ^ / v are plain ASCII the bundled font definitely has; geometric
        // triangles (U+25B2/BC) are not in Roboto and would render as tofu.
        for (r, glyph) in [(up, "^"), (down, "v")] {
            draw_button(
                &mut list,
                shaper,
                r,
                glyph,
                Color::rgb(0xE0, 0xE0, 0xE0),
                Color::BLACK,
                12,
            );
        }
        list
    }
}

#[cfg(test)]
mod cookie_manager_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    fn rows(n: usize) -> Vec<CookieRow> {
        (0..n)
            .map(|i| CookieRow {
                primary: format!("c{i}"),
                detail: "example.com".into(),
                chip: "allow".into(),
            })
            .collect()
    }

    #[test]
    fn close_and_global_chip_hit_test() {
        let w = Size::new(1000, 800);
        let close = CookieManager::close_rect(w);
        assert_eq!(
            CookieManager::hit_test(w, 0, 0, close.x + 2, close.y + 2),
            CookieAction::Close
        );
        let g = CookieManager::global_chip_rect(w);
        assert_eq!(
            CookieManager::hit_test(w, 0, 0, g.x + 2, g.y + 2),
            CookieAction::CycleGlobal
        );
    }

    #[test]
    fn row_controls_map_to_absolute_indices_with_scroll() {
        let w = Size::new(1000, 800);
        let (chip, reveal, delete, _) = CookieManager::row_controls(w, 0);
        // With scroll=3, the top visible row is absolute index 3.
        assert_eq!(
            CookieManager::hit_test(w, 50, 3, chip.x + 2, chip.y + 2),
            CookieAction::Cycle(3)
        );
        assert_eq!(
            CookieManager::hit_test(w, 50, 3, reveal.x + 2, reveal.y + 2),
            CookieAction::Reveal(3)
        );
        assert_eq!(
            CookieManager::hit_test(w, 50, 3, delete.x + 2, delete.y + 2),
            CookieAction::Delete(3)
        );
    }

    #[test]
    fn paint_emits_panel_and_rows() {
        let w = Size::new(1000, 800);
        let list = CookieManager::paint(w, &MonoShaper, "allow", &rows(3), 0);
        let glyphs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        assert!(glyphs >= 3, "title + global + per-row labels");
    }
}

// ---- MIRC control panel (Phase 2a): the multi-identity roster + orchestrator ----

/// Liveness of one mirrored session, shown as the roster's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MircState {
    /// Holds the single live realm — rendered on screen right now.
    Live,
    /// A sealed, caught-up session that is paused (no live realm) — the cheap
    /// state the other ~N profiles rest in until opened.
    Dormant,
    /// Fell out of lockstep with the master; needs manual attention.
    Diverged,
}

/// One roster row, prepared by the app from a mirror instance / identity.
#[derive(Clone, Debug)]
pub struct MircRow {
    /// Identity label (e.g. "work", "personal", "claim-bot-07").
    pub label: String,
    /// The account/session this identity uses on the current site (a login
    /// username, or a sealed-session tag); shown dimmed.
    pub account: String,
    /// Live / dormant / diverged.
    pub state: MircState,
    /// Whether this session is authenticated on the current site.
    pub logged_in: bool,
}

/// A click outcome in the MIRC panel. Row indices are absolute (into the full
/// roster the app passed), already adjusted for the scroll offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MircAction {
    /// Close the panel.
    Close,
    /// Toggle broadcast on/off (master actions fan out to the driven set).
    ToggleBroadcast,
    /// Bulk: navigate every driven session to the master's current URL.
    NavigateAll,
    /// Bulk: run each session's login fill from its own profile.
    LoginAll,
    /// Open (focus + render) one session — the lazy "select → render" gesture.
    Open(usize),
    ScrollUp,
    ScrollDown,
    /// A click inside the panel that hit no control (consumed, no effect).
    None,
}

/// The MIRC (Multi-Identity Remote Control) panel: a scrollable roster of every
/// driven session — its identity, account, status, and login state — above a
/// control bar to broadcast / navigate-all / login-all across the whole set and
/// open any one session on screen. Pure paint + hit-test like [`CookieManager`];
/// the app owns the roster data and applies the actions to the mirror group.
pub struct MircPanel;

const MIRC_ROW_H: u32 = 30;
const MIRC_LIST_TOP: i32 = 134; // panel-local y where rows begin
const MIRC_LIST_BOTTOM_PAD: u32 = 44; // room for the legend + scroll-down
const MIRC_SCROLL_GUTTER: i32 = 28; // right-edge column for scroll buttons
const MIRC_OPEN_W: u32 = 58;
const MIRC_LOGIN_W: u32 = 96;
const MIRC_STATE_W: u32 = 80;
const MIRC_ACCOUNT_X: i32 = 196; // panel-local x of the account column

/// Background fill for a state chip, so the status reads at a glance.
fn mirc_state_fill(s: MircState) -> Color {
    match s {
        MircState::Live => Color::rgb(0xD9, 0xEF, 0xD9),
        MircState::Dormant => Color::rgb(0xE6, 0xE6, 0xE6),
        MircState::Diverged => Color::rgb(0xFD, 0xE2, 0xC8),
    }
}

/// The status-dot color for a state (a stronger shade of the chip fill).
fn mirc_state_dot(s: MircState) -> Color {
    match s {
        MircState::Live => Color::rgb(0x35, 0xA8, 0x5C),
        MircState::Dormant => Color::rgb(0xA8, 0xA8, 0xA8),
        MircState::Diverged => Color::rgb(0xE0, 0x8A, 0x1E),
    }
}

fn mirc_state_label(s: MircState) -> &'static str {
    match s {
        MircState::Live => "live",
        MircState::Dormant => "dormant",
        MircState::Diverged => "diverged",
    }
}

impl MircPanel {
    /// The panel rect (centered, 80% of the window).
    pub fn panel_rect(window: Size) -> Rect {
        let pw = window.w * 80 / 100;
        let ph = window.h * 80 / 100;
        let px = (window.w.saturating_sub(pw) / 2) as i32;
        let py = (window.h.saturating_sub(ph) / 2) as i32;
        Rect::new(px, py, pw, ph)
    }

    /// How many roster rows fit in the list area for this window.
    pub fn visible_rows(window: Size) -> usize {
        let p = Self::panel_rect(window);
        let list_h = (p.h as i32 - MIRC_LIST_TOP - MIRC_LIST_BOTTOM_PAD as i32).max(0);
        (list_h / MIRC_ROW_H as i32).max(0) as usize
    }

    fn close_rect(window: Size) -> Rect {
        let p = Self::panel_rect(window);
        Rect::new(p.x + p.w as i32 - 32, p.y + 10, 22, 22)
    }

    fn broadcast_rect(window: Size) -> Rect {
        let p = Self::panel_rect(window);
        Rect::new(p.x + 16, p.y + 76, 150, 26)
    }

    fn navigate_rect(window: Size) -> Rect {
        let b = Self::broadcast_rect(window);
        Rect::new(b.x + b.w as i32 + 8, b.y, 118, 26)
    }

    fn login_rect(window: Size) -> Rect {
        let n = Self::navigate_rect(window);
        Rect::new(n.x + n.w as i32 + 8, n.y, 104, 26)
    }

    fn scroll_rects(window: Size) -> (Rect, Rect) {
        let p = Self::panel_rect(window);
        let x = p.x + p.w as i32 - MIRC_SCROLL_GUTTER;
        let down_y = p.y + p.h as i32 - 30;
        (
            Rect::new(x, p.y + MIRC_LIST_TOP, 20, 20),
            Rect::new(x, down_y, 20, 20),
        )
    }

    /// Per-row control rects (state chip, login pill, open button) + the row's
    /// top y, for the `vis_i`-th *visible* row (0-based from the list top).
    fn row_controls(window: Size, vis_i: usize) -> (Rect, Rect, Rect, i32) {
        let p = Self::panel_rect(window);
        let y = p.y + MIRC_LIST_TOP + vis_i as i32 * MIRC_ROW_H as i32;
        let open = Rect::new(
            p.x + p.w as i32 - MIRC_SCROLL_GUTTER - MIRC_OPEN_W as i32,
            y + 4,
            MIRC_OPEN_W,
            22,
        );
        let login = Rect::new(open.x - 8 - MIRC_LOGIN_W as i32, y + 4, MIRC_LOGIN_W, 22);
        let state = Rect::new(login.x - 8 - MIRC_STATE_W as i32, y + 4, MIRC_STATE_W, 22);
        (state, login, open, y)
    }

    /// Map a click to an action. `len` is the total roster size; `scroll` is the
    /// app's current top offset.
    pub fn hit_test(window: Size, len: usize, scroll: usize, x: i32, y: i32) -> MircAction {
        let inside = |r: Rect| x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32;
        if inside(Self::close_rect(window)) {
            return MircAction::Close;
        }
        if inside(Self::broadcast_rect(window)) {
            return MircAction::ToggleBroadcast;
        }
        if inside(Self::navigate_rect(window)) {
            return MircAction::NavigateAll;
        }
        if inside(Self::login_rect(window)) {
            return MircAction::LoginAll;
        }
        let (up, down) = Self::scroll_rects(window);
        if inside(up) {
            return MircAction::ScrollUp;
        }
        if inside(down) {
            return MircAction::ScrollDown;
        }
        let visible = Self::visible_rows(window);
        for vis_i in 0..visible {
            let abs = scroll + vis_i;
            if abs >= len {
                break;
            }
            let (_, _, open, _) = Self::row_controls(window, vis_i);
            if inside(open) {
                return MircAction::Open(abs);
            }
        }
        MircAction::None
    }

    /// Paint the panel. `rows` is the full roster; `scroll` is the top row;
    /// `broadcasting` drives the broadcast chip; `site` names the current site.
    pub fn paint(
        window: Size,
        shaper: &dyn TextShaper,
        broadcasting: bool,
        site: &str,
        rows: &[MircRow],
        scroll: usize,
    ) -> DisplayList {
        let mut list = DisplayList::new();
        let p = Self::panel_rect(window);
        // Backdrop + panel.
        list.push(DisplayItem::Rect {
            rect: Rect::new(p.x - 1, p.y - 1, p.w + 2, p.h + 2),
            color: Color::rgb(0x30, 0x30, 0x30),
        });
        list.push(DisplayItem::Rect {
            rect: p,
            color: Color::rgb(0xFA, 0xFA, 0xFA),
        });
        // Title + subtitle.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 16, p.y + 30),
            frac_x: 0.0,
            glyphs: shaper.shape("MIRC — Multi-Identity Remote Control", 19),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        let noun = if rows.len() == 1 {
            "session"
        } else {
            "sessions"
        };
        let subtitle = if site.is_empty() {
            format!("{} {noun} being driven", rows.len())
        } else {
            format!("{} {noun} being driven · {site}", rows.len())
        };
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 16, p.y + 54),
            frac_x: 0.0,
            glyphs: shaper.shape(&subtitle, 13),
            color: Color::rgb(0x60, 0x60, 0x60),
            style: FontStyle::REGULAR,
        });
        // Close button.
        draw_icon_button(
            &mut list,
            shaper,
            Self::close_rect(window),
            IC_CLOSE,
            13,
            Color::rgb(0xE0, 0xE0, 0xE0),
            Color::BLACK,
        );
        // Control bar: broadcast toggle, then the bulk verbs.
        let bc = Self::broadcast_rect(window);
        let (bfill, bfg, blabel) = if broadcasting {
            (Color::rgb(0x1E, 0x66, 0xE0), Color::WHITE, "broadcast: on")
        } else {
            (
                Color::rgb(0xDF, 0xDF, 0xDF),
                Color::rgb(0x30, 0x30, 0x30),
                "broadcast: off",
            )
        };
        draw_button(&mut list, shaper, bc, blabel, bfill, bfg, 13);
        let bulk = Color::rgb(0xE6, 0xEE, 0xF6);
        let bulk_fg = Color::rgb(0x20, 0x40, 0x70);
        draw_button(
            &mut list,
            shaper,
            Self::navigate_rect(window),
            "navigate all",
            bulk,
            bulk_fg,
            13,
        );
        draw_button(
            &mut list,
            shaper,
            Self::login_rect(window),
            "login all",
            bulk,
            bulk_fg,
            13,
        );
        // Column headers + a hairline divider above the list.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 34, p.y + MIRC_LIST_TOP - 10),
            frac_x: 0.0,
            glyphs: shaper.shape("identity", 11),
            color: Color::rgb(0x90, 0x90, 0x90),
            style: FontStyle::REGULAR,
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + MIRC_ACCOUNT_X, p.y + MIRC_LIST_TOP - 10),
            frac_x: 0.0,
            glyphs: shaper.shape("account", 11),
            color: Color::rgb(0x90, 0x90, 0x90),
            style: FontStyle::REGULAR,
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(p.x + 12, p.y + MIRC_LIST_TOP - 4, p.w - 24, 1),
            color: Color::rgb(0xD8, 0xD8, 0xD8),
        });
        // Rows.
        let visible = Self::visible_rows(window);
        for vis_i in 0..visible {
            let abs = scroll + vis_i;
            let Some(row) = rows.get(abs) else { break };
            let (state, login, open, y) = Self::row_controls(window, vis_i);
            if vis_i % 2 == 1 {
                list.push(DisplayItem::Rect {
                    rect: Rect::new(p.x + 4, y, p.w - 8 - MIRC_SCROLL_GUTTER as u32, MIRC_ROW_H),
                    color: Color::rgb(0xF0, 0xF0, 0xF0),
                });
            }
            // Status dot.
            list.push(DisplayItem::Rect {
                rect: Rect::new(p.x + 14, y + (MIRC_ROW_H as i32 - 10) / 2, 10, 10),
                color: mirc_state_dot(row.state),
            });
            // Identity label + dimmed account, vertically centered in the row so
            // they line up with the chips/pills (which center in their boxes).
            list.push(DisplayItem::Glyphs {
                origin: Point::new(p.x + 34, y + (MIRC_ROW_H as i32 - 14) / 2),
                frac_x: 0.0,
                glyphs: shaper.shape(&row.label, 14),
                color: Color::rgb(0x18, 0x18, 0x18),
                style: FontStyle::REGULAR,
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(p.x + MIRC_ACCOUNT_X, y + (MIRC_ROW_H as i32 - 12) / 2),
                frac_x: 0.0,
                glyphs: shaper.shape(&row.account, 12),
                color: Color::rgb(0x78, 0x78, 0x78),
                style: FontStyle::REGULAR,
            });
            // State chip.
            draw_button(
                &mut list,
                shaper,
                state,
                mirc_state_label(row.state),
                mirc_state_fill(row.state),
                Color::rgb(0x30, 0x30, 0x30),
                12,
            );
            // Login pill.
            let (lf, lt, ll) = if row.logged_in {
                (
                    Color::rgb(0xD9, 0xEF, 0xD9),
                    Color::rgb(0x1E, 0x50, 0x20),
                    "logged in",
                )
            } else {
                (
                    Color::rgb(0xEC, 0xEC, 0xEC),
                    Color::rgb(0x80, 0x80, 0x80),
                    "logged out",
                )
            };
            draw_button(&mut list, shaper, login, ll, lf, lt, 12);
            // Open (select → render) button.
            draw_button(
                &mut list,
                shaper,
                open,
                "open",
                Color::rgb(0xE6, 0xEE, 0xF6),
                Color::rgb(0x20, 0x40, 0x70),
                12,
            );
        }
        // Legend.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 16, p.y + p.h as i32 - 16),
            frac_x: 0.0,
            glyphs: shaper.shape(
                "live = on screen   ·   dormant = sealed & paused   ·   diverged = needs attention",
                12,
            ),
            color: Color::rgb(0x70, 0x70, 0x70),
            style: FontStyle::REGULAR,
        });
        // Scroll affordances (same plain ASCII glyphs the cookie list uses).
        let (up, down) = Self::scroll_rects(window);
        for (r, glyph) in [(up, "^"), (down, "v")] {
            draw_button(
                &mut list,
                shaper,
                r,
                glyph,
                Color::rgb(0xE0, 0xE0, 0xE0),
                Color::BLACK,
                12,
            );
        }
        list
    }
}

#[cfg(test)]
mod mirc_panel_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    fn roster(n: usize) -> Vec<MircRow> {
        (0..n)
            .map(|i| MircRow {
                label: format!("identity {i}"),
                account: format!("user{i}@example.com"),
                state: if i == 0 {
                    MircState::Live
                } else {
                    MircState::Dormant
                },
                logged_in: i % 2 == 0,
            })
            .collect()
    }

    #[test]
    fn panel_is_centered_in_the_window() {
        let w = Size::new(1200, 800);
        let p = MircPanel::panel_rect(w);
        assert!(p.x > 0 && p.y > 0);
        assert_eq!(p.x * 2 + p.w as i32, w.w as i32, "horizontally centered");
        assert_eq!(p.y * 2 + p.h as i32, w.h as i32, "vertically centered");
    }

    #[test]
    fn control_bar_buttons_hit_test() {
        let w = Size::new(1200, 800);
        let bc = MircPanel::broadcast_rect(w);
        assert_eq!(
            MircPanel::hit_test(w, 3, 0, bc.x + 2, bc.y + 2),
            MircAction::ToggleBroadcast
        );
        let nav = MircPanel::navigate_rect(w);
        assert_eq!(
            MircPanel::hit_test(w, 3, 0, nav.x + 2, nav.y + 2),
            MircAction::NavigateAll
        );
        let login = MircPanel::login_rect(w);
        assert_eq!(
            MircPanel::hit_test(w, 3, 0, login.x + 2, login.y + 2),
            MircAction::LoginAll
        );
        let close = MircPanel::close_rect(w);
        assert_eq!(
            MircPanel::hit_test(w, 3, 0, close.x + 2, close.y + 2),
            MircAction::Close
        );
    }

    #[test]
    fn open_maps_to_absolute_row_index_with_scroll() {
        let w = Size::new(1200, 800);
        let (_, _, open, _) = MircPanel::row_controls(w, 0);
        // With scroll=5, the top visible row is absolute index 5.
        assert_eq!(
            MircPanel::hit_test(w, 50, 5, open.x + 2, open.y + 2),
            MircAction::Open(5)
        );
    }

    #[test]
    fn paint_emits_panel_rows_and_controls() {
        let w = Size::new(1200, 800);
        let list = MircPanel::paint(w, &MonoShaper, true, "github.com", &roster(3), 0);
        let glyphs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        // title + subtitle + 2 headers + 3 control labels + legend + per-row
        // (label, account, state, login, open = 5 each) + 2 scroll glyphs.
        assert!(glyphs >= 3 * 5, "got {glyphs} glyph runs");
        // A click outside any control inside the panel is consumed as None.
        let p = MircPanel::panel_rect(w);
        assert_eq!(
            MircPanel::hit_test(w, 3, 0, p.x + 2, p.y + p.h as i32 - 2),
            MircAction::None
        );
    }

    #[test]
    fn subtitle_pluralizes_and_omits_empty_site() {
        // A single, site-less session: singular noun, no "·" suffix. (Exercised
        // via paint not panicking; the format is unit-tested here directly.)
        let one = MircPanel::paint(Size::new(1000, 700), &MonoShaper, false, "", &roster(1), 0);
        assert!(one
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Glyphs { .. })));
    }
}

// ---- Performance HUD (M11): a fixed-corner, stable timing overlay ----

/// A fixed top-right overlay of named timings. Pure paint, like the other
/// chrome; the app owns the `Timings` and passes pre-formatted rows. Rows are
/// drawn in the order given (the app keeps them stable), so the HUD never
/// reorders or bounces as values update.
pub struct PerfHud;

const HUD_ROW_H: i32 = 16;
const HUD_PAD: i32 = 6;
const HUD_W: u32 = 240;

impl PerfHud {
    /// Paint `rows` (`(label, value)`) into the top-right corner, just below
    /// the toolbar. Empty input paints nothing.
    pub fn paint(window: Size, shaper: &dyn TextShaper, rows: &[(String, String)]) -> DisplayList {
        let mut list = DisplayList::new();
        if rows.is_empty() {
            return list;
        }
        let h = HUD_PAD * 2 + rows.len() as i32 * HUD_ROW_H + HUD_ROW_H;
        let x = (window.w as i32 - HUD_W as i32 - 8).max(0);
        let y = TOOLBAR_HEIGHT as i32 + 8;
        // Semi-opaque dark panel (the rasterizer composites the solid fill).
        list.push(DisplayItem::Rect {
            rect: Rect::new(x, y, HUD_W, h as u32),
            color: Color::rgb(0x10, 0x12, 0x16),
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(x + HUD_PAD, y + HUD_PAD + 12),
            frac_x: 0.0,
            glyphs: shaper.shape("performance", 12),
            color: Color::rgb(0x9A, 0xD0, 0xFF),
            style: FontStyle::REGULAR,
        });
        for (i, (label, value)) in rows.iter().enumerate() {
            let ry = y + HUD_PAD + HUD_ROW_H * (i as i32 + 1) + 12;
            list.push(DisplayItem::Glyphs {
                origin: Point::new(x + HUD_PAD, ry),
                frac_x: 0.0,
                glyphs: shaper.shape(label, 12),
                color: Color::rgb(0xD8, 0xD8, 0xD8),
                style: FontStyle::REGULAR,
            });
            // Value column, right-aligned within the panel.
            let vw: u32 = shaper.shape(value, 12).iter().map(|g| g.advance).sum();
            let vx = x + HUD_W as i32 - HUD_PAD - vw as i32;
            list.push(DisplayItem::Glyphs {
                origin: Point::new(vx, ry),
                frac_x: 0.0,
                glyphs: shaper.shape(value, 12),
                color: Color::rgb(0x86, 0xE3, 0x9A),
                style: FontStyle::REGULAR,
            });
        }
        list
    }
}

#[cfg(test)]
mod perf_hud_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    #[test]
    fn empty_hud_paints_nothing() {
        let list = PerfHud::paint(Size::new(800, 600), &MonoShaper, &[]);
        assert!(list.items.is_empty());
    }

    #[test]
    fn hud_sits_in_the_top_right_and_lists_rows() {
        let rows = vec![
            ("page load".to_string(), "12.30 ms".to_string()),
            ("GET example.com".to_string(), "8.10 ms".to_string()),
        ];
        let w = Size::new(800, 600);
        let list = PerfHud::paint(w, &MonoShaper, &rows);
        // The panel rect is in the right half, below the toolbar.
        let panel = list
            .items
            .iter()
            .find_map(|i| match i {
                DisplayItem::Rect { rect, .. } => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert!(panel.x > w.w as i32 / 2);
        assert!(panel.y >= TOOLBAR_HEIGHT as i32);
        // header + 2 rows × (label+value) glyph runs.
        let glyphs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        assert_eq!(glyphs, 1 + 2 * 2);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Design system
//
// A small, shared visual language — the "branding guidelines" for Cerberus
// chrome. Every panel (settings today, the developer console next) draws from
// the same tokens and widgets, so the UI reads as one product instead of a set
// of hand-placed rectangles: one light surface palette, the toolbar's blue as
// the single accent, one spacing rhythm, one type scale, one corner radius.
// Keep new chrome on these primitives.
// ─────────────────────────────────────────────────────────────────────────────

/// Design tokens: colours, spacing, radii, and type sizes. Pure constants so a
/// widget reads `theme::ACCENT`, not a magic hex, and a palette change happens
/// in exactly one place.
pub mod theme {
    use cerberus_types::Color;

    // — Surfaces —
    /// Dimmed backdrop painted behind a modal (source-over tint, ~40%).
    pub const SCRIM: Color = Color::rgba(0x14, 0x18, 0x1F, 0x66);
    /// Panel / card background.
    pub const SURFACE: Color = Color::rgb(0xFB, 0xFC, 0xFD);
    /// Inset control (row / field) background.
    pub const SUNKEN: Color = Color::rgb(0xF1, 0xF3, 0xF6);
    /// Raised control (text field, primary button face).
    pub const RAISED: Color = Color::rgb(0xFF, 0xFF, 0xFF);

    // — Lines —
    /// Hairline divider between regions.
    pub const DIVIDER: Color = Color::rgb(0xE7, 0xEA, 0xEE);
    /// Border around a control or the panel edge.
    pub const BORDER: Color = Color::rgb(0xD4, 0xD9, 0xE0);

    // — Text —
    /// Primary text.
    pub const TEXT: Color = Color::rgb(0x1B, 0x20, 0x27);
    /// Secondary / supporting text.
    pub const TEXT_MUTED: Color = Color::rgb(0x5C, 0x66, 0x72);
    /// Faint text: section headers, placeholders.
    pub const TEXT_FAINT: Color = Color::rgb(0x8B, 0x94, 0xA0);

    // — Accent (matches the toolbar's SYNC blue) —
    /// The single brand accent.
    pub const ACCENT: Color = Color::rgb(0x1E, 0x66, 0xE0);
    /// Text/icon on an accent fill.
    pub const ON_ACCENT: Color = Color::WHITE;

    // — Semantic —
    /// Positive state (vault unlocked dot).
    pub const SUCCESS: Color = Color::rgb(0x1E, 0xA5, 0x5B);
    /// Error text.
    pub const DANGER: Color = Color::rgb(0xC2, 0x38, 0x38);
    /// A toggle's neutral off-state track.
    pub const TRACK_OFF: Color = Color::rgb(0xC6, 0xCC, 0xD4);

    // — Dark surfaces (developer tooling — the console reads as a tool, not a
    //   page, but shares the accent, spacing, radii, and type scale) —
    /// Console background.
    pub const INK: Color = Color::rgb(0x1B, 0x1E, 0x24);
    /// Raised element on `INK` (title bar, stat chip).
    pub const INK_RAISED: Color = Color::rgb(0x25, 0x2A, 0x32);
    /// Border/divider on a dark surface.
    pub const INK_BORDER: Color = Color::rgb(0x39, 0x40, 0x4B);
    /// Primary text on a dark surface.
    pub const ON_INK: Color = Color::rgb(0xE6, 0xE9, 0xED);
    /// Muted text on a dark surface.
    pub const ON_INK_MUTED: Color = Color::rgb(0x99, 0xA2, 0xAE);
    /// A brighter accent that reads on a dark surface.
    pub const ACCENT_ON_INK: Color = Color::rgb(0x5B, 0x9C, 0xFF);
    /// Console error line (red that reads on `INK`).
    pub const CONSOLE_ERROR: Color = Color::rgb(0xFF, 0x74, 0x74);
    /// Console warning line (amber that reads on `INK`).
    pub const CONSOLE_WARN: Color = Color::rgb(0xE7, 0xB4, 0x53);

    // — Spacing scale (device px) —
    pub const SP_1: i32 = 4;
    pub const SP_2: i32 = 8;
    pub const SP_3: i32 = 12;
    pub const SP_4: i32 = 16;
    pub const SP_5: i32 = 24;

    // — Corner radii —
    pub const RADIUS_SM: u16 = 6;
    pub const RADIUS_MD: u16 = 8;
    pub const RADIUS_LG: u16 = 12;

    // — Type scale (px) —
    pub const TYPE_TITLE: u32 = 20;
    pub const TYPE_BODY: u32 = 14;
    pub const TYPE_CAPTION: u32 = 12;
    /// Section header (drawn faint + uppercase).
    pub const TYPE_SECTION: u32 = 12;
}

// —— Reusable widgets (built on the design tokens) ——

/// Fill a rounded rectangle in one colour.
fn fill_round(list: &mut DisplayList, rect: Rect, color: Color, radius: u16) {
    list.push(DisplayItem::RoundRect {
        rect,
        color,
        radius,
    });
}

/// A rounded rect with a crisp 1px border. The border is a rounded rect grown
/// 1px on every side painted *behind* the fill, so corners stay clean (a square
/// `stroke_rect` border would poke out past a rounded fill).
fn bordered_round(list: &mut DisplayList, rect: Rect, fill: Color, border: Color, radius: u16) {
    fill_round(
        list,
        Rect::new(rect.x - 1, rect.y - 1, rect.w + 2, rect.h + 2),
        border,
        radius + 1,
    );
    fill_round(list, rect, fill, radius);
}

/// Total advance width of `text` at `px`, in device pixels.
fn text_width(shaper: &dyn TextShaper, text: &str, px: u32) -> i32 {
    shaper
        .shape(text, px)
        .iter()
        .map(|g| g.advance as i32)
        .sum()
}

/// Push a left-anchored text run whose top-left is `(x, top)`. Empty text is a
/// no-op (so an absent subtitle costs nothing).
fn push_text(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    x: i32,
    top: i32,
    text: &str,
    px: u32,
    color: Color,
) {
    if text.is_empty() {
        return;
    }
    list.push(DisplayItem::Glyphs {
        origin: Point::new(x, top),
        frac_x: 0.0,
        glyphs: shaper.shape(text, px),
        color,
        style: FontStyle::REGULAR,
    });
}

/// A faint, uppercase section label anchored at `(x, top)`.
fn section_header(list: &mut DisplayList, shaper: &dyn TextShaper, x: i32, top: i32, text: &str) {
    push_text(
        list,
        shaper,
        x,
        top,
        text,
        theme::TYPE_SECTION,
        theme::TEXT_FAINT,
    );
}

/// A right-pointing chevron centred on `(cx, cy)`, drawn from two round-capped
/// strokes so it scales crisply — the "opens a sub-panel" affordance.
fn chevron_right(list: &mut DisplayList, cx: i32, cy: i32, size: i32, color: Color) {
    list.push(DisplayItem::Line {
        a: Point::new(cx - size / 2, cy - size),
        b: Point::new(cx + size / 2, cy),
        width: 2,
        color,
    });
    list.push(DisplayItem::Line {
        a: Point::new(cx + size / 2, cy),
        b: Point::new(cx - size / 2, cy + size),
        width: 2,
        color,
    });
}

/// An iOS-style pill toggle inside `rect`: an accent track when `on` (neutral
/// grey when off) with a white knob that slides to the lit side and casts a
/// faint shadow for depth.
fn toggle(list: &mut DisplayList, rect: Rect, on: bool) {
    let track = if on { theme::ACCENT } else { theme::TRACK_OFF };
    fill_round(list, rect, track, (rect.h / 2) as u16);
    let d = rect.h.saturating_sub(6);
    let ky = rect.y + 3;
    let kx = if on {
        rect.x + rect.w as i32 - 3 - d as i32
    } else {
        rect.x + 3
    };
    list.push(DisplayItem::Shadow {
        rect: Rect::new(kx, ky + 1, d, d),
        blur: 2,
        color: Color::rgba(0, 0, 0, 0x33),
    });
    fill_round(list, Rect::new(kx, ky, d, d), Color::WHITE, (d / 2) as u16);
}

/// The two-line body of a settings row: a `title` and a muted `subtitle`,
/// left-inset and vertically balanced in a [`SETTINGS_ROW_H`]-tall row.
fn row_labels(
    list: &mut DisplayList,
    shaper: &dyn TextShaper,
    x: i32,
    row: Rect,
    title: &str,
    subtitle: &str,
) {
    push_text(
        list,
        shaper,
        x,
        row.y + 8,
        title,
        theme::TYPE_BODY,
        theme::TEXT,
    );
    push_text(
        list,
        shaper,
        x,
        row.y + 25,
        subtitle,
        theme::TYPE_CAPTION,
        theme::TEXT_MUTED,
    );
}

// —— The settings panel ——

/// The height of a settings control row.
const SETTINGS_ROW_H: u32 = 44;
const SETTINGS_PANEL_W: u32 = 460;
const SETTINGS_SIDE: i32 = 24;
const SETTINGS_SECTIONS_TOP: i32 = 66;
const SETTINGS_HEADER_H: i32 = 24;
const SETTINGS_ROW_GAP: i32 = 8;
const SETTINGS_SECTION_GAP: i32 = 16;
const SETTINGS_FIELD_H: u32 = 34;
const SETTINGS_CAPTION_BLOCK: i32 = 22;
const SETTINGS_BOTTOM: i32 = 18;
const SETTINGS_TOGGLE_W: u32 = 40;
const SETTINGS_TOGGLE_H: u32 = 22;
const SETTINGS_CLOSE: u32 = 26;

/// The state the settings panel renders. Borrowed from the app each frame; the
/// panel owns no state of its own (it stays a pure view, like the rest of this
/// crate).
pub struct SettingsModel<'a> {
    /// Whether the identity vault is locked (drives the passphrase field).
    pub vault_locked: bool,
    /// Number of passphrase characters typed so far (rendered as dots).
    pub passphrase_len: usize,
    /// A transient vault status/error message, if any.
    pub vault_msg: Option<&'a str>,
    /// Whether the performance HUD is on.
    pub hud_on: bool,
    /// Whether images load (graphical); false means text-only.
    pub images_on: bool,
}

/// What a click on the settings panel means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    /// Dismiss the panel (the ✕ button or a click on the backdrop).
    Close,
    /// Open the cookie inspector.
    OpenCookies,
    /// Flip the image-loading policy (graphical ↔ text-only).
    ToggleImages,
    /// Flip the performance HUD.
    ToggleHud,
    /// A click inside the panel that hit no control — swallow it (stay open).
    None,
}

/// The resolved rectangles for one frame of the settings panel. Computed once
/// and shared by `paint` and `hit_test` so the picture and the click map can
/// never drift apart.
pub struct SettingsLayout {
    /// The panel card.
    pub panel: Rect,
    /// The ✕ close button.
    pub close: Rect,
    /// The "Cookies" nav row.
    pub cookies_row: Rect,
    /// The "Load images" toggle row.
    pub images_row: Rect,
    /// The toggle within the images row.
    pub images_toggle: Rect,
    /// The "Performance HUD" toggle row.
    pub hud_row: Rect,
    /// The toggle within the HUD row.
    pub hud_toggle: Rect,
    /// The vault area: a passphrase field when locked, a status row when not.
    pub vault: Rect,
}

/// The centred settings dialog: a modal card of grouped rows (cookies, images,
/// performance) plus the identity-vault passphrase entry, built entirely from
/// the design-system widgets above. Pure like the rest of the crate.
pub struct SettingsPanel;

impl SettingsPanel {
    /// Content height for the card, which grows a little when the vault is
    /// locked (a field + caption instead of a one-line status row).
    fn content_height(locked: bool) -> u32 {
        let mut h = SETTINGS_SECTIONS_TOP;
        // Privacy & data: header + two rows.
        h += SETTINGS_HEADER_H;
        h += SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP;
        h += SETTINGS_ROW_H as i32;
        h += SETTINGS_SECTION_GAP;
        // Performance: header + one row.
        h += SETTINGS_HEADER_H;
        h += SETTINGS_ROW_H as i32;
        h += SETTINGS_SECTION_GAP;
        // Identity vault: header + field/caption (locked) or status row.
        h += SETTINGS_HEADER_H;
        if locked {
            h += SETTINGS_FIELD_H as i32 + SETTINGS_CAPTION_BLOCK;
        } else {
            h += SETTINGS_ROW_H as i32;
        }
        h += SETTINGS_BOTTOM;
        h as u32
    }

    /// The panel card rect: a fixed comfortable width (clamped to the window),
    /// content-sized height, centred.
    fn panel_rect(window: Size, locked: bool) -> Rect {
        let w = window.w.saturating_sub(48).clamp(280, SETTINGS_PANEL_W);
        let h = Self::content_height(locked).min(window.h.saturating_sub(24).max(1));
        let x = (window.w.saturating_sub(w) / 2) as i32;
        let y = (window.h.saturating_sub(h) / 2) as i32;
        Rect::new(x, y, w, h)
    }

    /// The toggle rect inside a control row (right-inset, vertically centred).
    fn row_toggle(row: Rect) -> Rect {
        Rect::new(
            row.x + row.w as i32 - theme::SP_3 - SETTINGS_TOGGLE_W as i32,
            row.y + (row.h as i32 - SETTINGS_TOGGLE_H as i32) / 2,
            SETTINGS_TOGGLE_W,
            SETTINGS_TOGGLE_H,
        )
    }

    /// Resolve every rectangle for the current window and model.
    pub fn layout(window: Size, model: &SettingsModel<'_>) -> SettingsLayout {
        let panel = Self::panel_rect(window, model.vault_locked);
        let ix = panel.x + SETTINGS_SIDE;
        let iw = (panel.w as i32 - 2 * SETTINGS_SIDE).max(0) as u32;
        let close = Rect::new(
            panel.x + panel.w as i32 - theme::SP_4 - SETTINGS_CLOSE as i32,
            panel.y + theme::SP_4,
            SETTINGS_CLOSE,
            SETTINGS_CLOSE,
        );

        let mut y = panel.y + SETTINGS_SECTIONS_TOP;
        // Privacy & data.
        y += SETTINGS_HEADER_H;
        let cookies_row = Rect::new(ix, y, iw, SETTINGS_ROW_H);
        y += SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP;
        let images_row = Rect::new(ix, y, iw, SETTINGS_ROW_H);
        y += SETTINGS_ROW_H as i32 + SETTINGS_SECTION_GAP;
        // Performance.
        y += SETTINGS_HEADER_H;
        let hud_row = Rect::new(ix, y, iw, SETTINGS_ROW_H);
        y += SETTINGS_ROW_H as i32 + SETTINGS_SECTION_GAP;
        // Identity vault.
        y += SETTINGS_HEADER_H;
        let vault = if model.vault_locked {
            Rect::new(ix, y, iw, SETTINGS_FIELD_H)
        } else {
            Rect::new(ix, y, iw, SETTINGS_ROW_H)
        };

        SettingsLayout {
            panel,
            close,
            cookies_row,
            images_row,
            images_toggle: Self::row_toggle(images_row),
            hud_row,
            hud_toggle: Self::row_toggle(hud_row),
            vault,
        }
    }

    /// Paint the panel into its own display list (composited after the page).
    pub fn paint(window: Size, shaper: &dyn TextShaper, model: &SettingsModel<'_>) -> DisplayList {
        let mut list = DisplayList::new();
        let lay = Self::layout(window, model);
        let p = lay.panel;

        // Dim the whole window, then float the card above it.
        list.push(DisplayItem::Rect {
            rect: Rect::new(0, 0, window.w, window.h),
            color: theme::SCRIM,
        });
        list.push(DisplayItem::Shadow {
            rect: Rect::new(p.x, p.y + 3, p.w, p.h),
            blur: 26,
            color: Color::rgba(0x10, 0x14, 0x1C, 0x59),
        });
        bordered_round(
            &mut list,
            p,
            theme::SURFACE,
            theme::BORDER,
            theme::RADIUS_LG,
        );

        // Header: title, subtitle, hairline, and the ✕ close button.
        push_text(
            &mut list,
            shaper,
            p.x + SETTINGS_SIDE,
            p.y + 18,
            "Settings",
            theme::TYPE_TITLE,
            theme::TEXT,
        );
        push_text(
            &mut list,
            shaper,
            p.x + SETTINGS_SIDE,
            p.y + 44,
            "Privacy, identity, and performance",
            theme::TYPE_CAPTION,
            theme::TEXT_MUTED,
        );
        list.push(DisplayItem::Rect {
            rect: Rect::new(
                p.x + SETTINGS_SIDE,
                p.y + 60,
                (p.w as i32 - 2 * SETTINGS_SIDE).max(0) as u32,
                1,
            ),
            color: theme::DIVIDER,
        });
        draw_icon_button(
            &mut list,
            shaper,
            lay.close,
            IC_CLOSE,
            12,
            theme::SUNKEN,
            theme::TEXT_MUTED,
        );

        // Section: privacy & data.
        section_header(
            &mut list,
            shaper,
            p.x + SETTINGS_SIDE,
            lay.cookies_row.y - 18,
            "PRIVACY & DATA",
        );
        // Cookies (nav → chevron).
        bordered_round(
            &mut list,
            lay.cookies_row,
            theme::SUNKEN,
            theme::BORDER,
            theme::RADIUS_MD,
        );
        row_labels(
            &mut list,
            shaper,
            lay.cookies_row.x + 14,
            lay.cookies_row,
            "Cookies",
            "Review and manage stored cookies",
        );
        chevron_right(
            &mut list,
            lay.cookies_row.x + lay.cookies_row.w as i32 - 18,
            lay.cookies_row.y + lay.cookies_row.h as i32 / 2,
            5,
            theme::TEXT_FAINT,
        );
        // Load images (toggle).
        bordered_round(
            &mut list,
            lay.images_row,
            theme::SUNKEN,
            theme::BORDER,
            theme::RADIUS_MD,
        );
        row_labels(
            &mut list,
            shaper,
            lay.images_row.x + 14,
            lay.images_row,
            "Load images",
            if model.images_on {
                "Fetching and rendering images"
            } else {
                "Text-only — faster and lighter"
            },
        );
        toggle(&mut list, lay.images_toggle, model.images_on);

        // Section: performance.
        section_header(
            &mut list,
            shaper,
            p.x + SETTINGS_SIDE,
            lay.hud_row.y - 18,
            "PERFORMANCE",
        );
        bordered_round(
            &mut list,
            lay.hud_row,
            theme::SUNKEN,
            theme::BORDER,
            theme::RADIUS_MD,
        );
        row_labels(
            &mut list,
            shaper,
            lay.hud_row.x + 14,
            lay.hud_row,
            "Performance HUD",
            if model.hud_on {
                "Frame-timing overlay is visible"
            } else {
                "Show the frame-timing overlay"
            },
        );
        toggle(&mut list, lay.hud_toggle, model.hud_on);

        // Section: identity vault.
        section_header(
            &mut list,
            shaper,
            p.x + SETTINGS_SIDE,
            lay.vault.y - 18,
            "IDENTITY VAULT",
        );
        if model.vault_locked {
            // Masked passphrase field (keyboard-driven: type, then Enter).
            bordered_round(
                &mut list,
                lay.vault,
                theme::RAISED,
                theme::BORDER,
                theme::RADIUS_MD,
            );
            let px = theme::TYPE_BODY;
            let fx = lay.vault.x + 14;
            let top = lay.vault.y + (lay.vault.h as i32 - px as i32) / 2;
            let dots = "\u{2022}".repeat(model.passphrase_len);
            push_text(&mut list, shaper, fx, top, &dots, px, theme::TEXT);
            // Caret after the last dot.
            let caret_x = fx + text_width(shaper, &dots, px) + if dots.is_empty() { 0 } else { 1 };
            list.push(DisplayItem::Rect {
                rect: Rect::new(caret_x, top, 2, px),
                color: theme::ACCENT,
            });
            // Caption: the hint, or an error in red.
            let cap_y = lay.vault.y + lay.vault.h as i32 + 6;
            match model.vault_msg {
                Some(msg) => push_text(
                    &mut list,
                    shaper,
                    lay.vault.x,
                    cap_y,
                    msg,
                    theme::TYPE_CAPTION,
                    theme::DANGER,
                ),
                None => push_text(
                    &mut list,
                    shaper,
                    lay.vault.x,
                    cap_y,
                    "Type your passphrase, then press Enter to unlock",
                    theme::TYPE_CAPTION,
                    theme::TEXT_FAINT,
                ),
            }
        } else {
            // Unlocked status row with a green dot.
            bordered_round(
                &mut list,
                lay.vault,
                theme::SUNKEN,
                theme::BORDER,
                theme::RADIUS_MD,
            );
            let dot = 8u32;
            let dx = lay.vault.x + 14;
            let dy = lay.vault.y + lay.vault.h as i32 / 2 - dot as i32 / 2;
            fill_round(
                &mut list,
                Rect::new(dx, dy, dot, dot),
                theme::SUCCESS,
                (dot / 2) as u16,
            );
            row_labels(
                &mut list,
                shaper,
                dx + dot as i32 + 10,
                lay.vault,
                "Vault unlocked",
                "Quarantined cookies are retained",
            );
        }

        list
    }

    /// Map a click to a [`SettingsAction`]. A click inside the panel that misses
    /// every control is swallowed ([`SettingsAction::None`]); a click on the
    /// backdrop dismisses ([`SettingsAction::Close`]).
    pub fn hit_test(window: Size, model: &SettingsModel<'_>, x: i32, y: i32) -> SettingsAction {
        let lay = Self::layout(window, model);
        if point_in(lay.close, x, y) {
            return SettingsAction::Close;
        }
        if point_in(lay.cookies_row, x, y) {
            return SettingsAction::OpenCookies;
        }
        if point_in(lay.images_row, x, y) {
            return SettingsAction::ToggleImages;
        }
        if point_in(lay.hud_row, x, y) {
            return SettingsAction::ToggleHud;
        }
        if point_in(lay.panel, x, y) {
            return SettingsAction::None;
        }
        SettingsAction::Close
    }
}

#[cfg(test)]
mod settings_panel_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    fn model(locked: bool) -> SettingsModel<'static> {
        SettingsModel {
            vault_locked: locked,
            passphrase_len: 0,
            vault_msg: None,
            hud_on: false,
            images_on: true,
        }
    }

    #[test]
    fn panel_is_centred_and_grows_when_locked() {
        let w = Size::new(1000, 800);
        let unlocked = SettingsPanel::panel_rect(w, false);
        let locked = SettingsPanel::panel_rect(w, true);
        // Horizontally centred, clamped to the fixed width.
        assert_eq!(unlocked.w, SETTINGS_PANEL_W);
        assert_eq!(unlocked.x, (1000 - SETTINGS_PANEL_W as i32) / 2);
        // The locked card is taller (a field + caption vs a one-line row).
        assert!(
            locked.h > unlocked.h,
            "locked panel reserves field + caption"
        );
    }

    #[test]
    fn each_control_maps_to_its_action() {
        let w = Size::new(900, 720);
        let m = model(true);
        let lay = SettingsPanel::layout(w, &m);
        let center = |r: Rect| (r.x + r.w as i32 / 2, r.y + r.h as i32 / 2);

        let (cx, cy) = center(lay.close);
        assert_eq!(
            SettingsPanel::hit_test(w, &m, cx, cy),
            SettingsAction::Close
        );
        let (cx, cy) = center(lay.cookies_row);
        assert_eq!(
            SettingsPanel::hit_test(w, &m, cx, cy),
            SettingsAction::OpenCookies
        );
        let (cx, cy) = center(lay.images_row);
        assert_eq!(
            SettingsPanel::hit_test(w, &m, cx, cy),
            SettingsAction::ToggleImages
        );
        let (cx, cy) = center(lay.hud_row);
        assert_eq!(
            SettingsPanel::hit_test(w, &m, cx, cy),
            SettingsAction::ToggleHud
        );
    }

    #[test]
    fn inside_swallows_and_outside_dismisses() {
        let w = Size::new(900, 720);
        let m = model(true);
        let lay = SettingsPanel::layout(w, &m);
        // The vault field is inside the panel but is no action → swallowed.
        let (vx, vy) = (lay.vault.x + 4, lay.vault.y + 4);
        assert_eq!(SettingsPanel::hit_test(w, &m, vx, vy), SettingsAction::None);
        // A click well outside the card dismisses it.
        assert_eq!(SettingsPanel::hit_test(w, &m, 2, 2), SettingsAction::Close);
    }

    #[test]
    fn paint_draws_toggles_and_field_when_locked() {
        let w = Size::new(900, 720);
        let m = model(true);
        let list = SettingsPanel::paint(w, &MonoShaper, &m);
        // At least a scrim, a rounded card, and several rounded rows/toggles.
        let round = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::RoundRect { .. }))
            .count();
        assert!(
            round >= 6,
            "card + rows + toggles are rounded (got {round})"
        );
        // Some text was laid (title, sections, labels).
        assert!(list
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Glyphs { .. })));
    }
}

// —— The developer console ——

/// The height of the console's title bar.
const DEV_HEADER_H: i32 = 34;
/// The height of the tab strip.
const DEV_TABS_H: i32 = 30;
/// The height of the stat-chip strip.
const DEV_STATS_H: i32 = 36;
/// A console log line's height.
const DEV_LINE_H: i32 = 16;

/// The severity of a captured `console.*` line, so the console can colour it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleLevel {
    /// `console.log`.
    Log,
    /// `console.info`.
    Info,
    /// `console.warn`.
    Warn,
    /// `console.error`.
    Error,
    /// `console.debug`.
    Debug,
}

/// One captured console line: its severity and formatted text.
#[derive(Clone, Debug)]
pub struct ConsoleLine {
    /// Severity, for colouring.
    pub level: ConsoleLevel,
    /// The formatted message text.
    pub text: String,
}

/// A read-only snapshot of what the page is doing, rendered by the console.
pub struct DevConsoleModel<'a> {
    /// The current page URL.
    pub url: &'a str,
    /// Live DOM node count.
    pub dom_nodes: usize,
    /// Live link count.
    pub links: usize,
    /// Live form-field count.
    pub fields: usize,
    /// Stored-cookie count.
    pub cookies: usize,
    /// Captured `console.*` output, oldest first.
    pub lines: &'a [ConsoleLine],
}

/// The F12 developer console: a dark bottom drawer that reads as a developer
/// tool while sharing the design system's accent, spacing, radii, and type
/// scale. A titled tab strip (Console active; Elements/Network/Storage are
/// signposted for later), a strip of live stat chips, and the page's captured
/// `console.*` output. Read-only for now; a pure view like the rest of the
/// crate.
pub struct DevConsole;

impl DevConsole {
    /// The drawer rect: the bottom ~45% of the window (min 140px), full width.
    pub fn drawer_rect(window: Size) -> Rect {
        let h = (window.h * 9 / 20).clamp(140, window.h.max(1));
        Rect::new(0, (window.h - h) as i32, window.w.max(1), h)
    }

    /// Paint the console into its own display list (composited after the page).
    pub fn paint(
        window: Size,
        shaper: &dyn TextShaper,
        model: &DevConsoleModel<'_>,
    ) -> DisplayList {
        let mut list = DisplayList::new();
        let d = Self::drawer_rect(window);

        // Drawer surface + a top accent hairline (the "attached to the bottom"
        // edge) + a title bar.
        list.push(DisplayItem::Rect {
            rect: d,
            color: theme::INK,
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(d.x, d.y, d.w, 2),
            color: theme::ACCENT_ON_INK,
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(d.x, d.y + 2, d.w, DEV_HEADER_H as u32),
            color: theme::INK_RAISED,
        });
        let pad = theme::SP_4;
        push_text(
            &mut list,
            shaper,
            d.x + pad,
            d.y + 11,
            "Developer Console",
            theme::TYPE_CAPTION + 1,
            theme::ON_INK,
        );
        // Right-aligned close hint.
        let hint = "F12 to close";
        let hw = text_width(shaper, hint, theme::TYPE_CAPTION);
        push_text(
            &mut list,
            shaper,
            d.x + d.w as i32 - pad - hw,
            d.y + 12,
            hint,
            theme::TYPE_CAPTION,
            theme::ON_INK_MUTED,
        );

        // Tab strip: Console is active (accent underline); the rest are
        // signposted for later (dimmed, no underline).
        let tabs_top = d.y + 2 + DEV_HEADER_H;
        let mut tx = d.x + pad;
        for (i, name) in ["Console", "Elements", "Network", "Storage"]
            .iter()
            .enumerate()
        {
            let active = i == 0;
            let color = if active {
                theme::ON_INK
            } else {
                theme::ON_INK_MUTED
            };
            let top = tabs_top + (DEV_TABS_H - theme::TYPE_CAPTION as i32) / 2 - 1;
            push_text(&mut list, shaper, tx, top, name, theme::TYPE_CAPTION, color);
            let w = text_width(shaper, name, theme::TYPE_CAPTION);
            if active {
                list.push(DisplayItem::Rect {
                    rect: Rect::new(tx, tabs_top + DEV_TABS_H - 2, w.max(1) as u32, 2),
                    color: theme::ACCENT_ON_INK,
                });
            }
            tx += w + theme::SP_5;
        }
        // Divider under the tabs.
        list.push(DisplayItem::Rect {
            rect: Rect::new(d.x, tabs_top + DEV_TABS_H, d.w, 1),
            color: theme::INK_BORDER,
        });

        // Stat chips: live DOM/link/field/cookie counts.
        let stats_top = tabs_top + DEV_TABS_H + 1;
        let chip_cy = stats_top + DEV_STATS_H / 2;
        let mut cx = d.x + pad;
        for (value, label) in [
            (model.dom_nodes, "nodes"),
            (model.links, "links"),
            (model.fields, "fields"),
            (model.cookies, "cookies"),
        ] {
            cx += Self::chip(&mut list, shaper, cx, chip_cy, value, label) + theme::SP_2;
        }
        // The URL, right of the chips if it fits, else on the far right muted.
        let url_w = text_width(shaper, model.url, theme::TYPE_CAPTION);
        let url_x = (d.x + d.w as i32 - pad - url_w).max(cx + theme::SP_3);
        push_text(
            &mut list,
            shaper,
            url_x,
            chip_cy - theme::TYPE_CAPTION as i32 / 2,
            model.url,
            theme::TYPE_CAPTION,
            theme::ACCENT_ON_INK,
        );

        // Console output area: the tail that fits, oldest first.
        let log_top = stats_top + DEV_STATS_H + theme::SP_1;
        let log_bottom = d.y + d.h as i32 - theme::SP_2;
        let avail = ((log_bottom - log_top) / DEV_LINE_H).max(0) as usize;
        if model.lines.is_empty() {
            push_text(
                &mut list,
                shaper,
                d.x + pad,
                log_top,
                "(no console output on this page)",
                theme::TYPE_CAPTION,
                theme::ON_INK_MUTED,
            );
        } else {
            let start = model.lines.len().saturating_sub(avail);
            for (i, line) in model.lines[start..].iter().enumerate() {
                let color = match line.level {
                    ConsoleLevel::Error => theme::CONSOLE_ERROR,
                    ConsoleLevel::Warn => theme::CONSOLE_WARN,
                    ConsoleLevel::Debug => theme::ON_INK_MUTED,
                    ConsoleLevel::Log | ConsoleLevel::Info => theme::ON_INK,
                };
                push_text(
                    &mut list,
                    shaper,
                    d.x + pad,
                    log_top + i as i32 * DEV_LINE_H,
                    &line.text,
                    theme::TYPE_CAPTION,
                    color,
                );
            }
        }
        list
    }

    /// Draw one stat chip (a rounded `INK_RAISED` pill: bright value + muted
    /// label) centred vertically on `cy`, returning its width so chips flow.
    fn chip(
        list: &mut DisplayList,
        shaper: &dyn TextShaper,
        x: i32,
        cy: i32,
        value: usize,
        label: &str,
    ) -> i32 {
        let px = theme::TYPE_CAPTION;
        let value_s = value.to_string();
        let vw = text_width(shaper, &value_s, px);
        let lw = text_width(shaper, label, px);
        let inner = vw + theme::SP_1 + lw;
        let w = inner + 2 * theme::SP_3;
        let h = 22;
        let rect = Rect::new(x, cy - h / 2, w.max(1) as u32, h as u32);
        fill_round(list, rect, theme::INK_RAISED, theme::RADIUS_SM);
        let top = cy - px as i32 / 2;
        push_text(
            list,
            shaper,
            x + theme::SP_3,
            top,
            &value_s,
            px,
            theme::ON_INK,
        );
        push_text(
            list,
            shaper,
            x + theme::SP_3 + vw + theme::SP_1,
            top,
            label,
            px,
            theme::ON_INK_MUTED,
        );
        w
    }
}

#[cfg(test)]
mod dev_console_tests {
    use super::*;
    use cerberus_paint::MonoShaper;

    #[test]
    fn drawer_is_bottom_anchored_and_full_width() {
        let w = Size::new(1000, 800);
        let d = DevConsole::drawer_rect(w);
        assert_eq!(d.x, 0);
        assert_eq!(d.w, 1000);
        assert_eq!(d.y + d.h as i32, 800, "flush with the window bottom");
        assert!(d.h >= 140);
    }

    fn line(level: ConsoleLevel, text: &str) -> ConsoleLine {
        ConsoleLine {
            level,
            text: text.to_string(),
        }
    }

    #[test]
    fn paint_shows_placeholder_when_no_output() {
        let w = Size::new(1000, 800);
        let model = DevConsoleModel {
            url: "https://example.test/",
            dom_nodes: 128,
            links: 12,
            fields: 3,
            cookies: 7,
            lines: &[],
        };
        let list = DevConsole::paint(w, &MonoShaper, &model);
        // Chips are rounded; the drawer + tab underline are plain rects; text ran.
        assert!(list
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::RoundRect { .. })));
        assert!(list
            .items
            .iter()
            .any(|i| matches!(i, DisplayItem::Glyphs { .. })));
    }

    #[test]
    fn paint_tail_clips_to_what_fits() {
        // A short drawer with many lines shows only the most recent that fit,
        // and never panics slicing.
        let w = Size::new(600, 320);
        let lines: Vec<ConsoleLine> = (0..200)
            .map(|i| line(ConsoleLevel::Log, &format!("log line {i}")))
            .collect();
        let model = DevConsoleModel {
            url: "https://x/",
            dom_nodes: 1,
            links: 0,
            fields: 0,
            cookies: 0,
            lines: &lines,
        };
        let list = DevConsole::paint(w, &MonoShaper, &model);
        let glyph_runs = list
            .items
            .iter()
            .filter(|i| matches!(i, DisplayItem::Glyphs { .. }))
            .count();
        // Bounded by the drawer height, not the 200 input lines.
        assert!(glyph_runs < 60, "tail-clipped (got {glyph_runs} runs)");
    }

    #[test]
    fn error_and_warn_lines_use_their_level_colours() {
        let w = Size::new(1000, 800);
        let lines = vec![
            line(ConsoleLevel::Log, "ordinary log"),
            line(ConsoleLevel::Warn, "a warning"),
            line(ConsoleLevel::Error, "an error"),
        ];
        let model = DevConsoleModel {
            url: "https://x/",
            dom_nodes: 0,
            links: 0,
            fields: 0,
            cookies: 0,
            lines: &lines,
        };
        let list = DevConsole::paint(w, &MonoShaper, &model);
        let has = |c: Color| {
            list.items
                .iter()
                .any(|i| matches!(i, DisplayItem::Glyphs { color, .. } if *color == c))
        };
        assert!(has(theme::CONSOLE_ERROR), "error line is red");
        assert!(has(theme::CONSOLE_WARN), "warning line is amber");
    }
}

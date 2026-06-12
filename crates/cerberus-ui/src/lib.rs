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
                    style: FontStyle::REGULAR,
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
            glyphs: shaper.shape(&msg, 13),
            color: Color::rgb(0x40, 0x38, 0x10),
            style: FontStyle::REGULAR,
        });

        for (action, rect) in Self::buttons(window) {
            let (fill, label) = match action {
                BannerAction::Allow => (Color::rgb(0xD9, 0xEF, 0xD9), "Allow"),
                BannerAction::Deny => (Color::rgb(0xF3, 0xD9, 0xD9), "Deny"),
                BannerAction::Dismiss => (Color::rgb(0xE8, 0xE8, 0xE8), "x"),
                BannerAction::None => continue,
            };
            list.push(DisplayItem::Rect { rect, color: fill });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(rect.x + 6, rect.y + 15),
                glyphs: shaper.shape(label, 12),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
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
const COOKIE_LIST_TOP: i32 = 84; // panel-local y where rows begin
const COOKIE_LIST_BOTTOM_PAD: u32 = 40;

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
            glyphs: shaper.shape(&format!("Cookies ({})", rows.len()), 20),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        // Close button.
        let close = Self::close_rect(window);
        list.push(DisplayItem::Rect {
            rect: close,
            color: Color::rgb(0xE0, 0xE0, 0xE0),
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(close.x + 6, close.y + 15),
            glyphs: shaper.shape("x", 13),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        // Global default chip.
        list.push(DisplayItem::Glyphs {
            origin: Point::new(p.x + 12, p.y + 63),
            glyphs: shaper.shape("global default:", 13),
            color: Color::rgb(0x50, 0x50, 0x50),
            style: FontStyle::REGULAR,
        });
        let gchip = Self::global_chip_rect(window);
        list.push(DisplayItem::Rect {
            rect: gchip,
            color: Color::rgb(0xD9, 0xE7, 0xF7),
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(gchip.x + 6, gchip.y + 15),
            glyphs: shaper.shape(global_chip, 12),
            color: Color::BLACK,
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
                glyphs: shaper.shape(&row.primary, 13),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(p.x + 12 + 260, y + 17),
                glyphs: shaper.shape(&row.detail, 11),
                color: Color::rgb(0x80, 0x80, 0x80),
                style: FontStyle::REGULAR,
            });
            // reveal (eye), chip, delete (x)
            list.push(DisplayItem::Rect {
                rect: reveal,
                color: Color::rgb(0xE8, 0xE8, 0xE8),
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(reveal.x + 5, reveal.y + 15),
                glyphs: shaper.shape("o", 12),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            list.push(DisplayItem::Rect {
                rect: chip,
                color: Color::rgb(0xD9, 0xEF, 0xD9),
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(chip.x + 5, chip.y + 15),
                glyphs: shaper.shape(&row.chip, 12),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
            list.push(DisplayItem::Rect {
                rect: delete,
                color: Color::rgb(0xF3, 0xD9, 0xD9),
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(delete.x + 6, delete.y + 15),
                glyphs: shaper.shape("x", 12),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
        }
        // Scroll affordances.
        let (up, down) = Self::scroll_rects(window);
        for (r, glyph) in [(up, "^"), (down, "v")] {
            list.push(DisplayItem::Rect {
                rect: r,
                color: Color::rgb(0xE0, 0xE0, 0xE0),
            });
            list.push(DisplayItem::Glyphs {
                origin: Point::new(r.x + 6, r.y + 15),
                glyphs: shaper.shape(glyph, 12),
                color: Color::BLACK,
                style: FontStyle::REGULAR,
            });
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

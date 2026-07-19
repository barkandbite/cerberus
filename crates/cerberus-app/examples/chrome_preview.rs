//! Render preview PNGs of the chrome that isn't covered by the settings/mirc
//! previews — the toolbar, the consent banner, the performance HUD, and the
//! cookie inspector — so the shared design-system look of every surface can be
//! reviewed without a display server. Each frame composes the *real* widgets
//! over a faux page exactly as the app paints them.
//!
//! Run: `cargo run -p cerberus-app --example chrome_preview`

use cerberus_headless::write_png;
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_text::TextEngine;
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use cerberus_ui::{
    ConsentBanner, CookieManager, CookieRow, PerfHud, Toolbar, BANNER_HEIGHT, TOOLBAR_HEIGHT,
};

/// Render scale (physical ÷ logical); 2× keeps the preview crisp when viewed.
const SCALE: f32 = 2.0;

fn main() {
    let text = TextEngine::new();

    // 1) Toolbar + consent banner over the page (the always-on bar chrome).
    let bar = Size::new(1080, 300);
    let mut fb = base_frame(&text, bar);
    let mut toolbar = Toolbar::new("claims");
    toolbar.url_text = "https://travel.example.gov/claims/queue".to_string();
    toolbar.can_back = true;
    toolbar.broadcasting = true;
    toolbar.sync_count = 12;
    text.rasterize(&toolbar.paint(bar, &text).scaled(SCALE), &mut fb);
    let banner = ConsentBanner::new("https://ads.tracker.net", 2);
    text.rasterize(&banner.paint(bar, &text).scaled(SCALE), &mut fb);
    write_png("chrome-bar.png", &fb).expect("write png");

    // 2) Toolbar + performance HUD (the top-right timing overlay).
    let hud_scene = Size::new(1080, 300);
    let mut fb = base_frame(&text, hud_scene);
    let mut toolbar = Toolbar::new("claims");
    toolbar.url_text = "https://travel.example.gov/claims/queue".to_string();
    toolbar.can_back = true;
    text.rasterize(&toolbar.paint(hud_scene, &text).scaled(SCALE), &mut fb);
    let hud = vec![
        ("page load".to_string(), "12.30 ms".to_string()),
        ("layout".to_string(), "3.10 ms".to_string()),
        ("paint".to_string(), "4.80 ms".to_string()),
        ("GET travel.example.gov".to_string(), "88.40 ms".to_string()),
    ];
    text.rasterize(
        &PerfHud::paint(hud_scene, &text, &hud).scaled(SCALE),
        &mut fb,
    );
    write_png("chrome-hud.png", &fb).expect("write png");

    // 3) The cookie inspector modal (scrim + card + disposition pills).
    let modal = Size::new(1080, 720);
    let mut fb = base_frame(&text, modal);
    let mut toolbar = Toolbar::new("claims");
    toolbar.url_text = "https://travel.example.gov/claims/queue".to_string();
    text.rasterize(&toolbar.paint(modal, &text).scaled(SCALE), &mut fb);
    let rows = cookie_rows();
    text.rasterize(
        &CookieManager::paint(modal, &text, "session", &rows, 0).scaled(SCALE),
        &mut fb,
    );
    write_png("chrome-cookies.png", &fb).expect("write png");

    println!("wrote chrome-bar.png, chrome-hud.png, chrome-cookies.png");
}

fn su(v: u32) -> u32 {
    ((v as f32 * SCALE).round() as u32).max(1)
}

/// A white frame with a faux page painted under the chrome.
fn base_frame(text: &TextEngine, logical: Size) -> Framebuffer {
    let physical = Size::new(su(logical.w), su(logical.h));
    let mut fb = Framebuffer::new(physical);
    fb.clear(Color::rgb(0xFF, 0xFF, 0xFF));
    text.rasterize(&faux_page(text, logical).scaled(SCALE), &mut fb);
    fb
}

/// A minimal faux page so the chrome has something to sit over.
fn faux_page(text: &TextEngine, logical: Size) -> DisplayList {
    let mut list = DisplayList::new();
    let top = (TOOLBAR_HEIGHT + BANNER_HEIGHT) as i32 + 34;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(48, top),
        frac_x: 0.0,
        glyphs: text.shape("Travel Claims — Processing Queue", 26),
        color: Color::rgb(0x22, 0x22, 0x22),
        style: FontStyle::REGULAR,
    });
    for i in 0..8 {
        let y = top + 44 + i * 30;
        list.push(DisplayItem::Glyphs {
            origin: Point::new(48, y),
            frac_x: 0.0,
            glyphs: text.shape(&format!("CLM-2026-30{i}   ·   pending review"), 14),
            color: Color::rgb(0x55, 0x55, 0x55),
            style: FontStyle::REGULAR,
        });
        list.push(DisplayItem::Rect {
            rect: Rect::new(44, y - 18, logical.w - 88, 1),
            color: Color::rgb(0xEE, 0xEE, 0xEE),
        });
    }
    list
}

/// A representative set of cookie rows spanning every disposition.
fn cookie_rows() -> Vec<CookieRow> {
    let mk = |primary: &str, detail: &str, chip: &str| CookieRow {
        primary: primary.to_string(),
        detail: detail.to_string(),
        chip: chip.to_string(),
    };
    vec![
        mk("session_id", "travel.example.gov · expires in 2h", "allow"),
        mk("csrf_token", "travel.example.gov · session", "allow"),
        mk("_ga", "ads.tracker.net · expires in 2y", "block"),
        mk("prefs", "travel.example.gov · expires in 30d", "session"),
        mk("_fbp", "ads.tracker.net · expires in 90d", "block"),
        mk("locale", "travel.example.gov · expires in 1y", "allow"),
    ]
}

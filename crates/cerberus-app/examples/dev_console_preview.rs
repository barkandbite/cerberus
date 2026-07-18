//! Render a preview PNG of the F12 developer console — rebuilt on the Cerberus
//! design system (`cerberus_ui::theme` dark tokens + shared widgets) — so the
//! look can be reviewed without a display server. Composes the *real*
//! `DevConsole` over a faux page, exactly as the app paints it.
//!
//! Run: `cargo run -p cerberus-app --example dev_console_preview`

use cerberus_headless::write_png;
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_text::TextEngine;
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use cerberus_ui::{DevConsole, DevConsoleModel, Toolbar};

/// Render scale (physical ÷ logical); 2× keeps the preview crisp when viewed.
const SCALE: f32 = 2.0;

fn main() {
    let logical = Size::new(1080, 720);
    let lines: Vec<String> = vec![
        "[log] app boot: hydrating 3 components".to_string(),
        "[log] fetch /api/session → 200 (42ms)".to_string(),
        "[warn] deprecated: use requestIdleCallback".to_string(),
        "[log] worker spawned: analytics".to_string(),
        "[error] TypeError: cannot read 'id' of null".to_string(),
        "[log] render committed in 8.1ms".to_string(),
    ];
    let model = DevConsoleModel {
        url: "https://example.com/account",
        dom_nodes: 428,
        links: 37,
        fields: 6,
        cookies: 9,
        lines: &lines,
    };
    write_png("dev-console.png", &render_scene(logical, &model)).expect("write png");

    let empty = DevConsoleModel {
        lines: &[],
        ..model
    };
    write_png("dev-console-empty.png", &render_scene(logical, &empty)).expect("write png");
    println!("wrote dev-console.png, dev-console-empty.png");
}

fn su(v: u32) -> u32 {
    ((v as f32 * SCALE).round() as u32).max(1)
}

/// Render the whole frame (faux page + toolbar + console drawer) at `SCALE`.
fn render_scene(logical: Size, model: &DevConsoleModel<'_>) -> Framebuffer {
    let text = TextEngine::new();
    let physical = Size::new(su(logical.w), su(logical.h));
    let mut fb = Framebuffer::new(physical);
    fb.clear(Color::WHITE);

    text.rasterize(&faux_page(&text, logical).scaled(SCALE), &mut fb);

    let mut toolbar = Toolbar::new("work");
    toolbar.url_text = model.url.to_string();
    toolbar.can_back = true;
    text.rasterize(&toolbar.paint(logical, &text).scaled(SCALE), &mut fb);

    text.rasterize(
        &DevConsole::paint(logical, &text, model).scaled(SCALE),
        &mut fb,
    );
    fb
}

/// A minimal faux page so the drawer has something to sit over.
fn faux_page(text: &TextEngine, logical: Size) -> DisplayList {
    let mut list = DisplayList::new();
    let top = cerberus_ui::TOOLBAR_HEIGHT as i32 + 28;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(48, top),
        frac_x: 0.0,
        glyphs: text.shape("Account", 26),
        color: Color::rgb(0x22, 0x22, 0x22),
        style: FontStyle::REGULAR,
    });
    for i in 0..8 {
        let y = top + 44 + i * 30;
        list.push(DisplayItem::Rect {
            rect: Rect::new(48, y - 14, logical.w - 96, 22),
            color: Color::rgb(0xF6, 0xF6, 0xF6),
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(56, y),
            frac_x: 0.0,
            glyphs: text.shape("Page content behind the console drawer", 13),
            color: Color::rgb(0x66, 0x66, 0x66),
            style: FontStyle::REGULAR,
        });
    }
    list
}

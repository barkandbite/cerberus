//! Render preview PNGs of the settings panel — the first surface built on the
//! Cerberus design system (`cerberus_ui::theme` + its widgets) — so the look can
//! be reviewed without a display server. It composes the *real* `SettingsPanel`
//! over a faux page exactly as the app paints it, in both the locked (passphrase
//! entry) and unlocked states.
//!
//! Run: `cargo run -p cerberus-app --example settings_preview`

use cerberus_headless::write_png;
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_text::TextEngine;
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use cerberus_ui::{SettingsModel, SettingsPanel, Toolbar};

/// Render scale (physical ÷ logical); 2× keeps the preview crisp when viewed.
const SCALE: f32 = 2.0;

fn main() {
    let logical = Size::new(1080, 720);

    // Locked: the vault wants a passphrase; images on, HUD off.
    let locked = SettingsModel {
        vault_locked: true,
        passphrase_len: 7,
        vault_msg: None,
        hud_on: false,
        images_on: true,
    };
    write_png("settings-locked.png", &render_scene(logical, &locked)).expect("write png");

    // Locked with an error caption after a bad passphrase.
    let bad = SettingsModel {
        vault_locked: true,
        passphrase_len: 4,
        vault_msg: Some("wrong passphrase"),
        hud_on: true,
        images_on: false,
    };
    write_png("settings-error.png", &render_scene(logical, &bad)).expect("write png");

    // Unlocked: the vault is open; HUD on, images text-only.
    let unlocked = SettingsModel {
        vault_locked: false,
        passphrase_len: 0,
        vault_msg: None,
        hud_on: true,
        images_on: false,
    };
    write_png("settings-unlocked.png", &render_scene(logical, &unlocked)).expect("write png");

    println!("wrote settings-locked.png, settings-error.png, settings-unlocked.png");
}

fn su(v: u32) -> u32 {
    ((v as f32 * SCALE).round() as u32).max(1)
}

/// Render the whole frame (faux page + toolbar + settings modal) at `SCALE`.
fn render_scene(logical: Size, model: &SettingsModel<'_>) -> Framebuffer {
    let text = TextEngine::new();
    let physical = Size::new(su(logical.w), su(logical.h));
    let mut fb = Framebuffer::new(physical);
    fb.clear(Color::rgb(0xF2, 0xF2, 0xF2));

    text.rasterize(&faux_page(&text, logical).scaled(SCALE), &mut fb);

    let mut toolbar = Toolbar::new("work");
    toolbar.url_text = "https://example.com/account".to_string();
    toolbar.can_back = true;
    text.rasterize(&toolbar.paint(logical, &text).scaled(SCALE), &mut fb);

    text.rasterize(
        &SettingsPanel::paint(logical, &text, model).scaled(SCALE),
        &mut fb,
    );
    fb
}

/// A minimal faux page so the modal has something to float over.
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
    for i in 0..12 {
        let y = top + 44 + i * 30;
        list.push(DisplayItem::Rect {
            rect: Rect::new(48, y - 14, logical.w - 96, 22),
            color: Color::rgb(0xFB, 0xFB, 0xFB),
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(56, y),
            frac_x: 0.0,
            glyphs: text.shape(
                "Setting row placeholder — the modal dims the page behind it",
                13,
            ),
            color: Color::rgb(0x66, 0x66, 0x66),
            style: FontStyle::REGULAR,
        });
    }
    list
}

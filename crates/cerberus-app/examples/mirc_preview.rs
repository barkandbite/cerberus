//! Render a preview PNG of the MIRC (Multi-Identity Remote Control) panel — the
//! Phase 2a roster + orchestrator — so the design can be reviewed without a
//! display server. It composes the *real* UI components (`Toolbar` with its MIRC
//! count badge, and `MircPanel`) over a faux page, exactly as the app paints
//! them, then writes the frame with the headless PNG encoder.
//!
//! Rendered at 2× (crisp, re-outlined glyphs) for legibility, plus a cropped
//! close-up of the toolbar button + first rows.
//!
//! Run: `cargo run -p cerberus-app --example mirc_preview`

use cerberus_headless::write_png;
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_text::TextEngine;
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use cerberus_ui::{MircPanel, MircRow, MircState, Toolbar};

/// Render scale (physical ÷ logical); 2× keeps the preview crisp when viewed.
const SCALE: f32 = 2.0;

fn main() {
    // A realistic working set: a dozen identities clearing a claims backlog.
    let site = "travel.example.gov";
    let working = roster(&[
        ("claims-01", "a.okafor@dot.gov", MircState::Live, true),
        ("claims-02", "b.nguyen@dot.gov", MircState::Dormant, true),
        ("claims-03", "c.silva@dot.gov", MircState::Dormant, true),
        ("claims-04", "d.adeyemi@dot.gov", MircState::Diverged, false),
        ("claims-05", "e.romano@dot.gov", MircState::Dormant, true),
        ("claims-06", "f.haddad@dot.gov", MircState::Dormant, false),
        ("claims-07", "g.kovac@dot.gov", MircState::Dormant, true),
        ("claims-08", "h.tanaka@dot.gov", MircState::Dormant, true),
        ("claims-09", "i.duarte@dot.gov", MircState::Dormant, false),
        ("claims-10", "j.fischer@dot.gov", MircState::Dormant, true),
        ("claims-11", "k.larsen@dot.gov", MircState::Dormant, true),
        ("claims-12", "l.mwangi@dot.gov", MircState::Dormant, true),
    ]);
    let logical = Size::new(1180, 760);
    let scene = render_scene(logical, site, &working, 0);
    write_png("mirc-panel.png", &scene).expect("write png");

    // A close-up of the top: the MIRC button + badge and the first rows, so the
    // icon and the text↔chip alignment are easy to scrutinize.
    let p = MircPanel::panel_rect(logical);
    let rows_crop = crop(
        &scene,
        Rect::new(
            sx(p.x - 8),
            0,
            su(p.w + 16),
            su((p.y + 134 + 7 * 30) as u32),
        ),
    );
    write_png("mirc-panel-rows.png", &rows_crop).expect("write png");

    // A tight close-up of the toolbar's right side: the MIRC button (multiperson
    // icon) with its broadcasting glow and "N" count badge, beside head+settings.
    let button = crop(
        &scene,
        Rect::new(sx(logical.w as i32 - 150), 0, su(150), su(40)),
    );
    write_png("mirc-button.png", &button).expect("write png");

    // The vision at scale: 100 sealed sessions, the panel scrolled near the top.
    let many: Vec<MircRow> = (0..100)
        .map(|i| MircRow {
            label: format!("claims-{:03}", i + 1),
            account: format!("agent{:03}@dot.gov", i + 1),
            state: match i {
                0 => MircState::Live,
                7 | 41 => MircState::Diverged,
                _ => MircState::Dormant,
            },
            logged_in: i % 5 != 3,
        })
        .collect();
    let scene100 = render_scene(logical, site, &many, 0);
    write_png("mirc-panel-100.png", &scene100).expect("write png");

    println!("wrote mirc-panel.png, mirc-panel-rows.png, mirc-button.png, mirc-panel-100.png");
}

fn sx(v: i32) -> i32 {
    (v as f32 * SCALE).round() as i32
}
fn su(v: u32) -> u32 {
    ((v as f32 * SCALE).round() as u32).max(1)
}

fn roster(items: &[(&str, &str, MircState, bool)]) -> Vec<MircRow> {
    items
        .iter()
        .map(|(label, account, state, logged_in)| MircRow {
            label: (*label).to_string(),
            account: (*account).to_string(),
            state: *state,
            logged_in: *logged_in,
        })
        .collect()
}

/// Render the whole frame (faux page + toolbar + MIRC panel) at `SCALE`.
fn render_scene(logical: Size, site: &str, rows: &[MircRow], scroll: usize) -> Framebuffer {
    let text = TextEngine::new();
    let physical = Size::new(su(logical.w), su(logical.h));
    let mut fb = Framebuffer::new(physical);
    fb.clear(Color::rgb(0xF2, 0xF2, 0xF2));

    text.rasterize(&faux_page(&text, logical, site).scaled(SCALE), &mut fb);

    let mut toolbar = Toolbar::new("claims-01");
    toolbar.url_text = format!("https://{site}/claims/queue");
    toolbar.can_back = true;
    toolbar.broadcasting = true;
    toolbar.sync_count = rows.len();
    text.rasterize(&toolbar.paint(logical, &text).scaled(SCALE), &mut fb);

    text.rasterize(
        &MircPanel::paint(logical, &text, true, site, rows, scroll).scaled(SCALE),
        &mut fb,
    );
    fb
}

/// Copy a physical-pixel sub-rect of `scene` into a new framebuffer.
fn crop(scene: &Framebuffer, region: Rect) -> Framebuffer {
    let mut out = Framebuffer::new(Size::new(region.w, region.h));
    out.clear(Color::WHITE);
    out.blit(Point::new(-region.x, -region.y), scene);
    out
}

/// A minimal faux "claims queue" page (logical coords) so the modal has context.
fn faux_page(text: &TextEngine, logical: Size, site: &str) -> DisplayList {
    let mut list = DisplayList::new();
    let top = cerberus_ui::TOOLBAR_HEIGHT as i32 + 24;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(40, top),
        frac_x: 0.0,
        glyphs: text.shape("Travel Claims — Processing Queue", 24),
        color: Color::rgb(0x22, 0x22, 0x22),
        style: FontStyle::REGULAR,
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(40, top + 30),
        frac_x: 0.0,
        glyphs: text.shape(&format!("{site}  ·  18,412 claims pending"), 14),
        color: Color::rgb(0x70, 0x70, 0x70),
        style: FontStyle::REGULAR,
    });
    for i in 0..10 {
        let y = top + 64 + i * 30;
        if i % 2 == 0 {
            list.push(DisplayItem::Rect {
                rect: Rect::new(40, y - 16, logical.w - 80, 28),
                color: Color::rgb(0xFB, 0xFB, 0xFB),
            });
        }
        list.push(DisplayItem::Glyphs {
            origin: Point::new(48, y),
            frac_x: 0.0,
            glyphs: text.shape(
                &format!(
                    "CLM-2026-{:05}    bump-through    pending review",
                    30481 + i * 7
                ),
                13,
            ),
            color: Color::rgb(0x55, 0x55, 0x55),
            style: FontStyle::REGULAR,
        });
    }
    list
}

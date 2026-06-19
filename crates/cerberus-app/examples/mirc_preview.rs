//! Render a preview PNG of the MIRC (Multi-Identity Remote Control) panel — the
//! Phase 2a roster + orchestrator — so the design can be reviewed without a
//! display server. It composes the *real* UI components (`Toolbar` with its SYNC
//! count badge, and `MircPanel`) over a faux page, exactly as the app paints
//! them, then writes the frame with the headless PNG encoder.
//!
//! Run: `cargo run -p cerberus-app --example mirc_preview`
//! Output: `mirc-panel.png` (a working set) and `mirc-panel-100.png` (at scale).

use cerberus_headless::write_png;
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_text::TextEngine;
use cerberus_types::{Color, FontStyle, Point, Rect, Size};
use cerberus_ui::{MircPanel, MircRow, MircState, Toolbar};

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
    render(Size::new(1360, 860), site, &working, 0, "mirc-panel.png");

    // The vision at scale: 100 sealed sessions, the panel scrolled near the top.
    let many: Vec<MircRow> = (0..100)
        .map(|i| MircRow {
            label: format!("claims-{:02}", i + 1),
            account: format!("agent{:03}@dot.gov", i + 1),
            state: match i {
                0 => MircState::Live,
                7 | 41 => MircState::Diverged,
                _ => MircState::Dormant,
            },
            logged_in: i % 5 != 3,
        })
        .collect();
    render(Size::new(1360, 860), site, &many, 0, "mirc-panel-100.png");

    println!("wrote mirc-panel.png and mirc-panel-100.png");
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

fn render(window: Size, site: &str, rows: &[MircRow], scroll: usize, out: &str) {
    let text = TextEngine::new();
    let mut fb = Framebuffer::new(window);
    fb.clear(Color::rgb(0xF2, 0xF2, 0xF2));

    // A faux page behind the modal, so the panel reads in context.
    paint_faux_page(&mut fb, &text, window, site);

    // The real toolbar, with the SYNC button driven (broadcasting) and wearing
    // the "N profiles" count badge.
    let mut toolbar = Toolbar::new("claims-01");
    toolbar.url_text = format!("https://{site}/claims/queue");
    toolbar.can_back = true;
    toolbar.broadcasting = true;
    toolbar.sync_count = rows.len();
    text.rasterize(&toolbar.paint(window, &text), &mut fb);

    // The MIRC panel itself.
    text.rasterize(
        &MircPanel::paint(window, &text, true, site, rows, scroll),
        &mut fb,
    );

    write_png(out, &fb).expect("write png");
}

/// A minimal faux "claims queue" page so the modal has realistic context.
fn paint_faux_page(fb: &mut Framebuffer, text: &TextEngine, window: Size, site: &str) {
    let mut list = DisplayList::new();
    let top = cerberus_ui::TOOLBAR_HEIGHT as i32 + 24;
    list.push(DisplayItem::Glyphs {
        origin: Point::new(40, top),
        glyphs: text.shape("Travel Claims — Processing Queue", 24),
        color: Color::rgb(0x22, 0x22, 0x22),
        style: FontStyle::REGULAR,
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(40, top + 30),
        glyphs: text.shape(&format!("{site}  ·  18,412 claims pending"), 14),
        color: Color::rgb(0x70, 0x70, 0x70),
        style: FontStyle::REGULAR,
    });
    // A few faux table rows.
    for i in 0..10 {
        let y = top + 64 + i * 30;
        if i % 2 == 0 {
            list.push(DisplayItem::Rect {
                rect: Rect::new(40, y - 16, window.w - 80, 28),
                color: Color::rgb(0xFB, 0xFB, 0xFB),
            });
        }
        list.push(DisplayItem::Glyphs {
            origin: Point::new(48, y),
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
    text.rasterize(&list, fb);
}

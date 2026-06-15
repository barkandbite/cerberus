//! Proof that a mirror group yields *independently rendered* windows (ADR-0017).
//!
//! The model is session-pure, but the point of multi-window is N separate
//! pictures on screen. This drives a group, catches a follower up, then renders
//! each window's converged document through the real style → layout → paint
//! pipeline (the same `render_document` the app uses, with the dependency-free
//! `MonoShaper`/`BoxRasterizer` the headless crate tests with) and asserts each
//! window painted, and that the two windows — different sessions of one site —
//! paint *differently*. The render stack is a dev-dependency only, so the
//! `cerberus-mirror` model itself stays pure.

use cerberus_css::CssEngine;
use cerberus_dom::{parse_html, Document};
use cerberus_headless::render_document;
use cerberus_js::JsEngineFactory;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{BlockLayout, NoForms, NoImages};
use cerberus_mirror::{Action, MirrorGroup, MirrorInstance, PageSource};
use cerberus_paint::{BoxRasterizer, Framebuffer, MonoShaper};
use cerberus_style::StyleEngine;
use cerberus_types::{Color, InstanceId, Size};

const SIZE: Size = Size::new(320, 240);

/// Serves each session a page that names its own identity, and gives different
/// identities different *structure* — exactly what running multiple accounts of
/// one site looks like (each account sees its own content).
struct VariablePage {
    master: InstanceId,
}

impl PageSource for VariablePage {
    fn load(&self, instance: InstanceId, _url: &str) -> Result<Document, String> {
        let extra = if instance == self.master {
            ""
        } else {
            "<p>an extra paragraph only this session sees</p>"
        };
        Ok(parse_html(&format!(
            "<h1 id=\"who\">Session {instance}</h1><p>shared</p>{extra}"
        )))
    }
}

/// Render an instance's converged document through the real pipeline.
fn render_instance(inst: &MirrorInstance) -> Framebuffer {
    let styled = CssEngine::new().style(inst.document());
    render_document(
        &styled,
        SIZE,
        Color::WHITE,
        &mut BlockLayout::default(),
        &MonoShaper,
        &BoxRasterizer,
        &NoImages,
        &NoForms,
    )
}

fn frame_pixels(fb: &Framebuffer) -> Vec<Option<Color>> {
    let mut pixels = Vec::with_capacity((SIZE.w * SIZE.h) as usize);
    for y in 0..SIZE.h {
        for x in 0..SIZE.w {
            pixels.push(fb.pixel(x, y));
        }
    }
    pixels
}

/// At least one non-background pixel was painted.
fn any_painted(fb: &Framebuffer) -> bool {
    (0..SIZE.h).any(|y| (0..SIZE.w).any(|x| fb.pixel(x, y) != Some(Color::WHITE)))
}

#[test]
fn each_window_renders_its_own_session_independently() {
    let master = InstanceId::from_u64_pair(0, 1);
    let follower = InstanceId::from_u64_pair(0, 2);
    let engine = QuickJsEngineFactory.instantiate().expect("engine");
    let members = vec![
        (master, "master".to_string()),
        (follower, "follower".to_string()),
    ];
    let source = VariablePage { master };
    let mut g =
        MirrorGroup::new(engine, Box::new(source), members, (SIZE.w, SIZE.h), "ua").unwrap();

    g.act(Action::Navigate("https://app.test/".into())).unwrap();
    g.focus(1).unwrap();

    let fb_master = render_instance(g.master());
    let fb_follower = render_instance(g.instance(1).unwrap());

    // Each window actually painted something...
    assert!(any_painted(&fb_master), "master window painted content");
    assert!(any_painted(&fb_follower), "follower window painted content");
    // ...and the two sessions paint differently (their DOMs differ).
    assert_ne!(
        frame_pixels(&fb_master),
        frame_pixels(&fb_follower),
        "two windows of one site under different identities must render differently"
    );
}

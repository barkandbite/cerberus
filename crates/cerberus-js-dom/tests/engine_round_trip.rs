//! Engine-driven bridge tests: run real page scripts against a real QuickJS
//! realm via [`run_page_scripts`] and assert the rebuilt Rust DOM reflects their
//! mutations.
//!
//! Each test snapshots a small starting [`Document`], runs one or more scripts,
//! and inspects the reconciled result. The QuickJS realm already installs the
//! speed-first prelude at `create_realm` (timers/observers fire immediately), so
//! these tests also confirm the two prelude layers compose.

use cerberus_dom::{Document, DocumentBuilder, NodeRef};
use cerberus_js::{JsEngine, JsEngineFactory};
use cerberus_js_dom::run_page_scripts;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

/// A fresh QuickJS engine with one realm created, plus that realm's id.
fn engine_and_realm() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    (engine, realm)
}

/// Depth-first search for the first element (or `node` itself) with the given
/// tag.
fn find_tag<'a>(node: NodeRef<'a>, tag: &str) -> Option<NodeRef<'a>> {
    if node.is_element() && node.tag() == tag {
        return Some(node);
    }
    node.children().find_map(|c| find_tag(c, tag))
}

/// Depth-first search for the first element whose `id` attribute matches.
fn find_id<'a>(node: NodeRef<'a>, id: &str) -> Option<NodeRef<'a>> {
    if node.is_element() && node.attr("id") == Some(id) {
        return Some(node);
    }
    node.children().find_map(|c| find_id(c, id))
}

/// `<html><body><div id="x">old</div></body></html>`.
fn doc_with_div_x() -> Document {
    let mut b = DocumentBuilder::new();
    let old = b.text("old");
    let div = b.element_attrs("div", vec![("id".into(), "x".into())], [old]);
    let body = b.element("body", [div]);
    let html = b.element("html", [body]);
    b.finish(html)
}

#[test]
fn script_sets_text_content() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["document.getElementById('x').textContent = 'new'".to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "new");
}

#[test]
fn script_creates_and_appends_element() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var p = document.createElement('p'); \
         p.textContent = 'appended'; \
         document.body.appendChild(p);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let body = find_tag(out.root(), "body").expect("body");
    let p = body
        .children()
        .find(|c| c.is_element() && c.tag() == "p")
        .expect("new <p> under body");
    assert_eq!(p.text_content(), "appended");
}

#[test]
fn script_sets_attribute_and_class() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('data-role', 'banner'); \
         x.classList.add('active'); x.classList.add('big');"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("data-role"), Some("banner"));
    let class = x.attr("class").expect("class attr");
    assert!(class.split(' ').any(|c| c == "active"), "got {class:?}");
    assert!(class.split(' ').any(|c| c == "big"), "got {class:?}");
}

#[test]
fn dom_content_loaded_listener_runs() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "document.addEventListener('DOMContentLoaded', function () { \
           document.getElementById('x').textContent = 'ready'; \
         });"
        .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.text_content(),
        "ready",
        "DOMContentLoaded listener should have fired during fire-load"
    );
}

#[test]
fn throwing_script_does_not_abort_run() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "throw new Error('boom')".to_string(),
        "document.body.appendChild(document.createElement('span'))".to_string(),
    ];

    // The first script throws; the run must continue and still return Ok.
    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run returns Ok");
    let body = find_tag(out.root(), "body").expect("body");
    assert!(
        body.children().any(|c| c.is_element() && c.tag() == "span"),
        "second script must still run after the first throws"
    );
}

#[test]
fn console_log_is_captured() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["console.log('hello', 42)".to_string()];

    // run_page_scripts leaves the realm intact, so we can read the capture buffer
    // out of the same realm with a follow-up eval.
    run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let joined = engine
        .eval(realm, "globalThis.__cerberusConsole.join('|')")
        .expect("read console");
    match joined {
        cerberus_js::JsValue::Str(s) => assert!(
            s.contains("hello 42"),
            "console capture should contain 'hello 42', got {s:?}"
        ),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn speed_first_still_applies() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    // setTimeout with a long delay must still have fired by serialize time,
    // because the QuickJS realm's speed-first prelude runs it immediately.
    let scripts = vec![
        "setTimeout(function () { document.getElementById('x').textContent = 'timed'; }, 9999);"
            .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.text_content(),
        "timed",
        "speed-first setTimeout should have fired immediately"
    );
}

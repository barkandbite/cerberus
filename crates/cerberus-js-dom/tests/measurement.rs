//! Layout-measurement bridge (ADR-0021): geometry → getBoundingClientRect,
//! cascaded styles → getComputedStyle, and viewport-aware matchMedia, over the
//! real QuickJS engine.

use cerberus_dom::{parse_html, Document, NodeRef};
use cerberus_js::{JsEngineFactory, JsValue};
use cerberus_js_dom::{install_page, set_computed_styles, set_geometry, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::{RealmId, Rect};

fn node_id(doc: &Document, id: &str) -> u64 {
    fn rec(n: NodeRef<'_>, id: &str) -> Option<u32> {
        if n.attr("id") == Some(id) {
            return Some(n.id());
        }
        n.children().find_map(|c| rec(c, id))
    }
    rec(doc.root(), id).expect("node present") as u64
}

#[test]
fn geometry_styles_and_media_are_observable_from_js() {
    let mut engine = QuickJsEngineFactory.instantiate().expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("realm");
    let doc = parse_html("<div id=\"x\">hi</div>");
    let env = PageEnv {
        url: "https://t.test/".into(),
        viewport: (800, 600),
        user_agent: "ua".into(),
        cookies: String::new(),
        local_storage: String::new(),
    };
    install_page(engine.as_mut(), realm, &doc, &env).expect("install");
    let id = node_id(&doc, "x");

    // getBoundingClientRect reflects pushed geometry.
    set_geometry(engine.as_mut(), realm, &[(id, Rect::new(10, 20, 30, 40))]).unwrap();
    assert_eq!(
        engine
            .eval(
                realm,
                "document.getElementById('x').getBoundingClientRect().width"
            )
            .unwrap(),
        JsValue::Number(30.0)
    );
    assert_eq!(
        engine
            .eval(
                realm,
                "document.getElementById('x').getBoundingClientRect().bottom"
            )
            .unwrap(),
        JsValue::Number(60.0) // y(20) + h(40)
    );

    // getComputedStyle reflects pushed cascade values.
    set_computed_styles(
        engine.as_mut(),
        realm,
        &[(id, vec![("color".into(), "rgb(1, 2, 3)".into())])],
    )
    .unwrap();
    assert_eq!(
        engine
            .eval(
                realm,
                "getComputedStyle(document.getElementById('x')).color"
            )
            .unwrap(),
        JsValue::Str("rgb(1, 2, 3)".into())
    );

    // matchMedia honors the viewport (800x600).
    assert_eq!(
        engine
            .eval(realm, "matchMedia('(max-width: 1000px)').matches ? 1 : 0")
            .unwrap(),
        JsValue::Number(1.0)
    );
    assert_eq!(
        engine
            .eval(realm, "matchMedia('(min-width: 1000px)').matches ? 1 : 0")
            .unwrap(),
        JsValue::Number(0.0)
    );
}

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

#[test]
fn computed_style_width_height_resolve_from_geometry() {
    let mut engine = QuickJsEngineFactory.instantiate().expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("realm");
    let doc = parse_html("<div id=\"x\" style=\"width:auto\">hi</div>");
    let env = PageEnv {
        url: "https://t.test/".into(),
        viewport: (800, 600),
        user_agent: "ua".into(),
        cookies: String::new(),
        local_storage: String::new(),
    };
    install_page(engine.as_mut(), realm, &doc, &env).expect("install");
    let id = node_id(&doc, "x");

    // Before layout: width reflects the authored (inline) value.
    assert_eq!(
        engine
            .eval(
                realm,
                "getComputedStyle(document.getElementById('x')).width"
            )
            .unwrap(),
        JsValue::Str("auto".into())
    );
    // After geometry is bridged, width/height are the used pixels (like a browser).
    set_geometry(engine.as_mut(), realm, &[(id, Rect::new(0, 0, 240, 100))]).unwrap();
    assert_eq!(
        engine
            .eval(
                realm,
                "getComputedStyle(document.getElementById('x')).width"
            )
            .unwrap(),
        JsValue::Str("240px".into())
    );
    assert_eq!(
        engine
            .eval(
                realm,
                "getComputedStyle(document.getElementById('x')).getPropertyValue('height')"
            )
            .unwrap(),
        JsValue::Str("100px".into())
    );
}

#[test]
fn offset_and_client_metrics_reflect_bridged_geometry() {
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
    set_geometry(engine.as_mut(), realm, &[(id, Rect::new(10, 20, 30, 40))]).unwrap();

    // offset*/client*/scroll* sizes come from the same bridged geometry (w=30,
    // h=40) as getBoundingClientRect, as rounded integers.
    for (expr, want) in [
        ("offsetWidth", 30.0),
        ("offsetHeight", 40.0),
        ("clientWidth", 30.0),
        ("clientHeight", 40.0),
        ("scrollWidth", 30.0),
        ("scrollHeight", 40.0),
        ("offsetLeft", 10.0),
        ("offsetTop", 20.0),
        ("scrollTop", 0.0),
        ("scrollLeft", 0.0),
    ] {
        assert_eq!(
            engine
                .eval(realm, &format!("document.getElementById('x').{expr}"))
                .unwrap(),
            JsValue::Number(want),
            "{expr}"
        );
    }
    // scrollTop is settable (code that sets it must not throw) but stays 0 (no
    // scroll model).
    engine
        .eval(realm, "document.getElementById('x').scrollTop = 50")
        .unwrap();
    assert_eq!(
        engine
            .eval(realm, "document.getElementById('x').scrollTop")
            .unwrap(),
        JsValue::Number(0.0)
    );
    // offsetParent of an in-flow element is the body.
    assert_eq!(
        engine
            .eval(
                realm,
                "document.getElementById('x').offsetParent === document.body ? 1 : 0"
            )
            .unwrap(),
        JsValue::Number(1.0)
    );
}

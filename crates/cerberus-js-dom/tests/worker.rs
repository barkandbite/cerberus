//! End-to-end tests for the single-thread `Worker` + `Blob` + object-URL shims
//! in the DOM prelude. These prove a real Worker round-trip works — the sensor
//! probe could never exercise it because the reese84 sensor never spawns a
//! worker, so this is where we validate the mechanism itself.

use cerberus_dom::{Document, DocumentBuilder};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{install_page, run_event_loop, EventLoopBudget, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

fn blank_document() -> Document {
    let mut b = DocumentBuilder::new();
    let head = b.element("head", []);
    let body = b.element("body", []);
    let html = b.element("html", [head, body]);
    b.finish(html)
}

fn engine_with_page() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    let env = PageEnv {
        url: "https://example.test/".into(),
        viewport: (1280, 800),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"
            .into(),
        cookie: String::new(),
    };
    let doc = blank_document();
    install_page(engine.as_mut(), realm, &doc, &env).expect("install page");
    (engine, realm)
}

/// Drain timers/microtasks a few times so deferred `postMessage` deliveries fire.
fn pump(engine: &mut dyn JsEngine, realm: RealmId) {
    for _ in 0..8 {
        let _ = run_event_loop(engine, realm, EventLoopBudget::default());
    }
}

#[test]
fn worker_round_trips_a_message_through_a_blob_url() {
    let (mut engine, realm) = engine_with_page();

    // Classic pattern: build worker source as a Blob, mint an object URL, spawn
    // the Worker, and exchange one message. The worker doubles what it receives.
    engine
        .eval(
            realm,
            r#"
            globalThis.__result = null;
            var src = "self.onmessage = function (e) { self.postMessage(e.data * 2); };";
            var blob = new Blob([src], { type: "application/javascript" });
            var url = URL.createObjectURL(blob);
            var w = new Worker(url);
            w.onmessage = function (e) { globalThis.__result = e.data; };
            w.postMessage(21);
            "#,
        )
        .expect("spawn worker");

    pump(engine.as_mut(), realm);

    let result = engine
        .eval(realm, "String(globalThis.__result)")
        .expect("read result");
    assert_eq!(
        result,
        JsValue::Str("42".into()),
        "worker should have doubled 21 -> 42 and posted it back"
    );
}

#[test]
fn worker_supports_addeventlistener_and_multiple_messages() {
    let (mut engine, realm) = engine_with_page();

    // The worker accumulates a running total across messages and reports it back;
    // both sides use addEventListener('message', ...) rather than onmessage.
    engine
        .eval(
            realm,
            r#"
            globalThis.__log = [];
            var src = "var total = 0;" +
                      "self.addEventListener('message', function (e) {" +
                      "  total += e.data;" +
                      "  self.postMessage(total);" +
                      "});";
            var url = URL.createObjectURL(new Blob([src]));
            var w = new Worker(url);
            w.addEventListener("message", function (e) { globalThis.__log.push(e.data); });
            w.postMessage(5);
            w.postMessage(10);
            w.postMessage(100);
            "#,
        )
        .expect("spawn worker");

    pump(engine.as_mut(), realm);

    let log = engine
        .eval(realm, "globalThis.__log.join(',')")
        .expect("read log");
    assert_eq!(
        log,
        JsValue::Str("5,15,115".into()),
        "worker keeps state across messages and reports the running total"
    );
}

#[test]
fn worker_can_use_shared_primitives_atob_and_json() {
    let (mut engine, realm) = engine_with_page();

    // A worker that does real work: base64-decode + JSON-parse a payload and post
    // back a computed field. Proves the worker scope inherits atob/JSON/etc.
    engine
        .eval(
            realm,
            r#"
            globalThis.__out = null;
            var src =
              "self.onmessage = function (e) {" +
              "  var json = atob(e.data);" +
              "  var obj = JSON.parse(json);" +
              "  self.postMessage(obj.a + obj.b);" +
              "};";
            var url = URL.createObjectURL(new Blob([src]));
            var w = new Worker(url);
            w.onmessage = function (e) { globalThis.__out = e.data; };
            w.postMessage(btoa(JSON.stringify({ a: 40, b: 2 })));
            "#,
        )
        .expect("spawn worker");

    pump(engine.as_mut(), realm);

    let out = engine
        .eval(realm, "String(globalThis.__out)")
        .expect("read out");
    assert_eq!(out, JsValue::Str("42".into()));
}

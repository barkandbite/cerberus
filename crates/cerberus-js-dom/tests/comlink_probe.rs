//! Validate our Worker/Blob shims against a REAL third-party Worker SDK:
//! Google's Comlink (an RPC-over-postMessage library). If a Comlink object
//! exposed in a worker can be `wrap`ped and called from the main thread and
//! returns the right value, our Worker + Blob + object-URL + postMessage stack
//! is correct against code we didn't write.
//!
//! Comlink is not committed. Fetch it and run explicitly:
//! ```text
//! curl -L https://unpkg.com/comlink@4.4.1/dist/umd/comlink.min.js -o /tmp/comlink.js
//! CERB_COMLINK=/tmp/comlink.js cargo test -p cerberus-js-dom --test comlink_probe \
//!     -- --ignored --nocapture
//! ```

use cerberus_dom::{Document, DocumentBuilder};
use cerberus_js::{JsEngineFactory, JsValue};
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

#[test]
#[ignore = "needs a locally-fetched Comlink via CERB_COMLINK; run explicitly"]
fn comlink_rpc_round_trips_through_our_worker() {
    let Ok(path) = std::env::var("CERB_COMLINK") else {
        eprintln!("CERB_COMLINK not set; skipping");
        return;
    };
    let comlink = std::fs::read_to_string(&path).expect("read comlink");
    eprintln!("comlink: {} bytes", comlink.len());

    let mut engine = QuickJsEngineFactory.instantiate().expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("realm");
    let env = PageEnv {
        url: "https://example.test/".into(),
        viewport: (1280, 800),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"
            .into(),
        cookie: String::new(),
    };
    install_page(engine.as_mut(), realm, &blank_document(), &env).expect("install");

    // Load Comlink into the main realm.
    engine.eval(realm, &comlink).expect("load comlink (main)");

    // Worker: bundle Comlink + expose a small API over the worker endpoint.
    let worker_src = format!(
        "{comlink}\n;(self.Comlink||globalThis.Comlink).expose({{ \
            add: function (a, b) {{ return a + b; }}, \
            greet: function (name) {{ return 'hi ' + name; }} \
         }}, self);"
    );
    // Build the worker from a Blob URL and wrap it, then make two RPC calls.
    let driver = format!(
        r#"
        globalThis.__add = null; globalThis.__greet = null; globalThis.__err = null;
        try {{
          var C = self.Comlink || globalThis.Comlink;
          var src = {src};
          var w = new Worker(URL.createObjectURL(new Blob([src])));
          var api = C.wrap(w);
          api.add(40, 2).then(function (r) {{ globalThis.__add = r; }},
                              function (e) {{ globalThis.__err = 'add: ' + e; }});
          api.greet('cerberus').then(function (r) {{ globalThis.__greet = r; }},
                                    function (e) {{ globalThis.__err = 'greet: ' + e; }});
        }} catch (e) {{ globalThis.__err = '' + e + ' ' + ((e && e.stack) || ''); }}
        "#,
        src = js_string_literal(&worker_src),
    );
    engine.eval(realm, &driver).expect("run comlink driver");

    // Comlink chains promises across several message hops; pump generously.
    for _ in 0..40 {
        let _ = run_event_loop(engine.as_mut(), realm, EventLoopBudget::default());
    }

    let report = engine
        .eval(
            realm,
            "JSON.stringify({ add: globalThis.__add, greet: globalThis.__greet, err: globalThis.__err || null })",
        )
        .expect("read report");
    match report {
        JsValue::Str(s) => eprintln!("\n===== comlink probe =====\n{s}\n========================="),
        other => eprintln!("unexpected: {other:?}"),
    }

    let add = engine.eval(realm, "String(globalThis.__add)").unwrap();
    assert_eq!(
        add,
        JsValue::Str("42".into()),
        "Comlink RPC add(40,2) should resolve to 42 through our Worker"
    );
}

/// Encode `s` as a JS double-quoted string literal (enough for embedding source).
fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

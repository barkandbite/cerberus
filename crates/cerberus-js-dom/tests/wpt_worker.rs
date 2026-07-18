//! Run the REAL Web Platform Tests harness (`testharness.js`) inside our Worker
//! shim and execute WPT-style assertions, then read back the results testharness
//! posts to the parent (the `DedicatedWorkerTestEnvironment` reports via
//! `self.postMessage`). This validates our Worker + `importScripts` against the
//! spec authors' own conformance framework — the authoritative check.
//!
//! `testharness.js` (~200 KB) is not committed. Fetch it and run explicitly:
//! ```text
//! curl -L https://wpt.live/resources/testharness.js -o /tmp/wpt/testharness.js
//! CERB_TESTHARNESS=/tmp/wpt/testharness.js \
//!   cargo test -p cerberus-js-dom --test wpt_worker -- --ignored --nocapture
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

/// The battery of assertions run inside the worker. These are the exact
/// `assert_*` / `test` / `promise_test` primitives real WPT worker tests use.
const WORKER_TEST_BODY: &str = r#"
importScripts('/resources/testharness.js');
test(function () { assert_equals(1 + 1, 2); }, 'arithmetic');
test(function () { assert_true([1, 2, 3].indexOf(2) !== -1, 'includes'); }, 'array-membership');
test(function () { assert_array_equals([1, 2, 3].map(function (x) { return x * 2; }), [2, 4, 6]); }, 'array-map');
test(function () { assert_throws_js(TypeError, function () { var o = null; return o.x; }); }, 'throws-typeerror');
test(function () {
  assert_equals(typeof self.postMessage, 'function', 'worker has postMessage');
  assert_equals(typeof importScripts, 'function', 'worker has importScripts');
  assert_equals(typeof self.location, 'object', 'worker has location');
}, 'worker-global-surface');
test(function () {
  assert_true(new TypeError('x') instanceof Error, 'TypeError is an Error');
}, 'error-instanceof');
promise_test(function () {
  return Promise.resolve(21).then(function (v) { assert_equals(v * 2, 42); });
}, 'promise-resolves');
done();
"#;

#[test]
#[ignore = "needs testharness.js via CERB_TESTHARNESS; run explicitly"]
fn wpt_testharness_runs_in_our_worker() {
    let Ok(path) = std::env::var("CERB_TESTHARNESS") else {
        eprintln!("CERB_TESTHARNESS not set; skipping");
        return;
    };
    let testharness = std::fs::read_to_string(&path).expect("read testharness.js");
    eprintln!("testharness.js: {} bytes", testharness.len());

    let mut engine = QuickJsEngineFactory.instantiate().expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("realm");
    let env = PageEnv {
        url: "https://wpt.test/workers/test.html".into(),
        viewport: (1280, 800),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"
            .into(),
        cookie: String::new(),
    };
    install_page(engine.as_mut(), realm, &blank_document(), &env).expect("install");

    // Populate the worker script cache so importScripts('/resources/testharness.js')
    // resolves to the real harness.
    let cache_setup = format!(
        "globalThis.__cerberusScriptCache = globalThis.__cerberusScriptCache || {{}};\n\
         globalThis.__cerberusScriptCache['/resources/testharness.js'] = {};",
        js_string_literal(&testharness)
    );
    engine.eval(realm, &cache_setup).expect("seed script cache");

    // Spawn a worker that imports testharness and runs the assertion battery,
    // and collect the messages testharness posts back (start/result/complete).
    let driver = format!(
        r#"
        globalThis.__complete = null; globalThis.__results = []; globalThis.__spawnErr = null; globalThis.__diag = [];
        try {{
          var w = new Worker(URL.createObjectURL(new Blob([{body}])));
          w.onmessage = function (e) {{
            var m = e.data;
            if (!m || typeof m !== 'object') return;
            if (m.type === 'diag') globalThis.__diag.push(m);
            if (m.type === 'result') globalThis.__results.push({{ name: m.name, status: m.status, message: m.message }});
            if (m.type === 'complete') globalThis.__complete = m;
          }};
          w.onerror = function (e) {{ globalThis.__spawnErr = (e && (e.message || e.error)) || 'worker error'; }};
        }} catch (e) {{ globalThis.__spawnErr = '' + e + ' ' + ((e && e.stack) || ''); }}
        "#,
        body = js_string_literal(WORKER_TEST_BODY),
    );
    engine.eval(realm, &driver).expect("run driver");

    // testharness posts start -> per-test result -> complete; pump generously.
    for _ in 0..60 {
        let _ = run_event_loop(engine.as_mut(), realm, EventLoopBudget::default());
    }

    let report = engine
        .eval(
            realm,
            "JSON.stringify({ \
               spawnErr: globalThis.__spawnErr || null, \
               diag: globalThis.__diag || [], \
               completed: !!globalThis.__complete, \
               harnessStatus: globalThis.__complete ? globalThis.__complete.status : null, \
               tests: (globalThis.__complete && globalThis.__complete.tests) ? \
                 globalThis.__complete.tests.map(function (t) { return { name: t.name, status: t.status, message: t.message }; }) : \
                 globalThis.__results, \
               workerImports: globalThis.__cerberusWorkerImports || [] \
             }, null, 1)",
        )
        .expect("read report");

    if let JsValue::Str(s) = &report {
        eprintln!(
            "\n===== WPT testharness-in-worker =====\n{s}\n====================================="
        );
    }

    // Assert the harness actually completed and every test passed (status 0).
    let completed = engine
        .eval(realm, "String(!!globalThis.__complete)")
        .unwrap();
    assert_eq!(
        completed,
        JsValue::Str("true".into()),
        "testharness must run to completion in the worker and post a 'complete' message"
    );
    let all_pass = engine
        .eval(
            realm,
            "String(globalThis.__complete.tests.every(function (t) { return t.status === 0; }))",
        )
        .unwrap();
    assert_eq!(
        all_pass,
        JsValue::Str("true".into()),
        "every WPT assertion must pass (status 0) in the worker"
    );
}

/// Encode `s` as a JS double-quoted string literal safe to embed in eval source.
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

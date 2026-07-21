//! Instrumented run of a real Imperva/Incapsula (reese84) sensor inside our
//! engine, to *measure* which browser APIs it needs that we don't yet provide —
//! instead of guessing at fingerprint-surface coverage (the design doc's
//! Phase-E "scoping spike", `docs/ideas/reese84-bot-challenge.md`).
//!
//! We are not solving or forging anything: we host the site's own unmodified
//! sensor and observe what a faithful modern-browser environment must expose so
//! their code runs to completion — the same thing every real browser does.
//!
//! The sensor is an ~800 KB obfuscated VM and is **not** committed. Point the
//! probe at a locally-fetched copy and run it explicitly:
//!
//! ```text
//! curl -A '<chrome-ua>' https://www.pokemoncenter.com/<sensor-path> -o /tmp/sensor.js
//! CERB_SENSOR=/tmp/sensor.js cargo test -p cerberus-js-dom --test reese84_probe \
//!     -- --ignored --nocapture
//! ```
//!
//! Output: the global identifiers the sensor referenced that our environment
//! lacks (with hit counts), the `navigator`/`screen` properties it read that
//! came back `undefined`, and the first hard error (with stack) if it threw.
//! That list is the Phase-E work queue, derived empirically.

use std::time::Duration;

use cerberus_dom::{Document, DocumentBuilder};
use cerberus_js::{JsEngine, JsValue};
use cerberus_js_dom::{fire_load, install_page, run_event_loop, EventLoopBudget, PageEnv};
use cerberus_js_quickjs::QuickJsEngine;
use cerberus_types::RealmId;

/// A Chrome-on-Windows UA matching the persona the sensor should see.
const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// An interstitial-shaped document that includes the sensor's own
/// `<script src=…>` element — the sensor looks itself up in the DOM by `src` and
/// reads challenge params off the tag, so it must be present. `src` matches the
/// URL the sensor was fetched from (`CERB_SENSOR_SRC`).
fn interstitial_document(script_src: &str) -> Document {
    let mut b = DocumentBuilder::new();
    let script = b.element_attrs(
        "script",
        vec![
            ("src".into(), script_src.into()),
            ("async".into(), String::new()),
        ],
        [],
    );
    // A same-origin challenge iframe, as the real interstitial serves.
    let iframe = b.element_attrs(
        "iframe",
        vec![
            ("id".into(), "main-iframe".into()),
            ("src".into(), "/_Incapsula_Resource?SWUDNSAI=31".into()),
        ],
        [],
    );
    let head = b.element("head", [script]);
    let body = b.element("body", [iframe]);
    let html = b.element("html", [head, body]);
    b.finish(html)
}

/// Non-invasive instrumentation. Imperva sensors detect a tampered environment
/// (e.g. `XMLHttpRequest.prototype.open.toString()` no longer `[native code]`)
/// and go dormant, so we must NOT hook natives. We only install a `with`-scope
/// trap (invisible to the sensor) recording *free* identifiers it reads that our
/// global lacks — returned as `undefined` so one run surfaces every gap instead
/// of stopping at the first — plus logging getters for a few commonly-probed
/// globals we don't define at all (defining an absent name is far less
/// detectable than replacing a present native).
///
/// Whether the sensor actually submits is then read from the engine's own real
/// sinks after the run — `__cerberusFetchQueue` and `__cerberusCookieWrites` —
/// which the untouched XHR/`document.cookie` shims feed natively.
const INSTRUMENT: &str = r#"
globalThis.__missing = {};
globalThis.__trap = new Proxy(Object.create(null), {
  has: function (_t, p) { try { return !(p in globalThis); } catch (e) { return false; } },
  get: function (_t, p) {
    var k = String(p);
    globalThis.__missing[k] = (globalThis.__missing[k] || 0) + 1;
    return undefined;
  }
});
['Blob', 'Worker', 'SharedWorker', 'URL', 'WebAssembly', 'OffscreenCanvas',
 'MutationObserver', 'importScripts', 'createImageBitmap', 'requestIdleCallback',
 'ReadableStream', 'BroadcastChannel'
].forEach(function (name) {
  if (!(name in globalThis)) {
    try {
      Object.defineProperty(globalThis, name, {
        configurable: true,
        get: function () { globalThis.__missing[name] = (globalThis.__missing[name] || 0) + 1; return undefined; }
      });
    } catch (e) { /* ignore */ }
  }
});
"#;

/// Simulate a little user interaction — reese84 gathers behavioral entropy
/// (pointer/mouse/key/touch/scroll) and may hold its submission until it has
/// some. Dispatch a burst of plausible events to both `document` and `window`.
const INTERACTION_SIM: &str = r#"
(function () {
  var types = ['pointermove','mousemove','pointerdown','mousedown','mouseup','click',
               'keydown','keyup','touchstart','touchmove','touchend','scroll','wheel','focus','blur'];
  var fired = 0;
  for (var round = 0; round < 6; round++) {
    types.forEach(function (t) {
      var ev = { type: t, bubbles: true, cancelable: true, isTrusted: true, timeStamp: round * 16,
                 clientX: 80 + round * 11, clientY: 140 + round * 7, pageX: 80, pageY: 140,
                 screenX: 90, screenY: 160, movementX: 4, movementY: 3, button: 0, buttons: 1,
                 key: 'a', code: 'KeyA', keyCode: 65, which: 65, target: globalThis.document,
                 touches: [{ clientX: 80, clientY: 140 }], changedTouches: [{ clientX: 80, clientY: 140 }] };
      try { if (globalThis.document && globalThis.document.dispatchEvent) globalThis.document.dispatchEvent(ev); } catch (e) {}
      try { if (globalThis.dispatchEvent) globalThis.dispatchEvent(ev); } catch (e) {}
      fired++;
    });
  }
  globalThis.__simFired = fired;
})();
"#;

#[test]
#[ignore = "needs a locally-fetched sensor via CERB_SENSOR; run explicitly"]
fn reese84_sensor_api_surface_probe() {
    let Ok(path) = std::env::var("CERB_SENSOR") else {
        eprintln!("CERB_SENSOR not set; skipping (see this file's header for usage)");
        return;
    };
    let sensor = std::fs::read_to_string(&path).expect("read sensor file");
    eprintln!("sensor: {} bytes from {path}", sensor.len());

    // A generous eval budget: the VM plus scope-trap overhead is far slower than
    // a normal page script, and this is a one-shot diagnostic, not production.
    let mut engine = QuickJsEngine::with_eval_timeout(Duration::from_secs(120)).expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("realm");

    let env = PageEnv {
        url: "https://www.pokemoncenter.com/".into(),
        viewport: (1280, 800),
        user_agent: CHROME_UA.into(),
        cookie: String::new(),
    };
    let script_src = std::env::var("CERB_SENSOR_SRC")
        .unwrap_or_else(|_| "/vice-come-Soldenyson-it-non-Banquoh-Chare-Hart-C".into());
    let doc = interstitial_document(&script_src);
    install_page(&mut engine, realm, &doc, &env).expect("install DOM prelude");

    engine
        .eval(realm, INSTRUMENT)
        .expect("install instrumentation");

    // Run the sensor inside the scope trap; capture any hard throw + stack.
    let wrapped = format!(
        "try {{ with (globalThis.__trap) {{\n{sensor}\n}} }} \
         catch (e) {{ globalThis.__err = '' + e + '\\n' + ((e && e.stack) || ''); }}"
    );
    // A thrown *engine* error (e.g. deadline) is itself a finding — report it.
    if let Err(e) = engine.eval(realm, &wrapped) {
        eprintln!("engine-level error running sensor: {e:?}");
    }

    // The handshake is usually deferred (setTimeout / DOMContentLoaded / XHR
    // callbacks). Pump the event loop so that work fires and its output-sink
    // actions are recorded.
    for _ in 0..8 {
        let _ = run_event_loop(&mut engine, realm, EventLoopBudget::default());
    }
    // The submission is commonly gated on the document lifecycle. Use the real
    // bridge (sets readyState='complete' + dispatches DOMContentLoaded/load the
    // way the app does), then also fire the softer signals, and pump again.
    let readystate_before = engine
        .eval(
            realm,
            "String((globalThis.document && document.readyState) || '?')",
        )
        .ok();
    let _ = fire_load(&mut engine, realm);
    for _ in 0..8 {
        let _ = run_event_loop(&mut engine, realm, EventLoopBudget::default());
    }
    let readystate_after = engine
        .eval(
            realm,
            "String((globalThis.document && document.readyState) || '?')",
        )
        .ok();
    eprintln!("readyState {readystate_before:?} -> {readystate_after:?}");

    // Behavioral-entropy path: simulate input, then pump again.
    let _ = engine.eval(realm, INTERACTION_SIM);
    for _ in 0..8 {
        let _ = run_event_loop(&mut engine, realm, EventLoopBudget::default());
    }

    let report = engine
        .eval(
            realm,
            "JSON.stringify({ \
               err: globalThis.__err || null, \
               missingGlobals: globalThis.__missing || {}, \
               fetchQueue: (globalThis.__cerberusFetchQueue || []).map(function (f) { \
                 return (f.method || 'GET') + ' ' + f.url + \
                        (f.body != null ? (' body=' + String(f.body).length + 'B') : ''); }), \
               cookieWrites: globalThis.__cerberusCookieWrites || [], \
               cookieAfter: (globalThis.document && globalThis.document.cookie) || '', \
               submitPrimitives: { \
                 XMLHttpRequest: typeof globalThis.XMLHttpRequest, \
                 fetch: typeof globalThis.fetch, \
                 sendBeacon: (globalThis.navigator && typeof globalThis.navigator.sendBeacon) || 'none', \
                 WebSocket: typeof globalThis.WebSocket, \
                 Image: typeof globalThis.Image, \
                 Blob: typeof globalThis.Blob, \
                 Worker: typeof globalThis.Worker \
               }, \
               workerScripts: globalThis.__cerberusWorkerScripts || [], \
               workerImports: globalThis.__cerberusWorkerImports || [], \
               simFired: globalThis.__simFired || 0, \
               storage: (function () { \
                 function dump(s) { var o = {}; try { for (var i = 0; i < s.length; i++) { \
                   var k = s.key(i); var v = s.getItem(k); o[k] = (v == null ? null : String(v).slice(0, 120)); } } catch (e) { o.__err = '' + e; } return o; } \
                 return { local: dump(globalThis.localStorage || {length:0,key:function(){},getItem:function(){}}), \
                          session: dump(globalThis.sessionStorage || {length:0,key:function(){},getItem:function(){}}) }; \
               })() \
             }, null, 0)",
        )
        .expect("read report");

    match report {
        JsValue::Str(json) => {
            eprintln!("\n===== reese84 sensor API-surface probe =====\n{json}\n============================================");
        }
        other => eprintln!("unexpected report value: {other:?}"),
    }
}

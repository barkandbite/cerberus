//! Conformance fixes for three defects an adversarial review confirmed on the
//! DOM surface: a `PermissionStatus` that was not an `EventTarget`, an all-zero
//! `performance` clock on a loaded document, and `TextEncoder` emitting raw
//! WTF-8 for unpaired surrogates instead of the U+FFFD replacement real Chrome
//! produces. Each is a concrete, byte- or type-level divergence a sensor probes.

use cerberus_dom::parse_html;
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{install_page, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

fn eval_str(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> String {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Str(s) => s,
        other => panic!("expected string from {src:?}, got {other:?}"),
    }
}

fn installed() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    let doc = parse_html("<html><body></body></html>");
    let env = PageEnv {
        url: "https://example.test/".into(),
        viewport: (1280, 800),
        user_agent: "Cerberus/0.0".into(),
        cookie: String::new(),
    };
    install_page(engine.as_mut(), realm, &doc, &env).expect("install page");
    (engine, realm)
}

#[test]
fn permission_status_is_a_subscribable_event_target() {
    // The common pattern permissions.query({name}).then(s => s.addEventListener(
    // 'change', cb)) must not throw; a plain {state,onchange} object (the old
    // shape) makes s.addEventListener undefined.
    let (mut engine, realm) = installed();
    let e = engine.as_mut();
    e.eval(
        realm,
        r#"
        globalThis.__perm = '';
        navigator.permissions.query({name:'notifications'}).then(function(s){
            var line = typeof s.addEventListener + '|' + typeof s.removeEventListener
                     + '|' + s.state + '|' + s.name;
            try { s.addEventListener('change', function(){}); line += '|subscribed'; }
            catch (err) { line += '|threw'; }
            globalThis.__perm = line;
        });
        void 0
    "#,
    )
    .expect("query");
    assert_eq!(
        eval_str(e, realm, "globalThis.__perm"),
        "function|function|default|notifications|subscribed",
    );
}

#[test]
fn performance_clock_is_epoch_anchored_and_monotonic() {
    // A loaded document with timeOrigin === 0 / all-zero timing is an impossible
    // read; timeOrigin+now() must also track Date.now().
    let (mut engine, realm) = installed();
    let probe = r#"(function(){
        var p = performance, t = p.timing;
        var originOk = p.timeOrigin > 1000000000000;
        var navEq = t.navigationStart === p.timeOrigin;
        var ordered = t.navigationStart < t.fetchStart && t.fetchStart < t.responseEnd
                   && t.responseEnd < t.domComplete && t.domComplete < t.loadEventEnd;
        var mono = p.now() < p.now();
        var drift = Math.abs((p.timeOrigin + p.now()) - Date.now()) < 5000;
        return [originOk, navEq, ordered, mono, drift].join(',');
    })()"#;
    assert_eq!(
        eval_str(engine.as_mut(), realm, probe),
        "true,true,true,true,true",
    );
}

#[test]
fn wall_clock_is_deterministic_not_process_time() {
    // Date.now()/new Date() must read a fixed base epoch (advanced by a
    // deterministic monotonic tick), not process wall-clock. Two fresh realms
    // therefore see the same clock — the prerequisite for a script-driven page to
    // render identically across loads. Explicit dates (Date.parse) still work.
    let probe = r#"(function(){
        var a = Date.now();
        var b = Date.now();
        var c = new Date().getTime();
        // Fixed plausible base epoch (2025-ish), strictly monotonic per call.
        var based = a >= 1751000000000 && a < 1751000001000;
        var mono = b > a && c > b;
        // Explicit dates are untouched.
        var parsed = Date.parse("2020-01-01T00:00:00Z") === 1577836800000;
        return [based, mono, parsed].join(',');
    })()"#;

    let (mut e1, r1) = installed();
    assert_eq!(
        eval_str(e1.as_mut(), r1, probe),
        "true,true,true",
        "clock is a deterministic monotonic epoch",
    );

    // Two independent, freshly-installed realms read the identical value on their
    // first Date.now() call — no process-entropy drift between two loads of the
    // same page (the prerequisite for reproducible screenshots).
    let (mut e2, r2) = installed();
    let (mut e3, r3) = installed();
    assert_eq!(
        eval_str(e2.as_mut(), r2, "String(Date.now())"),
        eval_str(e3.as_mut(), r3, "String(Date.now())"),
        "two fresh page loads see the same clock value",
    );
}

#[test]
fn text_encoder_substitutes_u_fffd_for_unpaired_surrogates() {
    // Well-formed text (incl. 3-byte BMP and 4-byte astral pairs) is byte-exact;
    // lone/unpaired surrogates become EF BF BD (U+FFFD), matching real Chrome
    // rather than the old raw 3-byte WTF-8 surrogate value.
    let (mut engine, realm) = installed();
    let probe = r#"(function(){
        function b(s){ return Array.prototype.join.call(new TextEncoder().encode(s), ','); }
        return [
            b('A'),             // 65
            b('é'),        // C3 A9  (2-byte)
            b('€'),        // E2 82 AC  (3-byte BMP, must NOT become U+FFFD)
            b('🔏'),  // F0 9F 94 8F  (astral pair, still valid)
            b('\ud83d'),        // EF BF BD  (lone high surrogate)
            b('\udc00'),        // EF BF BD  (lone low surrogate)
            b('a\ud83db')       // 97 EF BF BD 98  (high surrogate, no low follows)
        ].join('/');
    })()"#;
    assert_eq!(
        eval_str(engine.as_mut(), realm, probe),
        "65/195,169/226,130,172/240,159,148,143/239,191,189/239,191,189/97,239,191,189,98",
    );
}

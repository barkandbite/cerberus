//! The WebGL GPU identity (VENDOR/RENDERER + UNMASKED vendor/renderer) must come
//! from the coherent per-window profile (`globalThis.__CERBERUS_PROFILE__.gpu`)
//! when one is injected, read lazily at `getParameter` call time so prologue
//! ordering does not matter — and must fall back to the fixed Intel/ANGLE
//! persona when no profile is present (backward compatibility). Driven through a
//! real QuickJS realm, the same way the composition root wires a page.

use cerberus_farbling::{FarblingProvider, SeededFarbling};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

/// An engine with the given head seed's farbling prologue installed (prologue
/// first, exactly as the composition root does).
fn engine_for_seed(seed: u64) -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    engine
        .inject_prologue(realm, &SeededFarbling::new(seed).js_prologue())
        .expect("farbling prologue");
    (engine, realm)
}

fn eval_str(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> String {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    }
}

/// An Apple/Metal GPU profile (only the `.gpu` sub-object is needed here).
const APPLE_PROFILE: &str = "globalThis.__CERBERUS_PROFILE__ = {gpu:{vendor:'WebKit',renderer:'WebKit WebGL',unmaskedVendor:'Google Inc. (Apple)',unmaskedRenderer:'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'}};";

/// Reads all four GPU identity slots and joins them: VENDOR|RENDERER|UNMASKED_VENDOR|UNMASKED_RENDERER.
const WEBGL_ID: &str = r#"(function(){
    var gl = __cerberusFarble.attachCanvas({}).getContext('webgl');
    return gl.getParameter(0x1F00) + '|' + gl.getParameter(0x1F01)
         + '|' + gl.getParameter(0x9245) + '|' + gl.getParameter(0x9246);
})()"#;

/// Reads only UNMASKED_RENDERER_WEBGL (0x9246) — the slot the "GPU vs OS" tell
/// keys off of.
const UNMASKED_RENDERER: &str = r#"(function(){
    var gl = __cerberusFarble.attachCanvas({}).getContext('webgl');
    return gl.getParameter(0x9246);
})()"#;

fn eval_num(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> f64 {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn farble_seed_is_exported_on_global_and_is_per_head() {
    // Defect #1: the DOM prelude's crypto shim reseeds from globalThis.__FARBLE_*
    // (guard: `typeof g.__FARBLE_HI === "number"`). The prologue must export the
    // per-head seed onto the global object, not leave it IIFE-local.
    let (mut a, ra) = engine_for_seed(0xAAAA_BBBB_CCCC_DDDD);
    let (mut b, rb) = engine_for_seed(0x1111_2222_3333_4444);

    // The guard the DOM prelude uses must hold: the globals are numbers.
    assert_eq!(
        eval_str(a.as_mut(), ra, "typeof globalThis.__FARBLE_HI"),
        "number"
    );
    assert_eq!(
        eval_str(a.as_mut(), ra, "typeof globalThis.__FARBLE_LO"),
        "number"
    );

    // They carry this head's low/high 32 bits (safe-integer range).
    let a_hi = eval_num(a.as_mut(), ra, "globalThis.__FARBLE_HI");
    let a_lo = eval_num(a.as_mut(), ra, "globalThis.__FARBLE_LO");
    assert_eq!(a_hi, 0xAAAA_BBBBu32 as f64);
    assert_eq!(a_lo, 0xCCCC_DDDDu32 as f64);

    // Two different seeds export two different values (no cross-head correlation).
    let b_hi = eval_num(b.as_mut(), rb, "globalThis.__FARBLE_HI");
    let b_lo = eval_num(b.as_mut(), rb, "globalThis.__FARBLE_LO");
    assert_ne!(a_hi, b_hi);
    assert_ne!(a_lo, b_lo);
}

#[test]
fn webgl_identity_follows_injected_profile_gpu() {
    let (mut e, r) = engine_for_seed(0x1234);
    // Inject the profile AFTER the farbling prologue but BEFORE the probe — the
    // shim must pick it up lazily at getParameter time, not at prologue eval.
    e.eval(r, APPLE_PROFILE).expect("inject profile");

    // UNMASKED_RENDERER (0x9246) is the profile's Metal renderer, not the Intel
    // fallback.
    assert_eq!(
        eval_str(e.as_mut(), r, UNMASKED_RENDERER),
        "ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)"
    );

    // All four GPU slots track the profile.
    assert_eq!(
        eval_str(e.as_mut(), r, WEBGL_ID),
        "WebKit|WebKit WebGL|Google Inc. (Apple)|ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)"
    );
}

#[test]
fn webgl_identity_falls_back_to_intel_when_no_profile() {
    let (mut e, r) = engine_for_seed(0x1234);
    // No __CERBERUS_PROFILE__ injected: the fixed Intel/ANGLE persona stands,
    // proving backward compatibility with the existing farbling.rs assertions.
    assert_eq!(
        eval_str(e.as_mut(), r, UNMASKED_RENDERER),
        "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
    );
    assert_eq!(
        eval_str(e.as_mut(), r, WEBGL_ID),
        "WebKit|WebKit WebGL|Google Inc. (Intel)|ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
    );
}

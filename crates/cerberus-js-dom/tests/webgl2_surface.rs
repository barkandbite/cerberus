//! WebGL2-path coherence tests. A `getContext('webgl2')` context must present a
//! real Chrome/ANGLE WebGL2 surface: the extension list omits now-core WebGL1
//! extensions, core WebGL2 getParameter limits return realistic nonzero values,
//! and the core WebGL2 method set exists. The WebGL1 path must stay unchanged.

use cerberus_farbling::{FarblingProvider, SeededFarbling};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

fn engine_for_seed(seed: u64) -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    engine
        .inject_prologue(realm, &SeededFarbling::new(seed).js_prologue())
        .expect("farbling prologue");
    (engine, realm)
}

fn eval_bool(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> bool {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Bool(b) => b,
        other => panic!("expected bool, got {other:?}"),
    }
}

fn eval_num(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> f64 {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn webgl2_extension_list_omits_now_core_webgl1_extensions() {
    let (mut e, r) = engine_for_seed(0x1234);
    let probe = r#"(function(){
        var gl = __cerberusFarble.attachCanvas({}).getContext('webgl2');
        var x = gl.getSupportedExtensions();
        return (x.indexOf('OES_vertex_array_object')<0)
            && (x.indexOf('ANGLE_instanced_arrays')<0)
            && (x.indexOf('EXT_color_buffer_float')>=0)
            && (x.indexOf('WEBGL_debug_renderer_info')>=0);
    })()"#;
    assert!(
        eval_bool(e.as_mut(), r, probe),
        "webgl2 getSupportedExtensions must omit core-in-WebGL2 exts and keep the WebGL2 set"
    );
}

#[test]
fn webgl2_getparameter_returns_core_webgl2_limits() {
    let (mut e, r) = engine_for_seed(0x1234);
    let gl = |src: &str| format!("__cerberusFarble.attachCanvas({{}}).getContext('webgl2').{src}");
    assert_eq!(
        eval_num(e.as_mut(), r, &gl("getParameter(0x8824)")),
        8.0,
        "MAX_DRAW_BUFFERS"
    );
    assert_eq!(
        eval_num(e.as_mut(), r, &gl("getParameter(0x8D57)")),
        8.0,
        "MAX_SAMPLES"
    );
}

#[test]
fn webgl2_has_create_vertex_array_method() {
    let (mut e, r) = engine_for_seed(0x1234);
    let probe = r#"(function(){
        var gl = __cerberusFarble.attachCanvas({}).getContext('webgl2');
        if(typeof gl.createVertexArray !== 'function')return false;
        gl.createVertexArray();
        return true;
    })()"#;
    assert!(
        eval_bool(e.as_mut(), r, probe),
        "webgl2 createVertexArray must be a callable function"
    );
}

#[test]
fn webgl1_surface_is_unchanged() {
    let (mut e, r) = engine_for_seed(0x1234);
    let probe = r#"(function(){
        var gl = __cerberusFarble.attachCanvas({}).getContext('webgl');
        return (gl.getSupportedExtensions().indexOf('OES_vertex_array_object')>=0)
            && (typeof gl.createVertexArray === 'undefined');
    })()"#;
    assert!(
        eval_bool(e.as_mut(), r, probe),
        "webgl1 must still list OES_vertex_array_object and lack createVertexArray"
    );
}

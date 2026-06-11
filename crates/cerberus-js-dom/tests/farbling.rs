//! M6 farbling tests, driven through a real QuickJS realm: the per-head shims
//! (canvas readbacks, WebGL, audio, font metrics) must be deterministic for a
//! single head, uncorrelated across heads, and shaped plausibly enough that a
//! page consuming them keeps working.

use cerberus_dom::{parse_html, NodeRef};
use cerberus_farbling::{FarblingProvider, SeededFarbling};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{run_page_scripts, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

/// An engine with the given head seed's farbling prologue installed (the same
/// order the composition root uses: prologue first, DOM model later).
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

fn eval_num(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> f64 {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

/// The canonical fingerprinting dance: draw text + shapes, read back a hash
/// surface (the data URL). Uses a plain object so the test exercises the shim
/// without the DOM model.
const CANVAS_DANCE: &str = r#"
    (function(){
        var c = __cerberusFarble.attachCanvas({width:280, height:60});
        var ctx = c.getContext('2d');
        ctx.fillStyle = '#f60';
        ctx.fillRect(0, 0, 100, 20);
        ctx.font = '14px sans-serif';
        ctx.fillText('Cwm fjordbank glyphs vext quiz \u{1F50F}', 2, 15);
        ctx.strokeStyle = '#069';
        ctx.beginPath(); ctx.arc(50, 30, 20, 0, 6.28); ctx.stroke();
        return c.toDataURL();
    })()
"#;

#[test]
fn canvas_readback_is_deterministic_per_head_and_diverges_across_heads() {
    let (mut a1, r1) = engine_for_seed(0x1111);
    let (mut a2, r2) = engine_for_seed(0x1111);
    let (mut b, r3) = engine_for_seed(0x2222);

    let url_a1 = eval_str(a1.as_mut(), r1, CANVAS_DANCE);
    let url_a2 = eval_str(a2.as_mut(), r2, CANVAS_DANCE);
    let url_b = eval_str(b.as_mut(), r3, CANVAS_DANCE);

    // Same head, fresh engine: byte-identical (a tracker sees one stable id
    // per head)...
    assert_eq!(url_a1, url_a2);
    // ...but two heads never correlate.
    assert_ne!(url_a1, url_b);
}

#[test]
fn canvas_data_url_is_a_real_png() {
    let (mut e, r) = engine_for_seed(7);
    let url = eval_str(e.as_mut(), r, CANVAS_DANCE);
    // PNG signature, base64-encoded, after the data-URL header.
    assert!(
        url.starts_with("data:image/png;base64,iVBORw0KGgo"),
        "not a PNG data url: {}",
        &url[..url.len().min(48)]
    );
    assert!(url.len() > 100, "implausibly small payload");
}

#[test]
fn different_draw_ops_produce_different_readbacks() {
    let (mut e, r) = engine_for_seed(7);
    let url1 = eval_str(e.as_mut(), r, CANVAS_DANCE);
    let url2 = eval_str(
        e.as_mut(),
        r,
        r#"(function(){
            var c = __cerberusFarble.attachCanvas({width:280, height:60});
            var ctx = c.getContext('2d');
            ctx.fillText('completely different content', 0, 10);
            return c.toDataURL();
        })()"#,
    );
    assert_ne!(url1, url2, "op log must feed the readback");
}

#[test]
fn webgl_identity_is_uniform_but_readpixels_noise_is_per_head() {
    let gl_id = r#"(function(){
        var gl = __cerberusFarble.attachCanvas({}).getContext('webgl');
        return gl.getParameter(0x1F00) + '|' + gl.getParameter(0x9246);
    })()"#;
    let read_pixels = r#"(function(){
        var gl = __cerberusFarble.attachCanvas({}).getContext('webgl');
        var out = new Uint8Array(16);
        gl.readPixels(0, 0, 2, 2, 6408, 5121, out);
        return Array.prototype.join.call(out, ',');
    })()"#;

    let (mut a, ra) = engine_for_seed(0xAAAA);
    let (mut b, rb) = engine_for_seed(0xBBBB);

    // Vendor/renderer strings: uniform for every head (no entropy added).
    let id_a = eval_str(a.as_mut(), ra, gl_id);
    let id_b = eval_str(b.as_mut(), rb, gl_id);
    assert_eq!(id_a, id_b);
    assert_eq!(id_a, "Cerberus|Cerberus Software Renderer");

    // The pixel surface (the real entropy channel): per-head noise.
    let px_a = eval_str(a.as_mut(), ra, read_pixels);
    let px_b = eval_str(b.as_mut(), rb, read_pixels);
    assert_ne!(px_a, px_b);

    // And deterministic within one head.
    let px_a2 = eval_str(a.as_mut(), ra, read_pixels);
    assert_eq!(px_a, px_a2);
}

#[test]
fn audio_readbacks_are_seeded_near_silence() {
    let analyser = r#"(function(){
        var ctx = new AudioContext();
        var an = ctx.createAnalyser();
        var arr = new Float32Array(8);
        an.getFloatFrequencyData(arr);
        return Array.prototype.join.call(arr, ',');
    })()"#;

    let (mut a, ra) = engine_for_seed(1);
    let (mut b, rb) = engine_for_seed(2);
    let fa = eval_str(a.as_mut(), ra, analyser);
    let fb = eval_str(b.as_mut(), rb, analyser);
    assert_ne!(fa, fb, "two heads must not share an audio fingerprint");
    // Values stay near the -100 dB floor (plausible silence).
    for v in fa.split(',') {
        let n: f64 = v.parse().expect("float");
        assert!((-101.0..=-97.0).contains(&n), "implausible dB value {n}");
    }

    // The offline-render path resolves with a deterministic buffer. The
    // promise drains in the engine's post-eval job pump, so trigger first,
    // read the captured result in a second eval.
    let trigger = r#"
        globalThis.__out = '';
        new OfflineAudioContext(1, 64, 44100).startRendering().then(function(buf){
            var d = buf.getChannelData(0);
            globalThis.__out = d[0] + ',' + d[1] + ',' + d[63];
        });
        void 0
    "#;
    a.eval(ra, trigger).expect("trigger");
    let oa = eval_str(a.as_mut(), ra, "globalThis.__out");
    a.eval(ra, trigger).expect("trigger again");
    let oa2 = eval_str(a.as_mut(), ra, "globalThis.__out");
    assert_eq!(oa, oa2);
    assert!(!oa.is_empty(), "offline render promise never resolved");
}

#[test]
fn measure_text_jitter_is_bounded() {
    let (mut a, ra) = engine_for_seed(0x1234);
    let (mut b, rb) = engine_for_seed(0x9876);
    let probe = "__cerberusFarble.measureText('Hello world', '16px sans-serif').width";

    let wa = eval_num(a.as_mut(), ra, probe);
    let wb = eval_num(b.as_mut(), rb, probe);

    // Base width for 'Hello world' at 16px: 10 glyphs * 0.6 + 1 space * 0.33.
    let base = (10.0 * 0.6 + 0.33) * 16.0;
    for w in [wa, wb] {
        assert!(w >= base, "width below base: {w} < {base}");
        assert!(w <= base * 1.02, "jitter exceeds 2%: {w} vs {base}");
    }
    // Per-head divergence (it is a farbled surface).
    assert_ne!(wa, wb);
    // Determinism within a head.
    assert_eq!(wa, eval_num(a.as_mut(), ra, probe));
}

#[test]
fn page_scripts_reach_the_farbled_canvas_through_the_dom_model() {
    // Full-path test: prologue + DOM model + a parsed <canvas>, the way the
    // composition root wires a real page.
    let (mut engine, realm) = engine_for_seed(0x5EED);
    let document = parse_html(
        "<html><body><canvas id=c width=80 height=20></canvas><div id=out></div></body></html>",
    );
    let script = r#"
        var c = document.getElementById('c');
        var ctx = c.getContext('2d');
        ctx.fillText('fingerprint me', 1, 10);
        document.getElementById('out').textContent = c.toDataURL();
    "#;
    let env = PageEnv {
        url: "https://example.test/".into(),
        viewport: (800, 600),
        user_agent: "Cerberus/0.0".into(),
    };
    let rebuilt = run_page_scripts(
        engine.as_mut(),
        realm,
        &document,
        &[script.to_string()],
        &env,
    )
    .expect("bridge");

    fn find_id<'a>(node: NodeRef<'a>, id: &str) -> Option<NodeRef<'a>> {
        if node.attr("id") == Some(id) {
            return Some(node);
        }
        node.children().find_map(|c| find_id(c, id))
    }
    let out = find_id(rebuilt.root(), "out").expect("out div");
    let text = out.text_content();
    assert!(
        text.starts_with("data:image/png;base64,iVBORw0KGgo"),
        "canvas readback did not flow through the DOM model: {}",
        &text[..text.len().min(48)]
    );
}

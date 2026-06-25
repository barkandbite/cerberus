//! Phase 0 (epic GH #40, issue #41) — **offline web-platform fixtures**.
//!
//! These are the *authoritative correctness signal* for the web-platform epic:
//! static pages that drive the real QuickJS realm + DOM bridge through
//! [`run_page_scripts_with_fetch`] against a **stub network** (no external
//! traffic), then assert the reconciled Rust DOM. They run in CI from a
//! datacenter/cloud build environment with no reachability assumptions — the live
//! `pokemoncenter.com` check (Imperva reese84) is a separate residential
//! confirmation, because WAFs challenge datacenter IPs by reputation (ADR-0062),
//! which is not an engine defect.
//!
//! The capstone fixture (`reese84_shaped_challenge_flow_runs_end_to_end`) mirrors
//! the *structure* of the Imperva interstitial — an inline "initializeProtection"
//! that dynamically injects an external sensor script, which reads Web APIs,
//! POSTs a token, and reveals the page on success — so the challenge machinery is
//! exercised end to end here, offline.

use cerberus_dom::{Document, DocumentBuilder, NodeRef};
use cerberus_js::{JsEngine, JsEngineFactory};
use cerberus_js_dom::{
    install_page, run_page_scripts_with_fetch, run_scripts, take_cookie_writes, FetchClient,
    FetchRequest, FetchResponse, PageEnv,
};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;
use std::collections::HashMap;

// --- harness ----------------------------------------------------------------

/// A fresh QuickJS engine with one realm created, plus that realm's id.
fn engine_and_realm() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    (engine, realm)
}

/// A desktop-ish ambient environment with a concrete, coherent UA so the
/// navigator/screen fixtures can assert the values the script actually reads.
fn env() -> PageEnv {
    PageEnv {
        url: "https://fixture.test/page".into(),
        viewport: (1280, 800),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36"
            .into(),
        cookies: String::new(),
    }
}

/// The shared `env()` with a specific `document.cookie` seed (the non-HttpOnly
/// cookie string the host would compute from the jar).
fn env_with_cookies(cookies: &str) -> PageEnv {
    PageEnv {
        cookies: cookies.into(),
        ..env()
    }
}

/// DFS for the first element whose `id` matches.
fn find_id<'a>(node: NodeRef<'a>, id: &str) -> Option<NodeRef<'a>> {
    if node.is_element() && node.attr("id") == Some(id) {
        return Some(node);
    }
    node.children().find_map(|c| find_id(c, id))
}

/// `<html><head></head><body><div id="result">init</div></body></html>` — the
/// canonical fixture shell: scripts write their answer into `#result`.
fn shell_with(id: &str, initial: &str) -> Document {
    let mut b = DocumentBuilder::new();
    let txt = b.text(initial);
    let div = b.element_attrs("div", vec![("id".into(), id.into())], [txt]);
    let body = b.element("body", [div]);
    let head = b.element("head", []);
    let html = b.element("html", [head, body]);
    b.finish(html)
}

/// A multi-URL stub network seam: answers GET/POST by URL from a canned table and
/// records every request (so a fixture can assert the sensor POST happened).
#[derive(Default)]
struct Stub {
    responses: HashMap<String, (u16, String, String)>, // url -> (status, ctype, body)
    seen: Vec<FetchRequest>,
}

impl Stub {
    fn route(mut self, url: &str, ctype: &str, body: &str) -> Self {
        self.responses
            .insert(url.into(), (200, ctype.into(), body.into()));
        self
    }
}

impl FetchClient for Stub {
    fn fetch(&mut self, req: &FetchRequest) -> Result<FetchResponse, String> {
        self.seen.push(req.clone());
        match self.responses.get(&req.url) {
            Some((status, ctype, body)) => Ok(FetchResponse {
                status: *status,
                status_text: "OK".into(),
                url: req.url.clone(),
                headers: vec![("content-type".into(), ctype.clone())],
                body: body.clone(),
            }),
            None => Ok(FetchResponse {
                status: 404,
                status_text: "Not Found".into(),
                url: req.url.clone(),
                headers: Vec::new(),
                body: String::new(),
            }),
        }
    }
}

fn run(doc: &Document, scripts: &[&str], client: &mut Stub) -> Document {
    run_env(doc, scripts, env(), client)
}

fn run_env(doc: &Document, scripts: &[&str], env: PageEnv, client: &mut Stub) -> Document {
    let owned: Vec<String> = scripts.iter().map(|s| s.to_string()).collect();
    let (mut engine, realm) = engine_and_realm();
    run_page_scripts_with_fetch(engine.as_mut(), realm, doc, &owned, &env, client)
        .expect("run page scripts")
}

// --- fixtures ---------------------------------------------------------------

#[test]
fn dom_mutation_reflects_into_the_rebuilt_tree() {
    // createElement + appendChild + textContent must round-trip through the
    // serialize/rebuild bridge into the Rust DOM.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var p = document.createElement('p'); p.id = 'made'; \
           p.textContent = 'hello'; document.body.appendChild(p); \
           document.getElementById('result').textContent = 'mutated';"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "mutated"
    );
    let made = find_id(out.root(), "made").expect("script-created <p> is in the tree");
    assert_eq!(made.text_content(), "hello");
}

#[test]
fn navigator_and_screen_report_the_ambient_environment() {
    // Web-API reads must reflect the real PageEnv (coherent UA + viewport), not
    // placeholders. This is the offline analogue of what a sensor probes first.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var ok = navigator.userAgent.indexOf('Chrome/142') >= 0 \
             && screen.width === 1280 && navigator.webdriver === false \
             && typeof navigator.platform === 'string'; \
           document.getElementById('result').textContent = ok ? 'env-ok' : 'env-bad';"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "env-ok",
        "navigator/screen must report the ambient environment coherently"
    );
}

#[test]
fn microtasks_run_before_the_next_macrotask() {
    // Ordering oracle: sync → microtask (Promise/queueMicrotask) → macrotask
    // (setTimeout). The speed-first virtual clock still preserves this order.
    let doc = shell_with("result", "");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var log = ''; \
           setTimeout(function(){ log += 'T'; \
             document.getElementById('result').textContent = log; }, 0); \
           Promise.resolve().then(function(){ log += 'M'; }); \
           log += 'S';"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "SMT",
        "sync, then microtask, then timer macrotask"
    );
}

#[test]
fn dataset_view_and_structured_clone() {
    // el.dataset (data-* as camelCase, was absent) and structuredClone (deep,
    // cycle-safe clone, was absent) — both used constantly by app frameworks.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var d = document.getElementById('result'); \
           d.setAttribute('data-foo-bar', 'x'); var read = d.dataset.fooBar; \
           d.dataset.bazQux = 'y'; var attr = d.getAttribute('data-baz-qux'); \
           var keys = Object.keys(d.dataset).sort().join(','); \
           var orig = { a: 1, b: [1, 2, { c: 3 }], d: new Date(0) }; \
           var cl = structuredClone(orig); cl.b[2].c = 99; \
           var clone_ok = orig.b[2].c === 3 && cl.b[2].c === 99 && cl.d.getTime() === 0; \
           d.textContent = read + '|' + attr + '|' + keys + '|' + clone_ok;"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "x|y|bazQux,fooBar|true",
        "dataset camelCase<->data-* both ways + Object.keys; structuredClone deep + independent"
    );
}

#[test]
fn abort_controller_formdata_and_blob() {
    // All three were absent. Cover: AbortController fires its abort listener and
    // sets signal.aborted; FormData append/getAll/set/get; Blob size/type.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var ac = new AbortController(); var fired = ''; \
           ac.signal.addEventListener('abort', function () { fired = 'abort:' + ac.signal.aborted; }); \
           ac.abort(); \
           var fd = new FormData(); fd.append('a', '1'); fd.append('a', '2'); fd.set('b', '3'); \
           var fdstr = fd.getAll('a').join(',') + ';' + fd.get('b'); \
           var blob = new Blob(['hello', ' ', 'world'], { type: 'text/plain' }); \
           document.getElementById('result').textContent = \
             fired + '|' + fdstr + '|' + blob.size + ':' + blob.type;"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "abort:true|1,2;3|11:text/plain"
    );

    // fetch integration: a FormData body serializes URL-encoded; an already-aborted
    // signal rejects before hitting the network.
    let mut c2 = Stub::default().route("/p", "text/plain", "ok");
    let out2 = run(
        &doc,
        &[
            "var fd = new FormData(); fd.append('x', '1'); fd.append('y', 'two'); \
           fetch('/p', { method: 'POST', body: fd }); \
           var ac = new AbortController(); ac.abort(); \
           fetch('/blocked', { signal: ac.signal }).catch(function () { \
             document.getElementById('result').textContent = 'rejected'; });",
        ],
        &mut c2,
    );
    assert_eq!(
        find_id(out2.root(), "result").unwrap().text_content(),
        "rejected"
    );
    let post = c2
        .seen
        .iter()
        .find(|r| r.url == "/p")
        .expect("FormData POST serviced");
    assert_eq!(post.body, "x=1&y=two", "FormData serialized URL-encoded");
    assert!(
        !c2.seen.iter().any(|r| r.url == "/blocked"),
        "an aborted-signal fetch never reaches the network"
    );
}

#[test]
fn history_pushstate_updates_location_and_fires_popstate() {
    // SPA routing: pushState updates location + history.state/length; back()
    // restores the prior entry and fires popstate (was a no-op stub).
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var log = []; \
           window.addEventListener('popstate', function (e) { \
             log.push('pop:' + location.pathname + ':' + (e.state && e.state.n)); }); \
           history.pushState({ n: 1 }, '', '/a'); \
           var afterPush = location.pathname + ':' + history.state.n + ':' + history.length; \
           history.pushState({ n: 2 }, '', '/b?x=1'); \
           history.back(); \
           var afterBack = location.pathname + ':' + (history.state && history.state.n); \
           document.getElementById('result').textContent = afterPush + '|' + afterBack + '|' + log.join(',');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "/a:1:2|/a:1|pop:/a:1",
        "pushState updates location + state + length; back() restores + fires popstate"
    );
}

#[test]
fn query_selector_attribute_ops_pseudos_and_sibling_combinators() {
    // Extend the JS selector engine beyond compound+descendant/child: attribute
    // operators (^= $= *=), :nth-child/:not/:first-child, and sibling combinators
    // (+ ~) — all pervasive in querySelector.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var ul = document.createElement('ul'); \
           function li(cls, dx, txt) { var e = document.createElement('li'); \
             if (cls) e.className = cls; if (dx) e.setAttribute('data-x', dx); \
             e.textContent = txt; ul.appendChild(e); } \
           li('a', 'foobar', '1'); li('b', 'bazfoo', '2'); li('', 'qux', '3'); \
           document.body.appendChild(ul); var r = []; \
           r.push(ul.querySelectorAll('li[data-x^=\"foo\"]').length); \
           r.push(ul.querySelectorAll('li[data-x$=\"foo\"]').length); \
           r.push(ul.querySelectorAll('li[data-x*=\"z\"]').length); \
           r.push(ul.querySelectorAll('li:nth-child(2)')[0].textContent); \
           r.push(ul.querySelectorAll('li:not(.a)').length); \
           r.push(ul.querySelectorAll('li:first-child')[0].textContent); \
           r.push(ul.querySelectorAll('.a + li')[0].textContent); \
           r.push(ul.querySelectorAll('.a ~ li').length); \
           document.getElementById('result').textContent = r.join('|');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "1|1|1|2|2|1|2|2",
        "attr ^=/$=/*=; :nth-child(2); :not(.a); :first-child; + and ~ siblings"
    );
}

#[test]
fn event_dispatch_runs_capture_target_then_bubble() {
    // Capture-phase listeners (3rd arg true) were ignored. Full propagation must
    // be: capture (root→target's parent), target (capture then bubble listeners),
    // bubble (target's parent→root).
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var outer = document.createElement('div'); var inner = document.createElement('span'); \
           outer.appendChild(inner); document.body.appendChild(outer); var log = []; \
           outer.addEventListener('click', function () { log.push('outer-cap'); }, true); \
           outer.addEventListener('click', function () { log.push('outer-bub'); }, false); \
           inner.addEventListener('click', function () { log.push('inner-cap'); }, true); \
           inner.addEventListener('click', function () { log.push('inner-bub'); }); \
           inner.dispatchEvent(new Event('click', { bubbles: true })); \
           document.getElementById('result').textContent = log.join(',');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "outer-cap,inner-cap,inner-bub,outer-bub",
        "capture (outer) -> target (inner cap then bub) -> bubble (outer)"
    );
}

#[test]
fn clone_replacechild_and_modern_manipulation() {
    // cloneNode/replaceChild were missing (Phase 1 gap); append/prepend/after are
    // modern manipulation. Build a list, clone (deep, with attrs), replaceChild,
    // and after(); assert final text + structure.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var box = document.createElement('div'); document.body.appendChild(box); \
           var a = document.createElement('p'); a.textContent = 'A'; a.setAttribute('data-k', 'v'); \
           box.append(a, 'tail'); box.prepend('head'); \
           var clone = a.cloneNode(true); clone.id = 'clone'; box.append(clone); \
           var b = document.createElement('p'); b.textContent = 'B'; box.replaceChild(b, a); \
           var c = document.createElement('i'); c.textContent = 'C'; b.after(c); \
           document.getElementById('result').textContent = box.textContent + '|' \
             + clone.getAttribute('data-k') + '|' + clone.textContent + '|' + box.children.length;"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "headBCtailA|v|A|3",
        "append/prepend/after order; deep cloneNode copies attrs+text; replaceChild swaps"
    );
}

#[test]
fn dom_completeness_helpers() {
    // toggleAttribute, el.hidden, insertAdjacentElement, getElementsByName,
    // createElementNS, scrollIntoView/getClientRects — all were missing.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var d = document.getElementById('result'); \
           d.toggleAttribute('data-on'); var t1 = d.hasAttribute('data-on'); \
           d.toggleAttribute('data-on'); var t2 = d.hasAttribute('data-on'); \
           d.toggleAttribute('aria-x', true); var t3 = d.getAttribute('aria-x') === ''; \
           d.hidden = true; var h1 = d.getAttribute('hidden') === ''; \
           d.hidden = false; var h2 = !d.hasAttribute('hidden'); \
           var span = document.createElement('span'); span.id = 'adj'; \
           d.insertAdjacentElement('afterbegin', span); var adj = d.children[0].id === 'adj'; \
           var inp = document.createElement('input'); inp.setAttribute('name', 'q'); document.body.appendChild(inp); \
           var byName = document.getElementsByName('q').length; \
           var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg'); \
           var svgOk = svg.tagName.toLowerCase() === 'svg'; \
           d.scrollIntoView(); var rects = d.getClientRects().length; \
           document.getElementById('result').textContent = \
             [t1, t2, t3, h1, h2, adj, byName, svgOk, rects].join(',');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "true,false,true,true,true,true,1,true,1"
    );
}

#[test]
fn real_canvas_2d_fills_and_reads_back_actual_pixels() {
    // With the real-canvas mode opted in (farble stays the default), getContext
    // ('2d') backs a true pixel buffer: fillRect writes real colors, getImageData
    // reads them back (not noise), clearRect zeroes alpha.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["globalThis.__cerberusCanvasReal = true; \
           var cv = document.createElement('canvas'); cv.setAttribute('width', '20'); cv.setAttribute('height', '20'); \
           var ctx = cv.getContext('2d'); \
           ctx.fillStyle = '#ff0000'; ctx.fillRect(2, 2, 5, 5); \
           ctx.fillStyle = 'rgba(0,255,0,1)'; ctx.fillRect(10, 10, 3, 3); \
           var red = ctx.getImageData(3, 3, 1, 1).data; \
           var green = ctx.getImageData(11, 11, 1, 1).data; \
           var blank = ctx.getImageData(0, 0, 1, 1).data; \
           ctx.clearRect(2, 2, 5, 5); var cleared = ctx.getImageData(3, 3, 1, 1).data; \
           document.getElementById('result').textContent = \
             [red[0], red[1], red[2], red[3], green[1], blank[3], cleared[3]].join(',');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "255,0,0,255,255,0,0",
        "fillRect writes real RGBA; getImageData reads it; clearRect zeroes alpha"
    );
}

#[test]
fn real_canvas_todataurl_emits_a_valid_png() {
    // toDataURL on the real canvas produces an actual PNG data URL (correct
    // prefix + PNG signature bytes after base64-decoding), not a noise stub.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["globalThis.__cerberusCanvasReal = true; \
           var cv = document.createElement('canvas'); cv.setAttribute('width', '4'); cv.setAttribute('height', '4'); \
           var ctx = cv.getContext('2d'); ctx.fillStyle = '#0000ff'; ctx.fillRect(0, 0, 4, 4); \
           var url = cv.toDataURL(); var bin = atob(url.slice(22)); \
           var sig = [bin.charCodeAt(0), bin.charCodeAt(1), bin.charCodeAt(2), bin.charCodeAt(3)].join(','); \
           document.getElementById('result').textContent = url.slice(0, 22) + '|' + sig + '|' + (bin.length > 30);"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "data:image/png;base64,|137,80,78,71|true",
        "toDataURL emits a real PNG (signature 137,80,78,71)"
    );
}

#[test]
fn real_canvas_path_fill() {
    // beginPath/moveTo/lineTo/closePath/fill do a real scanline polygon fill (was
    // a no-op). Fill the whole canvas green, then a triangle red; check inside vs
    // outside the triangle.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["globalThis.__cerberusCanvasReal = true; \
           var cv = document.createElement('canvas'); cv.setAttribute('width', '10'); cv.setAttribute('height', '10'); \
           var ctx = cv.getContext('2d'); ctx.fillStyle = '#00ff00'; \
           ctx.beginPath(); ctx.moveTo(0,0); ctx.lineTo(10,0); ctx.lineTo(10,10); ctx.lineTo(0,10); ctx.closePath(); ctx.fill(); \
           var center = ctx.getImageData(5,5,1,1).data; \
           ctx.fillStyle = '#ff0000'; ctx.beginPath(); ctx.moveTo(0,0); ctx.lineTo(8,0); ctx.lineTo(0,8); ctx.closePath(); ctx.fill(); \
           var inTri = ctx.getImageData(1,1,1,1).data; var outTri = ctx.getImageData(7,7,1,1).data; \
           document.getElementById('result').textContent = [center[1], inTri[0], outTri[1]].join(',');"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "255,255,255",
        "rect fill (green) then triangle fill (red): inside=red, outside stays green"
    );
}

#[test]
fn performance_now_and_crypto_get_random_values_exist() {
    // The real sensor calls performance.now() and crypto.getRandomValues() early;
    // when they were missing it threw before doing anything. Assert the contract:
    // performance.now() is a monotonic number with a numeric timeOrigin, and
    // getRandomValues fills and returns the same TypedArray.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var t0 = performance.now(); \
           var a = new Uint8Array(16); var r = crypto.getRandomValues(a); \
           var t1 = performance.now(); \
           var ok = typeof t0 === 'number' && typeof t1 === 'number' && t1 >= t0 \
              && typeof performance.timeOrigin === 'number' \
              && r === a && a.length === 16 \
              && typeof crypto.randomUUID() === 'string' && crypto.randomUUID().length === 36; \
           document.getElementById('result').textContent = ok ? 'crypto-perf-ok' : 'bad';"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "crypto-perf-ok",
        "performance.now() and crypto.getRandomValues()/randomUUID() must be present and correct"
    );
}

#[test]
fn crypto_subtle_digest_sha256_matches_the_canonical_vector() {
    // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    // (NIST FIPS 180-4). Proves the digest is a real, correct hash — not a stub —
    // by checking against the canonical test vector. digest() returns a Promise,
    // so the .then runs on the microtask pump.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &[
            "crypto.subtle.digest('SHA-256', new Uint8Array([97,98,99])).then(function (buf) { \
             var v = new Uint8Array(buf), hex = ''; \
             for (var i = 0; i < v.length; i++) { var b = v[i].toString(16); \
               if (b.length < 2) b = '0' + b; hex += b; } \
             document.getElementById('result').textContent = hex; \
           });",
        ],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "crypto.subtle.digest('SHA-256', 'abc') must equal the FIPS 180-4 vector"
    );
}

#[test]
fn document_cookie_is_seeded_from_the_env_and_merges_writes() {
    // The host seeds document.cookie with the origin's non-HttpOnly cookies
    // (PageEnv.cookies); HttpOnly cookies are excluded upstream so they never
    // reach script. A script read sees the seed; a write merges into the readable
    // view by name.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run_env(
        &doc,
        &["var seeded = document.cookie; \
           document.cookie = 'sess=xyz; Path=/; Secure'; \
           document.cookie = 'a=99'; \
           document.getElementById('result').textContent = \
             seeded + ' | ' + document.cookie;"],
        env_with_cookies("a=1; b=2"),
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "a=1; b=2 | a=99; b=2; sess=xyz",
        "read sees the seed; writes merge by name (a replaced, sess appended)"
    );
}

#[test]
fn document_cookie_writes_are_captured_for_the_host_to_apply() {
    // The OUT half of the bridge: every `document.cookie = …` (raw, with
    // attributes) is queued for the host to apply to the real consent-gated jar
    // as a Set-Cookie. take_cookie_writes drains and empties the queue.
    let (mut engine, realm) = engine_and_realm();
    let doc = shell_with("result", "init");
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &["document.cookie = 'sid=abc; Path=/; HttpOnly'; document.cookie = 'p=1';".to_string()],
    )
    .expect("run");
    let writes = take_cookie_writes(engine.as_mut(), realm).expect("take");
    assert_eq!(
        writes,
        vec!["sid=abc; Path=/; HttpOnly".to_string(), "p=1".to_string()],
        "both assignments captured verbatim, in order"
    );
    assert!(
        take_cookie_writes(engine.as_mut(), realm)
            .expect("take2")
            .is_empty(),
        "draining empties the queue"
    );
}

#[test]
fn mutation_observer_delivers_childlist_and_attribute_records() {
    // Real MutationObserver (was a no-op stub): observing body with
    // childList+attributes+subtree must deliver, as a microtask, a childList
    // record for an appended node and attribute records for both the target and a
    // subtree descendant — in mutation order.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var log = []; \
           var mo = new MutationObserver(function (recs) { \
             for (var i = 0; i < recs.length; i++) { \
               log.push(recs[i].type + ':' + (recs[i].attributeName || \
                 (recs[i].addedNodes.length ? 'add' : 'rm'))); } \
           }); \
           mo.observe(document.body, { childList: true, attributes: true, subtree: true }); \
           var p = document.createElement('p'); document.body.appendChild(p); \
           document.body.setAttribute('data-x', '1'); \
           p.setAttribute('data-y', '2'); \
           Promise.resolve().then(function () { \
             document.getElementById('result').textContent = log.join(','); });"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "childList:add,attributes:data-x,attributes:data-y",
        "childList (appended <p>) then attribute records for target + subtree node, in order"
    );
}

#[test]
fn xmlhttprequest_get_and_post_ride_the_fetch_queue() {
    // XHR (previously absent) rides the same host-drained queue as fetch. A GET
    // with responseType 'json' drives readystatechange to DONE and exposes
    // status/responseText/response; a POST captures method, body, and a header.
    let doc = shell_with("result", "init");
    let mut client = Stub::default().route("/api", "application/json", "{\"v\":7}");
    let out = run(
        &doc,
        &["var x = new XMLHttpRequest(); \
           x.open('GET', '/api'); x.responseType = 'json'; \
           x.onreadystatechange = function () { \
             if (x.readyState === 4) { \
               document.getElementById('result').textContent = \
                 x.status + ':' + x.responseText + ':' + (x.response && x.response.v); } }; \
           x.send();"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "200:{\"v\":7}:7",
        "XHR GET reaches DONE with status + responseText + parsed json response"
    );

    let mut client2 = Stub::default().route("/submit", "text/plain", "ok");
    let _ = run(
        &doc,
        &["var x = new XMLHttpRequest(); x.open('POST', '/submit'); \
           x.setRequestHeader('X-T', '1'); x.send('payload');"],
        &mut client2,
    );
    let req = client2
        .seen
        .iter()
        .find(|r| r.url == "/submit")
        .expect("the XHR POST was serviced");
    assert_eq!(req.method, "POST");
    assert_eq!(req.body, "payload");
    assert!(
        req.headers.iter().any(|(n, v)| n == "X-T" && v == "1"),
        "the request header was captured, got {:?}",
        req.headers
    );
}

#[test]
fn event_constructors_and_sendbeacon_work() {
    // new Event / new CustomEvent were absent (dispatchEvent(new Event(...)) threw);
    // navigator.sendBeacon was absent. A CustomEvent carries detail to a target
    // listener; sendBeacon enqueues a fire-and-forget POST on the fetch queue.
    let doc = shell_with("result", "init");
    let mut client = Stub::default().route("/beacon", "text/plain", "");
    let out = run(
        &doc,
        &["var got = ''; var d = document.getElementById('result'); \
           d.addEventListener('ping', function (e) { got = e.type + ':' + e.detail; }); \
           d.dispatchEvent(new Event('noop')); \
           d.dispatchEvent(new CustomEvent('ping', { detail: 42 })); \
           var beac = navigator.sendBeacon('/beacon', 'hello'); \
           d.textContent = got + '|' + (beac === true) + '|' + (new Event('e').eventPhase === 0);"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "ping:42|true|true",
        "CustomEvent detail reaches the listener; sendBeacon returns true; Event shape ok"
    );
    assert!(
        client
            .seen
            .iter()
            .any(|r| r.url == "/beacon" && r.method == "POST" && r.body == "hello"),
        "sendBeacon enqueued a POST with the body, got {:?}",
        client
            .seen
            .iter()
            .map(|r| (&r.url, &r.method))
            .collect::<Vec<_>>()
    );
}

#[test]
fn local_storage_has_real_storage_semantics() {
    // setItem + bracket-write both store; Object.keys returns the DATA keys (not
    // method names — the prior bug); bracket-read and getItem agree; removeItem
    // and length work.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["localStorage.setItem('a', '1'); localStorage.b = '2'; \
           var keys = Object.keys(localStorage).sort().join(','); \
           var via = localStorage.getItem('b') + '/' + localStorage.a; \
           localStorage.removeItem('a'); \
           document.getElementById('result').textContent = keys + '|' + via + '|' \
             + localStorage.length + '|' + (localStorage.getItem('a') === null);"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "a,b|2/1|1|true",
        "Object.keys returns data keys; bracket + getItem agree; removeItem/length work"
    );
}

#[test]
fn url_and_url_search_params_parse_and_resolve() {
    // new URL(...) and URLSearchParams were absent (used pervasively). Cover
    // absolute parse (incl. non-default port), relative resolution (../ collapse),
    // and the URLSearchParams get/getAll/append/set/toString surface.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &[
            "var u = new URL('https://ex.test:8443/p/q?x=1&y=two#frag'); \
           var rel = new URL('../z?a=1', 'https://ex.test/p/q/'); \
           var sp = new URLSearchParams('a=1&a=2&b=3'); sp.append('c', '4'); sp.set('a', '9'); \
           document.getElementById('result').textContent = [ \
             u.protocol, u.hostname, u.port, u.pathname, u.searchParams.get('x'), u.hash, \
             rel.href, sp.get('a'), sp.getAll('b').join(','), sp.toString() \
           ].join('|');",
        ],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "https:|ex.test|8443|/p/q|1|#frag|https://ex.test/p/z?a=1|9|3|a=9&b=3&c=4",
        "URL parses components + resolves ../; URLSearchParams get/getAll/set/toString"
    );
}

#[test]
fn text_encoding_and_base64_round_trip_against_known_vectors() {
    // QuickJS ships none of these (not ECMAScript). Assert spec-correct UTF-8 +
    // base64 against known vectors, incl. a non-ASCII codepoint (U+20AC '€' ->
    // E2 82 AC) and a base64 round-trip.
    let doc = shell_with("result", "init");
    let mut client = Stub::default();
    let out = run(
        &doc,
        &["var abc = new TextEncoder().encode('abc'); \
           var euro = new TextEncoder().encode('\\u20AC'); \
           var rt = new TextDecoder().decode(new TextEncoder().encode('héllo €')); \
           var ok = abc.length === 3 && abc[0] === 97 && abc[2] === 99 \
              && euro.length === 3 && euro[0] === 226 && euro[1] === 130 && euro[2] === 172 \
              && btoa('abc') === 'YWJj' && atob('YWJj') === 'abc' \
              && btoa('Man') === 'TWFu' && btoa('a') === 'YQ==' && btoa('ab') === 'YWI=' \
              && rt === 'héllo €'; \
           document.getElementById('result').textContent = ok ? 'enc-ok' : 'enc-bad';"],
        &mut client,
    );
    assert_eq!(
        find_id(out.root(), "result").unwrap().text_content(),
        "enc-ok",
        "TextEncoder/TextDecoder/btoa/atob must match known vectors"
    );
}

#[test]
fn reese84_shaped_challenge_flow_runs_end_to_end() {
    // The capstone: a reese84-*shaped* interstitial, exercised entirely offline.
    //
    //  1. The page shows BLOCKED and runs an inline "initializeProtection" that
    //     dynamically injects an external sensor <script src="/sensor.js">.
    //  2. The host fetches + evals the sensor (the dynamic-external-script path).
    //  3. The sensor reads Web APIs, computes a token, and POSTs it to /_token.
    //  4. On the server's OK it reveals the page (sets #content = REVEALED).
    //
    // This proves the *machinery* the live Imperva milestone needs — dynamic
    // script injection, Web-API reads, a POST round-trip, and a JS-driven reveal —
    // works end to end, with no network and no flagged-IP dependency.
    let doc = shell_with("content", "BLOCKED");

    // The realistic sensor pipeline: collect a fingerprint, UTF-8 encode it,
    // SHA-256 it, base64 the digest into a token, POST it, reveal on OK. Exercises
    // getRandomValues + performance.now + navigator/screen + TextEncoder +
    // subtle.digest + btoa + fetch + the Promise chain + the DOM reveal at once.
    let sensor = "(function () {\
        var nonce = new Uint8Array(8); crypto.getRandomValues(nonce);\
        var fp = [navigator.userAgent, String(screen.width), navigator.platform,\
                  String(performance.now() >= 0), String(nonce.length)].join('|');\
        crypto.subtle.digest('SHA-256', new TextEncoder().encode(fp)).then(function (h) {\
          var b = new Uint8Array(h), bin = '';\
          for (var i = 0; i < b.length; i++) { bin += String.fromCharCode(b[i]); }\
          return fetch('/_token', { method: 'POST', body: btoa(bin) });\
        }).then(function (r) { return r.json(); })\
          .then(function (d) {\
            if (d && d.ok) { document.getElementById('content').textContent = 'REVEALED'; }\
          });\
    })();";

    let mut client = Stub::default()
        .route("/sensor.js", "application/javascript", sensor)
        .route("/_token", "application/json", "{\"ok\":true}");

    let init = "var s = document.createElement('script'); s.src = '/sensor.js'; \
                document.head.appendChild(s);";

    let out = run(&doc, &[init], &mut client);

    // The full challenge → token → reveal chain executed.
    assert_eq!(
        find_id(out.root(), "content").unwrap().text_content(),
        "REVEALED",
        "the dynamically-injected sensor revealed the page after the token POST"
    );
    // The sensor script was fetched (dynamic external-script load) ...
    assert!(
        client.seen.iter().any(|r| r.url == "/sensor.js"),
        "the dynamically-injected sensor was fetched, got {:?}",
        client.seen.iter().map(|r| &r.url).collect::<Vec<_>>()
    );
    // ... and the token POST round-tripped with the computed body.
    let post = client
        .seen
        .iter()
        .find(|r| r.url == "/_token")
        .expect("the sensor POSTed the token");
    assert_eq!(post.method, "POST");
    // The token is base64 of the 32-byte SHA-256 digest: 44 chars, one '=' pad.
    assert_eq!(
        post.body.len(),
        44,
        "base64 of a 32-byte digest, got {:?}",
        post.body
    );
    assert!(
        post.body.ends_with('='),
        "base64 padding, got {:?}",
        post.body
    );
}

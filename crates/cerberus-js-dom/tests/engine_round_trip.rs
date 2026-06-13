//! Engine-driven bridge tests: run real page scripts against a real QuickJS
//! realm via [`run_page_scripts`] and assert the rebuilt Rust DOM reflects their
//! mutations.
//!
//! Each test snapshots a small starting [`Document`], runs one or more scripts,
//! and inspects the reconciled result. The QuickJS realm already installs the
//! speed-first prelude at `create_realm` (timers/observers fire immediately), so
//! these tests also confirm the two prelude layers compose.

use cerberus_dom::{Document, DocumentBuilder, NodeRef};
use cerberus_js::{JsEngine, JsEngineFactory};
use cerberus_js_dom::{install_page, run_page_scripts, run_scripts, serialize_dom, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

/// A fresh QuickJS engine with one realm created, plus that realm's id.
fn engine_and_realm() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    (engine, realm)
}

/// A representative ambient environment shared by these tests: a full URL (so
/// `location.*` has every piece to parse) and a desktop-ish viewport.
fn env() -> PageEnv {
    PageEnv {
        url: "https://example.test/path?q=1#frag".into(),
        viewport: (1280, 800),
        // A representative escalated-rung UA, so the navigator test exercises
        // coherence (navigator.userAgent == this) and platform derivation.
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36"
            .into(),
    }
}

/// Depth-first search for the first element (or `node` itself) with the given
/// tag.
fn find_tag<'a>(node: NodeRef<'a>, tag: &str) -> Option<NodeRef<'a>> {
    if node.is_element() && node.tag() == tag {
        return Some(node);
    }
    node.children().find_map(|c| find_tag(c, tag))
}

/// Depth-first search for the first element whose `id` attribute matches.
fn find_id<'a>(node: NodeRef<'a>, id: &str) -> Option<NodeRef<'a>> {
    if node.is_element() && node.attr("id") == Some(id) {
        return Some(node);
    }
    node.children().find_map(|c| find_id(c, id))
}

/// `<html><body><div id="x">old</div></body></html>`.
fn doc_with_div_x() -> Document {
    let mut b = DocumentBuilder::new();
    let old = b.text("old");
    let div = b.element_attrs("div", vec![("id".into(), "x".into())], [old]);
    let body = b.element("body", [div]);
    let html = b.element("html", [body]);
    b.finish(html)
}

#[test]
fn script_sets_text_content() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["document.getElementById('x').textContent = 'new'".to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "new");
}

#[test]
fn script_creates_and_appends_element() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var p = document.createElement('p'); \
         p.textContent = 'appended'; \
         document.body.appendChild(p);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let body = find_tag(out.root(), "body").expect("body");
    let p = body
        .children()
        .find(|c| c.is_element() && c.tag() == "p")
        .expect("new <p> under body");
    assert_eq!(p.text_content(), "appended");
}

#[test]
fn script_sets_attribute_and_class() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('data-role', 'banner'); \
         x.classList.add('active'); x.classList.add('big');"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("data-role"), Some("banner"));
    let class = x.attr("class").expect("class attr");
    assert!(class.split(' ').any(|c| c == "active"), "got {class:?}");
    assert!(class.split(' ').any(|c| c == "big"), "got {class:?}");
}

#[test]
fn dom_content_loaded_listener_runs() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "document.addEventListener('DOMContentLoaded', function () { \
           document.getElementById('x').textContent = 'ready'; \
         });"
        .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.text_content(),
        "ready",
        "DOMContentLoaded listener should have fired during fire-load"
    );
}

#[test]
fn throwing_script_does_not_abort_run() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "throw new Error('boom')".to_string(),
        "document.body.appendChild(document.createElement('span'))".to_string(),
    ];

    // The first script throws; the run must continue and still return Ok.
    let out =
        run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run returns Ok");
    let body = find_tag(out.root(), "body").expect("body");
    assert!(
        body.children().any(|c| c.is_element() && c.tag() == "span"),
        "second script must still run after the first throws"
    );
}

#[test]
fn console_log_is_captured() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["console.log('hello', 42)".to_string()];

    // run_page_scripts leaves the realm intact, so we can read the capture buffer
    // out of the same realm with a follow-up eval.
    run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let joined = engine
        .eval(realm, "globalThis.__cerberusConsole.join('|')")
        .expect("read console");
    match joined {
        cerberus_js::JsValue::Str(s) => assert!(
            s.contains("hello 42"),
            "console capture should contain 'hello 42', got {s:?}"
        ),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn speed_first_still_applies() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    // setTimeout with a long delay must still have fired by serialize time,
    // because the QuickJS realm's speed-first prelude runs it immediately.
    let scripts = vec![
        "setTimeout(function () { document.getElementById('x').textContent = 'timed'; }, 9999);"
            .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.text_content(),
        "timed",
        "speed-first setTimeout should have fired immediately"
    );
}

// ---------------------------------------------------------------------------
// innerHTML / outerHTML
// ---------------------------------------------------------------------------

#[test]
fn inner_html_set_is_reparsed_into_dom() {
    // The setter stores a raw fragment in JS; Rust reparses it at reconcile so
    // the rebuilt #x has real <b>/<i> element children with the right text.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts =
        vec!["document.getElementById('x').innerHTML = '<b>hi</b><i>there</i>'".to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    let kids: Vec<_> = x.children().filter(|c| c.is_element()).collect();
    assert_eq!(kids.len(), 2, "#x should have two element children");
    assert_eq!(kids[0].tag(), "b");
    assert_eq!(kids[0].text_content(), "hi");
    assert_eq!(kids[1].tag(), "i");
    assert_eq!(kids[1].text_content(), "there");
    // The raw fragment was consumed; no stray `innerHTML` text leaked as a child.
    assert!(
        x.children().all(|c| c.is_element()),
        "innerHTML children should all be elements, got text too"
    );
}

#[test]
fn inner_html_get_serializes_children() {
    // Build children via DOM ops, then read `innerHTML` back in JS and stash it
    // on an attribute so we can assert the serialized markup after reconcile.
    // A void <img> must self-close (no </img>).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); x.textContent = ''; \
         var b = document.createElement('b'); b.textContent = 'hi'; x.appendChild(b); \
         var img = document.createElement('img'); img.setAttribute('src', 'a.png'); x.appendChild(img); \
         x.setAttribute('data-inner', x.innerHTML);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("data-inner"), Some("<b>hi</b><img src=\"a.png\">"));
}

#[test]
fn outer_html_serializes_element() {
    // `outerHTML` includes the element's own open/close tags and attributes.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "var x = document.getElementById('x'); x.textContent = 'body'; \
         x.setAttribute('data-outer', x.outerHTML);"
            .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    let outer = x.attr("data-outer").expect("data-outer");
    assert!(outer.starts_with("<div "), "got {outer:?}");
    assert!(outer.contains("id=\"x\""), "got {outer:?}");
    assert!(outer.ends_with("body</div>"), "got {outer:?}");
}

#[test]
fn insert_adjacent_html_beforeend_reparses() {
    // beforeend routes through the raw-HTML mechanism: pre-existing children are
    // serialized then the new fragment appended, and Rust reparses the whole.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); x.textContent = ''; \
         x.insertAdjacentHTML('beforeend', '<span>one</span>'); \
         x.insertAdjacentHTML('beforeend', '<span>two</span>');"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    let spans: Vec<_> = x
        .children()
        .filter(|c| c.is_element() && c.tag() == "span")
        .collect();
    assert_eq!(spans.len(), 2, "two appended spans expected");
    assert_eq!(spans[0].text_content(), "one");
    assert_eq!(spans[1].text_content(), "two");
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

#[test]
fn selector_compound_and_combinators() {
    // <ul> with two <li> (second `.x`), an <h1.title>, and a bare <span>. A
    // script tags matches with attributes; we assert via the rebuilt DOM.
    let mut b = DocumentBuilder::new();
    let li1t = b.text("a");
    let li1 = b.element("li", [li1t]);
    let li2t = b.text("b");
    let li2 = b.element_attrs("li", vec![("class".into(), "x".into())], [li2t]);
    let ul = b.element("ul", [li1, li2]);
    let h1t = b.text("T");
    let h1 = b.element_attrs("h1", vec![("class".into(), "title".into())], [h1t]);
    let span = b.element("span", []);
    let body = b.element("body", [ul, h1, span]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    // One script that tags matches of: child combinator `ul > li`, compound
    // `h1.title`, selector list `h1, span`, and descendant+compound `ul li.x`.
    // (No `//` comments inside the string: the `\` line-continuations collapse
    // the newlines, so a `//` would swallow the rest of the script.)
    let scripts = vec!["var lis = document.querySelectorAll('ul > li'); \
         for (var i = 0; i < lis.length; i++) lis[i].setAttribute('data-child', '1'); \
         var t = document.querySelector('h1.title'); if (t) t.setAttribute('data-compound', '1'); \
         var list = document.querySelectorAll('h1, span'); \
         for (var j = 0; j < list.length; j++) list[j].setAttribute('data-list', '1'); \
         var d = document.querySelectorAll('ul li.x'); \
         for (var k = 0; k < d.length; k++) d[k].setAttribute('data-desc', '1');"
        .to_string()];

    let (mut engine, realm) = engine_and_realm();
    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");

    // Both <li> matched `ul > li`; only the `.x` one matched `ul li.x`.
    let lis = collect_tag(out.root(), "li");
    assert_eq!(lis.len(), 2);
    assert!(
        lis.iter().all(|li| li.attr("data-child") == Some("1")),
        "both <li> should match `ul > li`"
    );
    let li_x = lis
        .iter()
        .find(|li| li.attr("class") == Some("x"))
        .expect("li.x");
    assert_eq!(li_x.attr("data-desc"), Some("1"), "li.x matches `ul li.x`");

    // <h1.title> matched the compound and the list; <span> matched only the list.
    let h1n = find_tag(out.root(), "h1").expect("h1");
    assert_eq!(h1n.attr("data-compound"), Some("1"));
    assert_eq!(h1n.attr("data-list"), Some("1"));
    let spann = find_tag(out.root(), "span").expect("span");
    assert_eq!(spann.attr("data-list"), Some("1"));
    assert_eq!(
        spann.attr("data-compound"),
        None,
        "<span> must NOT match `h1.title`"
    );
}

/// Collect every element with the given tag, document order.
fn collect_tag<'a>(node: NodeRef<'a>, tag: &str) -> Vec<NodeRef<'a>> {
    let mut acc = Vec::new();
    fn go<'a>(n: NodeRef<'a>, tag: &str, acc: &mut Vec<NodeRef<'a>>) {
        if n.is_element() && n.tag() == tag {
            acc.push(n);
        }
        for c in n.children() {
            go(c, tag, acc);
        }
    }
    go(node, tag, &mut acc);
    acc
}

// ---------------------------------------------------------------------------
// Page environment: location / navigator / storage / matchMedia / styles
// ---------------------------------------------------------------------------

#[test]
fn location_is_parsed_from_env() {
    // `env().url` is https://example.test/path?q=1#frag → pathname `/path`,
    // protocol `https:`. The script writes them into #x for us to assert.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec![
        "document.getElementById('x').textContent = location.pathname + '|' + location.protocol"
            .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "/path|https:");
}

#[test]
fn navigator_is_coherent_with_the_request_ua_and_locks_high_entropy() {
    // navigator.userAgent is EXACTLY the UA the network stack presented (so the
    // request header and the script-visible identity can't disagree),
    // navigator.platform is derived from it, language is uniform en-US, and
    // every high-entropy surface is absent.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('data-ua', String(navigator.userAgent)); \
         x.setAttribute('data-platform', String(navigator.platform)); \
         x.setAttribute('data-lang', String(navigator.language)); \
         x.setAttribute('data-hw', String(navigator.hardwareConcurrency)); \
         x.setAttribute('data-plugins', String(typeof navigator.plugins)); \
         x.setAttribute('data-media', String(typeof navigator.mediaDevices)); \
         x.setAttribute('data-webgl', String(typeof navigator.webgl)); \
         x.setAttribute('data-mem', String(typeof navigator.deviceMemory)); \
         x.setAttribute('data-batt', String(typeof navigator.getBattery)); \
         x.setAttribute('data-wd', String(navigator.webdriver));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    // Coherence: navigator.userAgent is exactly the request UA.
    let want_ua = env().user_agent;
    assert_eq!(x.attr("data-ua"), Some(want_ua.as_str()));
    // Platform is derived from that (Windows) UA, not the empty-string anomaly.
    assert_eq!(x.attr("data-platform"), Some("Win32"));
    assert_eq!(x.attr("data-lang"), Some("en-US"));
    assert_eq!(x.attr("data-hw"), Some("4"));
    // Surface lock: no high-entropy fingerprinting APIs are exposed.
    assert_eq!(x.attr("data-plugins"), Some("undefined"));
    assert_eq!(x.attr("data-media"), Some("undefined"));
    assert_eq!(x.attr("data-webgl"), Some("undefined"));
    assert_eq!(x.attr("data-mem"), Some("undefined"));
    assert_eq!(x.attr("data-batt"), Some("undefined"));
    // webdriver is present-and-false, so its absence isn't itself a tell.
    assert_eq!(x.attr("data-wd"), Some("false"));
}

#[test]
fn navigator_platform_tracks_the_user_agent() {
    // The OS in navigator.platform follows whatever UA we presented — honest
    // (Linux) by default, or the escalated rung's OS — so the two never diverge.
    let cases = [
        ("Cerberus/0.0", "Linux x86_64"),
        (
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
             (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
            "MacIntel",
        ),
        (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36",
            "Win32",
        ),
    ];
    for (ua, want_platform) in cases {
        let (mut engine, realm) = engine_and_realm();
        let doc = doc_with_div_x();
        let pe = PageEnv {
            url: "https://example.test/".into(),
            viewport: (800, 600),
            user_agent: ua.into(),
        };
        let scripts = vec!["var x = document.getElementById('x'); \
             x.setAttribute('data-ua', String(navigator.userAgent)); \
             x.setAttribute('data-platform', String(navigator.platform));"
            .to_string()];
        let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &pe).expect("run");
        let x = find_id(out.root(), "x").expect("#x present");
        assert_eq!(x.attr("data-ua"), Some(ua), "userAgent coherent for {ua:?}");
        assert_eq!(
            x.attr("data-platform"),
            Some(want_platform),
            "platform derivation for {ua:?}"
        );
    }
}

#[test]
fn local_storage_round_trips_within_a_run() {
    // setItem then getItem within the same run returns the value; length and
    // removeItem behave. (No persistence across runs — that is by design.)
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["localStorage.setItem('greeting', 'hello'); \
         localStorage.setItem('n', '2'); \
         var got = localStorage.getItem('greeting'); \
         var len = localStorage.length; \
         localStorage.removeItem('n'); \
         document.getElementById('x').textContent = got + '|' + len + '|' + localStorage.length;"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "hello|2|1");
}

#[test]
fn matchmedia_returns_not_matching() {
    // We do not honor media queries (speed-first); matchMedia always reports
    // matches:false and echoes the query in `media`.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var mq = window.matchMedia('(max-width: 600px)'); \
         document.getElementById('x').textContent = String(mq.matches) + '|' + mq.media;"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "false|(max-width: 600px)");
}

#[test]
fn get_computed_style_returns_inline_or_empty() {
    // getComputedStyle reflects inline `style` declarations and returns "" for
    // properties with no inline value (no CSS cascade is run).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('style', 'color: red; margin: 4px'); \
         var cs = window.getComputedStyle(x); \
         x.setAttribute('data-color', cs.getPropertyValue('color')); \
         x.setAttribute('data-missing', cs.getPropertyValue('display'));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("data-color"), Some("red"));
    assert_eq!(x.attr("data-missing"), Some(""));
}

#[test]
fn window_metrics_come_from_viewport() {
    // innerWidth/innerHeight and screen.* derive from PageEnv::viewport.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["document.getElementById('x').textContent = \
         window.innerWidth + 'x' + window.innerHeight + '|' + screen.width + 'x' + screen.availHeight;"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "1280x800|1280x800");
}

// ---------------------------------------------------------------------------
// Persistent realm: install once, interact (and read back) many times (M12a,
// ADR-0012). The realm and its live document model survive between calls; only
// `install_page` resets them, so script-created state accumulates across
// `run_scripts` batches and `serialize_dom` reads the *current* tree back out
// without re-running anything.
// ---------------------------------------------------------------------------

#[test]
fn serialize_dom_reads_live_model_without_rerunning() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();

    // Install once, then run an initial batch that appends <p id="a">.
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "var p = document.createElement('p'); p.setAttribute('id','a'); \
           p.textContent = 'first'; document.body.appendChild(p);"
                .to_string(),
        ],
    )
    .expect("batch 1");

    // Reading the model back does NOT re-run anything; <p id=a> is present.
    let first = serialize_dom(engine.as_mut(), realm).expect("serialize 1");
    let a = find_id(first.document.root(), "a").expect("#a after batch 1");
    assert_eq!(a.text_content(), "first");
}

#[test]
fn persistent_realm_accumulates_across_interactions() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");

    // Batch 1: append <p id="a">.
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "var p = document.createElement('p'); p.setAttribute('id','a'); \
           document.body.appendChild(p);"
                .to_string(),
        ],
    )
    .expect("batch 1");

    // Batch 2 — WITHOUT re-installing — must still see #a from batch 1 (proving
    // the live model persisted), and only then appends <p id="b">.
    run_scripts(
        engine.as_mut(),
        realm,
        &["if (document.getElementById('a')) { \
             var q = document.createElement('p'); q.setAttribute('id','b'); \
             document.body.appendChild(q); \
           }"
        .to_string()],
    )
    .expect("batch 2");

    let out = serialize_dom(engine.as_mut(), realm).expect("serialize");
    assert!(
        find_id(out.document.root(), "a").is_some(),
        "#a from the first interaction must survive into the second"
    );
    assert!(
        find_id(out.document.root(), "b").is_some(),
        "#b is appended only if batch 2 saw #a — proves the realm persisted"
    );
}

#[test]
fn reinstall_resets_the_live_model() {
    // The flip side: `install_page` IS a reset. After re-installing the original
    // snapshot, script-created #a is gone and the snapshot's #x is back — which
    // is exactly why interactive pages must install only once (ADR-0012).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();

    install_page(engine.as_mut(), realm, &doc, &env()).expect("install 1");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "var p = document.createElement('p'); p.setAttribute('id','a'); \
           document.body.appendChild(p);"
                .to_string(),
        ],
    )
    .expect("batch");
    let before = serialize_dom(engine.as_mut(), realm).expect("serialize before");
    assert!(
        find_id(before.document.root(), "a").is_some(),
        "#a present before reinstall"
    );
    drop(before);

    install_page(engine.as_mut(), realm, &doc, &env()).expect("install 2");
    let after = serialize_dom(engine.as_mut(), realm).expect("serialize after");
    assert!(
        find_id(after.document.root(), "a").is_none(),
        "re-install must reset the model back to the snapshot"
    );
    assert!(
        find_id(after.document.root(), "x").is_some(),
        "#x is restored from the snapshot after reinstall"
    );
}

#[test]
fn serialize_dom_id_map_correlates_rendered_nodes_to_js_ids() {
    // The id map lets the app map a rendered Rust node back to the live JS node
    // it came from (M12b hit-testing): #x's NodeId appears among the map values.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &["document.getElementById('x').setAttribute('data-k','v');".to_string()],
    )
    .expect("run");

    let rebuilt = serialize_dom(engine.as_mut(), realm).expect("serialize");
    assert!(!rebuilt.id_map.is_empty(), "id map should not be empty");
    let x = find_id(rebuilt.document.root(), "x").expect("#x present");
    assert!(
        rebuilt.id_map.values().any(|&nid| nid == x.id()),
        "the rendered #x NodeId must appear in the JS-id → NodeId map"
    );
}

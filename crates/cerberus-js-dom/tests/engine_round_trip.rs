//! Engine-driven bridge tests: run real page scripts against a real QuickJS
//! realm via [`run_page_scripts`] and assert the rebuilt Rust DOM reflects their
//! mutations.
//!
//! Each test snapshots a small starting [`Document`], runs one or more scripts,
//! and inspects the reconciled result. The QuickJS realm already installs the
//! speed-first prelude at `create_realm` (timers/observers fire immediately), so
//! these tests also confirm the two prelude layers compose.

use cerberus_dom::{Document, DocumentBuilder, NodeRef};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{
    dispatch_event, install_page, run_event_loop, run_page_scripts, run_scripts, serialize_dom,
    set_node_value, EventLoopBudget, PageEnv,
};
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
        cookie: String::new(),
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
fn match_media_resolves_length_units() {
    // The test viewport is 1280×800. matchMedia must resolve em/vw units, not
    // read them as bare pixels (`parseInt('100em')` → 100 would be wrong).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('em', String(matchMedia('(max-width: 100em)').matches)); \
         x.setAttribute('vw', String(matchMedia('(min-width: 200vw)').matches)); \
         x.setAttribute('px', String(matchMedia('(min-width: 600px)').matches));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    // 100em = 1600px; 1280 <= 1600 → true (a bare-100 misread would be false).
    assert_eq!(x.attr("em"), Some("true"), "em resolved to 1600px");
    // 200vw = 2560px; 1280 >= 2560 → false (a bare-200 misread would be true).
    assert_eq!(x.attr("vw"), Some("false"), "vw resolved to 2560px");
    assert_eq!(x.attr("px"), Some("true"), "plain px unaffected");
}

#[test]
fn dom_node_exposes_proto_accessor_with_methods() {
    // Anti-tampering / fingerprint code reads `node.__proto__.someMethod` to grab
    // a pristine, un-overridden DOM method off the node's prototype. `node.__proto__`
    // must therefore return the node's prototype (an object carrying the DOM
    // methods), not `undefined` — otherwise `node.__proto__.insertBefore` throws.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('proto', typeof x.__proto__); \
         x.setAttribute('ins', typeof (document.body.__proto__ && document.body.__proto__.insertBefore)); \
         x.setAttribute('gpo', String(Object.getPrototypeOf(x) === x.__proto__));"
        .to_string()];
    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("proto"),
        Some("object"),
        "node.__proto__ is an object"
    );
    assert_eq!(
        x.attr("ins"),
        Some("function"),
        "the prototype carries insertBefore"
    );
    assert_eq!(
        x.attr("gpo"),
        Some("true"),
        "__proto__ agrees with Object.getPrototypeOf"
    );
}

#[test]
fn document_collections_and_get_elements_by_name() {
    // document.forms (with named access), .images, .links (href-only), and
    // getElementsByName.
    let mut b = DocumentBuilder::new();
    let login = b.element_attrs("form", vec![("id".into(), "login".into())], []);
    let search = b.element_attrs("form", vec![("name".into(), "search".into())], []);
    let a1 = b.element_attrs("a", vec![("href".into(), "/a".into())], []);
    let a_nohref = b.element("a", []);
    let a2 = b.element_attrs("a", vec![("href".into(), "/b".into())], []);
    let img = b.element_attrs("img", vec![("src".into(), "/x.png".into())], []);
    let u1 = b.element_attrs("input", vec![("name".into(), "user".into())], []);
    let u2 = b.element_attrs("input", vec![("name".into(), "user".into())], []);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element(
        "body",
        [login, search, a1, a_nohref, a2, img, u1, u2, probe],
    );
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         p.setAttribute('forms', String(document.forms.length)); \
         p.setAttribute('byid', document.forms.login ? 'ok' : 'no'); \
         p.setAttribute('byname', document.forms.search ? 'ok' : 'no'); \
         p.setAttribute('images', String(document.images.length)); \
         p.setAttribute('links', String(document.links.length)); \
         p.setAttribute('named', String(document.getElementsByName('user').length));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("forms"), Some("2"), "two forms");
    assert_eq!(p.attr("byid"), Some("ok"), "document.forms.<id> access");
    assert_eq!(p.attr("byname"), Some("ok"), "document.forms.<name> access");
    assert_eq!(p.attr("images"), Some("1"), "one image");
    assert_eq!(
        p.attr("links"),
        Some("2"),
        "only anchors with href count as links"
    );
    assert_eq!(
        p.attr("named"),
        Some("2"),
        "getElementsByName collects both inputs"
    );
}

#[test]
fn query_selector_sibling_combinators() {
    // `+` (adjacent) and `~` (general) sibling combinators in querySelector.
    let mut b = DocumentBuilder::new();
    let h = b.element_attrs("h2", vec![("id".into(), "h".into())], []);
    let p1 = b.element_attrs("p", vec![("id".into(), "p1".into())], []);
    let p2 = b.element_attrs("p", vec![("id".into(), "p2".into())], []);
    let sp = b.element_attrs("span", vec![("id".into(), "s".into())], []);
    let p3 = b.element_attrs("p", vec![("id".into(), "p3".into())], []);
    let probe = b.element_attrs("div", vec![("id".into(), "d".into())], []);
    let body = b.element("body", [h, p1, p2, sp, p3, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var d = document.getElementById('d'); \
         d.setAttribute('adj', document.querySelector('h2 + p').id); \
         d.setAttribute('adj-none', String(document.querySelectorAll('h2 + span').length)); \
         var g = document.querySelectorAll('h2 ~ p'); \
         d.setAttribute('gen', Array.prototype.map.call(g, function (x) { return x.id; }).join(',')); \
         d.setAttribute('gen-span', document.querySelector('h2 ~ span').id); \
         d.setAttribute('m1', String(document.getElementById('p1').matches('h2 + p'))); \
         d.setAttribute('m2', String(document.getElementById('p2').matches('h2 + p')));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let d = find_id(out.root(), "d").expect("#d present");
    assert_eq!(
        d.attr("adj"),
        Some("p1"),
        "+ matches the immediately-next sibling"
    );
    assert_eq!(d.attr("adj-none"), Some("0"), "+ requires adjacency");
    assert_eq!(
        d.attr("gen"),
        Some("p1,p2,p3"),
        "~ matches all following siblings"
    );
    assert_eq!(
        d.attr("gen-span"),
        Some("s"),
        "~ works across element types"
    );
    assert_eq!(
        d.attr("m1"),
        Some("true"),
        "matches('h2 + p') on the adjacent p"
    );
    assert_eq!(d.attr("m2"), Some("false"), "not adjacent → no match");
}

#[test]
fn class_list_replace_and_value() {
    // classList.replace swaps a token in place (de-duplicating), and
    // classList.value reads/writes the whole token string.
    let mut b = DocumentBuilder::new();
    let x = b.element_attrs(
        "div",
        vec![("id".into(), "x".into()), ("class".into(), "a b c".into())],
        [],
    );
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [x, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var x = document.getElementById('x'); \
         p.setAttribute('ret', String(x.classList.replace('b', 'B'))); \
         p.setAttribute('cls', x.className); \
         p.setAttribute('miss', String(x.classList.replace('zzz', 'q'))); \
         p.setAttribute('val', x.classList.value); \
         x.classList.value = 'one two'; \
         p.setAttribute('setval', x.className);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("ret"),
        Some("true"),
        "replace returns true when present"
    );
    assert_eq!(p.attr("cls"), Some("a B c"), "token replaced in place");
    assert_eq!(
        p.attr("miss"),
        Some("false"),
        "replace of an absent token is false"
    );
    assert_eq!(
        p.attr("val"),
        Some("a B c"),
        "classList.value reads the string"
    );
    assert_eq!(
        p.attr("setval"),
        Some("one two"),
        "classList.value writes the class"
    );
}

#[test]
fn query_selector_attribute_operators() {
    // The substring/word/dash attribute operators (^= $= *= ~= |=) now work in
    // querySelector, not just presence and exact match.
    let mut b = DocumentBuilder::new();
    let a1 = b.element_attrs(
        "a",
        vec![
            ("id".into(), "a1".into()),
            ("href".into(), "https://example.com/x".into()),
            ("class".into(), "btn primary".into()),
            ("lang".into(), "en-US".into()),
        ],
        [],
    );
    let a2 = b.element_attrs(
        "a",
        vec![
            ("id".into(), "a2".into()),
            ("href".into(), "/local.png".into()),
            ("class".into(), "btn".into()),
        ],
        [],
    );
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [a1, a2, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         p.setAttribute('pre', document.querySelector('[href^=\"https\"]').id); \
         p.setAttribute('suf', document.querySelector('[href$=\".png\"]').id); \
         p.setAttribute('sub', document.querySelector('[href*=\"example\"]').id); \
         p.setAttribute('word', String(document.querySelectorAll('[class~=\"primary\"]').length)); \
         p.setAttribute('dash', document.querySelector('[lang|=\"en\"]').id); \
         p.setAttribute('btn', String(document.querySelectorAll('[class~=\"btn\"]').length)); \
         p.setAttribute('none', String(document.querySelectorAll('[href^=\"ftp\"]').length));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("pre"), Some("a1"), "^= prefix");
    assert_eq!(p.attr("suf"), Some("a2"), "$= suffix");
    assert_eq!(p.attr("sub"), Some("a1"), "*= substring");
    assert_eq!(p.attr("word"), Some("1"), "~= whitespace word");
    assert_eq!(p.attr("dash"), Some("a1"), "|= exact-or-hyphen prefix");
    assert_eq!(p.attr("btn"), Some("2"), "~= matches both");
    assert_eq!(p.attr("none"), Some("0"), "no false prefix match");
}

#[test]
fn query_selector_supports_state_and_structural_pseudos() {
    // querySelector/matches now understand the form-state pseudo-classes
    // (:checked/:disabled/:required) and the structural ones (:first-child,
    // :last-child, :empty); an unsupported dynamic pseudo (:hover) matches
    // nothing statically.
    let mut b = DocumentBuilder::new();
    let at = b.text("a");
    let a = b.element("li", [at]);
    let ct = b.text("c");
    let c_li = b.element("li", [ct]);
    let mid = b.text("b");
    let ul = b.element("ul", [a, mid, c_li]);
    let chk = b.element_attrs(
        "input",
        vec![
            ("id".into(), "c".into()),
            ("type".into(), "checkbox".into()),
            ("checked".into(), "".into()),
        ],
        [],
    );
    let dis = b.element_attrs(
        "input",
        vec![("id".into(), "d".into()), ("disabled".into(), "".into())],
        [],
    );
    let empty = b.element_attrs("div", vec![("id".into(), "e".into())], []);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [ul, chk, dis, empty, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         p.setAttribute('checked', document.querySelector('input:checked').id); \
         p.setAttribute('disabled', document.querySelector(':disabled').id); \
         p.setAttribute('first', document.querySelector('li:first-child').textContent); \
         p.setAttribute('last', document.querySelector('li:last-child').textContent); \
         p.setAttribute('empty', document.querySelector('div:empty').id); \
         p.setAttribute('matches', String(document.getElementById('c').matches(':checked'))); \
         p.setAttribute('hover', String(document.querySelectorAll('li:hover').length));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("checked"),
        Some("c"),
        ":checked finds the checked input"
    );
    assert_eq!(
        p.attr("disabled"),
        Some("d"),
        ":disabled finds the disabled input"
    );
    assert_eq!(
        p.attr("first"),
        Some("a"),
        ":first-child skips text siblings"
    );
    assert_eq!(p.attr("last"), Some("c"), ":last-child skips text siblings");
    assert_eq!(
        p.attr("empty"),
        Some("e"),
        ":empty matches the childless div"
    );
    assert_eq!(p.attr("matches"), Some("true"), "matches(':checked') works");
    assert_eq!(
        p.attr("hover"),
        Some("0"),
        ":hover never matches statically"
    );
}

#[test]
fn form_data_scrapes_successful_controls() {
    // new FormData(form) collects named, non-disabled controls; an unchecked
    // checkbox, a disabled input, and a <button> are all excluded.
    let mut b = DocumentBuilder::new();
    let user = b.element_attrs(
        "input",
        vec![
            ("name".into(), "user".into()),
            ("value".into(), "alice".into()),
        ],
        [],
    );
    let agree = b.element_attrs(
        "input",
        vec![
            ("name".into(), "agree".into()),
            ("type".into(), "checkbox".into()),
            ("checked".into(), "".into()),
        ],
        [],
    );
    let news = b.element_attrs(
        "input",
        vec![
            ("name".into(), "news".into()),
            ("type".into(), "checkbox".into()),
        ],
        [],
    );
    let skip = b.element_attrs(
        "input",
        vec![
            ("name".into(), "skip".into()),
            ("value".into(), "x".into()),
            ("disabled".into(), "".into()),
        ],
        [],
    );
    let btn = b.element_attrs(
        "button",
        vec![("name".into(), "btn".into()), ("value".into(), "go".into())],
        [],
    );
    let form = b.element_attrs(
        "form",
        vec![("id".into(), "f".into())],
        [user, agree, news, skip, btn],
    );
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [form, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var fd = new FormData(document.getElementById('f')); \
         p.setAttribute('user', fd.get('user')); \
         p.setAttribute('agree', fd.get('agree')); \
         p.setAttribute('news', String(fd.has('news'))); \
         p.setAttribute('skip', String(fd.has('skip'))); \
         p.setAttribute('btn', String(fd.has('btn'))); \
         fd.append('extra', '1'); fd.append('extra', '2'); \
         p.setAttribute('extra', fd.getAll('extra').join(','));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("user"), Some("alice"), "text control included");
    assert_eq!(p.attr("agree"), Some("on"), "checked checkbox → on");
    assert_eq!(p.attr("news"), Some("false"), "unchecked checkbox excluded");
    assert_eq!(p.attr("skip"), Some("false"), "disabled control excluded");
    assert_eq!(
        p.attr("btn"),
        Some("false"),
        "<button> excluded (no submitter)"
    );
    assert_eq!(p.attr("extra"), Some("1,2"), "programmatic append works");
}

#[test]
fn btoa_and_atob_round_trip_base64() {
    // Base64 encode/decode with correct padding, round-trip, and the Latin1
    // guard on btoa.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('enc', btoa('Man')); \
         x.setAttribute('pad', btoa('M')); \
         x.setAttribute('dec', atob('TWFu')); \
         x.setAttribute('round', atob(btoa('Hello, World!'))); \
         x.setAttribute('auth', btoa('user:pass')); \
         var threw = false; try { btoa('\\u2603'); } catch (e) { threw = true; } \
         x.setAttribute('latin1', String(threw));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("enc"), Some("TWFu"), "btoa encodes");
    assert_eq!(x.attr("pad"), Some("TQ=="), "btoa pads a 1-byte input");
    assert_eq!(x.attr("dec"), Some("Man"), "atob decodes");
    assert_eq!(
        x.attr("round"),
        Some("Hello, World!"),
        "btoa/atob round-trip"
    );
    assert_eq!(x.attr("auth"), Some("dXNlcjpwYXNz"), "basic-auth blob");
    assert_eq!(
        x.attr("latin1"),
        Some("true"),
        "btoa rejects codepoints > 255"
    );
}

#[test]
fn url_search_params_parses_mutates_and_serializes() {
    // A page-side URLSearchParams round-trip: parse (with `?`, `+`, `%`), the
    // multi-value accessors, set/append/delete, and encoded `toString`.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         var u = new URLSearchParams('?a=1&b=hello%20world&a=3'); \
         x.setAttribute('get', u.get('a')); \
         x.setAttribute('all', u.getAll('a').join(',')); \
         x.setAttribute('dec', u.get('b')); \
         u.append('c', 'x+y'); u.set('a', '9'); u.delete('b'); \
         x.setAttribute('str', u.toString());"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("get"), Some("1"), "get returns the first value");
    assert_eq!(x.attr("all"), Some("1,3"), "getAll returns every value");
    assert_eq!(x.attr("dec"), Some("hello world"), "%20 and query decode");
    // set collapses the two `a`s to one (=9), b removed, appended c encoded.
    assert_eq!(
        x.attr("str"),
        Some("a=9&c=x%2By"),
        "toString reflects mutations, encoded"
    );
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
fn insert_adjacent_element_and_text_all_positions() {
    // insertAdjacentElement places a real node at any of the four positions and
    // returns it (null for an unknown position); insertAdjacentText wraps a string.
    let mut b = DocumentBuilder::new();
    let mt = b.text("M");
    let mid = b.element_attrs("span", vec![("id".into(), "mid".into())], [mt]);
    let wrap = b.element_attrs("div", vec![("id".into(), "wrap".into())], [mid]);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [wrap, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var wrap = document.getElementById('wrap'); \
         var mid = document.getElementById('mid'); \
         var mk = function (t) { var e = document.createElement('i'); e.textContent = t; return e; }; \
         var ret = mid.insertAdjacentElement('beforebegin', mk('B')); \
         mid.insertAdjacentElement('afterbegin', mk('A')); \
         mid.insertAdjacentElement('beforeend', mk('E')); \
         mid.insertAdjacentElement('afterend', mk('F')); \
         mid.insertAdjacentText('beforeend', 'T'); \
         p.setAttribute('data-wrap', wrap.textContent); \
         p.setAttribute('data-mid', mid.textContent); \
         p.setAttribute('data-ret', String(ret.textContent)); \
         p.setAttribute('data-bad', String(mid.insertAdjacentElement('nope', mk('X'))));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    // wrap: B, then mid("A M E T"), then F.
    assert_eq!(
        p.attr("data-wrap"),
        Some("BAMETF"),
        "all four positions place content"
    );
    assert_eq!(
        p.attr("data-mid"),
        Some("AMET"),
        "afterbegin/beforeend/text land inside"
    );
    assert_eq!(
        p.attr("data-ret"),
        Some("B"),
        "returns the inserted element"
    );
    assert_eq!(
        p.attr("data-bad"),
        Some("null"),
        "unknown position returns null"
    );
}

#[test]
fn clone_node_shallow_and_deep() {
    // Shallow clone copies attributes but no children; deep clone copies the
    // subtree; clones are independent (mutating one attribute leaves the source
    // untouched) and start with no parent.
    let mut b = DocumentBuilder::new();
    let child_text = b.text("child");
    let span = b.element("span", [child_text]);
    let src = b.element_attrs(
        "div",
        vec![
            ("id".into(), "src".into()),
            ("class".into(), "orig".into()),
            ("data-x".into(), "1".into()),
        ],
        [span],
    );
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [src, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var src = document.getElementById('src'); \
         var sh = src.cloneNode(false); \
         p.setAttribute('sh-kids', String(sh.children.length)); \
         p.setAttribute('sh-class', sh.className); \
         var dp = src.cloneNode(true); \
         p.setAttribute('dp-text', dp.querySelector('span').textContent); \
         p.setAttribute('dp-parent', String(dp.parentNode)); \
         dp.setAttribute('data-x', '99'); \
         p.setAttribute('indep', src.getAttribute('data-x') + '/' + dp.getAttribute('data-x'));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("sh-kids"),
        Some("0"),
        "shallow clone has no children"
    );
    assert_eq!(
        p.attr("sh-class"),
        Some("orig"),
        "shallow clone copies attributes"
    );
    assert_eq!(
        p.attr("dp-text"),
        Some("child"),
        "deep clone copies the subtree"
    );
    assert_eq!(p.attr("dp-parent"), Some("null"), "a clone has no parent");
    assert_eq!(
        p.attr("indep"),
        Some("1/99"),
        "mutating the clone's attribute does not affect the source"
    );
}

#[test]
fn parentnode_childnode_insertion_methods() {
    // `append`/`prepend`/`before`/`after`/`replaceWith` accept a variadic mix of
    // nodes and strings (strings become text) in argument order, and `replaceWith`
    // removes the target. This mirrors the modern insertion idiom pages use.
    let mut b = DocumentBuilder::new();
    let bt = b.text("B");
    let mid = b.element_attrs("li", vec![("id".into(), "mid".into())], [bt]);
    let list = b.element_attrs("ul", vec![("id".into(), "list".into())], [mid]);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [list, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var list = document.getElementById('list'); \
         var mid = document.getElementById('mid'); \
         var a = document.createElement('li'); a.textContent = 'A'; list.prepend(a); \
         var c = document.createElement('li'); c.textContent = 'C'; list.append(c, ' tail'); \
         mid.before('X'); mid.after('Y'); \
         p.setAttribute('data-text', list.textContent); \
         var box = document.createElement('div'); box.append('1', '2', '3'); \
         p.setAttribute('data-order', box.textContent); \
         var r = document.createElement('li'); r.textContent = 'R'; mid.replaceWith(r); \
         p.setAttribute('data-gone', String(list.querySelector('#mid') === null));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("data-text"),
        Some("AXBYC tail"),
        "prepend/append/before/after place content in order"
    );
    assert_eq!(
        p.attr("data-order"),
        Some("123"),
        "variadic strings keep order"
    );
    assert_eq!(
        p.attr("data-gone"),
        Some("true"),
        "replaceWith removed the target"
    );
}

#[test]
fn element_sibling_and_count_accessors() {
    // `childElementCount` and element-only sibling walking skip text nodes, which
    // is how pages iterate a list without tripping over whitespace.
    let mut b = DocumentBuilder::new();
    let a = b.element_attrs("span", vec![("id".into(), "a".into())], []);
    let mid = b.text(" ws ");
    let c = b.element_attrs("span", vec![("id".into(), "c".into())], []);
    let wrap = b.element_attrs("div", vec![("id".into(), "wrap".into())], [a, mid, c]);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [wrap, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var wrap = document.getElementById('wrap'); \
         var a = document.getElementById('a'); \
         var c = document.getElementById('c'); \
         p.setAttribute('data-count', String(wrap.childElementCount)); \
         p.setAttribute('data-next', a.nextElementSibling.id); \
         p.setAttribute('data-prev', c.previousElementSibling.id); \
         p.setAttribute('data-end', String(c.nextElementSibling));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("data-count"), Some("2"), "two element children");
    assert_eq!(
        p.attr("data-next"),
        Some("c"),
        "nextElementSibling skips the text node"
    );
    assert_eq!(
        p.attr("data-prev"),
        Some("a"),
        "previousElementSibling skips the text node"
    );
    assert_eq!(
        p.attr("data-end"),
        Some("null"),
        "no element sibling at the end"
    );
}

#[test]
fn toggle_attribute_flips_and_honors_force() {
    // `toggleAttribute` flips presence, honors an explicit `force`, and returns
    // whether the attribute is present afterwards.
    let mut b = DocumentBuilder::new();
    let el = b.element_attrs("div", vec![("id".into(), "el".into())], []);
    let probe = b.element_attrs("p", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [el, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         var el = document.getElementById('el'); \
         p.setAttribute('r1', String(el.toggleAttribute('hidden'))); \
         p.setAttribute('r2', String(el.toggleAttribute('hidden'))); \
         el.toggleAttribute('data-keep', true); \
         el.toggleAttribute('data-drop', false); \
         p.setAttribute('r3', String(el.hasAttribute('data-keep'))); \
         p.setAttribute('r4', String(el.hasAttribute('data-drop')));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("r1"), Some("true"), "first toggle adds → present");
    assert_eq!(
        p.attr("r2"),
        Some("false"),
        "second toggle removes → absent"
    );
    assert_eq!(p.attr("r3"), Some("true"), "force:true adds");
    assert_eq!(
        p.attr("r4"),
        Some("false"),
        "force:false removes/keeps absent"
    );
    // The forced-on attribute survives to the reconciled DOM.
    let el = find_id(out.root(), "el").expect("#el present");
    assert!(el.attr("data-keep").is_some(), "data-keep persisted");
    assert!(el.attr("hidden").is_none(), "hidden toggled back off");
}

#[test]
fn hidden_property_reflects_the_attribute() {
    // `el.hidden` reads the `hidden` attribute and writing it toggles the
    // attribute (which the UA sheet renders as display:none).
    let mut b = DocumentBuilder::new();
    let h = b.element_attrs(
        "p",
        vec![("id".into(), "h".into()), ("hidden".into(), "".into())],
        [],
    );
    let v = b.element_attrs("p", vec![("id".into(), "v".into())], []);
    let probe = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [h, v, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); \
         p.setAttribute('h0', String(document.getElementById('h').hidden)); \
         p.setAttribute('v0', String(document.getElementById('v').hidden)); \
         document.getElementById('v').hidden = true;"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("h0"), Some("true"), "<p hidden>.hidden is true");
    assert_eq!(p.attr("v0"), Some("false"), "plain <p>.hidden is false");
    let v = find_id(out.root(), "v").expect("#v present");
    assert!(
        v.attr("hidden").is_some(),
        "v.hidden = true set the attribute"
    );
}

#[test]
fn href_and_src_resolve_to_absolute_urls() {
    // `a.href` / `img.src` return the attribute resolved against the document
    // location (env url is https://example.test/path?q=1#frag). A non-href
    // element has no `.href`.
    let mut b = DocumentBuilder::new();
    let a_abs = b.element_attrs(
        "a",
        vec![
            ("id".into(), "abs".into()),
            ("href".into(), "https://other.test/x".into()),
        ],
        [],
    );
    let a_root = b.element_attrs(
        "a",
        vec![
            ("id".into(), "root".into()),
            ("href".into(), "/root/p?q=1".into()),
        ],
        [],
    );
    let a_dot = b.element_attrs(
        "a",
        vec![("id".into(), "dot".into()), ("href".into(), "../up".into())],
        [],
    );
    let a_frag = b.element_attrs(
        "a",
        vec![("id".into(), "frag".into()), ("href".into(), "#sec".into())],
        [],
    );
    let im = b.element_attrs(
        "img",
        vec![
            ("id".into(), "im".into()),
            ("src".into(), "/img/a.png".into()),
        ],
        [],
    );
    let probe = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [a_abs, a_root, a_dot, a_frag, im, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var p = document.getElementById('p'); var $ = function (id) { return document.getElementById(id); }; \
         p.setAttribute('abs', $('abs').href); \
         p.setAttribute('root', $('root').href); \
         p.setAttribute('dot', $('dot').href); \
         p.setAttribute('frag', $('frag').href); \
         p.setAttribute('img', $('im').src); \
         p.setAttribute('div', String($('p').href));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("abs"),
        Some("https://other.test/x"),
        "absolute unchanged"
    );
    assert_eq!(
        p.attr("root"),
        Some("https://example.test/root/p?q=1"),
        "root-relative"
    );
    assert_eq!(
        p.attr("dot"),
        Some("https://example.test/up"),
        "dot-segment normalized"
    );
    assert_eq!(
        p.attr("frag"),
        Some("https://example.test/path?q=1#sec"),
        "fragment on base"
    );
    assert_eq!(
        p.attr("img"),
        Some("https://example.test/img/a.png"),
        "img.src absolute"
    );
    assert_eq!(
        p.attr("div"),
        Some("undefined"),
        "non-href element has no href"
    );
}

#[test]
fn dataset_maps_camelcase_to_data_attributes() {
    // `el.dataset.userId` reads `data-user-id`; assigning `dataset.newKey`
    // writes `data-new-key`; enumeration lists the camelCased keys.
    let mut b = DocumentBuilder::new();
    let d = b.element_attrs(
        "div",
        vec![
            ("id".into(), "d".into()),
            ("data-user-id".into(), "42".into()),
            ("data-role".into(), "admin".into()),
        ],
        [],
    );
    // A separate probe element, so writing results here doesn't add `data-*`
    // attributes to `#d` (which would show up in its own dataset enumeration).
    let p = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [d, p]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var d = document.getElementById('d'); \
         var p = document.getElementById('p'); \
         p.setAttribute('read', d.dataset.userId + ',' + d.dataset.role); \
         d.dataset.newKey = 'nv'; \
         p.setAttribute('keys', Object.keys(d.dataset).sort().join(','));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let d = find_id(out.root(), "d").expect("#d present");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("read"),
        Some("42,admin"),
        "camelCase read maps to data-*"
    );
    assert_eq!(
        d.attr("data-new-key"),
        Some("nv"),
        "dataset write maps to kebab attr"
    );
    // Enumeration lists camelCased keys (including the just-added one).
    assert_eq!(
        p.attr("keys"),
        Some("newKey,role,userId"),
        "Object.keys(dataset) lists camelCase keys"
    );
}

#[test]
fn select_value_index_and_options_reflect() {
    // `<select>.value`/`.selectedIndex`/`.options` read the chosen option, and
    // setting either selects an option by writing the `selected` attribute the
    // layout renderer reads — so JS selection and rendering stay in sync.
    let mut b = DocumentBuilder::new();
    let xo = b.text("X");
    let yo = b.text("Y");
    let zo = b.text("Z");
    let o1 = b.element_attrs("option", vec![("value".into(), "x".into())], [xo]);
    let o2 = b.element_attrs(
        "option",
        vec![("value".into(), "y".into()), ("selected".into(), "".into())],
        [yo],
    );
    let o3 = b.element_attrs("option", vec![("value".into(), "z".into())], [zo]);
    let sel = b.element_attrs("select", vec![("id".into(), "s".into())], [o1, o2, o3]);
    let probe = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [sel, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var s = document.getElementById('s'); \
         var p = document.getElementById('p'); \
         p.setAttribute('data-v', s.value); \
         p.setAttribute('data-i', String(s.selectedIndex)); \
         p.setAttribute('data-n', String(s.options.length)); \
         p.setAttribute('data-t2', s.options[2].text); \
         s.selectedIndex = 2; \
         p.setAttribute('data-v2', s.value); \
         s.value = 'x'; \
         p.setAttribute('data-i2', String(s.selectedIndex));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(p.attr("data-v"), Some("y"), "value is the selected option");
    assert_eq!(p.attr("data-i"), Some("1"), "selectedIndex of the selected");
    assert_eq!(p.attr("data-n"), Some("3"), "options.length");
    assert_eq!(p.attr("data-t2"), Some("Z"), "options[2].text");
    assert_eq!(p.attr("data-v2"), Some("z"), "selectedIndex=2 → value z");
    assert_eq!(p.attr("data-i2"), Some("0"), "value='x' → selectedIndex 0");
}

#[test]
fn form_elements_collects_controls_with_index_and_named_access() {
    // `form.elements` is the form's listed controls in tree order: `.length`
    // and indexing work, each control is reachable by `name`/`id` and via
    // `namedItem`, and `el.name` reflects the `name` attribute — the two idioms
    // pages use to read a form for submission.
    let mut b = DocumentBuilder::new();
    let user = b.element_attrs(
        "input",
        vec![
            ("name".into(), "user".into()),
            ("value".into(), "alice".into()),
        ],
        [],
    );
    let oa = b.text("A");
    let ob = b.text("B");
    let opt_a = b.element_attrs("option", vec![("value".into(), "a".into())], [oa]);
    let opt_b = b.element_attrs("option", vec![("value".into(), "b".into())], [ob]);
    let role = b.element_attrs(
        "select",
        vec![("name".into(), "role".into())],
        [opt_a, opt_b],
    );
    let go = b.element_attrs("button", vec![("id".into(), "go".into())], []);
    // A stray non-control descendant must NOT be counted.
    let span = b.element_attrs("span", vec![("name".into(), "nope".into())], []);
    let form = b.element_attrs(
        "form",
        vec![("id".into(), "f".into())],
        [user, role, go, span],
    );
    let probe = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [form, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var f = document.getElementById('f'); \
         var p = document.getElementById('p'); \
         var els = f.elements; \
         p.setAttribute('data-len', String(els.length)); \
         p.setAttribute('data-i0', els[0].name); \
         p.setAttribute('data-user', els.user.value); \
         p.setAttribute('data-named', els.namedItem('role') ? els.namedItem('role').tagName : '?'); \
         p.setAttribute('data-byid', els.go ? els.go.tagName : '?'); \
         var order = []; for (var i = 0; i < els.length; i++) order.push(els[i].name || els[i].id); \
         p.setAttribute('data-order', order.join(','));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("data-len"),
        Some("3"),
        "three listed controls (span excluded)"
    );
    assert_eq!(
        p.attr("data-i0"),
        Some("user"),
        "els[0].name reflects the name attr"
    );
    assert_eq!(p.attr("data-user"), Some("alice"), "named access by name");
    assert_eq!(
        p.attr("data-named"),
        Some("SELECT"),
        "namedItem looks up by name"
    );
    assert_eq!(
        p.attr("data-byid"),
        Some("BUTTON"),
        "named access falls back to id"
    );
    assert_eq!(
        p.attr("data-order"),
        Some("user,role,go"),
        "tree order preserved"
    );
}

#[test]
fn form_control_type_checked_and_textarea_value_reflect() {
    // `input.type` defaults to "text"; `input.checked` reflects (and writes) the
    // `checked` attribute so it drives rendering; `textarea.value` falls back to
    // the element's text content when there is no value attribute.
    let mut b = DocumentBuilder::new();
    let cbtext = b.text("area");
    let input = b.element_attrs(
        "input",
        vec![
            ("id".into(), "cb".into()),
            ("type".into(), "checkbox".into()),
        ],
        [],
    );
    let ta = b.element_attrs("textarea", vec![("id".into(), "ta".into())], [cbtext]);
    let probe = b.element_attrs("div", vec![("id".into(), "p".into())], []);
    let body = b.element("body", [input, ta, probe]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);

    let (mut engine, realm) = engine_and_realm();
    let scripts = vec!["var cb = document.getElementById('cb'); \
         var ta = document.getElementById('ta'); \
         var p = document.getElementById('p'); \
         p.setAttribute('data-type', cb.type); \
         p.setAttribute('data-checked0', String(cb.checked)); \
         cb.checked = true; \
         p.setAttribute('data-checked1', String(cb.checked)); \
         p.setAttribute('data-ta', ta.value);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let p = find_id(out.root(), "p").expect("#p present");
    assert_eq!(
        p.attr("data-type"),
        Some("checkbox"),
        "input.type reflects attr"
    );
    assert_eq!(
        p.attr("data-checked0"),
        Some("false"),
        "unchecked initially"
    );
    assert_eq!(
        p.attr("data-checked1"),
        Some("true"),
        "el.checked=true reflects"
    );
    assert_eq!(
        p.attr("data-ta"),
        Some("area"),
        "textarea.value is its text"
    );

    // The scripted `checked = true` must reflect onto the input for rendering.
    let cb = find_id(out.root(), "cb").expect("#cb present");
    assert!(
        cb.attr("checked").is_some(),
        "checked attribute set for layout"
    );
}

#[test]
fn programmatic_click_fires_listeners_and_bubbles() {
    // `element.click()` must dispatch a click event to the element's own
    // listeners and bubble to ancestors — a page toggling a menu via
    // `el.click()` is a common pattern. `focus`/`blur` must at least be
    // callable (they fire their listeners and track activeElement).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         var hits = 0; \
         x.addEventListener('click', function () { hits++; }); \
         var bodyHits = 0; \
         document.body.addEventListener('click', function () { bodyHits++; }); \
         x.click(); \
         x.focus(); x.blur(); \
         x.setAttribute('data-hits', String(hits)); \
         x.setAttribute('data-body-hits', String(bodyHits)); \
         x.setAttribute('data-active', String(document.activeElement === null));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("data-hits"),
        Some("1"),
        "own click listener fired once"
    );
    assert_eq!(
        x.attr("data-body-hits"),
        Some("1"),
        "click bubbled to the body listener"
    );
    assert_eq!(
        x.attr("data-active"),
        Some("true"),
        "blur() cleared document.activeElement"
    );
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
    // A long-delay setTimeout still fires by serialize time: run_page_scripts
    // drains the bounded event loop (ADR-0013) and virtual time ignores the
    // 9999ms delay — speed-first, now correctly ordered.
    let scripts = vec![
        "setTimeout(function () { document.getElementById('x').textContent = 'timed'; }, 9999);"
            .to_string(),
    ];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.text_content(),
        "timed",
        "the long-delay timer fired via the bounded loop (virtual time ignores the delay)"
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
fn navigator_is_a_coherent_chrome_on_windows_persona() {
    // navigator.userAgent is EXACTLY the UA the network stack presented (so the
    // request header and the script-visible identity can't disagree),
    // navigator.platform is derived from it, and the rest of the surface is a
    // COMPLETE, COHERENT Chrome-on-Windows persona: every API a real Chrome
    // exposes is present (an *absent* read is the tell a sensor fails on).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('data-ua', String(navigator.userAgent)); \
         x.setAttribute('data-platform', String(navigator.platform)); \
         x.setAttribute('data-lang', String(navigator.language)); \
         x.setAttribute('data-hw', String(navigator.hardwareConcurrency)); \
         x.setAttribute('data-vendor', String(navigator.vendor)); \
         x.setAttribute('data-psub', String(navigator.productSub)); \
         x.setAttribute('data-mem', String(navigator.deviceMemory)); \
         x.setAttribute('data-dnt', String(navigator.doNotTrack)); \
         x.setAttribute('data-plugins', String(typeof navigator.plugins) + ':' + navigator.plugins.length); \
         x.setAttribute('data-media', String(typeof navigator.mediaDevices)); \
         x.setAttribute('data-batt', String(typeof navigator.getBattery)); \
         x.setAttribute('data-perm', String(typeof navigator.permissions.query)); \
         x.setAttribute('data-uad', String(navigator.userAgentData.platform) + ':' + navigator.userAgentData.mobile); \
         x.setAttribute('data-conn', String(navigator.connection.effectiveType)); \
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
    // Complete, coherent Chrome-on-Windows surface.
    assert_eq!(x.attr("data-vendor"), Some("Google Inc."));
    assert_eq!(x.attr("data-psub"), Some("20030107"));
    assert_eq!(x.attr("data-mem"), Some("8"));
    assert_eq!(x.attr("data-dnt"), Some("null"));
    assert_eq!(x.attr("data-plugins"), Some("object:5"));
    assert_eq!(x.attr("data-media"), Some("object"));
    assert_eq!(x.attr("data-batt"), Some("function"));
    assert_eq!(x.attr("data-perm"), Some("function"));
    assert_eq!(x.attr("data-uad"), Some("Windows:false"));
    assert_eq!(x.attr("data-conn"), Some("4g"));
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
            cookie: String::new(),
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

#[test]
fn window_and_document_fingerprint_surface_is_complete() {
    // The window + document expose the full Chrome-on-Windows surface a scanner
    // reads: frame identity (top/parent/self all this window, no frameElement),
    // self-consistent window geometry (screen == avail == outer == inner with no
    // profile), visualViewport, a monotonic performance clock, CSS/getSelection,
    // and the document's static metadata
    // (characterSet/compatMode/contentType/visibilityState) plus fonts,
    // implementation, and an activeElement that defaults to <body>.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('w-top', String(window.top === window && window.parent === window && window.self === window && window.frames === window)); \
         x.setAttribute('w-frame', String(window.frameElement) + ':' + String(window.length) + ':' + String(window.name)); \
         x.setAttribute('w-outer', String(window.outerHeight - window.innerHeight) + ':' + String(window.outerWidth - window.innerWidth)); \
         x.setAttribute('w-vv', String(window.visualViewport.width) + 'x' + String(window.visualViewport.height) + '@' + String(window.visualViewport.scale)); \
         x.setAttribute('w-perf', String(window.performance.now() < window.performance.now()) + ':' + String(window.performance.memory.jsHeapSizeLimit)); \
         x.setAttribute('w-css', String(window.CSS.supports('display','flex')) + ':' + window.CSS.escape('a b')); \
         x.setAttribute('w-sel', String(window.getSelection().rangeCount)); \
         x.setAttribute('w-screenx', String(window.screenX) + ':' + String(window.screenLeft)); \
         x.setAttribute('d-ref', 'r=' + String(document.referrer)); \
         x.setAttribute('d-cs', document.characterSet + ':' + document.charset + ':' + document.inputEncoding); \
         x.setAttribute('d-mode', document.compatMode + ':' + document.contentType); \
         x.setAttribute('d-vis', document.visibilityState + ':' + String(document.hidden) + ':' + String(document.hasFocus())); \
         x.setAttribute('d-fonts', String(typeof document.fonts) + ':' + String(document.fonts.check('12px monospace'))); \
         x.setAttribute('d-impl', String(typeof document.implementation.createHTMLDocument)); \
         x.setAttribute('d-active', String(document.activeElement === document.body));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("w-top"),
        Some("true"),
        "top/parent/self/frames alias this window"
    );
    assert_eq!(
        x.attr("w-frame"),
        Some("null:0:"),
        "no host frame, no child frames, no name"
    );
    // No profile is injected here, so this is an inert maximized surface: the
    // outer window equals the inner viewport (no chrome), keeping the geometry
    // self-consistent (screen == avail == outer == inner) instead of the old
    // outerHeight > screen.height impossibility. Real ~88px browser chrome is
    // added only under a coherent profile, whose screen is a real monitor larger
    // than the viewport — that path is covered by the profile_persona geometry
    // test.
    assert_eq!(x.attr("w-outer"), Some("0:0"));
    assert_eq!(x.attr("w-vv"), Some("1280x800@1"));
    assert_eq!(
        x.attr("w-perf"),
        Some("true:2172649472"),
        "performance.now() is monotonic"
    );
    assert_eq!(x.attr("w-css"), Some("true:a b"));
    assert_eq!(x.attr("w-sel"), Some("0"));
    assert_eq!(x.attr("w-screenx"), Some("0:0"));
    assert_eq!(
        x.attr("d-ref"),
        Some("r="),
        "referrer present and empty (not undefined)"
    );
    assert_eq!(x.attr("d-cs"), Some("UTF-8:UTF-8:UTF-8"));
    assert_eq!(x.attr("d-mode"), Some("CSS1Compat:text/html"));
    assert_eq!(x.attr("d-vis"), Some("visible:false:true"));
    assert_eq!(x.attr("d-fonts"), Some("object:true"));
    assert_eq!(x.attr("d-impl"), Some("function"));
    assert_eq!(
        x.attr("d-active"),
        Some("true"),
        "activeElement defaults to <body>"
    );
}

#[test]
fn matchmedia_reports_a_coherent_desktop_persona() {
    // A light-scheme, motion-allowing, mouse-driven Windows desktop: exactly one
    // value of each discrete feature matches. The prior all-false behavior was an
    // impossible prefers-color-scheme state a scanner flags.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         function mm(q) { return String(window.matchMedia(q).matches); } \
         x.setAttribute('scheme', mm('(prefers-color-scheme: light)') + ':' + mm('(prefers-color-scheme: dark)')); \
         x.setAttribute('motion', mm('(prefers-reduced-motion: no-preference)') + ':' + mm('(prefers-reduced-motion: reduce)')); \
         x.setAttribute('pointer', mm('(pointer: fine)') + ':' + mm('(pointer: coarse)')); \
         x.setAttribute('hover', mm('(hover: hover)') + ':' + mm('(any-pointer: fine)')); \
         var one = window.matchMedia('(prefers-color-scheme: light)'); \
         x.setAttribute('shape', typeof one.onchange + ':' + typeof one.addEventListener + ':' + typeof one.addListener + ':' + one.media);"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("scheme"),
        Some("true:false"),
        "light matches, dark does not"
    );
    assert_eq!(x.attr("motion"), Some("true:false"));
    assert_eq!(x.attr("pointer"), Some("true:false"));
    assert_eq!(x.attr("hover"), Some("true:true"));
    // The MediaQueryList shape a listener-registering page needs.
    assert_eq!(
        x.attr("shape"),
        Some("object:function:function:(prefers-color-scheme: light)")
    );
}

#[test]
fn async_high_entropy_apis_resolve_to_chrome_values() {
    // The Promise-returning surface (userAgentData.getHighEntropyValues,
    // mediaDevices.enumerateDevices, permissions.query, getBattery,
    // storage.estimate) settles one microtask deep, which the per-eval job pump
    // flushes — so a `.then` writing back into the DOM is visible on serialize.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         navigator.userAgentData.getHighEntropyValues(['architecture','bitness','platformVersion']) \
           .then(function (h) { x.setAttribute('uad', h.architecture + ':' + h.bitness + ':' + h.platformVersion + ':' + h.uaFullVersion); }); \
         navigator.mediaDevices.enumerateDevices() \
           .then(function (d) { x.setAttribute('devs', String(d.length) + ':' + d[0].kind + ':' + d[1].kind + ':' + d[2].kind); }); \
         navigator.permissions.query({ name: 'geolocation' }) \
           .then(function (p) { x.setAttribute('perm-geo', p.state); }); \
         navigator.permissions.query({ name: 'notifications' }) \
           .then(function (p) { x.setAttribute('perm-notif', p.state); }); \
         navigator.getBattery() \
           .then(function (b) { x.setAttribute('batt', String(b.charging) + ':' + String(b.level) + ':' + String(b.dischargingTime)); }); \
         navigator.storage.estimate() \
           .then(function (e) { x.setAttribute('quota', String(e.quota)); });"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("uad"), Some("x86:64:15.0.0:142.0.0.0"));
    assert_eq!(x.attr("devs"), Some("3:audioinput:videoinput:audiooutput"));
    assert_eq!(x.attr("perm-geo"), Some("granted"));
    assert_eq!(
        x.attr("perm-notif"),
        Some("default"),
        "notifications default, others granted"
    );
    assert_eq!(x.attr("batt"), Some("true:1:Infinity"));
    assert_eq!(x.attr("quota"), Some("299977129984"));
}

#[test]
fn text_encoder_decoder_round_trips_utf8() {
    // Real UTF-8: ASCII (1B), Latin-1 (2B), CJK (3B), and an astral emoji (4B)
    // survive an encode -> decode round-trip, and the byte length is correct.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         var s = 'A\\u00e9\\u65e5\\ud83d\\ude00'; \
         var bytes = new TextEncoder().encode(s); \
         var back = new TextDecoder().decode(bytes); \
         x.setAttribute('rt', String(back === s)); \
         x.setAttribute('len', String(bytes.length)); \
         x.setAttribute('b0', String(bytes[0]));"
        .to_string()];

    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("rt"), Some("true"), "encode/decode round-trips");
    // 1 (A) + 2 (é) + 3 (日) + 4 (😀) = 10 bytes.
    assert_eq!(x.attr("len"), Some("10"));
    assert_eq!(
        x.attr("b0"),
        Some("65"),
        "'A' encodes to a single 0x41 byte"
    );
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

// ---------------------------------------------------------------------------
// Real DOM event dispatch (M12b): __cerberusDispatch runs listeners through the
// target + bubbling phases and reports preventDefault; serialize_dom then reads
// the handler's mutations back. The JS node id equals the snapshot NodeId right
// after install (serialize_document keys the wire id off NodeId), so we dispatch
// at the input doc's #x id.
// ---------------------------------------------------------------------------

/// Install `doc` and return #x's id in the fresh model (== its Rust `NodeId`).
fn install_and_x_id(engine: &mut dyn JsEngine, realm: RealmId, doc: &Document) -> u64 {
    install_page(engine, realm, doc, &env()).expect("install");
    u64::from(find_id(doc.root(), "x").expect("#x in doc").id())
}

#[test]
fn dispatch_click_runs_target_listener_and_bubbles_to_ancestor() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let x_id = install_and_x_id(engine.as_mut(), realm, &doc);
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').addEventListener('click', function () { \
             this.textContent = 'clicked'; }); \
           document.body.addEventListener('click', function (e) { \
             e.currentTarget.setAttribute('data-bubbled', '1'); });"
                .to_string(),
        ],
    )
    .expect("wire listeners");

    let out = dispatch_event(engine.as_mut(), realm, x_id, "click", "{}").expect("dispatch");
    assert!(out.dispatched, "target existed");
    assert!(!out.default_prevented, "no preventDefault");
    let x = find_id(out.dom.document.root(), "x").expect("#x");
    assert_eq!(x.text_content(), "clicked", "target listener ran");
    let body = find_tag(out.dom.document.root(), "body").expect("body");
    assert_eq!(
        body.attr("data-bubbled"),
        Some("1"),
        "click bubbled to the body listener"
    );
}

#[test]
fn dispatch_prevent_default_is_reported() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let x_id = install_and_x_id(engine.as_mut(), realm, &doc);
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').addEventListener('click', function (e) { \
             e.preventDefault(); });"
                .to_string(),
        ],
    )
    .expect("wire listener");

    let out = dispatch_event(engine.as_mut(), realm, x_id, "click", "{}").expect("dispatch");
    assert!(out.dispatched);
    assert!(
        out.default_prevented,
        "preventDefault on a cancelable event must be reported"
    );
}

#[test]
fn dispatch_stop_propagation_halts_bubbling() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let x_id = install_and_x_id(engine.as_mut(), realm, &doc);
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').addEventListener('click', function (e) { \
             e.stopPropagation(); this.setAttribute('data-hit', '1'); }); \
           document.body.addEventListener('click', function (e) { \
             e.currentTarget.setAttribute('data-bubbled', '1'); });"
                .to_string(),
        ],
    )
    .expect("wire listeners");

    let out = dispatch_event(engine.as_mut(), realm, x_id, "click", "{}").expect("dispatch");
    let x = find_id(out.dom.document.root(), "x").expect("#x");
    assert_eq!(x.attr("data-hit"), Some("1"), "target listener still ran");
    let body = find_tag(out.dom.document.root(), "body").expect("body");
    assert_eq!(
        body.attr("data-bubbled"),
        None,
        "stopPropagation must prevent the ancestor listener"
    );
}

#[test]
fn dispatch_non_bubbling_event_stays_on_target() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let x_id = install_and_x_id(engine.as_mut(), realm, &doc);
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').addEventListener('focus', function () { \
             this.setAttribute('data-focused', '1'); }); \
           document.body.addEventListener('focus', function (e) { \
             e.currentTarget.setAttribute('data-bubbled', '1'); });"
                .to_string(),
        ],
    )
    .expect("wire listeners");

    let out = dispatch_event(engine.as_mut(), realm, x_id, "focus", "{\"bubbles\":false}")
        .expect("dispatch");
    let x = find_id(out.dom.document.root(), "x").expect("#x");
    assert_eq!(x.attr("data-focused"), Some("1"), "target listener ran");
    let body = find_tag(out.dom.document.root(), "body").expect("body");
    assert_eq!(
        body.attr("data-bubbled"),
        None,
        "a non-bubbling event must not reach ancestors"
    );
}

#[test]
fn dispatch_carries_init_fields_to_listener() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let x_id = install_and_x_id(engine.as_mut(), realm, &doc);
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').addEventListener('keydown', function (e) { \
             this.setAttribute('data-key', String(e.key)); });"
                .to_string(),
        ],
    )
    .expect("wire listener");

    let out = dispatch_event(
        engine.as_mut(),
        realm,
        x_id,
        "keydown",
        "{\"key\":\"Enter\"}",
    )
    .expect("dispatch");
    let x = find_id(out.dom.document.root(), "x").expect("#x");
    assert_eq!(
        x.attr("data-key"),
        Some("Enter"),
        "the init field reached the listener as e.key"
    );
}

#[test]
fn dispatch_to_unknown_node_is_a_noop() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    let out = dispatch_event(engine.as_mut(), realm, 999_999, "click", "{}").expect("dispatch");
    assert!(!out.dispatched, "no such node");
    assert!(!out.default_prevented);
}

// ---------------------------------------------------------------------------
// Bounded virtual-clock event loop (M12c / ADR-0013): run_event_loop drains
// timers one-per-eval so microtasks interleave (correct ordering), under caps
// that guarantee termination on both axes (task count and virtual time).
// ---------------------------------------------------------------------------

/// `globalThis.order.join(",")` read out of the realm.
fn order_str(engine: &mut dyn JsEngine, realm: RealmId) -> String {
    match engine
        .eval(realm, "globalThis.order.join(',')")
        .expect("read order")
    {
        JsValue::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    }
}

/// A numeric global/expression read out of the realm.
fn num_global(engine: &mut dyn JsEngine, realm: RealmId, expr: &str) -> f64 {
    match engine.eval(realm, expr).expect("read number") {
        JsValue::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn event_loop_orders_sync_then_microtask_then_macrotask() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &["globalThis.order = []; \
           setTimeout(function () { globalThis.order.push('macro'); }, 0); \
           Promise.resolve().then(function () { globalThis.order.push('micro'); }); \
           globalThis.order.push('sync');"
            .to_string()],
    )
    .expect("run");
    // The per-eval job pump already ran the microtask; the timer has not fired.
    assert_eq!(order_str(engine.as_mut(), realm), "sync,micro");

    let stats = run_event_loop(engine.as_mut(), realm, EventLoopBudget::default()).expect("loop");
    assert_eq!(stats.tasks_run, 1);
    assert!(!stats.hit_task_cap);
    assert_eq!(
        order_str(engine.as_mut(), realm),
        "sync,micro,macro",
        "the macrotask runs only after sync code and all microtasks"
    );
}

#[test]
fn event_loop_caps_runaway_settimeout_recursion() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "globalThis.n = 0; (function loop() { globalThis.n++; setTimeout(loop, 0); })();"
                .to_string(),
        ],
    )
    .expect("run");
    // A 0-delay self-reschedule never advances the virtual clock; the task cap is
    // what stops it (this is the loop that would otherwise hang the browser).
    let budget = EventLoopBudget {
        max_tasks: 50,
        max_virtual_ms: 60_000,
        max_wall_ms: 0,
    };
    let stats = run_event_loop(engine.as_mut(), realm, budget).expect("loop");
    assert_eq!(stats.tasks_run, 50, "ran exactly the cap");
    assert!(
        stats.hit_task_cap,
        "stopped on the task cap, not an empty queue"
    );
    // 1 initial sync call + 50 capped reschedules that ran.
    assert_eq!(num_global(engine.as_mut(), realm, "globalThis.n"), 51.0);
}

#[test]
fn event_loop_caps_setinterval_by_virtual_clock() {
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &["globalThis.n = 0; setInterval(function () { globalThis.n++; }, 1000);".to_string()],
    )
    .expect("run");
    // A 5s virtual budget admits exactly five 1s ticks (due 1000..5000); the task
    // cap is slack, so the clock is what bounds the interval.
    let budget = EventLoopBudget {
        max_tasks: 10_000,
        max_virtual_ms: 5_000,
        max_wall_ms: 0,
    };
    let stats = run_event_loop(engine.as_mut(), realm, budget).expect("loop");
    assert_eq!(
        stats.tasks_run, 5,
        "interval ticks bounded by the virtual clock"
    );
    assert!(
        !stats.hit_task_cap,
        "stopped on the clock budget, not the task cap"
    );
    assert_eq!(num_global(engine.as_mut(), realm, "globalThis.n"), 5.0);
}

// ---------------------------------------------------------------------------
// Keyboard / input events (M12b): set_node_value pushes the live value into the
// model so an `input` handler reads e.target.value, and a handler's value rewrite
// round-trips back through serialize.
// ---------------------------------------------------------------------------

#[test]
fn set_value_then_input_event_sees_and_can_change_value() {
    let (mut engine, realm) = engine_and_realm();
    // <html><body><input id="t"></body></html>
    let mut b = DocumentBuilder::new();
    let input = b.element_attrs("input", vec![("id".into(), "t".into())], []);
    let body = b.element("body", [input]);
    let html = b.element("html", [body]);
    let doc = b.finish(html);
    let t_id = u64::from(find_id(doc.root(), "t").expect("#t").id());

    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('t').addEventListener('input', function (e) { \
             e.target.setAttribute('data-seen', e.target.value); \
             e.target.value = e.target.value.toUpperCase(); });"
                .to_string(),
        ],
    )
    .expect("run");

    // The app sets the live value, then fires `input`.
    set_node_value(engine.as_mut(), realm, t_id, "hi").expect("set value");
    let out = dispatch_event(engine.as_mut(), realm, t_id, "input", "{}").expect("dispatch");
    let t = find_id(out.dom.document.root(), "t").expect("#t");
    assert_eq!(
        t.attr("data-seen"),
        Some("hi"),
        "the handler read the just-set live value via e.target.value"
    );
    assert_eq!(
        t.attr("value"),
        Some("HI"),
        "the handler's value rewrite is reflected in the serialized DOM"
    );
}

#[test]
fn document_fonts_check_consults_the_persona_font_list() {
    // document.fonts.check enumerates installed fonts. With a per-head font set
    // injected (as the profile prologue does), a generic family and a listed
    // family resolve; an unlisted family does not — so check() agrees with the
    // measureText-based enumeration defense on one per-head-random list.
    let (mut engine, realm) = engine_and_realm();
    engine
        .eval(
            realm,
            "globalThis.__CERBERUS_PROFILE__ = { fonts: ['Georgia', 'Consolas'] };",
        )
        .expect("set profile");
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('c-generic', String(document.fonts.check('12px monospace'))); \
         x.setAttribute('c-listed', String(document.fonts.check(\"12px 'Georgia'\"))); \
         x.setAttribute('c-stack', String(document.fonts.check('italic bold 14px \"Consolas\", monospace'))); \
         x.setAttribute('c-absent', String(document.fonts.check(\"12px 'No Such Font 9000'\")));"
        .to_string()];
    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("c-generic"), Some("true"), "generic family resolves");
    assert_eq!(x.attr("c-listed"), Some("true"), "listed family resolves");
    assert_eq!(
        x.attr("c-stack"),
        Some("true"),
        "first family in a stack is listed"
    );
    assert_eq!(
        x.attr("c-absent"),
        Some("false"),
        "unlisted family does not resolve"
    );
}

#[test]
fn document_fonts_check_reports_page_font_face_families() {
    // A page's OWN @font-face families are reported "loaded" — matching a real
    // browser that loaded them — so a sensor can't flag us for not loading our
    // own web font. The host injects __CERBERUS_PAGE_FONTS__ from the parsed CSS;
    // the bytes are never fetched (ADR-0005), and this is additive to the persona
    // enumeration list (an undeclared, unlisted name still resolves false).
    let (mut engine, realm) = engine_and_realm();
    engine
        .eval(
            realm,
            "globalThis.__CERBERUS_PROFILE__ = { fonts: ['Georgia'] };\
             globalThis.__CERBERUS_PAGE_FONTS__ = ['mozilla text'];",
        )
        .expect("set globals");
    let doc = doc_with_div_x();
    let scripts = vec!["var x = document.getElementById('x'); \
         x.setAttribute('c-page', String(document.fonts.check(\"12px 'Mozilla Text'\"))); \
         x.setAttribute('c-absent', String(document.fonts.check(\"12px 'Zilla Slab 9000'\")));"
        .to_string()];
    let out = run_page_scripts(engine.as_mut(), realm, &doc, &scripts, &env()).expect("run");
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("c-page"),
        Some("true"),
        "page @font-face family reports loaded"
    );
    assert_eq!(
        x.attr("c-absent"),
        Some("false"),
        "a font neither declared nor in the persona list does not resolve"
    );
}

//! `fetch()` bridge tests (ADR-0014): a page script calls `fetch`, the host
//! drains the per-realm queue through a stub [`FetchClient`], settles the Promise,
//! and the bounded event loop runs the resulting `.then`/`.catch` chains — then we
//! assert the reconciled Rust DOM reflects what the chain wrote.
//!
//! `fetch()` itself never calls native code (the engine seam is eval-only); these
//! tests exercise the enqueue → host-drain → resolve round-trip end to end against
//! a real QuickJS realm (native Promises + the speed-first job pump).

use cerberus_dom::{Document, DocumentBuilder, NodeRef};
use cerberus_js::{JsEngine, JsEngineFactory};
use cerberus_js_dom::{
    drive_fetches, fire_load, install_page, run_page_scripts_with_fetch, run_scripts,
    serialize_dom, take_cookie_writes, take_navigations, EventLoopBudget, FetchBudget, FetchClient,
    FetchRequest, FetchResponse, Navigation, PageEnv,
};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;
use std::collections::HashMap;

/// A fresh QuickJS engine with one realm created, plus that realm's id.
fn engine_and_realm() -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    (engine, realm)
}

/// A representative ambient environment shared by these tests.
fn env() -> PageEnv {
    PageEnv {
        url: "https://example.test/".into(),
        viewport: (1280, 800),
        user_agent: "Cerberus/0.0".into(),
        cookie: String::new(),
    }
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

/// A stub network seam: it records every request it sees, and answers by URL from
/// a canned table (a 200 with the table's body), or 404s an unknown URL. Flip
/// `fail` to make every request a network error instead (drives `.catch`).
#[derive(Default)]
struct StubClient {
    /// Canned response bodies keyed by request URL.
    responses: HashMap<String, FetchResponse>,
    /// Every request passed to `fetch`, in call order.
    seen: Vec<FetchRequest>,
    /// When set, every `fetch` returns this `Err` (a network error).
    fail: Option<String>,
}

impl StubClient {
    fn with_body(url: &str, body: &str) -> Self {
        let mut responses = HashMap::new();
        responses.insert(
            url.to_string(),
            FetchResponse {
                status: 200,
                status_text: "OK".into(),
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.into(),
            },
        );
        Self {
            responses,
            ..Self::default()
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            fail: Some(message.to_string()),
            ..Self::default()
        }
    }
}

impl FetchClient for StubClient {
    fn fetch(&mut self, req: &FetchRequest) -> Result<FetchResponse, String> {
        self.seen.push(req.clone());
        if let Some(message) = &self.fail {
            return Err(message.clone());
        }
        match self.responses.get(&req.url) {
            Some(resp) => Ok(resp.clone()),
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

#[test]
fn fetch_json_then_chain_writes_to_dom() {
    // fetch('/api').then(r => r.json()).then(d => write d.v into #x). The stub
    // returns `{"v":42}`; after the pump runs the chain, #x text is "42".
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/api", r#"{"v":42}"#);
    let scripts = vec!["fetch('/api').then(function (r) { return r.json(); }) \
         .then(function (d) { document.getElementById('x').textContent = String(d.v); });"
        .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with fetch");

    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "42", "the .json().then() chain wrote d.v");
    assert_eq!(client.seen.len(), 1, "exactly one request was serviced");
    assert_eq!(client.seen[0].url, "/api");
    assert_eq!(client.seen[0].method, "GET");
}

#[test]
fn fetch_captures_post_method_headers_and_body() {
    // A POST with a header and a JSON body: the stub must capture method,
    // headers (normalized to [name,value] pairs), and the verbatim body.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/submit", "{}");
    let scripts = vec!["fetch('/submit', { method: 'post', \
         headers: { 'Content-Type': 'application/json' }, \
         body: '{\"a\":1}' });"
        .to_string()];

    run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
        .expect("run with fetch");

    assert_eq!(client.seen.len(), 1, "one request serviced");
    let req = &client.seen[0];
    assert_eq!(req.url, "/submit");
    assert_eq!(req.method, "POST", "method is upper-cased");
    assert_eq!(req.body, r#"{"a":1}"#, "body crosses verbatim");
    assert!(
        req.headers
            .iter()
            .any(|(n, v)| n == "Content-Type" && v == "application/json"),
        "the Content-Type header was captured, got {:?}",
        req.headers
    );
}

#[test]
fn rejected_fetch_runs_catch() {
    // The stub fails every request; the page's .catch must run and set an
    // attribute on #x. Reaching .catch proves the Promise was rejected with the
    // host's network-error message.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::failing("connection refused");
    let scripts = vec!["fetch('/api').then(function () { \
           document.getElementById('x').setAttribute('data-result', 'ok'); \
         }).catch(function (e) { \
           document.getElementById('x').setAttribute('data-result', 'caught:' + String(e.message)); \
         });"
    .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with fetch");

    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("data-result"),
        Some("caught:connection refused"),
        "the .catch ran with the host's network-error message"
    );
    assert_eq!(client.seen.len(), 1, "the one request was attempted");
}

#[test]
fn fetch_with_crlf_injected_header_rejects_cleanly() {
    // A page tries to smuggle an extra header via a CRLF in a header *value*
    // (issue #57: e.g. `"X": "a\r\nCookie: stolen=1"` could otherwise sneak a
    // `Cookie:` line past the engine's name-only allow-list). The bridge's
    // source-side guard (`decode_header_pairs`) must reject the fetch cleanly
    // — the page's `.catch` runs — rather than the malformed descriptor
    // silently reaching the network client or dangling forever.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/api", "{}");
    let scripts = vec!["fetch('/api', { headers: { 'X': 'a\\r\\nX-Injected: 1' } }) \
         .then(function () { \
           document.getElementById('x').setAttribute('data-result', 'ok'); \
         }).catch(function (e) { \
           document.getElementById('x').setAttribute('data-result', 'caught:' + String(e.message)); \
         });"
    .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with fetch");

    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(
        x.attr("data-result"),
        Some("caught:invalid header value"),
        "the CRLF-carrying header rejected the fetch instead of reaching the network"
    );
    assert!(
        client.seen.is_empty(),
        "the malformed request must never reach the network client"
    );
}

#[test]
fn fetch_budget_caps_runaway_then_chain() {
    // Each response schedules another fetch from its .then. With a small request
    // budget the pump must stop at max_requests and report hit_cap. We install +
    // run + fire load manually so we can pass a tight FetchBudget to drive_fetches.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/loop", "{}");

    install_page(engine.as_mut(), realm, &doc, &env()).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        // Every settled response fires another fetch — an unbounded chain that
        // only the fetch budget can stop.
        &["function go() { fetch('/loop').then(go); } go();".to_string()],
    )
    .expect("run");
    fire_load(engine.as_mut(), realm).expect("fire load");

    let budget = FetchBudget {
        max_rounds: 1000,
        max_requests: 5,
    };
    let stats = drive_fetches(
        engine.as_mut(),
        realm,
        &mut client,
        EventLoopBudget::default(),
        budget,
    )
    .expect("drive fetches");

    assert_eq!(stats.requests, 5, "serviced exactly the request cap");
    assert!(stats.hit_cap, "stopped on the budget, not a drained queue");
    // We attempted exactly the budgeted number of requests.
    assert_eq!(client.seen.len(), 5, "no requests beyond the cap were run");

    // The realm is still usable after the cap: read the DOM back without panic.
    let dom = serialize_dom(engine.as_mut(), realm).expect("serialize after cap");
    assert!(
        find_id(dom.document.root(), "x").is_some(),
        "#x still present after the capped pump"
    );
}

// ---- XMLHttpRequest (rides the same host fetch queue) --------------------

#[test]
fn xhr_get_writes_response_to_dom() {
    // A page opens an XHR GET, and its onload writes the responseText into #x.
    // Proves XHR enqueues onto the same host queue and settles through the same
    // resolve path as fetch().
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/api", r#"{"v":42}"#);
    let scripts = vec!["var xhr = new XMLHttpRequest(); \
         xhr.open('GET', '/api'); \
         xhr.onload = function () { \
           if (xhr.readyState === 4 && xhr.status === 200) \
             document.getElementById('x').textContent = xhr.responseText; \
         }; \
         xhr.send();"
        .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with xhr");

    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), r#"{"v":42}"#, "onload wrote responseText");
    assert_eq!(client.seen.len(), 1);
    assert_eq!(client.seen[0].method, "GET");
    assert_eq!(client.seen[0].url, "/api");
}

#[test]
fn xhr_post_captures_body_headers_and_reads_status_and_header() {
    // A POST with a request header and body; the load listener reads back the
    // status and a response header. Mirrors the reese84 sensor POST shape.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::with_body("/sensor", "{}");
    let scripts = vec!["var xhr = new XMLHttpRequest(); \
         xhr.open('POST', '/sensor'); \
         xhr.setRequestHeader('X-Sensor', 'abc'); \
         xhr.addEventListener('load', function () { \
           var el = document.getElementById('x'); \
           el.setAttribute('data-status', String(xhr.status)); \
           el.setAttribute('data-ct', xhr.getResponseHeader('content-type') || ''); \
         }); \
         xhr.send('payload=1');"
        .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with xhr");

    let req = &client.seen[0];
    assert_eq!(req.method, "POST", "method upper-cased");
    assert_eq!(req.body, "payload=1", "body crosses verbatim");
    assert!(
        req.headers
            .iter()
            .any(|(n, v)| n == "X-Sensor" && v == "abc"),
        "request header captured: {:?}",
        req.headers
    );
    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.attr("data-status"), Some("200"));
    assert_eq!(x.attr("data-ct"), Some("application/json"));
}

#[test]
fn xhr_network_error_fires_onerror_at_readystate_4() {
    // A failing client rejects the request; XHR must reach readyState 4 with
    // status 0 and fire onerror (not onload).
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut client = StubClient::failing("connection refused");
    let scripts = vec!["var xhr = new XMLHttpRequest(); \
         xhr.open('GET', '/api'); \
         xhr.onreadystatechange = function () { \
           if (xhr.readyState === 4) \
             document.getElementById('x').setAttribute('data-rs', '4'); \
         }; \
         xhr.onload = function () { \
           document.getElementById('x').setAttribute('data-onload', 'yes'); \
         }; \
         xhr.onerror = function () { \
           document.getElementById('x').textContent = 'error:' + xhr.status; \
         }; \
         xhr.send();"
        .to_string()];

    let out =
        run_page_scripts_with_fetch(engine.as_mut(), realm, &doc, &scripts, &env(), &mut client)
            .expect("run with xhr");

    let x = find_id(out.root(), "x").expect("#x present");
    assert_eq!(x.text_content(), "error:0", "onerror ran with status 0");
    assert_eq!(x.attr("data-rs"), Some("4"), "reached readyState 4");
    assert_eq!(x.attr("data-onload"), None, "onload did NOT fire on error");
}

// ---- document.cookie <-> sealed jar bridge (Phase B) --------------------

#[test]
fn document_cookie_seeds_from_env_and_captures_writes() {
    // The host seeds document.cookie with the instance's cookies (via env), a
    // script reads them, then sets a new cookie — which is queued verbatim (with
    // attributes) for the host to persist into the sealed jar.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut e = env();
    e.cookie = "sid=abc; theme=dark".into();
    install_page(engine.as_mut(), realm, &doc, &e).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.getElementById('x').setAttribute('data-seed', document.cookie);".to_string(),
            "document.cookie = 'token=xyz; Path=/; Max-Age=3600';".to_string(),
            // A second write; the in-memory view also reflects it for later reads.
            "document.getElementById('x').setAttribute('data-after', document.cookie);".to_string(),
        ],
    )
    .expect("scripts");

    let dom = serialize_dom(engine.as_mut(), realm).expect("serialize");
    let x = find_id(dom.document.root(), "x").expect("#x present");
    // The script read the seeded jar cookies.
    assert_eq!(x.attr("data-seed"), Some("sid=abc; theme=dark"));
    // The in-memory view updated (attributes stripped for the read view).
    assert_eq!(x.attr("data-after"), Some("sid=abc; theme=dark; token=xyz"));

    // The write is queued for the host with its full raw string (attrs intact),
    // so the sealed jar can honor Path/Max-Age exactly like a network Set-Cookie.
    let writes = take_cookie_writes(engine.as_mut(), realm).expect("take writes");
    assert_eq!(writes, vec!["token=xyz; Path=/; Max-Age=3600".to_string()]);
    // Draining again yields nothing (the queue was cleared).
    assert!(take_cookie_writes(engine.as_mut(), realm)
        .expect("take again")
        .is_empty());
}

#[test]
fn document_cookie_view_honors_deletion() {
    // A script deleting a cookie (Max-Age<=0) must see it leave the document.cookie
    // read view within the same turn, matching the sealed jar (which expires it).
    // The raw deletion string is still queued for the host to honor in the jar.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let mut e = env();
    e.cookie = "a=1; b=2".into();
    install_page(engine.as_mut(), realm, &doc, &e).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "document.cookie = 'a=; Max-Age=0';".to_string(),
            "document.getElementById('x').setAttribute('data-after', document.cookie);".to_string(),
        ],
    )
    .expect("scripts");

    let dom = serialize_dom(engine.as_mut(), realm).expect("serialize");
    let x = find_id(dom.document.root(), "x").expect("#x present");
    // `a` is dropped from the view; `b` remains.
    assert_eq!(x.attr("data-after"), Some("b=2"));

    // The deletion is still surfaced to the host so the jar expires it too.
    let writes = take_cookie_writes(engine.as_mut(), realm).expect("take writes");
    assert_eq!(writes, vec!["a=; Max-Age=0".to_string()]);
}

// ---- script navigation capture (Phase D) --------------------------------

#[test]
fn location_navigations_are_captured_for_the_host() {
    // location.assign/replace, window.location = "...", and location.reload() each
    // record a navigation intent for the host to perform (resolve + fetch). This is
    // the mechanism a cookie-gated reload (e.g. a solved bot challenge) rides.
    let (mut engine, realm) = engine_and_realm();
    let doc = doc_with_div_x();
    let e = env(); // https://example.test/
    install_page(engine.as_mut(), realm, &doc, &e).expect("install");
    run_scripts(
        engine.as_mut(),
        realm,
        &[
            "location.assign('/a');".to_string(),
            "location.replace('/b');".to_string(),
            "window.location = 'https://c.test/';".to_string(),
            "location.reload();".to_string(),
        ],
    )
    .expect("scripts");

    let navs = take_navigations(engine.as_mut(), realm).expect("navs");
    assert_eq!(
        navs,
        vec![
            Navigation {
                url: "/a".into(),
                replace: false
            },
            Navigation {
                url: "/b".into(),
                replace: true
            },
            Navigation {
                url: "https://c.test/".into(),
                replace: false
            },
            // reload() targets the now-current href (updated by the window.location
            // assignment above), replacing history.
            Navigation {
                url: "https://c.test/".into(),
                replace: true
            },
        ]
    );
    // Draining again yields nothing (the queue was cleared).
    assert!(take_navigations(engine.as_mut(), realm)
        .expect("again")
        .is_empty());
}

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
    serialize_dom, EventLoopBudget, FetchBudget, FetchClient, FetchRequest, FetchResponse, PageEnv,
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

//! Engine-agnostic DOM bridge (ADR-0008).
//!
//! Page `<script>`s expect a live `document`/`window` they can read and mutate.
//! Our Rust [`Document`] is an immutable arena ([`cerberus_dom`]), and the JS
//! engine seam ([`cerberus_js::JsEngine`]) is deliberately *eval-only* — no way
//! to reach into engine internals, no `unsafe`. This crate bridges the two with
//! a **snapshot → run → serialize → rebuild** round-trip, entirely over `eval`:
//!
//! 1. **Snapshot.** [`serialize_document`] walks the Rust DOM and emits a small
//!    JSON document (the wire format below).
//! 2. **Install.** We eval [`DOM_MODEL_PRELUDE`] — a self-contained JS document
//!    model — then hand it the snapshot; it builds JS node objects with the
//!    usual `parentNode`/`childNodes`/`children` links.
//! 3. **Run.** Each page script is evaluated in turn. A script that *throws*
//!    does not abort the run (browsers move on to the next `<script>`); only an
//!    engine/realm-level failure propagates.
//! 4. **Serialize + rebuild.** The model serializes its (now mutated) tree back
//!    to the same wire format, and [`rebuild_document`] reconstructs a fresh,
//!    immutable Rust [`Document`].
//!
//! Because the only seam is `eval`, the whole DOM surface lives in one auditable
//! JS string ([`DOM_MODEL_PRELUDE`]) plus a tiny Rust JSON layer ([`mod@json`],
//! no `serde`). This mirrors how [`cerberus_js_quickjs`]'s `SPEED_FIRST_PRELUDE`
//! installs its host shims; the two compose — when the realm was created by the
//! QuickJS adapter, `setTimeout`/`requestAnimationFrame`/observers already fire
//! immediately, so a script that defers a DOM write behind a timer still lands.
//!
//! # Wire format
//!
//! A document is `{"root": <int>, "nodes": [ <node>… ]}` where each node is
//! either an element
//! `{"id":<int>,"kind":"element","tag":<string>,"attrs":[[<string>,<string>],…],"children":[<int>,…]}`
//! or text `{"id":<int>,"kind":"text","text":<string>}`. The `id`s are arbitrary
//! unique integers used only to express the `children`/`root` links; on rebuild
//! they are renumbered to fresh [`cerberus_dom::NodeId`]s.
//!
//! An element may instead carry an `"innerHTML": <string>` field (and then *no*
//! `children`). That is the wire encoding of a node whose `.innerHTML` was set in
//! JS: rather than parse HTML in JavaScript, we ship the raw fragment string and
//! reparse it in Rust with [`cerberus_dom::parse_html`] at rebuild time (see
//! [`rebuild_document`]). This "deferred reparse" reuses the real Rust parser for
//! the dominant "render this HTML" pattern.
//!
//! # Implemented vs deferred DOM surface
//!
//! This is "real, v2": enough of `document`, element/text nodes, `window`,
//! `navigator`, `location`, storage, and `console` to run typical page
//! bootstraps and reconcile their structural mutations. Selectors now support
//! compound simple selectors plus descendant/child combinators and comma lists
//! (see [`DOM_MODEL_PRELUDE`]); sibling combinators (`~`/`+`) and pseudo-classes
//! are not supported. Layout APIs are stubbed (`getBoundingClientRect` is
//! all-zero) and `style` is store-only (`getComputedStyle` reflects inline
//! values only). See the [`DOM_MODEL_PRELUDE`] docs for the precise list.

mod json;

use cerberus_dom::{Document, DocumentBuilder, NodeId, NodeRef};
use cerberus_js::{JsEngine, JsError, JsValue};
use cerberus_types::{RealmId, Rect};
use json::Json;
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Something went wrong crossing the bridge.
///
/// The three arms separate concerns a caller cares about: a malformed wire
/// document ([`Json`](BridgeError::Json)) or a structurally invalid one
/// ([`Structure`](BridgeError::Structure)) is a bug in *our* serializer/model
/// contract, whereas [`Js`](BridgeError::Js) is an engine/realm-level failure
/// (e.g. no such realm) surfaced from `eval`. Note that a *page script* throwing
/// is deliberately **not** an error here — see [`run_page_scripts`].
#[derive(Debug)]
pub enum BridgeError {
    /// The JSON wire document could not be parsed.
    Json(String),
    /// The engine/realm raised an error while installing the model, running the
    /// fixed bridge evals, or serializing back out.
    Js(JsError),
    /// The wire document parsed but did not match the expected shape (missing
    /// fields, a `children` id with no matching node, a non-string serialize
    /// result, and so on).
    Structure(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Json(m) => write!(f, "DOM wire JSON error: {m}"),
            BridgeError::Js(e) => write!(f, "JS engine error: {e}"),
            BridgeError::Structure(m) => write!(f, "DOM wire structure error: {m}"),
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BridgeError::Js(e) => Some(e),
            _ => None,
        }
    }
}

impl From<JsError> for BridgeError {
    fn from(e: JsError) -> Self {
        BridgeError::Js(e)
    }
}

// ---------------------------------------------------------------------------
// Snapshot: Rust DOM -> wire JSON
// ---------------------------------------------------------------------------

/// Serialize a Rust [`Document`] into the JSON wire format (see the crate docs).
///
/// We walk from [`Document::root`] in source order, emitting one node object per
/// reachable node and reusing each node's existing [`NodeRef::id`] as the wire
/// id. Element attributes and children are preserved in document order. Text
/// node values are escaped by the JSON emitter ([`json::write_json_string`]).
pub fn serialize_document(doc: &Document) -> String {
    let mut out = String::from("{\"root\":");
    let root = doc.root();
    json::write_u64(&mut out, root.id() as u64);
    out.push_str(",\"nodes\":[");
    let mut first = true;
    serialize_node(&mut out, root, &mut first);
    out.push_str("]}");
    out
}

/// Emit `node` (and, depth-first, its descendants) as wire-node objects into
/// `out`. `first` tracks comma placement across the flat `nodes` array.
fn serialize_node(out: &mut String, node: NodeRef<'_>, first: &mut bool) {
    if !*first {
        out.push(',');
    }
    *first = false;

    if node.is_text() {
        out.push_str("{\"id\":");
        json::write_u64(out, node.id() as u64);
        out.push_str(",\"kind\":\"text\",\"text\":");
        json::write_json_string(out, node.text().unwrap_or(""));
        out.push('}');
        return;
    }

    // Element (treat the synthetic `#root`, or anything not text, as an element).
    out.push_str("{\"id\":");
    json::write_u64(out, node.id() as u64);
    out.push_str(",\"kind\":\"element\",\"tag\":");
    json::write_json_string(out, node.tag());
    out.push_str(",\"attrs\":[");
    for (i, (k, v)) in node.attrs().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        json::write_json_string(out, k);
        out.push(',');
        json::write_json_string(out, v);
        out.push(']');
    }
    out.push_str("],\"children\":[");
    for (i, child) in node.children().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::write_u64(out, child.id() as u64);
    }
    out.push_str("]}");

    // Emit the children's own node objects after this one (flat array; order
    // within the array does not matter, only the id links do).
    for child in node.children() {
        serialize_node(out, child, first);
    }
}

// ---------------------------------------------------------------------------
// Rebuild: wire JSON -> Rust DOM
// ---------------------------------------------------------------------------

/// A single decoded wire node, keyed in a map by its wire id.
enum WireNode {
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<u64>,
        /// Raw HTML set via `.innerHTML` in JS, to be reparsed in Rust. When
        /// present it takes precedence over `children` (the JS setter clears the
        /// node's children, so they are empty here anyway).
        inner_html: Option<String>,
    },
    Text {
        text: String,
    },
}

/// A rebuilt [`Document`] paired with the map from JS-model node ids (the wire
/// ids — each live node's stable `__id`) to the fresh [`NodeId`]s in the rebuilt
/// tree.
///
/// The map lets a caller correlate a rendered Rust node back to the live JS node
/// it came from — e.g. to dispatch a DOM event at the element under a click, or
/// to scope a re-render to a changed subtree (M12b/M12c). Nodes introduced by an
/// `innerHTML` reparse have no JS id and are absent from the map. (ADR-0012.)
#[derive(Debug)]
pub struct RebuiltDom {
    /// The reconstructed immutable document.
    pub document: Document,
    /// JS-model id (wire id) → fresh [`NodeId`] in [`RebuiltDom::document`].
    pub id_map: HashMap<u64, NodeId>,
}

/// Parse a JSON wire document and rebuild it into a fresh immutable
/// [`Document`].
///
/// See [`rebuild_document_mapped`] for the variant that also returns the
/// JS-id → [`NodeId`] map ([`RebuiltDom`]).
pub fn rebuild_document(json: &str) -> Result<Document, BridgeError> {
    Ok(rebuild_document_mapped(json)?.document)
}

/// Like [`rebuild_document`], but also returns the JS-id → [`NodeId`] map (see
/// [`RebuiltDom`]).
///
/// The wire ids are arbitrary; we renumber to fresh [`DocumentBuilder`] ids. The
/// builder requires children to exist before their parent, so we perform a
/// **post-order** traversal of the id graph starting at `root`: a node is
/// materialized only after all of its `children` ids have been, and each wire id
/// is mapped to the [`NodeId`] the builder hands back. Cycles and dangling
/// `children` ids are rejected as [`BridgeError::Structure`].
pub fn rebuild_document_mapped(json: &str) -> Result<RebuiltDom, BridgeError> {
    let value = json::parse(json).map_err(BridgeError::Json)?;

    let root_id = value
        .get("root")
        .and_then(Json::as_u64)
        .ok_or_else(|| BridgeError::Structure("missing or non-integer \"root\"".to_string()))?;

    let nodes_json = value
        .get("nodes")
        .and_then(Json::as_array)
        .ok_or_else(|| BridgeError::Structure("missing or non-array \"nodes\"".to_string()))?;

    // Decode every node into a map keyed by its wire id.
    let mut nodes: HashMap<u64, WireNode> = HashMap::with_capacity(nodes_json.len());
    for node in nodes_json {
        let (id, decoded) = decode_wire_node(node)?;
        if nodes.insert(id, decoded).is_some() {
            return Err(BridgeError::Structure(format!("duplicate node id {id}")));
        }
    }

    if !nodes.contains_key(&root_id) {
        return Err(BridgeError::Structure(format!(
            "root id {root_id} has no matching node"
        )));
    }

    // Post-order over the id graph: emit children before parents into the
    // builder, mapping each wire id to its fresh NodeId. An explicit stack keeps
    // this iterative (deep, even pathological, trees won't blow the Rust stack).
    let mut builder = DocumentBuilder::new();
    let mut fresh: HashMap<u64, NodeId> = HashMap::with_capacity(nodes.len());

    // `enter` = first visit (push children), `!enter` = post-visit (materialize).
    let mut stack: Vec<(u64, bool)> = vec![(root_id, true)];
    // Guard against cycles: a node currently on the path to the root.
    let mut on_path: HashMap<u64, ()> = HashMap::new();

    while let Some((id, enter)) = stack.pop() {
        if fresh.contains_key(&id) {
            continue; // already materialized via another parent link
        }
        let node = nodes
            .get(&id)
            .ok_or_else(|| BridgeError::Structure(format!("child id {id} has no matching node")))?;

        if enter {
            if on_path.insert(id, ()).is_some() {
                return Err(BridgeError::Structure(format!(
                    "cycle detected at node id {id}"
                )));
            }
            // Schedule the post-visit, then the children (so children pop first).
            stack.push((id, false));
            if let WireNode::Element { children, .. } = node {
                for &child in children.iter().rev() {
                    if !fresh.contains_key(&child) {
                        stack.push((child, true));
                    }
                }
            }
        } else {
            on_path.remove(&id);
            let new_id = match node {
                WireNode::Text { text } => builder.text(text.clone()),
                WireNode::Element {
                    tag,
                    attrs,
                    children,
                    inner_html,
                } => {
                    // A node carrying `innerHTML` is reparsed in Rust (deferred
                    // reparse): the raw fragment is fed to the real HTML parser
                    // and its body children are grafted in place of `children`
                    // (which the JS setter already cleared).
                    let child_ids: Vec<NodeId> = match inner_html {
                        Some(html) => graft_inner_html(&mut builder, html),
                        None => children
                            .iter()
                            .map(|c| {
                                fresh.get(c).copied().ok_or_else(|| {
                                    BridgeError::Structure(format!(
                                        "child id {c} not materialized before parent {id}"
                                    ))
                                })
                            })
                            .collect::<Result<_, _>>()?,
                    };
                    builder.element_attrs(tag.clone(), attrs.clone(), child_ids)
                }
            };
            fresh.insert(id, new_id);
        }
    }

    let root_fresh = *fresh
        .get(&root_id)
        .expect("root materialized by post-order");
    let document = builder.finish(root_fresh);
    Ok(RebuiltDom {
        document,
        id_map: fresh,
    })
}

/// Reparse an `innerHTML` fragment with [`cerberus_dom::parse_html`] and copy
/// the resulting children into `builder`, returning their fresh [`NodeId`]s (in
/// document order) so the caller can attach them to the node that owned the
/// `innerHTML`.
///
/// `parse_html` wraps its input in a synthetic `#root > html > body` scaffold, so
/// the fragment's real top-level nodes land under `<body>`. We locate that body
/// and graft *its* children; if no `<body>` materialized (e.g. the fragment
/// produced only a `<head>`), we fall back to the parsed root's own children.
fn graft_inner_html(builder: &mut DocumentBuilder, html: &str) -> Vec<NodeId> {
    let parsed = cerberus_dom::parse_html(html);
    let root = parsed.root();
    let graft_parent = find_body(root).unwrap_or(root);
    graft_parent
        .children()
        .map(|child| copy_subtree(builder, child))
        .collect()
}

/// Depth-first search for the first `<body>` element at or below `node`.
fn find_body<'a>(node: NodeRef<'a>) -> Option<NodeRef<'a>> {
    if node.is_element() && node.tag() == "body" {
        return Some(node);
    }
    node.children().find_map(find_body)
}

/// Deep-copy a parsed subtree from a foreign [`Document`] arena into `builder`,
/// returning the new node's [`NodeId`]. Recursive in lock-step with the parsed
/// tree's depth; HTML fragments are shallow in practice, and the parser itself
/// already bounds nesting.
fn copy_subtree(builder: &mut DocumentBuilder, node: NodeRef<'_>) -> NodeId {
    if let Some(text) = node.text() {
        return builder.text(text);
    }
    // Children first (post-order), then the element over their fresh ids.
    let child_ids: Vec<NodeId> = node
        .children()
        .map(|child| copy_subtree(builder, child))
        .collect();
    let attrs: Vec<(String, String)> = node.attrs().to_vec();
    builder.element_attrs(node.tag().to_string(), attrs, child_ids)
}

/// Decode one wire-node JSON object into a [`WireNode`] plus its wire id.
fn decode_wire_node(node: &Json) -> Result<(u64, WireNode), BridgeError> {
    let id = node
        .get("id")
        .and_then(Json::as_u64)
        .ok_or_else(|| BridgeError::Structure("node missing integer \"id\"".to_string()))?;
    let kind = node
        .get("kind")
        .and_then(Json::as_str)
        .ok_or_else(|| BridgeError::Structure(format!("node {id} missing \"kind\"")))?;

    match kind {
        "text" => {
            let text = node
                .get("text")
                .and_then(Json::as_str)
                .ok_or_else(|| BridgeError::Structure(format!("text node {id} missing \"text\"")))?
                .to_string();
            Ok((id, WireNode::Text { text }))
        }
        "element" => {
            let tag = node
                .get("tag")
                .and_then(Json::as_str)
                .ok_or_else(|| BridgeError::Structure(format!("element {id} missing \"tag\"")))?
                .to_string();

            let mut attrs = Vec::new();
            if let Some(arr) = node.get("attrs").and_then(Json::as_array) {
                for pair in arr {
                    let pair = pair.as_array().ok_or_else(|| {
                        BridgeError::Structure(format!("element {id} attr is not a pair array"))
                    })?;
                    let k = pair.first().and_then(Json::as_str).ok_or_else(|| {
                        BridgeError::Structure(format!("element {id} attr key is not a string"))
                    })?;
                    let v = pair.get(1).and_then(Json::as_str).ok_or_else(|| {
                        BridgeError::Structure(format!("element {id} attr value is not a string"))
                    })?;
                    attrs.push((k.to_string(), v.to_string()));
                }
            }

            let mut children = Vec::new();
            if let Some(arr) = node.get("children").and_then(Json::as_array) {
                for c in arr {
                    let c = c.as_u64().ok_or_else(|| {
                        BridgeError::Structure(format!("element {id} child id is not an integer"))
                    })?;
                    children.push(c);
                }
            }

            // `innerHTML`, if present, is the raw fragment to reparse in Rust at
            // graft time. A node carrying it should not also carry children (the
            // JS setter clears them); we tolerate both and prefer `innerHTML`.
            let inner_html = node
                .get("innerHTML")
                .and_then(Json::as_str)
                .map(str::to_string);

            Ok((
                id,
                WireNode::Element {
                    tag,
                    attrs,
                    children,
                    inner_html,
                },
            ))
        }
        other => Err(BridgeError::Structure(format!(
            "node {id} has unknown kind {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The page's ambient environment: the values `window.location`, `navigator`,
/// `window.innerWidth`/`screen`, etc. are derived from. Supplied by the caller
/// (the browser) because the bridge itself has no notion of "which URL" or "how
/// big the viewport is".
///
/// Kept deliberately small and low-entropy — see [`DOM_MODEL_PRELUDE`]'s
/// `navigator` notes on anti-fingerprinting. Per-head fingerprint *farbling* is a
/// separate concern (M6 / ADR-0002's farbling prologue), not this struct.
pub struct PageEnv {
    /// The document's URL, parsed in JS into `location.href`/`protocol`/`host`/…
    pub url: String,
    /// The layout viewport as `(width, height)` in CSS pixels; feeds
    /// `window.innerWidth`/`innerHeight` and `screen.*`.
    pub viewport: (u32, u32),
    /// The User-Agent the network stack presented to this origin. Feeds
    /// `navigator.userAgent` (and the OS-derived `navigator.platform`) so the
    /// script-visible identity matches the request header exactly — including
    /// when the honest-first ladder escalated for this site. Empty falls back to
    /// the honest default inside the prelude.
    pub user_agent: String,
    /// The document's script-readable cookies: the current origin's
    /// **non-HttpOnly** cookies as a `name=value; …` string, seeded into
    /// `document.cookie`. The host excludes HttpOnly cookies here so script can
    /// never read them (they still ride requests through the jar). Empty when
    /// there are none — the default for built-in/cookieless pages.
    pub cookies: String,
}

/// Encode `s` as a JS/JSON string literal (quotes included) suitable for
/// splicing into a `globalThis.__CERBERUS_ENV__ = …` eval. A valid JSON string
/// is also a valid JS string, and [`json::write_json_string`] escapes the quote,
/// backslash, and control characters that would otherwise break out of it.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    json::write_json_string(&mut out, s);
    out
}

/// Run page `<script>`s against a JS document model snapshotted from `document`,
/// and return a fresh Rust [`Document`] reflecting their mutations.
///
/// This is the one-shot composition of the persistent-realm seams below:
/// [`install_page`] → [`run_scripts`] → [`fire_load`] → [`serialize_dom`]. It
/// installs the model, runs the scripts, fires load, and reads the mutated tree
/// back out in a single call — what the one-shot headless [`render`] path and an
/// initial page load want.
///
/// For an **interactive** page (an SPA), the caller instead [`install_page`]s
/// once and then drives the *same* realm across many interactions (events,
/// timers, async resolves), reading each result with [`serialize_dom`], **never
/// re-installing** — re-running install resets the model and discards
/// script-created state (ADR-0012, persistent realm).
///
/// All work goes through `engine.eval(realm, …)`. A script that throws does not
/// abort the run (browsers continue to the next `<script>`); any other engine
/// error (e.g. [`JsError::NoSuchRealm`]) is infrastructure-level and propagates
/// as [`BridgeError::Js`].
/// Resolve a document's scripts to runnable source in document order: inline
/// bodies as-is, external `src`s fetched via `fetch` (skipped when it returns
/// `None` — a blocked or failed fetch) — ADR-0059.
pub fn resolve_scripts(
    scripts: &[cerberus_dom::ScriptRef],
    mut fetch: impl FnMut(&str) -> Option<String>,
) -> Vec<String> {
    scripts
        .iter()
        .filter_map(|s| match s {
            cerberus_dom::ScriptRef::Inline(body) => Some(body.clone()),
            cerberus_dom::ScriptRef::External(src) => fetch(src),
        })
        .collect()
}

pub fn run_page_scripts(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    document: &Document,
    scripts: &[String],
    env: &PageEnv,
) -> Result<Document, BridgeError> {
    install_page(engine, realm, document, env)?;
    // JS is best-effort within a wall-clock budget: a throw or a deadline
    // interrupt stops it but still renders the DOM built so far (ADR-0060).
    engine.set_deadline(JS_BUDGET_MS);
    let _ = run_scripts(engine, realm, scripts);
    let _ = fire_load(engine, realm);
    let _ = run_event_loop(engine, realm, EventLoopBudget::default());
    engine.clear_deadline();
    Ok(serialize_dom(engine, realm)?.document)
}

/// Wall-clock budget (ms) for a page's JavaScript: enough for real progressive
/// enhancement / hydration, but small enough that a heavy or looping page can
/// never hang the render — the DOM built so far is shown instead (ADR-0060).
pub const JS_BUDGET_MS: u64 = 1500;

/// Install the JS document model for `document` into `realm`: the ambient `env`
/// globals, the [`DOM_MODEL_PRELUDE`], and a snapshot of `document`.
///
/// Run this **once per page / navigation**. Afterwards drive the live realm with
/// [`run_scripts`], [`serialize_dom`] (and, later, event dispatch) rather than
/// re-installing: `__cerberusInstallDOM()` resets the model's id counter and node
/// index and rebuilds the tree from the snapshot, discarding any script-created
/// nodes, listeners, and timers — exactly what a persistent, interactive page
/// must *not* do between interactions (ADR-0012).
///
/// `env` is injected before the model installs (the prelude reads it to build
/// `location`/`navigator`/`screen`); the prelude is self-guarding, but a genuine
/// engine/compile failure here is fatal.
pub fn install_page(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    document: &Document,
    env: &PageEnv,
) -> Result<(), BridgeError> {
    // Inject the ambient environment, then install the document model.
    let env_install = format!(
        "globalThis.__CERBERUS_ENV__ = {{ url: {}, width: {}, height: {}, userAgent: {}, cookies: {} }};",
        js_string(&env.url),
        env.viewport.0,
        env.viewport.1,
        js_string(&env.user_agent),
        js_string(&env.cookies),
    );
    engine.eval(realm, &env_install)?;
    engine.eval(realm, DOM_MODEL_PRELUDE)?;

    // Hand it the snapshot and build the JS tree.
    let install = format!(
        "globalThis.__CERBERUS_DOM__ = {}; __cerberusInstallDOM();",
        serialize_document(document)
    );
    engine.eval(realm, &install)?;
    Ok(())
}

/// Evaluate page `scripts` in document order against an already-[`install_page`]d
/// realm.
///
/// **A script that throws does not abort the run** — browsers continue to the
/// next `<script>`, so we swallow [`JsError::Eval`] and keep going. Any *other*
/// engine error (e.g. [`JsError::NoSuchRealm`]) is infrastructure-level and
/// propagates as [`BridgeError::Js`].
pub fn run_scripts(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    scripts: &[String],
) -> Result<(), BridgeError> {
    for script in scripts {
        match engine.eval(realm, script) {
            Ok(_) | Err(JsError::Eval(_)) => {}
            Err(other) => return Err(BridgeError::Js(other)),
        }
    }
    Ok(())
}

/// Fire `DOMContentLoaded` then `load` into an installed realm via
/// `__cerberusFireLoad()` (synchronous, no waiting — speed-first).
///
/// Page-listener errors are swallowed by the model itself; only a realm-level
/// error propagates as [`BridgeError::Js`].
pub fn fire_load(engine: &mut dyn JsEngine, realm: RealmId) -> Result<(), BridgeError> {
    match engine.eval(realm, "__cerberusFireLoad();") {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Caps that guarantee [`run_event_loop`] terminates (ADR-0013). A page is
/// bounded on two axes because the pathological shapes escape different ones: a
/// 0-delay self-rescheduling `setTimeout` never advances the virtual clock (so
/// `max_tasks` stops it), while a `setInterval` advances it every tick (so
/// `max_virtual_ms` stops it).
#[derive(Clone, Copy, Debug)]
pub struct EventLoopBudget {
    /// Maximum macrotasks (timer/rAF/idle callbacks) to run in one drain.
    pub max_tasks: u32,
    /// Maximum virtual time, in ms, a task may be due at and still run.
    pub max_virtual_ms: u64,
}

impl Default for EventLoopBudget {
    fn default() -> Self {
        Self {
            max_tasks: 10_000,
            max_virtual_ms: 60_000,
        }
    }
}

/// What [`run_event_loop`] did, for the timing HUD and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventLoopStats {
    /// Number of macrotasks actually run.
    pub tasks_run: u32,
    /// `true` if the drain stopped on [`EventLoopBudget::max_tasks`] rather than
    /// emptying the queue — a page we deliberately stopped (it may still have
    /// pending timers).
    pub hit_task_cap: bool,
}

/// Drain the realm's macrotask queue (timers / rAF / idle) under `budget`,
/// running **one task per `eval`** so the engine's post-eval job pump interleaves
/// microtasks between macrotasks — the spec ordering (ADR-0013). Returns once the
/// queue is empty within the virtual-clock budget, or the task cap trips.
///
/// Pure `eval` orchestration: it calls `__cerberusStepTimer` (installed by the
/// engine's speed-first prelude). On an engine without it (e.g. the null engine)
/// the stepper eval yields a non-number, so the loop is a safe no-op.
pub fn run_event_loop(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    budget: EventLoopBudget,
) -> Result<EventLoopStats, BridgeError> {
    let step = format!("__cerberusStepTimer({})", budget.max_virtual_ms);
    let mut tasks_run = 0u32;
    while tasks_run < budget.max_tasks {
        match engine.eval(realm, &step) {
            Ok(JsValue::Number(n)) if n >= 1.0 => tasks_run += 1,
            // Empty queue (0) / unexpected value / a stepper throw: stop cleanly.
            Ok(_) | Err(JsError::Eval(_)) => {
                return Ok(EventLoopStats {
                    tasks_run,
                    hit_task_cap: false,
                })
            }
            Err(other) => return Err(BridgeError::Js(other)),
        }
    }
    Ok(EventLoopStats {
        tasks_run,
        hit_task_cap: true,
    })
}

/// Read the realm's **current** live document model back into a fresh Rust
/// [`Document`] (plus the JS-id → [`NodeId`] map, see [`RebuiltDom`]), **without**
/// resetting or re-running anything.
///
/// This is the persistent-realm re-render seam (ADR-0012): after the initial
/// [`install_page`]/[`run_scripts`], an interaction (a dispatched event, a timer
/// callback, an async resolve) mutates the live JS model in place, and this reads
/// the mutated tree out so the app can restyle/relayout/repaint. It evaluates the
/// model's `__cerberusSerializeDOM()` (JS tree → wire JSON) and
/// [`rebuild_document_mapped`]s the result.
///
/// Note the direction: this is the inverse of [`serialize_document`], which turns
/// a Rust [`Document`] into wire JSON for [`install_page`].
pub fn serialize_dom(engine: &mut dyn JsEngine, realm: RealmId) -> Result<RebuiltDom, BridgeError> {
    match engine.eval(realm, "__cerberusSerializeDOM()")? {
        JsValue::Str(s) => rebuild_document_mapped(&s),
        other => Err(BridgeError::Structure(format!(
            "__cerberusSerializeDOM did not return a string: {other:?}"
        ))),
    }
}

/// The outcome of dispatching a DOM event into the live realm (see
/// [`dispatch_event`]).
#[derive(Debug)]
pub struct Dispatched {
    /// `true` if the target node existed and the event was delivered.
    pub dispatched: bool,
    /// `true` if a listener called `preventDefault` on the (cancelable) event —
    /// the caller should then **skip** the browser default action.
    pub default_prevented: bool,
    /// The document re-read after the handlers ran, with its JS-id → [`NodeId`]
    /// map (so the next interaction can still correlate nodes).
    pub dom: RebuiltDom,
}

/// Set a form control's live `value` (the JS `el.value` property) before firing
/// an `input` event, so the handler reads the just-typed text. `node_id` is a JS
/// model id (a key of [`RebuiltDom::id_map`]); a missing node is a safe no-op.
pub fn set_node_value(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    node_id: u64,
    value: &str,
) -> Result<(), BridgeError> {
    let call = format!("__cerberusSetValue({}, {})", node_id, js_string(value));
    match engine.eval(realm, &call) {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Push layout geometry — device-pixel boxes keyed by JS node id — into the live
/// realm so `getBoundingClientRect` returns real rects (ADR-0021). Call after
/// layout; a missing node is a safe no-op. Empty input is a no-op.
pub fn set_geometry(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    boxes: &[(u64, Rect)],
) -> Result<(), BridgeError> {
    if boxes.is_empty() {
        return Ok(());
    }
    let mut json = String::from("{");
    for (i, (id, r)) in boxes.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "\"{id}\":{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            r.x, r.y, r.w, r.h
        ));
    }
    json.push('}');
    let call = format!("__cerberusSetGeometry({json})");
    match engine.eval(realm, &call) {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Push cascaded computed styles — `property → value` maps keyed by JS node id —
/// into the live realm so `getComputedStyle` reflects the cascade, not just
/// inline declarations (ADR-0021). Empty input is a no-op.
pub fn set_computed_styles(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    styles: &[(u64, Vec<(String, String)>)],
) -> Result<(), BridgeError> {
    if styles.is_empty() {
        return Ok(());
    }
    let mut json = String::from("{");
    for (i, (id, props)) in styles.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!("\"{id}\":{{"));
        for (j, (k, v)) in props.iter().enumerate() {
            if j > 0 {
                json.push(',');
            }
            json.push_str(&format!("{}:{}", js_string(k), js_string(v)));
        }
        json.push('}');
    }
    json.push('}');
    let call = format!("__cerberusSetComputedStyles({json})");
    match engine.eval(realm, &call) {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Dispatch a DOM `event_type` at the live node identified by `node_id` (a JS
/// model id — a key of [`RebuiltDom::id_map`]), run its listeners through the
/// target and bubbling phases, then read the mutated model back out.
///
/// `event_init_json` is a JSON **object literal** of extra event fields (e.g.
/// `{"key":"Enter"}`, `{"button":0}`, or `{"bubbles":false}`); pass `"{}"` for
/// none. The realm must already be [`install_page`]d. Inspect
/// [`Dispatched::default_prevented`] to decide whether to still perform the
/// browser default action (link navigation, form submit, …).
pub fn dispatch_event(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    node_id: u64,
    event_type: &str,
    event_init_json: &str,
) -> Result<Dispatched, BridgeError> {
    let call = format!(
        "__cerberusDispatch({}, {}, {})",
        node_id,
        js_string(event_type),
        event_init_json
    );
    let (dispatched, default_prevented) = match engine.eval(realm, &call)? {
        JsValue::Str(s) => parse_dispatch_result(&s)?,
        other => {
            return Err(BridgeError::Structure(format!(
                "__cerberusDispatch did not return a string: {other:?}"
            )))
        }
    };
    // A handler may have scheduled timers / microtasks (e.g. a debounced state
    // update); drain them before reading the DOM back so the result reflects them.
    run_event_loop(engine, realm, EventLoopBudget::default())?;
    let dom = serialize_dom(engine, realm)?;
    Ok(Dispatched {
        dispatched,
        default_prevented,
        dom,
    })
}

/// Decode the `{dispatched, defaultPrevented}` blob from `__cerberusDispatch`.
/// The values are wire integers (1/0) because the bridge's JSON has no boolean
/// type (see [`mod@json`]); absent/garbage fields decode as `false`.
fn parse_dispatch_result(s: &str) -> Result<(bool, bool), BridgeError> {
    let v = json::parse(s).map_err(BridgeError::Json)?;
    let dispatched = v.get("dispatched").and_then(Json::as_u64).unwrap_or(0) != 0;
    let default_prevented = v
        .get("defaultPrevented")
        .and_then(Json::as_u64)
        .unwrap_or(0)
        != 0;
    Ok((dispatched, default_prevented))
}

// ---------------------------------------------------------------------------
// fetch() — enqueue + host-drain + resolve (ADR-0014)
// ---------------------------------------------------------------------------

/// One request a page script asked for via `fetch(input, init)`, drained out of
/// the realm by [`take_fetches`].
///
/// `fetch()` never calls native code (the engine seam is eval-only): it pushes a
/// descriptor like this onto a per-realm queue and returns a `Promise`. The host
/// drains the queue, performs each request through a [`FetchClient`], and settles
/// the Promise with [`resolve_fetch`] / [`reject_fetch`]. The `id` correlates the
/// descriptor with its stashed resolver across that round-trip.
///
/// `headers` is the request header list in insertion order (the JS side
/// normalizes a plain object / array-of-pairs / `Headers` into `[name, value]`
/// strings). `body` is the request body as a UTF-8 text string (`""` when none);
/// binary bodies are out of scope for v1.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// The per-realm monotonic id keying this request's pending Promise.
    pub id: u64,
    /// The request URL (`String(input)`, or `input.url` for a Request-like).
    pub url: String,
    /// The HTTP method, upper-cased (`"GET"` when none supplied).
    pub method: String,
    /// Request headers in insertion order, as `(name, value)` strings.
    pub headers: Vec<(String, String)>,
    /// The request body as a UTF-8 text string; empty when none.
    pub body: String,
}

/// A response the host produced for a [`FetchRequest`], handed back to JS by
/// [`resolve_fetch`] to settle the page's Promise with a `Response`.
///
/// The JS `Response` derives `ok` (`200..=299`) and `redirected` itself, so this
/// carries no booleans — keeping it on the wire-JSON's integer-only diet (see
/// [`mod@json`]). `body` is the response body as a UTF-8 text string (v1: `text()`
/// returns it verbatim, `json()` `JSON.parse`s it).
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// The HTTP status code (e.g. `200`, `404`).
    pub status: u16,
    /// The HTTP status text (e.g. `"OK"`); may be empty.
    pub status_text: String,
    /// The final response URL (after any redirects the client followed).
    pub url: String,
    /// Response headers in order, as `(name, value)` strings.
    pub headers: Vec<(String, String)>,
    /// The response body as a UTF-8 text string.
    pub body: String,
}

/// The host's network seam: turn a [`FetchRequest`] into a [`FetchResponse`].
///
/// [`drive_fetches`] calls this once per drained request. An `Err(String)`
/// rejects the page's Promise with a `TypeError` carrying that message (the
/// browser's "network error" shape); an `Ok` resolves it with a `Response`. The
/// implementor owns *all* policy — DNS, TLS, redirects, CORS, caching, timeouts —
/// the bridge only marshals the request and response across the `eval` seam.
pub trait FetchClient {
    /// Perform `req`, returning the response or a network-error message.
    fn fetch(&mut self, req: &FetchRequest) -> Result<FetchResponse, String>;
}

/// Caps that guarantee [`drive_fetches`] terminates even when `.then` callbacks
/// keep issuing more `fetch`es.
///
/// The pump alternates "drain the event loop" with "drain the fetch queue", so a
/// page that schedules a fresh request from every response would spin forever.
/// `max_rounds` bounds how many drain rounds we make; `max_requests` bounds the
/// total requests serviced across the whole pump. Either tripping sets
/// [`FetchStats::hit_cap`].
#[derive(Clone, Copy, Debug)]
pub struct FetchBudget {
    /// Maximum number of drain rounds (each round services one batch of the
    /// queue, then re-runs the event loop).
    pub max_rounds: u32,
    /// Maximum total requests serviced across the whole pump.
    pub max_requests: u32,
}

impl Default for FetchBudget {
    fn default() -> Self {
        Self {
            max_rounds: 50,
            max_requests: 1000,
        }
    }
}

/// What [`drive_fetches`] did, for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Total requests serviced (resolved *or* rejected) across all rounds.
    pub requests: u32,
    /// Number of drain rounds that serviced at least one request.
    pub rounds: u32,
    /// `true` if a [`FetchBudget`] cap (rounds or requests) stopped the pump
    /// rather than the queue draining naturally.
    pub hit_cap: bool,
}

/// Drain the realm's pending `fetch` queue into Rust, returning the descriptors
/// and clearing the queue.
///
/// Drain the `document.cookie = …` assignments a script made this run, raw (with
/// attributes), so the host can apply each to the real consent-gated jar as a
/// `Set-Cookie`. Empty when the page set no cookies from script. Non-string
/// entries are skipped rather than failing the drain.
pub fn take_cookie_writes(
    engine: &mut dyn JsEngine,
    realm: RealmId,
) -> Result<Vec<String>, BridgeError> {
    let json = match engine.eval(realm, "__cerberusTakeCookieWrites()")? {
        JsValue::Str(s) => s,
        other => {
            return Err(BridgeError::Structure(format!(
                "__cerberusTakeCookieWrites did not return a string: {other:?}"
            )))
        }
    };
    let value = json::parse(&json).map_err(BridgeError::Json)?;
    let items = value.as_array().ok_or_else(|| {
        BridgeError::Structure("__cerberusTakeCookieWrites did not return an array".to_string())
    })?;
    Ok(items
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect())
}

/// Evals `__cerberusTakeFetches()` (which `JSON.stringify`s the queue then empties
/// it) and parses the array of `{id,url,method,headers:[[n,v]…],body}` objects. An
/// empty queue yields an empty `Vec`. A malformed entry is skipped rather than
/// failing the whole drain — a single bad descriptor must not strand the rest.
pub fn take_fetches(
    engine: &mut dyn JsEngine,
    realm: RealmId,
) -> Result<Vec<FetchRequest>, BridgeError> {
    let json = match engine.eval(realm, "__cerberusTakeFetches()")? {
        JsValue::Str(s) => s,
        other => {
            return Err(BridgeError::Structure(format!(
                "__cerberusTakeFetches did not return a string: {other:?}"
            )))
        }
    };
    let value = json::parse(&json).map_err(BridgeError::Json)?;
    let items = value.as_array().ok_or_else(|| {
        BridgeError::Structure("__cerberusTakeFetches did not return an array".to_string())
    })?;

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // A descriptor missing its id is unusable (we could never settle its
        // Promise), so skip it; other fields fall back to sane defaults.
        let id = match item.get("id").and_then(Json::as_u64) {
            Some(id) => id,
            None => continue,
        };
        let url = item
            .get("url")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        let method = item
            .get("method")
            .and_then(Json::as_str)
            .unwrap_or("GET")
            .to_string();
        let body = item
            .get("body")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        out.push(FetchRequest {
            id,
            url,
            method,
            headers: decode_header_pairs(item.get("headers")),
            body,
        });
    }
    Ok(out)
}

/// Drain `<script src>` elements inserted into the DOM by script (webpack/Next
/// chunk loading, analytics, lazy widgets) as `(id, url)` pairs to fetch + run on
/// the host, then fire each script's `load` event (ADR-0060).
pub fn take_script_loads(
    engine: &mut dyn JsEngine,
    realm: RealmId,
) -> Result<Vec<(u64, String)>, BridgeError> {
    let json = match engine.eval(realm, "__cerberusTakeScriptLoads()")? {
        JsValue::Str(s) => s,
        _ => return Ok(Vec::new()),
    };
    let value = json::parse(&json).map_err(BridgeError::Json)?;
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (Some(id), Some(url)) = (
            item.get("id").and_then(Json::as_u64),
            item.get("url").and_then(Json::as_str),
        ) else {
            continue;
        };
        out.push((id, url.to_string()));
    }
    Ok(out)
}

/// Decode a wire `headers` value (`[[name, value], …]`) into a `(name, value)`
/// list. Missing/garbage entries are skipped; a `None` field yields an empty list.
fn decode_header_pairs(headers: Option<&Json>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(arr) = headers.and_then(Json::as_array) {
        for pair in arr {
            let Some(pair) = pair.as_array() else {
                continue;
            };
            let name = pair.first().and_then(Json::as_str);
            let value = pair.get(1).and_then(Json::as_str);
            if let (Some(name), Some(value)) = (name, value) {
                out.push((name.to_string(), value.to_string()));
            }
        }
    }
    out
}

/// Emit a `FetchResponse` as a JS object literal into `out`, in the wire shape
/// `__cerberusResolveFetch` expects: `{status:<int>,statusText,url,headers:[[n,v]…],body}`.
///
/// Mirrors [`serialize_document`]'s emitter style — `status` as a bare integer,
/// every string through [`json::write_json_string`], headers as an array of
/// two-element arrays. No booleans cross (`ok`/`redirected` are computed in JS).
fn write_response_literal(out: &mut String, resp: &FetchResponse) {
    out.push_str("{\"status\":");
    json::write_u64(out, resp.status as u64);
    out.push_str(",\"statusText\":");
    json::write_json_string(out, &resp.status_text);
    out.push_str(",\"url\":");
    json::write_json_string(out, &resp.url);
    out.push_str(",\"headers\":[");
    for (i, (name, value)) in resp.headers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        json::write_json_string(out, name);
        out.push(',');
        json::write_json_string(out, value);
        out.push(']');
    }
    out.push_str("],\"body\":");
    json::write_json_string(out, &resp.body);
    out.push('}');
}

/// Settle the pending Promise for `req_id` with `resp` (resolving it with a
/// `Response`), via `__cerberusResolveFetch`.
///
/// Builds the response object literal ([`write_response_literal`]) and evals the
/// settle call. Resolving schedules the page's `.then` microtasks, which the next
/// [`run_event_loop`] in [`drive_fetches`] drains. An unknown id is a JS-side
/// no-op; a script-level throw is swallowed (only a realm-level error propagates).
pub fn resolve_fetch(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    req_id: u64,
    resp: &FetchResponse,
) -> Result<(), BridgeError> {
    let mut call = String::from("__cerberusResolveFetch(");
    json::write_u64(&mut call, req_id);
    call.push_str(", ");
    write_response_literal(&mut call, resp);
    call.push(')');
    match engine.eval(realm, &call) {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Reject the pending Promise for `req_id` with a `TypeError(message)` (the
/// browser's network-error shape), via `__cerberusRejectFetch`.
///
/// Rejecting schedules the page's `.catch`/rejection-`.then` microtasks, drained
/// by the next [`run_event_loop`] in [`drive_fetches`]. An unknown id is a JS-side
/// no-op; a script-level throw is swallowed (only a realm-level error propagates).
pub fn reject_fetch(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    req_id: u64,
    message: &str,
) -> Result<(), BridgeError> {
    let call = format!("__cerberusRejectFetch({}, {})", req_id, js_string(message));
    match engine.eval(realm, &call) {
        Ok(_) | Err(JsError::Eval(_)) => Ok(()),
        Err(other) => Err(BridgeError::Js(other)),
    }
}

/// Pump the realm's `fetch` queue to quiescence: drain the event loop, service
/// every queued request through `client`, and repeat until no new requests
/// appear — all under `loop_budget` (per-drain timer/microtask caps) and
/// `fetch_budget` (round/request caps that guarantee termination).
///
/// Each round first [`run_event_loop`]s (so a `fetch` deferred behind a timer, or
/// a `.then` from a previous round, lands in the queue), then [`take_fetches`]es
/// and services the batch with [`resolve_fetch`] / [`reject_fetch`]. Servicing a
/// response schedules more `.then` microtasks — and possibly more `fetch`es —
/// which the *next* round's event-loop drain surfaces. The pump stops when the
/// queue drains naturally, or a budget cap trips (setting [`FetchStats::hit_cap`];
/// any requests still queued at the request cap are rejected with
/// `"fetch budget exceeded"` so their Promises never dangle). A final
/// [`run_event_loop`] drains the microtasks from the last batch of responses.
pub fn drive_fetches(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    client: &mut dyn FetchClient,
    loop_budget: EventLoopBudget,
    fetch_budget: FetchBudget,
) -> Result<FetchStats, BridgeError> {
    let mut rounds = 0u32;
    let mut requests = 0u32;
    let mut hit_cap = false;
    // The JS interrupt bounds eval time, but the per-request *network* time isn't
    // JS — a page pulling many chunks could still take many seconds. A wall-clock
    // deadline on the whole drain keeps the render fast (ADR-0060).
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(JS_BUDGET_MS);

    'pump: loop {
        if std::time::Instant::now() >= deadline {
            hit_cap = true;
            break;
        }
        // Drain timers + microtasks so any pending/just-scheduled fetch enqueues.
        run_event_loop(engine, realm, loop_budget)?;

        if rounds >= fetch_budget.max_rounds {
            hit_cap = true;
            break;
        }
        let reqs = take_fetches(engine, realm)?;
        // Scripts injected into the DOM (webpack/Next chunk loading) are fetched
        // + run here, in the same drain loop, then their `load` events fire so the
        // loader continues — what lets client-rendered pages boot (ADR-0060).
        let script_loads = take_script_loads(engine, realm)?;
        if reqs.is_empty() && script_loads.is_empty() {
            break;
        }
        rounds += 1;

        for req in reqs {
            if requests >= fetch_budget.max_requests || std::time::Instant::now() >= deadline {
                // Out of request budget (or wall-clock): reject this and every
                // remaining queued request so no Promise is left dangling, then
                // stop the pump.
                hit_cap = true;
                reject_fetch(engine, realm, req.id, "fetch budget exceeded")?;
                for rest in take_fetches(engine, realm)? {
                    reject_fetch(engine, realm, rest.id, "fetch budget exceeded")?;
                }
                break 'pump;
            }
            requests += 1;
            match client.fetch(&req) {
                Ok(resp) => resolve_fetch(engine, realm, req.id, &resp)?,
                Err(message) => reject_fetch(engine, realm, req.id, &message)?,
            }
        }

        for (id, url) in script_loads {
            if requests >= fetch_budget.max_requests || std::time::Instant::now() >= deadline {
                hit_cap = true;
                let _ = engine.eval(realm, &format!("__cerberusScriptLoaded({id},0)"));
                continue;
            }
            requests += 1;
            let req = FetchRequest {
                id,
                url,
                method: "GET".to_string(),
                headers: Vec::new(),
                body: String::new(),
            };
            match client.fetch(&req) {
                Ok(resp) => {
                    // Run the fetched module in the realm, then fire its load.
                    let _ = engine.eval(realm, &resp.body);
                    let _ = engine.eval(realm, &format!("__cerberusScriptLoaded({id},1)"));
                }
                Err(_) => {
                    let _ = engine.eval(realm, &format!("__cerberusScriptLoaded({id},0)"));
                }
            }
        }
    }

    // Final drain: run the microtasks scheduled by the last batch of responses.
    run_event_loop(engine, realm, loop_budget)?;
    Ok(FetchStats {
        requests,
        rounds,
        hit_cap,
    })
}

/// Like [`run_page_scripts`], but with `fetch` support: after firing load it
/// [`drive_fetches`]es the realm against `client` so the page's `fetch` calls (and
/// the `.then` chains they unlock) run to quiescence before the DOM is read back.
///
/// Composition: [`install_page`] → [`run_scripts`] → [`fire_load`] →
/// [`drive_fetches`] → [`serialize_dom`]. Uses default [`EventLoopBudget`] and
/// [`FetchBudget`] caps. [`run_page_scripts`] is left untouched for callers that
/// have no network seam (it runs the event loop but never drains fetches, so a
/// page's `fetch` Promises simply never settle there).
pub fn run_page_scripts_with_fetch(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    document: &Document,
    scripts: &[String],
    env: &PageEnv,
    client: &mut dyn FetchClient,
) -> Result<Document, BridgeError> {
    install_page(engine, realm, document, env)?;
    // Best-effort JS within a wall-clock budget so a heavy/looping page (or one
    // pulling many chunks) can never hang the render — the DOM so far is shown
    // (ADR-0060). Cleared before serialization so reading the DOM back isn't cut.
    engine.set_deadline(JS_BUDGET_MS);
    let _ = run_scripts(engine, realm, scripts);
    let _ = fire_load(engine, realm);
    let _ = drive_fetches(
        engine,
        realm,
        client,
        EventLoopBudget::default(),
        FetchBudget::default(),
    );
    engine.clear_deadline();
    Ok(serialize_dom(engine, realm)?.document)
}

// ---------------------------------------------------------------------------
// The JS document model
// ---------------------------------------------------------------------------

/// The JavaScript document model, evaluated into a realm before any page script.
///
/// A single self-contained, defensively-guarded string (the same shape as
/// `cerberus-js-quickjs`'s `SPEED_FIRST_PRELUDE`). It installs `document`,
/// `window`, and `console`, plus the bridge entry points
/// (`__cerberusInstallDOM`, `__cerberusFireLoad`, `__cerberusSerializeDOM`).
/// Install and serialize internals are wrapped so the model never throws while
/// snapshotting or reconciling.
///
/// # Implemented
///
/// * **`document`**: `getElementById`, `querySelector`/`querySelectorAll`/
///   `matches`/`closest` (the v2 selector grammar below),
///   `getElementsByTagName`, `getElementsByClassName`, `createElement`,
///   `createTextNode`, `body`/`head`/`documentElement`, `title` (get/set),
///   `addEventListener`/`removeEventListener`, `readyState`
///   (`"loading"` → `"complete"`), `cookie` (in-memory get/set),
///   `location`/`URL`/`documentURI` (from [`PageEnv::url`]).
/// * **element / text nodes**: `nodeType`, `nodeName`/`tagName`, `textContent`
///   (get concatenates descendant text; set replaces children with one text
///   node), `innerHTML`/`outerHTML` (get serializes to HTML in JS; set stores a
///   raw fragment reparsed in Rust — see below), `insertAdjacentHTML`,
///   `getAttribute`/`setAttribute`/`removeAttribute`/`hasAttribute`/
///   `getAttributeNames`, `id`, `className`, `classList`
///   (`add`/`remove`/`toggle`/`contains`/`length`), `children`/`childNodes`,
///   `parentNode`/`parentElement`, `firstChild`/`lastChild`/`nextSibling`/
///   `previousSibling`, `appendChild`/`removeChild`/`insertBefore`/`remove`, a
///   store-only `style`, `getBoundingClientRect` (all-zero), scoped
///   `querySelector`/`querySelectorAll`/`matches`/`closest`.
/// * **`window`** = `globalThis`, with `window.document`,
///   `addEventListener`/`removeEventListener` (load events fired by fire-load),
///   `location`, `navigator`, `screen`, `history`, `localStorage`/
///   `sessionStorage`, `innerWidth`/`innerHeight`, `getComputedStyle`,
///   `matchMedia`.
/// * **`console`**: `log`/`warn`/`error`/`info`/`debug` push joined `String(arg)`
///   messages into `globalThis.__cerberusConsole`; never throw.
///
/// # Selector grammar
///
/// The selector engine supports a *selector list* (comma-separated) of
/// *complex* selectors, where a complex selector is a sequence of *compound*
/// selectors joined by the descendant (whitespace) or child (`>`) combinator. A
/// compound selector is a tag (or `*`) and/or any number of `.class` and/or `#id`
/// parts, plus optional attribute selectors `[name]` / `[name="value"]`, all of
/// which must match one element. **Not** supported (documented in the prelude):
/// sibling combinators `~`/`+`, pseudo-classes/elements (`:hover`, `::before`),
/// `>` at the start, and namespaces.
///
/// # `innerHTML` — deferred reparse
///
/// The `innerHTML` *setter* does not parse HTML in JS. It records the raw
/// fragment string on the node and drops the node's JS children; the node is
/// serialized with an `"innerHTML"` field and the fragment is reparsed by the
/// real Rust parser at [`rebuild_document`] time. **Limitation:** because the
/// children are not re-parsed *in JS*, reading them back (`el.children`,
/// `el.firstChild`, a follow-up `querySelector` into the fragment) mid-script is
/// not supported after a set; the `innerHTML` *getter* on such a node returns the
/// stored raw string. This covers the dominant "render this HTML" pattern.
///
/// # Anti-fingerprinting
///
/// `navigator` is deliberately low-entropy and identical for every head (fixed
/// generic `userAgent`, `en-US`, no plugins/`mediaDevices`/WebGL). Per-head
/// fingerprint *farbling* is M6 (ADR-0002's farbling prologue), not here.
pub const DOM_MODEL_PRELUDE: &str = r##"
(function () {
  try {
    var g = globalThis;

    // ---- console (capture, never throw) --------------------------------
    if (!Array.isArray(g.__cerberusConsole)) g.__cerberusConsole = [];
    function consoleSink() {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) {
        try { parts.push(String(arguments[i])); } catch (e) { parts.push(""); }
      }
      try { g.__cerberusConsole.push(parts.join(" ")); } catch (e) {}
    }
    g.console = {
      log: consoleSink, warn: consoleSink, error: consoleSink,
      info: consoleSink, debug: consoleSink,
    };

    // ---- node model ----------------------------------------------------
    // Every node is a plain object with a numeric __id, a __type (1 element /
    // 3 text), and tree links. Elements carry an ordered attribute list
    // (__attrs: array of [name, value]) and child list (__kids). Text nodes
    // carry __text. We keep ordered arrays (not Maps) so serialization is
    // deterministic and matches insertion order.

    var ELEMENT_NODE = 1;
    var TEXT_NODE = 3;

    var idCounter = 1;          // fresh-id source for nodes created at runtime
    var byId = Object.create(null);

    function freshId() {
      var n = idCounter++;
      // Skip any id already taken by the snapshot.
      while (byId[n]) n = idCounter++;
      return n;
    }

    function indexNode(node) { if (node && typeof node.__id === "number") byId[node.__id] = node; }

    // ---- attribute helpers ---------------------------------------------
    function attrIndex(el, name) {
      var a = el.__attrs;
      for (var i = 0; i < a.length; i++) if (a[i][0] === name) return i;
      return -1;
    }
    function getAttr(el, name) {
      var i = attrIndex(el, name);
      return i === -1 ? null : a_value(el, i);
    }
    function a_value(el, i) { return el.__attrs[i][1]; }
    function setAttr(el, name, value) {
      var v = String(value);
      var i = attrIndex(el, name);
      var old = (i === -1) ? null : el.__attrs[i][1];
      if (i === -1) el.__attrs.push([name, v]); else el.__attrs[i][1] = v;
      __moEmitAttr(el, name, old);
    }
    function removeAttr(el, name) {
      var i = attrIndex(el, name);
      if (i !== -1) { var old = el.__attrs[i][1]; el.__attrs.splice(i, 1); __moEmitAttr(el, name, old); }
    }

    // ---- classList -----------------------------------------------------
    function classTokens(el) {
      var c = getAttr(el, "class");
      if (!c) return [];
      return c.split(/\s+/).filter(function (t) { return t.length > 0; });
    }
    function writeClass(el, tokens) {
      if (tokens.length === 0) removeAttr(el, "class");
      else setAttr(el, "class", tokens.join(" "));
    }
    function makeClassList(el) {
      return {
        get length() { return classTokens(el).length; },
        contains: function (t) { return classTokens(el).indexOf(t) !== -1; },
        add: function () {
          var toks = classTokens(el);
          for (var i = 0; i < arguments.length; i++) {
            var t = String(arguments[i]);
            if (t && toks.indexOf(t) === -1) toks.push(t);
          }
          writeClass(el, toks);
        },
        remove: function () {
          var toks = classTokens(el);
          for (var i = 0; i < arguments.length; i++) {
            var t = String(arguments[i]);
            var k = toks.indexOf(t);
            if (k !== -1) toks.splice(k, 1);
          }
          writeClass(el, toks);
        },
        toggle: function (t, force) {
          t = String(t);
          var toks = classTokens(el);
          var has = toks.indexOf(t) !== -1;
          var want = (force === undefined) ? !has : !!force;
          if (want && !has) toks.push(t);
          else if (!want && has) toks.splice(toks.indexOf(t), 1);
          writeClass(el, toks);
          return want;
        },
        item: function (i) { return classTokens(el)[i] || null; },
        toString: function () { return getAttr(el, "class") || ""; },
      };
    }

    // ---- tree mutation -------------------------------------------------
    function detach(node) {
      var p = node.__parent;
      if (!p) return;
      var k = p.__kids.indexOf(node);
      if (k !== -1) p.__kids.splice(k, 1);
      node.__parent = null;
    }
    function clearRaw(node) {
      // Inserting/removing real children supersedes a pending innerHTML string:
      // a node holds EITHER a raw fragment OR live children, never both.
      if (typeof node.__rawHTML === "string") node.__rawHTML = undefined;
    }
    // A <script src> inserted into the tree by script (webpack/Next chunk
    // loading, analytics, lazy widgets) is queued for the host to fetch + run on
    // the next fetch drain, then its `load` fires (ADR-0060). `src` may be a
    // property (`script.src = url`) or an attribute.
    function maybeLoadScript(node) {
      if (!node || node.__type !== ELEMENT_NODE || node.__tag !== "script" || node.__cbLoaded) {
        return;
      }
      var src = node.src || getAttr(node, "src");
      if (!src) return;
      node.__cbLoaded = true;
      var id = g.__cerberusScriptId++;
      g.__cerberusScriptPending[id] = node;
      g.__cerberusScriptQueue.push({ id: id, url: String(src) });
    }
    function appendChild(parent, node) {
      detach(node);
      clearRaw(parent);
      parent.__kids.push(node);
      node.__parent = parent;
      maybeLoadScript(node);
      __moEmitChildList(parent, [node], null);
      return node;
    }
    function insertBefore(parent, node, ref) {
      if (ref == null) return appendChild(parent, node);
      detach(node);
      clearRaw(parent);
      var i = parent.__kids.indexOf(ref);
      if (i === -1) { parent.__kids.push(node); }
      else { parent.__kids.splice(i, 0, node); }
      node.__parent = parent;
      maybeLoadScript(node);
      __moEmitChildList(parent, [node], null);
      return node;
    }
    function removeChild(parent, node) {
      var k = parent.__kids.indexOf(node);
      if (k !== -1) { parent.__kids.splice(k, 1); node.__parent = null; __moEmitChildList(parent, null, [node]); }
      return node;
    }

    // ---- MutationObserver (real) ---------------------------------------
    // Real childList/attributes records, delivered as a microtask, so sensors and
    // frameworks that observe(...) actually see DOM changes (the prior stub was a
    // no-op). Hooked into the shared mutation helpers above; characterData and
    // node-move (detach) records are minimal. Overrides the speed-first no-op.
    var __moObservers = [];
    var __moScheduled = false;
    function __moSchedule() {
      if (__moScheduled) return;
      __moScheduled = true;
      var run = function () {
        __moScheduled = false;
        var obs = __moObservers.slice();
        for (var i = 0; i < obs.length; i++) {
          var o = obs[i];
          if (o.queue.length) {
            var recs = o.queue; o.queue = [];
            try { o.cb.call(o.instance, recs, o.instance); } catch (e) {}
          }
        }
      };
      if (typeof g.queueMicrotask === "function") { g.queueMicrotask(run); }
      else { Promise.resolve().then(run); }
    }
    function __moObserved(o, target) {
      if (o.target === target) return true;
      if (!o.opts.subtree) return false;
      var n = target.__parent;
      while (n) { if (n === o.target) return true; n = n.__parent; }
      return false;
    }
    function __moEmitChildList(parent, added, removed) {
      for (var i = 0; i < __moObservers.length; i++) {
        var o = __moObservers[i];
        if (o.opts.childList && __moObserved(o, parent)) {
          o.queue.push({ type: "childList", target: parent, attributeName: null,
            oldValue: null, addedNodes: added || [], removedNodes: removed || [] });
          __moSchedule();
        }
      }
    }
    function __moEmitAttr(el, name, oldValue) {
      for (var i = 0; i < __moObservers.length; i++) {
        var o = __moObservers[i];
        if (!o.opts.attributes || !__moObserved(o, el)) continue;
        if (o.opts.attributeFilter && o.opts.attributeFilter.indexOf(name) === -1) continue;
        o.queue.push({ type: "attributes", target: el, attributeName: name,
          oldValue: o.opts.attributeOldValue ? oldValue : null, addedNodes: [], removedNodes: [] });
        __moSchedule();
      }
    }
    g.MutationObserver = function MutationObserver(cb) { this.__cb = cb; this.__rec = null; };
    g.MutationObserver.prototype.observe = function (target, opts) {
      opts = opts || {};
      if (this.__rec) { this.__rec.target = target; this.__rec.opts = opts; return; }
      this.__rec = { target: target, opts: opts, cb: this.__cb, instance: this, queue: [] };
      __moObservers.push(this.__rec);
    };
    g.MutationObserver.prototype.disconnect = function () {
      var idx = __moObservers.indexOf(this.__rec);
      if (idx !== -1) __moObservers.splice(idx, 1);
      if (this.__rec) this.__rec.queue = [];
    };
    g.MutationObserver.prototype.takeRecords = function () {
      if (!this.__rec) return [];
      var q = this.__rec.queue; this.__rec.queue = []; return q;
    };

    function elementChildren(node) {
      return node.__kids.filter(function (c) { return c.__type === ELEMENT_NODE; });
    }

    // ---- textContent ---------------------------------------------------
    function collectText(node, acc) {
      if (node.__type === TEXT_NODE) { acc.push(node.__text); return; }
      for (var i = 0; i < node.__kids.length; i++) collectText(node.__kids[i], acc);
    }

    function walkElements(root, fn) {
      // Pre-order over elements, excluding `root` itself unless caller adds it.
      var kids = root.__kids;
      for (var i = 0; i < kids.length; i++) {
        var c = kids[i];
        if (c.__type === ELEMENT_NODE) { fn(c); walkElements(c, fn); }
      }
    }

    // ---- selector engine (compound + descendant/child + comma lists) ---
    // A selector list is parsed once into an array of "complex" selectors. A
    // complex selector is an array of steps [{combinator, compound}, …] read
    // left→right, where `compound` is { tag, id, classes[], attrs[] } and
    // `combinator` is how this compound relates to the PRECEDING one:
    //   " " descendant, ">" child, "" (only on the first step) the subject.
    // Matching is anchored on the rightmost (subject) compound and walks back
    // through ancestors/parents, so we never need sibling links here.
    //
    // Unsupported (by design, speed-first): sibling combinators `~`/`+`,
    // pseudo-classes/elements (`:hover`, `::before`), leading `>`, namespaces.
    // Attribute selectors are limited to `[name]` and `[name="value"]` /
    // `[name='value']` (presence and exact-match; no `~=`, `^=`, `*=`, …).
    function parseCompound(text) {
      // text is one compound run with no whitespace/combinators, e.g.
      // `div.foo#bar[data-x="1"]`. Returns null if it is empty/garbage.
      var compound = { tag: null, id: null, classes: [], attrs: [], pseudos: [] };
      var STOP = ".#[:";
      var i = 0, n = text.length, sawAny = false;
      while (i < n) {
        var ch = text.charAt(i);
        if (ch === "#") {
          i++; var s = i; while (i < n && !STOP.includes(text.charAt(i))) i++;
          compound.id = text.slice(s, i); sawAny = true;
        } else if (ch === ".") {
          i++; var s2 = i; while (i < n && !STOP.includes(text.charAt(i))) i++;
          if (i > s2) { compound.classes.push(text.slice(s2, i)); sawAny = true; }
        } else if (ch === "[") {
          var end = text.indexOf("]", i);
          if (end === -1) return null;            // unterminated → no match
          var body = text.slice(i + 1, end).trim();
          i = end + 1;
          var eq = body.indexOf("=");
          if (eq === -1) {
            if (body) { compound.attrs.push({ name: body, op: null, value: null }); sawAny = true; }
          } else {
            var op = "=", nameEnd = eq, opc = body.charAt(eq - 1);
            if ("~^$*|".indexOf(opc) !== -1) { op = opc + "="; nameEnd = eq - 1; }
            var an = body.slice(0, nameEnd).trim();
            var av = body.slice(eq + 1).trim();
            if (av.length >= 2 && (av.charAt(0) === '"' || av.charAt(0) === "'")) av = av.slice(1, -1);
            if (an) { compound.attrs.push({ name: an, op: op, value: av }); sawAny = true; }
          }
        } else if (ch === ":") {
          i++; if (text.charAt(i) === ":") i++;   // pseudo-element ::x → treat as a (non-matching) pseudo
          var ps = i; while (i < n && /[a-zA-Z\-]/.test(text.charAt(i))) i++;
          var pname = text.slice(ps, i).toLowerCase(), parg = null;
          if (text.charAt(i) === "(") {
            var depth = 1; i++; var as = i;
            while (i < n && depth > 0) { var pc = text.charAt(i); if (pc === "(") depth++; else if (pc === ")") depth--; if (depth > 0) i++; }
            parg = text.slice(as, i); i++;
          }
          compound.pseudos.push({ name: pname, arg: parg }); sawAny = true;
        } else {
          // A type (tag) selector or universal `*`; runs until the next part.
          var s3 = i; while (i < n && !STOP.includes(text.charAt(i))) i++;
          var tag = text.slice(s3, i);
          if (tag && tag !== "*") compound.tag = tag.toLowerCase();
          sawAny = true;
        }
      }
      return sawAny ? compound : null;
    }
    function __prevElemSib(el) {
      var p = el.__parent; if (!p) return null;
      var idx = p.__kids.indexOf(el);
      for (var i = idx - 1; i >= 0; i--) if (p.__kids[i].__type === ELEMENT_NODE) return p.__kids[i];
      return null;
    }
    function __nextElemSib(el) {
      var p = el.__parent; if (!p) return null;
      var idx = p.__kids.indexOf(el);
      for (var i = idx + 1; i < p.__kids.length; i++) if (p.__kids[i].__type === ELEMENT_NODE) return p.__kids[i];
      return null;
    }
    function __elemIndex(el) {
      var p = el.__parent; if (!p) return 1;
      var idx = 1;
      for (var i = 0; i < p.__kids.length; i++) {
        if (p.__kids[i].__type === ELEMENT_NODE) { if (p.__kids[i] === el) return idx; idx++; }
      }
      return idx;
    }
    function __matchNth(el, arg) {
      arg = String(arg).replace(/\s+/g, "").toLowerCase();
      var idx = __elemIndex(el);
      if (arg === "odd") return idx % 2 === 1;
      if (arg === "even") return idx % 2 === 0;
      var m = /^([+-]?\d*)n([+-]\d+)?$/.exec(arg);
      if (m) {
        var a = (m[1] === "" || m[1] === "+") ? 1 : (m[1] === "-" ? -1 : parseInt(m[1], 10));
        var b = m[2] ? parseInt(m[2], 10) : 0;
        if (a === 0) return idx === b;
        return (idx - b) % a === 0 && (idx - b) / a >= 0;
      }
      var num = parseInt(arg, 10);
      return !isNaN(num) && idx === num;
    }
    function parseComplex(text) {
      // Split one complex selector into steps, honoring the `>` child combinator
      // (with optional surrounding whitespace) and whitespace as descendant.
      var steps = [];
      var i = 0, n = text.length;
      var pendingCombinator = "";   // for the first compound: subject ("")
      while (i < n) {
        // Skip leading whitespace; remember it as a (possible) descendant combinator.
        var sawSpace = false;
        while (i < n && /\s/.test(text.charAt(i))) { i++; sawSpace = true; }
        if (i >= n) break;
        var cc = text.charAt(i);
        if (cc === ">" || cc === "+" || cc === "~") {
          pendingCombinator = cc; i++;
          // Skip whitespace after the combinator.
          while (i < n && /\s/.test(text.charAt(i))) i++;
        } else if (sawSpace && steps.length > 0) {
          pendingCombinator = " ";
        }
        // Read the compound run up to the next combinator/whitespace, skipping
        // bracketed [..] and parenthesized (..) (e.g. :not(...), :nth-child(2n+1)).
        var s = i;
        while (i < n && !/\s/.test(text.charAt(i)) && ">+~".indexOf(text.charAt(i)) === -1) {
          var rc = text.charAt(i);
          if (rc === "[") { var e = text.indexOf("]", i); i = (e === -1) ? n : e + 1; }
          else if (rc === "(") { var d = 1; i++; while (i < n && d > 0) { var qc = text.charAt(i); if (qc === "(") d++; else if (qc === ")") d--; i++; } }
          else i++;
        }
        var compound = parseCompound(text.slice(s, i));
        if (!compound) return null;               // malformed → whole complex fails
        steps.push({ combinator: pendingCombinator, compound: compound });
        pendingCombinator = "";
      }
      return steps.length ? steps : null;
    }
    function parseSelectorList(sel) {
      // Top-level comma split (no nesting to worry about — no `:not()` etc.).
      var out = [];
      var parts = String(sel).split(",");
      for (var i = 0; i < parts.length; i++) {
        var complex = parseComplex(parts[i].trim());
        if (complex) out.push(complex);
      }
      return out;
    }
    function matchesCompound(el, compound) {
      if (!el || el.__type !== ELEMENT_NODE) return false;
      if (compound.tag !== null && el.__tag.toLowerCase() !== compound.tag) return false;
      if (compound.id !== null && getAttr(el, "id") !== compound.id) return false;
      for (var i = 0; i < compound.classes.length; i++) {
        if (classTokens(el).indexOf(compound.classes[i]) === -1) return false;
      }
      for (var j = 0; j < compound.attrs.length; j++) {
        var a = compound.attrs[j];
        var v = getAttr(el, a.name);
        if (v === null) return false;
        if (a.op === null) continue;                       // presence only
        if (a.op === "=") { if (v !== a.value) return false; }
        else if (a.op === "^=") { if (a.value === "" || v.indexOf(a.value) !== 0) return false; }
        else if (a.op === "$=") { if (a.value === "" || v.slice(v.length - a.value.length) !== a.value) return false; }
        else if (a.op === "*=") { if (a.value === "" || v.indexOf(a.value) === -1) return false; }
        else if (a.op === "~=") { if (v.split(/\s+/).indexOf(a.value) === -1) return false; }
        else if (a.op === "|=") { if (v !== a.value && v.indexOf(a.value + "-") !== 0) return false; }
      }
      for (var p = 0; p < compound.pseudos.length; p++) {
        var ps = compound.pseudos[p], nm = ps.name;
        if (nm === "not") { if (ps.arg && matchesSelector(el, ps.arg)) return false; }
        else if (nm === "is" || nm === "where" || nm === "matches") { if (ps.arg && !matchesSelector(el, ps.arg)) return false; }
        else if (nm === "first-child") { if (__prevElemSib(el)) return false; }
        else if (nm === "last-child") { if (__nextElemSib(el)) return false; }
        else if (nm === "only-child") { if (__prevElemSib(el) || __nextElemSib(el)) return false; }
        else if (nm === "nth-child") { if (!__matchNth(el, ps.arg)) return false; }
        else if (nm === "empty") {
          for (var e = 0; e < el.__kids.length; e++) {
            var kd = el.__kids[e];
            if (kd.__type === ELEMENT_NODE || (kd.__type === TEXT_NODE && kd.__text.length)) return false;
          }
        } else if (nm === "root") { if (el.__tag.toLowerCase() !== "html") return false; }
        else if (nm === "checked") { if (getAttr(el, "checked") === null && !el.__checked) return false; }
        else if (nm === "disabled") { if (getAttr(el, "disabled") === null) return false; }
        else if (nm === "enabled") { if (getAttr(el, "disabled") !== null) return false; }
        else { return false; }                              // unknown pseudo never matches (e.g. :hover)
      }
      return true;
    }
    function matchesComplex(el, steps) {
      // Anchor on the rightmost step (the subject), then satisfy each earlier
      // step by walking ancestors (descendant) or the immediate parent (child).
      var k = steps.length - 1;
      if (!matchesCompound(el, steps[k].compound)) return false;
      var node = el;
      for (k = steps.length - 1; k > 0; k--) {
        var rel = steps[k].combinator;       // how step[k] relates to step[k-1]
        var want = steps[k - 1].compound;
        if (rel === ">") {
          node = node.__parent;
          if (!matchesCompound(node, want)) return false;
        } else if (rel === "+") {
          node = __prevElemSib(node);
          if (!node || !matchesCompound(node, want)) return false;
        } else if (rel === "~") {
          var sib = __prevElemSib(node), sok = false;
          while (sib) { if (matchesCompound(sib, want)) { sok = true; node = sib; break; } sib = __prevElemSib(sib); }
          if (!sok) return false;
        } else {
          // Descendant: find SOME ancestor matching `want`.
          var anc = node.__parent, ok = false;
          while (anc && anc.__type === ELEMENT_NODE) {
            if (matchesCompound(anc, want)) { ok = true; node = anc; break; }
            anc = anc.__parent;
          }
          if (!ok) return false;
        }
      }
      return true;
    }
    function matchesSelector(el, sel) {
      var list = parseSelectorList(sel);
      for (var i = 0; i < list.length; i++) if (matchesComplex(el, list[i])) return true;
      return false;
    }
    function queryAll(root, sel) {
      var list = parseSelectorList(sel);
      var out = [];
      if (!list.length) return out;
      walkElements(root, function (el) {
        for (var i = 0; i < list.length; i++) {
          if (matchesComplex(el, list[i])) { out.push(el); return; }
        }
      });
      return out;
    }
    function queryOne(root, sel) {
      var all = queryAll(root, sel);
      return all.length ? all[0] : null;
    }

    // ---- HTML serialization (for innerHTML/outerHTML getters) ----------
    // Void elements (no close tag) — kept in lock-step with the Rust parser's
    // VOID list so a serialize→reparse round-trip is stable.
    var VOID_ELEMENTS = {
      area: 1, base: 1, br: 1, col: 1, embed: 1, hr: 1, img: 1, input: 1,
      link: 1, meta: 1, param: 1, source: 1, track: 1, wbr: 1,
    };
    function escapeText(s) {
      return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    }
    function escapeAttr(s) {
      return String(s).replace(/&/g, "&amp;").replace(/"/g, "&quot;");
    }
    function serializeNodeHTML(node) {
      if (node.__type === TEXT_NODE) return escapeText(node.__text);
      var tag = node.__tag;
      // A node whose innerHTML was set holds a raw fragment string verbatim.
      var inner = (typeof node.__rawHTML === "string")
        ? node.__rawHTML
        : serializeChildrenHTML(node);
      var open = "<" + tag;
      for (var i = 0; i < node.__attrs.length; i++) {
        open += " " + node.__attrs[i][0] + '="' + escapeAttr(node.__attrs[i][1]) + '"';
      }
      open += ">";
      if (VOID_ELEMENTS[tag]) return open;       // void: no children, no close
      return open + inner + "</" + tag + ">";
    }
    function serializeChildrenHTML(node) {
      if (typeof node.__rawHTML === "string") return node.__rawHTML;
      var out = "";
      for (var i = 0; i < node.__kids.length; i++) out += serializeNodeHTML(node.__kids[i]);
      return out;
    }

    // ---- style helpers (used lazily by the element `style` accessor) ---
    // The style object is a Proxy built ONCE PER ELEMENT, but only when `.style`
    // is first read (cached on el.__styleObj). The thousands of nodes that never
    // touch `.style` therefore pay nothing. Behavior is store-only: assignments
    // are remembered and reflected back into the `style` attribute so a
    // round-trip preserves them, but nothing is rendered from them yet.
    function styleCssTextOf(store) {
      var parts = [];
      for (var k in store) parts.push(k + ": " + store[k]);
      return parts.join("; ");
    }
    function parseCssText(text, into) {
      text.split(";").forEach(function (decl) {
        var c = decl.indexOf(":");
        if (c === -1) return;
        var prop = decl.slice(0, c).trim();
        var val = decl.slice(c + 1).trim();
        if (prop) into[prop] = val;
      });
    }
    function buildStyleObject(el) {
      var styleStore = Object.create(null);
      function syncStyleAttr() {
        var text = styleCssTextOf(styleStore);
        if (text) setAttr(el, "style", text); else removeAttr(el, "style");
      }
      // If the element carries a style attribute, seed the store from it. Lazy
      // seeding is equivalent to eager seeding: the store is only observable
      // through this proxy (getComputedStyle reads the attribute directly).
      var initialStyle = getAttr(el, "style");
      if (initialStyle) parseCssText(initialStyle, styleStore);
      return new Proxy(styleStore, {
        get: function (t, k) {
          if (k === "setProperty") return function (p, v) { t[p] = String(v); syncStyleAttr(); };
          if (k === "removeProperty") return function (p) { var old = t[p]; delete t[p]; syncStyleAttr(); return old; };
          if (k === "getPropertyValue") return function (p) { return t[p] || ""; };
          if (k === "cssText") return styleCssTextOf(styleStore);
          return (k in t) ? t[k] : "";
        },
        set: function (t, k, v) {
          if (k === "cssText") { for (var kk in t) delete t[kk]; parseCssText(String(v), t); syncStyleAttr(); return true; }
          t[k] = String(v); syncStyleAttr(); return true;
        },
      });
    }

    // ---- shared node prototypes ----------------------------------------
    // BEHAVIOR LIVES ONCE on three shared prototype objects; each node instance
    // carries only DATA (__id/__type/__tag/__attrs/__kids/__parent/__text/…).
    // This replaces the old per-instance defineProperty + method-assignment
    // explosion (~40 accessors + closures per node), which ballooned resident
    // memory on large pages. NODE_PROTO holds the accessors/methods common to
    // every node; ELEMENT_PROTO and TEXT_PROTO inherit from it via
    // Object.create(NODE_PROTO) and add their own. Accessors read/write `this`.
    var NODE_PROTO = Object.create(null);
    var ELEMENT_PROTO = Object.create(NODE_PROTO);
    var TEXT_PROTO = Object.create(NODE_PROTO);

    function defAccessor(proto, name, getter, setter) {
      var desc = { get: getter, enumerable: false, configurable: true };
      if (setter) desc.set = setter;
      Object.defineProperty(proto, name, desc);
    }

    // -- common (NODE_PROTO): tree links + structural mutation --
    defAccessor(NODE_PROTO, "nodeType", function () { return this.__type; });
    defAccessor(NODE_PROTO, "parentNode", function () { return this.__parent; });
    defAccessor(NODE_PROTO, "parentElement", function () {
      return this.__parent && this.__parent.__type === ELEMENT_NODE ? this.__parent : null;
    });
    defAccessor(NODE_PROTO, "childNodes", function () { return this.__kids.slice(); });
    defAccessor(NODE_PROTO, "firstChild", function () { return this.__kids[0] || null; });
    defAccessor(NODE_PROTO, "lastChild", function () { return this.__kids[this.__kids.length - 1] || null; });
    defAccessor(NODE_PROTO, "nextSibling", function () {
      var p = this.__parent; if (!p) return null;
      var i = p.__kids.indexOf(this); return (i === -1) ? null : (p.__kids[i + 1] || null);
    });
    defAccessor(NODE_PROTO, "previousSibling", function () {
      var p = this.__parent; if (!p) return null;
      var i = p.__kids.indexOf(this); return (i <= 0) ? null : (p.__kids[i - 1] || null);
    });
    defAccessor(NODE_PROTO, "textContent",
      function () { var acc = []; collectText(this, acc); return acc.join(""); },
      function (value) {
        if (this.__type === TEXT_NODE) { this.__text = String(value); return; }
        for (var i = 0; i < this.__kids.length; i++) this.__kids[i].__parent = null;
        this.__kids = [];
        if (typeof this.__rawHTML === "string") this.__rawHTML = undefined;
        var t = makeText(String(value));
        appendChild(this, t);
      });
    NODE_PROTO.appendChild = function (child) { return appendChild(this, child); };
    NODE_PROTO.removeChild = function (child) { return removeChild(this, child); };
    NODE_PROTO.insertBefore = function (child, ref) { return insertBefore(this, child, ref); };
    NODE_PROTO.remove = function () { detach(this); };
    NODE_PROTO.replaceChild = function (newChild, oldChild) {
      var i = this.__kids.indexOf(oldChild);
      if (i === -1) return oldChild;
      detach(newChild);
      this.__kids[i] = newChild; newChild.__parent = this; oldChild.__parent = null;
      __moEmitChildList(this, [newChild], [oldChild]);
      return oldChild;
    };
    function __cloneNode(node, deep) {
      var copy;
      if (node.__type === TEXT_NODE) { copy = makeText(node.__text); }
      else {
        copy = makeElement(node.__tag);
        for (var i = 0; i < node.__attrs.length; i++) copy.__attrs.push([node.__attrs[i][0], node.__attrs[i][1]]);
        if (node.__rawHTML != null) copy.__rawHTML = node.__rawHTML;
        if (deep) for (var j = 0; j < node.__kids.length; j++) {
          var ch = __cloneNode(node.__kids[j], true); ch.__parent = copy; copy.__kids.push(ch);
        }
      }
      return copy;
    }
    NODE_PROTO.cloneNode = function (deep) { return __cloneNode(this, !!deep); };
    // Modern manipulation (append/prepend/before/after/replaceWith/
    // replaceChildren); a string argument becomes a text node.
    function __toNode(a) { return (a && a.__type !== undefined) ? a : makeText(a == null ? "" : String(a)); }
    ELEMENT_PROTO.append = function () { for (var i = 0; i < arguments.length; i++) appendChild(this, __toNode(arguments[i])); };
    ELEMENT_PROTO.prepend = function () {
      var first = this.__kids[0] || null;
      for (var i = 0; i < arguments.length; i++) insertBefore(this, __toNode(arguments[i]), first);
    };
    ELEMENT_PROTO.before = function () {
      var p = this.__parent; if (!p) return;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, __toNode(arguments[i]), this);
    };
    ELEMENT_PROTO.after = function () {
      var p = this.__parent; if (!p) return;
      var ref = p.__kids[p.__kids.indexOf(this) + 1] || null;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, __toNode(arguments[i]), ref);
    };
    ELEMENT_PROTO.replaceWith = function () {
      var p = this.__parent; if (!p) return;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, __toNode(arguments[i]), this);
      removeChild(p, this);
    };
    ELEMENT_PROTO.replaceChildren = function () {
      while (this.__kids.length) removeChild(this, this.__kids[0]);
      for (var i = 0; i < arguments.length; i++) appendChild(this, __toNode(arguments[i]));
    };
    NODE_PROTO.contains = function (other) {
      for (var n = other; n; n = n.__parent) if (n === this) return true;
      return false;
    };
    NODE_PROTO.hasChildNodes = function () { return this.__kids.length > 0; };

    // -- elements (ELEMENT_PROTO) --
    defAccessor(ELEMENT_PROTO, "tagName", function () { return this.__tag.toUpperCase(); });
    defAccessor(ELEMENT_PROTO, "nodeName", function () { return this.__tag.toUpperCase(); });
    defAccessor(ELEMENT_PROTO, "children", function () { return elementChildren(this); });
    defAccessor(ELEMENT_PROTO, "firstElementChild", function () { var c = elementChildren(this); return c[0] || null; });
    defAccessor(ELEMENT_PROTO, "lastElementChild", function () { var c = elementChildren(this); return c[c.length - 1] || null; });
    defAccessor(ELEMENT_PROTO, "id",
      function () { return getAttr(this, "id") || ""; },
      function (v) { setAttr(this, "id", v); });
    // Form-control current value, backed by the `value` attribute so handlers
    // can read/modify `el.value` and the change reflects in serialize/layout
    // (M12b input events).
    defAccessor(ELEMENT_PROTO, "value",
      function () { var v = getAttr(this, "value"); return v === null ? "" : v; },
      function (v) { setAttr(this, "value", String(v)); });
    defAccessor(ELEMENT_PROTO, "className",
      function () { return getAttr(this, "class") || ""; },
      function (v) { setAttr(this, "class", v); });
    defAccessor(ELEMENT_PROTO, "classList", function () {
      if (!this.__classList) this.__classList = makeClassList(this);
      return this.__classList;
    });
    defAccessor(ELEMENT_PROTO, "innerText",
      function () { var acc = []; collectText(this, acc); return acc.join(""); },
      function (v) { this.textContent = v; });

    // innerHTML — DEFERRED REPARSE. The setter does NOT parse HTML in JS: it
    // records the raw fragment on the node (__rawHTML) and drops the node's
    // JS children. The fragment is reparsed by the real Rust parser at
    // reconcile (see serialize -> rebuild_document). LIMITATION: the children
    // are not available in JS after a set, so reading them back mid-script
    // (el.children, querySelector into the fragment, ...) is not supported; the
    // getter returns the stored raw string. The getter on a non-raw node
    // serializes its current children to HTML in JS.
    defAccessor(ELEMENT_PROTO, "innerHTML",
      function () { return serializeChildrenHTML(this); },
      function (v) {
        for (var i = 0; i < this.__kids.length; i++) this.__kids[i].__parent = null;
        this.__kids = [];
        this.__rawHTML = String(v);
      });
    // outerHTML getter serializes this element (open tag, contents, close) to
    // HTML in JS. The setter is not supported (it would require splicing into
    // the parent and reparsing in place); we leave it as a silent no-op.
    defAccessor(ELEMENT_PROTO, "outerHTML",
      function () { return serializeNodeHTML(this); },
      function () { /* unsupported: see note above */ });
    // style: lazy per-element Proxy, built on first read and cached on
    // __styleObj so nodes that never touch `.style` allocate nothing.
    defAccessor(ELEMENT_PROTO, "style", function () {
      if (!this.__styleObj) this.__styleObj = buildStyleObject(this);
      return this.__styleObj;
    });

    // insertAdjacentHTML: reuses the raw-HTML mechanism. We support the two
    // common in-element positions by merging into __rawHTML (which the Rust
    // parser reparses); "afterbegin" prepends, "beforeend" appends. The
    // sibling positions "beforebegin"/"afterend" would need to splice raw HTML
    // into the PARENT and are not supported (documented no-op). Because this
    // routes through __rawHTML, any pre-existing JS children are first
    // serialized into the raw string (same deferred-reparse limitation).
    ELEMENT_PROTO.insertAdjacentHTML = function (position, html) {
      position = String(position).toLowerCase();
      html = String(html);
      var current = (typeof this.__rawHTML === "string")
        ? this.__rawHTML
        : serializeChildrenHTML(this);
      if (position === "afterbegin") {
        for (var i = 0; i < this.__kids.length; i++) this.__kids[i].__parent = null;
        this.__kids = [];
        this.__rawHTML = html + current;
      } else if (position === "beforeend") {
        for (var j = 0; j < this.__kids.length; j++) this.__kids[j].__parent = null;
        this.__kids = [];
        this.__rawHTML = current + html;
      }
      /* else: beforebegin/afterend unsupported -> no-op. */
    };

    ELEMENT_PROTO.getAttribute = function (n) { return getAttr(this, String(n)); };
    ELEMENT_PROTO.setAttribute = function (n, v) { setAttr(this, String(n), v); };
    ELEMENT_PROTO.removeAttribute = function (n) { removeAttr(this, String(n)); };
    ELEMENT_PROTO.hasAttribute = function (n) { return attrIndex(this, String(n)) !== -1; };
    ELEMENT_PROTO.getAttributeNames = function () { return this.__attrs.map(function (p) { return p[0]; }); };

    // el.dataset: a live view of the element's data-* attributes as camelCase
    // properties (frameworks use it constantly). data-foo-bar <-> el.dataset.fooBar.
    function __camelToKebab(s) { return String(s).replace(/[A-Z]/g, function (m) { return "-" + m.toLowerCase(); }); }
    function __kebabToCamel(s) { return s.replace(/-([a-z])/g, function (_m, c) { return c.toUpperCase(); }); }
    Object.defineProperty(ELEMENT_PROTO, "dataset", {
      get: function () {
        var el = this;
        return new Proxy({}, {
          get: function (t, k) {
            if (typeof k !== "string") return undefined;
            var v = getAttr(el, "data-" + __camelToKebab(k));
            return v == null ? undefined : v;
          },
          set: function (t, k, v) {
            if (typeof k === "string") setAttr(el, "data-" + __camelToKebab(k), String(v));
            return true;
          },
          has: function (t, k) {
            return typeof k === "string" && attrIndex(el, "data-" + __camelToKebab(k)) !== -1;
          },
          deleteProperty: function (t, k) {
            if (typeof k === "string") removeAttr(el, "data-" + __camelToKebab(k));
            return true;
          },
          ownKeys: function () {
            return el.__attrs
              .filter(function (p) { return p[0].slice(0, 5) === "data-"; })
              .map(function (p) { return __kebabToCamel(p[0].slice(5)); });
          },
          getOwnPropertyDescriptor: function (t, k) {
            var v = (typeof k === "string") ? getAttr(el, "data-" + __camelToKebab(k)) : null;
            if (v == null) return undefined;
            return { value: v, writable: true, enumerable: true, configurable: true };
          },
        });
      },
      enumerable: true, configurable: true,
    });

    ELEMENT_PROTO.getElementsByTagName = function (t) { return queryAll(this, String(t)); };
    ELEMENT_PROTO.getElementsByClassName = function (c) { return queryAll(this, "." + String(c)); };
    ELEMENT_PROTO.querySelector = function (s) { return queryOne(this, s); };
    ELEMENT_PROTO.querySelectorAll = function (s) { return queryAll(this, s); };
    ELEMENT_PROTO.matches = function (s) { return matchesSelector(this, s); };
    ELEMENT_PROTO.closest = function (s) {
      for (var n = this; n && n.__type === ELEMENT_NODE; n = n.__parent) if (matchesSelector(n, s)) return n;
      return null;
    };

    ELEMENT_PROTO.getBoundingClientRect = function () {
      var g = this.__geometry || { x: 0, y: 0, w: 0, h: 0 };
      return {
        x: g.x, y: g.y, top: g.y, left: g.x,
        right: g.x + g.w, bottom: g.y + g.h, width: g.w, height: g.h,
      };
    };

    // Inert event listener registry on elements (dispatch not yet driven by
    // the bridge beyond DOMContentLoaded/load on document+window). __listeners
    // is created lazily on first addEventListener so listener-free nodes pay
    // nothing.
    // Capture-aware listener registry: bubble/target listeners live in
    // __listeners, capture listeners in __capListeners (3rd arg `true` or
    // {capture:true}). Shared by element/document/window.
    function __addEL(node, type, fn, options) {
      type = String(type);
      var cap = options === true || (options && options.capture);
      var bucket = cap ? "__capListeners" : "__listeners";
      if (!node[bucket]) node[bucket] = Object.create(null);
      if (!node[bucket][type]) node[bucket][type] = [];
      if (typeof fn === "function") node[bucket][type].push(fn);
    }
    function __removeEL(node, type, fn, options) {
      type = String(type);
      var cap = options === true || (options && options.capture);
      var bucket = cap ? "__capListeners" : "__listeners";
      if (!node[bucket]) return;
      var arr = node[bucket][type]; if (!arr) return;
      var i = arr.indexOf(fn); if (i !== -1) arr.splice(i, 1);
    }
    // Full 3-phase propagation (capture → target → bubble) for both host-driven
    // events (__cerberusDispatch) and script dispatchEvent.
    function __propagate(target, ev) {
      var type = ev.type;
      if (ev.target == null) ev.target = target;
      var path;
      if (target === g.window) { path = [g.window]; }
      else if (target === g.document) { path = [g.document, g.window]; }
      else {
        path = [];
        for (var n = target; n; n = n.__parent) path.push(n);
        path.push(g.document); path.push(g.window);
      }
      function fire(node, arr, phase) {
        if (!arr || !arr.length) return;
        ev.currentTarget = node; ev.eventPhase = phase;
        var c = arr.slice();
        for (var j = 0; j < c.length; j++) {
          try { c[j].call(node, ev); } catch (e) {}
          if (ev.__stopImmediate) return;
        }
      }
      // CAPTURING_PHASE: window → target's parent.
      for (var i = path.length - 1; i >= 1; i--) {
        if (ev.__stop) break;
        fire(path[i], path[i].__capListeners ? path[i].__capListeners[type] : null, 1);
      }
      // AT_TARGET: target's capture then bubble listeners.
      if (!ev.__stop) {
        var tc = (target.__capListeners && target.__capListeners[type]) || [];
        var tb = (target.__listeners && target.__listeners[type]) || [];
        fire(target, tc.concat(tb), 2);
      }
      // BUBBLING_PHASE: target's parent → window (only if the event bubbles).
      if (ev.bubbles && !ev.__stop) {
        for (var i2 = 1; i2 < path.length; i2++) {
          if (ev.__stop) break;
          fire(path[i2], path[i2].__listeners ? path[i2].__listeners[type] : null, 3);
        }
      }
      ev.eventPhase = 0; ev.currentTarget = null;
    }
    ELEMENT_PROTO.addEventListener = function (type, fn, options) { __addEL(this, type, fn, options); };
    ELEMENT_PROTO.removeEventListener = function (type, fn, options) { __removeEL(this, type, fn, options); };
    ELEMENT_PROTO.dispatchEvent = function (ev) {
      if (!ev) return true;
      if (ev.__stop === undefined) { ev.__stop = false; ev.__stopImmediate = false; }
      __propagate(this, ev);
      return !ev.defaultPrevented;
    };

    // -- text nodes (TEXT_PROTO) --
    defAccessor(TEXT_PROTO, "nodeName", function () { return "#text"; });
    defAccessor(TEXT_PROTO, "data",
      function () { return this.__text; },
      function (v) { this.__text = String(v); });

    // ---- node constructors ---------------------------------------------
    // Nodes are created with Object.create(<proto>) and carry ONLY data fields;
    // all behavior comes from the shared prototype. No per-node defineProperty,
    // no per-node function assignments.
    function makeElement(tag, id) {
      var el = Object.create(ELEMENT_PROTO);
      el.__type = ELEMENT_NODE;
      el.__tag = String(tag).toLowerCase();
      el.__attrs = [];
      el.__kids = [];
      el.__parent = null;
      el.__id = (typeof id === "number") ? id : freshId();
      indexNode(el);
      // Fingerprintable surfaces come from the farbling prologue (per-head
      // seeded shims, installed before this model): every canvas — parsed or
      // script-created — gets its farbled getContext/toDataURL here.
      if (el.__tag === "canvas" && globalThis.__cerberusFarble) {
        globalThis.__cerberusFarble.attachCanvas(el);
      }
      return el;
    }
    function makeText(text, id) {
      var t = Object.create(TEXT_PROTO);
      t.__type = TEXT_NODE;
      t.__text = String(text);
      t.__kids = [];
      t.__parent = null;
      t.__id = (typeof id === "number") ? id : freshId();
      indexNode(t);
      return t;
    }

    // ---- document ------------------------------------------------------
    var document = {
      __listeners: Object.create(null),
      readyState: "loading",
      __cookie: "",
      __root: null,        // the synthetic #root element (snapshot root)
      documentElement: null,
      head: null,
      body: null,
      nodeType: 9,
    };

    Object.defineProperty(document, "title", {
      get: function () {
        var t = this.__titleEl;
        return t ? (function () { var acc = []; collectText(t, acc); return acc.join(""); })() : "";
      },
      set: function (v) {
        var t = this.__titleEl;
        if (t) { t.textContent = String(v); return; }
        // No <title> yet: create one under <head> (or documentElement / root).
        t = makeElement("title");
        t.textContent = String(v);
        var host = this.head || this.documentElement || this.__root;
        if (host) appendChild(host, t);
        this.__titleEl = t;
      },
      enumerable: true, configurable: true,
    });
    Object.defineProperty(document, "cookie", {
      get: function () { return this.__cookie; },
      set: function (v) {
        // Record the raw assignment (with attributes) for the host to apply to
        // the real jar (Set-Cookie semantics: Path/Expires/Secure/SameSite). The
        // in-page merge below keeps a readable name=value view for this run.
        var raw = String(v);
        if (!this.__cookieWrites) { this.__cookieWrites = []; }
        this.__cookieWrites.push(raw);
        var semi = raw.indexOf(";");
        var pair = (semi === -1 ? raw : raw.slice(0, semi)).trim();
        var eq = pair.indexOf("=");
        if (eq === -1) return;
        var name = pair.slice(0, eq).trim();
        var jar = this.__cookie ? this.__cookie.split("; ") : [];
        var replaced = false;
        for (var i = 0; i < jar.length; i++) {
          if (jar[i].slice(0, jar[i].indexOf("=")) === name) { jar[i] = pair; replaced = true; break; }
        }
        if (!replaced) jar.push(pair);
        this.__cookie = jar.join("; ");
      },
      enumerable: true, configurable: true,
    });

    document.getElementById = function (id) {
      id = String(id);
      var found = null;
      if (this.documentElement) {
        if (getAttr(this.documentElement, "id") === id) return this.documentElement;
        walkElements(this.documentElement, function (el) { if (!found && getAttr(el, "id") === id) found = el; });
      }
      return found;
    };
    document.getElementsByTagName = function (t) { return this.documentElement ? queryAll(this.documentElement, String(t)) : []; };
    document.getElementsByClassName = function (c) { return this.documentElement ? queryAll(this.documentElement, "." + String(c)) : []; };
    document.querySelector = function (s) { return this.documentElement ? queryOne(this.documentElement, s) : null; };
    document.querySelectorAll = function (s) { return this.documentElement ? queryAll(this.documentElement, s) : []; };
    document.createElement = function (tag) { return makeElement(tag); };
    document.createTextNode = function (text) { return makeText(text); };
    document.createDocumentFragment = function () {
      // A lightweight fragment: appendChild moves its children, like the spec,
      // but we model it as a bare element whose kids get re-parented on insert.
      return makeElement("#fragment");
    };
    document.addEventListener = function (type, fn, options) { __addEL(this, type, fn, options); };
    document.removeEventListener = function (type, fn, options) { __removeEL(this, type, fn, options); };
    document.dispatchEvent = function (ev) {
      if (!ev) return true;
      if (ev.__stop === undefined) { ev.__stop = false; ev.__stopImmediate = false; }
      __propagate(this, ev);
      return !ev.defaultPrevented;
    };

    g.document = document;

    // ---- window = globalThis -------------------------------------------
    g.window = g;
    g.self = g;
    window.document = document;
    if (!window.__listeners) window.__listeners = Object.create(null);
    window.addEventListener = function (type, fn, options) { __addEL(this, type, fn, options); };
    window.removeEventListener = function (type, fn, options) { __removeEL(this, type, fn, options); };
    window.dispatchEvent = function (ev) {
      if (!ev) return true;
      if (ev.__stop === undefined) { ev.__stop = false; ev.__stopImmediate = false; }
      __propagate(this, ev);
      return !ev.defaultPrevented;
    };

    // ---- ambient environment (location/navigator/screen/storage/…) -----
    // All derived from globalThis.__CERBERUS_ENV__ = { url, width, height },
    // injected by run_page_scripts before this prelude. We never throw: a
    // missing/garbage env falls back to inert defaults.
    var env = (g.__CERBERUS_ENV__ && typeof g.__CERBERUS_ENV__ === "object") ? g.__CERBERUS_ENV__ : {};
    // Seed document.cookie with the origin's non-HttpOnly cookies from the real
    // jar (the host excludes HttpOnly, so script never sees them). Built-in /
    // cookieless pages pass "" and keep the empty default.
    if (typeof env.cookies === "string" && env.cookies) { document.__cookie = env.cookies; }
    var envUrl = (typeof env.url === "string") ? env.url : "about:blank";
    var vpW = (typeof env.width === "number") ? env.width : 0;
    var vpH = (typeof env.height === "number") ? env.height : 0;
    // The UA the network stack actually presented to this origin (honest by
    // default; the escalated rung if this site forced it). Falls back to our
    // honest identity if absent, so header and navigator can never disagree.
    var envUA = (typeof env.userAgent === "string" && env.userAgent) ? env.userAgent : "Cerberus/0.0";

    // ---- location ------------------------------------------------------
    // A small JS regex parser for the URL into the WHATWG-ish pieces pages
    // read. assign/replace/reload are no-ops: navigation is the browser's job
    // in this model, not the page's.
    function parseLocation(url) {
      var loc = {
        href: url, protocol: "", host: "", hostname: "", port: "",
        origin: "", pathname: "", search: "", hash: "",
      };
      // scheme://authority/path?query#fragment  (authority optional).
      var m = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)(\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(url);
      if (!m) { loc.pathname = url; return loc; }
      loc.protocol = m[1] || "";
      var authority = m[3] || "";
      loc.pathname = m[4] || "";
      loc.search = m[5] || "";
      loc.hash = m[6] || "";
      if (authority) {
        loc.host = authority;
        var colon = authority.lastIndexOf(":");
        if (colon !== -1 && /^[0-9]+$/.test(authority.slice(colon + 1))) {
          loc.hostname = authority.slice(0, colon);
          loc.port = authority.slice(colon + 1);
        } else {
          loc.hostname = authority;
        }
        loc.origin = loc.protocol + "//" + authority;
      }
      if (!loc.pathname && authority) loc.pathname = "/";
      return loc;
    }
    var locationObj = parseLocation(envUrl);
    locationObj.assign = function () {};
    locationObj.replace = function () {};
    locationObj.reload = function () {};
    locationObj.toString = function () { return this.href; };
    g.location = locationObj;
    window.location = locationObj;
    document.location = locationObj;
    Object.defineProperty(document, "URL", { get: function () { return locationObj.href; }, enumerable: true, configurable: true });
    Object.defineProperty(document, "documentURI", { get: function () { return locationObj.href; }, enumerable: true, configurable: true });

    // ---- navigator -----------------------------------------------------
    // IDENTITY MODEL (two rules):
    //  1. The User-Agent is HONEST-FIRST and COHERENT. `userAgent` is whatever
    //     the network stack actually sent this origin — our real `Cerberus/0.0`
    //     by default, or, only if the site's bot management forced the fallback
    //     ladder, the SAME escalated string the request header carried. The OS
    //     in `platform` is derived from it. So the header and the script-visible
    //     identity can never disagree; a mismatch would itself be a fingerprint.
    //  2. Every OTHER signal is UNIFORM and low-entropy for every user and head,
    //     never reflecting the real device — denying a tracker a stable
    //     cross-site identity at all times, regardless of the UA. We expose NO
    //     high-entropy surface: no plugins, mediaDevices, WebGL, deviceMemory,
    //     or Battery API. (Per-head ±1 farbling of the remaining high-entropy
    //     reads — canvas / audio / font-metrics — is the separate active step.)
    var navPlatform = envUA.indexOf("Windows") >= 0 ? "Win32"
      : (envUA.indexOf("Mac OS X") >= 0 || envUA.indexOf("Macintosh") >= 0) ? "MacIntel"
      : "Linux x86_64";
    g.navigator = {
      userAgent: envUA,
      appCodeName: "Mozilla",
      appName: "Netscape",
      appVersion: envUA.indexOf("Mozilla/") === 0 ? envUA.slice(8) : envUA,
      product: "Gecko",
      vendor: "",
      language: "en-US",
      languages: ["en-US", "en"],
      platform: navPlatform,
      hardwareConcurrency: 4,
      maxTouchPoints: 0,
      onLine: true,
      cookieEnabled: true,
      webdriver: false,
    };
    // sendBeacon: fire-and-forget POST on the shared fetch queue (sensors/analytics
    // ship telemetry through it). Returns true if queued.
    g.navigator.sendBeacon = function (url, data) {
      try {
        if (!Array.isArray(g.__cerberusFetchQueue)) return false;
        var id = g.__cerberusFetchId++;
        g.__cerberusFetchQueue.push({
          id: id, url: String(url), method: "POST", headers: [],
          body: (data != null ? String(data) : "")
        });
        g.__cerberusFetchPending[id] = { xhr: true, settle: function () {}, fail: function () {} };
        return true;
      } catch (e) { return false; }
    };

    // ---- Event / CustomEvent constructors ------------------------------
    // `new Event('x')` / `new CustomEvent('x', {detail})` were absent, so
    // dispatchEvent(new Event(...)) threw. Minimal spec-shaped events.
    g.Event = function Event(type, init) {
      init = init || {};
      this.type = String(type);
      this.bubbles = !!init.bubbles;
      this.cancelable = !!init.cancelable;
      this.composed = !!init.composed;
      this.defaultPrevented = false;
      this.target = null; this.currentTarget = null; this.eventPhase = 0;
      this.timeStamp = (g.performance && g.performance.now) ? g.performance.now() : 0;
      this.__stop = false; this.__stopImmediate = false;
    };
    g.Event.NONE = 0; g.Event.CAPTURING_PHASE = 1; g.Event.AT_TARGET = 2; g.Event.BUBBLING_PHASE = 3;
    g.Event.prototype.preventDefault = function () { if (this.cancelable) this.defaultPrevented = true; };
    g.Event.prototype.stopPropagation = function () { this.__stop = true; };
    g.Event.prototype.stopImmediatePropagation = function () { this.__stop = true; this.__stopImmediate = true; };
    g.CustomEvent = function CustomEvent(type, init) {
      init = init || {};
      g.Event.call(this, type, init);
      this.detail = (init.detail !== undefined) ? init.detail : null;
    };
    g.CustomEvent.prototype = Object.create(g.Event.prototype);
    g.CustomEvent.prototype.constructor = g.CustomEvent;

    // ---- URL / URLSearchParams -----------------------------------------
    // `new URL(...)` and `URLSearchParams` are used pervasively (routing, link
    // building, query parsing); their absence silently broke that code. A
    // pragmatic (not full-WHATWG) parser handling absolute URLs + common relative
    // resolution, with a spec-shaped URLSearchParams.
    g.URLSearchParams = function URLSearchParams(init) {
      this.__p = [];
      var self = this;
      if (init == null || init === "") return;
      if (typeof init === "string") {
        var s = init.charAt(0) === "?" ? init.slice(1) : init;
        if (s) {
          s.split("&").forEach(function (pair) {
            if (!pair) return;
            var eq = pair.indexOf("=");
            var k = eq === -1 ? pair : pair.slice(0, eq);
            var v = eq === -1 ? "" : pair.slice(eq + 1);
            self.__p.push([decodeURIComponent(k.replace(/\+/g, " ")),
                           decodeURIComponent(v.replace(/\+/g, " "))]);
          });
        }
      } else if (typeof init.length === "number") {
        for (var i = 0; i < init.length; i++) self.__p.push([String(init[i][0]), String(init[i][1])]);
      } else if (typeof init === "object") {
        for (var key in init) if (Object.prototype.hasOwnProperty.call(init, key)) {
          self.__p.push([String(key), String(init[key])]);
        }
      }
    };
    var USP = g.URLSearchParams.prototype;
    USP.append = function (k, v) { this.__p.push([String(k), String(v)]); };
    USP.delete = function (k) { k = String(k); this.__p = this.__p.filter(function (e) { return e[0] !== k; }); };
    USP.get = function (k) { k = String(k); for (var i = 0; i < this.__p.length; i++) if (this.__p[i][0] === k) return this.__p[i][1]; return null; };
    USP.getAll = function (k) { k = String(k); return this.__p.filter(function (e) { return e[0] === k; }).map(function (e) { return e[1]; }); };
    USP.has = function (k) { return this.get(String(k)) !== null; };
    USP.set = function (k, v) {
      k = String(k); v = String(v); var done = false, out = [];
      for (var i = 0; i < this.__p.length; i++) {
        if (this.__p[i][0] === k) { if (!done) { out.push([k, v]); done = true; } }
        else out.push(this.__p[i]);
      }
      if (!done) out.push([k, v]);
      this.__p = out;
    };
    USP.forEach = function (cb, t) { for (var i = 0; i < this.__p.length; i++) cb.call(t, this.__p[i][1], this.__p[i][0], this); };
    USP.keys = function () { return this.__p.map(function (e) { return e[0]; }); };
    USP.values = function () { return this.__p.map(function (e) { return e[1]; }); };
    USP.entries = function () { return this.__p.map(function (e) { return [e[0], e[1]]; }); };
    USP.sort = function () { this.__p.sort(function (a, b) { return a[0] < b[0] ? -1 : (a[0] > b[0] ? 1 : 0); }); };
    USP.toString = function () {
      return this.__p.map(function (e) { return encodeURIComponent(e[0]) + "=" + encodeURIComponent(e[1]); }).join("&");
    };

    function __normPath(p) {
      var parts = p.split("/"), out = [];
      for (var i = 0; i < parts.length; i++) {
        if (parts[i] === "..") { if (out.length > 1) out.pop(); }
        else if (parts[i] !== ".") out.push(parts[i]);
      }
      var r = out.join("/");
      return r.charAt(0) === "/" ? r : "/" + r;
    }
    function __resolveRel(base, ref) {
      ref = String(ref);
      if (ref === "") return base;
      if (ref.slice(0, 2) === "//") return (/^([a-zA-Z][\w+.\-]*:)/.exec(base) || ["", "https:"])[1] + ref;
      var bm = /^([a-zA-Z][\w+.\-]*:)\/\/([^\/?#]*)([^?#]*)(\?[^#]*)?/.exec(base);
      if (!bm) return ref;
      var pre = bm[1] + "//" + bm[2], bpath = bm[3] || "/", bquery = bm[4] || "";
      if (ref.charAt(0) === "#") return pre + bpath + bquery + ref;
      if (ref.charAt(0) === "?") return pre + bpath + ref;
      var split = ref.search(/[?#]/), tail = split === -1 ? "" : ref.slice(split);
      var path = split === -1 ? ref : ref.slice(0, split);
      if (path.charAt(0) !== "/") {
        var dir = bpath.slice(0, bpath.lastIndexOf("/") + 1);
        path = dir + path;
      }
      return pre + __normPath(path) + tail;
    }
    g.URL = function URL(url, base) {
      var u = String(url);
      if (!/^[a-zA-Z][\w+.\-]*:/.test(u)) {
        if (base == null) throw new TypeError("Invalid URL: " + u);
        u = __resolveRel(String(base), u);
      }
      var m = /^([a-zA-Z][\w+.\-]*:)(\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(u);
      if (!m) throw new TypeError("Invalid URL: " + u);
      var self = this, authority = m[3] || "";
      this.protocol = m[1];
      this.username = ""; this.password = "";
      var host = authority, at = authority.lastIndexOf("@");
      if (at !== -1) {
        var cred = authority.slice(0, at); host = authority.slice(at + 1);
        var ci = cred.indexOf(":");
        if (ci !== -1) { this.username = cred.slice(0, ci); this.password = cred.slice(ci + 1); }
        else this.username = cred;
      }
      this.hostname = host; this.port = "";
      var pi = host.lastIndexOf(":");
      if (pi !== -1 && /^\d+$/.test(host.slice(pi + 1))) { this.hostname = host.slice(0, pi); this.port = host.slice(pi + 1); }
      if ((this.protocol === "http:" && this.port === "80") || (this.protocol === "https:" && this.port === "443")) this.port = "";
      this.host = this.port ? this.hostname + ":" + this.port : this.hostname;
      this.pathname = (m[4] || "") || (authority ? "/" : "");
      this.hash = m[6] || "";
      this.searchParams = new g.URLSearchParams(m[5] || "");
      Object.defineProperty(this, "search", {
        get: function () { var s = self.searchParams.toString(); return s ? "?" + s : ""; },
        set: function (v) { self.searchParams = new g.URLSearchParams(String(v)); },
        enumerable: true, configurable: true,
      });
      Object.defineProperty(this, "origin", {
        get: function () {
          return (self.protocol === "http:" || self.protocol === "https:") ? self.protocol + "//" + self.host : "null";
        },
        enumerable: true, configurable: true,
      });
      Object.defineProperty(this, "href", {
        get: function () {
          var auth = self.host;
          if (self.username) auth = self.username + (self.password ? ":" + self.password : "") + "@" + auth;
          return self.protocol + "//" + auth + self.pathname + self.search + self.hash;
        },
        enumerable: true, configurable: true,
      });
    };
    g.URL.prototype.toString = function () { return this.href; };
    g.URL.prototype.toJSON = function () { return this.href; };

    // structuredClone: deep clone of plain data (objects/arrays/Date/Map/Set/
    // typed arrays), cycle-safe. Common in modern app code.
    g.structuredClone = function (input) {
      return (function clone(v, seen) {
        if (v === null || typeof v !== "object") return v;
        if (seen.has(v)) return seen.get(v);
        var out;
        if (Array.isArray(v)) { out = []; seen.set(v, out); for (var i = 0; i < v.length; i++) out[i] = clone(v[i], seen); return out; }
        if (v instanceof Date) return new Date(v.getTime());
        if (typeof ArrayBuffer === "function" && v instanceof ArrayBuffer) return v.slice(0);
        if (typeof ArrayBuffer === "function" && ArrayBuffer.isView(v)) return new v.constructor(v);
        if (typeof Map === "function" && v instanceof Map) {
          out = new Map(); seen.set(v, out);
          v.forEach(function (vv, kk) { out.set(clone(kk, seen), clone(vv, seen)); });
          return out;
        }
        if (typeof Set === "function" && v instanceof Set) {
          out = new Set(); seen.set(v, out);
          v.forEach(function (vv) { out.add(clone(vv, seen)); });
          return out;
        }
        out = {}; seen.set(v, out);
        for (var key in v) if (Object.prototype.hasOwnProperty.call(v, key)) out[key] = clone(v[key], seen);
        return out;
      })(input, new Map());
    };

    // ---- AbortController / AbortSignal ---------------------------------
    // Modern fetch/event code passes a signal and calls .abort(); absence broke it.
    g.AbortSignal = function AbortSignal() { this.aborted = false; this.reason = undefined; this.onabort = null; this.__l = []; };
    g.AbortSignal.prototype.addEventListener = function (t, fn) { if (t === "abort" && typeof fn === "function") this.__l.push(fn); };
    g.AbortSignal.prototype.removeEventListener = function (t, fn) { if (t === "abort") { var i = this.__l.indexOf(fn); if (i !== -1) this.__l.splice(i, 1); } };
    g.AbortSignal.prototype.dispatchEvent = function () { return true; };
    g.AbortSignal.prototype.throwIfAborted = function () { if (this.aborted) throw this.reason; };
    g.AbortSignal.abort = function (reason) { var s = new g.AbortSignal(); s.aborted = true; s.reason = reason !== undefined ? reason : new Error("AbortError"); return s; };
    g.AbortSignal.timeout = function () { return new g.AbortSignal(); };
    g.AbortController = function AbortController() { this.signal = new g.AbortSignal(); };
    g.AbortController.prototype.abort = function (reason) {
      var s = this.signal;
      if (s.aborted) return;
      s.aborted = true;
      s.reason = reason !== undefined ? reason : new Error("AbortError");
      var ev = { type: "abort", target: s, currentTarget: s };
      if (typeof s.onabort === "function") { try { s.onabort.call(s, ev); } catch (e) {} }
      var ls = s.__l.slice();
      for (var i = 0; i < ls.length; i++) { try { ls[i].call(s, ev); } catch (e) {} }
    };

    // ---- Blob ----------------------------------------------------------
    g.Blob = function Blob(parts, options) {
      parts = parts || [];
      var strs = [];
      for (var i = 0; i < parts.length; i++) {
        var p = parts[i];
        strs.push(typeof p === "string" ? p : (p && p.__blobText != null ? p.__blobText : String(p)));
      }
      this.__blobText = strs.join("");
      this.size = this.__blobText.length;
      this.type = (options && options.type) ? String(options.type) : "";
    };
    g.Blob.prototype.text = function () { return Promise.resolve(this.__blobText); };
    g.Blob.prototype.slice = function (s, e, type) { return new g.Blob([this.__blobText.slice(s, e)], { type: type || this.type }); };
    g.Blob.prototype.arrayBuffer = function () {
      var s = this.__blobText, buf = new ArrayBuffer(s.length), v = new Uint8Array(buf);
      for (var i = 0; i < s.length; i++) v[i] = s.charCodeAt(i) & 0xff;
      return Promise.resolve(buf);
    };

    // ---- FormData ------------------------------------------------------
    g.FormData = function FormData() { this.__e = []; };
    g.FormData.prototype.append = function (k, v) { this.__e.push([String(k), v]); };
    g.FormData.prototype.set = function (k, v) { k = String(k); this.__e = this.__e.filter(function (e) { return e[0] !== k; }); this.__e.push([k, v]); };
    g.FormData.prototype.get = function (k) { k = String(k); for (var i = 0; i < this.__e.length; i++) if (this.__e[i][0] === k) return this.__e[i][1]; return null; };
    g.FormData.prototype.getAll = function (k) { k = String(k); return this.__e.filter(function (e) { return e[0] === k; }).map(function (e) { return e[1]; }); };
    g.FormData.prototype.has = function (k) { return this.get(String(k)) !== null; };
    g.FormData.prototype.delete = function (k) { k = String(k); this.__e = this.__e.filter(function (e) { return e[0] !== k; }); };
    g.FormData.prototype.forEach = function (cb, t) { for (var i = 0; i < this.__e.length; i++) cb.call(t, this.__e[i][1], this.__e[i][0], this); };
    g.FormData.prototype.keys = function () { return this.__e.map(function (e) { return e[0]; }); };
    g.FormData.prototype.values = function () { return this.__e.map(function (e) { return e[1]; }); };
    g.FormData.prototype.entries = function () { return this.__e.map(function (e) { return [e[0], e[1]]; }); };
    // Serialize a FormData/Blob body to a string the host can send (URL-encoded
    // for FormData; the raw text for a Blob). Used by fetch/XHR below.
    g.__cerberusBodyToString = function (body) {
      if (body == null) return "";
      if (typeof body === "string") return body;
      if (body instanceof g.FormData) {
        return body.__e.map(function (e) {
          return encodeURIComponent(e[0]) + "=" + encodeURIComponent(typeof e[1] === "string" ? e[1] : (e[1] && e[1].__blobText != null ? e[1].__blobText : String(e[1])));
        }).join("&");
      }
      if (body instanceof g.Blob) return body.__blobText;
      if (body instanceof g.URLSearchParams) return body.toString();
      return String(body);
    };

    // ---- screen + window metrics ---------------------------------------
    g.screen = {
      width: vpW, height: vpH, availWidth: vpW, availHeight: vpH,
      colorDepth: 24, pixelDepth: 24,
    };
    window.innerWidth = vpW;
    window.innerHeight = vpH;
    window.outerWidth = vpW;
    window.outerHeight = vpH;
    window.devicePixelRatio = 1;
    window.scrollX = 0; window.scrollY = 0;
    window.pageXOffset = 0; window.pageYOffset = 0;
    window.scrollTo = function () {}; window.scrollBy = function () {}; window.scroll = function () {};

    // ---- performance ---------------------------------------------------
    // Real elapsed time from the native Date clock (QuickJS Date is wall-clock),
    // so performance.now() is monotonic, sub-call-distinct, and reflects actual
    // work — what timing-sensitive page code (and bot sensors) read.
    var __perfOrigin = Date.now();
    g.performance = {
      timeOrigin: __perfOrigin,
      now: function () { return Date.now() - __perfOrigin; },
      timing: { navigationStart: __perfOrigin },
      mark: function () {}, measure: function () {},
      getEntries: function () { return []; },
      getEntriesByType: function () { return []; },
      getEntriesByName: function () { return []; },
      clearMarks: function () {}, clearMeasures: function () {},
    };

    // ---- crypto (getRandomValues / randomUUID) -------------------------
    // getRandomValues fills an integer TypedArray with random values (its whole
    // purpose IS randomness — distinct from the fingerprint surfaces that are
    // farbled/consistent). subtle (digest/HMAC) is a separate follow-up.
    g.crypto = g.crypto || {};
    g.crypto.getRandomValues = function (arr) {
      if (!arr || typeof arr.length !== "number" || typeof arr.BYTES_PER_ELEMENT !== "number") {
        throw new TypeError("getRandomValues expects an integer TypedArray");
      }
      var bpe = arr.BYTES_PER_ELEMENT;
      var bound = bpe >= 4 ? 4294967296 : (1 << (8 * bpe));
      for (var i = 0; i < arr.length; i++) {
        arr[i] = Math.floor(Math.random() * bound);
      }
      return arr;
    };
    g.crypto.randomUUID = function () {
      var h = "0123456789abcdef", s = "";
      for (var i = 0; i < 36; i++) {
        if (i === 8 || i === 13 || i === 18 || i === 23) { s += "-"; }
        else if (i === 14) { s += "4"; }
        else if (i === 19) { s += h[8 + Math.floor(Math.random() * 4)]; }
        else { s += h[Math.floor(Math.random() * 16)]; }
      }
      return s;
    };
    // crypto.subtle.digest('SHA-256', data) -> Promise<ArrayBuffer>. A real,
    // spec-correct SHA-256 (sensors hash their fingerprint); other algorithms
    // reject. Pure JS with 32-bit ops so it needs no native bridge.
    var __K256 = [
      0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
      0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
      0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
      0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
      0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
      0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
      0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
      0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2];
    function __sha256(bytes) {
      function rotr(x, n) { return (x >>> n) | (x << (32 - n)); }
      var H = [0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19];
      var l = bytes.length;
      var withOne = l + 1;
      var pad = (56 - (withOne % 64) + 64) % 64;
      var total = withOne + pad + 8;
      var m = new Array(total);
      var i;
      for (i = 0; i < l; i++) { m[i] = bytes[i] & 0xff; }
      m[l] = 0x80;
      for (i = l + 1; i < total - 8; i++) { m[i] = 0; }
      var hi = Math.floor(l / 0x20000000);          // (l*8) >> 32
      var lo = (l * 8) >>> 0;                        // low 32 bits of bit-length
      m[total-8]=(hi>>>24)&0xff; m[total-7]=(hi>>>16)&0xff; m[total-6]=(hi>>>8)&0xff; m[total-5]=hi&0xff;
      m[total-4]=(lo>>>24)&0xff; m[total-3]=(lo>>>16)&0xff; m[total-2]=(lo>>>8)&0xff; m[total-1]=lo&0xff;
      var w = new Array(64), off, t;
      for (off = 0; off < total; off += 64) {
        for (t = 0; t < 16; t++) {
          w[t] = ((m[off+t*4]<<24)|(m[off+t*4+1]<<16)|(m[off+t*4+2]<<8)|(m[off+t*4+3])) >>> 0;
        }
        for (t = 16; t < 64; t++) {
          var s0 = rotr(w[t-15],7) ^ rotr(w[t-15],18) ^ (w[t-15] >>> 3);
          var s1 = rotr(w[t-2],17) ^ rotr(w[t-2],19) ^ (w[t-2] >>> 10);
          w[t] = (((w[t-16] + s0) >>> 0) + ((w[t-7] + s1) >>> 0)) >>> 0;
        }
        var a=H[0],b=H[1],c=H[2],d=H[3],e=H[4],f=H[5],gg=H[6],h=H[7];
        for (t = 0; t < 64; t++) {
          var S1 = rotr(e,6) ^ rotr(e,11) ^ rotr(e,25);
          var ch = (e & f) ^ ((~e) & gg);
          var temp1 = (((h + S1) >>> 0) + ((ch + ((__K256[t] + w[t]) >>> 0)) >>> 0)) >>> 0;
          var S0 = rotr(a,2) ^ rotr(a,13) ^ rotr(a,22);
          var maj = (a & b) ^ (a & c) ^ (b & c);
          var temp2 = (S0 + maj) >>> 0;
          h=gg; gg=f; f=e; e=(d + temp1) >>> 0; d=c; c=b; b=a; a=(temp1 + temp2) >>> 0;
        }
        H[0]=(H[0]+a)>>>0; H[1]=(H[1]+b)>>>0; H[2]=(H[2]+c)>>>0; H[3]=(H[3]+d)>>>0;
        H[4]=(H[4]+e)>>>0; H[5]=(H[5]+f)>>>0; H[6]=(H[6]+gg)>>>0; H[7]=(H[7]+h)>>>0;
      }
      var out = new Array(32);
      for (i = 0; i < 8; i++) {
        out[i*4]=(H[i]>>>24)&0xff; out[i*4+1]=(H[i]>>>16)&0xff;
        out[i*4+2]=(H[i]>>>8)&0xff; out[i*4+3]=H[i]&0xff;
      }
      return out;
    }
    g.crypto.subtle = g.crypto.subtle || {};
    g.crypto.subtle.digest = function (algo, data) {
      var name = String((typeof algo === "string") ? algo : (algo && algo.name) || "").toUpperCase();
      var bytes;
      if (data instanceof ArrayBuffer) { bytes = new Uint8Array(data); }
      else if (data && data.buffer instanceof ArrayBuffer) {
        bytes = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
      } else if (data && typeof data.length === "number") { bytes = data; }
      else { return Promise.reject(new TypeError("digest: invalid data")); }
      if (name !== "SHA-256") {
        return Promise.reject(new Error("digest: only SHA-256 is implemented"));
      }
      var h = __sha256(bytes);
      var buf = new ArrayBuffer(32), v = new Uint8Array(buf);
      for (var i = 0; i < 32; i++) { v[i] = h[i]; }
      return Promise.resolve(buf);
    };

    // ---- encoding (TextEncoder/Decoder, btoa/atob) ---------------------
    // Not ECMAScript — QuickJS doesn't ship them, but sensors encode/hash their
    // payload through them. Spec-correct UTF-8 + base64, guarded so a native
    // implementation (if ever present) wins.
    function __utf8Encode(str) {
      str = String(str); var out = [];
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c < 0x80) { out.push(c); }
        else if (c < 0x800) { out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f)); }
        else if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
          var c2 = str.charCodeAt(i + 1);
          if (c2 >= 0xdc00 && c2 <= 0xdfff) {
            var cp = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00); i++;
            out.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f),
                     0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
          } else { out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f)); }
        } else { out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f)); }
      }
      return out;
    }
    function __utf8Decode(bytes) {
      var s = "", i = 0, n = bytes.length;
      while (i < n) {
        var b = bytes[i++];
        if (b < 0x80) { s += String.fromCharCode(b); }
        else if (b < 0xe0) { s += String.fromCharCode(((b & 0x1f) << 6) | (bytes[i++] & 0x3f)); }
        else if (b < 0xf0) {
          s += String.fromCharCode(((b & 0x0f) << 12) | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f));
        } else {
          var cp = (((b & 0x07) << 18) | ((bytes[i++] & 0x3f) << 12)
                    | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f)) - 0x10000;
          s += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
        }
      }
      return s;
    }
    function __asBytes(data) {
      if (data instanceof ArrayBuffer) { return new Uint8Array(data); }
      if (data && data.buffer instanceof ArrayBuffer) {
        return new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
      }
      return data || [];
    }
    if (typeof g.TextEncoder !== "function") {
      g.TextEncoder = function TextEncoder() { this.encoding = "utf-8"; };
      g.TextEncoder.prototype.encode = function (str) {
        var a = __utf8Encode(str), u = new Uint8Array(a.length);
        for (var i = 0; i < a.length; i++) { u[i] = a[i]; }
        return u;
      };
    }
    if (typeof g.TextDecoder !== "function") {
      g.TextDecoder = function TextDecoder(enc) { this.encoding = enc || "utf-8"; };
      g.TextDecoder.prototype.decode = function (data) {
        return data == null ? "" : __utf8Decode(__asBytes(data));
      };
    }
    var __b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if (typeof g.btoa !== "function") {
      g.btoa = function (input) {
        input = String(input); var out = "", i = 0;
        while (i < input.length) {
          var c1 = input.charCodeAt(i++) & 0xff;
          var has2 = i < input.length, c2 = has2 ? input.charCodeAt(i++) & 0xff : 0;
          var has3 = i < input.length, c3 = has3 ? input.charCodeAt(i++) & 0xff : 0;
          var e3 = has2 ? (((c2 & 15) << 2) | (c3 >> 6)) : 64;
          var e4 = has3 ? (c3 & 63) : 64;
          out += __b64.charAt(c1 >> 2) + __b64.charAt(((c1 & 3) << 4) | (c2 >> 4))
               + (e3 === 64 ? "=" : __b64.charAt(e3)) + (e4 === 64 ? "=" : __b64.charAt(e4));
        }
        return out;
      };
    }
    if (typeof g.atob !== "function") {
      g.atob = function (input) {
        input = String(input).replace(/[^A-Za-z0-9+/=]/g, "");
        function val(ch) { if (ch === "=") { return 64; } var v = __b64.indexOf(ch); return v < 0 ? 0 : v; }
        var out = "", i = 0;
        while (i < input.length) {
          var e1 = val(input.charAt(i++)), e2 = val(input.charAt(i++));
          var e3 = val(input.charAt(i++)), e4 = val(input.charAt(i++));
          out += String.fromCharCode((e1 << 2) | (e2 >> 4));
          if (e3 !== 64) { out += String.fromCharCode(((e2 & 15) << 4) | (e3 >> 2)); }
          if (e4 !== 64) { out += String.fromCharCode(((e3 & 3) << 6) | e4); }
        }
        return out;
      };
    }

    // ---- storage (in-memory, RUN-SCOPED) -------------------------------
    // getItem/setItem/removeItem/clear/key/length plus index access via the
    // methods. These live for THIS RUN ONLY — there is no persistence across
    // run_page_scripts calls (the realm/prelude is reinstalled each time).
    function makeStorage() {
      // Real Storage semantics via a Proxy: stored keys are own ENUMERABLE
      // properties (so Object.keys/for-in see the DATA, not the methods), bracket
      // access (localStorage.foo / localStorage.foo = x) reads/writes the store,
      // and the methods stay non-enumerable. (The old plain-object version leaked
      // its method names through Object.keys and dropped bracket writes.)
      var data = Object.create(null);
      var api = {
        getItem: function (k) { k = String(k); return (k in data) ? data[k] : null; },
        setItem: function (k, v) { data[String(k)] = String(v); },
        removeItem: function (k) { delete data[String(k)]; },
        clear: function () { for (var k in data) delete data[k]; },
        key: function (i) { var ks = Object.keys(data); i = i >>> 0; return (i < ks.length) ? ks[i] : null; },
      };
      return new Proxy(api, {
        get: function (t, k) {
          if (k === "length") return Object.keys(data).length;
          if (typeof k !== "string") return api[k];
          if (k in api) return api[k];
          return (k in data) ? data[k] : undefined;
        },
        set: function (t, k, v) {
          if (typeof k === "string" && !(k in api)) data[k] = String(v);
          return true;
        },
        has: function (t, k) {
          return k === "length" || (typeof k === "string" && (k in data || k in api));
        },
        deleteProperty: function (t, k) { delete data[String(k)]; return true; },
        ownKeys: function () { return Object.keys(data); },
        getOwnPropertyDescriptor: function (t, k) {
          var kk = String(k);
          if (kk in data) return { value: data[kk], writable: true, enumerable: true, configurable: true };
          return undefined;
        },
      });
    }
    g.localStorage = makeStorage();
    g.sessionStorage = makeStorage();

    // ---- getComputedStyle (inline values only) -------------------------
    // Returns an object whose getPropertyValue(name) yields the element's
    // inline `style` value if present, else "". We do not run a layout/CSS
    // cascade (speed-first), so only inline declarations are visible. Also
    // exposed as best-effort direct property access.
    window.getComputedStyle = function (el) {
      var decls = Object.create(null);
      if (el && el.__type === ELEMENT_NODE) {
        // Cascaded computed values pushed from the layout engine (Rust) first,
        // then inline `style=` declarations, which win.
        if (el.__computedStyles) {
          for (var ck in el.__computedStyles) decls[ck] = el.__computedStyles[ck];
        }
        var inline = getAttr(el, "style");
        if (inline) {
          inline.split(";").forEach(function (d) {
            var c = d.indexOf(":");
            if (c === -1) return;
            var p = d.slice(0, c).trim();
            var v = d.slice(c + 1).trim();
            if (p) decls[p] = v;
          });
        }
      }
      return new Proxy(decls, {
        get: function (t, k) {
          if (k === "getPropertyValue") return function (p) { return t[p] || ""; };
          if (k === "getPropertyPriority") return function () { return ""; };
          return (k in t) ? t[k] : "";
        },
      });
    };

    // ---- matchMedia (honors width/height/orientation vs the viewport) ---
    function __cerberusEvalMedia(query, w, h) {
      return String(query).split(",").some(function (branch) {
        var re = /\(([a-z-]+)\s*:\s*([^)]+)\)/g, m, ok = true, any = false;
        while ((m = re.exec(branch)) !== null) {
          any = true;
          var name = m[1], val = m[2].trim(), px = parseInt(val, 10) || 0;
          if (name === "min-width") ok = ok && w >= px;
          else if (name === "max-width") ok = ok && w <= px;
          else if (name === "min-height") ok = ok && h >= px;
          else if (name === "max-height") ok = ok && h <= px;
          else if (name === "orientation") ok = ok && (val === "portrait" ? h >= w : w > h);
        }
        return any && ok;
      });
    }
    window.matchMedia = function (q) {
      var env = globalThis.__CERBERUS_ENV__ || { width: 0, height: 0 };
      return {
        matches: __cerberusEvalMedia(q, env.width | 0, env.height | 0),
        media: String(q), onchange: null,
        addListener: function () {}, removeListener: function () {},
        addEventListener: function () {}, removeEventListener: function () {},
        dispatchEvent: function () { return false; },
      };
    };

    // ---- history (real pushState/replaceState + popstate) --------------
    // SPAs route via history.pushState; the old no-op broke client-side URL
    // state. Maintains a stack, updates `location` (via URL resolution), and
    // fires popstate on back/forward/go.
    var __histStack = [{ state: null, url: locationObj.href }];
    var __histIndex = 0;
    function __applyLocation(href) {
      try {
        var u = new g.URL(String(href), locationObj.href);
        locationObj.href = u.href; locationObj.protocol = u.protocol;
        locationObj.host = u.host; locationObj.hostname = u.hostname; locationObj.port = u.port;
        locationObj.origin = u.origin; locationObj.pathname = u.pathname;
        locationObj.search = u.search; locationObj.hash = u.hash;
      } catch (e) {}
    }
    function __firePopstate(state) {
      var ev = { type: "popstate", state: state, target: g, currentTarget: g };
      if (typeof g.onpopstate === "function") { try { g.onpopstate.call(g, ev); } catch (e) {} }
      var ls = (window.__listeners && window.__listeners["popstate"]) || [];
      var c = ls.slice();
      for (var i = 0; i < c.length; i++) { try { c[i].call(g, ev); } catch (e) {} }
    }
    g.history = {
      scrollRestoration: "auto",
      get length() { return __histStack.length; },
      get state() { return __histStack[__histIndex].state; },
      pushState: function (state, title, url) {
        if (url != null) __applyLocation(url);
        __histStack = __histStack.slice(0, __histIndex + 1);
        __histStack.push({ state: state, url: locationObj.href });
        __histIndex = __histStack.length - 1;
      },
      replaceState: function (state, title, url) {
        if (url != null) __applyLocation(url);
        __histStack[__histIndex] = { state: state, url: locationObj.href };
      },
      back: function () { this.go(-1); },
      forward: function () { this.go(1); },
      go: function (delta) {
        delta = delta || 0;
        var ni = __histIndex + delta;
        if (delta === 0 || ni < 0 || ni >= __histStack.length) return;
        __histIndex = ni;
        var entry = __histStack[ni];
        __applyLocation(entry.url);
        __firePopstate(entry.state);
      },
    };

    // ---- install: snapshot -> JS tree ----------------------------------
    g.__cerberusInstallDOM = function () {
      try {
        var snap = g.__CERBERUS_DOM__;
        if (!snap || !Array.isArray(snap.nodes)) return;

        // Reset indices (install may run once per page).
        byId = Object.create(null);
        idCounter = 1;

        // First pass: materialize bare nodes so children can be linked by id.
        var raw = Object.create(null);
        var maxId = 0;
        for (var i = 0; i < snap.nodes.length; i++) {
          var n = snap.nodes[i];
          if (!n || typeof n.id !== "number") continue;
          if (n.id > maxId) maxId = n.id;
          var node;
          if (n.kind === "text") {
            node = makeText(typeof n.text === "string" ? n.text : "", n.id);
          } else {
            node = makeElement(typeof n.tag === "string" ? n.tag : "div", n.id);
            if (Array.isArray(n.attrs)) {
              for (var a = 0; a < n.attrs.length; a++) {
                var pair = n.attrs[a];
                if (Array.isArray(pair) && pair.length >= 2) setAttr(node, String(pair[0]), pair[1]);
              }
            }
          }
          raw[n.id] = { node: node, spec: n };
        }
        idCounter = maxId + 1;

        // Second pass: link children in order.
        for (var key in raw) {
          var entry = raw[key];
          var spec = entry.spec;
          if (spec.kind === "element" && Array.isArray(spec.children)) {
            for (var c = 0; c < spec.children.length; c++) {
              var child = raw[spec.children[c]];
              if (child) appendChild(entry.node, child.node);
            }
          }
        }

        // Root + well-known elements.
        var rootEntry = raw[snap.root];
        var root = rootEntry ? rootEntry.node : makeElement("#root");
        document.__root = root;

        // documentElement = the <html> if present, else the snapshot root.
        var htmlEl = null, headEl = null, bodyEl = null, titleEl = null;
        walkElements(root, function (el) {
          var tag = el.__tag;
          if (!htmlEl && tag === "html") htmlEl = el;
          if (!headEl && tag === "head") headEl = el;
          if (!bodyEl && tag === "body") bodyEl = el;
          if (!titleEl && tag === "title") titleEl = el;
        });
        if (root.__tag === "html") htmlEl = root;

        document.documentElement = htmlEl || root;
        document.head = headEl || null;
        document.body = bodyEl || null;
        document.__titleEl = titleEl || null;
      } catch (e) {
        // Install must never throw; leave document in whatever partial state.
      }
    };

    // ---- fire-load -----------------------------------------------------
    g.__cerberusFireLoad = function () {
      try {
        document.readyState = "complete";
        var dcl = { type: "DOMContentLoaded", target: document, bubbles: false, cancelable: false };
        try { document.dispatchEvent(dcl); } catch (e) {}
        var loadDoc = { type: "load", target: document, bubbles: false, cancelable: false };
        try { document.dispatchEvent(loadDoc); } catch (e) {}
        var loadWin = { type: "load", target: window, bubbles: false, cancelable: false };
        try { window.dispatchEvent(loadWin); } catch (e) {}
      } catch (e) {}
    };

    // ---- form-control value injection (M12b input events) -------------
    // Set a control's live value from Rust before firing an `input` event, so a
    // handler reads the just-typed e.target.value. Safe no-op for a missing id.
    g.__cerberusSetValue = function (nodeId, value) {
      try {
        var n = byId[nodeId];
        if (n) n.value = String(value);
      } catch (e) {}
    };

    // Layout geometry pushed from Rust after layout, keyed by JS node id, so
    // getBoundingClientRect returns real boxes (ADR-0021).
    g.__cerberusSetGeometry = function (geom) {
      try {
        for (var gid in geom) {
          var gn = byId[gid];
          if (gn) gn.__geometry = geom[gid];
        }
      } catch (e) {}
    };

    // Cascaded computed styles pushed from Rust, keyed by JS node id, so
    // getComputedStyle reflects the cascade, not just inline (ADR-0021).
    g.__cerberusSetComputedStyles = function (styles) {
      try {
        for (var sid in styles) {
          var sn = byId[sid];
          if (sn) sn.__computedStyles = styles[sid];
        }
      } catch (e) {}
    };

    // ---- event dispatch (M12b) -----------------------------------------
    // Dispatch a real DOM event at the node with the given JS id, running its
    // listeners through the target and bubbling phases (capture is not
    // modelled). `init` carries extra event fields (e.g. {key:"Enter"} or
    // {button:0}) and may set bubbles/cancelable (both default true).
    // document/window participate in bubbling. The mutated DOM is read
    // separately via __cerberusSerializeDOM; this returns {dispatched,
    // defaultPrevented} as 1/0 (the wire JSON has no boolean type) so the Rust
    // side can decide whether to run the browser default action.
    g.__cerberusDispatch = function (nodeId, type, init) {
      try {
        type = String(type);
        var target = byId[nodeId];
        if (!target) return JSON.stringify({ dispatched: 0, defaultPrevented: 0 });

        var ev = {
          type: type,
          target: target,
          currentTarget: null,
          eventPhase: 0,
          bubbles: !(init && init.bubbles === false),
          cancelable: !(init && init.cancelable === false),
          defaultPrevented: false,
          isTrusted: true,
          timeStamp: 0,
          __stop: false,
          __stopImmediate: false,
          preventDefault: function () { if (this.cancelable) this.defaultPrevented = true; },
          stopPropagation: function () { this.__stop = true; },
          stopImmediatePropagation: function () { this.__stop = true; this.__stopImmediate = true; },
        };
        // Copy caller-supplied fields (key, code, button, detail, …) without
        // clobbering the machinery above.
        if (init && typeof init === "object") {
          for (var k in init) {
            if (Object.prototype.hasOwnProperty.call(init, k) && !(k in ev)) ev[k] = init[k];
          }
        }

        // Full capture → target → bubble propagation (shared with dispatchEvent).
        __propagate(target, ev);

        return JSON.stringify({ dispatched: 1, defaultPrevented: ev.defaultPrevented ? 1 : 0 });
      } catch (e) {
        return JSON.stringify({ dispatched: 0, defaultPrevented: 0 });
      }
    };

    // ---- fetch (enqueue + host-drain + resolve, ADR-0014) --------------
    // fetch() does NOT call native code (the engine seam is eval-only). It
    // pushes a request descriptor onto a per-realm queue and returns a real
    // (native QuickJS) Promise whose resolve/reject are stashed under a
    // monotonic id. The Rust host drains the queue via __cerberusTakeFetches,
    // performs each request through a host FetchClient, and settles the Promise
    // via __cerberusResolveFetch / __cerberusRejectFetch. Settling schedules the
    // .then microtasks, which the bounded event loop drains; fetches scheduled
    // from a .then surface on the host's next drain round (under caps). Every
    // entry point is try/catch-guarded and never throws across the seam.
    if (!Array.isArray(g.__cerberusFetchQueue)) g.__cerberusFetchQueue = [];
    if (!g.__cerberusFetchPending) g.__cerberusFetchPending = Object.create(null);
    if (typeof g.__cerberusFetchId !== "number") g.__cerberusFetchId = 1;
    // Injected-<script> load queue (ADR-0060), mirroring the fetch queue.
    if (!Array.isArray(g.__cerberusScriptQueue)) g.__cerberusScriptQueue = [];
    if (!g.__cerberusScriptPending) g.__cerberusScriptPending = Object.create(null);
    if (typeof g.__cerberusScriptId !== "number") g.__cerberusScriptId = 1;

    // ---- Headers (case-insensitive, minimal) ---------------------------
    // Backed by an ordered array of [originalName, value]; lookups fold case.
    // Constructed from a plain object, an array of [name,value] pairs, or
    // another Headers (anything with .forEach). Used by Response.
    function makeHeaders(init) {
      var list = [];
      function indexOf(name) {
        var lc = String(name).toLowerCase();
        for (var i = 0; i < list.length; i++) if (list[i][0].toLowerCase() === lc) return i;
        return -1;
      }
      var h = {
        append: function (name, value) { list.push([String(name), String(value)]); },
        set: function (name, value) {
          var i = indexOf(name);
          if (i === -1) list.push([String(name), String(value)]);
          else list[i] = [String(name), String(value)];
        },
        get: function (name) {
          // The spec joins multiple same-name values with ", "; do likewise.
          var lc = String(name).toLowerCase(), out = null;
          for (var i = 0; i < list.length; i++) {
            if (list[i][0].toLowerCase() === lc) out = (out === null) ? list[i][1] : out + ", " + list[i][1];
          }
          return out;
        },
        has: function (name) { return indexOf(name) !== -1; },
        "delete": function (name) {
          var lc = String(name).toLowerCase();
          for (var i = list.length - 1; i >= 0; i--) if (list[i][0].toLowerCase() === lc) list.splice(i, 1);
        },
        forEach: function (cb, thisArg) {
          for (var i = 0; i < list.length; i++) cb.call(thisArg, list[i][1], list[i][0], h);
        },
        entries: function () { return list.map(function (p) { return [p[0], p[1]]; }); },
        keys: function () { return list.map(function (p) { return p[0]; }); },
        values: function () { return list.map(function (p) { return p[1]; }); },
        __pairs: function () { return list.map(function (p) { return [p[0], p[1]]; }); },
      };
      try {
        if (init) {
          if (typeof init.forEach === "function" && !Array.isArray(init)) {
            init.forEach(function (v, k) { h.append(k, v); });
          } else if (Array.isArray(init)) {
            for (var i = 0; i < init.length; i++) {
              var pair = init[i];
              if (pair && pair.length >= 2) h.append(pair[0], pair[1]);
            }
          } else if (typeof init === "object") {
            for (var k in init) {
              if (Object.prototype.hasOwnProperty.call(init, k)) h.append(k, init[k]);
            }
          }
        }
      } catch (e) {}
      return h;
    }
    g.Headers = function (init) { return makeHeaders(init); };

    // ---- normalize an init.headers into [[name,value],...] -------------
    function normalizeHeaders(init) {
      var out = [];
      try {
        var hh = init && init.headers;
        if (!hh) return out;
        if (typeof hh.forEach === "function" && !Array.isArray(hh)) {
          hh.forEach(function (v, k) { out.push([String(k), String(v)]); });
        } else if (Array.isArray(hh)) {
          for (var i = 0; i < hh.length; i++) {
            var pair = hh[i];
            if (pair && pair.length >= 2) out.push([String(pair[0]), String(pair[1])]);
          }
        } else if (typeof hh === "object") {
          for (var k in hh) {
            if (Object.prototype.hasOwnProperty.call(hh, k)) out.push([String(k), String(hh[k])]);
          }
        }
      } catch (e) {}
      return out;
    }

    // ---- Response factory (body is a UTF-8 text string in v1) ----------
    function makeResponse(status, statusText, url, headerPairs, bodyText) {
      status = status >>> 0;
      var resp = {
        ok: (status >= 200 && status <= 299),
        status: status,
        statusText: String(statusText == null ? "" : statusText),
        url: String(url == null ? "" : url),
        redirected: false,
        type: "basic",
        headers: makeHeaders(headerPairs || []),
        bodyUsed: false,
        _bodyText: String(bodyText == null ? "" : bodyText),
        text: function () { return Promise.resolve(this._bodyText); },
        json: function () {
          var t = this._bodyText;
          return new Promise(function (resolve, reject) {
            try { resolve(JSON.parse(t)); } catch (e) { reject(e); }
          });
        },
        clone: function () {
          var c = makeResponse(this.status, this.statusText, this.url, this.headers.__pairs(), this._bodyText);
          c.redirected = this.redirected;
          c.type = this.type;
          return c;
        },
      };
      return resp;
    }

    g.fetch = function (input, init) {
      try {
        var url;
        if (input && typeof input === "object" && input.url != null) url = String(input.url);
        else url = String(input);
        if (init && init.signal && init.signal.aborted) {
          return Promise.reject(init.signal.reason || new Error("AbortError"));
        }
        var method = (init && init.method) ? String(init.method).toUpperCase() : "GET";
        var headers = normalizeHeaders(init);
        var body = (init && init.body != null) ? g.__cerberusBodyToString(init.body) : "";
        var id = g.__cerberusFetchId++;
        g.__cerberusFetchQueue.push({ id: id, url: url, method: method, headers: headers, body: body });
        return new Promise(function (resolve, reject) {
          g.__cerberusFetchPending[id] = { resolve: resolve, reject: reject };
        });
      } catch (e) {
        // A malformed call still yields a rejected Promise (never throws sync).
        return Promise.reject(new TypeError("fetch failed: " + String(e)));
      }
    };

    // Drain the pending request queue as a JSON string, then CLEAR it. Each
    // entry is {id:<int>,url,method,headers:[[n,v]...],body}. "[]" when empty.
    g.__cerberusTakeFetches = function () {
      try {
        var q = g.__cerberusFetchQueue;
        if (!Array.isArray(q) || q.length === 0) return "[]";
        g.__cerberusFetchQueue = [];
        return JSON.stringify(q);
      } catch (e) {
        return "[]";
      }
    };

    // Drain the injected-<script> load queue; entries are {id,url} (ADR-0060).
    g.__cerberusTakeScriptLoads = function () {
      try {
        var q = g.__cerberusScriptQueue;
        if (!Array.isArray(q) || q.length === 0) return "[]";
        g.__cerberusScriptQueue = [];
        return JSON.stringify(q);
      } catch (e) {
        return "[]";
      }
    };

    // Drain script-set document.cookie assignments (raw, with attributes) so the
    // host can apply them to the real consent-gated jar as Set-Cookie.
    g.__cerberusTakeCookieWrites = function () {
      try {
        var w = (g.document && g.document.__cookieWrites) || [];
        if (!Array.isArray(w) || w.length === 0) return "[]";
        g.document.__cookieWrites = [];
        return JSON.stringify(w);
      } catch (e) {
        return "[]";
      }
    };

    // The host ran (ok=1) or failed (ok=0) the script for `id`; fire its event so
    // the loader's callback chain (webpack `__webpack_require__.l`, etc.) continues.
    g.__cerberusScriptLoaded = function (id, ok) {
      try {
        var node = g.__cerberusScriptPending[id];
        if (!node) return;
        delete g.__cerberusScriptPending[id];
        var ev = { type: ok ? "load" : "error", target: node, currentTarget: node };
        var handler = ok ? node.onload : node.onerror;
        if (typeof handler === "function") { try { handler.call(node, ev); } catch (e) {} }
        if (typeof node.dispatchEvent === "function") {
          try { node.dispatchEvent(ev); } catch (e) {}
        }
      } catch (e) {}
    };

    // Resolve the Promise for `id` with a Response built from `resp` =
    // {status:<int>,statusText,url,headers:[[n,v]...],body}. ok/redirected are
    // computed here (the wire JSON carries no booleans). Unknown id is a no-op.
    g.__cerberusResolveFetch = function (id, resp) {
      try {
        var entry = g.__cerberusFetchPending[id];
        if (!entry) return;
        delete g.__cerberusFetchPending[id];
        resp = resp || {};
        // XHR rides the same queue but settles into its state machine, not a Promise.
        if (entry.xhr) { entry.settle(resp); return; }
        var response = makeResponse(
          (typeof resp.status === "number") ? resp.status : 200,
          resp.statusText, resp.url, resp.headers, resp.body
        );
        entry.resolve(response);
      } catch (e) {}
    };

    // Reject the Promise for `id` with a TypeError(message). Unknown id no-op.
    g.__cerberusRejectFetch = function (id, message) {
      try {
        var entry = g.__cerberusFetchPending[id];
        if (!entry) return;
        delete g.__cerberusFetchPending[id];
        if (entry.xhr) { entry.fail(String(message)); return; }
        entry.reject(new TypeError(String(message)));
      } catch (e) {}
    };

    // ---- XMLHttpRequest (async; rides the same fetch queue) -------------
    // Older analytics/ad/bot libraries use XHR, not fetch. open/send enqueue a
    // request like fetch(); the host's drain settles it into the XHR state
    // machine, firing readystatechange/load/error/loadend.
    g.XMLHttpRequest = function XMLHttpRequest() {
      this.readyState = 0; this.status = 0; this.statusText = "";
      this.responseText = ""; this.response = ""; this.responseType = ""; this.responseURL = "";
      this.timeout = 0; this.withCredentials = false;
      this.onreadystatechange = null; this.onload = null; this.onerror = null;
      this.onloadend = null; this.onabort = null; this.ontimeout = null;
      this.__method = "GET"; this.__url = ""; this.__reqHeaders = [];
      this.__respHeaders = []; this.__listeners = {}; this.__sent = false;
    };
    var XHRP = g.XMLHttpRequest.prototype;
    XHRP.UNSENT = 0; XHRP.OPENED = 1; XHRP.HEADERS_RECEIVED = 2; XHRP.LOADING = 3; XHRP.DONE = 4;
    g.XMLHttpRequest.UNSENT = 0; g.XMLHttpRequest.OPENED = 1; g.XMLHttpRequest.HEADERS_RECEIVED = 2;
    g.XMLHttpRequest.LOADING = 3; g.XMLHttpRequest.DONE = 4;
    XHRP.open = function (method, url) {
      this.__method = String(method || "GET").toUpperCase();
      this.__url = String(url);
      this.__reqHeaders = []; this.__sent = false;
      this.readyState = 1; this.__fire("readystatechange");
    };
    XHRP.setRequestHeader = function (name, value) {
      this.__reqHeaders.push([String(name), String(value)]);
    };
    XHRP.getResponseHeader = function (name) {
      name = String(name).toLowerCase();
      for (var i = 0; i < this.__respHeaders.length; i++) {
        if (String(this.__respHeaders[i][0]).toLowerCase() === name) return this.__respHeaders[i][1];
      }
      return null;
    };
    XHRP.getAllResponseHeaders = function () {
      var out = "";
      for (var i = 0; i < this.__respHeaders.length; i++) {
        out += this.__respHeaders[i][0] + ": " + this.__respHeaders[i][1] + "\r\n";
      }
      return out;
    };
    XHRP.addEventListener = function (t, fn) {
      (this.__listeners[t] = this.__listeners[t] || []).push(fn);
    };
    XHRP.removeEventListener = function (t, fn) {
      var l = this.__listeners[t]; if (!l) return;
      var i = l.indexOf(fn); if (i !== -1) l.splice(i, 1);
    };
    XHRP.__fire = function (type) {
      var ev = { type: type, target: this, currentTarget: this };
      if (typeof this["on" + type] === "function") { try { this["on" + type].call(this, ev); } catch (e) {} }
      var l = this.__listeners[type];
      if (l) { var c = l.slice(); for (var i = 0; i < c.length; i++) { try { c[i].call(this, ev); } catch (e) {} } }
    };
    XHRP.send = function (body) {
      var self = this;
      if (this.readyState !== 1 || this.__sent) { throw new Error("XHR: invalid state for send()"); }
      this.__sent = true;
      var id = g.__cerberusFetchId++;
      g.__cerberusFetchQueue.push({
        id: id, url: this.__url, method: this.__method,
        headers: this.__reqHeaders.slice(), body: g.__cerberusBodyToString(body)
      });
      g.__cerberusFetchPending[id] = {
        xhr: true,
        settle: function (resp) {
          self.status = (typeof resp.status === "number") ? resp.status : 0;
          self.statusText = resp.statusText || "";
          self.responseURL = resp.url || self.__url;
          self.__respHeaders = Array.isArray(resp.headers) ? resp.headers : [];
          var text = (resp.body != null) ? String(resp.body) : "";
          self.responseText = text;
          if (self.responseType === "json") {
            try { self.response = JSON.parse(text); } catch (e) { self.response = null; }
          } else { self.response = text; }
          self.readyState = 2; self.__fire("readystatechange");
          self.readyState = 3; self.__fire("readystatechange");
          self.readyState = 4; self.__fire("readystatechange");
          self.__fire("load"); self.__fire("loadend");
        },
        fail: function () {
          self.status = 0; self.readyState = 4; self.__fire("readystatechange");
          self.__fire("error"); self.__fire("loadend");
        }
      };
    };
    XHRP.abort = function () {
      if (this.readyState !== 0 && this.readyState !== 4) {
        this.readyState = 4; this.status = 0;
        this.__fire("readystatechange"); this.__fire("abort"); this.__fire("loadend");
      }
    };

    // ---- serialize: JS tree -> wire JSON -------------------------------
    g.__cerberusSerializeDOM = function () {
      try {
        var root = document.documentElement || document.__root;
        if (!root) return JSON.stringify({ root: 0, nodes: [] });

        var nodes = [];
        var seen = Object.create(null);

        function ensureId(node) {
          if (typeof node.__id !== "number" || seen[node.__id]) {
            // Assign a fresh id if missing or colliding with an already-emitted id.
            if (typeof node.__id !== "number") node.__id = freshId();
          }
          return node.__id;
        }

        function emit(node) {
          var id = ensureId(node);
          if (seen[id]) return id;
          seen[id] = true;
          if (node.__type === TEXT_NODE) {
            nodes.push({ id: id, kind: "text", text: node.__text });
            return id;
          }
          var attrs = [];
          for (var a = 0; a < node.__attrs.length; a++) attrs.push([node.__attrs[a][0], node.__attrs[a][1]]);
          // A node whose innerHTML was set carries a raw fragment instead of JS
          // children. Emit it with an "innerHTML" field (no children); Rust
          // reparses it with the real HTML parser at rebuild time.
          if (typeof node.__rawHTML === "string") {
            nodes.push({ id: id, kind: "element", tag: node.__tag, attrs: attrs, innerHTML: node.__rawHTML });
            return id;
          }
          var childIds = [];
          for (var i = 0; i < node.__kids.length; i++) childIds.push(emit(node.__kids[i]));
          nodes.push({ id: id, kind: "element", tag: node.__tag, attrs: attrs, children: childIds });
          return id;
        }

        var rootId = emit(root);
        return JSON.stringify({ root: rootId, nodes: nodes });
      } catch (e) {
        return JSON.stringify({ root: 0, nodes: [{ id: 0, kind: "element", tag: "html", attrs: [], children: [] }] });
      }
    };
  } catch (e) {
    // The model must never throw at install time.
  }
})();
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk two documents in lockstep and assert structural equality: same kind,
    /// tag, attributes (in order), text, and recursively the same children.
    fn assert_same(a: NodeRef<'_>, b: NodeRef<'_>) {
        assert_eq!(a.is_element(), b.is_element(), "element-ness differs");
        assert_eq!(a.is_text(), b.is_text(), "text-ness differs");
        if a.is_text() {
            assert_eq!(a.text(), b.text(), "text differs");
            return;
        }
        assert_eq!(a.tag(), b.tag(), "tag differs");
        assert_eq!(a.attrs(), b.attrs(), "attrs differ for <{}>", a.tag());
        let ac: Vec<_> = a.children().collect();
        let bc: Vec<_> = b.children().collect();
        assert_eq!(ac.len(), bc.len(), "child count differs for <{}>", a.tag());
        for (ca, cb) in ac.into_iter().zip(bc) {
            assert_same(ca, cb);
        }
    }

    /// Build a moderately nested document with attributes and tricky text.
    fn sample_document() -> Document {
        let mut b = DocumentBuilder::new();
        // Tricky text: quotes, backslash, newline, angle brackets/ampersand, and
        // multi-byte Unicode — all must survive the JSON round-trip verbatim.
        let tricky = b.text("q\"u\\o\nte <tag> & café 日本語");
        let span = b.element_attrs(
            "span",
            vec![
                ("class".into(), "a b".into()),
                ("data-x".into(), "1".into()),
            ],
            [tricky],
        );
        let p_text = b.text("hello world");
        let p = b.element("p", [p_text, span]);
        let title_text = b.text("Title & Co");
        let title = b.element("title", [title_text]);
        let head = b.element("head", [title]);
        let body = b.element_attrs("body", vec![("id".into(), "main".into())], [p]);
        let html = b.element("html", [head, body]);
        b.finish(html)
    }

    #[test]
    fn serialize_then_rebuild_is_structurally_identical() {
        let doc = sample_document();
        let wire = serialize_document(&doc);
        let rebuilt = rebuild_document(&wire).expect("rebuild");
        assert_same(doc.root(), rebuilt.root());
    }

    #[test]
    fn serialize_then_rebuild_preserves_unicode_and_escapes() {
        // Focused check that the gnarliest text comes back byte-for-byte.
        let mut b = DocumentBuilder::new();
        let t = b.text("\"\\\n\r\t<>& café 日本語 \u{1F600}");
        let root = b.element("div", [t]);
        let doc = b.finish(root);

        let rebuilt = rebuild_document(&serialize_document(&doc)).expect("rebuild");
        let child = rebuilt.root().children().next().expect("text child");
        assert_eq!(child.text(), Some("\"\\\n\r\t<>& café 日本語 \u{1F600}"));
    }

    #[test]
    fn rebuild_rejects_malformed_json() {
        for bad in [
            "",                            // empty
            "{",                           // truncated object
            "{\"root\":0,\"nodes\":",      // truncated
            "not json at all",             // garbage
            "{\"root\":1.5,\"nodes\":[]}", // float (parser rejects)
            "[1,2,3",                      // unterminated array
        ] {
            match rebuild_document(bad) {
                Err(BridgeError::Json(_)) => {}
                other => panic!("expected Json error for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn rebuild_rejects_structurally_invalid_documents() {
        // Parses as JSON, but the shape is wrong → Structure errors.
        let cases = [
            r#"{"nodes":[]}"#,                                   // missing root
            r#"{"root":5,"nodes":[]}"#,                          // root id absent
            r#"{"root":0,"nodes":[{"id":0,"kind":"mystery"}]}"#, // unknown kind
            r#"{"root":0,"nodes":[{"id":0,"kind":"element","tag":"a","attrs":[],"children":[9]}]}"#, // dangling child
        ];
        for bad in cases {
            match rebuild_document(bad) {
                Err(BridgeError::Structure(_)) => {}
                other => panic!("expected Structure error for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn rebuild_renumbers_arbitrary_wire_ids() {
        // Wire ids need not be contiguous or ordered; rebuild must still link
        // them correctly via fresh NodeIds.
        let wire = r#"{"root":100,"nodes":[
            {"id":100,"kind":"element","tag":"ul","attrs":[],"children":[42,7]},
            {"id":7,"kind":"element","tag":"li","attrs":[],"children":[3]},
            {"id":42,"kind":"element","tag":"li","attrs":[],"children":[]},
            {"id":3,"kind":"text","text":"second"}
        ]}"#;
        let doc = rebuild_document(wire).expect("rebuild");
        let root = doc.root();
        assert_eq!(root.tag(), "ul");
        let kids: Vec<_> = root.children().collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].tag(), "li");
        assert!(kids[0].children().next().is_none(), "first li is empty");
        assert_eq!(kids[1].tag(), "li");
        assert_eq!(kids[1].text_content(), "second");
    }

    #[test]
    fn serialize_emits_expected_shape() {
        // Spot-check the wire text for a tiny known document.
        let mut b = DocumentBuilder::new();
        let t = b.text("hi");
        let div = b.element_attrs("div", vec![("id".into(), "x".into())], [t]);
        let doc = b.finish(div);
        let wire = serialize_document(&doc);
        // Root is the div (id 1), text is id 0.
        assert!(wire.starts_with("{\"root\":1,\"nodes\":["), "got {wire}");
        assert!(wire.contains("\"kind\":\"element\""));
        assert!(wire.contains("\"tag\":\"div\""));
        assert!(wire.contains("[\"id\",\"x\"]"));
        assert!(wire.contains("\"kind\":\"text\",\"text\":\"hi\""));
    }

    #[test]
    fn rebuild_grafts_inner_html_fragment() {
        // A wire node carrying `innerHTML` (and no children) is reparsed in Rust:
        // the fragment's top-level nodes become the node's real children.
        let wire = r#"{"root":1,"nodes":[
            {"id":1,"kind":"element","tag":"div","attrs":[["id","x"]],"innerHTML":"<b>hi</b><i>there</i>"}
        ]}"#;
        let doc = rebuild_document(wire).expect("rebuild");
        let root = doc.root();
        assert_eq!(root.tag(), "div");
        assert_eq!(root.attr("id"), Some("x"));
        let kids: Vec<_> = root.children().filter(|c| c.is_element()).collect();
        assert_eq!(kids.len(), 2, "two grafted element children");
        assert_eq!(kids[0].tag(), "b");
        assert_eq!(kids[0].text_content(), "hi");
        assert_eq!(kids[1].tag(), "i");
        assert_eq!(kids[1].text_content(), "there");
    }

    #[test]
    fn rebuild_inner_html_takes_precedence_over_children() {
        // If a node carries BOTH `innerHTML` and `children`, the reparsed
        // fragment wins (the JS setter clears children, but we tolerate both).
        let wire = r#"{"root":1,"nodes":[
            {"id":1,"kind":"element","tag":"div","attrs":[],"children":[2],"innerHTML":"<span>fromhtml</span>"},
            {"id":2,"kind":"text","text":"fromchildren"}
        ]}"#;
        let doc = rebuild_document(wire).expect("rebuild");
        let root = doc.root();
        let kids: Vec<_> = root.children().collect();
        assert_eq!(
            kids.len(),
            1,
            "only the grafted fragment, not the text child"
        );
        assert_eq!(kids[0].tag(), "span");
        assert_eq!(kids[0].text_content(), "fromhtml");
    }

    #[test]
    fn rebuild_inner_html_nested_fragment_grafts_deeply() {
        // Nested markup grafts as a real subtree (exercises copy_subtree depth).
        let wire = r#"{"root":1,"nodes":[
            {"id":1,"kind":"element","tag":"ul","attrs":[],"innerHTML":"<li class=\"a\">one</li><li>two<b>!</b></li>"}
        ]}"#;
        let doc = rebuild_document(wire).expect("rebuild");
        let root = doc.root();
        assert_eq!(root.tag(), "ul");
        let lis: Vec<_> = root.children().filter(|c| c.is_element()).collect();
        assert_eq!(lis.len(), 2);
        assert_eq!(lis[0].tag(), "li");
        assert_eq!(lis[0].attr("class"), Some("a"));
        assert_eq!(lis[0].text_content(), "one");
        // Second <li> has nested <b>.
        let b = lis[1]
            .children()
            .find(|c| c.is_element() && c.tag() == "b")
            .expect("nested <b>");
        assert_eq!(b.text_content(), "!");
    }

    #[test]
    fn rebuild_inner_html_empty_fragment_yields_no_children() {
        // An empty fragment leaves the node childless (no panic, no stray nodes).
        let wire = r#"{"root":1,"nodes":[
            {"id":1,"kind":"element","tag":"div","attrs":[],"innerHTML":""}
        ]}"#;
        let doc = rebuild_document(wire).expect("rebuild");
        assert_eq!(doc.root().tag(), "div");
        assert!(doc.root().children().next().is_none(), "no children");
    }
}

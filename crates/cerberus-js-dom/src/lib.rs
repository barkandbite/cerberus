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
    /// The cookies the sealed jar would send to this origin, as a
    /// `"name=value; name=value"` string (no attributes) — seeds `document.cookie`
    /// so script reads see the instance's cookies. Empty when none. Writes back
    /// to `document.cookie` are captured separately (`take_cookie_writes`).
    pub cookie: String,
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
pub fn run_page_scripts(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    document: &Document,
    scripts: &[String],
    env: &PageEnv,
) -> Result<Document, BridgeError> {
    install_page(engine, realm, document, env)?;
    run_scripts(engine, realm, scripts)?;
    fire_load(engine, realm)?;
    run_event_loop(engine, realm, EventLoopBudget::default())?;
    Ok(serialize_dom(engine, realm)?.document)
}

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
        "globalThis.__CERBERUS_ENV__ = {{ url: {}, width: {}, height: {}, userAgent: {}, cookie: {} }};",
        js_string(&env.url),
        env.viewport.0,
        env.viewport.1,
        js_string(&env.user_agent),
        js_string(&env.cookie),
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
/// Evals `__cerberusTakeFetches()` (which `JSON.stringify`s the queue then empties
/// it) and parses the array of `{id,url,method,headers:[[n,v]…],body}` objects. An
/// empty queue yields an empty `Vec`. A malformed entry is skipped rather than
/// failing the whole drain — a single bad descriptor must not strand the rest.
///
/// A descriptor whose headers contain a CR, LF, or NUL byte (source-side guard,
/// issue #57 — a page could otherwise smuggle e.g. `"X": "a\r\nCookie: c=1"`
/// past the header-*name* allow-list in `cerberus-net::engine`) is not handed
/// back at all: since its `id` is still known here, we immediately
/// [`reject_fetch`] it with a clean network-error message — matching how
/// `cerberus-app`'s `pump_fetches` already rejects synchronously-invalid
/// requests (an unsupported URL, a consent-blocked origin) before ever handing
/// them to the network — rather than silently dropping the request and
/// leaving its Promise to dangle forever.
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
        let headers = match decode_header_pairs(item.get("headers")) {
            Ok(headers) => headers,
            Err(()) => {
                reject_fetch(engine, realm, id, "invalid header value")?;
                continue;
            }
        };
        out.push(FetchRequest {
            id,
            url,
            method,
            headers,
            body,
        });
    }
    Ok(out)
}

/// Drain the realm's queued `document.cookie =` write strings, returning the raw
/// values (each the full `"name=value; attrs…"` string a script assigned) and
/// clearing the queue. The caller persists each into the per-instance sealed
/// cookie jar exactly like a network `Set-Cookie`, so a script-set cookie (e.g. a
/// bot challenge's token) survives to the next request. An empty queue yields an
/// empty `Vec`; a malformed drain (non-string entries) is skipped, never fatal.
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
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect())
}

/// A navigation a page's script requested: `location.assign`/`replace`/`reload`,
/// `location.href = …`, or `window.location = "…"`. `url` is exactly what the
/// script supplied (possibly relative — the host resolves it against the current
/// document); `replace` is set for history-replacing navigations (`replace()`,
/// `reload()`), which the host may use to decide history semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    /// The target URL as the script supplied it (may be relative).
    pub url: String,
    /// Whether this replaces the current history entry rather than pushing one.
    pub replace: bool,
}

/// Drain the realm's queued script navigations (in request order) and clear the
/// queue. Each entry is a [`Navigation`]; the host resolves the URL and performs
/// the load (a cookie-gated reload after a bot challenge sets its token rides
/// this path). An empty queue yields an empty `Vec`; malformed entries are
/// skipped, never fatal.
pub fn take_navigations(
    engine: &mut dyn JsEngine,
    realm: RealmId,
) -> Result<Vec<Navigation>, BridgeError> {
    let json = match engine.eval(realm, "__cerberusTakeNavigations()")? {
        JsValue::Str(s) => s,
        other => {
            return Err(BridgeError::Structure(format!(
                "__cerberusTakeNavigations did not return a string: {other:?}"
            )))
        }
    };
    let value = json::parse(&json).map_err(BridgeError::Json)?;
    let items = value.as_array().ok_or_else(|| {
        BridgeError::Structure("__cerberusTakeNavigations did not return an array".to_string())
    })?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Json::as_str)?.to_string();
            let replace = item.get("replace").and_then(Json::as_u64).unwrap_or(0) != 0;
            Some(Navigation { url, replace })
        })
        .collect())
}

/// Decode a wire `headers` value (`[[name, value], …]`) into a `(name, value)`
/// list. A missing/garbage *pair* is skipped (a `None` field yields an empty
/// list); a pair whose name or value contains a CR, LF, or NUL byte fails the
/// whole decode (`Err(())`) instead, so the caller can reject the owning
/// `fetch()` cleanly rather than silently mangling or forwarding it — see
/// [`take_fetches`].
fn decode_header_pairs(headers: Option<&Json>) -> Result<Vec<(String, String)>, ()> {
    let mut out = Vec::new();
    if let Some(arr) = headers.and_then(Json::as_array) {
        for pair in arr {
            let Some(pair) = pair.as_array() else {
                continue;
            };
            let name = pair.first().and_then(Json::as_str);
            let value = pair.get(1).and_then(Json::as_str);
            if let (Some(name), Some(value)) = (name, value) {
                if has_crlf_or_nul(name) || has_crlf_or_nul(value) {
                    return Err(());
                }
                out.push((name.to_string(), value.to_string()));
            }
        }
    }
    Ok(out)
}

/// Whether `s` contains a CR, LF, or NUL byte — the bytes that let a header
/// name/value break out of its wire line (see [`decode_header_pairs`]).
fn has_crlf_or_nul(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, 0x0D | 0x0A | 0x00))
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

    'pump: loop {
        // Drain timers + microtasks so any pending/just-scheduled fetch enqueues.
        run_event_loop(engine, realm, loop_budget)?;

        if rounds >= fetch_budget.max_rounds {
            hit_cap = true;
            break;
        }
        let reqs = take_fetches(engine, realm)?;
        if reqs.is_empty() {
            break;
        }
        rounds += 1;

        for req in reqs {
            if requests >= fetch_budget.max_requests {
                // Out of request budget: reject this and every remaining queued
                // request so no Promise is left dangling, then stop the pump.
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
    run_scripts(engine, realm, scripts)?;
    fire_load(engine, realm)?;
    drive_fetches(
        engine,
        realm,
        client,
        EventLoopBudget::default(),
        FetchBudget::default(),
    )?;
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
///   `insertAdjacentElement`/`insertAdjacentText` (all four positions),
///   `getAttribute`/`setAttribute`/`removeAttribute`/`hasAttribute`/
///   `toggleAttribute`/`getAttributeNames`, `id`, `className`, `classList`
///   (`add`/`remove`/`toggle`/`contains`/`length`), form-control `value`
///   (`<textarea>`/`<select>`/`<option>` aware), `type` (spec defaults),
///   `checked`/`hidden` (reflect the like-named attribute), `<select>` `options`/
///   `selectedIndex` and `<option>` `selected`/`text` (backed by the `selected`
///   attribute the renderer reads), `children`/`childNodes`/`childElementCount`,
///   `parentNode`/`parentElement`, `firstChild`/`lastChild`/`nextSibling`/
///   `previousSibling`, `firstElementChild`/`lastElementChild`/
///   `nextElementSibling`/`previousElementSibling`,
///   `appendChild`/`removeChild`/`insertBefore`/`remove`,
///   `append`/`prepend`/`before`/`after`/`replaceWith` (variadic node-or-string
///   insertion), `cloneNode` (shallow/deep), a
///   `dataset` (live `data-*` <-> camelCase map), `href`/`src` (resolved to an
///   absolute URL against the document location), store-only `style`,
///   `getBoundingClientRect` (all-zero), scoped
///   `querySelector`/`querySelectorAll`/`matches`/`closest`,
///   `addEventListener`/`removeEventListener`/`dispatchEvent` and the
///   `click`/`focus`/`blur` convenience methods (dispatch through the same
///   bubbling path as real input; `focus`/`blur` track `document.activeElement`).
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
/// The script-visible surface presents a **complete, coherent Chrome-142 on
/// Windows-11 (Win64/x64)** persona: `navigator`/`window`/`document` expose the
/// full set of APIs a real Chrome does (`userAgentData`, `mediaDevices`,
/// `permissions`, `connection`, `performance`, `visualViewport`, plugins, …) with
/// Chrome-on-Windows values, so a scanner or anti-bot sensor sees no missing or
/// impossible reads. Values are **fixed** (a single validation-phase identity);
/// per-head fingerprint *farbling* of the genuinely high-entropy reads (canvas /
/// audio / WebGL / font metrics) is the separate farbling prologue (M6,
/// ADR-0002), installed before this model, not here.
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
      if (i === -1) el.__attrs.push([name, v]); else el.__attrs[i][1] = v;
    }
    function removeAttr(el, name) {
      var i = attrIndex(el, name);
      if (i !== -1) el.__attrs.splice(i, 1);
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
        replace: function (oldT, newT) {
          // Swap `oldT` for `newT` in place (returns whether it was present),
          // then de-duplicate so the token list stays unique (DOMTokenList).
          oldT = String(oldT); newT = String(newT);
          var toks = classTokens(el);
          var k = toks.indexOf(oldT);
          if (k === -1) return false;
          toks[k] = newT;
          var seen = Object.create(null), out = [];
          for (var i = 0; i < toks.length; i++) {
            if (!seen[toks[i]]) { seen[toks[i]] = 1; out.push(toks[i]); }
          }
          writeClass(el, out);
          return true;
        },
        item: function (i) { return classTokens(el)[i] || null; },
        // `classList.value` is the serialized token string, readable and writable.
        get value() { return getAttr(el, "class") || ""; },
        set value(v) { setAttr(el, "class", String(v)); },
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
    function appendChild(parent, node) {
      detach(node);
      clearRaw(parent);
      parent.__kids.push(node);
      node.__parent = parent;
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
      return node;
    }
    function removeChild(parent, node) {
      var k = parent.__kids.indexOf(node);
      if (k !== -1) { parent.__kids.splice(k, 1); node.__parent = null; }
      return node;
    }

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
    // Supported pseudo-classes: form state (`:checked`, `:disabled`, `:enabled`,
    // `:required`, `:optional`, `:read-only`, `:read-write`) and structural
    // (`:first-child`, `:last-child`, `:only-child`, `:empty`, `:root`). Any other
    // pseudo — dynamic state like `:hover`, or a `::pseudo-element` — never matches
    // statically (as in the cascade engine). Unsupported (by design, speed-first):
    // `:nth-child()`/`:not()`, namespaces. Combinators: descendant, child (`>`),
    // adjacent-sibling (`+`), general-sibling (`~`). Attribute selectors support
    // presence (`[name]`), exact match (`[name="value"]`), and the
    // `^= $= *= ~= |=` operators.
    function parseCompound(text) {
      // text is one compound run with no whitespace/combinators, e.g.
      // `div.foo#bar[data-x="1"]`. Returns null if it is empty/garbage.
      var compound = { tag: null, id: null, classes: [], attrs: [], pseudos: [] };
      var i = 0, n = text.length, sawAny = false;
      while (i < n) {
        var ch = text.charAt(i);
        if (ch === "#") {
          i++; var s = i; while (i < n && !".#[:".includes(text.charAt(i))) i++;
          compound.id = text.slice(s, i); sawAny = true;
        } else if (ch === ".") {
          i++; var s2 = i; while (i < n && !".#[:".includes(text.charAt(i))) i++;
          if (i > s2) { compound.classes.push(text.slice(s2, i)); sawAny = true; }
        } else if (ch === ":") {
          // Pseudo-class (or `::`-prefixed pseudo-element). Read the name, and any
          // parenthesized argument. A `:` used to fall into the type branch, so
          // `input:checked` parsed a bogus tag "input:checked" and never matched.
          i++;
          if (text.charAt(i) === ":") i++; // ::pseudo-element → treated as unknown
          var ps = i;
          while (i < n && !".#[:(".includes(text.charAt(i))) i++;
          var pname = text.slice(ps, i).toLowerCase();
          var parg = null;
          if (text.charAt(i) === "(") {
            var pe = text.indexOf(")", i);
            if (pe === -1) return null; // unterminated → no match
            parg = text.slice(i + 1, pe).trim();
            i = pe + 1;
          }
          if (pname) { compound.pseudos.push({ name: pname, arg: parg }); sawAny = true; }
        } else if (ch === "[") {
          var end = text.indexOf("]", i);
          if (end === -1) return null;            // unterminated → no match
          var body = text.slice(i + 1, end).trim();
          i = end + 1;
          var eq = body.indexOf("=");
          if (eq === -1) {
            if (body) { compound.attrs.push({ name: body, value: null, op: null }); sawAny = true; }
          } else {
            // An operator may prefix the `=`: ^= $= *= ~= |= (else exact `=`).
            var op = "=";
            var nameEnd = eq;
            if (eq > 0 && "^$*~|".includes(body.charAt(eq - 1))) {
              op = body.charAt(eq - 1) + "=";
              nameEnd = eq - 1;
            }
            var an = body.slice(0, nameEnd).trim();
            var av = body.slice(eq + 1).trim();
            if (av.length >= 2 && (av.charAt(0) === '"' || av.charAt(0) === "'")) av = av.slice(1, -1);
            if (an) { compound.attrs.push({ name: an, value: av, op: op }); sawAny = true; }
          }
        } else {
          // A type (tag) selector or universal `*`; runs until the next part.
          var s3 = i; while (i < n && !".#[:".includes(text.charAt(i))) i++;
          var tag = text.slice(s3, i);
          if (tag && tag !== "*") compound.tag = tag.toLowerCase();
          sawAny = true;
        }
      }
      return sawAny ? compound : null;
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
          // `>` child, `+` adjacent-sibling, `~` general-sibling.
          pendingCombinator = cc; i++;
          while (i < n && /\s/.test(text.charAt(i))) i++;
        } else if (sawSpace && steps.length > 0) {
          pendingCombinator = " ";
        }
        // Read the compound run up to the next combinator/whitespace.
        var s = i;
        while (i < n && !/\s/.test(text.charAt(i)) && !">+~".includes(text.charAt(i))) {
          if (text.charAt(i) === "[") { var e = text.indexOf("]", i); i = (e === -1) ? n : e + 1; }
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
        if (v === null) return false;           // attribute must be present
        if (a.value === null) continue;         // `[name]` presence-only
        switch (a.op) {
          case "^=": if (!a.value || v.indexOf(a.value) !== 0) return false; break;
          case "$=": if (!a.value || v.slice(-a.value.length) !== a.value || v.length < a.value.length) return false; break;
          case "*=": if (!a.value || v.indexOf(a.value) === -1) return false; break;
          case "~=": if (v.split(/\s+/).indexOf(a.value) === -1) return false; break;
          case "|=": if (v !== a.value && v.indexOf(a.value + "-") !== 0) return false; break;
          default:   if (v !== a.value) return false; // `=` exact
        }
      }
      if (compound.pseudos) {
        for (var q = 0; q < compound.pseudos.length; q++) {
          if (!matchesPseudo(el, compound.pseudos[q])) return false;
        }
      }
      return true;
    }
    // A small, useful set of pseudo-classes for querySelector/matches: the
    // attribute-backed form-state ones (`:checked`, `:disabled`, …) plus the
    // structural ones. Anything else — dynamic state like `:hover`, or an
    // unsupported pseudo — never matches statically, matching the cascade engine.
    var FORM_CONTROLS = {
      input: 1, select: 1, textarea: 1, button: 1, fieldset: 1, optgroup: 1, option: 1,
    };
    function elementSiblingsOf(el) {
      var p = el.__parent;
      return p ? p.__kids.filter(function (c) { return c.__type === ELEMENT_NODE; }) : [el];
    }
    function matchesPseudo(el, ps) {
      switch (ps.name) {
        case "checked": return getAttr(el, "checked") !== null || getAttr(el, "selected") !== null;
        case "disabled": return getAttr(el, "disabled") !== null;
        case "enabled": return !!FORM_CONTROLS[el.__tag] && getAttr(el, "disabled") === null;
        case "required": return getAttr(el, "required") !== null;
        case "optional": return !!FORM_CONTROLS[el.__tag] && getAttr(el, "required") === null;
        case "read-only": return getAttr(el, "readonly") !== null;
        case "read-write": return getAttr(el, "readonly") === null;
        case "root": return el.__tag === "html";
        case "empty":
          for (var i = 0; i < el.__kids.length; i++) {
            var k = el.__kids[i];
            if (k.__type === ELEMENT_NODE) return false;
            if (k.__type === TEXT_NODE && k.__text.length) return false;
          }
          return true;
        case "first-child": return elementSiblingsOf(el)[0] === el;
        case "last-child": { var s = elementSiblingsOf(el); return s[s.length - 1] === el; }
        case "only-child": return elementSiblingsOf(el).length === 1;
        default: return false;
      }
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
          // Adjacent sibling: the element immediately before `node`.
          var asibs = elementSiblingsOf(node), ai = asibs.indexOf(node);
          if (ai <= 0 || !matchesCompound(asibs[ai - 1], want)) return false;
          node = asibs[ai - 1];
        } else if (rel === "~") {
          // General sibling: SOME preceding element sibling matching `want`.
          var gsibs = elementSiblingsOf(node), gi = gsibs.indexOf(node), gok = false;
          for (var gs = gi - 1; gs >= 0; gs--) {
            if (matchesCompound(gsibs[gs], want)) { gok = true; node = gsibs[gs]; break; }
          }
          if (!gok) return false;
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

    // A real DOM node exposes the `__proto__` accessor (inherited from
    // Object.prototype) that walks its prototype chain. Anti-tampering code
    // reads `node.__proto__.someMethod` to grab a pristine, un-overridden DOM
    // method off the prototype. Our node prototypes are rooted at
    // Object.create(null), so that accessor is otherwise absent and
    // `node.__proto__` reads as `undefined` — which crashes such code (and any
    // fingerprint solver that relies on it). Define it on NODE_PROTO so
    // `node.__proto__` returns the node's actual prototype (carrying the DOM
    // methods), mirroring a real browser.
    defAccessor(
      NODE_PROTO,
      "__proto__",
      function () {
        return Object.getPrototypeOf(this);
      },
      function (p) {
        try {
          Object.setPrototypeOf(this, p);
        } catch (e) {}
      }
    );

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
    NODE_PROTO.contains = function (other) {
      for (var n = other; n; n = n.__parent) if (n === this) return true;
      return false;
    };
    NODE_PROTO.hasChildNodes = function () { return this.__kids.length > 0; };
    // ParentNode/ChildNode convenience insertion (`append`/`prepend`/`before`/
    // `after`/`replaceWith`). Each accepts a variadic mix of nodes and strings
    // (strings become text nodes), preserving argument order — the modern idiom
    // pages use in place of createTextNode + appendChild/insertBefore chains.
    function coerceInsertable(arg) {
      return (arg && typeof arg === "object" && typeof arg.__type === "number")
        ? arg : makeText(String(arg));
    }
    NODE_PROTO.append = function () {
      for (var i = 0; i < arguments.length; i++) appendChild(this, coerceInsertable(arguments[i]));
    };
    NODE_PROTO.prepend = function () {
      var ref = this.__kids[0] || null;
      for (var i = 0; i < arguments.length; i++) insertBefore(this, coerceInsertable(arguments[i]), ref);
    };
    NODE_PROTO.before = function () {
      var p = this.__parent; if (!p) return;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, coerceInsertable(arguments[i]), this);
    };
    NODE_PROTO.after = function () {
      var p = this.__parent; if (!p) return;
      var idx = p.__kids.indexOf(this);
      var ref = p.__kids[idx + 1] || null;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, coerceInsertable(arguments[i]), ref);
    };
    NODE_PROTO.replaceWith = function () {
      var p = this.__parent; if (!p) return;
      for (var i = 0; i < arguments.length; i++) insertBefore(p, coerceInsertable(arguments[i]), this);
      detach(this);
    };
    // `cloneNode(deep)`: a fresh node (new id, no parent, no event listeners),
    // with attributes copied into independent pairs so mutating the clone can't
    // leak back to the original. A shallow clone has no children; a deep clone
    // recursively clones the subtree (or carries over a pending innerHTML
    // fragment). Pages clone `<template>`/list rows constantly.
    NODE_PROTO.cloneNode = function (deep) {
      if (this.__type === TEXT_NODE) return makeText(this.__text);
      var copy = makeElement(this.__tag);
      copy.__attrs = this.__attrs.map(function (p) { return [p[0], p[1]]; });
      if (deep) {
        if (typeof this.__rawHTML === "string") {
          copy.__rawHTML = this.__rawHTML;
        } else {
          for (var i = 0; i < this.__kids.length; i++) {
            appendChild(copy, this.__kids[i].cloneNode(true));
          }
        }
      }
      return copy;
    };

    // -- elements (ELEMENT_PROTO) --
    defAccessor(ELEMENT_PROTO, "tagName", function () { return this.__tag.toUpperCase(); });
    defAccessor(ELEMENT_PROTO, "nodeName", function () { return this.__tag.toUpperCase(); });
    defAccessor(ELEMENT_PROTO, "children", function () { return elementChildren(this); });
    defAccessor(ELEMENT_PROTO, "firstElementChild", function () { var c = elementChildren(this); return c[0] || null; });
    defAccessor(ELEMENT_PROTO, "lastElementChild", function () { var c = elementChildren(this); return c[c.length - 1] || null; });
    defAccessor(ELEMENT_PROTO, "childElementCount", function () { return elementChildren(this).length; });
    // Element-only siblings: skip text/comment nodes, which `nextSibling`/
    // `previousSibling` (on NODE_PROTO) would return. Pages walk these constantly
    // to iterate a list without tripping over whitespace text nodes.
    defAccessor(ELEMENT_PROTO, "nextElementSibling", function () {
      var p = this.__parent; if (!p) return null;
      var kids = p.__kids, i = kids.indexOf(this);
      for (var j = i + 1; j < kids.length; j++) { if (kids[j].__type === ELEMENT_NODE) return kids[j]; }
      return null;
    });
    defAccessor(ELEMENT_PROTO, "previousElementSibling", function () {
      var p = this.__parent; if (!p) return null;
      var kids = p.__kids, i = kids.indexOf(this);
      for (var j = i - 1; j >= 0; j--) { if (kids[j].__type === ELEMENT_NODE) return kids[j]; }
      return null;
    });
    defAccessor(ELEMENT_PROTO, "id",
      function () { return getAttr(this, "id") || ""; },
      function (v) { setAttr(this, "id", v); });
    // Form-control current value, backed by the `value` attribute so handlers
    // can read/modify `el.value` and the change reflects in serialize/layout
    // (M12b input events). A <textarea> has no value attribute — its value is its
    // text content — so fall back to that when the attribute is unset.
    // The option elements of a <select>, in document order (descending into
    // <optgroup>). Shared by the select/option accessors below and mirrors the
    // layout side's `collect_options`, so JS selection and rendering agree.
    function selectOptions(sel) {
      var out = [];
      (function walk(n) {
        var kids = n.__kids || [];
        for (var i = 0; i < kids.length; i++) {
          var c = kids[i];
          if (c.__type !== ELEMENT_NODE) continue;
          if (c.__tag === "option") out.push(c);
          else if (c.__tag === "optgroup") walk(c);
        }
      })(sel);
      return out;
    }
    function optionText(opt) { var acc = []; collectText(opt, acc); return acc.join("").trim(); }
    function optionValue(opt) { var v = getAttr(opt, "value"); return v === null ? optionText(opt) : v; }
    defAccessor(ELEMENT_PROTO, "value",
      function () {
        if (this.__tag === "select") {
          var opts = selectOptions(this);
          var sel = null;
          for (var i = 0; i < opts.length; i++) { if (getAttr(opts[i], "selected") !== null) { sel = opts[i]; break; } }
          if (!sel && opts.length) sel = opts[0];
          return sel ? optionValue(sel) : "";
        }
        if (this.__tag === "option") return optionValue(this);
        var v = getAttr(this, "value");
        if (v !== null) return v;
        if (this.__tag === "textarea") { var acc = []; collectText(this, acc); return acc.join(""); }
        return "";
      },
      function (v) {
        if (this.__tag === "select") {
          // Selecting by value: mark the first matching option selected, clear
          // the rest (reflected via the `selected` attribute layout reads).
          var opts = selectOptions(this), target = String(v);
          for (var i = 0; i < opts.length; i++) {
            if (optionValue(opts[i]) === target) { setAttr(opts[i], "selected", ""); }
            else { removeAttr(opts[i], "selected"); }
          }
          return;
        }
        setAttr(this, "value", String(v));
      });
    // <select>.selectedIndex / .options and <option>.selected / .text, all
    // backed by the `selected` attribute so scripted selection reflects into
    // serialize/layout (which renders the chosen option from that attribute).
    defAccessor(ELEMENT_PROTO, "options", function () { return selectOptions(this); });
    defAccessor(ELEMENT_PROTO, "selectedIndex",
      function () {
        var opts = selectOptions(this);
        for (var i = 0; i < opts.length; i++) { if (getAttr(opts[i], "selected") !== null) return i; }
        return opts.length ? 0 : -1;
      },
      function (idx) {
        var opts = selectOptions(this); idx = Number(idx);
        for (var i = 0; i < opts.length; i++) {
          if (i === idx) { setAttr(opts[i], "selected", ""); } else { removeAttr(opts[i], "selected"); }
        }
      });
    defAccessor(ELEMENT_PROTO, "selected",
      function () { return getAttr(this, "selected") !== null; },
      function (v) { if (v) { setAttr(this, "selected", ""); } else { removeAttr(this, "selected"); } });
    defAccessor(ELEMENT_PROTO, "text", function () { return optionText(this); });
    // `input.type` (and friends): reflect the `type` attribute, defaulting per
    // element as the DOM spec does (input→"text", button→"submit", …), so a
    // page that branches on `el.type` sees a sensible value instead of undefined.
    defAccessor(ELEMENT_PROTO, "type",
      function () {
        var t = getAttr(this, "type");
        if (t !== null) return this.__tag === "input" ? String(t).toLowerCase() : t;
        switch (this.__tag) {
          case "input": return "text";
          case "button": return "submit";
          case "textarea": return "textarea";
          case "select": return getAttr(this, "multiple") !== null ? "select-multiple" : "select-one";
          default: return "";
        }
      },
      function (v) { setAttr(this, "type", String(v)); });
    // `checked` for checkbox/radio, backed by the `checked` attribute so both
    // the initial HTML state (`<input checked>`) reads back true and a scripted
    // `el.checked = true/false` reflects into serialize/layout (which renders a
    // checkbox from that attribute — see cerberus-layout).
    defAccessor(ELEMENT_PROTO, "checked",
      function () { return getAttr(this, "checked") !== null; },
      function (v) { if (v) { setAttr(this, "checked", ""); } else { removeAttr(this, "checked"); } });
    // `form.elements`: the form's listed controls as an HTMLFormControlsCollection
    // — an array (so `.length`, indexing and iteration work) that also exposes each
    // control by its `name`/`id` (`form.elements.user`) and a `namedItem(name)`
    // method, matching the two idioms real pages use to read a form. Only listed
    // control tags participate (input/select/textarea/button/fieldset/output/object),
    // in tree order. Non-form elements return an empty collection.
    var LISTED_CONTROLS = {
      input: 1, select: 1, textarea: 1, button: 1, fieldset: 1, output: 1, object: 1,
    };
    defAccessor(ELEMENT_PROTO, "elements", function () {
      var out = [];
      if (this.__tag !== "form") return out;
      walkElements(this, function (el) {
        if (LISTED_CONTROLS[el.__tag]) out.push(el);
      });
      // Named access mirrors the spec: id and name both index the collection,
      // and `namedItem` looks up by either. Defined non-enumerable so they don't
      // perturb `.length`/index iteration.
      var named = {};
      for (var i = 0; i < out.length; i++) {
        var key = getAttr(out[i], "name");
        if (key === null) key = getAttr(out[i], "id");
        if (key !== null && key !== "" && !(key in named)) named[key] = out[i];
      }
      for (var k in named) {
        Object.defineProperty(out, k, { value: named[k], enumerable: false, configurable: true });
      }
      Object.defineProperty(out, "namedItem", {
        value: function (n) { return named[n] || null; },
        enumerable: false, configurable: true,
      });
      return out;
    });
    // `el.hidden` reflects the `hidden` boolean attribute, so `el.hidden = true`
    // hides the element via the UA `[hidden] { display: none }` rule (and reads
    // back the initial `<x hidden>` state).
    defAccessor(ELEMENT_PROTO, "hidden",
      function () { return getAttr(this, "hidden") !== null; },
      function (v) { if (v) { setAttr(this, "hidden", ""); } else { removeAttr(this, "hidden"); } });
    // `el.disabled` reflects the `disabled` boolean attribute — read by form
    // handling (e.g. FormData skips disabled controls) and toggled by scripts.
    defAccessor(ELEMENT_PROTO, "disabled",
      function () { return getAttr(this, "disabled") !== null; },
      function (v) { if (v) { setAttr(this, "disabled", ""); } else { removeAttr(this, "disabled"); } });
    // `el.name` reflects the `name` attribute (form controls, `<form>`, `<img>`,
    // …). Pages iterate `form.elements` and read `.name` to build submissions, so
    // an unreflected name read back undefined and lost the field.
    defAccessor(ELEMENT_PROTO, "name",
      function () { return getAttr(this, "name") || ""; },
      function (v) { setAttr(this, "name", String(v)); });
    defAccessor(ELEMENT_PROTO, "className",
      function () { return getAttr(this, "class") || ""; },
      function (v) { setAttr(this, "class", v); });
    defAccessor(ELEMENT_PROTO, "classList", function () {
      if (!this.__classList) this.__classList = makeClassList(this);
      return this.__classList;
    });
    // `el.dataset.fooBar` <-> the `data-foo-bar` attribute (the HTML camelCase
    // <-> kebab-case mapping). A live Proxy over the element's attributes so both
    // reads and writes go straight through `getAttr`/`setAttr` — a very common
    // page idiom (`el.dataset.id`) that otherwise read back undefined.
    defAccessor(ELEMENT_PROTO, "dataset", function () {
      if (this.__dataset) return this.__dataset;
      var el = this;
      function toAttr(k) {
        return "data-" + String(k).replace(/[A-Z]/g, function (m) { return "-" + m.toLowerCase(); });
      }
      function toKey(a) {
        return a.slice(5).replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
      }
      function keys() {
        var ks = [];
        for (var i = 0; i < el.__attrs.length; i++) {
          if (el.__attrs[i][0].indexOf("data-") === 0) ks.push(toKey(el.__attrs[i][0]));
        }
        return ks;
      }
      this.__dataset = new Proxy({}, {
        get: function (_, k) {
          if (typeof k !== "string") return undefined;
          var v = getAttr(el, toAttr(k)); return v === null ? undefined : v;
        },
        set: function (_, k, v) { setAttr(el, toAttr(k), String(v)); return true; },
        has: function (_, k) { return typeof k === "string" && getAttr(el, toAttr(k)) !== null; },
        deleteProperty: function (_, k) { removeAttr(el, toAttr(k)); return true; },
        ownKeys: function () { return keys(); },
        getOwnPropertyDescriptor: function (_, k) {
          if (typeof k === "string" && getAttr(el, toAttr(k)) !== null) {
            return { enumerable: true, configurable: true, writable: true, value: getAttr(el, toAttr(k)) };
          }
          return undefined;
        },
      });
      return this.__dataset;
    });
    // Resolve a possibly-relative URL against the document location, so
    // `a.href` / `img.src` return the *absolute* URL the spec requires (pages
    // routinely compare or log these). Reads `g.location` at call time (set
    // later in this prelude). Handles absolute, protocol-relative, absolute-
    // path, query-only, fragment-only and dot-segment relative references.
    function resolveUrl(rel) {
      rel = (rel == null) ? "" : String(rel);
      var loc = g.location || {};
      var baseHref = loc.href || "";
      if (rel === "") return baseHref;
      if (/^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(rel)) return rel; // already absolute
      var protocol = loc.protocol || "https:";
      var origin = loc.origin || (protocol + "//" + (loc.host || ""));
      if (rel.indexOf("//") === 0) return protocol + rel;      // protocol-relative
      if (rel.charAt(0) === "#") return baseHref.split("#")[0] + rel;
      if (rel.charAt(0) === "?") return baseHref.split("#")[0].split("?")[0] + rel;
      var tail = "";
      var hi = rel.search(/[?#]/);
      if (hi >= 0) { tail = rel.slice(hi); rel = rel.slice(0, hi); }
      var path;
      if (rel.charAt(0) === "/") {
        path = rel;
      } else {
        var basePath = loc.pathname || "/";
        var dir = basePath.slice(0, basePath.lastIndexOf("/") + 1) || "/";
        path = dir + rel;
      }
      var parts = path.split("/"), stack = [];
      for (var i = 0; i < parts.length; i++) {
        if (parts[i] === "..") { stack.pop(); }
        else if (parts[i] !== ".") { stack.push(parts[i]); }
      }
      var norm = stack.join("/");
      if (norm.charAt(0) !== "/") norm = "/" + norm;
      return origin + norm + tail;
    }
    var HREF_TAGS = { a: 1, area: 1, link: 1, base: 1 };
    var SRC_TAGS = { img: 1, script: 1, iframe: 1, source: 1, audio: 1, video: 1, track: 1, embed: 1, frame: 1 };
    // `.href` / `.src` reflect their attribute resolved to an absolute URL (or
    // "" when the element type carries the attribute but it is unset); other
    // element types have no such property (undefined), per the DOM.
    defAccessor(ELEMENT_PROTO, "href",
      function () {
        if (!HREF_TAGS[this.__tag]) return undefined;
        var v = getAttr(this, "href"); return v === null ? "" : resolveUrl(v);
      },
      function (v) { setAttr(this, "href", String(v)); });
    defAccessor(ELEMENT_PROTO, "src",
      function () {
        if (!SRC_TAGS[this.__tag]) return undefined;
        var v = getAttr(this, "src"); return v === null ? "" : resolveUrl(v);
      },
      function (v) { setAttr(this, "src", String(v)); });
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
    // insertAdjacentElement/Text: unlike insertAdjacentHTML (which routes through
    // the deferred __rawHTML reparse and only supports the two in-element spots),
    // these insert a real node with preserved identity, so all four positions work
    // by delegating to the ParentNode/ChildNode primitives. `insertAdjacentElement`
    // returns the inserted node (or null for an unknown position); `Text` wraps the
    // string in a text node.
    ELEMENT_PROTO.insertAdjacentElement = function (position, el) {
      switch (String(position).toLowerCase()) {
        case "beforebegin": this.before(el); break;
        case "afterbegin": this.prepend(el); break;
        case "beforeend": this.append(el); break;
        case "afterend": this.after(el); break;
        default: return null;
      }
      return el;
    };
    ELEMENT_PROTO.insertAdjacentText = function (position, text) {
      this.insertAdjacentElement(String(position), makeText(String(text)));
    };

    ELEMENT_PROTO.getAttribute = function (n) { return getAttr(this, String(n)); };
    ELEMENT_PROTO.setAttribute = function (n, v) { setAttr(this, String(n), v); };
    ELEMENT_PROTO.removeAttribute = function (n) { removeAttr(this, String(n)); };
    ELEMENT_PROTO.hasAttribute = function (n) { return attrIndex(this, String(n)) !== -1; };
    ELEMENT_PROTO.getAttributeNames = function () { return this.__attrs.map(function (p) { return p[0]; }); };
    // `toggleAttribute(name[, force])`: with no `force`, flip presence; with a
    // `force` argument, add when truthy / remove when falsy. Returns whether the
    // attribute is present afterwards (per DOM). A common toggle idiom for
    // boolean attributes like `disabled`/`hidden`/`aria-*`.
    ELEMENT_PROTO.toggleAttribute = function (n, force) {
      n = String(n);
      var present = attrIndex(this, n) !== -1;
      var want = (arguments.length > 1) ? !!force : !present;
      if (want === present) return present;
      if (want) { setAttr(this, n, ""); } else { removeAttr(this, n); }
      return want;
    };

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
    ELEMENT_PROTO.addEventListener = function (type, fn) {
      type = String(type);
      if (!this.__listeners) this.__listeners = Object.create(null);
      if (!this.__listeners[type]) this.__listeners[type] = [];
      if (typeof fn === "function") this.__listeners[type].push(fn);
    };
    ELEMENT_PROTO.removeEventListener = function (type, fn) {
      type = String(type); if (!this.__listeners) return;
      var arr = this.__listeners[type]; if (!arr) return;
      var i = arr.indexOf(fn); if (i !== -1) arr.splice(i, 1);
    };
    ELEMENT_PROTO.dispatchEvent = function (ev) {
      if (!this.__listeners) return true;
      var arr = this.__listeners[ev && ev.type]; if (!arr) return true;
      for (var i = 0; i < arr.slice().length; i++) { try { arr[i].call(this, ev); } catch (e) {} }
      return true;
    };
    // Programmatic click()/focus()/blur() route through the same bubbling
    // dispatcher real pointer/focus interactions use (`__cerberusDispatch`,
    // defined later — resolved at call time), so a page that toggles a menu via
    // `el.click()` or fires focus/blur handlers works. `click` bubbles and is
    // cancelable; `focus`/`blur` do not bubble (per the DOM spec). The Rust-side
    // default action (navigation, form submit) is driven by real user input, not
    // a scripted click — this fires the listeners, which is the common case.
    ELEMENT_PROTO.click = function () {
      g.__cerberusDispatch(this.__id, "click", { bubbles: true, cancelable: true });
    };
    ELEMENT_PROTO.focus = function () {
      if (g.document) g.document.activeElement = this;
      g.__cerberusDispatch(this.__id, "focus", { bubbles: false, cancelable: false });
    };
    ELEMENT_PROTO.blur = function () {
      if (g.document && g.document.activeElement === this) g.document.activeElement = null;
      g.__cerberusDispatch(this.__id, "blur", { bubbles: false, cancelable: false });
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
      // --- static document metadata (a coherent, served-as-HTML document) ---
      // A missing/undefined read here reads as a non-browser environment to a
      // scanner; these are the fixed values Chrome reports for a top-level
      // UTF-8 HTML page.
      referrer: "",
      characterSet: "UTF-8",
      charset: "UTF-8",
      inputEncoding: "UTF-8",
      compatMode: "CSS1Compat",
      contentType: "text/html",
      visibilityState: "visible",
      hidden: false,
      dir: "",
      designMode: "off",
      currentScript: null,
      // Focus lives on <body> until a script focuses something; never
      // undefined. Re-pinned to document.body at the end of install (below).
      activeElement: null,
      hasFocus: function () { return true; },
    };
    // FontFaceSet + DOMImplementation: present on every document; sensors read
    // document.fonts.ready and implementation.createHTMLDocument, and probe
    // document.fonts.check("<size> '<Family>'") to enumerate installed fonts.
    // check() consults this head's presented font set so it agrees with the
    // measureText-based enumeration defense (both keyed off the same per-head
    // list) — a generic family, or a name in __CERBERUS_PROFILE__.fonts, is
    // "available"; anything else is not. size stays 0 (it counts page @font-face
    // loads, not system fonts) to match a real FontFaceSet.
    document.fonts = {
      ready: Promise.resolve(), status: "loaded", size: 0,
      check: function (font) {
        var s = String(font || "");
        var m = /(?:\d*\.?\d+)(?:px|pt|pc|em|rem|ex|ch|vw|vh|%)\s+(.+)$/.exec(s);
        var fam = (m ? m[1] : s).split(",")[0].trim().replace(/^["']|["']$/g, "").toLowerCase();
        if (!fam) return true;
        var GEN = { "serif":1,"sans-serif":1,"monospace":1,"cursive":1,"fantasy":1,
          "system-ui":1,"ui-serif":1,"ui-sans-serif":1,"ui-monospace":1,"ui-rounded":1,
          "math":1,"emoji":1,"-apple-system":1,"blinkmacsystemfont":1 };
        if (GEN[fam]) return true;
        var p = g.__CERBERUS_PROFILE__, list = (p && p.fonts) || null;
        if (!list) return true; // no persona wired: don't leak "nothing installed"
        for (var i = 0; i < list.length; i++) { if (String(list[i]).toLowerCase() === fam) return true; }
        return false;
      },
      load: function () { return Promise.resolve([]); },
      values: function () { return [][Symbol.iterator](); },
      forEach: function () {},
      addEventListener: function () {}, removeEventListener: function () {},
    };
    document.implementation = {
      createHTMLDocument: function () { return document; },
      createDocumentType: function () { return {}; },
      hasFeature: function () { return true; },
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
        // A script cookie write. Record the FULL raw string (with attributes) so
        // the host can persist it into the per-instance sealed jar exactly like a
        // network Set-Cookie (Domain/Path/Expires/Secure honored there). Then
        // update the in-memory view: keep the first "name=value" pair,
        // appending/replacing by name (attributes are dropped from the view).
        var raw = String(v);
        if (Array.isArray(g.__cerberusCookieWrites)) g.__cerberusCookieWrites.push(raw);
        var semi = raw.indexOf(";");
        var pair = (semi === -1 ? raw : raw.slice(0, semi)).trim();
        var eq = pair.indexOf("=");
        if (eq === -1) return;
        var name = pair.slice(0, eq).trim();
        var jar = this.__cookie ? this.__cookie.split("; ") : [];
        // Deletion: a non-positive Max-Age (the modern idiom; it wins over Expires)
        // or a past Expires drops the cookie from the view, matching the jar (which
        // expires it) so a same-turn re-read agrees. The raw string was already
        // queued above for the jar to honor Path/Domain/etc. exactly.
        var isDelete = false;
        if (semi !== -1) {
          var attrs = raw.slice(semi + 1);
          var ma = /(?:^|;)\s*max-age\s*=\s*(-?\d+)/i.exec(attrs);
          if (ma) {
            if (parseInt(ma[1], 10) <= 0) isDelete = true;
          } else {
            var ex = /(?:^|;)\s*expires\s*=\s*([^;]+)/i.exec(attrs);
            if (ex) { try { var t = Date.parse(ex[1]); if (t === t && t <= Date.now()) isDelete = true; } catch (e) {} }
          }
        }
        var out = [];
        var replaced = false;
        for (var i = 0; i < jar.length; i++) {
          if (jar[i].slice(0, jar[i].indexOf("=")) === name) {
            replaced = true;
            if (!isDelete) out.push(pair);
          } else {
            out.push(jar[i]);
          }
        }
        if (!replaced && !isDelete) out.push(pair);
        this.__cookie = out.join("; ");
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
    // Document collections: `document.forms` (with named access by id/name, as
    // pages do `document.forms.login`), `document.images`, and `document.links`
    // (anchors/areas that actually have an href). Live-ish: recomputed per read.
    Object.defineProperty(document, "forms", {
      get: function () {
        var list = this.documentElement ? queryAll(this.documentElement, "form") : [];
        for (var i = 0; i < list.length; i++) {
          var key = getAttr(list[i], "id") || getAttr(list[i], "name");
          if (key && !(key in list)) {
            Object.defineProperty(list, key, { value: list[i], enumerable: false, configurable: true });
          }
        }
        return list;
      },
      enumerable: true, configurable: true,
    });
    Object.defineProperty(document, "images", {
      get: function () { return this.documentElement ? queryAll(this.documentElement, "img") : []; },
      enumerable: true, configurable: true,
    });
    Object.defineProperty(document, "links", {
      get: function () {
        if (!this.documentElement) return [];
        var out = [];
        walkElements(this.documentElement, function (el) {
          if ((el.__tag === "a" || el.__tag === "area") && getAttr(el, "href") !== null) out.push(el);
        });
        return out;
      },
      enumerable: true, configurable: true,
    });
    document.getElementsByName = function (n) {
      n = String(n);
      var out = [];
      if (this.documentElement) {
        walkElements(this.documentElement, function (el) { if (getAttr(el, "name") === n) out.push(el); });
      }
      return out;
    };
    document.createElement = function (tag) { return makeElement(tag); };
    document.createTextNode = function (text) { return makeText(text); };
    document.createDocumentFragment = function () {
      // A lightweight fragment: appendChild moves its children, like the spec,
      // but we model it as a bare element whose kids get re-parented on insert.
      return makeElement("#fragment");
    };
    document.addEventListener = function (type, fn) {
      type = String(type); if (!this.__listeners[type]) this.__listeners[type] = [];
      if (typeof fn === "function") this.__listeners[type].push(fn);
    };
    document.removeEventListener = function (type, fn) {
      type = String(type); var arr = this.__listeners[type]; if (!arr) return;
      var i = arr.indexOf(fn); if (i !== -1) arr.splice(i, 1);
    };
    document.dispatchEvent = function (ev) {
      var arr = this.__listeners[ev && ev.type]; if (!arr) return true;
      var copy = arr.slice();
      for (var i = 0; i < copy.length; i++) { try { copy[i].call(this, ev); } catch (e) {} }
      return true;
    };

    g.document = document;

    // ---- window = globalThis -------------------------------------------
    g.window = g;
    g.self = g;
    window.document = document;
    if (!window.__listeners) window.__listeners = Object.create(null);
    window.addEventListener = function (type, fn) {
      type = String(type); if (!this.__listeners[type]) this.__listeners[type] = [];
      if (typeof fn === "function") this.__listeners[type].push(fn);
    };
    window.removeEventListener = function (type, fn) {
      type = String(type); var arr = this.__listeners[type]; if (!arr) return;
      var i = arr.indexOf(fn); if (i !== -1) arr.splice(i, 1);
    };
    window.dispatchEvent = function (ev) {
      var arr = this.__listeners[ev && ev.type]; if (!arr) return true;
      var copy = arr.slice();
      for (var i = 0; i < copy.length; i++) { try { copy[i].call(this, ev); } catch (e) {} }
      return true;
    };

    // ---- ambient environment (location/navigator/screen/storage/…) -----
    // All derived from globalThis.__CERBERUS_ENV__ = { url, width, height },
    // injected by run_page_scripts before this prelude. We never throw: a
    // missing/garbage env falls back to inert defaults.
    var env = (g.__CERBERUS_ENV__ && typeof g.__CERBERUS_ENV__ === "object") ? g.__CERBERUS_ENV__ : {};
    // A COHERENT per-head fingerprint profile, injected by the app before this
    // prelude (cerberus-profile's profile_prologue). When present, EVERY identity
    // axis (userAgent/platform/vendor/UA-CH/screen/viewport) is read from it so
    // the axes cannot disagree — a split-brain (e.g. Linux platform + Windows
    // userAgentData) is the canonical anti-bot cross-check. When absent (unit
    // tests inject none), we fall back to the honest-first envUA persona below.
    var __prof = (g.__CERBERUS_PROFILE__ && typeof g.__CERBERUS_PROFILE__ === "object") ? g.__CERBERUS_PROFILE__ : null;
    var __pn = (__prof && __prof.navigator && typeof __prof.navigator === "object") ? __prof.navigator : null;
    var envUrl = (typeof env.url === "string") ? env.url : "about:blank";
    var vpW = (typeof env.width === "number") ? env.width : 0;
    var vpH = (typeof env.height === "number") ? env.height : 0;
    // The UA the network stack actually presented to this origin (honest by
    // default; the escalated rung if this site forced it). Falls back to our
    // honest identity if absent, so header and navigator can never disagree.
    var envUA = (typeof env.userAgent === "string" && env.userAgent) ? env.userAgent : "Cerberus/0.0";
    // Seed `document.cookie` with the cookies the sealed jar would send to this
    // origin (a "name=value; name=value" string, no attributes). The host
    // supplies it per install/navigation, so after a challenge sets a cookie the
    // reload re-seeds it from the jar. Writes are captured back (see the setter).
    document.__cookie = (typeof env.cookie === "string") ? env.cookie : "";

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
    var __locHref = locationObj.href;
    // Navigation is the host's job (resolve + fetch + re-render), so the page's
    // location methods and `location.href =` RECORD the intent for the host to
    // drain (__cerberusTakeNavigations). A cookie-gated reload — e.g. a bot
    // challenge that sets its token then reloads to fetch the real page —
    // depends on this. `replace` distinguishes history-replacing navigations.
    function recordNav(u, replace) {
      u = String(u);
      if (!u) return;
      // `replace` as 1/0, not a JS boolean: the host's JSON layer carries only
      // strings/ints/arrays/objects (booleans are intentionally unsupported).
      if (Array.isArray(g.__cerberusNavigations)) {
        g.__cerberusNavigations.push({ url: u, replace: replace ? 1 : 0 });
      }
    }
    locationObj.assign = function (u) { recordNav(u, false); };
    locationObj.replace = function (u) { recordNav(u, true); };
    locationObj.reload = function () { recordNav(__locHref, true); };
    locationObj.toString = function () { return this.href; };
    // `location.href = "..."` navigates (like assign); keep the readable value in
    // sync so a same-turn re-read reflects the assignment.
    Object.defineProperty(locationObj, "href", {
      get: function () { return __locHref; },
      set: function (u) { __locHref = String(u); recordNav(__locHref, false); },
      enumerable: true, configurable: true,
    });
    // `window.location = "url"` / `document.location = "url"` are navigations too
    // (string assignment); reading still returns the location object. window and
    // self alias globalThis (g), so defining on g covers both.
    function defineLocation(obj) {
      Object.defineProperty(obj, "location", {
        get: function () { return locationObj; },
        set: function (u) { if (typeof u === "string") locationObj.href = u; },
        enumerable: true, configurable: true,
      });
    }
    defineLocation(g);
    defineLocation(document);
    Object.defineProperty(document, "URL", { get: function () { return locationObj.href; }, enumerable: true, configurable: true });
    Object.defineProperty(document, "documentURI", { get: function () { return locationObj.href; }, enumerable: true, configurable: true });

    // ---- navigator -----------------------------------------------------
    // IDENTITY MODEL:
    //  1. The User-Agent is HONEST-FIRST and COHERENT. `userAgent` is whatever
    //     the network stack actually sent this origin — our real `Cerberus/0.0`
    //     by default, or, only if the site's bot management forced the fallback
    //     ladder, the SAME escalated string the request header carried. The OS
    //     in `platform` is derived from it. So the header and the script-visible
    //     identity can never disagree; a mismatch would itself be a fingerprint.
    //  2. The rest of the surface is a COMPLETE, COHERENT Chrome-142-on-Windows
    //     persona (fixed for the validation phase): every API a real Chrome
    //     exposes is present with Chrome-on-Windows values, because a *missing*
    //     or *impossible* read is itself the tell an anti-bot sensor (reese84 /
    //     Imperva) or fingerprint scanner (pixelscan) fails on. The genuinely
    //     high-entropy reads (canvas / audio / WebGL / font-metrics) are ±1
    //     farbled per head by the separate farbling prologue, not here.
    // Identity axes: from the coherent profile when present, else the honest-first
    // envUA and its derived OS. Read defensively; a missing profile field falls
    // back to the same default the no-profile path uses.
    var navUA = (__pn && typeof __pn.userAgent === "string" && __pn.userAgent) ? __pn.userAgent : envUA;
    var navPlatform = (__pn && typeof __pn.platform === "string" && __pn.platform) ? __pn.platform
      : (navUA.indexOf("Windows") >= 0 ? "Win32"
        : (navUA.indexOf("Mac OS X") >= 0 || navUA.indexOf("Macintosh") >= 0) ? "MacIntel"
        : "Linux x86_64");
    var navVendor = (__pn && typeof __pn.vendor === "string") ? __pn.vendor : "Google Inc.";
    var navHwc = (__pn && typeof __pn.hardwareConcurrency === "number") ? __pn.hardwareConcurrency : 4;
    var navLanguage = (__pn && typeof __pn.language === "string" && __pn.language) ? __pn.language : "en-US";
    var navLanguages = (__pn && __pn.languages && typeof __pn.languages.length === "number" && __pn.languages.length)
      ? __pn.languages : ["en-US", "en"];
    var navMaxTouch = (__pn && typeof __pn.maxTouchPoints === "number") ? __pn.maxTouchPoints : 0;
    g.navigator = {
      userAgent: navUA,
      appCodeName: "Mozilla",
      appName: "Netscape",
      appVersion: navUA.indexOf("Mozilla/") === 0 ? navUA.slice(8) : navUA,
      product: "Gecko",
      productSub: "20030107",
      vendor: navVendor,
      vendorSub: "",
      language: navLanguage,
      languages: navLanguages,
      platform: navPlatform,
      hardwareConcurrency: navHwc,
      maxTouchPoints: navMaxTouch,
      onLine: true,
      cookieEnabled: true,
      doNotTrack: null,
      webdriver: false,
    };
    // deviceMemory is quantized GiB on Chromium but ABSENT on Firefox. With a
    // profile, a null value means "expose no such property" (real Firefox) — never
    // an explicit null, which is itself a tell. Without a profile keep today's 8.
    if (__pn) {
      if (typeof __pn.deviceMemory === "number") { g.navigator.deviceMemory = __pn.deviceMemory; }
    } else {
      g.navigator.deviceMemory = 8;
    }

    // High-entropy Client Hints + device/permission APIs. With a profile, EVERY
    // UA-CH axis is read from __prof.navigator.userAgentData so it can never
    // disagree with userAgent/platform; a null there means the browser exposes no
    // UA-CH at all (Firefox/Safari), so navigator.userAgentData stays undefined.
    // Without a profile we keep the fixed Chrome-142 / Windows-11 persona. A
    // scanner reads userAgentData.getHighEntropyValues(), mediaDevices.
    // enumerateDevices(), permissions.query(), etc.; any being undefined reads as
    // a non-browser.
    if (!__pn) {
      var __uaBrands = [
        { brand: "Chromium", version: "142" },
        { brand: "Google Chrome", version: "142" },
        { brand: "Not_A Brand", version: "24" },
      ];
      var __uaFullVersionList = [
        { brand: "Chromium", version: "142.0.0.0" },
        { brand: "Google Chrome", version: "142.0.0.0" },
        { brand: "Not_A Brand", version: "24.0.0.0" },
      ];
      var __highEntropyUA = {
        architecture: "x86", bitness: "64",
        brands: __uaBrands, fullVersionList: __uaFullVersionList,
        mobile: false, model: "", platform: "Windows", platformVersion: "15.0.0",
        uaFullVersion: "142.0.0.0", wow64: false,
      };
      g.navigator.userAgentData = {
        brands: __uaBrands, mobile: false, platform: "Windows",
        getHighEntropyValues: function () { return Promise.resolve(__highEntropyUA); },
        toJSON: function () { return { brands: __uaBrands, mobile: false, platform: "Windows" }; },
      };
    } else if (__pn.userAgentData && typeof __pn.userAgentData === "object") {
      // Profile with UA-CH: derive every axis from it (never the Windows const).
      var __uad = __pn.userAgentData;
      var __pBrands = (__uad.brands && typeof __uad.brands.length === "number") ? __uad.brands : [];
      var __pMobile = __uad.mobile === true;
      var __pUaPlatform = (typeof __uad.platform === "string") ? __uad.platform : "";
      var __pArch = (typeof __uad.architecture === "string") ? __uad.architecture : "";
      var __pBitness = (typeof __uad.bitness === "string") ? __uad.bitness : "";
      var __pPlatVer = (typeof __uad.platformVersion === "string") ? __uad.platformVersion : "";
      var __pFullVer = (typeof __uad.uaFullVersion === "string") ? __uad.uaFullVersion : "";
      // fullVersionList is not carried in the prologue; synthesize it coherently
      // from the brands + uaFullVersion. The real (non-GREASE) brands share the
      // full version; a GREASE brand keeps its own <major>.0.0.0.
      var __pMajor = __pFullVer ? String(__pFullVer).split(".")[0] : "";
      var __pFvl = [];
      for (var __bi = 0; __bi < __pBrands.length; __bi++) {
        var __b = __pBrands[__bi];
        var __bver = String(__b.version);
        __pFvl.push({ brand: __b.brand, version: (__bver === __pMajor ? __pFullVer : (__bver + ".0.0.0")) });
      }
      var __pHigh = {
        architecture: __pArch, bitness: __pBitness,
        brands: __pBrands, fullVersionList: __pFvl,
        mobile: __pMobile, model: "", platform: __pUaPlatform,
        platformVersion: __pPlatVer, uaFullVersion: __pFullVer, wow64: false,
      };
      g.navigator.userAgentData = {
        brands: __pBrands, mobile: __pMobile, platform: __pUaPlatform,
        getHighEntropyValues: function () { return Promise.resolve(__pHigh); },
        toJSON: function () { return { brands: __pBrands, mobile: __pMobile, platform: __pUaPlatform }; },
      };
    }
    // else: profile present but UA-CH null (Firefox/Safari) — leave
    // navigator.userAgentData absent, i.e. reads back as undefined.
    g.navigator.connection = {
      effectiveType: "4g", rtt: 50, downlink: 10, saveData: false, onchange: null,
      addEventListener: function () {}, removeEventListener: function () {},
    };
    g.navigator.mediaDevices = {
      enumerateDevices: function () {
        return Promise.resolve([
          { deviceId: "", kind: "audioinput", label: "", groupId: "" },
          { deviceId: "", kind: "videoinput", label: "", groupId: "" },
          { deviceId: "", kind: "audiooutput", label: "", groupId: "" },
        ]);
      },
      getUserMedia: function () { return Promise.reject(new Error("NotAllowedError")); },
      getDisplayMedia: function () { return Promise.reject(new Error("NotAllowedError")); },
      ondevicechange: null,
      addEventListener: function () {}, removeEventListener: function () {},
      dispatchEvent: function () { return false; },
    };
    g.navigator.permissions = {
      query: function (o) {
        // A real PermissionStatus extends EventTarget: pages routinely do
        // `permissions.query({name:...}).then(s => s.addEventListener('change', cb))`,
        // which throws if addEventListener is missing — and a sensor probes
        // `typeof status.addEventListener` as a conformance tell.
        return Promise.resolve({
          state: (o && o.name === "notifications") ? "default" : "granted",
          name: (o && o.name) ? o.name : "",
          onchange: null,
          addEventListener: function () {}, removeEventListener: function () {},
          dispatchEvent: function () { return false; },
        });
      },
    };
    g.navigator.getBattery = function () {
      return Promise.resolve({
        charging: true, chargingTime: 0, dischargingTime: Infinity, level: 1,
        onchargingchange: null, onchargingtimechange: null,
        ondischargingtimechange: null, onlevelchange: null,
        addEventListener: function () {}, removeEventListener: function () {},
        dispatchEvent: function () { return false; },
      });
    };
    g.navigator.sendBeacon = function (url, data) {
      // A real beacon is a keep-alive POST; route it through the shared fetch
      // queue (defined later in the prelude) so telemetry actually goes out and
      // is observable, instead of silently dropping it.
      try { if (typeof g.__cerberusBeacon === "function") g.__cerberusBeacon(url, "POST", data != null ? data : ""); } catch (e) {}
      return true;
    };
    g.navigator.storage = {
      estimate: function () { return Promise.resolve({ quota: 299977129984, usage: 0 }); },
      persisted: function () { return Promise.resolve(false); },
    };
    g.navigator.getGamepads = function () { return [null, null, null, null]; };

    // ---- screen + window metrics ---------------------------------------
    // GEOMETRY MODEL: the physically-impossible layout (window taller than the
    // monitor) is itself a bot flag, so we enforce
    //   innerHeight <= outerHeight <= availHeight <= screen.height
    // (and the width analogue) on BOTH paths. With a profile, `screen` is a real
    // monitor whose avail* already reserve OS chrome and whose viewport already
    // fits the work area; without one we fall back to the env viewport as an inert
    // no-chrome surface (screen == viewport), which trivially satisfies the
    // invariant.
    var __ps = (__prof && __prof.screen && typeof __prof.screen === "object") ? __prof.screen : null;
    var __pv = (__prof && __prof.viewport && typeof __prof.viewport === "object") ? __prof.viewport : null;
    function __numOr(v, d) { return (typeof v === "number" && isFinite(v)) ? v : d; }
    var innerW = __pv ? __numOr(__pv.innerWidth, vpW) : vpW;
    var innerH = __pv ? __numOr(__pv.innerHeight, vpH) : vpH;
    var scrW = __ps ? __numOr(__ps.width, innerW) : innerW;
    var scrH = __ps ? __numOr(__ps.height, innerH) : innerH;
    var availW = __ps ? __numOr(__ps.availWidth, scrW) : innerW;
    var availH = __ps ? __numOr(__ps.availHeight, scrH) : innerH;
    var availLeft = __ps ? __numOr(__ps.availLeft, 0) : 0;
    var availTop = __ps ? __numOr(__ps.availTop, 0) : 0;
    var colorDepth = __ps ? __numOr(__ps.colorDepth, 24) : 24;
    var pixelDepth = __ps ? __numOr(__ps.pixelDepth, 24) : 24;
    var devicePixelRatio = __ps ? __numOr(__ps.devicePixelRatio, 1) : 1;
    g.screen = {
      width: scrW, height: scrH, availWidth: availW, availHeight: availH,
      availLeft: availLeft, availTop: availTop,
      colorDepth: colorDepth, pixelDepth: pixelDepth,
      // ScreenOrientation: real browsers always expose this; anti-bot sensors
      // read screen.orientation.type/angle and throw if it is absent.
      orientation: {
        type: (scrW >= scrH ? "landscape-primary" : "portrait-primary"),
        angle: 0, onchange: null,
        addEventListener: function () {}, removeEventListener: function () {},
        dispatchEvent: function () { return false; },
        lock: function () { return Promise.reject(new Error("NotSupportedError")); },
        unlock: function () {},
      },
    };
    window.innerWidth = innerW;
    window.innerHeight = innerH;
    // The outer window is the viewport plus browser chrome (tabstrip + toolbar +
    // omnibox, ~88px on Windows Chrome). We CLAMP to the work area so the outer
    // window can never exceed the monitor — outerHeight > screen.height is an
    // instant headless tell. With no profile the surface has no chrome (screen ==
    // viewport), so outer == inner; a window taller than the screen never occurs.
    var __chromeH = __prof ? 88 : 0;
    window.outerWidth = Math.min(innerW, availW);
    window.outerHeight = Math.min(innerH + __chromeH, availH);
    window.devicePixelRatio = devicePixelRatio;
    window.scrollX = 0; window.scrollY = 0;
    window.pageXOffset = 0; window.pageYOffset = 0;
    window.scrollTo = function () {}; window.scrollBy = function () {}; window.scroll = function () {};

    // ---- window frame identity + fingerprint surface -------------------
    // A top-level browsing context: top/parent/frames/self all alias this
    // window, there is no host <iframe> (frameElement null), and there are no
    // child frames (length 0). A sensor cross-checks these; a mismatch (e.g.
    // self !== window, or a stray frameElement) reads as an instrumented frame.
    g.top = g; g.parent = g; g.frames = g;
    window.frameElement = null;
    window.length = 0;
    window.name = "";
    window.screenX = 0; window.screenY = 0;
    window.screenLeft = 0; window.screenTop = 0;
    window.postMessage = function () {};
    window.getSelection = function () {
      return {
        toString: function () { return ""; }, rangeCount: 0,
        removeAllRanges: function () {}, addRange: function () {},
        getRangeAt: function () { return {}; },
      };
    };
    window.CSS = { supports: function () { return true; }, escape: function (s) { return String(s); } };
    // visualViewport mirrors the layout viewport (no pinch-zoom in this model).
    window.visualViewport = {
      width: innerW, height: innerH, scale: 1, offsetLeft: 0, offsetTop: 0,
      pageLeft: 0, pageTop: 0, onresize: null, onscroll: null,
      addEventListener: function () {}, removeEventListener: function () {},
    };
    // Deterministic wall clock. The real Date.now()/new Date() read process
    // wall-clock time, which (a) varies every render — the same script-driven
    // page then lays out differently each load, so a screenshot is not
    // reproducible — and (b) is a timing / clock-skew fingerprint surface. We
    // replace the *current-time* reads with a fixed base epoch advanced by a
    // deterministic monotonic tick; explicit dates (new Date(ms), Date.parse,
    // Date.UTC) and every prototype method are preserved unchanged. The base is
    // a plausible recent "now" so cookie-expiry and campaign-date logic still
    // behaves. (This is the "Date neutralized" path the performance shim below
    // already anticipates.)
    (function () {
      var __RD = Date, __base = 1751000000000, __tick = 0;
      function __now() { __tick += 1; return __base + __tick; }
      function CDate() {
        if (arguments.length === 0) { return new __RD(__now()); }
        return Reflect.construct(__RD, arguments);
      }
      CDate.prototype = __RD.prototype;
      CDate.now = __now;
      CDate.parse = __RD.parse;
      CDate.UTC = __RD.UTC;
      globalThis.Date = CDate;
    })();
    // performance: a MONOTONIC ms clock anchored to a real epoch. A loaded
    // document with timeOrigin === 0 and all-zero timing is an impossible read
    // (loadEventEnd would predate the epoch), and timeOrigin+now() must track
    // Date.now() within a few thousand ms. We anchor timeOrigin to the wall
    // clock (Date is live here) and lay a plausible ~388ms load sequence under
    // it; now() reports elapsed time since the anchor, forced strictly
    // increasing. If Date were ever neutralized we fall back to a fixed
    // plausible epoch and the old counter, still never emitting a zero clock.
    // ---- Web IDL basics: DOMException, Event/CustomEvent, EventTarget ---
    // QuickJS ships none of these. They are load-bearing for conformance:
    // crypto.getRandomValues throws DOMExceptions, real code constructs Events,
    // and objects like `performance` are EventTargets. Guarded so any native
    // impl wins.
    if (typeof g.DOMException !== "function") {
      var DOM_CODES = {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
        NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
        SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
        InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18,
        NetworkError: 19, AbortError: 20, URLMismatchError: 21,
        QuotaExceededError: 22, TimeoutError: 23, InvalidNodeTypeError: 24,
        DataCloneError: 25
      };
      var DOMExceptionCtor = function DOMException(message, name) {
        var e = this instanceof DOMExceptionCtor ? this : Object.create(DOMExceptionCtor.prototype);
        var nm = name === undefined ? "Error" : String(name);
        var msg = message === undefined ? "" : String(message);
        Object.defineProperty(e, "message", { value: msg, configurable: true, writable: true });
        Object.defineProperty(e, "name", { value: nm, configurable: true, writable: true });
        Object.defineProperty(e, "code", { value: DOM_CODES[nm] || 0, configurable: true, writable: true });
        var st = ""; try { st = new Error(msg).stack || ""; } catch (x) {}
        Object.defineProperty(e, "stack", { value: nm + ": " + msg + "\n" + st, configurable: true, writable: true });
        return e;
      };
      DOMExceptionCtor.prototype = Object.create(Error.prototype);
      DOMExceptionCtor.prototype.constructor = DOMExceptionCtor;
      DOMExceptionCtor.prototype.name = "Error";
      DOMExceptionCtor.prototype.message = "";
      DOMExceptionCtor.prototype.toString = function () { return this.name + ": " + this.message; };
      g.DOMException = DOMExceptionCtor;
    }
    if (typeof g.QuotaExceededError !== "function") {
      // Modern interface (a DOMException subclass) that
      // assert_throws_quotaexceedederror and storage/crypto APIs check against.
      var QEE = function QuotaExceededError(message, options) {
        var e = this instanceof QEE ? this : Object.create(QEE.prototype);
        g.DOMException.call(e, message, "QuotaExceededError");
        var q = (options && options.quota != null) ? Number(options.quota) : null;
        var r = (options && options.requested != null) ? Number(options.requested) : null;
        Object.defineProperty(e, "quota", { value: q, configurable: true });
        Object.defineProperty(e, "requested", { value: r, configurable: true });
        return e;
      };
      QEE.prototype = Object.create(g.DOMException.prototype);
      QEE.prototype.constructor = QEE;
      g.QuotaExceededError = QEE;
    }
    if (typeof g.Event !== "function") {
      g.Event = function Event(type, init) {
        init = init || {};
        this.type = String(type);
        this.bubbles = !!init.bubbles; this.cancelable = !!init.cancelable; this.composed = !!init.composed;
        this.defaultPrevented = false; this.target = null; this.currentTarget = null;
        this.eventPhase = 0; this.timeStamp = 0; this.isTrusted = false;
      };
      g.Event.prototype.preventDefault = function () { if (this.cancelable) this.defaultPrevented = true; };
      g.Event.prototype.stopPropagation = function () {};
      g.Event.prototype.stopImmediatePropagation = function () {};
      g.Event.NONE = 0; g.Event.CAPTURING_PHASE = 1; g.Event.AT_TARGET = 2; g.Event.BUBBLING_PHASE = 3;
    }
    if (typeof g.CustomEvent !== "function") {
      g.CustomEvent = function CustomEvent(type, init) {
        g.Event.call(this, type, init);
        this.detail = (init && "detail" in init) ? init.detail : null;
      };
      g.CustomEvent.prototype = Object.create(g.Event.prototype);
      g.CustomEvent.prototype.constructor = g.CustomEvent;
    }
    if (typeof g.URLSearchParams !== "function") {
      var USP = function URLSearchParams(init) {
        this.__p = [];
        var self = this;
        if (typeof init === "string") {
          var q = init.charAt(0) === "?" ? init.slice(1) : init;
          if (q) q.split("&").forEach(function (pair) {
            if (!pair) return;
            var i = pair.indexOf("=");
            var k = i < 0 ? pair : pair.slice(0, i);
            var v = i < 0 ? "" : pair.slice(i + 1);
            try { self.__p.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); }
            catch (e) { self.__p.push([k, v]); }
          });
        } else if (init && typeof init.forEach === "function" && !Array.isArray(init)) {
          init.forEach(function (v, k) { self.__p.push([String(k), String(v)]); });
        } else if (Array.isArray(init)) {
          init.forEach(function (e) { self.__p.push([String(e[0]), String(e[1])]); });
        } else if (init && typeof init === "object") {
          for (var kk in init) if (Object.prototype.hasOwnProperty.call(init, kk)) self.__p.push([String(kk), String(init[kk])]);
        }
      };
      USP.prototype.append = function (k, v) { this.__p.push([String(k), String(v)]); };
      USP.prototype.set = function (k, v) {
        k = String(k); v = String(v); var seen = false;
        this.__p = this.__p.filter(function (e) { if (e[0] !== k) return true; if (!seen) { e[1] = v; seen = true; return true; } return false; });
        if (!seen) this.__p.push([k, v]);
      };
      USP.prototype.get = function (k) { k = String(k); for (var i = 0; i < this.__p.length; i++) if (this.__p[i][0] === k) return this.__p[i][1]; return null; };
      USP.prototype.getAll = function (k) { k = String(k); return this.__p.filter(function (e) { return e[0] === k; }).map(function (e) { return e[1]; }); };
      USP.prototype.has = function (k) { return this.get(String(k)) !== null; };
      USP.prototype["delete"] = function (k) { k = String(k); this.__p = this.__p.filter(function (e) { return e[0] !== k; }); };
      USP.prototype.forEach = function (fn, thisArg) { var self = this; this.__p.slice().forEach(function (e) { fn.call(thisArg, e[1], e[0], self); }); };
      USP.prototype.toString = function () { return this.__p.map(function (e) { return encodeURIComponent(e[0]) + "=" + encodeURIComponent(e[1]); }).join("&"); };
      g.URLSearchParams = USP;
    }
    if (typeof g.URL !== "function") {
      // A pragmatic URL parser (absolute + relative resolution + special-scheme
      // origins). Not full-WHATWG (that torture-tests thousands of edge cases),
      // but correct for the URLs real pages use. `createObjectURL`/`revokeObjectURL`
      // statics are attached later in the Blob section.
      var SPECIAL = { "http:": "80", "https:": "443", "ws:": "80", "wss:": "443", "ftp:": "21" };
      // RFC 3986 remove_dot_segments: resolve "." and ".." in a path.
      var normPath = function (input) {
        var output = [];
        while (input.length) {
          if (input.slice(0, 3) === "../") input = input.slice(3);
          else if (input.slice(0, 2) === "./") input = input.slice(2);
          else if (input.slice(0, 3) === "/./") input = "/" + input.slice(3);
          else if (input === "/.") input = "/";
          else if (input.slice(0, 4) === "/../") { input = "/" + input.slice(4); output.pop(); }
          else if (input === "/..") { input = "/"; output.pop(); }
          else if (input === "." || input === "..") input = "";
          else {
            var seg, rest = input.charAt(0) === "/" ? input.indexOf("/", 1) : input.indexOf("/");
            seg = rest < 0 ? input : input.slice(0, rest);
            output.push(seg); input = input.slice(seg.length);
          }
        }
        return output.join("");
      };
      var __oldURL = (typeof g.URL === "object") ? g.URL : null;
      var URLCtor = function URL(url, base) {
        var input = String(url == null ? "" : url).replace(/^[\x00-\x20]+|[\x00-\x20]+$/g, "");
        var abs, hasScheme = /^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(input);
        if (hasScheme) {
          abs = input;
        } else {
          if (base == null) throw new TypeError("Failed to construct 'URL': Invalid URL");
          var b = (base && base.__isURL) ? base : new URLCtor(base);
          if (input === "") abs = b.href;
          else if (input.slice(0, 2) === "//") abs = b.protocol + input;
          else if (input.charAt(0) === "/") abs = b.protocol + "//" + b.__auth + input;
          else if (input.charAt(0) === "?") abs = b.protocol + "//" + b.__auth + b.pathname + input;
          else if (input.charAt(0) === "#") abs = b.protocol + "//" + b.__auth + b.pathname + b.search + input;
          else {
            var dir = b.pathname.slice(0, b.pathname.lastIndexOf("/") + 1);
            abs = b.protocol + "//" + b.__auth + (dir || "/") + input;
          }
        }
        var pm = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)(\/\/([^\/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(abs);
        if (!pm) throw new TypeError("Failed to construct 'URL': Invalid URL");
        this.__isURL = true;
        this.protocol = pm[1].toLowerCase();
        var hasAuthority = pm[2] != null;
        var authority = pm[3] || "";
        var at = authority.lastIndexOf("@");
        var userinfo = at >= 0 ? authority.slice(0, at) : "";
        var hostport = at >= 0 ? authority.slice(at + 1) : authority;
        var ci = userinfo.indexOf(":");
        this.username = userinfo ? (ci >= 0 ? userinfo.slice(0, ci) : userinfo) : "";
        this.password = (userinfo && ci >= 0) ? userinfo.slice(ci + 1) : "";
        if (hostport.charAt(0) === "[") { var rb = hostport.indexOf("]"); this.hostname = hostport.slice(0, rb + 1).toLowerCase(); this.port = hostport.slice(rb + 2) || ""; }
        else { var c = hostport.lastIndexOf(":"); if (c >= 0) { this.hostname = hostport.slice(0, c).toLowerCase(); this.port = hostport.slice(c + 1); } else { this.hostname = hostport.toLowerCase(); this.port = ""; } }
        if (this.port && SPECIAL[this.protocol] === this.port) this.port = "";
        this.host = this.hostname + (this.port ? ":" + this.port : "");
        this.__auth = (userinfo ? userinfo + "@" : "") + this.host;
        this.pathname = hasAuthority ? normPath(pm[4] || "/") : (pm[4] || "");
        this.search = pm[5] || "";
        this.hash = pm[6] || "";
        this.searchParams = new g.URLSearchParams(this.search);
        this.origin = (SPECIAL[this.protocol] && this.host) ? (this.protocol + "//" + this.host) : "null";
        this.href = hasAuthority
          ? this.protocol + "//" + this.__auth + this.pathname + this.search + this.hash
          : this.protocol + this.pathname + this.search + this.hash;
      };
      URLCtor.prototype.toString = function () { return this.href; };
      URLCtor.prototype.toJSON = function () { return this.href; };
      if (__oldURL) { for (var __uk in __oldURL) { try { URLCtor[__uk] = __oldURL[__uk]; } catch (e) {} } }
      g.URL = URLCtor;
    }
    if (typeof g.EventTarget !== "function") {
      g.EventTarget = function EventTarget() { this.__ls = Object.create(null); };
      g.EventTarget.prototype.addEventListener = function (type, fn, opts) {
        if (typeof fn !== "function" && !(fn && typeof fn.handleEvent === "function")) return;
        type = String(type); if (!this.__ls) this.__ls = Object.create(null);
        (this.__ls[type] = this.__ls[type] || []).push({ fn: fn, once: !!(opts && opts.once) });
      };
      g.EventTarget.prototype.removeEventListener = function (type, fn) {
        type = String(type); var a = this.__ls && this.__ls[type]; if (!a) return;
        for (var i = a.length - 1; i >= 0; i--) if (a[i].fn === fn) a.splice(i, 1);
      };
      g.EventTarget.prototype.dispatchEvent = function (ev) {
        var a = this.__ls && this.__ls[ev && ev.type]; if (!a) return true;
        if (ev) { ev.target = this; ev.currentTarget = this; }
        var copy = a.slice();
        for (var i = 0; i < copy.length; i++) {
          var l = copy[i], h = (l.fn && typeof l.fn.handleEvent === "function") ? l.fn.handleEvent : l.fn;
          try { h.call(this, ev); } catch (x) {}
          if (l.once) { var j = a.indexOf(l); if (j >= 0) a.splice(j, 1); }
        }
        return !(ev && ev.defaultPrevented);
      };
    }

    (function () {
      var __rawNow = (typeof Date === "function" && Date.now) ? Date.now() : 0;
      var __hasClock = __rawNow > 1000000000000;
      var __origin = __hasClock ? (__rawNow - 388) : 1751000000000;
      var __perfCounter = 388;
      var __perfLast = 0;
      window.performance = {
        now: function () {
          var t = __hasClock ? (Date.now() - __origin) : (__perfCounter += 0.1);
          if (t <= __perfLast) { t = __perfLast + 0.001; }
          __perfLast = t;
          return t;
        },
        timeOrigin: __origin,
        // Legacy PerformanceTiming: absolute epoch-ms, monotonically ordered
        // navigationStart < fetchStart < ... < loadEventEnd (== ~now).
        timing: {
          navigationStart: __origin,
          unloadEventStart: 0, unloadEventEnd: 0, redirectStart: 0, redirectEnd: 0,
          fetchStart: __origin + 3,
          domainLookupStart: __origin + 5, domainLookupEnd: __origin + 20,
          connectStart: __origin + 20, secureConnectionStart: __origin + 30,
          connectEnd: __origin + 45, requestStart: __origin + 46,
          responseStart: __origin + 120, responseEnd: __origin + 180,
          domLoading: __origin + 125, domInteractive: __origin + 260,
          domContentLoadedEventStart: __origin + 262,
          domContentLoadedEventEnd: __origin + 268, domComplete: __origin + 380,
          loadEventStart: __origin + 381, loadEventEnd: __origin + 388,
        },
        navigation: { type: 0, redirectCount: 0 },
        memory: { jsHeapSizeLimit: 2172649472, totalJSHeapSize: 20000000, usedJSHeapSize: 10000000 },
        getEntriesByType: function () { return []; },
        getEntries: function () { return []; },
        getEntriesByName: function () { return []; },
        mark: function () {}, measure: function () {},
        clearMarks: function () {}, clearMeasures: function () {},
        setResourceTimingBufferSize: function () {},
        toJSON: function () { return {}; },
      };
      // Performance is an EventTarget (WPT hr-time checks event dispatch works).
      window.performance.__ls = Object.create(null);
      window.performance.addEventListener = g.EventTarget.prototype.addEventListener;
      window.performance.removeEventListener = g.EventTarget.prototype.removeEventListener;
      window.performance.dispatchEvent = g.EventTarget.prototype.dispatchEvent;
    })();
    // TextEncoder / TextDecoder — real UTF-8 (QuickJS may ship none). Anti-bot
    // payloads are encoded/decoded with these; a throwing or absent impl breaks
    // the sensor. Guarded so a native impl (if present) wins.
    if (typeof g.TextEncoder !== "function") {
      g.TextEncoder = function () { this.encoding = "utf-8"; };
      g.TextEncoder.prototype.encode = function (str) {
        str = String(str === undefined ? "" : str);
        var bytes = [];
        for (var i = 0; i < str.length; i++) {
          var c = str.charCodeAt(i);
          if (c < 0x80) {
            bytes.push(c);
          } else if (c < 0x800) {
            bytes.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
          } else if (c >= 0xd800 && c <= 0xdbff) {
            // High surrogate: pair it with a following low surrogate for the
            // 4-byte astral encoding, else the WHATWG encoding spec substitutes
            // U+FFFD. Real Chrome emits EF BF BD here, NOT the raw 3-byte WTF-8
            // surrogate value — a byte-for-byte fingerprint tell otherwise.
            var c2 = (i + 1 < str.length) ? str.charCodeAt(i + 1) : 0;
            if (c2 >= 0xdc00 && c2 <= 0xdfff) {
              var cp = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
              bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
              i++;
            } else {
              bytes.push(0xef, 0xbf, 0xbd);
            }
          } else if (c >= 0xdc00 && c <= 0xdfff) {
            // A lone low surrogate is also ill-formed → U+FFFD.
            bytes.push(0xef, 0xbf, 0xbd);
          } else {
            bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
          }
        }
        return new Uint8Array(bytes);
      };
      g.TextEncoder.prototype.encodeInto = function (str, dest) {
        var enc = this.encode(str), n = Math.min(enc.length, dest.length);
        for (var i = 0; i < n; i++) dest[i] = enc[i];
        return { read: n, written: n };
      };
    }
    if (typeof g.TextDecoder !== "function") {
      g.TextDecoder = function (label) {
        this.encoding = label ? String(label).toLowerCase() : "utf-8";
        this.fatal = false; this.ignoreBOM = false;
      };
      g.TextDecoder.prototype.decode = function (buf) {
        if (buf == null) return "";
        // Accept a Uint8Array, any ArrayBufferView (honouring its offset/length),
        // or a bare ArrayBuffer.
        var bytes = (buf instanceof Uint8Array) ? buf
          : (buf && buf.buffer && typeof buf.byteOffset === "number")
            ? new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength)
            : new Uint8Array(buf);
        var enc = this.encoding;
        if (enc === "utf-16le" || enc === "utf-16" || enc === "utf-16be") {
          var le = (enc !== "utf-16be");
          var s = "", k = 0, m = bytes.length;
          var first = true;
          while (k + 1 < m) {
            var unit = le ? (bytes[k] | (bytes[k + 1] << 8)) : ((bytes[k] << 8) | bytes[k + 1]);
            k += 2;
            // Strip a leading BOM (U+FEFF) unless ignoreBOM is set.
            if (first) { first = false; if (!this.ignoreBOM && unit === 0xFEFF) continue; }
            s += String.fromCharCode(unit);
          }
          return s;
        }
        var out = "", i = 0, n = bytes.length;
        while (i < n) {
          var b0 = bytes[i++];
          if (b0 < 0x80) {
            out += String.fromCharCode(b0);
          } else if (b0 >= 0xc0 && b0 < 0xe0) {
            var b1 = bytes[i++] & 0x3f;
            out += String.fromCharCode(((b0 & 0x1f) << 6) | b1);
          } else if (b0 >= 0xe0 && b0 < 0xf0) {
            var e1 = bytes[i++] & 0x3f, e2 = bytes[i++] & 0x3f;
            out += String.fromCharCode(((b0 & 0x0f) << 12) | (e1 << 6) | e2);
          } else {
            var f1 = bytes[i++] & 0x3f, f2 = bytes[i++] & 0x3f, f3 = bytes[i++] & 0x3f;
            var dcp = (((b0 & 0x07) << 18) | (f1 << 12) | (f2 << 6) | f3) - 0x10000;
            out += String.fromCharCode(0xd800 + (dcp >> 10), 0xdc00 + (dcp & 0x3ff));
          }
        }
        return out;
      };
    }

    // ---- storage (in-memory, RUN-SCOPED) -------------------------------
    // getItem/setItem/removeItem/clear/key/length plus index access via the
    // methods. These live for THIS RUN ONLY — there is no persistence across
    // run_page_scripts calls (the realm/prelude is reinstalled each time).
    function makeStorage() {
      var data = Object.create(null);
      var keys = [];
      return {
        getItem: function (k) { k = String(k); return (k in data) ? data[k] : null; },
        setItem: function (k, v) {
          k = String(k);
          if (!(k in data)) keys.push(k);
          data[k] = String(v);
        },
        removeItem: function (k) {
          k = String(k);
          if (k in data) { delete data[k]; var i = keys.indexOf(k); if (i !== -1) keys.splice(i, 1); }
        },
        clear: function () { data = Object.create(null); keys = []; },
        key: function (i) { i = i >>> 0; return (i < keys.length) ? keys[i] : null; },
        get length() { return keys.length; },
      };
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
    // Resolve a media-query length to px against the viewport, honoring units —
    // `parseInt` alone silently read `30em`/`50vw` as bare pixel counts. `em`/
    // `rem` use the 16px root default (matchMedia has no element context), and
    // `vw`/`vh` are viewport-relative.
    function __cerberusMediaPx(val, w, h) {
      var num = parseFloat(val);
      if (isNaN(num)) return 0;
      if (/vw$/i.test(val)) return num * w / 100;
      if (/vh$/i.test(val)) return num * h / 100;
      if (/r?em$/i.test(val)) return num * 16;
      return num; // px or unitless
    }
    // Discrete (non-length) media features fixed to the Chrome-on-Windows
    // desktop persona: a light-scheme, motion-allowing, mouse-driven machine.
    // Returning `false` for *every* prefers-color-scheme value (the prior
    // behavior) is an impossible state a scanner flags; exactly one value of
    // each feature must match.
    var __cerberusMediaFeatures = {
      "prefers-color-scheme": "light",
      "prefers-reduced-motion": "no-preference",
      "prefers-reduced-transparency": "no-preference",
      "prefers-contrast": "no-preference",
      "forced-colors": "none",
      "pointer": "fine",
      "any-pointer": "fine",
      "hover": "hover",
      "any-hover": "hover",
      "color-gamut": "srgb",
      "dynamic-range": "standard",
      "update": "fast",
      "scripting": "enabled",
    };
    function __cerberusEvalMedia(query, w, h) {
      return String(query).split(",").some(function (branch) {
        var re = /\(([a-z-]+)\s*:\s*([^)]+)\)/g, m, ok = true, any = false;
        while ((m = re.exec(branch)) !== null) {
          any = true;
          var name = m[1], val = m[2].trim(), px = __cerberusMediaPx(val, w, h);
          if (name === "min-width") ok = ok && w >= px;
          else if (name === "max-width") ok = ok && w <= px;
          else if (name === "min-height") ok = ok && h >= px;
          else if (name === "max-height") ok = ok && h <= px;
          else if (name === "orientation") ok = ok && (val === "portrait" ? h >= w : w > h);
          else if (Object.prototype.hasOwnProperty.call(__cerberusMediaFeatures, name)) {
            ok = ok && (val === __cerberusMediaFeatures[name]);
          }
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

    // ---- history (inert) -----------------------------------------------
    g.history = {
      length: 1, state: null, scrollRestoration: "auto",
      pushState: function () {}, replaceState: function () {},
      back: function () {}, forward: function () {}, go: function () {},
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
        // Focus defaults to <body> (never undefined) until a script focuses
        // something; a real document.activeElement is body on load, not null.
        document.activeElement = document.body || null;
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
          __stopNow: false,
          preventDefault: function () { if (this.cancelable) this.defaultPrevented = true; },
          stopPropagation: function () { this.__stop = true; },
          stopImmediatePropagation: function () { this.__stop = true; this.__stopNow = true; },
        };
        // Copy caller-supplied fields (key, code, button, detail, …) without
        // clobbering the machinery above.
        if (init && typeof init === "object") {
          for (var k in init) {
            if (Object.prototype.hasOwnProperty.call(init, k) && !(k in ev)) ev[k] = init[k];
          }
        }

        // Propagation path: target → ancestor elements → document → window.
        var path = [];
        for (var n = target; n; n = n.__parent) path.push(n);
        if (ev.bubbles) { path.push(document); path.push(window); }

        // Target phase (index 0) then bubbling; a non-bubbling event runs only
        // the target's own listeners.
        var limit = ev.bubbles ? path.length : 1;
        for (var i = 0; i < limit; i++) {
          var cur = path[i];
          var arr = cur.__listeners ? cur.__listeners[type] : null;
          if (arr && arr.length) {
            ev.currentTarget = cur;
            ev.eventPhase = (cur === target) ? 2 : 3; // AT_TARGET / BUBBLING_PHASE
            var copy = arr.slice();
            for (var j = 0; j < copy.length; j++) {
              try { copy[j].call(cur, ev); } catch (e) {}
              if (ev.__stopNow) break;
            }
          }
          if (ev.__stop) break;
        }

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
    // Raw `document.cookie =` write strings awaiting persistence into the sealed
    // jar; the host drains them via __cerberusTakeCookieWrites after each turn.
    // RESET unconditionally on install (unlike the fetch queue above): install_page
    // runs once per navigation, before this page's scripts, so a write the previous
    // origin's scripts made but whose capture was skipped (e.g. a serialize failure
    // left node_to_js empty) is DISCARDED here rather than drained later and
    // misattributed to this new origin's first party. First-party-only, enforced.
    g.__cerberusCookieWrites = [];
    // Script-requested navigations (location.assign/replace/reload, location.href=,
    // window.location=). RESET per install for the same reason as the cookie queue:
    // a navigation the previous page asked for but the host didn't act on must not
    // fire against this new page. The host drains via __cerberusTakeNavigations.
    g.__cerberusNavigations = [];

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

    // URLSearchParams: query-string parsing/building, a near-universal page idiom
    // (reading `?a=1&b=2`, mutating params, `.toString()` for a new URL). Pure
    // JS over an ordered [name, value] list; `+` decodes to space and values are
    // percent-encoded on serialize, per the WHATWG application/x-www-form-urlencoded
    // rules. Returns an object (works with or without `new`, like `Headers`).
    g.URLSearchParams = function (init) {
      var pairs = [];
      var dec = function (x) {
        try { return decodeURIComponent(String(x).replace(/\+/g, " ")); }
        catch (e) { return String(x); }
      };
      var enc = function (x) { return encodeURIComponent(String(x)).replace(/%20/g, "+"); };
      var parse = function (s) {
        s = String(s);
        if (s.charAt(0) === "?") s = s.slice(1);
        if (!s) return;
        var parts = s.split("&");
        for (var i = 0; i < parts.length; i++) {
          if (!parts[i]) continue;
          var eq = parts[i].indexOf("=");
          if (eq === -1) pairs.push([dec(parts[i]), ""]);
          else pairs.push([dec(parts[i].slice(0, eq)), dec(parts[i].slice(eq + 1))]);
        }
      };
      if (init != null) {
        if (typeof init === "string") {
          parse(init);
        } else if (typeof init.length === "number") {
          for (var i = 0; i < init.length; i++) pairs.push([String(init[i][0]), String(init[i][1])]);
        } else if (typeof init === "object") {
          for (var k in init) {
            if (Object.prototype.hasOwnProperty.call(init, k)) pairs.push([k, String(init[k])]);
          }
        }
      }
      var api = {
        get: function (n) { n = String(n); for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) return pairs[i][1]; return null; },
        getAll: function (n) { n = String(n); var out = []; for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) out.push(pairs[i][1]); return out; },
        has: function (n) { n = String(n); for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) return true; return false; },
        append: function (n, v) { pairs.push([String(n), String(v)]); },
        set: function (n, v) {
          // Update the FIRST occurrence in place (preserving its position) and
          // drop any later ones; append if absent — per the WHATWG set() steps.
          n = String(n); v = String(v);
          var first = -1;
          for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === n) { first = i; break; } }
          if (first === -1) { pairs.push([n, v]); return; }
          pairs[first][1] = v;
          for (var j = pairs.length - 1; j > first; j--) { if (pairs[j][0] === n) pairs.splice(j, 1); }
        },
        "delete": function (n) { n = String(n); for (var i = pairs.length - 1; i >= 0; i--) if (pairs[i][0] === n) pairs.splice(i, 1); },
        sort: function () { pairs.sort(function (a, b) { return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0; }); },
        forEach: function (fn, thisArg) { for (var i = 0; i < pairs.length; i++) fn.call(thisArg, pairs[i][1], pairs[i][0], api); },
        keys: function () { return pairs.map(function (p) { return p[0]; }); },
        values: function () { return pairs.map(function (p) { return p[1]; }); },
        entries: function () { return pairs.map(function (p) { return [p[0], p[1]]; }); },
        toString: function () { return pairs.map(function (p) { return enc(p[0]) + "=" + enc(p[1]); }).join("&"); },
      };
      return api;
    };

    // btoa/atob: base64 of a binary (Latin-1) string and back. Common for data
    // URIs, Basic-auth headers, and token blobs. Pure JS since QuickJS ships no
    // base64 global; btoa throws on codepoints > 255 (like a real browser).
    var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    g.btoa = function (input) {
      var s = String(input), out = "", i = 0;
      while (i < s.length) {
        var b1 = s.charCodeAt(i++);
        var b2 = i < s.length ? s.charCodeAt(i++) : NaN;
        var b3 = i < s.length ? s.charCodeAt(i++) : NaN;
        if (b1 > 255 || (b2 === b2 && b2 > 255) || (b3 === b3 && b3 > 255)) {
          throw new g.DOMException("The string to be encoded contains characters outside of the Latin1 range.", "InvalidCharacterError");
        }
        var e1 = b1 >> 2;
        var e2 = ((b1 & 3) << 4) | (b2 === b2 ? b2 >> 4 : 0);
        var e3 = b2 === b2 ? (((b2 & 15) << 2) | (b3 === b3 ? b3 >> 6 : 0)) : 64;
        var e4 = b3 === b3 ? (b3 & 63) : 64;
        out += B64.charAt(e1) + B64.charAt(e2)
             + (e3 === 64 ? "=" : B64.charAt(e3))
             + (e4 === 64 ? "=" : B64.charAt(e4));
      }
      return out;
    };
    g.atob = function (input) {
      var s = String(input).replace(/[ \t\n\f\r]/g, "");
      if (s.length % 4 === 1) throw new g.DOMException("The string to be decoded is not correctly encoded.", "InvalidCharacterError");
      s = s.replace(/=+$/, "");
      var out = "", bits = 0, buffer = 0;
      for (var i = 0; i < s.length; i++) {
        var idx = B64.indexOf(s.charAt(i));
        if (idx === -1) throw new g.DOMException("The string to be decoded is not correctly encoded.", "InvalidCharacterError");
        buffer = (buffer << 6) | idx;
        bits += 6;
        if (bits >= 8) { bits -= 8; out += String.fromCharCode((buffer >> bits) & 0xff); }
      }
      return out;
    };

    // ---- Web Crypto (getRandomValues + a minimal subtle) ----------------
    // Many real sites use crypto for ids/nonces; anti-bot sensors probe it.
    // getRandomValues fills from a per-run counter mixed with the farbling seed
    // if present (xorshift), so it is unpredictable within a run but needs no
    // host entropy. NOTE: not cryptographically strong — a follow-up should back
    // this with real OS entropy via a host binding.
    if (!g.crypto || typeof g.crypto.getRandomValues !== "function") {
      // Seed from the per-head farbling globals if the farbling prologue exposed
      // them (deterministic, but distinct per head); else a fixed constant. The
      // old g.__farbleSeed was never set, so the stream was a constant — the
      // bug this fixes. Guard the seed away from 0 (xorshift sticks at 0).
      var __seedHi = (typeof g.__FARBLE_HI === "number") ? (g.__FARBLE_HI >>> 0) : 0x2545F491;
      var __seedLo = (typeof g.__FARBLE_LO === "number") ? (g.__FARBLE_LO >>> 0) : 0x9e3779b9;
      var __cs = ((__seedHi ^ __seedLo ^ 0x9e3779b9) >>> 0) || 0x2545F491;
      var __crypto = g.crypto || {};
      var __INT_VIEWS = {
        Int8Array: 1, Uint8Array: 1, Uint8ClampedArray: 1, Int16Array: 1,
        Uint16Array: 1, Int32Array: 1, Uint32Array: 1, BigInt64Array: 1, BigUint64Array: 1
      };
      __crypto.getRandomValues = function (a) {
        // WHATWG: only integer typed arrays are allowed; Float*/DataView reject
        // with TypeMismatchError, and > 65536 bytes with QuotaExceededError.
        // Use the [[TypedArrayName]] tag (not constructor.name) so subclasses
        // of an integer view — whose constructor name differs — are accepted.
        var tag = Object.prototype.toString.call(a);
        var kind = (tag.slice(0, 8) === "[object " && tag.charAt(tag.length - 1) === "]")
          ? tag.slice(8, -1) : "";
        if (!a || !(kind in __INT_VIEWS)) {
          throw new g.DOMException("The provided ArrayBufferView is not an integer-typed view", "TypeMismatchError");
        }
        if (a.byteLength > 65536) {
          // Per spec this QuotaExceededError leaves quota/requested null (they
          // are meaningful for storage quotas, not the entropy cap).
          throw new g.QuotaExceededError(
            "The ArrayBufferView's byte length (" + a.byteLength + ") exceeds the number of bytes of entropy available (65536)");
        }
        var big = (kind === "BigInt64Array" || kind === "BigUint64Array");
        for (var i = 0; i < a.length; i++) {
          __cs ^= __cs << 13; __cs ^= __cs >>> 17; __cs ^= __cs << 5; __cs >>>= 0;
          if (big) {
            __cs ^= __cs << 13; __cs ^= __cs >>> 17; __cs ^= __cs << 5; __cs >>>= 0;
            a[i] = BigInt(__cs >>> 0);
          } else {
            a[i] = (a.BYTES_PER_ELEMENT === 1) ? (__cs & 0xff)
                 : (a.BYTES_PER_ELEMENT === 2) ? (__cs & 0xffff) : (__cs >>> 0);
          }
        }
        return a;
      };
      __crypto.randomUUID = function () {
        var b = new Uint8Array(16); __crypto.getRandomValues(b);
        b[6] = (b[6] & 0x0f) | 0x40; b[8] = (b[8] & 0x3f) | 0x80;
        var h = ""; for (var i = 0; i < 16; i++) h += (b[i] + 0x100).toString(16).slice(1);
        return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" + h.slice(16, 20) + "-" + h.slice(20);
      };
      if (!__crypto.subtle) __crypto.subtle = { digest: function () { return Promise.resolve(new ArrayBuffer(32)); } };
      g.crypto = __crypto;
    }

    // ---- Intl (minimal DateTimeFormat/NumberFormat) ---------------------
    // QuickJS ships no Intl; sites and sensors read the resolved timezone/locale.
    if (typeof g.Intl === "undefined") {
      var __tz = "America/New_York", __loc = "en-US";
      g.Intl = {
        DateTimeFormat: function () { return { resolvedOptions: function () {
          return { timeZone: __tz, locale: __loc, calendar: "gregory", numberingSystem: "latn" }; },
          format: function () { return ""; }, formatToParts: function () { return []; } }; },
        NumberFormat: function () { return { resolvedOptions: function () {
          return { locale: __loc, numberingSystem: "latn" }; }, format: function (x) { return String(x); } }; },
        Collator: function () { return { resolvedOptions: function () { return { locale: __loc }; },
          compare: function (a, b) { return a < b ? -1 : a > b ? 1 : 0; } }; },
      };
    }

    // ---- navigator.plugins / mimeTypes (Chrome's built-in PDF set) -------
    // Empty plugins reads as headless; real Chrome exposes 5 PDF plugin entries.
    (function () {
      function plugin(name) {
        var p = { name: name, filename: "internal-pdf-viewer", description: "Portable Document Format", length: 1 };
        p[0] = { type: "application/pdf", suffixes: "pdf", description: "", enabledPlugin: p };
        p.item = function () { return p[0]; }; p.namedItem = function () { return p[0]; };
        return p;
      }
      var list = [plugin("PDF Viewer"), plugin("Chrome PDF Viewer"), plugin("Chromium PDF Viewer"),
                  plugin("Microsoft Edge PDF Viewer"), plugin("WebKit built-in PDF")];
      list.item = function (i) { return list[i] || null; };
      list.namedItem = function (nm) { for (var i = 0; i < list.length; i++) if (list[i].name === nm) return list[i]; return null; };
      list.refresh = function () {};
      var mimes = { length: 2, item: function (i) { return mimes[i] || null; }, namedItem: function () { return null; },
        0: { type: "application/pdf", suffixes: "pdf", description: "", enabledPlugin: list[0] },
        1: { type: "text/pdf", suffixes: "pdf", description: "", enabledPlugin: list[0] } };
      try { Object.defineProperty(navigator, "plugins", { value: list, configurable: true }); } catch (e) {}
      try { Object.defineProperty(navigator, "mimeTypes", { value: mimes, configurable: true }); } catch (e) {}
      try { Object.defineProperty(navigator, "pdfViewerEnabled", { value: true, configurable: true }); } catch (e) {}
    })();

    // ---- window.chrome (present on every real Chrome) --------------------
    if (!g.chrome) {
      g.chrome = {
        app: { isInstalled: false, InstallState: { DISABLED: "disabled", INSTALLED: "installed", NOT_INSTALLED: "not_installed" }, RunningState: { CANNOT_RUN: "cannot_run", READY_TO_RUN: "ready_to_run", RUNNING: "running" } },
        runtime: { connect: function () {}, sendMessage: function () {}, onMessage: { addListener: function () {} } },
        csi: function () { return { startE: 0, onloadT: 0, pageT: 0, tran: 15 }; },
        loadTimes: function () { return { requestTime: 0, startLoadTime: 0, commitLoadTime: 0, finishDocumentLoadTime: 0, finishLoadTime: 0, firstPaintTime: 0, firstPaintAfterLoadTime: 0, navigationType: "Other", wasFetchedViaSpdy: true, wasNpnNegotiated: true, npnNegotiatedProtocol: "h2", wasAlternateProtocolAvailable: false, connectionInfo: "h2" }; },
      };
    }

    // FormData: collect a form's control values, the standard companion to
    // `fetch(url, { method: 'POST', body: new FormData(form) })`. `new
    // FormData(form)` scrapes the form's *successful* controls — named,
    // non-disabled; checkboxes/radios only when checked (defaulting to "on");
    // buttons and file inputs are skipped — mirroring what a real submission
    // sends. Also usable programmatically (append/set/etc.), like URLSearchParams.
    g.FormData = function (form) {
      var pairs = [];
      if (form && form.__tag === "form") {
        var els = form.elements;
        for (var i = 0; i < els.length; i++) {
          var el = els[i];
          var name = getAttr(el, "name");
          if (name === null || name === "") continue;
          if (getAttr(el, "disabled") !== null) continue;
          var tag = el.__tag, type = String(el.type || "").toLowerCase();
          if (tag === "button") {
            continue; // a <button> only submits when it is the submitter
          } else if (tag === "input" && (type === "checkbox" || type === "radio")) {
            if (getAttr(el, "checked") === null) continue;
            pairs.push([name, getAttr(el, "value") !== null ? el.value : "on"]);
          } else if (tag === "input"
              && (type === "submit" || type === "reset" || type === "button"
                  || type === "image" || type === "file")) {
            continue; // not a successful control here (no submitter / no File)
          } else {
            pairs.push([name, el.value != null ? String(el.value) : ""]);
          }
        }
      }
      var api = {
        get: function (n) { n = String(n); for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) return pairs[i][1]; return null; },
        getAll: function (n) { n = String(n); var out = []; for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) out.push(pairs[i][1]); return out; },
        has: function (n) { n = String(n); for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === n) return true; return false; },
        append: function (n, v) { pairs.push([String(n), String(v)]); },
        set: function (n, v) {
          n = String(n); v = String(v);
          var first = -1;
          for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === n) { first = i; break; } }
          if (first === -1) { pairs.push([n, v]); return; }
          pairs[first][1] = v;
          for (var j = pairs.length - 1; j > first; j--) { if (pairs[j][0] === n) pairs.splice(j, 1); }
        },
        "delete": function (n) { n = String(n); for (var i = pairs.length - 1; i >= 0; i--) if (pairs[i][0] === n) pairs.splice(i, 1); },
        forEach: function (fn, thisArg) { for (var i = 0; i < pairs.length; i++) fn.call(thisArg, pairs[i][1], pairs[i][0], api); },
        keys: function () { return pairs.map(function (p) { return p[0]; }); },
        values: function () { return pairs.map(function (p) { return p[1]; }); },
        entries: function () { return pairs.map(function (p) { return [p[0], p[1]]; }); },
      };
      return api;
    };

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
        var method = (init && init.method) ? String(init.method).toUpperCase() : "GET";
        var headers = normalizeHeaders(init);
        var body = (init && init.body != null) ? String(init.body) : "";
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

    // Drain the queued `document.cookie =` write strings as a JSON array, then
    // CLEAR it. The host persists each into the per-instance sealed jar.
    g.__cerberusTakeCookieWrites = function () {
      try {
        var q = g.__cerberusCookieWrites;
        if (!Array.isArray(q) || q.length === 0) return "[]";
        g.__cerberusCookieWrites = [];
        return JSON.stringify(q);
      } catch (e) {
        return "[]";
      }
    };

    // Drain the queued navigations as a JSON array of {url, replace}, then CLEAR
    // it. The host resolves each URL against the document and performs the load.
    g.__cerberusTakeNavigations = function () {
      try {
        var q = g.__cerberusNavigations;
        if (!Array.isArray(q) || q.length === 0) return "[]";
        g.__cerberusNavigations = [];
        return JSON.stringify(q);
      } catch (e) {
        return "[]";
      }
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
        entry.reject(new TypeError(String(message)));
      } catch (e) {}
    };

    // ---- XMLHttpRequest (over the same host fetch queue) ---------------
    // A minimal but real XHR: open / setRequestHeader / send, plus status /
    // responseText / readyState and the load / error / readystatechange events
    // (both `on*` handlers and addEventListener). It enqueues onto the *same*
    // __cerberusFetchQueue as fetch() and settles through the *same*
    // __cerberusResolveFetch / __cerberusRejectFetch path — registering its own
    // resolve/reject under the request id — so it inherits the per-instance
    // sealed cookie jar and the consent gate for free, with no host changes.
    // Bodies and responses are text (v1). Synchronous mode (`async === false`)
    // cannot truly block here (the host services the queue only after the
    // script yields), so it is treated as asynchronous; a script that relies on
    // a blocking read is out of scope. This is the primitive bot-challenge
    // sensors (e.g. reese84/Imperva) use to POST their payload — see
    // docs/ideas/reese84-bot-challenge.md.
    function XMLHttpRequest() {
      this.readyState = 0; // UNSENT
      this.status = 0;
      this.statusText = "";
      this.responseText = "";
      this.response = "";
      this.responseType = "";
      this.responseURL = "";
      this.withCredentials = false; // cookies always ride the sealed jar
      this.timeout = 0;
      this.onreadystatechange = null;
      this.onload = null;
      this.onerror = null;
      this.onloadend = null;
      this.__method = "GET";
      this.__url = "";
      this.__reqHeaders = [];
      this.__respHeaders = [];
      this.__listeners = { load: [], error: [], readystatechange: [], loadend: [] };
      this.__sent = false;
      this.__aborted = false;
    }
    XMLHttpRequest.UNSENT = 0;
    XMLHttpRequest.OPENED = 1;
    XMLHttpRequest.HEADERS_RECEIVED = 2;
    XMLHttpRequest.LOADING = 3;
    XMLHttpRequest.DONE = 4;
    XMLHttpRequest.prototype.open = function (method, url) {
      this.__method = String(method == null ? "GET" : method).toUpperCase();
      this.__url = String(url == null ? "" : url);
      this.__reqHeaders = [];
      this.__sent = false;
      this.__aborted = false;
      this.readyState = 1; // OPENED
      this.__fireReadyState();
    };
    XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
      if (this.readyState !== 1) return;
      this.__reqHeaders.push([String(name), String(value)]);
    };
    XMLHttpRequest.prototype.getResponseHeader = function (name) {
      var lc = String(name).toLowerCase();
      for (var i = 0; i < this.__respHeaders.length; i++) {
        if (String(this.__respHeaders[i][0]).toLowerCase() === lc) return this.__respHeaders[i][1];
      }
      return null;
    };
    XMLHttpRequest.prototype.getAllResponseHeaders = function () {
      var out = "";
      for (var i = 0; i < this.__respHeaders.length; i++) {
        out += this.__respHeaders[i][0] + ": " + this.__respHeaders[i][1] + "\r\n";
      }
      return out;
    };
    XMLHttpRequest.prototype.addEventListener = function (type, fn) {
      if (this.__listeners[type] && typeof fn === "function") this.__listeners[type].push(fn);
    };
    XMLHttpRequest.prototype.removeEventListener = function (type, fn) {
      var l = this.__listeners[type];
      if (!l) return;
      for (var i = 0; i < l.length; i++) if (l[i] === fn) { l.splice(i, 1); break; }
    };
    XMLHttpRequest.prototype.__fire = function (type) {
      var ev = { type: type, target: this, currentTarget: this };
      var h = this["on" + type];
      try { if (typeof h === "function") h.call(this, ev); } catch (e) {}
      var l = this.__listeners[type];
      if (l) { var snap = l.slice(); for (var i = 0; i < snap.length; i++) { try { snap[i].call(this, ev); } catch (e) {} } }
    };
    XMLHttpRequest.prototype.__fireReadyState = function () {
      this.__fire("readystatechange");
    };
    XMLHttpRequest.prototype.send = function (body) {
      if (this.__sent || this.readyState !== 1) return;
      this.__sent = true;
      var self = this;
      var id = g.__cerberusFetchId++;
      g.__cerberusFetchPending[id] = {
        resolve: function (response) {
          if (self.__aborted) return;
          self.status = (response && typeof response.status === "number") ? response.status : 0;
          self.statusText = (response && response.statusText) ? response.statusText : "";
          self.responseText = (response && response._bodyText != null) ? response._bodyText : "";
          self.response = self.responseText;
          self.responseURL = (response && response.url) ? response.url : self.__url;
          self.__respHeaders = (response && response.headers && response.headers.__pairs)
            ? response.headers.__pairs() : [];
          self.readyState = 4; // DONE
          self.__fireReadyState();
          self.__fire("load");
          self.__fire("loadend");
        },
        reject: function (_message) {
          if (self.__aborted) return;
          self.status = 0;
          self.responseText = "";
          self.readyState = 4; // DONE (with an error)
          self.__fireReadyState();
          self.__fire("error");
          self.__fire("loadend");
        },
      };
      g.__cerberusFetchQueue.push({
        id: id,
        url: self.__url,
        method: self.__method,
        headers: self.__reqHeaders,
        body: (body != null ? String(body) : ""),
      });
    };
    XMLHttpRequest.prototype.abort = function () {
      this.__aborted = true;
      this.readyState = 0;
      this.status = 0;
    };
    g.XMLHttpRequest = XMLHttpRequest;

    // ---- Blob + object URLs + Worker + Image + WebSocket ----------------
    // Speed-first, single-thread shims. Modern sites (and anti-bot sensors)
    // routinely offload work to a Blob-backed Worker, or beacon telemetry via
    // `new Image().src` / `navigator.sendBeacon`. A real Worker runs off-thread;
    // we run its code synchronously in-realm and route `postMessage` both ways
    // through the existing timer queue. Network from a Worker or a beacon reuses
    // the page's fetch queue + sealed jar, so it stays first-party and consented.
    (function () {
      // Fire-and-forget request onto the shared fetch queue (beacons: no caller
      // awaits the response). Reuses the same drain + sealed jar as fetch/XHR.
      function beacon(url, method, body) {
        try {
          var id = g.__cerberusFetchId++;
          g.__cerberusFetchQueue.push({
            id: id, url: String(url), method: method || "GET",
            headers: [], body: body != null ? String(body) : ""
          });
          g.__cerberusFetchPending[id] = { resolve: function () {}, reject: function () {} };
          return id;
        } catch (e) { return -1; }
      }
      g.__cerberusBeacon = beacon;

      if (typeof g.Blob !== "function") {
        g.Blob = function (parts, opts) {
          this.__parts = [];
          if (parts && parts.length) for (var i = 0; i < parts.length; i++) this.__parts.push(String(parts[i]));
          this.type = (opts && opts.type) ? String(opts.type) : "";
          var n = 0; for (var j = 0; j < this.__parts.length; j++) n += this.__parts[j].length;
          this.size = n;
        };
        g.Blob.prototype.slice = function () { return new g.Blob(this.__parts, { type: this.type }); };
        g.Blob.prototype.text = function () { return Promise.resolve(this.__parts.join("")); };
        g.Blob.prototype.arrayBuffer = function () {
          var s = this.__parts.join(""), buf = new ArrayBuffer(s.length), v = new Uint8Array(buf);
          for (var i = 0; i < s.length; i++) v[i] = s.charCodeAt(i) & 0xff;
          return Promise.resolve(buf);
        };
      }

      // object URLs: map blob: URLs back to their Blob so Worker() reads source.
      var __blobs = Object.create(null), __blobSeq = 1;
      if (!g.URL) g.URL = {};
      if (typeof g.URL.createObjectURL !== "function") {
        g.URL.createObjectURL = function (blob) {
          var origin = (g.location && g.location.origin) || "null";
          var id = "blob:" + origin + "/cerberus-" + (__blobSeq++);
          __blobs[id] = blob;
          return id;
        };
        g.URL.revokeObjectURL = function (id) { delete __blobs[String(id)]; };
      }
      function blobSource(u) {
        var b = __blobs[String(u)];
        return b && b.__parts ? b.__parts.join("") : null;
      }

      if (typeof g.Worker !== "function") {
        g.Worker = function (scriptUrl) {
          var outer = this;
          outer.onmessage = null; outer.onerror = null; outer.onmessageerror = null;
          outer.__ls = { message: [], error: [] };
          outer.addEventListener = function (t, fn) { if (outer.__ls[t]) outer.__ls[t].push(fn); };
          outer.removeEventListener = function (t, fn) { var a = outer.__ls[t]; if (a) { var i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); } };
          outer.terminate = function () { outer.__dead = true; };

          var scope = { onmessage: null, name: "", __ls: { message: [], error: [] } };
          scope.self = scope;
          // Give the scope a WorkerGlobalScope/DedicatedWorkerGlobalScope identity
          // so code that branches on `self instanceof DedicatedWorkerGlobalScope`
          // (e.g. the WPT testharness, and real libraries feature-detecting the
          // worker context) takes the worker path.
          scope.WorkerGlobalScope = function WorkerGlobalScope() {};
          scope.DedicatedWorkerGlobalScope = function DedicatedWorkerGlobalScope() {};
          scope.DedicatedWorkerGlobalScope.prototype = Object.create(scope.WorkerGlobalScope.prototype);
          try { Object.setPrototypeOf(scope, scope.DedicatedWorkerGlobalScope.prototype); } catch (e) {}
          scope.location = g.location; scope.navigator = g.navigator;
          scope.setTimeout = g.setTimeout; scope.clearTimeout = g.clearTimeout;
          scope.setInterval = g.setInterval; scope.clearInterval = g.clearInterval;
          scope.queueMicrotask = g.queueMicrotask;
          scope.fetch = g.fetch; scope.XMLHttpRequest = g.XMLHttpRequest;
          scope.crypto = g.crypto; scope.atob = g.atob; scope.btoa = g.btoa;
          scope.TextEncoder = g.TextEncoder; scope.TextDecoder = g.TextDecoder;
          scope.performance = g.performance; scope.Blob = g.Blob; scope.URL = g.URL;
          // Web IDL constructors a worker exposes on `self` — code (and the WPT
          // testharness) reaches them via `self.X`, and they must be the SAME
          // objects the real global throws/constructs so `instanceof` holds.
          scope.DOMException = g.DOMException; scope.QuotaExceededError = g.QuotaExceededError;
          scope.Event = g.Event; scope.CustomEvent = g.CustomEvent; scope.EventTarget = g.EventTarget;
          scope.addEventListener = function (t, fn) { if (scope.__ls[t]) scope.__ls[t].push(fn); };
          scope.removeEventListener = function (t, fn) { var a = scope.__ls[t]; if (a) { var i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); } };
          scope.close = function () { scope.__dead = true; };
          // Run `source` in the worker scope: bare globals resolve to `self`
          // (via `with (this)`), which is the invariant real worker code and
          // test harnesses rely on (they assign their API onto `self`).
          function runInScope(source) {
            var f = new Function(
              "self", "postMessage", "importScripts", "addEventListener", "removeEventListener",
              "close", "location", "navigator", "setTimeout", "clearTimeout", "setInterval",
              "clearInterval", "queueMicrotask", "fetch", "XMLHttpRequest", "crypto", "atob",
              "btoa", "TextEncoder", "TextDecoder", "performance", "Blob", "URL",
              "with (this) {\n" + source + "\n}");
            return f.call(scope, scope, scope.postMessage, scope.importScripts, scope.addEventListener,
              scope.removeEventListener, scope.close, scope.location, scope.navigator, scope.setTimeout,
              scope.clearTimeout, scope.setInterval, scope.clearInterval, scope.queueMicrotask, scope.fetch,
              scope.XMLHttpRequest, scope.crypto, scope.atob, scope.btoa, scope.TextEncoder, scope.TextDecoder,
              scope.performance, scope.Blob, scope.URL);
          }
          // importScripts: synchronous script loading. Real workers hit the
          // network here; our arch avoids sync network, so resolve from a
          // prefetched cache (host- or test-populated `__cerberusScriptCache`).
          // Unknown scripts are recorded for diagnostics and skipped.
          scope.importScripts = function () {
            for (var i = 0; i < arguments.length; i++) {
              var u = String(arguments[i]);
              (g.__cerberusWorkerImports = g.__cerberusWorkerImports || []).push(u);
              var cache = g.__cerberusScriptCache;
              var s = cache ? (cache[u] || cache[u.replace(/^.*\/\/[^/]+/, "")]) : null;
              if (s != null) runInScope(s);
            }
          };
          function deliver(target, ls, data) {
            if (target && target.__dead) return;
            g.setTimeout(function () {
              var ev = { data: data, type: "message" };
              var h = target.onmessage; if (typeof h === "function") { try { h.call(target, ev); } catch (e) {} }
              ls.message.forEach(function (fn) { try { fn.call(target, ev); } catch (e) {} });
            }, 0);
          }
          scope.postMessage = function (data) { deliver(outer, outer.__ls, data); };  // worker -> main
          outer.postMessage = function (data) { deliver(scope, scope.__ls, data); };   // main -> worker

          var src = blobSource(scriptUrl);
          if (src == null) {
            // Not a blob URL — try the prefetched script cache (real workers are
            // often `new Worker('/path.js')`), else record and give up.
            var c = g.__cerberusScriptCache;
            src = c ? (c[String(scriptUrl)] || c[String(scriptUrl).replace(/^.*\/\/[^/]+/, "")]) : null;
            if (src == null) { (g.__cerberusWorkerScripts = g.__cerberusWorkerScripts || []).push(String(scriptUrl)); return; }
          }
          try {
            runInScope(src);
          } catch (e) {
            g.setTimeout(function () {
              var ev = { type: "error", message: String(e), error: e };
              if (typeof outer.onerror === "function") { try { outer.onerror(ev); } catch (_) {} }
              outer.__ls.error.forEach(function (fn) { try { fn(ev); } catch (_) {} });
            }, 0);
          }
        };
      }

      if (typeof g.Image !== "function") {
        g.Image = function (w, h) {
          var self = this;
          self.width = w || 0; self.height = h || 0;
          self.naturalWidth = 0; self.naturalHeight = 0;
          self.complete = false; self.onload = null; self.onerror = null;
          var _src = "";
          Object.defineProperty(self, "src", {
            configurable: true, enumerable: true,
            get: function () { return _src; },
            set: function (v) {
              _src = String(v);
              // An <img> load is a GET; route non-data beacons like the network
              // does so sensor pixel beacons actually go out and are observable.
              if (_src && _src.indexOf("data:") !== 0) g.__cerberusBeacon(_src, "GET", null);
              self.complete = true;
              g.setTimeout(function () { if (typeof self.onload === "function") { try { self.onload({ type: "load" }); } catch (e) {} } }, 0);
            }
          });
        };
      }

      if (typeof g.WebSocket !== "function") {
        g.WebSocket = function (url, protocols) {
          var self = this;
          self.url = String(url); self.readyState = 0; // CONNECTING
          self.onopen = null; self.onmessage = null; self.onerror = null; self.onclose = null;
          self.protocol = protocols ? String([].concat(protocols)[0]) : "";
          self.bufferedAmount = 0; self.extensions = "";
          self.send = function () {};
          self.addEventListener = function () {};
          self.removeEventListener = function () {};
          self.close = function () { self.readyState = 3; if (typeof self.onclose === "function") { try { self.onclose({ type: "close", code: 1000, wasClean: true }); } catch (e) {} } };
          // Report open so feature-detection passes; no messages arrive (a live
          // socket to the sensor is out of scope for this speed-first shim).
          g.setTimeout(function () { self.readyState = 1; if (typeof self.onopen === "function") { try { self.onopen({ type: "open" }); } catch (e) {} } }, 0);
        };
        g.WebSocket.CONNECTING = 0; g.WebSocket.OPEN = 1; g.WebSocket.CLOSING = 2; g.WebSocket.CLOSED = 3;
      }
    })();

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

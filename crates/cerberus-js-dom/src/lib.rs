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
use cerberus_types::RealmId;
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

/// Parse a JSON wire document and rebuild it into a fresh immutable
/// [`Document`].
///
/// The wire ids are arbitrary; we renumber to fresh [`DocumentBuilder`] ids. The
/// builder requires children to exist before their parent, so we perform a
/// **post-order** traversal of the id graph starting at `root`: a node is
/// materialized only after all of its `children` ids have been, and each wire id
/// is mapped to the [`NodeId`] the builder hands back. Cycles and dangling
/// `children` ids are rejected as [`BridgeError::Structure`].
pub fn rebuild_document(json: &str) -> Result<Document, BridgeError> {
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
    Ok(builder.finish(root_fresh))
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
/// All work goes through `engine.eval(realm, …)`:
///
/// 1. Inject `env` as a global, then install [`DOM_MODEL_PRELUDE`] (which reads
///    that global to build `location`/`navigator`/`screen`/storage and defines
///    `document`, `window`, helpers).
/// 2. Hand the model a snapshot of `document` and call `__cerberusInstallDOM()`.
/// 3. Evaluate each script in `scripts`, in order. **A script that throws does
///    not abort the run** — browsers continue to the next `<script>`, so we
///    swallow [`JsError::Eval`] and keep going. Any *other* engine error
///    (e.g. [`JsError::NoSuchRealm`]) is infrastructure-level and propagates.
/// 4. Fire `load`/`DOMContentLoaded` via `__cerberusFireLoad()` (page-listener
///    errors are likewise swallowed by the model).
/// 5. Serialize the mutated tree and [`rebuild_document`] it.
///
/// Steps 1, 2 and the final serialize are bridge infrastructure: an engine error
/// there is fatal and returned as [`BridgeError::Js`].
pub fn run_page_scripts(
    engine: &mut dyn JsEngine,
    realm: RealmId,
    document: &Document,
    scripts: &[String],
    env: &PageEnv,
) -> Result<Document, BridgeError> {
    // 1. Inject the ambient environment, then install the document model. The
    //    env global is read by the prelude as it builds `location`/`navigator`/
    //    `screen`; it must land *before* the model installs. The prelude is
    //    self-guarding, but a genuine engine/compile failure is fatal.
    let env_install = format!(
        "globalThis.__CERBERUS_ENV__ = {{ url: {}, width: {}, height: {} }};",
        js_string(&env.url),
        env.viewport.0,
        env.viewport.1,
    );
    engine.eval(realm, &env_install)?;
    engine.eval(realm, DOM_MODEL_PRELUDE)?;

    // 2. Hand it the snapshot and build the JS tree.
    let install = format!(
        "globalThis.__CERBERUS_DOM__ = {}; __cerberusInstallDOM();",
        serialize_document(document)
    );
    engine.eval(realm, &install)?;

    // 3. Run each page script. A throw is page-level (not infrastructure): the
    //    browser keeps going to the next <script>, so we swallow `Eval` errors.
    //    A realm-level error (no such realm, etc.) is infrastructure and aborts.
    for script in scripts {
        match engine.eval(realm, script) {
            Ok(_) | Err(JsError::Eval(_)) => {}
            Err(other) => return Err(BridgeError::Js(other)),
        }
    }

    // 4. Fire DOMContentLoaded + load. The model swallows listener errors itself;
    //    a realm-level error is still fatal.
    match engine.eval(realm, "__cerberusFireLoad();") {
        Ok(_) | Err(JsError::Eval(_)) => {}
        Err(other) => return Err(BridgeError::Js(other)),
    }

    // 5. Serialize the mutated tree back out and rebuild a fresh Rust Document.
    let out = engine.eval(realm, "__cerberusSerializeDOM()")?;
    match out {
        JsValue::Str(s) => rebuild_document(&s),
        other => Err(BridgeError::Structure(format!(
            "__cerberusSerializeDOM did not return a string: {other:?}"
        ))),
    }
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
    // Unsupported (by design, speed-first): sibling combinators `~`/`+`,
    // pseudo-classes/elements (`:hover`, `::before`), leading `>`, namespaces.
    // Attribute selectors are limited to `[name]` and `[name="value"]` /
    // `[name='value']` (presence and exact-match; no `~=`, `^=`, `*=`, …).
    function parseCompound(text) {
      // text is one compound run with no whitespace/combinators, e.g.
      // `div.foo#bar[data-x="1"]`. Returns null if it is empty/garbage.
      var compound = { tag: null, id: null, classes: [], attrs: [] };
      var i = 0, n = text.length, sawAny = false;
      while (i < n) {
        var ch = text.charAt(i);
        if (ch === "#") {
          i++; var s = i; while (i < n && !".#[".includes(text.charAt(i))) i++;
          compound.id = text.slice(s, i); sawAny = true;
        } else if (ch === ".") {
          i++; var s2 = i; while (i < n && !".#[".includes(text.charAt(i))) i++;
          if (i > s2) { compound.classes.push(text.slice(s2, i)); sawAny = true; }
        } else if (ch === "[") {
          var end = text.indexOf("]", i);
          if (end === -1) return null;            // unterminated → no match
          var body = text.slice(i + 1, end).trim();
          i = end + 1;
          var eq = body.indexOf("=");
          if (eq === -1) {
            if (body) { compound.attrs.push({ name: body, value: null }); sawAny = true; }
          } else {
            var an = body.slice(0, eq).trim();
            var av = body.slice(eq + 1).trim();
            if (av.length >= 2 && (av.charAt(0) === '"' || av.charAt(0) === "'")) av = av.slice(1, -1);
            if (an) { compound.attrs.push({ name: an, value: av }); sawAny = true; }
          }
        } else {
          // A type (tag) selector or universal `*`; runs until the next part.
          var s3 = i; while (i < n && !".#[".includes(text.charAt(i))) i++;
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
        if (text.charAt(i) === ">") {
          pendingCombinator = ">"; i++;
          // Skip whitespace after `>`.
          while (i < n && /\s/.test(text.charAt(i))) i++;
        } else if (sawSpace && steps.length > 0) {
          pendingCombinator = " ";
        }
        // Read the compound run up to the next combinator/whitespace.
        var s = i;
        while (i < n && !/\s/.test(text.charAt(i)) && text.charAt(i) !== ">") {
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
        if (v === null) return false;
        if (a.value !== null && v !== a.value) return false;
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
      return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
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
        // Minimal in-memory cookie jar: keep the first "name=value" pair,
        // appending/replacing by name. Attributes (path, expires…) are ignored.
        var raw = String(v);
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
    var envUrl = (typeof env.url === "string") ? env.url : "about:blank";
    var vpW = (typeof env.width === "number") ? env.width : 0;
    var vpH = (typeof env.height === "number") ? env.height : 0;

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
    // DELIBERATELY MINIMAL and GENERIC / low-entropy — every head looks the
    // same. Per-head fingerprint farbling is M6 (ADR-0002 farbling prologue),
    // not here; we do NOT expose plugins, mediaDevices, webgl, etc.
    g.navigator = {
      userAgent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Cerberus/1.0 Safari/537.36",
      appName: "Netscape",
      appVersion: "5.0",
      product: "Gecko",
      vendor: "",
      language: "en-US",
      languages: ["en-US"],
      platform: "",
      hardwareConcurrency: 4,
      onLine: true,
      cookieEnabled: true,
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

    // ---- matchMedia (never matches — we don't honor media queries) -----
    window.matchMedia = function (q) {
      return {
        matches: false, media: String(q), onchange: null,
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

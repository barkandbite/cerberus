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
//! # Implemented vs deferred DOM surface
//!
//! This is "minimal but real": enough of `document`, element/text nodes,
//! `window`, and `console` to run typical page bootstraps and reconcile their
//! structural mutations. Selectors are a single simple selector only (`#id`,
//! `.class`, or `tag` — no combinators). Layout APIs are stubbed
//! (`getBoundingClientRect` is all-zero), `style` is store-only, and richer
//! `window` surfaces (`navigator`, `location`, `localStorage`,
//! `getComputedStyle`, `matchMedia`) are intentionally left to a follow-up. See
//! the [`DOM_MODEL_PRELUDE`] docs for the precise list.

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
                } => {
                    let child_ids: Vec<NodeId> = children
                        .iter()
                        .map(|c| {
                            fresh.get(c).copied().ok_or_else(|| {
                                BridgeError::Structure(format!(
                                    "child id {c} not materialized before parent {id}"
                                ))
                            })
                        })
                        .collect::<Result<_, _>>()?;
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

            Ok((
                id,
                WireNode::Element {
                    tag,
                    attrs,
                    children,
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

/// Run page `<script>`s against a JS document model snapshotted from `document`,
/// and return a fresh Rust [`Document`] reflecting their mutations.
///
/// All work goes through `engine.eval(realm, …)`:
///
/// 1. Install [`DOM_MODEL_PRELUDE`] (defines `document`, `window`, helpers).
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
) -> Result<Document, BridgeError> {
    // 1. Install the document model. The prelude is self-guarding, but a genuine
    //    engine/compile failure still surfaces here and is fatal.
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
/// * **`document`**: `getElementById`, `querySelector`/`querySelectorAll`
///   (single simple selector — `#id`, `.class`, or `tag`; **no combinators**),
///   `getElementsByTagName`, `getElementsByClassName`, `createElement`,
///   `createTextNode`, `body`/`head`/`documentElement`, `title` (get/set),
///   `addEventListener`/`removeEventListener`, `readyState`
///   (`"loading"` → `"complete"`), `cookie` (in-memory get/set).
/// * **element / text nodes**: `nodeType`, `nodeName`/`tagName`, `textContent`
///   (get concatenates descendant text; set replaces children with one text
///   node), `getAttribute`/`setAttribute`/`removeAttribute`/`hasAttribute`/
///   `getAttributeNames`, `id`, `className`, `classList`
///   (`add`/`remove`/`toggle`/`contains`/`length`), `children`/`childNodes`,
///   `parentNode`/`parentElement`, `firstChild`/`lastChild`/`nextSibling`/
///   `previousSibling`, `appendChild`/`removeChild`/`insertBefore`/`remove`, a
///   store-only `style`, `getBoundingClientRect` (all-zero), scoped
///   `querySelector`/`querySelectorAll`.
/// * **`window`** = `globalThis`, with `window.document` and
///   `addEventListener`/`removeEventListener` (load events fired by fire-load).
/// * **`console`**: `log`/`warn`/`error`/`info`/`debug` push joined `String(arg)`
///   messages into `globalThis.__cerberusConsole`; never throw.
///
/// # Deferred (follow-up)
///
/// `navigator`, `location`, `localStorage`/`sessionStorage`, `getComputedStyle`,
/// `matchMedia`; CSS-aware `style` rendering; complex/compound selectors; live
/// collection objects (we expose plain arrays). The structure is kept clean so
/// these slot in without reshaping the model.
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
    function appendChild(parent, node) {
      detach(node);
      parent.__kids.push(node);
      node.__parent = parent;
      return node;
    }
    function insertBefore(parent, node, ref) {
      if (ref == null) return appendChild(parent, node);
      detach(node);
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

    // ---- selector matching (single simple selector only) ---------------
    function matchesSimple(el, sel) {
      if (el.__type !== ELEMENT_NODE) return false;
      if (sel.charAt(0) === "#") return getAttr(el, "id") === sel.slice(1);
      if (sel.charAt(0) === ".") return classTokens(el).indexOf(sel.slice(1)) !== -1;
      return el.__tag.toLowerCase() === sel.toLowerCase();
    }
    function walkElements(root, fn) {
      // Pre-order over elements, excluding `root` itself unless caller adds it.
      var kids = root.__kids;
      for (var i = 0; i < kids.length; i++) {
        var c = kids[i];
        if (c.__type === ELEMENT_NODE) { fn(c); walkElements(c, fn); }
      }
    }
    function queryAll(root, sel) {
      sel = String(sel).trim();
      var out = [];
      if (!sel) return out;
      walkElements(root, function (el) { if (matchesSimple(el, sel)) out.push(el); });
      return out;
    }
    function queryOne(root, sel) {
      var all = queryAll(root, sel);
      return all.length ? all[0] : null;
    }

    // ---- node prototype (shared accessors via defineProperty) ----------
    function defineNodeAccessors(node) {
      Object.defineProperty(node, "nodeType", { get: function () { return this.__type; }, enumerable: false, configurable: true });

      Object.defineProperty(node, "parentNode", { get: function () { return this.__parent; }, enumerable: false, configurable: true });
      Object.defineProperty(node, "parentElement", {
        get: function () { return this.__parent && this.__parent.__type === ELEMENT_NODE ? this.__parent : null; },
        enumerable: false, configurable: true,
      });
      Object.defineProperty(node, "childNodes", { get: function () { return this.__kids.slice(); }, enumerable: false, configurable: true });
      Object.defineProperty(node, "firstChild", { get: function () { return this.__kids[0] || null; }, enumerable: false, configurable: true });
      Object.defineProperty(node, "lastChild", { get: function () { return this.__kids[this.__kids.length - 1] || null; }, enumerable: false, configurable: true });
      Object.defineProperty(node, "nextSibling", {
        get: function () {
          var p = this.__parent; if (!p) return null;
          var i = p.__kids.indexOf(this); return (i === -1) ? null : (p.__kids[i + 1] || null);
        }, enumerable: false, configurable: true,
      });
      Object.defineProperty(node, "previousSibling", {
        get: function () {
          var p = this.__parent; if (!p) return null;
          var i = p.__kids.indexOf(this); return (i <= 0) ? null : (p.__kids[i - 1] || null);
        }, enumerable: false, configurable: true,
      });

      Object.defineProperty(node, "textContent", {
        get: function () { var acc = []; collectText(this, acc); return acc.join(""); },
        set: function (value) {
          if (this.__type === TEXT_NODE) { this.__text = String(value); return; }
          for (var i = 0; i < this.__kids.length; i++) this.__kids[i].__parent = null;
          this.__kids = [];
          var t = makeText(String(value));
          appendChild(this, t);
        },
        enumerable: false, configurable: true,
      });

      node.appendChild = function (child) { return appendChild(this, child); };
      node.removeChild = function (child) { return removeChild(this, child); };
      node.insertBefore = function (child, ref) { return insertBefore(this, child, ref); };
      node.remove = function () { detach(this); };
      node.contains = function (other) {
        for (var n = other; n; n = n.__parent) if (n === this) return true;
        return false;
      };
      node.hasChildNodes = function () { return this.__kids.length > 0; };
    }

    function defineElementAccessors(el) {
      Object.defineProperty(el, "tagName", { get: function () { return this.__tag.toUpperCase(); }, enumerable: false, configurable: true });
      Object.defineProperty(el, "nodeName", { get: function () { return this.__tag.toUpperCase(); }, enumerable: false, configurable: true });
      Object.defineProperty(el, "children", {
        get: function () { return elementChildren(this); }, enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "firstElementChild", {
        get: function () { var c = elementChildren(this); return c[0] || null; }, enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "lastElementChild", {
        get: function () { var c = elementChildren(this); return c[c.length - 1] || null; }, enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "id", {
        get: function () { return getAttr(this, "id") || ""; },
        set: function (v) { setAttr(this, "id", v); },
        enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "className", {
        get: function () { return getAttr(this, "class") || ""; },
        set: function (v) { setAttr(this, "class", v); },
        enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "classList", {
        get: function () { if (!this.__classList) this.__classList = makeClassList(this); return this.__classList; },
        enumerable: false, configurable: true,
      });
      Object.defineProperty(el, "innerText", {
        get: function () { var acc = []; collectText(this, acc); return acc.join(""); },
        set: function (v) { this.textContent = v; },
        enumerable: false, configurable: true,
      });

      el.getAttribute = function (n) { return getAttr(this, String(n)); };
      el.setAttribute = function (n, v) { setAttr(this, String(n), v); };
      el.removeAttribute = function (n) { removeAttr(this, String(n)); };
      el.hasAttribute = function (n) { return attrIndex(this, String(n)) !== -1; };
      el.getAttributeNames = function () { return this.__attrs.map(function (p) { return p[0]; }); };

      el.getElementsByTagName = function (t) { return queryAll(this, String(t)); };
      el.getElementsByClassName = function (c) { return queryAll(this, "." + String(c)); };
      el.querySelector = function (s) { return queryOne(this, s); };
      el.querySelectorAll = function (s) { return queryAll(this, s); };
      el.matches = function (s) { return matchesSimple(this, String(s).trim()); };
      el.closest = function (s) {
        s = String(s).trim();
        for (var n = this; n && n.__type === ELEMENT_NODE; n = n.__parent) if (matchesSimple(n, s)) return n;
        return null;
      };

      el.getBoundingClientRect = function () {
        return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
      };

      // style: store-only. Assignments are remembered and reflected back into
      // the `style` attribute so a round-trip preserves them, but nothing is
      // rendered from them yet.
      var styleStore = Object.create(null);
      var styleObj = new Proxy(styleStore, {
        get: function (t, k) {
          if (k === "setProperty") return function (p, v) { t[p] = String(v); syncStyleAttr(); };
          if (k === "removeProperty") return function (p) { var old = t[p]; delete t[p]; syncStyleAttr(); return old; };
          if (k === "getPropertyValue") return function (p) { return t[p] || ""; };
          if (k === "cssText") return styleCssText();
          return (k in t) ? t[k] : "";
        },
        set: function (t, k, v) {
          if (k === "cssText") { for (var kk in t) delete t[kk]; parseCssText(String(v), t); syncStyleAttr(); return true; }
          t[k] = String(v); syncStyleAttr(); return true;
        },
      });
      function styleCssText() {
        var parts = [];
        for (var k in styleStore) parts.push(k + ": " + styleStore[k]);
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
      function syncStyleAttr() {
        var text = styleCssText();
        if (text) setAttr(el, "style", text); else removeAttr(el, "style");
      }
      Object.defineProperty(el, "style", { get: function () { return styleObj; }, enumerable: false, configurable: true });
      // If the snapshot carried a style attribute, seed the store from it.
      var initialStyle = getAttr(el, "style");
      if (initialStyle) parseCssText(initialStyle, styleStore);

      // Inert event listener registry on elements (dispatch not yet driven by
      // the bridge beyond DOMContentLoaded/load on document+window).
      el.__listeners = el.__listeners || Object.create(null);
      el.addEventListener = function (type, fn) {
        type = String(type); if (!this.__listeners[type]) this.__listeners[type] = [];
        if (typeof fn === "function") this.__listeners[type].push(fn);
      };
      el.removeEventListener = function (type, fn) {
        type = String(type); var arr = this.__listeners[type]; if (!arr) return;
        var i = arr.indexOf(fn); if (i !== -1) arr.splice(i, 1);
      };
      el.dispatchEvent = function (ev) {
        var arr = this.__listeners[ev && ev.type]; if (!arr) return true;
        for (var i = 0; i < arr.slice().length; i++) { try { arr[i].call(this, ev); } catch (e) {} }
        return true;
      };
    }

    // ---- node constructors ---------------------------------------------
    function makeElement(tag, id) {
      var el = {
        __type: ELEMENT_NODE,
        __tag: String(tag).toLowerCase(),
        __attrs: [],
        __kids: [],
        __parent: null,
        __id: (typeof id === "number") ? id : freshId(),
      };
      defineNodeAccessors(el);
      defineElementAccessors(el);
      indexNode(el);
      return el;
    }
    function makeText(text, id) {
      var t = {
        __type: TEXT_NODE,
        __text: String(text),
        __kids: [],
        __parent: null,
        __id: (typeof id === "number") ? id : freshId(),
      };
      defineNodeAccessors(t);
      Object.defineProperty(t, "nodeName", { get: function () { return "#text"; }, enumerable: false, configurable: true });
      Object.defineProperty(t, "data", {
        get: function () { return this.__text; }, set: function (v) { this.__text = String(v); },
        enumerable: false, configurable: true,
      });
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
          var childIds = [];
          for (var i = 0; i < node.__kids.length; i++) childIds.push(emit(node.__kids[i]));
          var attrs = [];
          for (var a = 0; a < node.__attrs.length; a++) attrs.push([node.__attrs[a][0], node.__attrs[a][1]]);
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
}

//! Resolving a [`Target`] against a concrete [`Document`], and the inverse
//! (describing a node as the most stable [`Target`]).
//!
//! Targets are matched per-document so the same action lands correctly on
//! followers whose DOM differs from the master's. Resolution returns a
//! [`NodeId`]; the group then maps it to the live JS-model id via the inverted
//! id-map ([`invert_id_map`]) to dispatch an event at the right realm node.

use std::collections::HashMap;

use cerberus_dom::{Document, NodeId, NodeRef};

use crate::action::Target;

/// Resolve `target` to a node in `doc`, or `None` if it does not match — which
/// the caller treats as divergence, not an error.
pub fn resolve(doc: &Document, target: &Target) -> Option<NodeId> {
    match target {
        Target::Id(id) => find_matching(doc.root(), &|n: NodeRef<'_>| {
            n.attr("id") == Some(id.as_str())
        }),
        Target::Text { tag, text } => find_matching(doc.root(), &|n: NodeRef<'_>| {
            n.is_element()
                && tag.as_deref().is_none_or(|t| n.tag() == t)
                && n.text_content().trim() == text
        }),
        Target::Path(path) => resolve_path(doc, path),
    }
}

/// Describe `node` as the most stable [`Target`] (master side): prefer its `id`,
/// then its visible text, then a structural child-index path. `None` only if the
/// node is absent.
pub fn describe(doc: &Document, node: NodeId) -> Option<Target> {
    let nref = node_ref(doc, node)?;
    if nref.is_element() {
        if let Some(id) = nref.attr("id") {
            if !id.is_empty() {
                return Some(Target::Id(id.to_string()));
            }
        }
        let text = nref.text_content();
        let trimmed = text.trim();
        if !trimmed.is_empty() && trimmed.len() <= 80 {
            return Some(Target::Text {
                tag: Some(nref.tag().to_string()),
                text: trimmed.to_string(),
            });
        }
    }
    path_to(doc, node).map(Target::Path)
}

/// Invert a JS-model-id → [`NodeId`] map (as returned by the bridge) into the
/// [`NodeId`] → JS-id direction the group needs to dispatch at a resolved node.
pub fn invert_id_map(id_map: &HashMap<u64, NodeId>) -> HashMap<NodeId, u64> {
    id_map.iter().map(|(&js, &node)| (node, js)).collect()
}

/// The concatenated text of `node` and its descendants, or `None` if absent.
pub fn text_content_of(doc: &Document, node: NodeId) -> Option<String> {
    node_ref(doc, node).map(|n| n.text_content())
}

/// Pre-order search for the first node satisfying `pred`.
fn find_matching<'a>(node: NodeRef<'a>, pred: &dyn Fn(NodeRef<'a>) -> bool) -> Option<NodeId> {
    if pred(node) {
        return Some(node.id());
    }
    for child in node.children() {
        if let Some(id) = find_matching(child, pred) {
            return Some(id);
        }
    }
    None
}

/// A cursor at `target`, or `None` if it is not in the document.
fn node_ref(doc: &Document, target: NodeId) -> Option<NodeRef<'_>> {
    fn rec<'a>(node: NodeRef<'a>, target: NodeId) -> Option<NodeRef<'a>> {
        if node.id() == target {
            return Some(node);
        }
        for child in node.children() {
            if let Some(found) = rec(child, target) {
                return Some(found);
            }
        }
        None
    }
    rec(doc.root(), target)
}

/// Walk from the root following `path` child-indices.
fn resolve_path(doc: &Document, path: &[usize]) -> Option<NodeId> {
    let mut cur = doc.root();
    for &i in path {
        cur = cur.children().nth(i)?;
    }
    Some(cur.id())
}

/// The child-index path from the root to `target`, or `None` if absent.
fn path_to(doc: &Document, target: NodeId) -> Option<Vec<usize>> {
    fn rec(node: NodeRef<'_>, target: NodeId, acc: &mut Vec<usize>) -> bool {
        if node.id() == target {
            return true;
        }
        for (i, child) in node.children().enumerate() {
            acc.push(i);
            if rec(child, target, acc) {
                return true;
            }
            acc.pop();
        }
        false
    }
    let mut acc = Vec::new();
    rec(doc.root(), target, &mut acc).then_some(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_dom::parse_html;

    fn doc() -> Document {
        parse_html("<div id=\"wrap\"><button id=\"go\">Click me</button><p>Hello world</p></div>")
    }

    #[test]
    fn resolves_by_id() {
        let d = doc();
        let go = resolve(&d, &Target::Id("go".into())).expect("#go");
        assert_eq!(text_content_of(&d, go).as_deref(), Some("Click me"));
        assert_eq!(resolve(&d, &Target::Id("missing".into())), None);
    }

    #[test]
    fn resolves_by_text_with_tag() {
        let d = doc();
        let p = resolve(
            &d,
            &Target::Text {
                tag: Some("p".into()),
                text: "Hello world".into(),
            },
        )
        .expect("the paragraph");
        assert_eq!(text_content_of(&d, p).as_deref(), Some("Hello world"));
        // Wrong tag does not match even though the text is reachable.
        assert_eq!(
            resolve(
                &d,
                &Target::Text {
                    tag: Some("h1".into()),
                    text: "Hello world".into()
                }
            ),
            None
        );
    }

    #[test]
    fn path_round_trips_through_describe() {
        let d = doc();
        // The <p> has no id; describe should fall back to text, and the button
        // (with an id) should describe as Id.
        let go = resolve(&d, &Target::Id("go".into())).unwrap();
        assert_eq!(describe(&d, go), Some(Target::Id("go".into())));

        // A path resolves back to the same node it was derived from.
        let p = resolve(
            &d,
            &Target::Text {
                tag: Some("p".into()),
                text: "Hello world".into(),
            },
        )
        .unwrap();
        let path = path_to(&d, p).expect("a path to <p>");
        assert_eq!(resolve(&d, &Target::Path(path)), Some(p));
    }

    #[test]
    fn invert_id_map_swaps_direction() {
        let mut m = HashMap::new();
        m.insert(7u64, 3u32);
        m.insert(9u64, 4u32);
        let inv = invert_id_map(&m);
        assert_eq!(inv.get(&3u32), Some(&7u64));
        assert_eq!(inv.get(&4u32), Some(&9u64));
    }
}

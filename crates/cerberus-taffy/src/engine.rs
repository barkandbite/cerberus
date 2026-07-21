//! `TaffyLayout` — a `LayoutEngine` that lays out block/flex/grid **box geometry**
//! with taffy and delegates every inline formatting context to the block engine's
//! shared inline painter (`cerberus_layout::flow_inline`).
//!
//! The split (`RENDERING_ARCHITECTURE_PLAN.md`, Stage 3):
//!
//! * **taffy owns the box tree.** Each element whose `display` is block/flex/grid
//!   (and that the walker doesn't render wholesale — see [`walk_handled`]) becomes
//!   a taffy node via [`crate::to_taffy_style`]. taffy computes positions and
//!   sizes with the standardized CSS algorithms instead of our hand-rolled cursor.
//! * **the inline layer stays ours.** Text, inline elements, inline-blocks,
//!   images, form controls, lists, and tables are *leaves*: taffy sizes them
//!   through a measure closure that flows the content with `flow_inline`, and the
//!   paint pass flows them again at their final rect. So shaping and inline flow
//!   remain the single source of truth below the box level, and lists/tables reuse
//!   the walker's existing rendering unchanged.
//!
//! This is the opt-in engine behind `LayoutEngineKind::Taffy`; `Block` stays the
//! default until a page's taffy RMSE is no worse on the parity corpus.

use cerberus_layout::{
    box_decorations, flow_inline, ElementBox, FormState, ImageProvider, InlineFlow, LaidOut,
    LayoutEngine,
};
use cerberus_paint::{translate_items, DisplayList, TextShaper};
use cerberus_style::{ComputedStyle, Display, StyledChild, StyledDom, StyledNode, TextAlign};
use cerberus_types::{Rect, Size};
use std::cell::RefCell;
use std::collections::HashMap;
use taffy::geometry::Size as TaffySize;
use taffy::style::{AvailableSpace, Dimension};
use taffy::{NodeId, TaffyTree};

/// Memoized inline-leaf flows, keyed by `(leaf index, flow width)`. taffy calls
/// the measure closure while sizing; the flow it produces there is reused at
/// paint time (translated into place) instead of re-shaping the run — the leaf's
/// content is otherwise flowed twice. Keyed by width so a paint at a different
/// width than the last measure safely re-flows.
type FlowCache = RefCell<HashMap<(usize, i32), InlineFlow>>;

/// The taffy box engine. Stateless between calls — a fresh tree is built per
/// layout so there is nothing to carry across documents.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaffyLayout;

/// One anonymous inline formatting context — a run of inline-level children of a
/// block container. taffy places it at the container's content box (already inset
/// by the container's padding/border), so its own rect is where the text flows.
/// Holds borrowed styled data (valid for the layout call) plus the inherited style
/// bare text runs paint with.
struct Leaf<'a> {
    /// The inline-level children flowed into this box.
    children: &'a [StyledChild],
    /// Inherited style for bare text runs (color/font/size).
    text_style: ComputedStyle,
    align: TextAlign,
}

/// One node in our parallel tree, kept alongside taffy's so the paint pass can
/// walk structure + styled data together (taffy stores only geometry + the leaf
/// index it needs for measurement).
struct Built<'a> {
    id: NodeId,
    /// The element box (decorations + hit region), or `None` for an anonymous run.
    element: Option<&'a StyledNode>,
    /// Index into `leaves` if this node flows inline content; `None` for a pure
    /// block/flex/grid container.
    leaf: Option<usize>,
    /// Indices into `nodes` (children, in visual order).
    children: Vec<usize>,
}

/// Tags the walker renders as a self-contained subtree (replaced content, form
/// controls, lists, tables). These stay inline-level leaves so `flow_inline` (and
/// thus `walk`) renders them exactly as the block engine does, rather than taffy
/// trying to lay out their internals.
fn walk_handled(tag: &str) -> bool {
    matches!(
        tag,
        "img"
            | "input"
            | "button"
            | "textarea"
            | "select"
            | "hr"
            | "br"
            | "canvas"
            | "svg"
            | "video"
            | "audio"
            | "iframe"
            | "object"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "caption"
            | "colgroup"
            | "col"
            | "ul"
            | "ol"
            | "li"
            | "dl"
            | "dt"
            | "dd"
    )
}

/// Whether a child establishes its own taffy box (a block/flex/grid container we
/// recurse into) rather than participating in an inline run.
fn is_block_container(child: &StyledChild) -> bool {
    match child {
        StyledChild::Text(_) => false,
        StyledChild::Element(e) => {
            matches!(
                e.style.display,
                Display::Block | Display::Flex | Display::Grid
            ) && !walk_handled(&e.tag)
        }
    }
}

/// A margin length in px if it is a plain `px` value (the common case for UA and
/// author block margins); `None` for `%`/viewport/auto, which we don't collapse.
fn px_len(l: cerberus_style::Len) -> Option<i32> {
    match l {
        cerberus_style::Len::Px(p) => Some(p),
        _ => None,
    }
}

/// Whether a run of children carries anything worth a box (non-whitespace text or
/// any element), so purely-whitespace gaps between blocks don't add empty leaves.
fn run_has_content(run: &[StyledChild]) -> bool {
    run.iter().any(|c| match c {
        StyledChild::Text(t) => !t.trim().is_empty(),
        StyledChild::Element(_) => true,
    })
}

/// Builds the taffy tree + parallel `Built` arena from the styled tree.
struct Builder<'a> {
    tree: TaffyTree<usize>,
    nodes: Vec<Built<'a>>,
    leaves: Vec<Leaf<'a>>,
    /// Viewport px, so `to_taffy_style` resolves `vw`/`vh`/`vmin`/`vmax` units on
    /// every node (not just the root) — a `width: 60vw` must become 600px at a
    /// 1000px viewport, not 0.
    vw: i32,
    vh: i32,
}

impl<'a> Builder<'a> {
    fn new(vw: i32, vh: i32) -> Self {
        Self {
            tree: TaffyTree::new(),
            nodes: Vec::new(),
            leaves: Vec::new(),
            vw,
            vh,
        }
    }

    /// Overwrite a node's top margin (px) — used to apply collapsed margins.
    fn set_margin_top(&mut self, id: NodeId, top: i32) {
        if let Ok(style) = self.tree.style(id) {
            let mut style = style.clone();
            style.margin.top = taffy::style::LengthPercentageAuto::length(top as f32);
            let _ = self.tree.set_style(id, style);
        }
    }

    /// Push an anonymous inline-leaf node and return its `nodes` index.
    fn push_leaf(
        &mut self,
        children: &'a [StyledChild],
        text_style: ComputedStyle,
        align: TextAlign,
    ) -> usize {
        let leaf_idx = self.leaves.len();
        self.leaves.push(Leaf {
            children,
            text_style,
            align,
        });
        let id = self
            .tree
            .new_leaf_with_context(taffy::Style::default(), leaf_idx)
            .expect("taffy leaf");
        let node_idx = self.nodes.len();
        self.nodes.push(Built {
            id,
            element: None,
            leaf: Some(leaf_idx),
            children: Vec::new(),
        });
        node_idx
    }

    /// Recursively build `node`, returning its `nodes` index (or `None` if
    /// `display:none`). `style_override` lets the caller pin the root's width.
    fn build(
        &mut self,
        node: &'a StyledNode,
        style_override: Option<taffy::Style>,
    ) -> Option<usize> {
        if node.style.display == Display::None {
            return None;
        }
        let style =
            style_override.unwrap_or_else(|| crate::to_taffy_style(&node.style, self.vw, self.vh));

        // Every element is a taffy **container** (block/flex/grid box); its inline
        // content becomes anonymous inline-leaf children. Crucially a block box
        // with only text is NOT a measure-leaf — a taffy leaf sized by its measure
        // function shrink-to-fits its content, but a block box must fill the
        // container width (CSS 2.1 §10.3.3) and only *then* wrap its inline content
        // into that width. Wrapping the text in an anonymous leaf child gives the
        // block its normal stretch while the leaf, placed at the container's
        // content box, measures the height for that definite width.
        let mut child_nodes: Vec<usize> = Vec::new();
        let mut child_ids: Vec<NodeId> = Vec::new();
        let mut run_start = 0usize;
        let children = &node.children;
        // Adjacent-sibling margin collapsing: taffy sums vertical margins, but CSS
        // collapses a block's bottom margin with the next block's top margin to
        // their max. Only meaningful in a block formatting context (not flex/grid),
        // and we track the previous block child's bottom margin to shrink the next
        // one's top so the gap becomes max(bottom, top) instead of their sum.
        let collapsing = node.style.display == Display::Block;
        let mut prev_bottom: Option<i32> = None;
        for i in 0..children.len() {
            if is_block_container(&children[i]) {
                let run = &children[run_start..i];
                if run_has_content(run) {
                    let ni = self.push_leaf(run, node.style.clone(), node.style.text_align);
                    child_ids.push(self.nodes[ni].id);
                    child_nodes.push(ni);
                    // Inline content between blocks breaks margin adjacency.
                    prev_bottom = None;
                }
                if let StyledChild::Element(e) = &children[i] {
                    if let Some(ni) = self.build(e, None) {
                        let id = self.nodes[ni].id;
                        if collapsing {
                            if let (Some(pb), Some(top)) = (prev_bottom, px_len(e.style.margin_top))
                            {
                                // gap = pb + collapsed_top = max(pb, top).
                                self.set_margin_top(id, (top.max(pb) - pb).max(0));
                            }
                            prev_bottom = px_len(e.style.margin_bottom);
                        }
                        child_ids.push(id);
                        child_nodes.push(ni);
                    }
                }
                run_start = i + 1;
            }
        }
        let tail = &children[run_start..];
        if run_has_content(tail) {
            let ni = self.push_leaf(tail, node.style.clone(), node.style.text_align);
            child_ids.push(self.nodes[ni].id);
            child_nodes.push(ni);
        }

        let id = self
            .tree
            .new_with_children(style, &child_ids)
            .expect("taffy container");
        let node_idx = self.nodes.len();
        self.nodes.push(Built {
            id,
            element: Some(node),
            leaf: None,
            children: child_nodes,
        });
        Some(node_idx)
    }
}

impl LayoutEngine for TaffyLayout {
    fn layout(
        &mut self,
        styled: &StyledDom,
        viewport: Size,
        shaper: &dyn TextShaper,
        images: &dyn ImageProvider,
        forms: &dyn FormState,
    ) -> LaidOut {
        let vw = viewport.w as i32;
        let vh = viewport.h as i32;

        // Build the tree. Pin the root to the viewport width so block children have
        // a definite containing block to stretch into (matching the walker's
        // full-width page box).
        let mut b = Builder::new(vw, vh);
        let mut root_style = crate::to_taffy_style(&styled.root.style, vw, vh);
        root_style.size.width = Dimension::length(vw as f32);
        let Some(root_idx) = b.build(&styled.root, Some(root_style)) else {
            return LaidOut::default();
        };
        let root_id = b.nodes[root_idx].id;

        // Compute geometry. Width is the definite viewport; height grows to content
        // (max-content) so the document can be taller than the viewport.
        let leaves = &b.leaves;
        let cache: FlowCache = RefCell::new(HashMap::new());
        b.tree
            .compute_layout_with_measure(
                root_id,
                TaffySize {
                    width: AvailableSpace::Definite(vw as f32),
                    height: AvailableSpace::MaxContent,
                },
                |known, avail, _id, ctx, _style| {
                    let Some(&mut leaf_idx) = ctx else {
                        return TaffySize::ZERO;
                    };
                    let leaf = &leaves[leaf_idx];
                    // Content width to flow at: a known/definite width when taffy
                    // has one, else min-content (0 → wrap at every space, yielding
                    // the widest word) or max-content (a wide probe → one line).
                    let content_w = match (known.width, avail.width) {
                        (Some(w), _) => w,
                        (None, AvailableSpace::Definite(w)) => w,
                        (None, AvailableSpace::MinContent) => 0.0,
                        (None, AvailableSpace::MaxContent) => 1_000_000.0,
                    };
                    let key_w = (content_w as i32).max(1);
                    let flow = flow_inline(
                        leaf.children,
                        &leaf.text_style,
                        leaf.align,
                        0,
                        key_w,
                        0,
                        shaper,
                        images,
                        forms,
                        0,
                        vw,
                        vh,
                    );
                    let size = TaffySize {
                        width: flow.width as f32,
                        height: flow.height as f32,
                    };
                    // Stash for the paint pass to reuse (translated) at this width.
                    cache.borrow_mut().insert((leaf_idx, key_w), flow);
                    size
                },
            )
            .expect("taffy compute");

        // Paint pass: walk structure + geometry together, decorate each element
        // box, then flow each leaf's inline content at its final rect.
        let mut out = LaidOut {
            display: DisplayList::new(),
            links: Vec::new(),
            fields: Vec::new(),
            elements: Vec::new(),
        };
        let mut field_counter = 0u32;
        paint(
            &b,
            root_idx,
            0,
            0,
            shaper,
            images,
            forms,
            vw,
            vh,
            &cache,
            &mut out,
            &mut field_counter,
        );
        out
    }
}

/// Emit one node's decorations + content, then recurse. `(ox, oy)` is the parent
/// border-box top-left; taffy child `location` is relative to it.
#[allow(clippy::too_many_arguments)]
fn paint(
    b: &Builder<'_>,
    idx: usize,
    ox: i32,
    oy: i32,
    shaper: &dyn TextShaper,
    images: &dyn ImageProvider,
    forms: &dyn FormState,
    vw: i32,
    vh: i32,
    cache: &FlowCache,
    out: &mut LaidOut,
    field_counter: &mut u32,
) {
    let node = &b.nodes[idx];
    let layout = b.tree.layout(node.id).expect("taffy layout");
    let x = ox + layout.location.x.round() as i32;
    let y = oy + layout.location.y.round() as i32;
    let w = layout.size.width.round() as i32;
    let h = layout.size.height.round() as i32;
    let rect = Rect::new(x, y, w.max(0) as u32, h.max(0) as u32);

    if let Some(el) = node.element {
        box_decorations(&el.style, rect, images, &mut out.display.items);
        out.elements.push(ElementBox {
            rect,
            node: el.node_id,
        });
    }

    if let Some(leaf_idx) = node.leaf {
        let leaf = &b.leaves[leaf_idx];
        // taffy already positioned this anonymous leaf at its parent's content box
        // (inset by the parent's padding + border), so its own rect is where the
        // inline content flows. The measure pass already flowed this run at this
        // width, so reuse that (translated into place) instead of re-shaping —
        // except when it produced form fields, whose ids were numbered from 0 in
        // measure and must instead continue the document counter here.
        let key_w = w.max(1);
        let cached = cache.borrow_mut().remove(&(leaf_idx, key_w));
        match cached {
            Some(mut f) if f.fields.is_empty() => {
                translate_items(&mut f.display, x, y);
                for l in &mut f.links {
                    l.rect.x += x;
                    l.rect.y += y;
                }
                for e in &mut f.elements {
                    e.rect.x += x;
                    e.rect.y += y;
                }
                out.display.items.append(&mut f.display);
                out.links.append(&mut f.links);
                out.elements.append(&mut f.elements);
                // No fields → the counter is unchanged.
            }
            _ => {
                let flow = flow_inline(
                    leaf.children,
                    &leaf.text_style,
                    leaf.align,
                    x,
                    (x + w).max(x + 1),
                    y,
                    shaper,
                    images,
                    forms,
                    *field_counter,
                    vw,
                    vh,
                );
                out.display.items.extend(flow.display);
                out.links.extend(flow.links);
                out.fields.extend(flow.fields);
                out.elements.extend(flow.elements);
                *field_counter = flow.next_field_id;
            }
        }
    }

    for &c in &node.children {
        paint(
            b,
            c,
            x,
            y,
            shaper,
            images,
            forms,
            vw,
            vh,
            cache,
            out,
            field_counter,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_layout::{NoForms, NoImages};
    use cerberus_paint::MonoShaper;
    use cerberus_style::{FlexDirection, Len};

    fn el(id: u32, tag: &str, style: ComputedStyle, children: Vec<StyledChild>) -> StyledNode {
        StyledNode {
            tag: tag.into(),
            attrs: Vec::new(),
            style,
            children,
            node_id: id,
        }
    }

    fn block(w: Option<i32>, h: i32) -> ComputedStyle {
        let mut s = ComputedStyle::initial();
        s.display = Display::Block;
        if let Some(w) = w {
            s.width = Len::Px(w);
        }
        s.height = Len::Px(h);
        s
    }

    /// The rect of the element with `node_id == id`, if painted.
    fn rect_of(out: &LaidOut, id: u32) -> Option<Rect> {
        out.elements.iter().find(|e| e.node == id).map(|e| e.rect)
    }

    fn run(root: StyledNode, w: u32, h: u32) -> LaidOut {
        TaffyLayout.layout(
            &StyledDom {
                root,
                font_face_families: Vec::new(),
            },
            Size { w, h },
            &MonoShaper,
            &NoImages,
            &NoForms,
        )
    }

    #[test]
    fn block_children_stack_vertically() {
        let root = el(
            0,
            "div",
            block(None, 0),
            vec![
                StyledChild::Element(Box::new(el(1, "div", block(None, 40), vec![]))),
                StyledChild::Element(Box::new(el(2, "div", block(None, 30), vec![]))),
            ],
        );
        let out = run(root, 200, 200);
        let a = rect_of(&out, 1).expect("first box");
        let b = rect_of(&out, 2).expect("second box");
        // Stacked: second sits directly below the first (40px tall).
        assert_eq!(a.y, 0);
        assert_eq!(b.y, 40);
        assert_eq!(a.x, b.x);
        // Block children stretch to the (viewport) container width.
        assert_eq!(a.w, 200);
        assert_eq!(b.w, 200);
    }

    #[test]
    fn flex_row_places_items_side_by_side() {
        let mut row = block(None, 0);
        row.display = Display::Flex;
        row.flex_direction = FlexDirection::Row;
        let item = |id| {
            let mut s = block(Some(50), 30);
            s.flex_shrink = 0.0;
            StyledChild::Element(Box::new(el(id, "div", s, vec![])))
        };
        let root = el(0, "div", row, vec![item(1), item(2)]);
        let out = run(root, 200, 200);
        let a = rect_of(&out, 1).expect("item 1");
        let b = rect_of(&out, 2).expect("item 2");
        // Same row, second starts where the first ends (50px wide, no gap).
        assert_eq!(a.y, b.y);
        assert_eq!(a.x, 0);
        assert_eq!(b.x, 50);
        assert_eq!(a.w, 50);
        assert_eq!(b.w, 50);
    }

    #[test]
    fn display_none_root_lays_out_nothing() {
        let mut s = block(None, 40);
        s.display = Display::None;
        let out = run(el(0, "div", s, vec![]), 200, 200);
        assert!(out.elements.is_empty());
        assert!(out.display.items.is_empty());
    }
}

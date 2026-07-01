//! One mirrored window's per-session state.
//!
//! An instance is a sealed session ([`InstanceId`]) plus how far it has caught
//! up to the master ([`cursor`](MirrorInstance::cursor)) and its last rendered
//! DOM. Only the focused instance holds the live realm (`live`); the rest are
//! dormant snapshots.

use std::collections::HashMap;

use cerberus_dom::{parse_html, Document, NodeId};
use cerberus_types::InstanceId;

use crate::action::{Action, Target};
use crate::resolve::{resolve, text_content_of};

/// Why a follower could not stay in lockstep with the master.
///
/// A divergence is an expected outcome (the follower's session legitimately
/// differs), not a failure: the offending action could not be resolved in this
/// session, so replay stopped here for manual attention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// A human-readable reason (e.g. the target was absent in this session).
    pub reason: String,
    /// The action that could not be applied.
    pub action: Action,
}

/// A single window in a [`MirrorGroup`](crate::MirrorGroup).
pub struct MirrorInstance {
    pub(crate) id: InstanceId,
    pub(crate) label: String,
    pub(crate) cursor: usize,
    pub(crate) url: Option<String>,
    pub(crate) doc: Document,
    pub(crate) node_to_js: HashMap<NodeId, u64>,
    pub(crate) live: bool,
    /// Whether this instance's resident DOM has been dropped (dormant). A
    /// released instance must rebuild via catch-up on its next focus; a resident
    /// one that is already converged can be re-viewed without any rebuild.
    pub(crate) released: bool,
    pub(crate) diverged: Option<Divergence>,
    /// This instance's per-head anti-fingerprinting JS shim, injected into its
    /// realm on every catch-up (issue #69) so mirrored identities do not share a
    /// byte-identical canvas/audio/WebGL fingerprint. Pre-rendered by the caller
    /// (`Head::farbling.js_prologue()` in the app) so this crate needs no
    /// dependency on `cerberus-farbling` — same reasoning as `user_agent` being a
    /// plain `String` here rather than a UA-generating type.
    pub(crate) farbling_prologue: String,
}

impl MirrorInstance {
    /// A fresh, dormant instance with an empty document and no realm.
    pub(crate) fn new(id: InstanceId, label: String, farbling_prologue: String) -> Self {
        Self {
            id,
            label,
            cursor: 0,
            url: None,
            doc: parse_html(""),
            node_to_js: HashMap::new(),
            live: false,
            released: false,
            diverged: None,
            farbling_prologue,
        }
    }

    /// This window's sealed-session identity.
    pub fn id(&self) -> InstanceId {
        self.id
    }

    /// The window's display label (e.g. "work", "personal").
    pub fn label(&self) -> &str {
        &self.label
    }

    /// How many log actions this instance has applied (its catch-up cursor).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The currently loaded URL, if any.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The instance's last rendered document.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Whether this instance currently owns the single live realm.
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The divergence flag, if this instance fell out of lockstep.
    pub fn diverged(&self) -> Option<&Divergence> {
        self.diverged.as_ref()
    }

    /// Convenience: the text of the element with `id` in this instance's
    /// document (handy for assertions and simple read-outs).
    pub fn text_of_id(&self, id: &str) -> Option<String> {
        let node = resolve(&self.doc, &Target::Id(id.to_string()))?;
        text_content_of(&self.doc, node)
    }

    /// Drop this instance's resident DOM and node map, keeping only its cursor,
    /// URL, and identity. Used when an instance is dormant (hidden/minimized) so
    /// resident memory does not grow with the profile count — a later focus
    /// re-materializes it from the action log via catch-up.
    pub(crate) fn release(&mut self) {
        self.doc = parse_html("");
        self.node_to_js = HashMap::new();
        self.released = true;
    }
}

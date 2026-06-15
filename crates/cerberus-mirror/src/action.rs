//! The semantic action model: what the master records and a follower replays.
//!
//! Actions carry *intent*, not pixels. A [`Target`] is a stable, re-resolvable
//! descriptor so the same action lands on the right node even when a follower's
//! session renders a different DOM (a logged-in vs. logged-out layout, an A/B
//! variant). See [`crate::resolve`] for how a target is matched per-document.

/// A stable way to name a DOM node across sessions, resolved per-document at
/// replay time (never a coordinate or a live engine handle).
///
/// Ordered by preference when *recording* (see [`crate::describe`]): an `id` is
/// the most stable, then visible `text`, then a structural [`Path`] of
/// child-indices as a last resort.
///
/// [`Path`]: Target::Path
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Match the first element whose `id` attribute equals this string.
    Id(String),
    /// Match the first element (optionally restricted to `tag`) whose trimmed
    /// `text_content` equals `text` — e.g. a button labelled "Sign in".
    Text {
        /// Restrict to this element tag, or match any tag when `None`.
        tag: Option<String>,
        /// The exact trimmed visible text to match.
        text: String,
    },
    /// Walk from the document root following these child indices. Deterministic
    /// for an identical tree; on a structurally different follower it resolves
    /// to a different node or none (surfacing as divergence) — which is why it
    /// is the last-resort descriptor.
    Path(Vec<usize>),
}

/// One recorded user intent. Replaying it on a follower reproduces the master's
/// step in that follower's own session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Load a URL (each session fetches it under its own identity).
    Navigate(String),
    /// Click (and run listeners on) the target node.
    Click(Target),
    /// Set the target control's value to `text`, then fire an `input` event.
    Input {
        /// The control to type into.
        target: Target,
        /// The text to set as the control's value.
        text: String,
    },
    /// Submit the target form (fires a `submit` event).
    Submit(Target),
    /// Scroll to a position. Recorded for fidelity; it mutates no DOM, so a
    /// follower stores it without a realm round-trip.
    Scroll {
        /// Horizontal scroll offset in CSS pixels.
        x: i32,
        /// Vertical scroll offset in CSS pixels.
        y: i32,
    },
}

/// An append-only, ordered list of [`Action`]s with a length the followers use
/// as a cursor. The master only ever appends; followers read a prefix.
#[derive(Clone, Debug, Default)]
pub struct ActionLog {
    actions: Vec<Action>,
}

impl ActionLog {
    /// A new, empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an action (master side).
    pub fn push(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// The number of actions recorded — the head cursor.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether no action has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The actions in order.
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_appends_in_order_and_tracks_length() {
        let mut log = ActionLog::new();
        assert!(log.is_empty());
        log.push(Action::Navigate("https://a.test/".into()));
        log.push(Action::Click(Target::Id("go".into())));
        assert_eq!(log.len(), 2);
        assert_eq!(log.actions()[0], Action::Navigate("https://a.test/".into()));
        assert_eq!(log.actions()[1], Action::Click(Target::Id("go".into())));
    }
}

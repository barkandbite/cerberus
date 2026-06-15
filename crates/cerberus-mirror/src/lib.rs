//! Concurrent multi-window **mirror groups** (ADR-0017).
//!
//! A mirror group is one **master** window the user drives plus N **follower**
//! windows showing the *same site* — every navigation, click, and keystroke on
//! the master is matched on each follower — while each window is a *separate
//! sealed session* (its own [`InstanceId`], cookies, storage, identity). The
//! payoff: operate several accounts of one site at once by acting once.
//!
//! [`InstanceId`]: cerberus_types::InstanceId
//!
//! # The constraint, and how this satisfies it
//!
//! PLAN §1's prime directive allows **at most one live JS engine**. N windows
//! each holding a live realm would be N engines. So mirror groups never run more
//! than one realm at a time:
//!
//! - The master records intent as a [`Action`] log of *portable* steps —
//!   targets are stable descriptors ([`Target`]), never pixel coordinates — so a
//!   follower can replay them against *its own* (possibly divergent) DOM.
//! - Only the **focused** instance owns the single live realm; backgrounded
//!   followers are just *(a log cursor + a serialized DOM)* and hold no engine.
//! - Focusing a follower tears down the live realm, instantiates the focused
//!   one, and **fast-forwards** it through the log in its own session until it
//!   converges ([`MirrorGroup::focus`]). This is the macro/catch-up model:
//!   logically all windows track the master; physically one runs JS at a time.
//!
//! When a follower cannot resolve an action's target (logged out, a different
//! A/B variant, a captcha) the instance is flagged [`Divergence`] rather than
//! guessing — faithfulness over forced lockstep.
//!
//! This crate is the *model*: it is fully headless-testable over the real engine
//! seam ([`cerberus_js`]) and DOM bridge ([`cerberus_js_dom`]). Driving real OS
//! windows from it is the shell's job (`cerberus-shell-winit`).

mod action;
mod group;
mod instance;
mod resolve;
mod source;

pub use action::{Action, ActionLog, Target};
pub use group::MirrorGroup;
pub use instance::{Divergence, MirrorInstance};
pub use resolve::{describe, invert_id_map, resolve, text_content_of};
pub use source::PageSource;

use std::fmt;

/// Something went wrong driving a [`MirrorGroup`].
///
/// A target that fails to resolve in a follower is **not** an error — that is a
/// [`Divergence`] recorded on the instance and surfaced for manual attention.
/// These arms are genuine failures: a misconfigured group, a bad index, or an
/// engine / DOM-bridge / page-source fault.
#[derive(Debug)]
pub enum MirrorError {
    /// A group was constructed with no members.
    Empty,
    /// An instance index outside `0..instances().len()`.
    NoSuchInstance(usize),
    /// The JS engine seam failed (realm create/destroy/eval).
    Engine(cerberus_js::JsError),
    /// The DOM bridge failed (install / serialize / dispatch).
    Bridge(cerberus_js_dom::BridgeError),
    /// The [`PageSource`] could not load a page for a session.
    Source(String),
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorError::Empty => write!(f, "a mirror group needs at least one member"),
            MirrorError::NoSuchInstance(i) => write!(f, "no such instance: {i}"),
            MirrorError::Engine(e) => write!(f, "engine error: {e}"),
            MirrorError::Bridge(e) => write!(f, "DOM bridge error: {e}"),
            MirrorError::Source(m) => write!(f, "page source error: {m}"),
        }
    }
}

impl std::error::Error for MirrorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MirrorError::Engine(e) => Some(e),
            MirrorError::Bridge(e) => Some(e),
            _ => None,
        }
    }
}

impl From<cerberus_js::JsError> for MirrorError {
    fn from(e: cerberus_js::JsError) -> Self {
        MirrorError::Engine(e)
    }
}

impl From<cerberus_js_dom::BridgeError> for MirrorError {
    fn from(e: cerberus_js_dom::BridgeError) -> Self {
        MirrorError::Bridge(e)
    }
}

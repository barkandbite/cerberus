//! The page-source seam: how a session turns a URL into a document.
//!
//! Each instance is a *separate sealed session*, so loading the same URL may
//! yield different DOMs (different cookies → different account view). The group
//! never reaches the network directly; it asks a `PageSource`, passing the
//! [`InstanceId`] so the implementation routes through that session's jar,
//! proxy, and identity. The app wires a real loader here; tests wire a fake.
//!
//! [`InstanceId`]: cerberus_types::InstanceId

use cerberus_dom::Document;
use cerberus_types::InstanceId;

/// Loads a page for a specific session.
///
/// `load` returns the parsed [`Document`] (its inline scripts available via
/// [`Document::scripts`]) for `url` *as seen by* `instance`. An `Err(String)` is
/// a transport/parse failure for that session; it surfaces as
/// [`MirrorError::Source`](crate::MirrorError::Source).
pub trait PageSource {
    /// Load `url` under the identity `instance`.
    fn load(&self, instance: InstanceId, url: &str) -> Result<Document, String>;
}

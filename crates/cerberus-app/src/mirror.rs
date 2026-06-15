//! App integration for mirror groups (ADR-0018).
//!
//! A real [`PageSource`] that loads pages for a sealed instance through the
//! existing **synchronous** stack — built-in `cerberus:` pages via
//! [`BuiltinHttpClient`], http(s) via the jar-attached network client — and a
//! builder that turns the head manager's identities into a [`MirrorGroup`].
//!
//! The network client carries the sealed cookie jar; each load passes a
//! [`FetchContext`] tagged with the instance, so every profile fetches under its
//! *own* session while the group runs the single shared engine (ADR-0017).

use std::sync::Arc;

use cerberus_dom::{parse_html, Document};
use cerberus_identity::HeadManager;
use cerberus_js::JsEngineFactory;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_mirror::{MirrorError, MirrorGroup, PageSource};
use cerberus_net::{BuiltinHttpClient, FetchContext, FetchKind, HttpClient};
use cerberus_types::InstanceId;
use cerberus_url::parse as parse_url;

/// A [`PageSource`] over the app's synchronous load path.
///
/// Holds the jar-attached network client (the same kind the one-shot `render`
/// builds via `network_client`); `None` serves only built-in pages (tests and
/// offline modes). Built-in `cerberus:` URLs always go to [`BuiltinHttpClient`],
/// so they work with or without a network client.
pub struct AppPageSource {
    client: Option<Arc<dyn HttpClient>>,
}

impl AppPageSource {
    /// A source that loads real http(s) pages through `client` (which must
    /// already carry the sealed cookie jar), plus built-in pages.
    pub fn new(client: Arc<dyn HttpClient>) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// A source that serves only built-in `cerberus:` pages (no network).
    pub fn builtin_only() -> Self {
        Self { client: None }
    }
}

impl PageSource for AppPageSource {
    fn load(&self, instance: InstanceId, url: &str) -> Result<Document, String> {
        let parsed = parse_url(url).map_err(|e| e.to_string())?;
        let response = if parsed.is_builtin() {
            BuiltinHttpClient
                .get(&parsed)
                .map_err(|e| format!("{e:?}"))?
        } else {
            let client = self
                .client
                .as_ref()
                .ok_or_else(|| format!("no network client configured for {url}"))?;
            // Tag the fetch with this profile's instance so the sealed jar
            // attaches/captures cookies under the right session.
            let ctx = FetchContext {
                instance,
                kind: FetchKind::Navigation,
            };
            client.get_in(&parsed, &ctx).map_err(|e| format!("{e:?}"))?
        };
        Ok(parse_html(&String::from_utf8_lossy(&response.body)))
    }
}

/// Build a [`MirrorGroup`] whose members are the identities in `heads`, in order
/// (the first is the master). The group gets its **own** engine; a caller
/// entering mirror mode should tear down the single-window engine first so the
/// global ≤1-live-engine invariant holds (ADR-0017/0018).
pub fn mirror_group_from_heads(
    heads: &HeadManager,
    source: Box<dyn PageSource>,
    viewport: (u32, u32),
    user_agent: impl Into<String>,
) -> Result<MirrorGroup, MirrorError> {
    let members = heads
        .heads()
        .iter()
        .map(|h| (h.instance, h.label.clone()))
        .collect();
    let engine = QuickJsEngineFactory
        .instantiate()
        .map_err(MirrorError::Engine)?;
    MirrorGroup::new(engine, source, members, viewport, user_agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_identity::Head;
    use cerberus_mirror::Action;
    use cerberus_types::{HeadId, InstanceId};

    fn two_identities() -> HeadManager {
        let list = vec![
            Head::new(
                HeadId::from_u64_pair(0, 1),
                InstanceId::from_u64_pair(0, 1),
                "work",
                0x1111,
            ),
            Head::new(
                HeadId::from_u64_pair(0, 2),
                InstanceId::from_u64_pair(0, 2),
                "personal",
                0x2222,
            ),
        ];
        HeadManager::new(list, Box::new(QuickJsEngineFactory))
    }

    #[test]
    fn group_drives_builtin_pages_across_identities() {
        let heads = two_identities();
        let source = Box::new(AppPageSource::builtin_only());
        let mut group =
            mirror_group_from_heads(&heads, source, (1024, 768), "ua").expect("build group");
        assert_eq!(group.instances().len(), 2);
        assert_eq!(group.live_realms(), 0);

        // Drive the master to a real built-in page through the app load path.
        group
            .act(Action::Navigate("cerberus:about".into()))
            .expect("navigate");
        let master_text = group.master().document().root().text_content();
        assert!(
            master_text.contains("About Cerberus"),
            "master loaded the real built-in page, got {master_text:?}"
        );

        // The follower catches up to the same page in its own session.
        group.focus(1).expect("focus follower");
        let follower_text = group.instance(1).unwrap().document().root().text_content();
        assert_eq!(master_text, follower_text, "follower converged");
        assert!(group.live_realms() <= 1, "at most one live realm");
        assert_eq!(group.instance(1).unwrap().cursor(), group.log().len());
    }
}

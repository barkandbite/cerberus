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

use std::collections::HashMap;
use std::sync::Arc;

use cerberus_autofill::{fill_plan, Profile};
use cerberus_css::CssEngine;
use cerberus_dom::{parse_html, Document, NodeId};
use cerberus_identity::HeadManager;
use cerberus_js::JsEngineFactory;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{BlockLayout, ElementBox, LayoutEngine, NoForms, NoImages};
use cerberus_mirror::{FillKind, FillProvider, MirrorError, MirrorGroup, PageSource};
use cerberus_net::{BuiltinHttpClient, FetchContext, FetchKind, HttpClient};
use cerberus_paint::{Framebuffer, Rasterizer};
use cerberus_shell::MultiSurfaceApp;
use cerberus_style::StyleEngine;
use cerberus_text::TextEngine;
use cerberus_types::{Color, InstanceId, Rect, Size};
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

/// Maps each identity to its autofill [`Profile`] for a [`MirrorGroup`], so one
/// master `Fill` fills every window from its **own** profile. Built from the
/// vault-loaded profiles when entering mirror mode.
pub struct ProfileFillProvider {
    profiles: HashMap<InstanceId, Profile>,
}

impl ProfileFillProvider {
    /// Wrap a per-identity profile map.
    pub fn new(profiles: HashMap<InstanceId, Profile>) -> Self {
        Self { profiles }
    }
}

impl FillProvider for ProfileFillProvider {
    fn fills(&self, instance: InstanceId, kind: FillKind, doc: &Document) -> Vec<(NodeId, String)> {
        match self.profiles.get(&instance) {
            Some(profile) => fill_plan(doc, profile, kind),
            None => Vec::new(),
        }
    }
}

/// Drives a [`MirrorGroup`] across N surfaces (ADR-0017/0018): renders each
/// instance's page and turns clicks on the **master** window into broadcast
/// actions. Window 0 is the master; the rest mirror it and catch up when
/// [`focus`](MultiSurfaceApp::focus)ed. Implements [`MultiSurfaceApp`] so
/// `cerberus-shell-winit::run_multi` can place it in N OS windows.
pub struct MirrorShell {
    group: MirrorGroup,
    style: CssEngine,
    text: TextEngine,
    background: Color,
    /// Hit boxes from the master's last render, for click → target mapping.
    master_elements: Vec<ElementBox>,
}

impl MirrorShell {
    /// Wrap a built group (e.g. from [`mirror_group_from_heads`]).
    pub fn new(group: MirrorGroup) -> Self {
        Self {
            group,
            style: CssEngine::new(),
            text: TextEngine::new(),
            background: Color::WHITE,
            master_elements: Vec::new(),
        }
    }

    /// The driven group (read-only).
    pub fn group(&self) -> &MirrorGroup {
        &self.group
    }

    /// How many profiles are being driven — the count the toolbar badge shows.
    pub fn driven_count(&self) -> usize {
        self.group.instances().len()
    }

    /// Navigate every window to `url` (recorded on the master, replayed on each
    /// follower when it next catches up).
    pub fn navigate(&mut self, url: &str) -> Result<(), MirrorError> {
        self.group
            .act(cerberus_mirror::Action::Navigate(url.to_string()))
    }

    /// Render instance `idx`'s current document, keeping the master's hit boxes.
    fn render_instance(&mut self, idx: usize, size: Size) -> Framebuffer {
        let mut fb = Framebuffer::new(size);
        fb.clear(self.background);
        let Some(instance) = self.group.instance(idx) else {
            return fb;
        };
        let styled = self.style.style(instance.document());
        let mut layout = BlockLayout::default();
        let laid = layout.layout(&styled, size, &self.text, &NoImages, &NoForms);
        self.text.rasterize(&laid.display, &mut fb);
        if idx == self.group.master_index() {
            self.master_elements = laid.elements;
        }
        fb
    }
}

impl MultiSurfaceApp for MirrorShell {
    fn window_count(&self) -> usize {
        self.group.instances().len()
    }

    fn title(&self, idx: usize) -> String {
        match self.group.instance(idx) {
            Some(instance) => format!("Cerberus — {}", instance.label()),
            None => "Cerberus".to_string(),
        }
    }

    fn render(&mut self, idx: usize, size: Size) -> Framebuffer {
        self.render_instance(idx, size)
    }

    fn pointer_down(&mut self, idx: usize, x: i32, y: i32) -> Vec<usize> {
        // Only the master is driven directly; followers mirror it.
        if idx != self.group.master_index() {
            return Vec::new();
        }
        // Innermost element under the point → the most specific target.
        let target = {
            let doc = self.group.master().document();
            self.master_elements
                .iter()
                .filter(|e| rect_contains(e.rect, x, y))
                .min_by_key(|e| u64::from(e.rect.w) * u64::from(e.rect.h))
                .and_then(|e| cerberus_mirror::describe(doc, e.node))
        };
        match target {
            Some(t) => {
                let _ = self.group.act(cerberus_mirror::Action::Click(t));
                vec![self.group.master_index()]
            }
            None => Vec::new(),
        }
    }

    fn text_input(&mut self, _idx: usize, _c: char) -> Vec<usize> {
        // Typed-text and autofill driving arrive with the autofill phase
        // (ADR-0019); clicks already broadcast through `pointer_down`.
        Vec::new()
    }

    fn focus(&mut self, idx: usize) -> Vec<usize> {
        match self.group.focus(idx) {
            Ok(()) => vec![idx],
            Err(_) => Vec::new(),
        }
    }

    fn surface_hidden(&mut self, idx: usize) {
        // A hidden window's instance can drop its resident DOM until shown again
        // — the memory win that keeps thousands of profiles cheap (ADR-0017).
        let _ = self.group.release(idx);
    }
}

/// Whether device point `(x, y)` is inside `r`.
fn rect_contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32
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
    fn profile_fill_provider_maps_per_identity() {
        use cerberus_autofill::Login;
        let inst = InstanceId::from_u64_pair(0, 1);
        let other = InstanceId::from_u64_pair(0, 2);
        let mut profiles = HashMap::new();
        profiles.insert(
            inst,
            Profile {
                login: Login {
                    username: "ada".into(),
                    password: "pw".into(),
                },
                ..Profile::default()
            },
        );
        let provider = ProfileFillProvider::new(profiles);
        let doc = parse_html("<input id=\"u\" name=\"username\">");
        assert_eq!(provider.fills(inst, FillKind::Login, &doc).len(), 1);
        assert!(provider.fills(other, FillKind::All, &doc).is_empty());
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

    #[test]
    fn shell_drives_master_clicks_and_catches_up_followers() {
        let heads = two_identities();
        let group = mirror_group_from_heads(
            &heads,
            Box::new(AppPageSource::builtin_only()),
            (800, 600),
            "ua",
        )
        .unwrap();
        let mut shell = MirrorShell::new(group);
        assert_eq!(shell.window_count(), 2);
        assert_eq!(shell.driven_count(), 2);
        assert!(shell.title(0).contains("work"));

        shell.navigate("cerberus:about").unwrap();
        assert_eq!(shell.group().log().len(), 1);

        let size = Size::new(800, 600);
        let fb = shell.render(0, size);
        assert!(fb.pixel(0, 0).is_some());

        // Some master click lands on a page element and broadcasts an action.
        let before = shell.group().log().len();
        let mut acted = false;
        'scan: for gy in (0..600).step_by(15) {
            for gx in (0..800).step_by(15) {
                if !shell.pointer_down(0, gx, gy).is_empty() {
                    acted = true;
                    break 'scan;
                }
            }
        }
        assert!(acted, "a master click should hit an element and broadcast");
        assert!(shell.group().log().len() > before);

        // Followers are not driven directly.
        assert!(shell.pointer_down(1, 5, 5).is_empty());

        // Focusing the follower catches it up to the master.
        assert_eq!(shell.focus(1), vec![1usize]);
        assert_eq!(
            shell.group().instance(1).unwrap().cursor(),
            shell.group().log().len()
        );
    }

    #[test]
    fn rect_contains_edges() {
        let r = Rect {
            x: 10,
            y: 10,
            w: 100,
            h: 50,
        };
        assert!(rect_contains(r, 10, 10));
        assert!(rect_contains(r, 109, 59));
        assert!(!rect_contains(r, 9, 10));
        assert!(!rect_contains(r, 110, 10));
        assert!(!rect_contains(r, 10, 60));
    }
}

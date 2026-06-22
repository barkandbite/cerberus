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
use cerberus_dom::{parse_html, Document, NodeId, NodeRef};
use cerberus_identity::HeadManager;
use cerberus_js::JsEngineFactory;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{BlockLayout, ElementBox, FormFieldBox, LayoutEngine, NoForms, NoImages};
use cerberus_mirror::{
    Action, FillKind, FillProvider, MirrorError, MirrorGroup, PageSource, Target,
};
use cerberus_net::{BuiltinHttpClient, FetchContext, FetchKind, HttpClient};
use cerberus_paint::{Framebuffer, Rasterizer};
use cerberus_shell::MultiSurfaceApp;
use cerberus_style::StyleEngine;
use cerberus_text::TextEngine;
use cerberus_types::{Color, InstanceId, Rect, Size};
use cerberus_ui::DrivenBadge;
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
    fn fills(
        &self,
        instance: InstanceId,
        kind: FillKind,
        doc: &Document,
        page_host: &str,
    ) -> Vec<(NodeId, String)> {
        match self.profiles.get(&instance) {
            Some(profile) => fill_plan(doc, profile, kind, page_host),
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
    /// Form-control hit boxes from the master's last render. Controls are not
    /// `ElementBox`es, so they are tracked separately and mapped to their node
    /// via the canonical control numbering when clicked.
    master_fields: Vec<FormFieldBox>,
    /// The text field the master last focused (by click), if any — the routing
    /// target for typed characters. `None` when focus is not on a text field.
    focused_target: Option<Target>,
    /// The working value of the focused field, mutated per keystroke and sent
    /// whole on each [`Action::Input`] (so a follower converges in one replay,
    /// and the log coalesces a run of keystrokes into a single entry).
    input_buffer: String,
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
            master_fields: Vec::new(),
            focused_target: None,
            input_buffer: String::new(),
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

    /// The host of the master's current page — the "site" the badge names. Empty
    /// for built-in pages (no host) or before the first navigation.
    pub fn driven_site(&self) -> String {
        self.group
            .master()
            .url()
            .and_then(|u| parse_url(u).ok())
            .map(|p| p.host)
            .filter(|h| !h.is_empty())
            .unwrap_or_default()
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
            self.master_fields = laid.fields;
            // Overlay the owner's "N profiles being driven" badge on the master.
            let badge =
                DrivenBadge::paint(size, self.driven_count(), &self.driven_site(), &self.text);
            self.text.rasterize(&badge, &mut fb);
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
        // Resolve the click to a node, preferring a form control (the specific
        // leaf) over a generic element box. If it is a text field, capture it
        // (seeded with its current value) as the typing focus.
        let (target, focus_value) = {
            let doc = self.group.master().document();
            // Smallest control box under the point → its node, via the canonical
            // control numbering layout shares (`collect_controls`).
            let field_node = self
                .master_fields
                .iter()
                .filter(|f| rect_contains(f.rect, x, y))
                .min_by_key(|f| u64::from(f.rect.w) * u64::from(f.rect.h))
                .and_then(|f| {
                    crate::collect_controls(doc.root())
                        .into_iter()
                        .find(|c| c.id == f.id)
                        .map(|c| c.el.id())
                });
            // Else the innermost generic element (links, blocks).
            let node = field_node.or_else(|| {
                self.master_elements
                    .iter()
                    .filter(|e| rect_contains(e.rect, x, y))
                    .min_by_key(|e| u64::from(e.rect.w) * u64::from(e.rect.h))
                    .map(|e| e.node)
            });
            match node {
                Some(n) => {
                    let focus_value = doc.node(n).filter(|r| is_text_input(*r)).map(input_value);
                    (cerberus_mirror::describe(doc, n), focus_value)
                }
                None => (None, None),
            }
        };
        match target {
            Some(t) => {
                match focus_value {
                    // Clicking a text field focuses it for typing.
                    Some(value) => {
                        self.focused_target = Some(t.clone());
                        self.input_buffer = value;
                    }
                    // Clicking anything else moves focus off any text field.
                    None => self.focused_target = None,
                }
                let _ = self.group.act(Action::Click(t));
                vec![self.group.master_index()]
            }
            None => {
                self.focused_target = None;
                Vec::new()
            }
        }
    }

    fn text_input(&mut self, idx: usize, c: char) -> Vec<usize> {
        // Only the master is typed into; followers replay the resulting Input.
        if idx != self.group.master_index() {
            return Vec::new();
        }
        let Some(target) = self.focused_target.clone() else {
            return Vec::new();
        };
        match c {
            // Backspace / delete edit the working value.
            '\u{8}' | '\u{7f}' => {
                self.input_buffer.pop();
            }
            // No submit/tab routing yet; ignore other control characters.
            c if c.is_control() => return Vec::new(),
            c => self.input_buffer.push(c),
        }
        // Send the whole value; `MirrorGroup::act` coalesces a run of same-target
        // inputs into one log entry, and each follower converges in one replay.
        match self.group.act(Action::Input {
            target,
            text: self.input_buffer.clone(),
        }) {
            Ok(()) => vec![self.group.master_index()],
            Err(_) => Vec::new(),
        }
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

/// Whether `n` is a text-entry control that typed characters should route into:
/// a `<textarea>`, or an `<input>` of a text-like type (the default when absent).
fn is_text_input(n: NodeRef<'_>) -> bool {
    match n.tag() {
        "textarea" => true,
        "input" => matches!(
            n.attr("type").unwrap_or("text"),
            "text" | "search" | "email" | "url" | "tel" | "password" | "number" | ""
        ),
        _ => false,
    }
}

/// The current value of a text control, to seed the typing buffer on focus: a
/// `<textarea>`'s text content, otherwise an `<input>`'s `value` attribute.
fn input_value(n: NodeRef<'_>) -> String {
    match n.tag() {
        "textarea" => n.text_content(),
        _ => n.attr("value").unwrap_or("").to_string(),
    }
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
        // Username is not a secret, so it fills regardless of origin binding.
        assert_eq!(
            provider
                .fills(inst, FillKind::Login, &doc, "any.test")
                .len(),
            1
        );
        assert!(provider
            .fills(other, FillKind::All, &doc, "any.test")
            .is_empty());
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

    /// A page with a text field whose `input` event mirrors the typed value into
    /// `#out`, so a test can observe what the field received.
    struct InputPage;
    impl PageSource for InputPage {
        fn load(&self, _instance: InstanceId, _url: &str) -> Result<Document, String> {
            Ok(parse_html(
                "<html><body><input id=\"user\" value=\"\">\
                 <div id=\"out\">empty</div>\
                 <script>document.getElementById('user').addEventListener('input',\
                 function(e){document.getElementById('out').textContent=e.target.value;});\
                 </script></body></html>",
            ))
        }
    }

    #[test]
    fn master_typing_routes_to_focused_field_and_follower_converges() {
        let master = InstanceId::from_u64_pair(0, 1);
        let follower = InstanceId::from_u64_pair(0, 2);
        let engine = QuickJsEngineFactory.instantiate().unwrap();
        let members = vec![
            (master, "work".to_string()),
            (follower, "personal".to_string()),
        ];
        let group =
            MirrorGroup::new(engine, Box::new(InputPage), members, (800, 600), "ua").unwrap();
        let mut shell = MirrorShell::new(group);

        shell.navigate("https://app.test/").unwrap();
        let size = Size::new(800, 600);
        shell.render(0, size);

        // Click across the top until a click focuses the text field — i.e. the
        // next keystroke routes as an `Action::Input` instead of being dropped.
        let mut focused = false;
        'scan: for gy in (0..60).step_by(3) {
            for gx in (0..400).step_by(3) {
                shell.pointer_down(0, gx, gy);
                let before = shell.group().log().len();
                shell.text_input(0, 'h');
                if shell.group().log().len() > before
                    && matches!(
                        shell.group().log().actions().last(),
                        Some(Action::Input { .. })
                    )
                {
                    focused = true;
                    break 'scan;
                }
            }
        }
        assert!(
            focused,
            "clicking the text field routed a keystroke to an Input"
        );

        // A second keystroke coalesces with the first into a single log entry.
        shell.text_input(0, 'i');
        let inputs = shell
            .group()
            .log()
            .actions()
            .iter()
            .filter(|a| matches!(a, Action::Input { .. }))
            .count();
        assert_eq!(inputs, 1, "consecutive keystrokes coalesce into one Input");
        assert_eq!(
            shell.group().log().actions().last(),
            Some(&Action::Input {
                target: Target::Id("user".into()),
                text: "hi".into(),
            })
        );
        assert_eq!(
            shell.group().master().text_of_id("out").as_deref(),
            Some("hi"),
            "the master's input handler saw the typed value"
        );

        // The follower replays the single Input in its own session and converges.
        shell.focus(1);
        assert_eq!(
            shell
                .group()
                .instance(1)
                .unwrap()
                .text_of_id("out")
                .as_deref(),
            Some("hi"),
            "the follower typed the same value in its own session"
        );
        assert!(shell.group().live_realms() <= 1);
    }

    #[test]
    fn driven_site_reports_master_host_and_master_render_composites_badge() {
        let master = InstanceId::from_u64_pair(0, 1);
        let follower = InstanceId::from_u64_pair(0, 2);
        let engine = QuickJsEngineFactory.instantiate().unwrap();
        let members = vec![
            (master, "work".to_string()),
            (follower, "personal".to_string()),
        ];
        let group =
            MirrorGroup::new(engine, Box::new(InputPage), members, (800, 600), "ua").unwrap();
        let mut shell = MirrorShell::new(group);

        assert_eq!(shell.driven_count(), 2);
        assert_eq!(
            shell.driven_site(),
            "",
            "no site before the first navigation"
        );

        shell.navigate("https://app.test/").unwrap();
        assert_eq!(shell.driven_site(), "app.test");

        // The master render composites the badge overlay without panicking.
        let fb = shell.render(0, Size::new(800, 600));
        assert!(fb.pixel(0, 0).is_some());
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

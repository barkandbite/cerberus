//! Cerberus composition root.
//!
//! This is the *only* place that knows concrete adapters. Every subsystem is
//! reached through its trait; swapping an adapter (e.g. the null JS engine for a
//! real V8 adapter) is a change here and nowhere else. The `render` function
//! drives the full M0 path end-to-end:
//!
//! identities → sealed storage → (built-in) fetch → parse → layout → paint →
//! present, with the consent and farbling seams exercised along the way.

use cerberus_consent::{ConsentPolicy, Decision, DefaultDenyPolicy};
use cerberus_dns_doh::DohResolver;
use cerberus_dom::{parse_trivial, Document, Element, Node};
use cerberus_headless::render_document;
use cerberus_identity::{Head, HeadManager};
use cerberus_js::NullJsEngineFactory;
use cerberus_layout::BlockLayout;
use cerberus_net::{BuiltinHttpClient, HttpClient, Router};
use cerberus_paint::{DisplayItem, DisplayList, Framebuffer, Rasterizer, TextShaper};
use cerberus_shell::{FrameApp, HeadlessSurface, PlatformSurface};
use cerberus_storage::{Cookie, Group, StorageEnvironment};
use cerberus_text::TextEngine;
use cerberus_tls_rustls::RustlsProvider;
use cerberus_types::{Color, HeadId, InstanceId, Origin, Point, RealmId, Rect, Size};
use cerberus_ui::{Toolbar, ToolbarAction};
use cerberus_url::parse as parse_url;

/// What to render and how.
#[derive(Clone, Debug)]
pub struct RenderConfig {
    pub url: String,
    pub viewport: Size,
    pub background: Color,
    /// Headed mode raises consent prompts; headless denies third-party silently.
    pub headed: bool,
    /// Trust the OS root store instead of the bundled roots (for TLS-inspecting
    /// proxies). Off by default.
    pub system_roots: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            url: "cerberus:home".to_string(),
            viewport: Size::new(800, 600),
            background: Color::WHITE,
            headed: false,
            system_roots: false,
        }
    }
}

/// A summary of one render, plus the produced frame.
#[derive(Debug)]
pub struct RenderOutcome {
    pub url: String,
    pub status: u16,
    pub viewport: Size,
    pub content_size: Size,
    pub active_head: String,
    pub engine_name: String,
    pub engines_live: usize,
    pub realms_live: usize,
    pub active_cookies: usize,
    /// Decision for a representative third-party access (demonstrates consent).
    pub third_party_decision: Decision,
    pub framebuffer: Framebuffer,
}

/// Errors surfaced by the composition root.
#[derive(Debug)]
pub enum AppError {
    Url(String),
    Net(String),
    Js(String),
    Io(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Url(m) => write!(f, "url error: {m}"),
            AppError::Net(m) => write!(f, "network error: {m}"),
            AppError::Js(m) => write!(f, "js error: {m}"),
            AppError::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for AppError {}

/// Build the three identities ("heads"): work / personal / throwaway. Each has
/// a distinct sealed instance and a distinct farbling seed.
pub fn default_heads() -> Vec<Head> {
    vec![
        Head::new(
            HeadId::from_u64_pair(0, 1),
            InstanceId::from_u64_pair(0, 0x10),
            "work",
            0x5151_5151_5151_5151,
        ),
        Head::new(
            HeadId::from_u64_pair(0, 2),
            InstanceId::from_u64_pair(0, 0x20),
            "personal",
            0xA2A2_A2A2_A2A2_A2A2,
        ),
        Head::new(
            HeadId::from_u64_pair(0, 3),
            InstanceId::from_u64_pair(0, 0x30),
            "throwaway",
            0x3F3F_3F3F_3F3F_3F3F,
        ),
    ]
}

/// Build the network client: built-in `cerberus:` pages are served locally;
/// `http(s)` goes through our HTTP engine over rustls TLS + Quad9 DoH.
pub fn network_client(system_roots: bool) -> Router {
    let provider = || {
        if system_roots {
            RustlsProvider::with_system_roots().unwrap_or_default()
        } else {
            RustlsProvider::new()
        }
    };
    Router::new(
        Box::new(provider()),
        Box::new(DohResolver::quad9(Box::new(provider()))),
    )
}

/// Run the full render pipeline and return a summary plus the frame.
pub fn render(config: &RenderConfig) -> Result<RenderOutcome, AppError> {
    let url = parse_url(&config.url).map_err(|e| AppError::Url(e.to_string()))?;

    // --- Identities: one engine live at a time, instantiated lazily. ---
    let mut heads = HeadManager::new(default_heads(), Box::new(NullJsEngineFactory));
    let active_instance = heads.active().instance;
    let active_label = heads.active().label.clone();

    // First-party context for this navigation.
    let first_party = url.origin().unwrap_or_else(|| {
        Origin::new(
            url.scheme.clone(),
            url.opaque.clone().unwrap_or_default(),
            None,
        )
    });

    // --- Sealed storage: set a first-party "active" cookie in this head. ---
    let mut storage = StorageEnvironment::with_no_vault();
    storage
        .instance(active_instance)
        .set_cookie(
            &first_party,
            Cookie::host("session", "demo", first_party.host.clone()),
            Group::Active,
        )
        .map_err(|e| AppError::Io(format!("{e:?}")))?;
    let active_cookies = storage
        .instance(active_instance)
        .cookies_for_request(&first_party, &first_party)
        .len();

    // --- Consent: a representative third-party access is denied by default. ---
    let mut consent = DefaultDenyPolicy::new(config.headed);
    let third_party = Origin::new("https", "ads.tracker.net", None);
    let third_party_decision = consent
        .evaluate(active_instance, &third_party, &first_party)
        .decision;

    // --- Fetch: built-in pages locally, http(s) over the real network stack. ---
    let response = if url.is_builtin() {
        BuiltinHttpClient.get(&url)
    } else {
        network_client(config.system_roots).get(&url)
    }
    .map_err(|e| AppError::Net(format!("{e:?}")))?;
    let body = String::from_utf8_lossy(&response.body);
    let document = parse_trivial(&body);

    // --- Toolbar (minimal UI) over the page content, with real fonts. ---
    let text = TextEngine::new();
    let mut toolbar = Toolbar::new(active_label.clone());
    toolbar.url_text = config.url.clone();
    let content = toolbar.content_size(config.viewport);

    // Lay out + paint the page into the content area only.
    let mut layout = BlockLayout::default();
    let page = render_document(
        &document,
        content,
        config.background,
        &mut layout,
        &text,
        &text,
    );

    // Compose: page under the toolbar, toolbar painted on top.
    let mut framebuffer = Framebuffer::new(config.viewport);
    framebuffer.clear(config.background);
    framebuffer.blit(toolbar.content_origin(), &page);
    text.rasterize(&toolbar.paint(config.viewport, &text), &mut framebuffer);

    // --- Present via the platform surface seam (headless capture). ---
    let mut surface = HeadlessSurface::new(config.viewport);
    surface
        .present(&framebuffer)
        .map_err(|e| AppError::Io(format!("{e:?}")))?;

    // --- JS engine seam: instantiate the active head's engine (this also
    // injects the head's farbling prologue) and run a trivial eval. ---
    let base_realm = RealmId(heads.active().id.0);
    let engine = heads.engine().map_err(|e| AppError::Js(format!("{e:?}")))?;
    engine
        .eval(base_realm, "void 0")
        .map_err(|e| AppError::Js(format!("{e:?}")))?;
    let engine_name = engine.name().to_string();
    let realms_live = engine.realm_count();
    let engines_live = heads.engines_live();

    Ok(RenderOutcome {
        url: config.url.clone(),
        status: response.status,
        viewport: config.viewport,
        content_size: content,
        active_head: active_label,
        engine_name,
        engines_live,
        realms_live,
        active_cookies,
        third_party_decision,
        framebuffer: surface.last_frame().cloned().unwrap_or(framebuffer),
    })
}

/// An interactive, single-page browser: one toolbar over one page, with a
/// linear history (Back/Forward), driven by the platform layer via [`FrameApp`].
///
/// Until the network stack lands (M1) it serves the built-in `cerberus:` pages
/// and shows a graceful error page for anything else, so the UI and navigation
/// are fully exercisable now.
pub struct BrowserApp {
    heads: HeadManager,
    storage: StorageEnvironment,
    toolbar: Toolbar,
    text: TextEngine,
    history: Vec<String>,
    index: usize,
    document: Document,
    status: u16,
    settings_open: bool,
    background: Color,
    last_size: Size,
}

impl BrowserApp {
    /// Create a browser on the default heads, showing `cerberus:home`.
    pub fn new() -> Self {
        let heads = HeadManager::new(default_heads(), Box::new(NullJsEngineFactory));
        let label = heads.active().label.clone();
        let mut app = Self {
            heads,
            storage: StorageEnvironment::with_no_vault(),
            toolbar: Toolbar::new(label),
            text: TextEngine::new(),
            history: Vec::new(),
            index: 0,
            document: empty_document(),
            status: 0,
            settings_open: false,
            background: Color::WHITE,
            last_size: Size::new(800, 600),
        };
        app.navigate("cerberus:home");
        app
    }

    /// The active head's label (e.g. "work").
    pub fn active_head(&self) -> &str {
        self.heads.active().label.as_str()
    }

    /// Live JS engines (always 0 or 1 — the memory-first invariant).
    pub fn engines_live(&self) -> usize {
        self.heads.engines_live()
    }

    /// The current page's HTTP status (0 if the load failed locally).
    pub fn status(&self) -> u16 {
        self.status
    }

    fn load(&mut self, input: &str) {
        self.toolbar.url_text = input.to_string();
        self.toolbar.url_focused = false;
        self.toolbar.loading = true;

        self.document = match parse_url(input) {
            Ok(url) => match BuiltinHttpClient.get(&url) {
                Ok(resp) => {
                    self.status = resp.status;
                    if let Some(origin) = first_party_of(&url) {
                        self.set_session_cookie(&origin);
                    }
                    parse_trivial(&String::from_utf8_lossy(&resp.body))
                }
                Err(e) => {
                    self.status = 0;
                    error_document(input, &format!("{e:?}"))
                }
            },
            Err(e) => {
                self.status = 0;
                error_document(input, &e.to_string())
            }
        };

        self.toolbar.loading = false;
        self.update_nav();
    }

    fn set_session_cookie(&mut self, first_party: &Origin) {
        let instance = self.heads.active().instance;
        let _ = self.storage.instance(instance).set_cookie(
            first_party,
            Cookie::host("session", "demo", first_party.host.clone()),
            Group::Active,
        );
    }

    fn navigate(&mut self, input: &str) {
        if !self.history.is_empty() {
            self.history.truncate(self.index + 1);
        }
        self.history.push(input.to_string());
        self.index = self.history.len() - 1;
        self.load(input);
    }

    fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        let url = self.history[self.index].clone();
        self.load(&url);
        true
    }

    fn forward(&mut self) -> bool {
        if self.index + 1 >= self.history.len() {
            return false;
        }
        self.index += 1;
        let url = self.history[self.index].clone();
        self.load(&url);
        true
    }

    fn reload(&mut self) {
        if let Some(url) = self.history.get(self.index).cloned() {
            self.load(&url);
        }
    }

    fn update_nav(&mut self) {
        self.toolbar.can_back = self.index > 0;
        self.toolbar.can_forward = self.index + 1 < self.history.len();
    }

    /// Switch to the next head: tears down the current JS engine and lazily
    /// instantiates the new head's (keeps at most one engine live).
    fn switch_head(&mut self) {
        let next = (self.heads.active_index() + 1) % self.heads.heads().len();
        let _ = self.heads.switch_to(next);
        self.toolbar.head_label = self.heads.active().label.clone();
        let _ = self.heads.engine();
    }

    fn handle(&mut self, action: ToolbarAction) -> bool {
        match action {
            ToolbarAction::Back => self.back(),
            ToolbarAction::Forward => self.forward(),
            ToolbarAction::Reload => {
                self.reload();
                true
            }
            ToolbarAction::Stop => {
                self.toolbar.loading = false;
                true
            }
            ToolbarAction::FocusUrl => {
                self.toolbar.url_focused = true;
                true
            }
            ToolbarAction::Navigate(url) => {
                self.navigate(&url);
                true
            }
            ToolbarAction::SwitchHead => {
                self.switch_head();
                true
            }
            ToolbarAction::OpenSettings => {
                self.settings_open = !self.settings_open;
                true
            }
            ToolbarAction::None => false,
        }
    }
}

impl Default for BrowserApp {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameApp for BrowserApp {
    fn title(&self) -> String {
        format!("Cerberus — {}", self.toolbar.head_label)
    }

    fn render_frame(&mut self, size: Size) -> Framebuffer {
        self.last_size = size;
        let content = self.toolbar.content_size(size);

        let mut layout = BlockLayout::default();
        let page = render_document(
            &self.document,
            content,
            self.background,
            &mut layout,
            &self.text,
            &self.text,
        );

        let mut fb = Framebuffer::new(size);
        fb.clear(self.background);
        fb.blit(self.toolbar.content_origin(), &page);
        self.text
            .rasterize(&self.toolbar.paint(size, &self.text), &mut fb);
        if self.settings_open {
            paint_settings_overlay(&mut fb, size, &self.text, &self.text);
        }
        fb
    }

    fn pointer_down(&mut self, x: i32, y: i32) -> bool {
        if self.settings_open {
            self.settings_open = false;
            return true;
        }
        let action = self.toolbar.hit_test(self.last_size, x, y);
        if action == ToolbarAction::None && self.toolbar.url_focused {
            self.toolbar.url_focused = false;
            return true;
        }
        self.handle(action)
    }

    fn text_input(&mut self, c: char) -> bool {
        if self.toolbar.url_focused {
            self.toolbar.type_char(c);
            return true;
        }
        false
    }

    fn submit(&mut self) -> bool {
        if self.toolbar.url_focused {
            let action = self.toolbar.submit_url();
            return self.handle(action);
        }
        false
    }

    fn backspace(&mut self) -> bool {
        if self.toolbar.url_focused {
            self.toolbar.backspace();
            return true;
        }
        false
    }
}

fn empty_document() -> Document {
    Document {
        root: Element::new("#root"),
    }
}

fn first_party_of(url: &cerberus_url::Url) -> Option<Origin> {
    url.origin().or_else(|| {
        url.opaque
            .as_ref()
            .map(|o| Origin::new(url.scheme.clone(), o.clone(), None))
    })
}

fn error_document(url: &str, message: &str) -> Document {
    let mut body = Element::new("body");
    for (tag, text) in [
        ("h1", "Cannot load page".to_string()),
        ("p", url.to_string()),
        ("p", message.to_string()),
    ] {
        let mut el = Element::new(tag);
        el.children.push(Node::Text(text));
        body.children.push(Node::Element(el));
    }
    let mut root = Element::new("#root");
    root.children.push(Node::Element(body));
    Document { root }
}

/// Paint a simple centered settings panel (placeholder; real settings at M5+).
fn paint_settings_overlay(
    fb: &mut Framebuffer,
    size: Size,
    shaper: &dyn TextShaper,
    raster: &dyn Rasterizer,
) {
    let pw = size.w * 3 / 5;
    let ph = size.h * 3 / 5;
    let px = (size.w.saturating_sub(pw) / 2) as i32;
    let py = (size.h.saturating_sub(ph) / 2) as i32;

    let mut list = DisplayList::new();
    list.push(DisplayItem::Rect {
        rect: Rect::new(px - 1, py - 1, pw + 2, ph + 2),
        color: Color::rgb(0x40, 0x40, 0x40),
    });
    list.push(DisplayItem::Rect {
        rect: Rect::new(px, py, pw, ph),
        color: Color::rgb(0xFA, 0xFA, 0xFA),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(px + 12, py + 20),
        glyphs: shaper.shape("Settings", 22),
        color: Color::BLACK,
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(px + 12, py + 52),
        glyphs: shaper.shape("identities | vault | consent | farbling (coming soon)", 14),
        color: Color::rgb(0x50, 0x50, 0x50),
    });
    raster.rasterize(&list, fb);
}

/// Resident set size in kilobytes, read from `/proc/self/status` (Linux only).
/// Returns `None` on platforms without procfs.
pub fn resident_set_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_builtin_home_end_to_end() {
        let outcome = render(&RenderConfig::default()).expect("render should succeed");
        assert_eq!(outcome.status, 200);
        assert_eq!(outcome.engine_name, "null");
        // Memory-first invariant: never more than one engine live.
        assert_eq!(outcome.engines_live, 1);
        assert_eq!(outcome.realms_live, 1);
        // The first-party cookie is active and attachable.
        assert_eq!(outcome.active_cookies, 1);
        // Third-party access is denied by default in headless mode.
        assert_eq!(outcome.third_party_decision, Decision::Deny);
        // A frame was produced at the requested size.
        assert_eq!(outcome.framebuffer.size, RenderConfig::default().viewport);
    }

    #[test]
    fn browser_opens_on_home_with_lazy_engine() {
        let b = BrowserApp::new();
        assert_eq!(b.status(), 200);
        assert_eq!(b.active_head(), "work");
        assert_eq!(b.engines_live(), 0, "engine must be lazy until used");
        assert!(!b.toolbar.can_back, "no history yet");
    }

    #[test]
    fn browser_navigation_walks_history() {
        let mut b = BrowserApp::new();
        b.navigate("cerberus:about");
        assert!(b.toolbar.can_back);
        assert!(!b.toolbar.can_forward);

        assert!(b.back());
        assert_eq!(b.history[b.index], "cerberus:home");
        assert!(b.toolbar.can_forward);

        assert!(b.forward());
        assert_eq!(b.history[b.index], "cerberus:about");
        assert!(!b.forward(), "already at the front");
    }

    #[test]
    fn browser_unknown_url_shows_error_page_not_crash() {
        let mut b = BrowserApp::new();
        b.navigate("https://example.com/");
        assert_eq!(b.status(), 0);
        assert!(b.document.root.text_content().contains("Cannot load page"));
    }

    #[test]
    fn browser_switch_head_keeps_at_most_one_engine() {
        let mut b = BrowserApp::new();
        b.switch_head();
        assert_eq!(b.active_head(), "personal");
        assert_eq!(b.engines_live(), 1);
        b.switch_head();
        assert_eq!(b.active_head(), "throwaway");
        assert_eq!(b.engines_live(), 1, "never more than one engine");
    }

    #[test]
    fn browser_renders_toolbar_over_page() {
        let mut b = BrowserApp::new();
        let fb = b.render_frame(Size::new(400, 300));
        assert_eq!(fb.size, Size::new(400, 300));
        // Toolbar background near the top.
        assert_eq!(fb.pixel(200, 1), Some(Color::rgb(0xEC, 0xEC, 0xEC)));
        // Page background below the toolbar.
        assert_eq!(fb.pixel(380, 200), Some(Color::WHITE));
    }

    #[test]
    fn browser_url_typing_requires_focus() {
        let mut b = BrowserApp::new();
        assert!(!b.text_input('z'), "ignored until the URL box is focused");
        assert!(b.pointer_down(200, 10), "click focuses the URL box");
        assert!(b.text_input('z'));
    }
}

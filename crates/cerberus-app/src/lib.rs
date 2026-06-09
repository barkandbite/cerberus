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
use cerberus_dom::parse_trivial;
use cerberus_headless::render_document;
use cerberus_identity::{Head, HeadManager};
use cerberus_js::NullJsEngineFactory;
use cerberus_layout::BlockLayout;
use cerberus_net::{BuiltinHttpClient, HttpClient};
use cerberus_paint::{BoxRasterizer, Framebuffer, MonoShaper, Rasterizer};
use cerberus_shell::{HeadlessSurface, PlatformSurface};
use cerberus_storage::{Cookie, Group, StorageEnvironment};
use cerberus_types::{Color, HeadId, InstanceId, Origin, RealmId, Size};
use cerberus_ui::Toolbar;
use cerberus_url::parse as parse_url;

/// What to render and how.
#[derive(Clone, Debug)]
pub struct RenderConfig {
    pub url: String,
    pub viewport: Size,
    pub background: Color,
    /// Headed mode raises consent prompts; headless denies third-party silently.
    pub headed: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            url: "cerberus:home".to_string(),
            viewport: Size::new(800, 600),
            background: Color::WHITE,
            headed: false,
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

/// Run the full M0 render pipeline and return a summary plus the frame.
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

    // --- Fetch (built-in client until M1) and parse. ---
    let response = BuiltinHttpClient
        .get(&url)
        .map_err(|e| AppError::Net(format!("{e:?}")))?;
    let body = String::from_utf8_lossy(&response.body);
    let document = parse_trivial(&body);

    // --- Toolbar (minimal UI) over the page content. ---
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
        &MonoShaper,
        &BoxRasterizer,
    );

    // Compose: page under the toolbar, toolbar painted on top.
    let mut framebuffer = Framebuffer::new(config.viewport);
    framebuffer.clear(config.background);
    framebuffer.blit(toolbar.content_origin(), &page);
    BoxRasterizer.rasterize(
        &toolbar.paint(config.viewport, &MonoShaper),
        &mut framebuffer,
    );

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
}

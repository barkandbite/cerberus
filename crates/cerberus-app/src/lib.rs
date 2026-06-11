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
use cerberus_css::CssEngine;
use cerberus_dns_doh::DohResolver;
use cerberus_dom::{parse_html, Document, DocumentBuilder, NodeRef};
use cerberus_headless::render_document;
use cerberus_identity::{Head, HeadManager};
use cerberus_image::ImageCodec;
use cerberus_js_dom::{run_page_scripts, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{
    BlockLayout, FieldKind, FormFieldBox, FormState, ImageProvider, LayoutEngine, LinkBox, NoForms,
};
use cerberus_net::{
    BuiltinHttpClient, HttpCache, HttpClient, HttpResponse, Router, DEFAULT_USER_AGENT,
};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, Framebuffer, ImageDecoder, Rasterizer, TextShaper,
};
use cerberus_shell::{FrameApp, HeadlessSurface, PlatformSurface, Waker};
use cerberus_storage::{Cookie, Group, StorageEnvironment};
use cerberus_style::{StyleEngine, StyledDom};
use cerberus_text::TextEngine;
use cerberus_tls_rustls::RustlsProvider;
use cerberus_types::{Color, FontStyle, HeadId, InstanceId, Origin, Point, RealmId, Rect, Size};
use cerberus_ui::{Toolbar, ToolbarAction};
use cerberus_url::{join as join_url, parse as parse_url, Url};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

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
    /// Inline page `<script>`s executed against the JS document model (ADR-0008).
    pub scripts_ran: usize,
    pub active_cookies: usize,
    /// `<img>` sub-resources fetched, and how many decoded successfully.
    pub images_requested: usize,
    pub images_decoded: usize,
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

/// Install the PSL-backed registrable-domain matcher into `cerberus-types`
/// so every `Origin::site()` comparison (storage partitioning, consent,
/// cookie-domain validation) uses real eTLD+1. Idempotent.
fn install_psl() {
    cerberus_types::install_registrable_domain(cerberus_consent::psl::registrable_domain);
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
    install_psl();
    let url = parse_url(&config.url).map_err(|e| AppError::Url(e.to_string()))?;

    // --- Identities: one engine live at a time, instantiated lazily. ---
    let mut heads = HeadManager::new(default_heads(), Box::new(QuickJsEngineFactory));
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

    // --- Fetch: built-in pages locally, http(s) over the real network stack.
    // Capture the User-Agent the stack actually presented to this origin (honest
    // by default; the escalated rung if bot management forced it) so the page's
    // `navigator.userAgent` matches the request header exactly. ---
    let (response, active_ua) = if url.is_builtin() {
        let resp = BuiltinHttpClient
            .get(&url)
            .map_err(|e| AppError::Net(format!("{e:?}")))?;
        (resp, DEFAULT_USER_AGENT.to_string())
    } else {
        let client = network_client(config.system_roots);
        let resp = client
            .get(&url)
            .map_err(|e| AppError::Net(format!("{e:?}")))?;
        let ua = client.user_agent_for(&url);
        (resp, ua)
    };
    let body = String::from_utf8_lossy(&response.body);
    let mut document = parse_html(&body);

    // --- JS engine seam: instantiate the active head's engine (this also injects
    // the head's farbling prologue), then run the page's inline scripts (if any)
    // against a JS document model and reconcile their DOM mutations back into a
    // fresh Document — *before* styling/layout/images, so script-built content
    // participates in the render (ADR-0008). A script-less page keeps the realm
    // warm with a trivial eval and pays nothing for the bridge. ---
    let base_realm = RealmId(heads.active().id.0);
    let scripts_ran = document.scripts().len();
    let engine = heads.engine().map_err(|e| AppError::Js(format!("{e:?}")))?;
    if scripts_ran == 0 {
        engine
            .eval(base_realm, "void 0")
            .map_err(|e| AppError::Js(format!("{e:?}")))?;
    } else {
        let env = PageEnv {
            url: config.url.clone(),
            viewport: (config.viewport.w, config.viewport.h),
            user_agent: active_ua,
        };
        document = run_page_scripts(engine, base_realm, &document, document.scripts(), &env)
            .map_err(|e| AppError::Js(format!("{e:?}")))?;
    }
    let engine_name = engine.name().to_string();
    let realms_live = engine.realm_count();
    let engines_live = heads.engines_live();

    let styled = CssEngine::new().style(&document);

    // Fetch + decode this page's images up front (the one-shot path is
    // synchronous; the interactive browser fetches them on its worker). No
    // network client is built when the page has no http(s) images.
    let images = fetch_images_sync(&document, &url, config.system_roots);
    let images_requested = images.len();
    let images_decoded = images
        .values()
        .filter(|s| matches!(s, ImageState::Ready(_)))
        .count();
    let provider = StoreImages {
        base: Some(&url),
        images: &images,
    };

    // --- Toolbar (minimal UI) over the page content, with real fonts. ---
    let text = TextEngine::new();
    let mut toolbar = Toolbar::new(active_label.clone());
    toolbar.url_text = config.url.clone();
    let content = toolbar.content_size(config.viewport);

    // Lay out + paint the page into the content area only.
    let mut layout = BlockLayout::default();
    let page = render_document(
        &styled,
        content,
        config.background,
        &mut layout,
        &text,
        &text,
        &provider,
        &NoForms,
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

    Ok(RenderOutcome {
        url: config.url.clone(),
        status: response.status,
        viewport: config.viewport,
        content_size: content,
        active_head: active_label,
        engine_name,
        engines_live,
        realms_live,
        scripts_ran,
        active_cookies,
        images_requested,
        images_decoded,
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
/// A fetched page handed back from the loader.
#[derive(Clone)]
struct FetchedPage {
    url: String,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The User-Agent the stack presented to this origin (for coherent
    /// `navigator.userAgent`); honest unless this site forced an escalation.
    user_agent: String,
}

/// In-flight navigation bookkeeping.
struct Pending {
    id: u64,
    /// If this load is an https upgrade of an `http` URL, the original URL — so a
    /// failure can offer the risk prompt.
    http_fallback: Option<String>,
}

/// A job for the network worker.
enum Job {
    Page { id: u64, url: String },
    Sub { url: String },
}

/// A completed job (page navigation, or an image sub-resource).
enum Done {
    Page {
        id: u64,
        requested_url: String,
        result: Result<FetchedPage, String>,
    },
    Sub {
        url: String,
        bytes: Result<Vec<u8>, String>,
    },
}

/// Performs page + sub-resource loads off the UI thread. Abstracted so the load
/// state machine is testable without the network (see `FakeLoader` in tests).
trait PageLoader {
    /// Queue a page navigation.
    fn request(&self, id: u64, url: String);
    /// Queue an image sub-resource fetch (absolute URL).
    fn request_subresource(&self, url: String);
    /// Non-blocking poll for a completed job.
    fn try_recv(&mut self) -> Option<Done>;
    /// Receive a waker to notify the UI when a result is ready.
    fn set_waker(&mut self, waker: Arc<dyn Waker>);
}

/// The production loader: a worker thread owning the network client.
struct NetLoader {
    tx: Sender<Job>,
    rx: Receiver<Done>,
    waker: Arc<Mutex<Option<Arc<dyn Waker>>>>,
    _worker: JoinHandle<()>,
}

impl NetLoader {
    fn new(system_roots: bool) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<Job>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Done>();
        let waker: Arc<Mutex<Option<Arc<dyn Waker>>>> = Arc::new(Mutex::new(None));
        let worker_waker = waker.clone();

        let worker = std::thread::spawn(move || {
            // Build the network client (rustls config) once, on the worker.
            let client = network_client(system_roots);
            while let Ok(job) = req_rx.recv() {
                let done = match job {
                    Job::Page { id, url } => {
                        let result = fetch_page(&client, &url);
                        Done::Page {
                            id,
                            requested_url: url,
                            result,
                        }
                    }
                    Job::Sub { url } => {
                        let bytes = fetch_bytes(&client, &url);
                        Done::Sub { url, bytes }
                    }
                };
                if out_tx.send(done).is_err() {
                    break;
                }
                if let Some(w) = worker_waker.lock().unwrap().clone() {
                    w.wake();
                }
            }
        });

        Self {
            tx: req_tx,
            rx: out_rx,
            waker,
            _worker: worker,
        }
    }
}

impl PageLoader for NetLoader {
    fn request(&self, id: u64, url: String) {
        let _ = self.tx.send(Job::Page { id, url });
    }
    fn request_subresource(&self, url: String) {
        let _ = self.tx.send(Job::Sub { url });
    }
    fn try_recv(&mut self) -> Option<Done> {
        self.rx.try_recv().ok()
    }
    fn set_waker(&mut self, waker: Arc<dyn Waker>) {
        *self.waker.lock().unwrap() = Some(waker);
    }
}

fn fetch_page(client: &Router, url: &str) -> Result<FetchedPage, String> {
    let parsed = parse_url(url).map_err(|e| e.to_string())?;
    let resp = client.get(&parsed).map_err(|e| format!("{e:?}"))?;
    let user_agent = client.user_agent_for(&parsed);
    Ok(FetchedPage {
        url: url.to_string(),
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
        user_agent,
    })
}

fn fetch_bytes(client: &Router, url: &str) -> Result<Vec<u8>, String> {
    let parsed = parse_url(url).map_err(|e| e.to_string())?;
    let resp = client.get(&parsed).map_err(|e| format!("{e:?}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("HTTP {}", resp.status));
    }
    Ok(resp.body)
}

/// Synchronously fetch + decode every `<img>` in `document`, keyed by absolute
/// URL. Used by the one-shot [`render`]; the interactive browser fetches images
/// on its worker instead. Returns an empty map — and builds no network client —
/// when the page has no http(s) images.
/// Per-page cap on *decoded* image memory. Images are fetched/decoded in
/// document order, which in block layout runs top-to-bottom — so on an
/// image-heavy page (e.g. apple.com's ~100 hero shots) this keeps the images
/// near the top, where the one-shot viewport actually looks, and defers the
/// off-screen tail the frame would crop away anyway, rather than holding every
/// full-resolution bitmap resident at once. Pages under the cap are unaffected.
///
/// Sized from measurement: decoded image volume costs ~1.4 MB of RSS per image
/// on apple.com, so a 16 MB ceiling (≈8–14 images, comfortably more than a
/// 900px viewport shows) keeps that page at ~61 MB — inside the 64 MB budget —
/// versus ~101 MB unbounded, while leaving light pages untouched.
const IMAGE_DECODE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

fn fetch_images_sync(
    document: &Document,
    base: &Url,
    system_roots: bool,
) -> HashMap<String, ImageState> {
    let mut srcs = Vec::new();
    collect_image_urls(document.root(), &mut srcs);

    let mut urls: Vec<String> = Vec::new();
    for src in srcs {
        let abs = resolve_subresource(Some(base), &src);
        if (abs.starts_with("http://") || abs.starts_with("https://")) && !urls.contains(&abs) {
            urls.push(abs);
        }
    }
    if urls.is_empty() {
        return HashMap::new();
    }

    let codec = ImageCodec::new();
    let client = network_client(system_roots);
    let mut out = HashMap::with_capacity(urls.len());
    let mut decoded_bytes = 0usize;
    for url in urls {
        // Once the decoded-memory budget is spent, defer the remaining
        // (off-screen) images: they aren't fetched or decoded, and lay out as
        // their reserved/placeholder box instead of a resident bitmap.
        if decoded_bytes >= IMAGE_DECODE_BUDGET_BYTES {
            out.insert(url, ImageState::Pending);
            continue;
        }
        let state = match fetch_bytes(&client, &url)
            .and_then(|b| codec.decode(&b).map_err(|e| format!("{e:?}")))
        {
            Ok(img) => {
                decoded_bytes += img.rgba.len();
                ImageState::Ready(Arc::new(img))
            }
            Err(_) => ImageState::Failed,
        };
        out.insert(url, state);
    }
    out
}

/// Normalize a URL-bar entry: keep `cerberus:`/explicit-scheme inputs, otherwise
/// assume `https://`.
fn normalize_url(input: &str) -> String {
    let t = input.trim();
    if t.starts_with("cerberus:") || t.contains("://") {
        t.to_string()
    } else {
        format!("https://{t}")
    }
}

/// State of an image sub-resource in the per-page store.
enum ImageState {
    Pending,
    Ready(Arc<DecodedImage>),
    Failed,
}

/// Image provider over the browser's per-page store. Resolves an element's
/// `src` against the current page URL (which is how the store is keyed).
struct StoreImages<'a> {
    base: Option<&'a Url>,
    images: &'a HashMap<String, ImageState>,
}

impl ImageProvider for StoreImages<'_> {
    fn get(&self, src: &str) -> Option<Arc<DecodedImage>> {
        match self.images.get(&resolve_subresource(self.base, src)) {
            Some(ImageState::Ready(img)) => Some(img.clone()),
            _ => None,
        }
    }
}

fn resolve_subresource(base: Option<&Url>, src: &str) -> String {
    match base {
        Some(b) => join_url(b, src)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| src.to_string()),
        None => src.to_string(),
    }
}

/// Collect `<img>` sources from an element subtree, preferring `data-src` (the
/// real URL behind lazy-loaders) over `src`.
fn collect_image_urls(node: NodeRef<'_>, out: &mut Vec<String>) {
    if node.tag() == "img" {
        if let Some(src) = node.attr("data-src").or_else(|| node.attr("src")) {
            out.push(src.to_string());
        }
    }
    for child in node.children() {
        if child.is_element() {
            collect_image_urls(child, out);
        }
    }
}

/// Live, per-page state of the interactive form controls, keyed by the field id
/// (the 0-based pre-order index over every `<input>`/`<textarea>`/`<select>`/
/// `<button>` — the same numbering layout assigns). A control appears here only
/// once the user touches it; layout renders untouched controls from their DOM
/// defaults. Cleared on every page load (form state is per page).
#[derive(Default)]
struct FormStore {
    /// Edited text of text fields / textareas.
    values: HashMap<u32, String>,
    /// Live checked state of checkboxes / radios.
    checked: HashMap<u32, bool>,
    /// Chosen option index of `<select>`s.
    selected: HashMap<u32, usize>,
}

impl FormStore {
    fn clear(&mut self) {
        self.values.clear();
        self.checked.clear();
        self.selected.clear();
    }
}

impl FormState for FormStore {
    fn value(&self, id: u32) -> Option<&str> {
        self.values.get(&id).map(String::as_str)
    }
    fn checked(&self, id: u32) -> bool {
        self.checked.get(&id).copied().unwrap_or(false)
    }
    fn select_index(&self, id: u32) -> Option<usize> {
        self.selected.get(&id).copied()
    }
}

/// The interactive single-page browser: one toolbar over one page, linear
/// history, background loads, and the https→prompt→block policy.
pub struct BrowserApp {
    heads: HeadManager,
    storage: StorageEnvironment,
    cache: HttpCache,
    loader: Box<dyn PageLoader>,
    toolbar: Toolbar,
    text: TextEngine,
    style_engine: CssEngine,
    image_codec: ImageCodec,
    images: HashMap<String, ImageState>,
    history: Vec<String>,
    index: usize,
    document: Document,
    styled: StyledDom,
    status: u16,
    /// The committed URL of the current page (base for resolving links).
    current_url: Option<Url>,
    /// The User-Agent presented to the current page's origin (honest by default;
    /// the escalated rung if forced). Feeds `navigator.userAgent` so the page's
    /// script-visible identity matches the request header.
    active_ua: String,
    /// The `<title>` of the current page, if any.
    page_title: Option<String>,
    /// Clickable link boxes from the last rendered frame (window coordinates).
    links: Vec<LinkBox>,
    /// Interactive form-control hit boxes from the last frame (window coords).
    form_fields: Vec<FormFieldBox>,
    /// Live form-control state for the current page.
    forms: FormStore,
    /// The currently focused text field/textarea, if any (a field id).
    focused_field: Option<u32>,
    pending: Option<Pending>,
    next_id: u64,
    /// When `Some`, an `http` URL is awaiting the user's risk confirmation.
    insecure_prompt: Option<String>,
    /// Hit region of the "Load anyway" button while the prompt is shown.
    insecure_button: Option<Rect>,
    settings_open: bool,
    background: Color,
    last_size: Size,
}

impl BrowserApp {
    /// Create a browser on the default heads, showing `cerberus:home`.
    pub fn new() -> Self {
        Self::with_loader(Box::new(NetLoader::new(false)))
    }

    /// Like [`new`](Self::new) but trusting the OS root store (for TLS-inspecting
    /// proxies); see `RustlsProvider::with_system_roots`.
    pub fn with_options(system_roots: bool) -> Self {
        Self::with_loader(Box::new(NetLoader::new(system_roots)))
    }

    fn with_loader(loader: Box<dyn PageLoader>) -> Self {
        install_psl();
        let heads = HeadManager::new(default_heads(), Box::new(QuickJsEngineFactory));
        let label = heads.active().label.clone();
        let style_engine = CssEngine::new();
        let styled = style_engine.style(&empty_document());
        let mut app = Self {
            heads,
            storage: StorageEnvironment::with_no_vault(),
            cache: HttpCache::new(),
            loader,
            toolbar: Toolbar::new(label),
            text: TextEngine::new(),
            style_engine,
            image_codec: ImageCodec::new(),
            images: HashMap::new(),
            history: Vec::new(),
            index: 0,
            document: empty_document(),
            styled,
            status: 0,
            current_url: None,
            active_ua: DEFAULT_USER_AGENT.to_string(),
            page_title: None,
            links: Vec::new(),
            form_fields: Vec::new(),
            forms: FormStore::default(),
            focused_field: None,
            pending: None,
            next_id: 1,
            insecure_prompt: None,
            insecure_button: None,
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

    /// Begin loading `url`: built-in pages synchronously; http(s) on the worker,
    /// upgrading `http`→`https` first.
    fn start_load(&mut self, url: &str) {
        self.insecure_prompt = None;
        self.insecure_button = None;
        self.toolbar.url_focused = false;
        // Drop the previous page's images: the store only ever holds the
        // current page's sub-resources (memory is priority #1).
        self.images.clear();
        // Form state is per page: clear edited values, focus, and hit boxes.
        self.forms.clear();
        self.focused_field = None;
        self.form_fields.clear();

        if url.starts_with("cerberus:") {
            self.load_builtin(url);
            return;
        }
        let (target, http_fallback) = if url.starts_with("http://") {
            (
                url.replacen("http://", "https://", 1),
                Some(url.to_string()),
            )
        } else {
            (url.to_string(), None)
        };
        self.dispatch(target, http_fallback);
    }

    /// Serve from cache if fresh, else queue a background fetch.
    fn dispatch(&mut self, target: String, http_fallback: Option<String>) {
        let instance = self.heads.active().instance;
        if let Some(resp) = self.cache.get(instance, &target) {
            self.commit_response(
                &target,
                resp.status,
                &resp.headers,
                &resp.body,
                DEFAULT_USER_AGENT,
                false,
            );
            return;
        }
        self.toolbar.url_text = target.clone();
        self.toolbar.loading = true;
        self.set_document(loading_document(&target));
        let id = self.next_id;
        self.next_id += 1;
        self.pending = Some(Pending { id, http_fallback });
        self.loader.request(id, target);
    }

    fn load_builtin(&mut self, url: &str) {
        match parse_url(url) {
            Ok(u) => match BuiltinHttpClient.get(&u) {
                Ok(resp) => self.commit_response(
                    url,
                    resp.status,
                    &resp.headers,
                    &resp.body,
                    DEFAULT_USER_AGENT,
                    false,
                ),
                Err(e) => self.show_error(url, &format!("{e:?}")),
            },
            Err(e) => self.show_error(url, &e.to_string()),
        }
    }

    /// Set + style the current document (one cascade per page load). Inline page
    /// scripts (if any) run first against the JS document model and their DOM
    /// mutations are reconciled back before styling (ADR-0008).
    fn set_document(&mut self, doc: Document) {
        let doc = self.run_scripts(doc);
        self.page_title = doc.title();
        self.styled = self.style_engine.style(&doc);
        self.document = doc;
    }

    /// Run the document's inline scripts against the active head's engine and
    /// return the reconciled document. Script-less pages return untouched (and
    /// keep the engine lazy); on any bridge failure we fall back to the
    /// unscripted DOM so the page still renders.
    fn run_scripts(&mut self, doc: Document) -> Document {
        if doc.scripts().is_empty() {
            return doc;
        }
        let realm = RealmId(self.heads.active().id.0);
        let env = PageEnv {
            url: self.toolbar.url_text.clone(),
            viewport: (self.last_size.w, self.last_size.h),
            user_agent: self.active_ua.clone(),
        };
        let engine = match self.heads.engine() {
            Ok(engine) => engine,
            Err(_) => return doc,
        };
        // Bind the result before matching so `doc`'s borrows (in the call) end
        // before the `Err` arm moves it.
        let reconciled = run_page_scripts(engine, realm, &doc, doc.scripts(), &env);
        match reconciled {
            Ok(rebuilt) => rebuilt,
            Err(_) => doc,
        }
    }

    fn commit_response(
        &mut self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
        user_agent: &str,
        store_in_cache: bool,
    ) {
        // Record the UA this origin saw, so the page's navigator.userAgent (built
        // in run_scripts → set_document) matches the request header.
        self.active_ua = user_agent.to_string();
        let instance = self.heads.active().instance;
        if store_in_cache {
            self.cache.store(
                instance,
                url,
                &HttpResponse {
                    status,
                    headers: headers.to_vec(),
                    body: body.to_vec(),
                },
            );
        }
        self.status = status;
        self.set_document(parse_html(&String::from_utf8_lossy(body)));
        self.toolbar.url_text = url.to_string();
        self.toolbar.loading = false;
        self.insecure_prompt = None;
        self.current_url = parse_url(url).ok();

        let origin = self.current_url.as_ref().and_then(first_party_of);
        if let Some(origin) = origin {
            self.set_session_cookie(&origin);
        }
        self.request_page_images();
        self.update_nav();
    }

    fn show_error(&mut self, url: &str, message: &str) {
        self.status = 0;
        self.set_document(error_document(url, message));
        self.current_url = parse_url(url).ok();
        self.toolbar.url_text = url.to_string();
        self.toolbar.loading = false;
        self.update_nav();
    }

    /// Apply a completed load. Testable entry point — no network or threads.
    /// Apply a completed page load. Testable entry point — no network or threads.
    fn handle_page(
        &mut self,
        id: u64,
        requested_url: String,
        result: Result<FetchedPage, String>,
    ) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        if id != pending.id {
            return false; // stale: superseded by Stop or a newer navigation
        }
        let http_fallback = pending.http_fallback.clone();
        self.pending = None;
        match result {
            Ok(page) => self.commit_response(
                &page.url,
                page.status,
                &page.headers,
                &page.body,
                &page.user_agent,
                true,
            ),
            Err(err) => match http_fallback {
                Some(http_url) => {
                    // The https upgrade failed; offer the plaintext risk prompt.
                    self.toolbar.loading = false;
                    self.set_document(insecure_prompt_document(&http_url, &err));
                    self.insecure_prompt = Some(http_url);
                }
                None => self.show_error(&requested_url, &err),
            },
        }
        true
    }

    /// Apply a decoded image sub-resource (or its failure) to the store.
    fn handle_subresource(&mut self, url: String, bytes: Result<Vec<u8>, String>) -> bool {
        let state =
            match bytes.and_then(|b| self.image_codec.decode(&b).map_err(|e| format!("{e:?}"))) {
                Ok(img) => ImageState::Ready(Arc::new(img)),
                Err(_) => ImageState::Failed,
            };
        self.images.insert(url, state);
        true // a newly-decoded image changes layout — redraw
    }

    /// Scan the current document for `<img>` sources and queue a background
    /// fetch for each new http(s) image. Lazy-loading hints are ignored — every
    /// image is fetched immediately (speed-first; see the layout `img` path).
    fn request_page_images(&mut self) {
        let mut srcs = Vec::new();
        collect_image_urls(self.document.root(), &mut srcs);
        for src in srcs {
            let abs = resolve_subresource(self.current_url.as_ref(), &src);
            // Only http(s) sub-resources go to the network worker.
            if !(abs.starts_with("http://") || abs.starts_with("https://")) {
                continue;
            }
            // One fetch per distinct URL per page.
            if self.images.contains_key(&abs) {
                continue;
            }
            self.images.insert(abs.clone(), ImageState::Pending);
            self.loader.request_subresource(abs);
        }
    }

    /// Confirm the risk prompt: load the original `http` URL in plaintext.
    fn confirm_insecure(&mut self) {
        if let Some(http_url) = self.insecure_prompt.take() {
            self.dispatch(http_url, None);
        }
    }

    /// The href of the link under `(x, y)`, if any (window coordinates).
    fn link_at(&self, x: i32, y: i32) -> Option<String> {
        self.links
            .iter()
            .find(|l| point_in_rect(l.rect, x, y))
            .map(|l| l.href.clone())
    }

    /// Follow a link, resolving `href` against the current page URL.
    fn open_link(&mut self, href: &str) {
        let target = match &self.current_url {
            Some(base) => join_url(base, href)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.to_string()),
            None => href.to_string(),
        };
        self.navigate(&target);
    }

    /// The form-control hit box under `(x, y)`, if any (window coordinates).
    fn field_at(&self, x: i32, y: i32) -> Option<FormFieldBox> {
        self.form_fields
            .iter()
            .find(|f| point_in_rect(f.rect, x, y))
            .cloned()
    }

    /// Handle a click that landed on form control `field`. Returns true (the
    /// click is always consumed once it hits a control).
    fn click_field(&mut self, field: &FormFieldBox) -> bool {
        match field.kind {
            FieldKind::Text | FieldKind::Textarea => {
                self.focused_field = Some(field.id);
                self.toolbar.url_focused = false;
            }
            FieldKind::Checkbox => {
                let now = !self.forms.checked(field.id);
                self.forms.checked.insert(field.id, now);
                self.focused_field = None;
            }
            FieldKind::Radio => {
                self.check_radio(field.id);
                self.focused_field = None;
            }
            FieldKind::Select => {
                self.cycle_select(field.id);
                self.focused_field = None;
            }
            FieldKind::Button => {
                self.focused_field = None;
                self.submit_from(field.id);
            }
        }
        true
    }

    /// Check radio `id` and clear every other radio sharing its `name` in the
    /// same enclosing form (mutually-exclusive radio-group behaviour).
    fn check_radio(&mut self, id: u32) {
        let controls = collect_controls(self.document.root());
        let Some(this) = controls.iter().find(|c| c.id == id) else {
            return;
        };
        let name = this.el.attr("name").unwrap_or_default().to_string();
        let group = this.form;
        for c in &controls {
            let is_radio = c.el.tag() == "input"
                && c.el
                    .attr("type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("radio"));
            if is_radio && same_form(c.form, group) && c.el.attr("name").unwrap_or_default() == name
            {
                self.forms.checked.insert(c.id, c.id == id);
            }
        }
    }

    /// Advance a `<select>` to its next option (wrapping). Reads the option count
    /// from the DOM and the current index from the store (or the DOM default).
    fn cycle_select(&mut self, id: u32) {
        let controls = collect_controls(self.document.root());
        let Some(sel) = controls.iter().find(|c| c.id == id) else {
            return;
        };
        let count = count_options(sel.el);
        if count == 0 {
            return;
        }
        let current = self
            .forms
            .select_index(id)
            .unwrap_or_else(|| dom_selected_index(sel.el));
        self.forms.selected.insert(id, (current + 1) % count);
    }

    /// Submit the form enclosing control `id` (or the whole document if the
    /// control has no `<form>` ancestor), as a GET navigation.
    fn submit_from(&mut self, id: u32) {
        let controls = collect_controls(self.document.root());
        let Some(this) = controls.iter().find(|c| c.id == id) else {
            return;
        };
        // The enclosing <form> (if any) supplies the action/method; its absence
        // means the whole document is treated as one big form.
        let form_el = this.form;
        let query = build_query(&controls, form_el, &self.forms);
        let action = form_el.and_then(|f| f.attr("action")).unwrap_or("");
        // Method: GET today; POST falls back to a GET of the action.
        // TODO POST: send the body instead of a query once the net layer allows.
        let target = self.resolve_action(action, &query);
        self.navigate(&target);
    }

    /// Resolve a form `action` against the current URL and append `?query`.
    fn resolve_action(&self, action: &str, query: &str) -> String {
        let base = match &self.current_url {
            Some(base) if !action.is_empty() => join_url(base, action)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| action.to_string()),
            Some(base) => base.to_string(),
            None if !action.is_empty() => action.to_string(),
            None => String::new(),
        };
        // Replace any existing query with the form's serialized controls.
        let stem = base.split('#').next().unwrap_or(&base);
        let stem = stem.split('?').next().unwrap_or(stem);
        if query.is_empty() {
            stem.to_string()
        } else {
            format!("{stem}?{query}")
        }
    }

    /// Paint a 1px caret at the end of the focused text field's value into the
    /// page framebuffer. `origin` is the page's top-left in window coordinates,
    /// used to map the (window-space) field rect back into page-local pixels.
    ///
    /// The caret font size is recovered from a single-line text field's box
    /// height (`font_size + 2*FIELD_PAD`); a multi-row `<textarea>` reuses one
    /// line of that, which is exact for the single-line editing we support today
    /// (newlines can't be typed — Enter submits). The caret is always clamped
    /// inside the box.
    fn paint_caret(&self, page: &mut Framebuffer, origin: Point) {
        let Some(id) = self.focused_field else {
            return;
        };
        let Some(field) = self.form_fields.iter().find(|f| f.id == id) else {
            return;
        };
        if !matches!(field.kind, FieldKind::Text | FieldKind::Textarea) {
            return;
        }
        // Map the field rect back into page-local coordinates.
        let rect = field.rect;
        let lx = rect.x - origin.x;
        let ly = rect.y - origin.y;
        // A single-line field is font_size + 2*FIELD_PAD high; a textarea is
        // taller, so cap the caret to one line height there.
        let box_inner = (rect.h as i32 - 2 * FIELD_PAD).max(8);
        let px = if field.kind == FieldKind::Textarea {
            box_inner.min(20) as u32
        } else {
            box_inner as u32
        };
        // Width of the current value up to the caret (the last line for areas).
        let value = self.forms.value(id).unwrap_or("");
        let last_line = value.rsplit('\n').next().unwrap_or(value);
        let text_w: u32 = self
            .text
            .shape(last_line, px)
            .iter()
            .map(|g| g.advance)
            .sum();
        let inner_w = (rect.w as i32 - 2 * FIELD_PAD).max(0);
        let caret_x = lx + FIELD_PAD + (text_w as i32).min(inner_w);
        let mut list = DisplayList::new();
        list.push(DisplayItem::Rect {
            rect: Rect::new(caret_x, ly + FIELD_PAD, 1, px),
            color: Color::rgb(0x22, 0x22, 0x22),
        });
        self.text.rasterize(&list, page);
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
        let url = normalize_url(input);
        if !self.history.is_empty() {
            self.history.truncate(self.index + 1);
        }
        self.history.push(url.clone());
        self.index = self.history.len() - 1;
        self.start_load(&url);
    }

    fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        let url = self.history[self.index].clone();
        self.start_load(&url);
        true
    }

    fn forward(&mut self) -> bool {
        if self.index + 1 >= self.history.len() {
            return false;
        }
        self.index += 1;
        let url = self.history[self.index].clone();
        self.start_load(&url);
        true
    }

    fn reload(&mut self) {
        if let Some(url) = self.history.get(self.index).cloned() {
            self.start_load(&url);
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
                // Cancel the in-flight load: drop the pending id so its result
                // is ignored when it arrives.
                self.pending = None;
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
        match &self.page_title {
            Some(t) => format!("{t} — Cerberus ({})", self.toolbar.head_label),
            None => format!("Cerberus — {}", self.toolbar.head_label),
        }
    }

    fn set_waker(&mut self, waker: Arc<dyn Waker>) {
        self.loader.set_waker(waker);
    }

    fn poll(&mut self) -> bool {
        let mut redraw = false;
        while let Some(done) = self.loader.try_recv() {
            redraw |= match done {
                Done::Page {
                    id,
                    requested_url,
                    result,
                } => self.handle_page(id, requested_url, result),
                Done::Sub { url, bytes } => self.handle_subresource(url, bytes),
            };
        }
        redraw
    }

    fn render_frame(&mut self, size: Size) -> Framebuffer {
        self.last_size = size;
        let content = self.toolbar.content_size(size);
        let origin = self.toolbar.content_origin();

        let provider = StoreImages {
            base: self.current_url.as_ref(),
            images: &self.images,
        };
        let mut layout = BlockLayout::default();
        let laid = layout.layout(&self.styled, content, &self.text, &provider, &self.forms);

        let mut page = Framebuffer::new(content);
        page.clear(self.background);
        self.text.rasterize(&laid.display, &mut page);

        // Record link hit-boxes in window coordinates for click handling.
        self.links = laid
            .links
            .into_iter()
            .map(|mut l| {
                l.rect.x += origin.x;
                l.rect.y += origin.y;
                l
            })
            .collect();

        // Record form-control hit-boxes in window coordinates too.
        self.form_fields = laid
            .fields
            .into_iter()
            .map(|mut f| {
                f.rect.x += origin.x;
                f.rect.y += origin.y;
                f
            })
            .collect();

        // Draw a caret at the end of the focused text field's value. The field's
        // own value is already painted by layout into `page`; we just add the bar.
        self.paint_caret(&mut page, origin);

        let mut fb = Framebuffer::new(size);
        fb.clear(self.background);
        fb.blit(origin, &page);
        self.text
            .rasterize(&self.toolbar.paint(size, &self.text), &mut fb);
        if self.insecure_prompt.is_some() {
            self.insecure_button = Some(paint_insecure_button(&mut fb, &self.text));
        }
        if self.settings_open {
            paint_settings_overlay(&mut fb, size, &self.text, &self.text);
        }
        fb
    }

    fn pointer_down(&mut self, x: i32, y: i32) -> bool {
        if self.insecure_prompt.is_some() {
            if let Some(button) = self.insecure_button {
                if point_in_rect(button, x, y) {
                    self.confirm_insecure();
                    return true;
                }
            }
        }
        if self.settings_open {
            self.settings_open = false;
            return true;
        }
        // Page-area click: a form control wins over a link, which wins over
        // plain content. A click anywhere in the page that misses every control
        // also drops form focus (and is consumed if it actually had focus).
        if y >= cerberus_ui::TOOLBAR_HEIGHT as i32 {
            if let Some(field) = self.field_at(x, y) {
                return self.click_field(&field);
            }
            let had_focus = self.focused_field.take().is_some();
            if let Some(href) = self.link_at(x, y) {
                self.open_link(&href);
                return true;
            }
            if had_focus {
                return true; // the click dismissed the focused field
            }
        }
        let action = self.toolbar.hit_test(self.last_size, x, y);
        if action == ToolbarAction::None && self.toolbar.url_focused {
            self.toolbar.url_focused = false;
            return true;
        }
        self.handle(action)
    }

    fn text_input(&mut self, c: char) -> bool {
        // The URL box takes priority while it is focused.
        if self.toolbar.url_focused {
            self.toolbar.type_char(c);
            return true;
        }
        // Otherwise type into the focused text field/textarea.
        if let Some(id) = self.focused_field {
            if !c.is_control() {
                self.forms.values.entry(id).or_default().push(c);
            }
            return true;
        }
        false
    }

    fn submit(&mut self) -> bool {
        if self.toolbar.url_focused {
            let action = self.toolbar.submit_url();
            return self.handle(action);
        }
        // Enter in a focused field submits its enclosing form.
        if let Some(id) = self.focused_field {
            self.submit_from(id);
            return true;
        }
        false
    }

    fn backspace(&mut self) -> bool {
        if self.toolbar.url_focused {
            self.toolbar.backspace();
            return true;
        }
        if let Some(id) = self.focused_field {
            if let Some(v) = self.forms.values.get_mut(&id) {
                v.pop();
            }
            return true;
        }
        false
    }
}

fn empty_document() -> Document {
    let mut b = DocumentBuilder::new();
    let root = b.element("#root", []);
    b.finish(root)
}

fn first_party_of(url: &cerberus_url::Url) -> Option<Origin> {
    url.origin().or_else(|| {
        url.opaque
            .as_ref()
            .map(|o| Origin::new(url.scheme.clone(), o.clone(), None))
    })
}

fn error_document(url: &str, message: &str) -> Document {
    let mut b = DocumentBuilder::new();
    let mut kids = Vec::new();
    for (tag, text) in [
        ("h1", "Cannot load page".to_string()),
        ("p", url.to_string()),
        ("p", message.to_string()),
    ] {
        let t = b.text(text);
        kids.push(b.element(tag, [t]));
    }
    let body = b.element("body", kids);
    let root = b.element("#root", [body]);
    b.finish(root)
}

fn loading_document(url: &str) -> Document {
    simple_document("Loading…", url, None)
}

fn insecure_prompt_document(http_url: &str, error: &str) -> Document {
    simple_document(
        "This site doesn't support HTTPS",
        http_url,
        Some(&format!(
            "HTTPS failed ({error}). Loading over plaintext http is not private. \
             Click \"Load anyway (insecure)\" below to proceed, or enter a different address."
        )),
    )
}

fn simple_document(heading: &str, line: &str, note: Option<&str>) -> Document {
    let mut b = DocumentBuilder::new();
    let mut kids = Vec::new();
    for (tag, text) in [("h1", heading.to_string()), ("p", line.to_string())] {
        let t = b.text(text);
        kids.push(b.element(tag, [t]));
    }
    if let Some(n) = note {
        let t = b.text(n.to_string());
        kids.push(b.element("p", [t]));
    }
    let body = b.element("body", kids);
    let root = b.element("#root", [body]);
    b.finish(root)
}

fn point_in_rect(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32
}

// --- Form controls: the id convention + GET submission. ---

/// Inner padding of a form control, mirroring `cerberus_layout::FIELD_PAD`. Used
/// only to place the focus caret relative to a field's rect.
const FIELD_PAD: i32 = 4;

/// One interactive control located in the DOM, tagged with its field id (the
/// 0-based pre-order index matching layout's numbering) and its enclosing
/// `<form>` element, if any.
struct ControlRef<'a> {
    id: u32,
    el: NodeRef<'a>,
    form: Option<NodeRef<'a>>,
}

/// Whether `tag` is a control that consumes a field id (the same set layout
/// counts: every `<input>`/`<textarea>`/`<select>`/`<button>`).
fn is_control_tag(tag: &str) -> bool {
    matches!(tag, "input" | "textarea" | "select" | "button")
}

/// Walk the document in pre-order, assigning each control its field id and
/// recording its nearest enclosing `<form>`. This is the *single canonical*
/// numbering the app shares with layout, so a clicked box maps to the right
/// control and submission groups controls by their real form.
fn collect_controls(root: NodeRef<'_>) -> Vec<ControlRef<'_>> {
    let mut out = Vec::new();
    let mut next_id = 0u32;
    walk_controls(root, None, &mut next_id, &mut out);
    out
}

fn walk_controls<'a>(
    el: NodeRef<'a>,
    form: Option<NodeRef<'a>>,
    next_id: &mut u32,
    out: &mut Vec<ControlRef<'a>>,
) {
    if is_control_tag(el.tag()) {
        out.push(ControlRef {
            id: *next_id,
            el,
            form,
        });
        *next_id += 1;
    }
    // Descend; controls inside a <form> inherit it as their enclosing form.
    let inner_form = if el.tag() == "form" { Some(el) } else { form };
    for child in el.children() {
        if child.is_element() {
            walk_controls(child, inner_form, next_id, out);
        }
    }
}

/// Whether two optional form refs denote the same `<form>` element (or both the
/// implicit "no form" group).
fn same_form(a: Option<NodeRef<'_>>, b: Option<NodeRef<'_>>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.id() == y.id(),
        (None, None) => true,
        _ => false,
    }
}

/// Number of `<option>` descendants of a `<select>`.
fn count_options(select: NodeRef<'_>) -> usize {
    let mut n = 0;
    count_options_into(select, &mut n);
    n
}

fn count_options_into(el: NodeRef<'_>, n: &mut usize) {
    for child in el.children() {
        if child.is_element() {
            match child.tag() {
                "option" => *n += 1,
                "optgroup" => count_options_into(child, n),
                _ => {}
            }
        }
    }
}

/// The DOM-selected option index of a `<select>` (the first `selected` option,
/// else 0).
fn dom_selected_index(select: NodeRef<'_>) -> usize {
    let mut options = Vec::new();
    collect_option_pairs(select, &mut options);
    options
        .iter()
        .position(|(_, _, selected)| *selected)
        .unwrap_or(0)
}

/// Flatten a `<select>`'s options to `(value, text, selected)` triples, where
/// `value` is the option's `value` attr or its text when absent.
fn collect_option_pairs(el: NodeRef<'_>, out: &mut Vec<(String, String, bool)>) {
    for child in el.children() {
        if child.is_element() {
            match child.tag() {
                "option" => {
                    let text = child.text_content().trim().to_string();
                    let value = child
                        .attr("value")
                        .map(str::to_string)
                        .unwrap_or(text.clone());
                    out.push((value, text, child.attr("selected").is_some()));
                }
                "optgroup" => collect_option_pairs(child, out),
                _ => {}
            }
        }
    }
}

/// Serialize the successful controls of one form (identified by `form` — `None`
/// means the implicit whole-document form) into a `name=value&...` query string,
/// reading live edits from `store` and falling back to DOM defaults.
fn build_query(
    controls: &[ControlRef<'_>],
    form: Option<NodeRef<'_>>,
    store: &FormStore,
) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for c in controls.iter().filter(|c| same_form(c.form, form)) {
        let Some(name) = c.el.attr("name").filter(|n| !n.is_empty()) else {
            continue; // unnamed controls are never successful
        };
        for value in control_values(c, store) {
            pairs.push(format!(
                "{}={}",
                encode_component(name),
                encode_component(&value)
            ));
        }
    }
    pairs.join("&")
}

/// The submitted value(s) of one control (empty if it is not successful, e.g. an
/// unchecked box or a button).
fn control_values(c: &ControlRef<'_>, store: &FormStore) -> Vec<String> {
    match c.el.tag() {
        "textarea" => vec![store
            .value(c.id)
            .map(str::to_string)
            .unwrap_or_else(|| c.el.text_content().trim_end_matches('\n').to_string())],
        "select" => {
            let mut options = Vec::new();
            collect_option_pairs(c.el, &mut options);
            if options.is_empty() {
                return Vec::new();
            }
            let idx = store
                .select_index(c.id)
                .unwrap_or_else(|| dom_selected_index(c.el))
                .min(options.len() - 1);
            vec![options[idx].0.clone()]
        }
        "button" => Vec::new(), // a <button> is not a submitted value here
        _ => input_values(c, store), // <input>
    }
}

/// The submitted value(s) of an `<input>`.
fn input_values(c: &ControlRef<'_>, store: &FormStore) -> Vec<String> {
    let kind =
        c.el.attr("type")
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string());
    match kind.as_str() {
        // Buttons never contribute their own value on a generic submit.
        "submit" | "reset" | "button" | "image" => Vec::new(),
        "checkbox" | "radio" => {
            // Touched? use the live state; else fall back to the DOM `checked`.
            let on = store
                .checked
                .get(&c.id)
                .copied()
                .unwrap_or_else(|| c.el.attr("checked").is_some());
            if on {
                vec![c.el.attr("value").unwrap_or("on").to_string()]
            } else {
                Vec::new()
            }
        }
        "hidden" => vec![c.el.attr("value").unwrap_or("").to_string()],
        // text, search, email, password, … : live edit, else the DOM value.
        _ => vec![store
            .value(c.id)
            .map(str::to_string)
            .unwrap_or_else(|| c.el.attr("value").unwrap_or("").to_string())],
    }
}

/// Percent-encode one `application/x-www-form-urlencoded` component: spaces
/// become `+`, the unreserved set (`A–Z a–z 0–9 - _ . ~`) passes through, and
/// every other byte is `%`-escaped (uppercase hex).
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex_digit(b >> 4));
                out.push(hex_digit(b & 0x0F));
            }
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

/// Paint the "Load anyway (insecure)" button into the content area; return its
/// hit rect.
fn paint_insecure_button(fb: &mut Framebuffer, text: &TextEngine) -> Rect {
    let rect = Rect::new(12, cerberus_ui::TOOLBAR_HEIGHT as i32 + 96, 240, 32);
    let mut list = DisplayList::new();
    list.push(DisplayItem::Rect {
        rect,
        color: Color::rgb(0xC0, 0x39, 0x2B),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(rect.x + 8, rect.y + 8),
        glyphs: text.shape("Load anyway (insecure)", 16),
        color: Color::WHITE,
        style: FontStyle::REGULAR,
    });
    text.rasterize(&list, fb);
    rect
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
        style: FontStyle::REGULAR,
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(px + 12, py + 52),
        glyphs: shaper.shape("identities | vault | consent | farbling (coming soon)", 14),
        color: Color::rgb(0x50, 0x50, 0x50),
        style: FontStyle::REGULAR,
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
        assert_eq!(outcome.engine_name, "quickjs");
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

    // ---- Hermetic test harness: a fake loader, no network or threads. ----

    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    struct FakeLoader {
        responses: HashMap<String, Result<FetchedPage, String>>,
        images: HashMap<String, Result<Vec<u8>, String>>,
        queue: RefCell<VecDeque<Done>>,
    }

    impl FakeLoader {
        fn new(responses: Vec<(&str, Result<FetchedPage, String>)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(u, r)| (u.to_string(), r))
                    .collect(),
                images: HashMap::new(),
                queue: RefCell::new(VecDeque::new()),
            }
        }

        fn with_images(mut self, images: Vec<(&str, Result<Vec<u8>, String>)>) -> Self {
            self.images = images
                .into_iter()
                .map(|(u, r)| (u.to_string(), r))
                .collect();
            self
        }
    }

    impl PageLoader for FakeLoader {
        fn request(&self, id: u64, url: String) {
            let result = self
                .responses
                .get(&url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no canned response for {url}")));
            self.queue.borrow_mut().push_back(Done::Page {
                id,
                requested_url: url,
                result,
            });
        }
        fn request_subresource(&self, url: String) {
            let bytes = self
                .images
                .get(&url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no canned image for {url}")));
            self.queue.borrow_mut().push_back(Done::Sub { url, bytes });
        }
        fn try_recv(&mut self) -> Option<Done> {
            self.queue.get_mut().pop_front()
        }
        fn set_waker(&mut self, _waker: Arc<dyn Waker>) {}
    }

    fn page(url: &str, status: u16, cache_control: Option<&str>, body: &str) -> FetchedPage {
        let headers = cache_control
            .map(|cc| vec![("Cache-Control".to_string(), cc.to_string())])
            .unwrap_or_default();
        FetchedPage {
            url: url.to_string(),
            status,
            headers,
            body: body.as_bytes().to_vec(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }
    }

    fn fake_app(responses: Vec<(&str, Result<FetchedPage, String>)>) -> BrowserApp {
        BrowserApp::with_loader(Box::new(FakeLoader::new(responses)))
    }

    fn fake_app_img(
        responses: Vec<(&str, Result<FetchedPage, String>)>,
        images: Vec<(&str, Result<Vec<u8>, String>)>,
    ) -> BrowserApp {
        BrowserApp::with_loader(Box::new(FakeLoader::new(responses).with_images(images)))
    }

    /// A small valid PNG, for the image-pipeline tests. Uses the `image` crate
    /// directly — this is dev-only fixture generation; production decoding goes
    /// through the `cerberus-image` adapter behind the `ImageDecoder` seam.
    fn test_png(w: u32, h: u32) -> Vec<u8> {
        use image::{ImageFormat, RgbaImage};
        use std::io::Cursor;
        let img = RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn browser_opens_on_home_with_lazy_engine() {
        let b = fake_app(vec![]);
        assert_eq!(b.status(), 200);
        assert_eq!(b.active_head(), "work");
        assert_eq!(b.engines_live(), 0, "engine must be lazy until used");
        assert!(!b.toolbar.can_back, "no history yet");
    }

    #[test]
    fn browser_navigation_walks_history() {
        let mut b = fake_app(vec![]);
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
    fn browser_loads_real_page_on_a_background_request() {
        let mut b = fake_app(vec![(
            "https://site.test/",
            Ok(page("https://site.test/", 200, None, "<h1>Hello</h1>")),
        )]);
        b.navigate("https://site.test/");
        // The fetch is in flight: loading, with a pending request.
        assert!(b.toolbar.loading);
        assert!(b.pending.is_some());

        assert!(b.poll(), "result drained on poll");
        assert_eq!(b.status(), 200);
        assert!(b.document.root().text_content().contains("Hello"));
        assert!(!b.toolbar.loading);
    }

    #[test]
    fn browser_runs_inline_script_and_reflects_dom_mutation() {
        let mut b = fake_app(vec![(
            "https://script.test/",
            Ok(page(
                "https://script.test/",
                200,
                None,
                "<div id=\"app\">old</div>\
                 <script>document.getElementById('app').textContent = 'new-from-js'</script>",
            )),
        )]);
        b.navigate("https://script.test/");
        assert!(b.poll());
        let text = b.document.root().text_content();
        assert!(
            text.contains("new-from-js"),
            "script mutation missing; got {text:?}"
        );
        assert!(
            !text.contains("old"),
            "original text should be replaced; got {text:?}"
        );
    }

    #[test]
    fn browser_script_can_build_content_and_fire_domcontentloaded() {
        let mut b = fake_app(vec![(
            "https://build.test/",
            Ok(page(
                "https://build.test/",
                200,
                None,
                "<body><ul id=\"list\"></ul>\
                 <script>\
                   document.addEventListener('DOMContentLoaded', function () {\
                     var li = document.createElement('li');\
                     li.textContent = 'built-by-script';\
                     document.getElementById('list').appendChild(li);\
                   });\
                 </script></body>",
            )),
        )]);
        b.navigate("https://build.test/");
        assert!(b.poll());
        // The element is created by a DOMContentLoaded handler — which the bridge
        // fires synchronously after the scripts (speed-first), then reconciles.
        assert!(
            b.document.root().text_content().contains("built-by-script"),
            "DOMContentLoaded-built content missing; got {:?}",
            b.document.root().text_content()
        );
    }

    #[test]
    fn browser_script_innerhtml_is_reparsed_into_the_render() {
        let mut b = fake_app(vec![(
            "https://inner.test/",
            Ok(page(
                "https://inner.test/",
                200,
                None,
                "<body><div id=\"slot\">loading</div>\
                 <script>document.getElementById('slot').innerHTML = \
                   '<h2>Headline</h2><p>From innerHTML</p>'</script></body>",
            )),
        )]);
        b.navigate("https://inner.test/");
        assert!(b.poll());
        // innerHTML is reparsed by our Rust parser at reconcile, so the fragment's
        // elements become real DOM nodes in the rendered document.
        let text = b.document.root().text_content();
        assert!(
            text.contains("Headline"),
            "innerHTML <h2> missing; got {text:?}"
        );
        assert!(
            text.contains("From innerHTML"),
            "innerHTML <p> missing; got {text:?}"
        );
        assert!(
            !text.contains("loading"),
            "placeholder should be replaced; got {text:?}"
        );
    }

    #[test]
    fn browser_https_upgrade_then_insecure_prompt_then_proceed() {
        let mut b = fake_app(vec![
            ("https://insecure.test/", Err("UnknownIssuer".to_string())),
            (
                "http://insecure.test/",
                Ok(page("http://insecure.test/", 200, None, "<h1>Plain</h1>")),
            ),
        ]);
        // Entering an http URL upgrades to https first.
        b.navigate("http://insecure.test/");
        assert!(b.poll());
        // https failed -> risk prompt for the original http URL.
        assert_eq!(b.insecure_prompt.as_deref(), Some("http://insecure.test/"));
        assert!(b.document.root().text_content().contains("HTTPS"));

        // Confirming loads the plaintext http page.
        b.confirm_insecure();
        assert!(b.pending.is_some());
        assert!(b.poll());
        assert_eq!(b.status(), 200);
        assert!(b.document.root().text_content().contains("Plain"));
        assert!(b.insecure_prompt.is_none());
    }

    #[test]
    fn browser_cache_serves_repeat_without_a_new_request() {
        let mut b = fake_app(vec![(
            "https://c.test/",
            Ok(page(
                "https://c.test/",
                200,
                Some("max-age=60"),
                "<h1>Cached</h1>",
            )),
        )]);
        b.navigate("https://c.test/");
        assert!(b.poll());
        assert_eq!(b.status(), 200);

        // Second visit is served from the per-instance cache: no pending request.
        b.navigate("https://c.test/");
        assert!(b.pending.is_none(), "served from cache");
        assert!(!b.toolbar.loading);
        assert!(b.document.root().text_content().contains("Cached"));
    }

    #[test]
    fn browser_stop_cancels_the_in_flight_load() {
        let mut b = fake_app(vec![(
            "https://s.test/",
            Ok(page("https://s.test/", 200, None, "x")),
        )]);
        b.navigate("https://s.test/");
        assert!(b.pending.is_some());

        assert!(b.handle(ToolbarAction::Stop));
        assert!(b.pending.is_none());
        assert!(!b.toolbar.loading);
        // The late result is ignored.
        assert!(!b.poll(), "stale outcome dropped after Stop");
    }

    #[test]
    fn browser_switch_head_keeps_at_most_one_engine() {
        let mut b = fake_app(vec![]);
        b.switch_head();
        assert_eq!(b.active_head(), "personal");
        assert_eq!(b.engines_live(), 1);
        b.switch_head();
        assert_eq!(b.active_head(), "throwaway");
        assert_eq!(b.engines_live(), 1, "never more than one engine");
    }

    #[test]
    fn browser_renders_toolbar_over_page() {
        let mut b = fake_app(vec![]);
        let fb = b.render_frame(Size::new(400, 300));
        assert_eq!(fb.size, Size::new(400, 300));
        assert_eq!(fb.pixel(200, 1), Some(Color::rgb(0xEC, 0xEC, 0xEC)));
        assert_eq!(fb.pixel(380, 200), Some(Color::WHITE));
    }

    #[test]
    fn browser_url_typing_requires_focus() {
        let mut b = fake_app(vec![]);
        assert!(!b.text_input('z'), "ignored until the URL box is focused");
        assert!(b.pointer_down(200, 10), "click focuses the URL box");
        assert!(b.text_input('z'));
    }

    #[test]
    fn browser_follows_a_link() {
        let mut b = fake_app(vec![
            (
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<p><a href=\"/next\">go</a></p>",
                )),
            ),
            (
                "https://site.test/next",
                Ok(page("https://site.test/next", 200, None, "<h1>Next</h1>")),
            ),
        ]);
        b.navigate("https://site.test/");
        assert!(b.poll());

        // Render to populate link hit-boxes, then click the first link.
        b.render_frame(Size::new(800, 600));
        assert!(!b.links.is_empty(), "link box present");
        let r = b.links[0].rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "click hits the link");
        assert!(b.pending.is_some(), "navigation started");

        assert!(b.poll());
        assert!(b.document.root().text_content().contains("Next"));
        assert_eq!(b.toolbar.url_text, "https://site.test/next");
    }

    #[test]
    fn browser_fetches_decodes_and_serves_page_images() {
        let png = test_png(6, 4);
        let mut b = fake_app_img(
            vec![(
                "https://img.test/",
                Ok(page(
                    "https://img.test/",
                    200,
                    None,
                    // Same src twice: must dedup to a single fetch.
                    "<img src=\"/pic.png\"><img src=\"/pic.png\">",
                )),
            )],
            vec![("https://img.test/pic.png", Ok(png))],
        );
        b.navigate("https://img.test/");
        // One poll drains the page *and* the image sub-resource it queued.
        assert!(b.poll());

        // Deduped to a single fetch, decoded and stored Ready.
        assert_eq!(b.images.len(), 1);
        assert!(matches!(
            b.images.get("https://img.test/pic.png"),
            Some(ImageState::Ready(_))
        ));

        // The provider the renderer builds resolves the element's `src` against
        // the page URL and hands layout the decoded image.
        let provider = StoreImages {
            base: b.current_url.as_ref(),
            images: &b.images,
        };
        assert!(
            provider.get("/pic.png").is_some(),
            "provider supplies the decoded image to layout"
        );
        // A frame renders without panicking now that an Image item is present.
        b.render_frame(Size::new(800, 600));
    }

    #[test]
    fn browser_skips_non_http_images_and_records_decode_failures() {
        let mut b = fake_app_img(
            vec![(
                "https://img.test/",
                Ok(page(
                    "https://img.test/",
                    200,
                    None,
                    "<img src=\"data:image/png;base64,AAAA\"><img src=\"/broken.png\">",
                )),
            )],
            vec![("https://img.test/broken.png", Ok(b"not a png".to_vec()))],
        );
        b.navigate("https://img.test/");
        assert!(b.poll());
        // The `data:` URL is never fetched; only the http(s) image is, and its
        // garbage bytes are recorded as a decode failure (not left Pending).
        assert_eq!(b.images.len(), 1);
        assert!(matches!(
            b.images.get("https://img.test/broken.png"),
            Some(ImageState::Failed)
        ));
    }

    #[test]
    fn navigation_clears_the_previous_pages_images() {
        let png = test_png(2, 2);
        let mut b = fake_app_img(
            vec![
                (
                    "https://a.test/",
                    Ok(page("https://a.test/", 200, None, "<img src=\"/x.png\">")),
                ),
                (
                    "https://b.test/",
                    Ok(page("https://b.test/", 200, None, "<h1>no images</h1>")),
                ),
            ],
            vec![("https://a.test/x.png", Ok(png))],
        );
        b.navigate("https://a.test/");
        assert!(b.poll());
        assert_eq!(b.images.len(), 1);

        // Leaving the page drops its images (memory is bounded to one page).
        b.navigate("https://b.test/");
        assert!(b.poll());
        assert!(b.images.is_empty(), "previous page's images were cleared");
    }

    // ---- Form interactivity ----

    /// Load `url` into a fresh app, draining the background fetch.
    fn loaded(responses: Vec<(&str, Result<FetchedPage, String>)>, url: &str) -> BrowserApp {
        let mut b = fake_app(responses);
        b.navigate(url);
        assert!(b.poll(), "page load drained");
        b
    }

    #[test]
    fn typing_into_a_focused_text_field_updates_the_store() {
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/s'><input name='q'></form>",
                )),
            )],
            "https://site.test/",
        );
        // Render to populate the field hit-boxes, then focus the field.
        b.render_frame(Size::new(800, 600));
        assert_eq!(b.form_fields.len(), 1, "one text field laid out");
        let id = b.form_fields[0].id;
        let r = b.form_fields[0].rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "click focuses the field");
        assert_eq!(b.focused_field, Some(id));
        assert!(!b.toolbar.url_focused, "URL box defocused on field click");

        // Typing flows into the store keyed by the field id.
        assert!(b.text_input('h'));
        assert!(b.text_input('i'));
        assert_eq!(b.forms.value(id), Some("hi"));

        // Backspace pops; a clicked-away pointer drops focus.
        assert!(b.backspace());
        assert_eq!(b.forms.value(id), Some("h"));
        assert!(b.pointer_down(r.x + 1, r.y + 200), "click off the field");
        assert_eq!(b.focused_field, None);
    }

    #[test]
    fn submitting_a_text_field_navigates_with_an_encoded_query() {
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/s'><input name='q'></form>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let r = b.form_fields[0].rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1));
        assert!(b.text_input('h'));
        assert!(b.text_input('i'));

        // Enter submits the enclosing form: GET to action?name=value.
        assert!(b.submit(), "submit consumed");
        assert!(b.pending.is_some(), "navigation started");
        assert_eq!(b.toolbar.url_text, "https://site.test/s?q=hi");
    }

    #[test]
    fn submit_button_click_submits_the_form() {
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/go'><input name='q' value='a b'>\
                     <input type='submit' value='Send'></form>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        // Two controls: the text field (id 0) and the submit button (id 1).
        let submit = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Button))
            .expect("submit button box")
            .rect;
        assert!(b.pointer_down(submit.x + 1, submit.y + 1), "click submit");
        // The DOM value "a b" round-trips through the encoder (space -> +).
        assert_eq!(b.toolbar.url_text, "https://site.test/go?q=a+b");
    }

    /// A page with a single checkbox `name='a' value='1'` plus a submit button.
    fn checkbox_page() -> Vec<(&'static str, Result<FetchedPage, String>)> {
        vec![(
            "https://site.test/",
            Ok(page(
                "https://site.test/",
                200,
                None,
                "<form action='/s'><input type='checkbox' name='a' value='1'>\
                 <input type='submit'></form>",
            )),
        )]
    }

    #[test]
    fn checkbox_click_toggles_its_checked_state() {
        let mut b = loaded(checkbox_page(), "https://site.test/");
        b.render_frame(Size::new(800, 600));
        let cb = b.form_fields[0].clone();
        assert_eq!(cb.kind, FieldKind::Checkbox);
        assert!(!b.forms.checked(cb.id), "unchecked by default");

        assert!(b.pointer_down(cb.rect.x + 1, cb.rect.y + 1), "toggle on");
        assert!(b.forms.checked(cb.id));
        assert!(b.pointer_down(cb.rect.x + 1, cb.rect.y + 1), "toggle off");
        assert!(!b.forms.checked(cb.id));
    }

    #[test]
    fn checkbox_is_submitted_only_when_checked() {
        // Unchecked: an empty query.
        let mut b = loaded(checkbox_page(), "https://site.test/");
        b.render_frame(Size::new(800, 600));
        let submit = b.form_fields[1].rect;
        assert!(b.pointer_down(submit.x + 1, submit.y + 1));
        assert_eq!(b.toolbar.url_text, "https://site.test/s");

        // Checked: a=1 is included.
        let mut b = loaded(checkbox_page(), "https://site.test/");
        b.render_frame(Size::new(800, 600));
        let cb = b.form_fields[0].rect;
        let submit = b.form_fields[1].rect;
        assert!(b.pointer_down(cb.x + 1, cb.y + 1), "check it");
        assert!(b.pointer_down(submit.x + 1, submit.y + 1));
        assert_eq!(b.toolbar.url_text, "https://site.test/s?a=1");
    }

    #[test]
    fn radio_group_is_mutually_exclusive_in_its_form() {
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/r'>\
                     <input type='radio' name='c' value='x'>\
                     <input type='radio' name='c' value='y'>\
                     <input type='submit'></form>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let x = b.form_fields[0].clone();
        let y = b.form_fields[1].clone();
        let submit = b.form_fields[2].rect;

        assert!(b.pointer_down(x.rect.x + 1, x.rect.y + 1));
        assert!(b.pointer_down(y.rect.x + 1, y.rect.y + 1));
        // Selecting y clears x (same name, same form).
        assert!(!b.forms.checked(x.id));
        assert!(b.forms.checked(y.id));
        assert!(b.pointer_down(submit.x + 1, submit.y + 1));
        assert_eq!(b.toolbar.url_text, "https://site.test/r?c=y");
    }

    #[test]
    fn select_cycles_options_and_submits_the_choice() {
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/s'>\
                     <select name='k'><option value='a'>A</option>\
                     <option value='b'>B</option></select>\
                     <input type='submit'></form>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let sel = b.form_fields[0].clone();
        assert_eq!(sel.kind, FieldKind::Select);
        let submit = b.form_fields[1].rect;

        // Two clicks advance past B and wrap back to A (the store, not a nav).
        assert!(b.pointer_down(sel.rect.x + 1, sel.rect.y + 1));
        assert_eq!(b.forms.select_index(sel.id), Some(1));
        assert!(b.pointer_down(sel.rect.x + 1, sel.rect.y + 1));
        assert_eq!(b.forms.select_index(sel.id), Some(0), "wraps around");

        // One more click selects B, and submitting sends its value.
        assert!(b.pointer_down(sel.rect.x + 1, sel.rect.y + 1));
        assert!(b.pointer_down(submit.x + 1, submit.y + 1));
        assert_eq!(b.toolbar.url_text, "https://site.test/s?k=b");
    }
}

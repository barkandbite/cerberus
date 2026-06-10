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
use cerberus_dom::{parse_html, Document, Element, Node};
use cerberus_headless::render_document;
use cerberus_identity::{Head, HeadManager};
use cerberus_image::ImageCodec;
use cerberus_js::NullJsEngineFactory;
use cerberus_layout::{BlockLayout, ImageProvider, LayoutEngine, LinkBox};
use cerberus_net::{BuiltinHttpClient, HttpCache, HttpClient, HttpResponse, Router};
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
    let document = parse_html(&body);
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
    Ok(FetchedPage {
        url: url.to_string(),
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
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
fn fetch_images_sync(
    document: &Document,
    base: &Url,
    system_roots: bool,
) -> HashMap<String, ImageState> {
    let mut srcs = Vec::new();
    collect_image_urls(&document.root, &mut srcs);

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
    urls.into_iter()
        .map(|url| {
            let state = match fetch_bytes(&client, &url)
                .and_then(|b| codec.decode(&b).map_err(|e| format!("{e:?}")))
            {
                Ok(img) => ImageState::Ready(Arc::new(img)),
                Err(_) => ImageState::Failed,
            };
            (url, state)
        })
        .collect()
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
fn collect_image_urls(el: &Element, out: &mut Vec<String>) {
    if el.tag == "img" {
        if let Some(src) = el.attr("data-src").or_else(|| el.attr("src")) {
            out.push(src.to_string());
        }
    }
    for child in &el.children {
        if let Node::Element(e) = child {
            collect_image_urls(e, out);
        }
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
    /// The `<title>` of the current page, if any.
    page_title: Option<String>,
    /// Clickable link boxes from the last rendered frame (window coordinates).
    links: Vec<LinkBox>,
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
        let heads = HeadManager::new(default_heads(), Box::new(NullJsEngineFactory));
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
            page_title: None,
            links: Vec::new(),
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
            self.commit_response(&target, resp.status, &resp.headers, &resp.body, false);
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
                Ok(resp) => {
                    self.commit_response(url, resp.status, &resp.headers, &resp.body, false)
                }
                Err(e) => self.show_error(url, &format!("{e:?}")),
            },
            Err(e) => self.show_error(url, &e.to_string()),
        }
    }

    /// Set + style the current document (one cascade per page load).
    fn set_document(&mut self, doc: Document) {
        self.page_title = doc.title();
        self.styled = self.style_engine.style(&doc);
        self.document = doc;
    }

    fn commit_response(
        &mut self,
        url: &str,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
        store_in_cache: bool,
    ) {
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
            Ok(page) => {
                self.commit_response(&page.url, page.status, &page.headers, &page.body, true)
            }
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
        collect_image_urls(&self.document.root, &mut srcs);
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
        let laid = layout.layout(&self.styled, content, &self.text, &provider);

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
        // Page-area click: follow a link if one is under the cursor.
        if y >= cerberus_ui::TOOLBAR_HEIGHT as i32 {
            if let Some(href) = self.link_at(x, y) {
                self.open_link(&href);
                return true;
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
    let mut body = Element::new("body");
    for (tag, text) in [("h1", heading.to_string()), ("p", line.to_string())] {
        let mut el = Element::new(tag);
        el.children.push(Node::Text(text));
        body.children.push(Node::Element(el));
    }
    if let Some(n) = note {
        let mut el = Element::new("p");
        el.children.push(Node::Text(n.to_string()));
        body.children.push(Node::Element(el));
    }
    let mut root = Element::new("#root");
    root.children.push(Node::Element(body));
    Document { root }
}

fn point_in_rect(r: Rect, x: i32, y: i32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32
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
        assert!(b.document.root.text_content().contains("Hello"));
        assert!(!b.toolbar.loading);
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
        assert!(b.document.root.text_content().contains("HTTPS"));

        // Confirming loads the plaintext http page.
        b.confirm_insecure();
        assert!(b.pending.is_some());
        assert!(b.poll());
        assert_eq!(b.status(), 200);
        assert!(b.document.root.text_content().contains("Plain"));
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
        assert!(b.document.root.text_content().contains("Cached"));
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
        assert!(b.document.root.text_content().contains("Next"));
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
}

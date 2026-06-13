//! Cerberus composition root.
//!
//! This is the *only* place that knows concrete adapters. Every subsystem is
//! reached through its trait; swapping an adapter (e.g. the null JS engine for a
//! real V8 adapter) is a change here and nowhere else. The `render` function
//! drives the full M0 path end-to-end:
//!
//! identities → sealed storage → (built-in) fetch → parse → layout → paint →
//! present, with the consent and farbling seams exercised along the way.

use cerberus_consent::{ConsentEvent, ConsentPolicy, Decision, DefaultDenyPolicy};
use cerberus_crypto::Secret;
use cerberus_crypto_rustcrypto::{Argon2idKdf, XChaCha20Poly1305Aead};
use cerberus_css::CssEngine;
use cerberus_dns_doh::DohResolver;
use cerberus_dom::{parse_html, Document, DocumentBuilder, NodeId, NodeRef};
use cerberus_headless::render_document;
use cerberus_identity::{Head, HeadManager};
use cerberus_image::ImageCodec;
use cerberus_js_dom::{
    dispatch_event, fire_load, install_page, run_page_scripts, serialize_dom, PageEnv, RebuiltDom,
};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{
    BlockLayout, FieldKind, FormFieldBox, FormState, ImageProvider, LayoutEngine, LinkBox, NoForms,
    NoImages,
};
use cerberus_net::{
    parse_proxy, BuiltinHttpClient, CookieJar, FetchContext, FetchKind, HttpCache, HttpClient,
    HttpResponse, ProxyConfig, Router, DEFAULT_USER_AGENT,
};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, Framebuffer, ImageDecoder, Rasterizer, TextShaper,
};
use cerberus_shell::{FrameApp, HeadlessSurface, PlatformSurface, Waker};
use cerberus_storage::{
    atomic_write, parse_set_cookie, random_bytes, CookieDisposition, CookiePolicy, CookieView,
    EncryptedVault, Group, StorageEnvironment, DEFAULT_TIMED_SECS,
};
use cerberus_style::{StyleEngine, StyledDom};
use cerberus_text::TextEngine;
use cerberus_tls_rustls::RustlsProvider;
use cerberus_types::{Color, FontStyle, HeadId, InstanceId, Origin, Point, RealmId, Rect, Size};
use cerberus_ui::{
    BannerAction, ConsentBanner, CookieAction, CookieManager, CookieRow, PerfHud, Toolbar,
    ToolbarAction, BANNER_HEIGHT,
};
use cerberus_url::{join as join_url, parse as parse_url, Url};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod timings;
use timings::Timings;

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
    /// Persistent profile directory. `None` (the default) is fully ephemeral:
    /// nothing touches disk — the privacy default.
    pub data_dir: Option<String>,
    /// Capture the rendered page's text content (automation: `--dump-text`).
    pub dump_text: bool,
    /// Single egress proxy (`host:port`); all traffic tunnels through it.
    pub proxy: Option<String>,
    /// Collect per-stage timings into [`RenderOutcome::timings`] (`--timers`).
    pub timers: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            url: "cerberus:home".to_string(),
            viewport: Size::new(800, 600),
            background: Color::WHITE,
            headed: false,
            system_roots: false,
            data_dir: None,
            dump_text: false,
            proxy: None,
            timers: false,
        }
    }
}

/// Launch options for the interactive browser.
#[derive(Clone, Debug, Default)]
pub struct AppOptions {
    /// Trust the OS root store (TLS-inspecting proxies). Off by default.
    pub system_roots: bool,
    /// Persistent profile directory. `None` (default) = fully ephemeral.
    pub data_dir: Option<PathBuf>,
    /// Single egress proxy (`host:port`); all traffic tunnels through it.
    pub proxy: Option<String>,
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
    /// Decision for a representative third-party access (the default posture).
    pub third_party_decision: Decision,
    /// Subresources refused by the consent policy (third-party, no rule).
    pub subresources_blocked: usize,
    /// The page's text content, when [`RenderConfig::dump_text`] asked for it.
    pub page_text: Option<String>,
    /// Per-stage `(label, milliseconds)` timings, when `--timers` is set (M11).
    pub timings: Vec<(String, f64)>,
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

// ---- Persistent profile (--data-dir): salt, vault, cookies, heads ----

const VAULT_SALT_FILE: &str = "vault.salt";
const HEADS_FILE: &str = "heads.txt";
const CONSENT_RULES_FILE: &str = "consent.rules";
const COOKIES_POLICY_FILE: &str = "cookies.policy";

/// Load the per-cookie disposition policy from a profile dir (default when
/// absent or ephemeral).
fn load_cookie_policy(dir: Option<&Path>) -> CookiePolicy {
    let mut policy = CookiePolicy::new();
    if let Some(dir) = dir {
        if let Ok(text) = std::fs::read_to_string(dir.join(COOKIES_POLICY_FILE)) {
            policy.load(&text);
        }
    }
    policy
}

/// Load the profile's KDF salt, creating a random one on first run.
fn load_or_create_salt(dir: &Path) -> std::io::Result<[u8; 16]> {
    let path = dir.join(VAULT_SALT_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => bytes.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "vault.salt is not 16 bytes",
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let salt: [u8; 16] = random_bytes(16).try_into().expect("16 random bytes");
            atomic_write(&path, &salt)?;
            Ok(salt)
        }
        Err(e) => Err(e),
    }
}

/// Open (or initialize) a profile's sealed storage: XChaCha20-Poly1305 +
/// Argon2id vault (locked until the user unlocks it) over the on-disk
/// cookie partitions.
fn open_profile_storage(dir: &Path) -> std::io::Result<StorageEnvironment> {
    std::fs::create_dir_all(dir)?;
    let salt = load_or_create_salt(dir)?;
    let vault = EncryptedVault::new(
        Box::new(XChaCha20Poly1305Aead::new()),
        Box::new(Argon2idKdf::new()),
        salt,
    );
    StorageEnvironment::load(dir, Box::new(vault))
}

/// A profile's heads: random instance ids + farbling seeds minted on first
/// run (per-profile unlinkability), persisted in a human-auditable text file.
fn fresh_profile_heads() -> Vec<Head> {
    ["work", "personal", "throwaway"]
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let instance_bytes: [u8; 16] = random_bytes(16).try_into().expect("16 random bytes");
            let seed_bytes: [u8; 8] = random_bytes(8).try_into().expect("8 random bytes");
            Head::new(
                HeadId::from_u64_pair(0, i as u64 + 1),
                InstanceId(cerberus_types::Id128::from_bytes(instance_bytes)),
                *label,
                u64::from_le_bytes(seed_bytes),
            )
        })
        .collect()
}

/// Parse `heads.txt`: `cerberus-heads v1`, `active <idx>`, then one
/// `head <head-id> <instance-id> <seed-hex> <label>` line per head.
fn load_heads(dir: &Path) -> Option<(Vec<Head>, usize)> {
    let text = std::fs::read_to_string(dir.join(HEADS_FILE)).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "cerberus-heads v1" {
        return None;
    }
    let mut active = 0usize;
    let mut heads = Vec::new();
    for line in lines {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("active") => active = parts.next()?.parse().ok()?,
            Some("head") => {
                let id = HeadId::from_hex(parts.next()?)?;
                let instance = InstanceId::from_hex(parts.next()?)?;
                let seed = u64::from_str_radix(parts.next()?, 16).ok()?;
                let label = parts.collect::<Vec<_>>().join(" ");
                if label.is_empty() {
                    return None;
                }
                heads.push(Head::new(id, instance, label, seed));
            }
            Some(_) | None => continue,
        }
    }
    if heads.is_empty() || active >= heads.len() {
        return None;
    }
    Some((heads, active))
}

fn save_heads(dir: &Path, heads: &[Head], active: usize) -> std::io::Result<()> {
    use cerberus_farbling::FarblingProvider as _;
    let mut out = String::from("cerberus-heads v1\n");
    out.push_str(&format!("active {active}\n"));
    for h in heads {
        out.push_str(&format!(
            "head {} {} {:016x} {}\n",
            h.id,
            h.instance,
            h.farbling.seed(),
            h.label
        ));
    }
    atomic_write(&dir.join(HEADS_FILE), out.as_bytes())
}

/// The active head's sealed instance for a profile dir (or the first default
/// head when there's no `heads.txt`).
fn profile_active_instance(dir: &Path) -> InstanceId {
    match load_heads(dir) {
        Some((heads, active)) => heads[active].instance,
        None => default_heads()[0].instance,
    }
}

/// Headless cookie administration (`cerberus-app cookies`): list the active
/// head's cookies in a profile and optionally set a disposition. `set` is
/// `NAME=DISP` (e.g. `cart=timed:3600`); `site` is the first-party site key.
/// Returns one display line per cookie. Pure over a `--data-dir` profile, so
/// it is fully testable without a window.
pub fn cookie_admin(
    data_dir: &str,
    site: Option<&str>,
    set: Option<&str>,
) -> Result<Vec<String>, AppError> {
    install_psl();
    let dir = Path::new(data_dir);
    let mut env = open_profile_storage(dir).map_err(|e| AppError::Io(e.to_string()))?;
    let mut policy = load_cookie_policy(Some(dir));
    let instance = profile_active_instance(dir);

    if let Some(set) = set {
        let (name, tok) = set
            .split_once('=')
            .ok_or_else(|| AppError::Io(format!("--set wants NAME=DISP, got {set:?}")))?;
        let disp = CookieDisposition::parse_token(tok)
            .ok_or_else(|| AppError::Io(format!("unknown disposition {tok:?}")))?;
        let site = site.ok_or_else(|| AppError::Io("--set needs --site".into()))?;
        policy.set_override(site, name, disp);
        atomic_write(
            &dir.join(COOKIES_POLICY_FILE),
            policy.serialize().as_bytes(),
        )
        .map_err(|e| AppError::Io(e.to_string()))?;
        env.instance(instance).set_disposition(site, name, disp);
        env.save(dir).map_err(|e| AppError::Io(e.to_string()))?;
    }

    let mut lines: Vec<String> = env
        .instance(instance)
        .cookie_views()
        .into_iter()
        .filter(|v| site.is_none_or(|s| v.fp_site == s))
        .map(|v: CookieView| {
            let exp = v
                .expires
                .map(|t| t.to_string())
                .unwrap_or_else(|| "session".into());
            format!(
                "{}  {}={}  [{}]  exp={}",
                v.fp_site,
                v.name,
                v.value,
                v.disposition.label(),
                exp
            )
        })
        .collect();
    lines.sort();
    Ok(lines)
}

/// Build the network client: built-in `cerberus:` pages are served locally;
/// `http(s)` goes through our HTTP engine over rustls TLS + Quad9 DoH. When a
/// `jar` is supplied, context-carrying fetches attach/capture cookies per hop;
/// with a `proxy`, every connection tunnels through that single egress.
pub fn network_client(
    system_roots: bool,
    jar: Option<Arc<dyn CookieJar>>,
    proxy: Option<ProxyConfig>,
) -> Router {
    let provider = || {
        if system_roots {
            RustlsProvider::with_system_roots().unwrap_or_default()
        } else {
            RustlsProvider::new()
        }
    };
    Router::with_options(
        Box::new(provider()),
        Box::new(DohResolver::quad9(Box::new(provider()))),
        jar,
        proxy,
    )
}

/// The cookie seam over sealed storage: attaches only what
/// `InstanceStore::cookies_for_request` allows (active, in-scope, unexpired,
/// never quarantined) and routes captured `Set-Cookie`s through the consent
/// policy — same-site is the first party's own; cross-site is Allowed
/// (standing rule), Denied (dropped), or Prompted (quarantined pending the
/// user's decision, with the event surfaced in the consent banner).
struct SealedJar {
    storage: Arc<Mutex<StorageEnvironment>>,
    /// The same policy object the UI-thread fetch gating consults.
    policy: Arc<Mutex<DefaultDenyPolicy>>,
    /// Per-cookie disposition policy (Allow/Session/Timed/Block/Allow-once),
    /// applied to accepted cookies on capture and consulted on attach.
    cookies: Arc<Mutex<CookiePolicy>>,
    /// Prompt events raised on the worker, drained by the UI in `poll()`.
    /// Lock discipline: never held while `storage` or `policy` is held.
    events: Arc<Mutex<Vec<ConsentEvent>>>,
}

impl CookieJar for SealedJar {
    fn cookie_header(
        &self,
        instance: InstanceId,
        request: &Url,
        first_party: &Origin,
    ) -> Option<String> {
        let origin = request.origin()?;
        // Cross-site requests only carry cookies under a standing Allow rule
        // (the read path raises no prompts — that happens at capture/fetch).
        if origin.is_third_party_to(first_party) {
            let decision = self
                .policy
                .lock()
                .unwrap()
                .evaluate(instance, &origin, first_party)
                .decision;
            if decision != Decision::Allow {
                return None;
            }
        }
        let mut env = self.storage.lock().unwrap();
        let mut store = env.instance(instance);
        let cookies = store.cookies_for_request(&origin, first_party);
        if cookies.is_empty() {
            return None;
        }
        let header = cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        // Account for Allow-once cookies now that they've been attached.
        store.consume_allow_once(&origin, first_party);
        Some(header)
    }

    fn set_cookie(&self, instance: InstanceId, request: &Url, first_party: &Origin, value: &str) {
        let Some(origin) = request.origin() else {
            return;
        };
        let Some(cookie) = parse_set_cookie(value, &origin.host, request.scheme == "https") else {
            return;
        };
        let outcome = self
            .policy
            .lock()
            .unwrap()
            .evaluate(instance, &origin, first_party);
        let group = match outcome.decision {
            Decision::Allow => Group::Active,
            // Denied: the cookie ceases to exist.
            Decision::Deny => return,
            // Awaiting the user: quarantine. (A locked vault rejects the
            // write, which is still deny — the cookie is simply gone.)
            Decision::Prompt => Group::Quarantined,
        };
        if let Some(event) = outcome.event {
            self.events.lock().unwrap().push(event);
        }
        // For an accepted (Active) cookie, the user's disposition decides its
        // lifetime/persistence (Block drops it entirely). Quarantined cookies
        // keep the default until the user releases them.
        let disposition = if group == Group::Active {
            self.cookies
                .lock()
                .unwrap()
                .resolve(&first_party.site(), &cookie.name)
        } else {
            CookieDisposition::Allow
        };
        let mut env = self.storage.lock().unwrap();
        let _ = env
            .instance(instance)
            .set_cookie_with(first_party, cookie, group, disposition);
    }
}

/// Run the full render pipeline and return a summary plus the frame.
pub fn render(config: &RenderConfig) -> Result<RenderOutcome, AppError> {
    install_psl();
    let mut timings = Timings::new();
    timings.begin_navigation();
    let url = parse_url(&config.url).map_err(|e| AppError::Url(e.to_string()))?;

    // --- Identities: one engine live at a time, instantiated lazily. With a
    // profile, the persisted heads are used (same instances as the interactive
    // browser, so one-shot renders see the same sealed cookies). ---
    let profile_heads = config.data_dir.as_deref().map(Path::new).map(|dir| {
        load_heads(dir).unwrap_or_else(|| {
            let heads = fresh_profile_heads();
            if let Err(e) = save_heads(dir, &heads, 0) {
                eprintln!("cerberus: cannot save heads: {e}");
            }
            (heads, 0)
        })
    });
    let (head_list, active_idx) = profile_heads.unwrap_or_else(|| (default_heads(), 0));
    let mut heads = HeadManager::new(head_list, Box::new(QuickJsEngineFactory));
    if active_idx != 0 {
        let _ = heads.switch_to(active_idx);
    }
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

    // --- Sealed storage behind the cookie seam. Ephemeral by default; with a
    // data dir the profile's cookies load (vault stays locked in one-shot
    // mode, so cross-site cookies are dropped at the quarantine door —
    // default-deny either way). ---
    let storage = Arc::new(Mutex::new(match &config.data_dir {
        Some(dir) => {
            open_profile_storage(Path::new(dir)).map_err(|e| AppError::Io(e.to_string()))?
        }
        None => StorageEnvironment::with_no_vault(),
    }));

    // --- Consent: the policy that gates this page's cookies and subresources.
    // One-shot headless mode denies unruled third parties silently; a profile's
    // standing rules are honored. ---
    let mut policy = DefaultDenyPolicy::new(config.headed);
    if let Some(dir) = &config.data_dir {
        if let Ok(text) = std::fs::read_to_string(Path::new(dir).join(CONSENT_RULES_FILE)) {
            policy.load_rules(&text);
        }
    }
    let consent = Arc::new(Mutex::new(policy));
    let cookie_policy = Arc::new(Mutex::new(load_cookie_policy(
        config.data_dir.as_deref().map(Path::new),
    )));
    let jar: Arc<dyn CookieJar> = Arc::new(SealedJar {
        storage: storage.clone(),
        policy: consent.clone(),
        cookies: cookie_policy.clone(),
        // One-shot renders have no banner; prompt events are dropped.
        events: Arc::new(Mutex::new(Vec::new())),
    });

    // The default posture for a not-yet-ruled third party (what a tracker
    // would get): the same policy object that enforces this page below.
    let third_party = Origin::new("https", "ads.tracker.net", None);
    let third_party_decision = consent
        .lock()
        .unwrap()
        .evaluate(active_instance, &third_party, &first_party)
        .decision;

    // --- Fetch: built-in pages locally, http(s) over the real network stack
    // with the cookie jar attached. Capture the User-Agent the stack actually
    // presented to this origin (honest by default; the escalated rung if bot
    // management forced it) so the page's `navigator.userAgent` matches the
    // request header exactly. ---
    let nav_ctx = FetchContext {
        instance: active_instance,
        kind: FetchKind::Navigation,
    };
    let proxy = match config.proxy.as_deref() {
        // Fail closed: a bad proxy must not fall back to direct connections.
        Some(p) => Some(parse_proxy(p).map_err(|e| AppError::Net(format!("{e:?}")))?),
        None => None,
    };
    let fetch_t = Instant::now();
    let (response, active_ua, client) = if url.is_builtin() {
        let resp = BuiltinHttpClient
            .get(&url)
            .map_err(|e| AppError::Net(format!("{e:?}")))?;
        (resp, DEFAULT_USER_AGENT.to_string(), None)
    } else {
        let client = network_client(config.system_roots, Some(jar.clone()), proxy);
        let resp = client
            .get_in(&url, &nav_ctx)
            .map_err(|e| AppError::Net(format!("{e:?}")))?;
        let ua = client.user_agent_for(&url);
        (resp, ua, Some(client))
    };
    timings.record(format!("GET {}", url.host), fetch_t.elapsed());
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
    let scripts_t = Instant::now();
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
    timings.record("scripts", scripts_t.elapsed());

    let style_t = Instant::now();
    let styled = CssEngine::new().style(&document);
    timings.record("style", style_t.elapsed());

    // Fetch + decode this page's images up front (the one-shot path is
    // synchronous; the interactive browser fetches them on its worker), in the
    // page's subresource context so image fetches carry/capture cookies under
    // the same first party. Built-in pages reference no network images.
    let sub_ctx = FetchContext {
        instance: active_instance,
        kind: FetchKind::Subresource {
            first_party: first_party.clone(),
        },
    };
    let images = match &client {
        Some(client) => {
            fetch_images_sync(&document, &url, client, &sub_ctx, &consent, &first_party)
        }
        None => HashMap::new(),
    };
    let subresources_blocked = images
        .values()
        .filter(|s| matches!(s, ImageState::Blocked))
        .count();
    let images_requested = images.len() - subresources_blocked;
    let images_decoded = images
        .values()
        .filter(|s| matches!(s, ImageState::Ready(_)))
        .count();
    let provider = StoreImages {
        base: Some(&url),
        images: &images,
    };

    // Cookies now resident for this page's site — captured from the real
    // responses through the sealed jar (zero for builtin/cookieless pages).
    let active_cookies = {
        let mut env = storage.lock().unwrap();
        let count = env
            .instance(active_instance)
            .cookies_for_request(&first_party, &first_party)
            .len();
        if let Some(dir) = &config.data_dir {
            env.save(Path::new(dir))
                .map_err(|e| AppError::Io(e.to_string()))?;
        }
        count
    };

    // --- Toolbar (minimal UI) over the page content, with real fonts. ---
    let text = TextEngine::new();
    let mut toolbar = Toolbar::new(active_label.clone());
    toolbar.url_text = config.url.clone();
    let content = toolbar.content_size(config.viewport);

    // Lay out + paint the page into the content area only.
    let layout_t = Instant::now();
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
    timings.record("layout+paint", layout_t.elapsed());
    timings.record_page_load();

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
        subresources_blocked,
        page_text: config.dump_text.then(|| visible_text(document.root())),
        timings: if config.timers {
            timings.as_pairs()
        } else {
            Vec::new()
        },
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
    /// Wall-clock the request→full-response took (server response time, M11).
    elapsed: Duration,
}

/// In-flight navigation bookkeeping.
struct Pending {
    id: u64,
    /// If this load is an https upgrade of an `http` URL, the original URL — so a
    /// failure can offer the risk prompt.
    http_fallback: Option<String>,
}

/// A job for the network worker. The `FetchContext` travels by value: it must
/// reflect the instance/first-party at *queue* time (a head switch mid-flight
/// must not re-attribute the fetch).
enum Job {
    Page {
        id: u64,
        url: String,
        ctx: FetchContext,
    },
    Sub {
        url: String,
        ctx: FetchContext,
    },
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
        elapsed: Duration,
    },
}

/// Performs page + sub-resource loads off the UI thread. Abstracted so the load
/// state machine is testable without the network (see `FakeLoader` in tests).
trait PageLoader {
    /// Queue a page navigation in an identity context.
    fn request(&self, id: u64, url: String, ctx: FetchContext);
    /// Queue an image sub-resource fetch (absolute URL) in an identity context.
    fn request_subresource(&self, url: String, ctx: FetchContext);
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
    fn new(
        system_roots: bool,
        jar: Option<Arc<dyn CookieJar>>,
        proxy: Option<ProxyConfig>,
    ) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<Job>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Done>();
        let waker: Arc<Mutex<Option<Arc<dyn Waker>>>> = Arc::new(Mutex::new(None));
        let worker_waker = waker.clone();

        let worker = std::thread::spawn(move || {
            // Build the network client (rustls config) once, on the worker.
            let client = network_client(system_roots, jar, proxy);
            while let Ok(job) = req_rx.recv() {
                let done = match job {
                    Job::Page { id, url, ctx } => {
                        let result = fetch_page(&client, &url, &ctx);
                        Done::Page {
                            id,
                            requested_url: url,
                            result,
                        }
                    }
                    Job::Sub { url, ctx } => {
                        let t = std::time::Instant::now();
                        let bytes = fetch_bytes(&client, &url, &ctx);
                        Done::Sub {
                            url,
                            bytes,
                            elapsed: t.elapsed(),
                        }
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
    fn request(&self, id: u64, url: String, ctx: FetchContext) {
        let _ = self.tx.send(Job::Page { id, url, ctx });
    }
    fn request_subresource(&self, url: String, ctx: FetchContext) {
        let _ = self.tx.send(Job::Sub { url, ctx });
    }
    fn try_recv(&mut self) -> Option<Done> {
        self.rx.try_recv().ok()
    }
    fn set_waker(&mut self, waker: Arc<dyn Waker>) {
        *self.waker.lock().unwrap() = Some(waker);
    }
}

fn fetch_page(client: &Router, url: &str, ctx: &FetchContext) -> Result<FetchedPage, String> {
    let parsed = parse_url(url).map_err(|e| e.to_string())?;
    let t = std::time::Instant::now();
    let resp = client.get_in(&parsed, ctx).map_err(|e| format!("{e:?}"))?;
    let elapsed = t.elapsed();
    let user_agent = client.user_agent_for(&parsed);
    Ok(FetchedPage {
        url: url.to_string(),
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
        user_agent,
        elapsed,
    })
}

fn fetch_bytes(client: &Router, url: &str, ctx: &FetchContext) -> Result<Vec<u8>, String> {
    let parsed = parse_url(url).map_err(|e| e.to_string())?;
    let resp = client.get_in(&parsed, ctx).map_err(|e| format!("{e:?}"))?;
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
    client: &Router,
    ctx: &FetchContext,
    policy: &Mutex<DefaultDenyPolicy>,
    first_party: &Origin,
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
    let mut out = HashMap::with_capacity(urls.len());
    let mut decoded_bytes = 0usize;
    for url in urls {
        // Consent gate: unruled third-party subresources never hit the network.
        let allowed = parse_url(&url)
            .ok()
            .and_then(|u| u.origin())
            .is_some_and(|origin| {
                policy
                    .lock()
                    .unwrap()
                    .evaluate(ctx.instance, &origin, first_party)
                    .decision
                    == Decision::Allow
            });
        if !allowed {
            out.insert(url, ImageState::Blocked);
            continue;
        }
        // Once the decoded-memory budget is spent, defer the remaining
        // (off-screen) images: they aren't fetched or decoded, and lay out as
        // their reserved/placeholder box instead of a resident bitmap.
        if decoded_bytes >= IMAGE_DECODE_BUDGET_BYTES {
            out.insert(url, ImageState::Pending);
            continue;
        }
        let state = match fetch_bytes(client, &url, ctx)
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
    /// Refused by the consent policy (third-party, no Allow rule). Paints as
    /// the placeholder/alt box; an Allow rule un-blocks and re-requests.
    Blocked,
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

/// The page's user-visible text: like `text_content`, but `<script>`/`<style>`
/// payloads are skipped (they are code, not page text — for `--dump-text`).
fn visible_text(node: NodeRef<'_>) -> String {
    fn walk(node: NodeRef<'_>, out: &mut String) {
        if matches!(node.tag(), "script" | "style") {
            return;
        }
        if let Some(text) = node.text() {
            out.push_str(text);
        }
        for child in node.children() {
            walk(child, out);
        }
    }
    let mut out = String::new();
    walk(node, &mut out);
    out
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
    /// Shared with the network worker's cookie jar (`SealedJar`), which
    /// attaches/captures cookies per hop. Lock discipline: take this lock
    /// transiently (lock → `instance()` → op → unlock) and never while holding
    /// another lock.
    storage: Arc<Mutex<StorageEnvironment>>,
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
    /// Map from a rendered node's `NodeId` (in `document`) to its live JS-model
    /// id, refreshed whenever scripts run or an event is dispatched. Lets a
    /// click correlate the hit node back to the realm node to dispatch at (M12b /
    /// ADR-0012). Empty for script-less pages (no realm, no dispatch targets).
    node_to_js: HashMap<NodeId, u64>,
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
    /// Consent policy shared with the worker-side cookie jar.
    consent: Arc<Mutex<DefaultDenyPolicy>>,
    /// Per-cookie disposition policy, shared with the worker's `SealedJar`.
    cookie_policy: Arc<Mutex<CookiePolicy>>,
    /// Worker-raised consent events, drained into `consent_prompts` by poll().
    pending_consent: Arc<Mutex<Vec<ConsentEvent>>>,
    /// Prompts awaiting the user, shown one at a time in the banner.
    consent_prompts: Vec<ConsentEvent>,
    /// Persistent profile dir (None = ephemeral; nothing touches disk).
    data_dir: Option<PathBuf>,
    /// Passphrase being typed into the settings overlay (cleared on submit).
    vault_input: String,
    /// Outcome line shown under the vault prompt.
    vault_msg: Option<String>,
    /// Whether the cookie inspector overlay is open.
    cookie_manager_open: bool,
    /// Top row offset of the cookie inspector list.
    cookie_scroll: usize,
    /// Cookies whose value the user has revealed `(fp_site, name)`.
    cookie_revealed: std::collections::HashSet<(String, String)>,
    /// In-progress TTL edit in the inspector `(fp_site, name, digits)`.
    cookie_ttl_edit: Option<(String, String, String)>,
    /// Per-page performance measurements (M11).
    timings: Timings,
    /// Whether the performance HUD is shown.
    hud_on: bool,
}

impl BrowserApp {
    /// Create a browser on the default heads, showing `cerberus:home`.
    pub fn new() -> Self {
        Self::with_options(false)
    }

    /// Like [`new`](Self::new) but trusting the OS root store (for TLS-inspecting
    /// proxies); see `RustlsProvider::with_system_roots`.
    pub fn with_options(system_roots: bool) -> Self {
        Self::with_config(AppOptions {
            system_roots,
            ..AppOptions::default()
        })
    }

    /// Create a browser from launch options. With a `data_dir`, cookies, the
    /// vault, and head seeds persist across runs; a profile that fails to open
    /// falls back to ephemeral (the on-disk data is left untouched, and
    /// nothing is written over it).
    pub fn with_config(options: AppOptions) -> Self {
        install_psl();
        let (env, data_dir) = match &options.data_dir {
            Some(dir) => match open_profile_storage(dir) {
                Ok(env) => (env, Some(dir.clone())),
                Err(e) => {
                    eprintln!(
                        "cerberus: cannot open profile {}: {e}; running ephemeral",
                        dir.display()
                    );
                    (StorageEnvironment::with_no_vault(), None)
                }
            },
            None => (StorageEnvironment::with_no_vault(), None),
        };
        let storage = Arc::new(Mutex::new(env));
        let mut policy = DefaultDenyPolicy::new(true);
        if let Some(dir) = &data_dir {
            if let Ok(text) = std::fs::read_to_string(dir.join(CONSENT_RULES_FILE)) {
                policy.load_rules(&text);
            }
        }
        let consent = Arc::new(Mutex::new(policy));
        let cookie_policy = Arc::new(Mutex::new(load_cookie_policy(data_dir.as_deref())));
        let pending_consent: Arc<Mutex<Vec<ConsentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let jar: Arc<dyn CookieJar> = Arc::new(SealedJar {
            storage: storage.clone(),
            policy: consent.clone(),
            cookies: cookie_policy.clone(),
            events: pending_consent.clone(),
        });
        let (heads, active) = match &data_dir {
            Some(dir) => load_heads(dir).unwrap_or_else(|| {
                let heads = fresh_profile_heads();
                if let Err(e) = save_heads(dir, &heads, 0) {
                    eprintln!("cerberus: cannot save heads: {e}");
                }
                (heads, 0)
            }),
            None => (default_heads(), 0),
        };
        let proxy = options.proxy.as_deref().map(|p| {
            parse_proxy(p).unwrap_or_else(|e| {
                // A misconfigured proxy must fail closed, not fall back to
                // direct connections (that would silently deanonymize).
                panic!("invalid --proxy {p:?}: {e:?}")
            })
        });
        let mut app = Self::build(
            Box::new(NetLoader::new(options.system_roots, Some(jar), proxy)),
            storage,
            heads,
            data_dir,
        );
        app.consent = consent;
        app.cookie_policy = cookie_policy;
        app.pending_consent = pending_consent;
        if active != 0 {
            let _ = app.heads.switch_to(active);
            app.toolbar.head_label = app.heads.active().label.clone();
        }
        app
    }

    /// Test seam: a fake loader and a fresh (jar-less) storage environment.
    #[cfg(test)]
    fn with_loader(loader: Box<dyn PageLoader>) -> Self {
        install_psl();
        Self::build(
            loader,
            Arc::new(Mutex::new(StorageEnvironment::with_no_vault())),
            default_heads(),
            None,
        )
    }

    fn build(
        loader: Box<dyn PageLoader>,
        storage: Arc<Mutex<StorageEnvironment>>,
        heads: Vec<Head>,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let heads = HeadManager::new(heads, Box::new(QuickJsEngineFactory));
        let label = heads.active().label.clone();
        let style_engine = CssEngine::new();
        let styled = style_engine.style(&empty_document());
        let mut app = Self {
            heads,
            storage,
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
            node_to_js: HashMap::new(),
            forms: FormStore::default(),
            focused_field: None,
            pending: None,
            next_id: 1,
            insecure_prompt: None,
            insecure_button: None,
            settings_open: false,
            background: Color::WHITE,
            last_size: Size::new(800, 600),
            consent: Arc::new(Mutex::new(DefaultDenyPolicy::new(true))),
            cookie_policy: Arc::new(Mutex::new(CookiePolicy::new())),
            pending_consent: Arc::new(Mutex::new(Vec::new())),
            consent_prompts: Vec::new(),
            data_dir,
            vault_input: String::new(),
            vault_msg: None,
            cookie_manager_open: false,
            cookie_scroll: 0,
            cookie_revealed: std::collections::HashSet::new(),
            cookie_ttl_edit: None,
            timings: Timings::new(),
            hud_on: false,
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
        // New page: reset the performance table and stamp the clock (M11).
        self.timings.begin_navigation();
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
        let ctx = FetchContext {
            instance,
            kind: FetchKind::Navigation,
        };
        self.loader.request(id, target, ctx);
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
        // Time scripts and style separately (M11); `Instant` directly because
        // both calls borrow `self`.
        let t = Instant::now();
        let doc = self.run_scripts(doc);
        self.timings.record("scripts", t.elapsed());
        self.page_title = doc.title();
        let t = Instant::now();
        self.styled = self.style_engine.style(&doc);
        self.timings.record("style", t.elapsed());
        self.document = doc;
    }

    /// Run the document's inline scripts against the active head's engine and
    /// return the reconciled document. Script-less pages return untouched (and
    /// keep the engine lazy); on any bridge failure we fall back to the
    /// unscripted DOM so the page still renders.
    fn run_scripts(&mut self, doc: Document) -> Document {
        // Each navigation rebuilds the realm's model, so the previous node↔JS
        // correlation is stale. A script-less page has no realm and no dispatch
        // targets: keep the map empty and return the DOM untouched.
        self.node_to_js.clear();
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
        // Persistent-realm path (ADR-0012): install the model once, run the page
        // scripts, fire load, then read the mutated tree back *with* its
        // JS-id → NodeId map so later interactions can dispatch events at the
        // right realm node. On any bridge failure, fall back to the unscripted
        // DOM so the page still renders.
        if install_page(engine, realm, &doc, &env).is_err() {
            return doc;
        }
        if cerberus_js_dom::run_scripts(engine, realm, doc.scripts()).is_err() {
            return doc;
        }
        let _ = fire_load(engine, realm);
        match serialize_dom(engine, realm) {
            Ok(rebuilt) => {
                let RebuiltDom { document, id_map } = rebuilt;
                self.node_to_js = invert_id_map(&id_map);
                document
            }
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

        self.request_page_images();
        self.update_nav();
        // Page-load total covers fetch → parse → scripts → style (M11);
        // layout+paint is timed per frame in render_frame.
        self.timings.record_page_load();
        self.persist();
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
                // Server response time for the navigation (M11).
                let label = parse_url(&page.url)
                    .ok()
                    .map(|u| format!("GET {}", u.host))
                    .unwrap_or_else(|| "GET".into());
                self.timings.record(label, page.elapsed);
                self.commit_response(
                    &page.url,
                    page.status,
                    &page.headers,
                    &page.body,
                    &page.user_agent,
                    true,
                )
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
    fn handle_subresource(
        &mut self,
        url: String,
        bytes: Result<Vec<u8>, String>,
        elapsed: Duration,
    ) -> bool {
        // Subresources are aggregated into one stable row so an image-heavy
        // page doesn't flood (and reflow) the HUD.
        self.timings.add("subresources", elapsed);
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
        let first_party = self.current_url.as_ref().and_then(first_party_of);
        let instance = self.heads.active().instance;
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
            let Some(first_party) = first_party.clone() else {
                continue;
            };
            // Consent gate: third-party subresources need an Allow rule;
            // otherwise they are blocked (and prompted, headed).
            if self.gate_subresource(&abs, &first_party) != Decision::Allow {
                self.images.insert(abs, ImageState::Blocked);
                continue;
            }
            self.images.insert(abs.clone(), ImageState::Pending);
            self.loader.request_subresource(
                abs,
                FetchContext {
                    instance,
                    kind: FetchKind::Subresource { first_party },
                },
            );
        }
    }

    // ---- Cookie inspector (M10) ----

    /// Snapshot the active head's cookies as inspector rows (sorted, with
    /// values masked unless revealed).
    fn cookie_rows(&self) -> Vec<(String, String, CookieRow)> {
        let instance = self.heads.active().instance;
        let mut views = self
            .storage
            .lock()
            .unwrap()
            .instance(instance)
            .cookie_views();
        views.sort_by(|a, b| (&a.fp_site, &a.name).cmp(&(&b.fp_site, &b.name)));
        views
            .into_iter()
            .map(|v| {
                let revealed = self
                    .cookie_revealed
                    .contains(&(v.fp_site.clone(), v.name.clone()));
                let shown = if revealed {
                    format!("{}={}", v.name, v.value)
                } else {
                    format!("{}=•••", v.name)
                };
                let exp = v
                    .expires
                    .map(|t| format!("exp {t}"))
                    .unwrap_or_else(|| "session".into());
                let row = CookieRow {
                    primary: shown,
                    detail: format!("{}  {}", v.domain, exp),
                    chip: v.disposition.label(),
                };
                (v.fp_site, v.name, row)
            })
            .collect()
    }

    /// Persist the cookie policy (and any cookie changes) to the profile.
    fn save_cookie_policy(&mut self) {
        if let Some(dir) = &self.data_dir {
            let text = self.cookie_policy.lock().unwrap().serialize();
            if let Err(e) = atomic_write(&dir.join(COOKIES_POLICY_FILE), text.as_bytes()) {
                eprintln!("cerberus: cannot save cookie policy: {e}");
            }
        }
        self.persist();
    }

    /// Apply one inspector action to storage + the policy, then persist.
    fn apply_cookie_action(&mut self, action: CookieAction) {
        let rows = self.cookie_rows();
        let instance = self.heads.active().instance;
        match action {
            CookieAction::Close => {
                self.cookie_manager_open = false;
                self.cookie_ttl_edit = None;
            }
            CookieAction::ScrollUp => self.cookie_scroll = self.cookie_scroll.saturating_sub(1),
            CookieAction::ScrollDown => {
                if self.cookie_scroll + 1 < rows.len() {
                    self.cookie_scroll += 1;
                }
            }
            CookieAction::CycleGlobal => {
                let next = self.cookie_policy.lock().unwrap().global().cycle();
                self.cookie_policy.lock().unwrap().set_global(next);
                self.save_cookie_policy();
            }
            CookieAction::Reveal(i) => {
                if let Some((site, name, _)) = rows.get(i) {
                    let key = (site.clone(), name.clone());
                    if !self.cookie_revealed.remove(&key) {
                        self.cookie_revealed.insert(key);
                    }
                }
            }
            CookieAction::Delete(i) => {
                if let Some((site, name, _)) = rows.get(i) {
                    self.storage
                        .lock()
                        .unwrap()
                        .instance(instance)
                        .delete_cookie(site, name);
                    self.cookie_policy.lock().unwrap().set_override(
                        site,
                        name,
                        CookieDisposition::Block,
                    );
                    self.save_cookie_policy();
                }
            }
            CookieAction::Cycle(i) => {
                if let Some((site, name, _)) = rows.get(i).cloned() {
                    let current = self.cookie_policy.lock().unwrap().resolve(&site, &name);
                    let next = current.cycle();
                    self.cookie_policy
                        .lock()
                        .unwrap()
                        .set_override(&site, &name, next);
                    self.storage
                        .lock()
                        .unwrap()
                        .instance(instance)
                        .set_disposition(&site, &name, next);
                    self.save_cookie_policy();
                    // Landing on Timed opens an inline editor for the exact secs.
                    if let CookieDisposition::Timed(secs) = next {
                        self.cookie_ttl_edit = Some((site, name, secs.to_string()));
                    } else {
                        self.cookie_ttl_edit = None;
                    }
                }
            }
            CookieAction::None => {}
        }
    }

    /// Commit the in-progress TTL edit (Enter, or before another action).
    fn commit_ttl_edit(&mut self) {
        let Some((site, name, buf)) = self.cookie_ttl_edit.take() else {
            return;
        };
        let secs: u64 = buf.parse().unwrap_or(DEFAULT_TIMED_SECS);
        let d = CookieDisposition::Timed(secs);
        let instance = self.heads.active().instance;
        self.cookie_policy
            .lock()
            .unwrap()
            .set_override(&site, &name, d);
        self.storage
            .lock()
            .unwrap()
            .instance(instance)
            .set_disposition(&site, &name, d);
        self.save_cookie_policy();
    }

    /// Evaluate the consent policy for one subresource URL in the context of
    /// `first_party`; queues a deduplicated banner prompt on `Prompt`.
    fn gate_subresource(&mut self, abs_url: &str, first_party: &Origin) -> Decision {
        let Some(origin) = parse_url(abs_url).ok().and_then(|u| u.origin()) else {
            return Decision::Deny;
        };
        let instance = self.heads.active().instance;
        let outcome = self
            .consent
            .lock()
            .unwrap()
            .evaluate(instance, &origin, first_party);
        if let Some(event) = outcome.event {
            self.queue_consent_prompt(event);
        }
        outcome.decision
    }

    /// Add a prompt to the banner queue unless an equivalent one is pending.
    fn queue_consent_prompt(&mut self, event: ConsentEvent) {
        let dup = self.consent_prompts.iter().any(|e| {
            e.instance == event.instance
                && e.request.site() == event.request.site()
                && e.first_party.site() == event.first_party.site()
        });
        if !dup {
            self.consent_prompts.push(event);
        }
    }

    /// Apply the user's banner decision to the front prompt.
    fn resolve_consent(&mut self, action: BannerAction) {
        if self.consent_prompts.is_empty() {
            return;
        }
        let event = self.consent_prompts.remove(0);
        match action {
            BannerAction::Allow | BannerAction::Deny => {
                let allow = action == BannerAction::Allow;
                self.consent.lock().unwrap().add_rule(
                    event.instance,
                    &event.request,
                    &event.first_party,
                    allow,
                );
                self.save_consent_rules();
                if allow {
                    self.unblock_site(&event);
                }
            }
            // Dismiss: no standing rule; the default (deny) keeps applying.
            BannerAction::Dismiss | BannerAction::None => {}
        }
    }

    /// After an Allow rule: release matching quarantined cookies and re-request
    /// this site's blocked subresources.
    fn unblock_site(&mut self, event: &ConsentEvent) {
        let allowed_site = event.request.site();
        // Quarantined cookies whose domain belongs to the allowed site.
        {
            let mut env = self.storage.lock().unwrap();
            let mut store = env.instance(event.instance);
            let names: Vec<String> = store
                .quarantined_cookies(&event.first_party)
                .into_iter()
                .filter(|c| Origin::new("https", c.domain.clone(), None).site() == allowed_site)
                .map(|c| c.name)
                .collect();
            for name in names {
                let _ = store.release_from_quarantine(&name, &event.first_party);
            }
        }
        self.persist();
        // Blocked images for that site re-enter the normal pipeline.
        let blocked: Vec<String> = self
            .images
            .iter()
            .filter(|(url, state)| {
                matches!(state, ImageState::Blocked)
                    && parse_url(url)
                        .ok()
                        .and_then(|u| u.origin())
                        .is_some_and(|o| o.site() == allowed_site)
            })
            .map(|(url, _)| url.clone())
            .collect();
        for url in blocked {
            self.images.remove(&url);
        }
        self.request_page_images();
    }

    /// Persist the standing consent rules into the profile (if any).
    fn save_consent_rules(&self) {
        let Some(dir) = &self.data_dir else { return };
        let text = self.consent.lock().unwrap().serialize_rules();
        if let Err(e) = atomic_write(&dir.join(CONSENT_RULES_FILE), text.as_bytes()) {
            eprintln!("cerberus: cannot save consent rules: {e}");
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
        // M12b: dispatch a real `click` to any JS listener first; the default
        // action below (focus, toggle, cycle, submit) runs only if no handler
        // called preventDefault. Script-less pages have no JS correlate, so this
        // is a no-op and the default action proceeds exactly as before.
        if let Some(node) = self.control_node_id(field.id) {
            if self.dispatch_dom(node, "click", "{}") {
                return true;
            }
        }
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

    /// The `NodeId` (in `self.document`) of the form control with field index
    /// `field_id`, via the same canonical pre-order walk layout uses for ids.
    fn control_node_id(&self, field_id: u32) -> Option<NodeId> {
        collect_controls(self.document.root())
            .iter()
            .find(|c| c.id == field_id)
            .map(|c| c.el.id())
    }

    /// Dispatch DOM `event_type` at `node` (a `NodeId` in `self.document`) into
    /// the live JS realm, reconcile any mutations the handler made, and report
    /// whether it called `preventDefault`. Returns `false` (a no-op) when the
    /// page has no realm or the node has no JS correlate, so non-scripted pages
    /// behave exactly as before (M12b / ADR-0012).
    fn dispatch_dom(&mut self, node: NodeId, event_type: &str, init_json: &str) -> bool {
        let Some(&js_id) = self.node_to_js.get(&node) else {
            return false;
        };
        let realm = RealmId(self.heads.active().id.0);
        let t = Instant::now();
        let result = {
            let engine = match self.heads.engine() {
                Ok(engine) => engine,
                Err(_) => return false,
            };
            dispatch_event(engine, realm, js_id, event_type, init_json)
        };
        let Ok(dispatched) = result else {
            return false;
        };
        // HUD handler row (M11): "click handler", "input handler", …
        self.timings
            .record(format!("{event_type} handler"), t.elapsed());
        let prevented = dispatched.default_prevented;
        self.reconcile_dispatched(dispatched.dom);
        prevented
    }

    /// Adopt the DOM read back after an event dispatch: refresh the node↔JS map,
    /// restyle, and swap in the new document (the next frame relays out and
    /// repaints). Mirrors the styling half of [`BrowserApp::set_document`].
    fn reconcile_dispatched(&mut self, dom: RebuiltDom) {
        let RebuiltDom { document, id_map } = dom;
        self.node_to_js = invert_id_map(&id_map);
        self.page_title = document.title();
        let t = Instant::now();
        self.styled = self.style_engine.style(&document);
        self.timings.record("style", t.elapsed());
        self.document = document;
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

    /// Attempt a vault unlock with the passphrase typed into the settings
    /// overlay. The input is cleared either way; the derived key (and the
    /// Secret's copy of the passphrase) zeroize on drop.
    fn try_unlock_vault(&mut self) {
        if self.vault_input.is_empty() {
            return;
        }
        let pass = Secret::from_passphrase(&self.vault_input);
        self.vault_input.clear();
        let result = self.storage.lock().unwrap().unlock_vault(&pass);
        self.vault_msg = Some(match result {
            Ok(()) => "vault unlocked".to_string(),
            Err(_) if self.data_dir.is_none() => {
                "no persistent profile (start with --data-dir)".to_string()
            }
            Err(_) => "wrong passphrase".to_string(),
        });
        // First unlock seals the check sentinel — persist it.
        self.persist();
    }

    /// Flush unsaved cookie/vault state to the profile dir (no-op when
    /// ephemeral or clean). Called after commits, head switches, and on Drop.
    fn persist(&mut self) {
        let Some(dir) = self.data_dir.clone() else {
            return;
        };
        let mut env = self.storage.lock().unwrap();
        if env.needs_save() {
            if let Err(e) = env.save(&dir) {
                eprintln!("cerberus: cannot persist profile: {e}");
            }
        }
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
        if let Some(dir) = self.data_dir.clone() {
            if let Err(e) = save_heads(&dir, self.heads.heads(), self.heads.active_index()) {
                eprintln!("cerberus: cannot save heads: {e}");
            }
        }
        self.persist();
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

impl Drop for BrowserApp {
    fn drop(&mut self) {
        self.persist();
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
        // Worker-side consent events (cookie capture) surface in the banner.
        let drained: Vec<ConsentEvent> =
            std::mem::take(self.pending_consent.lock().unwrap().as_mut());
        for event in drained {
            self.queue_consent_prompt(event);
            redraw = true;
        }
        while let Some(done) = self.loader.try_recv() {
            redraw |= match done {
                Done::Page {
                    id,
                    requested_url,
                    result,
                } => self.handle_page(id, requested_url, result),
                Done::Sub {
                    url,
                    bytes,
                    elapsed,
                } => self.handle_subresource(url, bytes, elapsed),
            };
        }
        redraw
    }

    fn render_frame(&mut self, size: Size) -> Framebuffer {
        self.last_size = size;
        let banner_h = if self.consent_prompts.is_empty() {
            0
        } else {
            BANNER_HEIGHT
        };
        let mut content = self.toolbar.content_size(size);
        content.h = content.h.saturating_sub(banner_h);
        let mut origin = self.toolbar.content_origin();
        origin.y += banner_h as i32;

        // Time layout+paint (M11). The image provider's borrow of `self` is
        // scoped to this block so the timing record (a `&mut self` op) is free.
        let t = Instant::now();
        let (laid, mut page) = {
            let provider = StoreImages {
                base: self.current_url.as_ref(),
                images: &self.images,
            };
            let mut layout = BlockLayout::default();
            let laid = layout.layout(&self.styled, content, &self.text, &provider, &self.forms);
            let mut page = Framebuffer::new(content);
            page.clear(self.background);
            self.text.rasterize(&laid.display, &mut page);
            (laid, page)
        };
        self.timings.record("layout+paint", t.elapsed());

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
        if let Some(event) = self.consent_prompts.first() {
            let banner = ConsentBanner::new(event.request.site(), self.consent_prompts.len() - 1);
            self.text
                .rasterize(&banner.paint(size, &self.text), &mut fb);
        }
        if self.insecure_prompt.is_some() {
            self.insecure_button = Some(paint_insecure_button(&mut fb, &self.text));
        }
        if self.settings_open {
            let vault_locked = self.storage.lock().unwrap().vault_locked();
            paint_settings_overlay(
                &mut fb,
                size,
                &self.text,
                &self.text,
                vault_locked,
                self.vault_input.chars().count(),
                self.vault_msg.as_deref(),
                self.hud_on,
            );
        }
        if self.cookie_manager_open {
            let global = self.cookie_policy.lock().unwrap().global().label();
            let rows: Vec<CookieRow> = self.cookie_rows().into_iter().map(|(_, _, r)| r).collect();
            self.text.rasterize(
                &CookieManager::paint(size, &self.text, &global, &rows, self.cookie_scroll),
                &mut fb,
            );
            if let Some((_, _, buf)) = &self.cookie_ttl_edit {
                let p = CookieManager::panel_rect(size);
                let mut list = DisplayList::new();
                list.push(DisplayItem::Glyphs {
                    origin: Point::new(p.x + 12, p.y + p.h as i32 - 14),
                    glyphs: self
                        .text
                        .shape(&format!("Timed seconds: {buf}_  (Enter)"), 13),
                    color: Color::rgb(0x20, 0x40, 0x70),
                    style: FontStyle::REGULAR,
                });
                self.text.rasterize(&list, &mut fb);
            }
        }
        // Performance HUD on top of everything, when enabled (M11).
        if self.hud_on {
            let rows = self.timings.display_rows();
            self.text
                .rasterize(&PerfHud::paint(size, &self.text, &rows), &mut fb);
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
        if self.cookie_manager_open {
            // The inspector owns all clicks while open. Commit any pending TTL
            // edit first, then apply the clicked control (a click outside the
            // panel closes it).
            self.commit_ttl_edit();
            if point_in_rect(CookieManager::panel_rect(self.last_size), x, y) {
                let len = self.cookie_rows().len();
                let action = CookieManager::hit_test(self.last_size, len, self.cookie_scroll, x, y);
                self.apply_cookie_action(action);
            } else {
                self.cookie_manager_open = false;
            }
            return true;
        }
        if self.settings_open {
            // A click on the "manage cookies" row opens the inspector.
            if point_in_rect(settings_cookies_rect(self.last_size), x, y) {
                self.settings_open = false;
                self.vault_msg = None;
                self.cookie_manager_open = true;
                self.cookie_scroll = 0;
                return true;
            }
            // Toggle the performance HUD.
            if point_in_rect(settings_timers_rect(self.last_size), x, y) {
                self.hud_on = !self.hud_on;
                return true;
            }
            // Clicks inside the panel stay in the panel (passphrase entry);
            // clicking outside dismisses it.
            if !point_in_rect(settings_panel_rect(self.last_size), x, y) {
                self.settings_open = false;
                self.vault_msg = None;
            }
            return true;
        }
        // The consent banner (when shown) owns its strip.
        if let Some(event) = self.consent_prompts.first() {
            let strip = ConsentBanner::rect(self.last_size);
            if point_in_rect(strip, x, y) {
                let banner =
                    ConsentBanner::new(event.request.site(), self.consent_prompts.len() - 1);
                let action = banner.hit_test(self.last_size, x, y);
                if action != BannerAction::None {
                    self.resolve_consent(action);
                }
                return true;
            }
        }
        let banner_h = if self.consent_prompts.is_empty() {
            0
        } else {
            BANNER_HEIGHT
        };
        // Page-area click: a form control wins over a link, which wins over
        // plain content. A click anywhere in the page that misses every control
        // also drops form focus (and is consumed if it actually had focus).
        if y >= (cerberus_ui::TOOLBAR_HEIGHT + banner_h) as i32 {
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
        // The cookie inspector's TTL editor captures digits.
        if self.cookie_manager_open {
            if let Some((_, _, buf)) = &mut self.cookie_ttl_edit {
                if c.is_ascii_digit() && buf.len() < 9 {
                    buf.push(c);
                }
            }
            return true;
        }
        // The settings overlay captures typing for the vault passphrase.
        if self.settings_open {
            if !c.is_control() {
                self.vault_input.push(c);
            }
            return true;
        }
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
        if self.cookie_manager_open {
            self.commit_ttl_edit();
            return true;
        }
        if self.settings_open {
            self.try_unlock_vault();
            return true;
        }
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
        if self.cookie_manager_open {
            if let Some((_, _, buf)) = &mut self.cookie_ttl_edit {
                buf.pop();
            }
            return true;
        }
        if self.settings_open {
            self.vault_input.pop();
            return true;
        }
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

/// Invert a JS-id → `NodeId` map into `NodeId` → JS-id. Each rebuilt node has a
/// unique id, so this is a bijection over the correlated nodes (nodes with no JS
/// origin — e.g. `innerHTML`-reparsed fragments — simply don't appear).
fn invert_id_map(map: &HashMap<u64, NodeId>) -> HashMap<NodeId, u64> {
    map.iter().map(|(&js, &node)| (node, js)).collect()
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

/// The settings panel's window rect (shared by paint and hit-testing).
fn settings_panel_rect(size: Size) -> Rect {
    let pw = size.w * 3 / 5;
    let ph = size.h * 3 / 5;
    let px = (size.w.saturating_sub(pw) / 2) as i32;
    let py = (size.h.saturating_sub(ph) / 2) as i32;
    Rect::new(px, py, pw, ph)
}

/// The clickable "manage cookies" row inside the settings overlay.
fn settings_cookies_rect(size: Size) -> Rect {
    let p = settings_panel_rect(size);
    Rect::new(p.x + 12, p.y + 176, 220, 22)
}

/// The clickable "performance HUD" toggle row inside the settings overlay.
fn settings_timers_rect(size: Size) -> Rect {
    let p = settings_panel_rect(size);
    Rect::new(p.x + 12, p.y + 204, 220, 22)
}

/// Paint the centered settings panel: vault state + passphrase entry.
#[allow(clippy::too_many_arguments)]
fn paint_settings_overlay(
    fb: &mut Framebuffer,
    size: Size,
    shaper: &dyn TextShaper,
    raster: &dyn Rasterizer,
    vault_locked: bool,
    input_chars: usize,
    vault_msg: Option<&str>,
    hud_on: bool,
) {
    let panel = settings_panel_rect(size);
    let (px, py, pw, ph) = (panel.x, panel.y, panel.w, panel.h);

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
    let vault_line = if vault_locked {
        "vault: locked (quarantined cookies are dropped)"
    } else {
        "vault: unlocked"
    };
    list.push(DisplayItem::Glyphs {
        origin: Point::new(px + 12, py + 78),
        glyphs: shaper.shape(vault_line, 14),
        color: Color::rgb(0x50, 0x50, 0x50),
        style: FontStyle::REGULAR,
    });
    if vault_locked {
        // Masked passphrase entry: type + Enter while the panel is open.
        let mask = "\u{2022}".repeat(input_chars);
        list.push(DisplayItem::Glyphs {
            origin: Point::new(px + 12, py + 104),
            glyphs: shaper.shape(&format!("passphrase: {mask}_"), 14),
            color: Color::BLACK,
            style: FontStyle::REGULAR,
        });
        list.push(DisplayItem::Glyphs {
            origin: Point::new(px + 12, py + 126),
            glyphs: shaper.shape("(type, then Enter to unlock)", 12),
            color: Color::rgb(0x80, 0x80, 0x80),
            style: FontStyle::REGULAR,
        });
    }
    if let Some(msg) = vault_msg {
        list.push(DisplayItem::Glyphs {
            origin: Point::new(px + 12, py + 150),
            glyphs: shaper.shape(msg, 14),
            color: Color::rgb(0x90, 0x30, 0x30),
            style: FontStyle::REGULAR,
        });
    }
    // Entry point to the cookie inspector.
    let cr = settings_cookies_rect(size);
    list.push(DisplayItem::Rect {
        rect: cr,
        color: Color::rgb(0xE6, 0xEE, 0xF6),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(cr.x + 6, cr.y + 16),
        glyphs: shaper.shape("manage cookies  >", 14),
        color: Color::rgb(0x20, 0x40, 0x70),
        style: FontStyle::REGULAR,
    });
    // Performance HUD toggle.
    let tr = settings_timers_rect(size);
    list.push(DisplayItem::Rect {
        rect: tr,
        color: Color::rgb(0xE6, 0xEE, 0xF6),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(tr.x + 6, tr.y + 16),
        glyphs: shaper.shape(
            if hud_on {
                "performance HUD: on"
            } else {
                "performance HUD: off"
            },
            14,
        ),
        color: Color::rgb(0x20, 0x40, 0x70),
        style: FontStyle::REGULAR,
    });
    raster.rasterize(&list, fb);
}

/// One pipeline-stage benchmark result.
pub struct BenchStage {
    pub name: &'static str,
    pub median_ms: f64,
}

/// Time the render pipeline stage-by-stage over a synthetic fixture page
/// (~200 elements: headings, paragraphs, a table, lists, inline styles, and a
/// script). Medians over `iters` runs. The fixture is embedded so results are
/// comparable across machines and runs — this is the M9 benchmark suite.
pub fn bench_pipeline(iters: usize) -> Vec<BenchStage> {
    use std::time::Instant;
    install_psl();

    // Build the fixture once (string building is not part of any stage).
    let mut html = String::from("<html><head><title>bench</title></head><body>");
    for i in 0..40 {
        html.push_str(&format!(
            "<h2 style=\"color:#336699\">Section {i}</h2>             <p>Paragraph with <b>bold</b>, <i>italics</i>, and a              <a href=\"/l{i}\">link {i}</a>.</p>             <ul><li>alpha {i}</li><li>beta</li><li>gamma</li></ul>"
        ));
    }
    html.push_str("<table>");
    for r in 0..20 {
        html.push_str(&format!(
            "<tr><td>r{r}c0</td><td>r{r}c1</td><th>r{r}h</th></tr>"
        ));
    }
    html.push_str("</table>");
    html.push_str(
        "<script>for (var i=0;i<200;i++){var d=document.createElement('div');         d.textContent='js '+i;document.body.appendChild(d);}</script>",
    );
    html.push_str("</body></html>");

    let median = |mut xs: Vec<f64>| -> f64 {
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs[xs.len() / 2]
    };
    let time = |f: &mut dyn FnMut()| -> f64 {
        let t = Instant::now();
        f();
        t.elapsed().as_secs_f64() * 1000.0
    };

    let mut out = Vec::new();

    let mut parse_times = Vec::with_capacity(iters);
    for _ in 0..iters {
        parse_times.push(time(&mut || {
            std::hint::black_box(parse_html(&html));
        }));
    }
    out.push(BenchStage {
        name: "parse",
        median_ms: median(parse_times),
    });

    let document = parse_html(&html);
    let css = CssEngine::new();
    let mut style_times = Vec::with_capacity(iters);
    for _ in 0..iters {
        style_times.push(time(&mut || {
            std::hint::black_box(css.style(&document));
        }));
    }
    out.push(BenchStage {
        name: "style",
        median_ms: median(style_times),
    });

    let styled = css.style(&document);
    let text = TextEngine::new();
    let viewport = Size::new(1280, 1024);
    let mut layout_times = Vec::with_capacity(iters);
    for _ in 0..iters {
        layout_times.push(time(&mut || {
            let mut layout = BlockLayout::default();
            std::hint::black_box(layout.layout(&styled, viewport, &text, &NoImages, &NoForms));
        }));
    }
    out.push(BenchStage {
        name: "layout",
        median_ms: median(layout_times),
    });

    let mut paint_times = Vec::with_capacity(iters);
    for _ in 0..iters {
        paint_times.push(time(&mut || {
            let mut layout = BlockLayout::default();
            std::hint::black_box(render_document(
                &styled,
                viewport,
                Color::WHITE,
                &mut layout,
                &text,
                &text,
                &NoImages,
                &NoForms,
            ));
        }));
    }
    out.push(BenchStage {
        name: "layout+paint",
        median_ms: median(paint_times),
    });

    // JS: engine instantiation + the fixture's script through the DOM bridge.
    let mut js_times = Vec::with_capacity(iters.min(5)); // engines are heavier
    for _ in 0..iters.min(5) {
        js_times.push(time(&mut || {
            let mut heads = HeadManager::new(default_heads(), Box::new(QuickJsEngineFactory));
            let realm = RealmId(heads.active().id.0);
            let engine = heads.engine().expect("engine");
            let env = PageEnv {
                url: "https://bench.test/".into(),
                viewport: (1280, 1024),
                user_agent: DEFAULT_USER_AGENT.into(),
            };
            std::hint::black_box(
                run_page_scripts(engine, realm, &document, document.scripts(), &env).expect("js"),
            );
        }));
    }
    out.push(BenchStage {
        name: "js (engine+bridge)",
        median_ms: median(js_times),
    });

    out
}

/// Measure RSS around `switches` head switches on a live browser (PLAN §5:
/// after a switch the resident set must stay within +10% of the pre-switch
/// idle — the proof that engine teardown leaks neither realms nor heap).
/// Returns `(before_kb, after_kb)`, or `None` where procfs is unavailable.
pub fn head_switch_rss(switches: usize) -> Option<(u64, u64)> {
    let mut app = BrowserApp::new();
    // Warm the engine once so the baseline includes a live isolate.
    let _ = app.heads.engine();
    let before = resident_set_kb()?;
    for _ in 0..switches {
        app.switch_head();
    }
    let after = resident_set_kb()?;
    Some((before, after))
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
        // Cookies are real now (captured from responses through the sealed
        // jar); the builtin page sets none.
        assert_eq!(outcome.active_cookies, 0);
        // Third-party access is denied by default in headless mode.
        assert_eq!(outcome.third_party_decision, Decision::Deny);
        // A frame was produced at the requested size.
        assert_eq!(outcome.framebuffer.size, RenderConfig::default().viewport);
    }

    // ---- Persistent profile helpers ----

    #[test]
    fn profile_heads_round_trip_and_are_random_per_profile() {
        use cerberus_farbling::FarblingProvider as _;
        let dir = std::env::temp_dir().join(format!("cerb-heads-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let heads = fresh_profile_heads();
        // Fresh profiles mint distinct random instances and seeds.
        assert_ne!(heads[0].instance, heads[1].instance);
        assert_ne!(heads[0].farbling.seed(), heads[1].farbling.seed());

        save_heads(&dir, &heads, 2).unwrap();
        let (loaded, active) = load_heads(&dir).unwrap();
        assert_eq!(active, 2);
        assert_eq!(loaded.len(), heads.len());
        for (a, b) in heads.iter().zip(&loaded) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.instance, b.instance);
            assert_eq!(a.label, b.label);
            assert_eq!(a.farbling.seed(), b.farbling.seed());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn profile_salt_is_created_once_and_stable() {
        let dir = std::env::temp_dir().join(format!("cerb-salt-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let env = open_profile_storage(&dir).unwrap();
        assert!(env.vault_locked());
        drop(env);
        let salt1 = std::fs::read(dir.join(VAULT_SALT_FILE)).unwrap();
        assert_eq!(salt1.len(), 16);

        let _env = open_profile_storage(&dir).unwrap();
        let salt2 = std::fs::read(dir.join(VAULT_SALT_FILE)).unwrap();
        assert_eq!(salt1, salt2, "salt must be stable across opens");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- The sealed cookie jar (the app side of the engine's cookie seam) ----

    #[allow(clippy::type_complexity)]
    fn jar_with_env() -> (
        SealedJar,
        Arc<Mutex<StorageEnvironment>>,
        Arc<Mutex<Vec<ConsentEvent>>>,
        Arc<Mutex<CookiePolicy>>,
    ) {
        install_psl();
        let storage = Arc::new(Mutex::new(StorageEnvironment::with_no_vault()));
        let events: Arc<Mutex<Vec<ConsentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let cookies = Arc::new(Mutex::new(CookiePolicy::new()));
        (
            SealedJar {
                storage: storage.clone(),
                policy: Arc::new(Mutex::new(DefaultDenyPolicy::new(true))),
                cookies: cookies.clone(),
                events: events.clone(),
            },
            storage,
            events,
            cookies,
        )
    }

    #[test]
    fn jar_stores_same_site_cookies_and_attaches_them() {
        let (jar, _env, _events, _cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let url = parse_url("https://shop.example.com/login").unwrap();
        let fp = url.origin().unwrap();

        assert_eq!(jar.cookie_header(instance, &url, &fp), None);
        jar.set_cookie(instance, &url, &fp, "sid=abc; Path=/; Secure");
        assert_eq!(
            jar.cookie_header(instance, &url, &fp).as_deref(),
            Some("sid=abc")
        );

        // Host-only cookie: NOT sent to a sibling subdomain...
        let sub = parse_url("https://cdn.example.com/a.png").unwrap();
        assert_eq!(jar.cookie_header(instance, &sub, &fp), None);

        // ...but a `Domain` cookie is shared across the site.
        jar.set_cookie(instance, &url, &fp, "site=1; Domain=example.com; Secure");
        assert_eq!(
            jar.cookie_header(instance, &sub, &fp).as_deref(),
            Some("site=1")
        );
    }

    #[test]
    fn jar_drops_cross_site_cookies_while_vault_is_locked() {
        let (jar, env, events, _cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let fp = Origin::new("https", "news.example.com", None);
        let tracker = parse_url("https://ads.tracker.net/pixel.gif").unwrap();

        // Third-party Set-Cookie: the policy says Prompt, quarantine is the
        // only path, and the locked vault rejects it — the cookie ceases to
        // exist. The prompt event is queued for the banner.
        jar.set_cookie(instance, &tracker, &fp, "uid=xyz");
        assert_eq!(jar.cookie_header(instance, &tracker, &fp), None);
        assert!(env
            .lock()
            .unwrap()
            .instance(instance)
            .quarantined_names(&fp)
            .is_empty());
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[test]
    fn jar_is_sealed_per_instance() {
        let (jar, _env, _events, _cookies) = jar_with_env();
        let a = InstanceId::from_u64_pair(0, 0xA);
        let b = InstanceId::from_u64_pair(0, 0xB);
        let url = parse_url("https://shop.example.com/").unwrap();
        let fp = url.origin().unwrap();

        jar.set_cookie(a, &url, &fp, "sid=only-in-a");
        assert!(jar.cookie_header(a, &url, &fp).is_some());
        assert!(jar.cookie_header(b, &url, &fp).is_none());
    }

    #[test]
    fn jar_applies_the_cookie_disposition_policy() {
        let (jar, env, _events, cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let url = parse_url("https://shop.example.com/").unwrap();
        let fp = url.origin().unwrap();
        let site = fp.site();

        // Global default Block → first-party cookie is dropped on capture.
        cookies.lock().unwrap().set_global(CookieDisposition::Block);
        jar.set_cookie(instance, &url, &fp, "a=1; Secure");
        assert!(env
            .lock()
            .unwrap()
            .instance(instance)
            .cookie_views()
            .is_empty());

        // A per-cookie Timed override wins over the global Block.
        cookies
            .lock()
            .unwrap()
            .set_override(&site, "b", CookieDisposition::Timed(120));
        jar.set_cookie(instance, &url, &fp, "b=2; Secure");
        let views = env.lock().unwrap().instance(instance).cookie_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "b");
        assert_eq!(views[0].disposition, CookieDisposition::Timed(120));

        // Allow-once: attached on the first request, gone on the second.
        cookies
            .lock()
            .unwrap()
            .set_override(&site, "c", CookieDisposition::AllowOnce);
        jar.set_cookie(instance, &url, &fp, "c=3; Secure");
        let h1 = jar.cookie_header(instance, &url, &fp).unwrap();
        assert!(h1.contains("c=3"));
        let h2 = jar.cookie_header(instance, &url, &fp).unwrap_or_default();
        assert!(
            !h2.contains("c=3"),
            "allow-once must not send twice: {h2:?}"
        );
    }

    #[test]
    fn cookie_admin_lists_and_sets_a_profile() {
        let dir = std::env::temp_dir().join(format!("cerb-cadmin-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let dir_s = dir.to_str().unwrap();

        // Empty profile lists nothing.
        assert!(cookie_admin(dir_s, None, None).unwrap().is_empty());

        // Seed a cookie into the active head's instance via the storage layer.
        install_psl();
        let instance = profile_active_instance(&dir);
        {
            let mut env = open_profile_storage(&dir).unwrap();
            let mut c = cerberus_storage::Cookie::host("sid", "v", "example.com");
            c.expires =
                Some(cerberus_storage::parse_http_date("Tue, 19 Jan 2038 03:14:07 GMT").unwrap());
            env.instance(instance)
                .set_cookie(&Origin::new("https", "example.com", None), c, Group::Active)
                .unwrap();
            env.save(&dir).unwrap();
        }
        let listed = cookie_admin(dir_s, None, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].contains("sid=v") && listed[0].contains("[allow]"));

        // Retune it to Timed via the admin path; the policy file is written.
        cookie_admin(dir_s, Some("https://example.com"), Some("sid=timed:60")).unwrap();
        assert!(dir.join("cookies.policy").exists());
        let after = cookie_admin(dir_s, None, None).unwrap();
        assert!(after[0].contains("Timed 60s"), "got {:?}", after[0]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_collects_stable_named_timings() {
        let cfg = RenderConfig {
            timers: true,
            ..RenderConfig::default()
        };
        let outcome = render(&cfg).expect("render");
        let labels: Vec<&str> = outcome.timings.iter().map(|(l, _)| l.as_str()).collect();
        // The builtin page exercises fetch → scripts → style → layout+paint,
        // plus the page-load total, in a stable order.
        assert!(labels.contains(&"scripts"), "{labels:?}");
        assert!(labels.contains(&"style"), "{labels:?}");
        assert!(labels.contains(&"layout+paint"), "{labels:?}");
        assert!(labels.contains(&"page load"), "{labels:?}");
        assert!(labels.iter().any(|l| l.starts_with("GET ")), "{labels:?}");
        // page load is last (recorded after the stages) and ≥ 0.
        assert_eq!(*labels.last().unwrap(), "page load");
        assert!(outcome.timings.iter().all(|(_, ms)| *ms >= 0.0));

        // Without --timers the field stays empty (zero overhead surfaced).
        let plain = render(&RenderConfig::default()).expect("render");
        assert!(plain.timings.is_empty());
    }

    #[test]
    fn interactive_timings_record_a_network_row_in_stable_order() {
        let mut app = fake_app(vec![(
            "https://t.test/",
            Ok(page("https://t.test/", 200, None, "<p>hi</p>")),
        )]);
        app.navigate("https://t.test/");
        assert!(app.poll());
        app.render_frame(Size::new(800, 600));
        let labels: Vec<String> = app.timings.rows().iter().map(|r| r.label.clone()).collect();
        // The FakeLoader injects a 7ms elapsed for the navigation request.
        assert!(labels.iter().any(|l| l == "GET t.test"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "layout+paint"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "page load"), "{labels:?}");
        // A second frame updates layout+paint in place — no new row, no reorder.
        let before = labels.len();
        app.render_frame(Size::new(800, 600));
        assert_eq!(app.timings.rows().len(), before);
    }

    #[test]
    fn cookie_inspector_cycles_deletes_and_edits_ttl() {
        let mut app = fake_app(vec![(
            "cerberus:home",
            Ok(page("cerberus:home", 200, None, "<p>hi</p>")),
        )]);
        let inst = app.heads.active().instance;
        let fp = Origin::new("https", "example.com", None);
        app.storage
            .lock()
            .unwrap()
            .instance(inst)
            .set_cookie(
                &fp,
                cerberus_storage::Cookie::host("sid", "v", "example.com"),
                Group::Active,
            )
            .unwrap();
        app.cookie_manager_open = true;

        let rows = app.cookie_rows();
        assert_eq!(rows.len(), 1);
        // Value masked until revealed.
        assert!(rows[0].2.primary.contains("•••"));
        app.apply_cookie_action(CookieAction::Reveal(0));
        assert!(app.cookie_rows()[0].2.primary.contains("sid=v"));

        // Cycle: Allow → Session.
        app.apply_cookie_action(CookieAction::Cycle(0));
        assert_eq!(
            app.cookie_policy
                .lock()
                .unwrap()
                .resolve("https://example.com", "sid"),
            CookieDisposition::Session
        );
        // Cycle again: Session → Timed(default), which opens the TTL editor.
        app.apply_cookie_action(CookieAction::Cycle(0));
        assert!(app.cookie_ttl_edit.is_some());
        // Type a new TTL and commit.
        if let Some((_, _, buf)) = &mut app.cookie_ttl_edit {
            *buf = "90".to_string();
        }
        app.commit_ttl_edit();
        assert_eq!(
            app.cookie_policy
                .lock()
                .unwrap()
                .resolve("https://example.com", "sid"),
            CookieDisposition::Timed(90)
        );

        // Delete removes the cookie and records a Block override.
        app.apply_cookie_action(CookieAction::Delete(0));
        assert!(app.cookie_rows().is_empty());
        assert_eq!(
            app.cookie_policy
                .lock()
                .unwrap()
                .resolve("https://example.com", "sid"),
            CookieDisposition::Block
        );

        // Global default cycles without panicking.
        app.apply_cookie_action(CookieAction::CycleGlobal);
        assert_ne!(
            app.cookie_policy.lock().unwrap().global(),
            CookieDisposition::Allow
        );
        // Close.
        app.apply_cookie_action(CookieAction::Close);
        assert!(!app.cookie_manager_open);
    }

    #[test]
    fn jar_rejects_malformed_and_misdomained_cookies() {
        let (jar, _env, _events, _cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let url = parse_url("https://shop.example.com/").unwrap();
        let fp = url.origin().unwrap();

        jar.set_cookie(instance, &url, &fp, "no-equals");
        jar.set_cookie(instance, &url, &fp, "a=1; Domain=other.com");
        jar.set_cookie(instance, &url, &fp, "b=2; Domain=com");
        assert_eq!(jar.cookie_header(instance, &url, &fp), None);
    }

    // ---- Consent enforcement (M5) ----

    #[test]
    fn third_party_images_are_blocked_then_allowed_via_the_banner() {
        let mut b = fake_app_img(
            vec![(
                "https://news.test/",
                Ok(page(
                    "https://news.test/",
                    200,
                    None,
                    "<img src=\"https://ads.tracker.net/pixel.png\"> \
                     <img src=\"/own.png\">",
                )),
            )],
            vec![
                // Only the first-party image has a canned response; if the
                // tracker pixel were fetched it would resolve to Failed.
                ("https://news.test/own.png", Ok(test_png(2, 2))),
            ],
        );
        b.navigate("https://news.test/");
        assert!(b.poll());

        // The third-party image never reached the loader: Blocked, not Failed.
        assert!(matches!(
            b.images.get("https://ads.tracker.net/pixel.png"),
            Some(ImageState::Blocked)
        ));
        // The first-party image went through the normal pipeline.
        assert!(matches!(
            b.images.get("https://news.test/own.png"),
            Some(ImageState::Ready(_))
        ));
        // A banner prompt is pending for the tracker site.
        assert_eq!(b.consent_prompts.len(), 1);
        assert_eq!(b.consent_prompts[0].request.site(), "https://tracker.net");

        // The user allows it: a standing rule lands and the image re-requests
        // (the loader has no canned bytes, so it resolves Failed — proof the
        // fetch actually went out this time).
        b.resolve_consent(BannerAction::Allow);
        assert!(b.poll());
        assert!(b.consent_prompts.is_empty());
        assert!(matches!(
            b.images.get("https://ads.tracker.net/pixel.png"),
            Some(ImageState::Failed)
        ));
        // And the rule persists in the policy: gating now answers Allow.
        let fp = Origin::new("https", "news.test", None);
        assert_eq!(
            b.gate_subresource("https://ads.tracker.net/pixel.png", &fp),
            Decision::Allow
        );
    }

    #[test]
    fn deny_leaves_the_site_blocked_without_new_prompts() {
        let mut b = fake_app_img(
            vec![(
                "https://news.test/",
                Ok(page(
                    "https://news.test/",
                    200,
                    None,
                    "<img src=\"https://ads.tracker.net/pixel.png\">",
                )),
            )],
            vec![],
        );
        b.navigate("https://news.test/");
        assert!(b.poll());
        assert_eq!(b.consent_prompts.len(), 1);

        b.resolve_consent(BannerAction::Deny);
        assert!(b.consent_prompts.is_empty());
        // Still blocked, and re-gating answers Deny with no new prompt.
        let fp = Origin::new("https", "news.test", None);
        assert_eq!(
            b.gate_subresource("https://ads.tracker.net/pixel.png", &fp),
            Decision::Deny
        );
        assert!(b.consent_prompts.is_empty());
    }

    // ---- Heads (M7): the switch swaps the sealed instance everywhere ----

    #[test]
    fn head_switch_changes_the_sealed_instance_and_engine() {
        let loader = FakeLoader::new(vec![(
            "https://a.test/",
            Ok(page("https://a.test/", 200, None, "<p>one</p>")),
        )]);
        let seen = loader.seen_instances.clone();
        let mut b = BrowserApp::with_loader(Box::new(loader));

        b.navigate("https://a.test/");
        assert!(b.poll());
        let first_instance = b.heads.active().instance;

        b.switch_head();
        assert_ne!(b.heads.active().instance, first_instance);
        // Memory-first invariant survives the switch: at most one engine.
        assert!(b.engines_live() <= 1);

        b.navigate("https://a.test/");
        assert!(b.poll());

        // The network worker was handed two *different* sealed instances —
        // the fetch path itself is what isolates the heads (the per-instance
        // cache means head B's load cannot be served from head A's entry).
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "two page loads requested");
        assert_eq!(seen[0], first_instance);
        assert_ne!(seen[1], seen[0]);
        assert_eq!(seen[1], b.heads.active().instance);
    }

    // ---- Hermetic test harness: a fake loader, no network or threads. ----

    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    struct FakeLoader {
        responses: HashMap<String, Result<FetchedPage, String>>,
        images: HashMap<String, Result<Vec<u8>, String>>,
        queue: RefCell<VecDeque<Done>>,
        /// Instances seen on page requests, in order (head-switch tests).
        seen_instances: Arc<Mutex<Vec<InstanceId>>>,
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
                seen_instances: Arc::new(Mutex::new(Vec::new())),
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
        fn request(&self, id: u64, url: String, ctx: FetchContext) {
            self.seen_instances.lock().unwrap().push(ctx.instance);
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
        fn request_subresource(&self, url: String, _ctx: FetchContext) {
            let bytes = self
                .images
                .get(&url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no canned image for {url}")));
            self.queue.borrow_mut().push_back(Done::Sub {
                url,
                bytes,
                elapsed: Duration::from_millis(0),
            });
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
            elapsed: Duration::from_millis(7),
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

    /// The value of `attr` on the first element with `id` (depth-first).
    fn attr_of_id(node: NodeRef<'_>, id: &str, attr: &str) -> Option<String> {
        if node.is_element() && node.attr("id") == Some(id) {
            return node.attr(attr).map(str::to_string);
        }
        node.children().find_map(|c| attr_of_id(c, id, attr))
    }

    #[test]
    fn button_click_dispatches_to_js_and_preventdefault_stops_the_default() {
        // A scripted page: clicking the button runs its JS handler, which bumps
        // a data attribute and calls preventDefault — so the DOM mutates and the
        // default form submit does NOT navigate. A second click proves the realm
        // persisted (the counter advances rather than resetting).
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/go'><input type='submit' id='b' value='+1'></form>\
                     <script>document.getElementById('b').addEventListener('click', function (e) { \
                       e.preventDefault(); \
                       var n = document.getElementById('b'); \
                       var c = parseInt(n.getAttribute('data-count') || '0', 10) + 1; \
                       n.setAttribute('data-count', String(c)); \
                     });</script>",
                )),
            )],
            "https://site.test/",
        );

        b.render_frame(Size::new(800, 600));
        let r1 = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Button))
            .expect("button box")
            .rect;
        assert!(b.pointer_down(r1.x + 1, r1.y + 1), "first click consumed");
        assert_eq!(
            attr_of_id(b.document.root(), "b", "data-count").as_deref(),
            Some("1"),
            "the click handler ran and mutated the DOM"
        );
        assert!(
            b.pending.is_none(),
            "preventDefault must stop the default submit/navigation"
        );

        // Second click on the persistent realm: the counter advances to 2.
        b.render_frame(Size::new(800, 600));
        let r2 = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Button))
            .expect("button box")
            .rect;
        assert!(b.pointer_down(r2.x + 1, r2.y + 1), "second click consumed");
        assert_eq!(
            attr_of_id(b.document.root(), "b", "data-count").as_deref(),
            Some("2"),
            "the realm persisted across clicks (counter advanced, not reset)"
        );
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

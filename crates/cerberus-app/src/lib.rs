//! Cerberus composition root.
//!
//! This is the *only* place that knows concrete adapters. Every subsystem is
//! reached through its trait; swapping an adapter (e.g. the null JS engine for a
//! real V8 adapter) is a change here and nowhere else. The `render` function
//! drives the full M0 path end-to-end:
//!
//! identities → sealed storage → (built-in) fetch → parse → layout → paint →
//! present, with the consent and farbling seams exercised along the way.

mod inline_svg;
pub mod mirror;
pub mod parity;

use inline_svg::replace_inline_svgs;

/// Lock a `Mutex`, recovering the guard if a previous holder panicked and
/// poisoned it instead of propagating the panic — one poisoned critical
/// section must not sink the whole browser session.
trait LockRecover<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for std::sync::Mutex<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

use cerberus_consent::{ConsentEvent, ConsentPolicy, Decision, DefaultDenyPolicy};
use cerberus_crypto::Secret;
use cerberus_crypto_rustcrypto::{Argon2idKdf, XChaCha20Poly1305Aead};
use cerberus_css::CssEngine;
use cerberus_dns_doh::DohResolver;
use cerberus_dom::{parse_html, Document, DocumentBuilder, NodeId, NodeRef};
use cerberus_headless::{render_document, render_document_laid};
use cerberus_identity::{Head, HeadManager};
use cerberus_image::ImageCodec;
use cerberus_js::JsEngineFactory;
use cerberus_js_dom::{
    dispatch_event, fire_load, install_page, reject_fetch, resolve_fetch, run_event_loop,
    run_page_scripts, run_page_scripts_with_fetch, serialize_dom, set_node_value,
    take_cookie_writes, take_fetches, take_navigations, EventLoopBudget, FetchClient, FetchRequest,
    FetchResponse, PageEnv, RebuiltDom,
};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_layout::{
    pick_img_url, pick_picture_url, BlockLayout, ElementBox, FieldKind, FormFieldBox, FormState,
    ImageProvider, LayoutEngine, LinkBox, NoForms, NoImages, PictureSource,
};
pub use cerberus_layout::{ImageDisplayMode, LayoutEngineKind};

/// Which images render as text (the resource-saving text-only option): a global
/// default mode plus per-image overrides that flip it. `text_only(url)` is the
/// single decision consulted by both the fetch skip and the render provider, so
/// they always agree. An override is matched as a substring of the resolved URL,
/// so a caller can name one image (its file name) or a whole path.
#[derive(Clone, Debug, Default)]
pub struct ImagePolicy {
    /// The default when no override matches.
    pub default: ImageDisplayMode,
    /// Resolved-URL substrings whose match flips an image to the opposite of the
    /// default (text-only in a graphical default, graphical in a text-only one).
    pub overrides: Vec<String>,
}

impl ImagePolicy {
    fn text_only(&self, url: &str) -> bool {
        let flipped = self
            .overrides
            .iter()
            .any(|o| !o.is_empty() && url.contains(o.as_str()));
        (self.default == ImageDisplayMode::TextOnly) ^ flipped
    }
}

/// Construct the selected layout engine, composing the two adapters here (in the
/// app) so `cerberus-layout` need not depend on `cerberus-taffy` — the taffy
/// engine implements `cerberus-layout`'s trait, and this is the only place that
/// knows both. `Block` is the hand-rolled walker; `Taffy` is the standardized
/// block/flex/grid box engine (`RENDERING_ARCHITECTURE_PLAN.md`, Stage 3).
fn make_layout(kind: LayoutEngineKind) -> Box<dyn LayoutEngine> {
    match kind {
        LayoutEngineKind::Block => Box::new(BlockLayout::default()),
        LayoutEngineKind::Taffy => Box::new(cerberus_taffy::TaffyLayout),
    }
}
use cerberus_net::{
    parse_proxy, BuiltinHttpClient, CachingResolver, CookieJar, FallbackResolver, FetchContext,
    FetchKind, HttpCache, HttpClient, HttpResponse, ProxyConfig, Router, SystemResolver,
    DEFAULT_USER_AGENT,
};
use cerberus_paint::{
    DecodedImage, DisplayItem, DisplayList, Framebuffer, ImageDecoder, Rasterizer, TextShaper,
};
use cerberus_shell::{FrameApp, HeadlessSurface, PlatformSurface, Waker};
use cerberus_storage::{
    atomic_write, parse_set_cookie, random_bytes, CookieDisposition, CookiePolicy, CookieView,
    EncryptedVault, Group, StorageEnvironment, DEFAULT_TIMED_SECS,
};
use cerberus_style::{Display, ExternalSheets, StyleEngine, StyledChild, StyledDom, StyledNode};
use cerberus_text::TextEngine;
use cerberus_tls_rustls::RustlsProvider;
use cerberus_types::{Color, FontStyle, HeadId, InstanceId, Origin, Point, RealmId, Rect, Size};
use cerberus_ui::{
    BannerAction, ConsentBanner, CookieAction, CookieManager, CookieRow, MircAction, MircPanel,
    MircRow, MircState, PerfHud, Toolbar, ToolbarAction, BANNER_HEIGHT,
};
use cerberus_url::{join as join_url, parse as parse_url, Url};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zeroize::Zeroize;

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
    /// Which layout engine to use (`--engine block|taffy`), for A/B parity
    /// comparison during the layout migration. Defaults to the `CERB_LAYOUT` env
    /// (else the block walker).
    pub layout_engine: LayoutEngineKind,
    /// Image display default (`--images graphical|text-only`): text-only renders
    /// each image's alt/caption instead of the graphic and never fetches its
    /// bytes, saving memory/CPU/network. Defaults to the `CERB_IMAGES` env.
    pub image_mode: ImageDisplayMode,
    /// Per-image overrides that flip [`image_mode`](Self::image_mode) for the
    /// images whose resolved URL contains one of these substrings — the
    /// per-image granularity of the option.
    pub text_only_images: Vec<String>,
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
            layout_engine: LayoutEngineKind::from_env(),
            image_mode: ImageDisplayMode::from_env(),
            text_only_images: Vec::new(),
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
    /// Link hit-boxes the layout produced (href + rect in content coordinates)
    /// — the clickability surface, for auditing that every visible anchor is
    /// dispatchable.
    pub links: Vec<cerberus_layout::LinkBox>,
    /// Form-control hit-boxes (buttons, fields) the layout produced.
    pub fields: Vec<cerberus_layout::FormFieldBox>,
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

/// Mint a fresh head: a random sealed instance id + farbling seed for `label`,
/// with head id derived from `index`. Per-profile unlinkability.
fn mint_head(label: &str, index: u64) -> Head {
    let instance_bytes: [u8; 16] = random_bytes(16).try_into().expect("16 random bytes");
    let seed_bytes: [u8; 8] = random_bytes(8).try_into().expect("8 random bytes");
    Head::new(
        HeadId::from_u64_pair(0, index),
        InstanceId(cerberus_types::Id128::from_bytes(instance_bytes)),
        label,
        u64::from_le_bytes(seed_bytes),
    )
}

/// A profile's heads: random instance ids + farbling seeds minted on first
/// run (per-profile unlinkability), persisted in a human-auditable text file.
/// The three labels are only the first-run default — a profile may hold any
/// number of identities (see [`identities_admin`]).
fn fresh_profile_heads() -> Vec<Head> {
    ["work", "personal", "throwaway"]
        .iter()
        .enumerate()
        .map(|(i, label)| mint_head(label, i as u64 + 1))
        .collect()
}

/// Parse `heads.txt`: `cerberus-heads v1`, `active <idx>`, then one
/// `head <head-id> <instance-id> <seed-hex> <label>` line per head, each
/// optionally followed by a `proxy <head-id> <host:port>` line (a head with no
/// such line has no per-identity proxy). The `proxy` line is a v1-compatible
/// addition: older files simply have none, and the `head` line format — where
/// the label is the rest of the line — is unchanged.
fn load_heads(dir: &Path) -> Option<(Vec<Head>, usize)> {
    let text = std::fs::read_to_string(dir.join(HEADS_FILE)).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "cerberus-heads v1" {
        return None;
    }
    let mut active = 0usize;
    let mut heads = Vec::new();
    // `proxy` lines may appear after the head they name; collect and apply once
    // all heads are read, so ordering within the file doesn't matter.
    let mut proxies: HashMap<HeadId, String> = HashMap::new();
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
            Some("proxy") => {
                let id = HeadId::from_hex(parts.next()?)?;
                let value = parts.next()?.to_string();
                proxies.insert(id, value);
            }
            Some(_) | None => continue,
        }
    }
    if heads.is_empty() || active >= heads.len() {
        return None;
    }
    for h in &mut heads {
        if let Some(p) = proxies.get(&h.id) {
            h.proxy = Some(p.clone());
        }
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
        if let Some(proxy) = &h.proxy {
            out.push_str(&format!("proxy {} {}\n", h.id, proxy));
        }
    }
    atomic_write(&dir.join(HEADS_FILE), out.as_bytes())
}

/// Headless identities admin (the `identities` CLI): list, add, or remove a
/// profile's sealed identities, persisted in `heads.txt`. A profile holds
/// arbitrary N identities — the 3-head default is just the first run, and the
/// mirror driver (`--mirror`) drives every identity the profile has.
pub fn identities_admin(
    dir: &str,
    add: Option<&str>,
    remove: Option<usize>,
) -> Result<Vec<String>, String> {
    identities_admin_full(dir, add, remove, None, None)
}

/// Full identities admin, adding per-identity egress proxy control (per-window
/// proxy): `set_proxy` is `<idx>=<host:port>` to route that identity's traffic
/// through its own proxy; `clear_proxy` is an index whose proxy is removed
/// (falls back to the global `--proxy` / direct). The proxy string is validated
/// here (fail-closed) so a bad value never reaches `heads.txt`.
pub fn identities_admin_full(
    dir: &str,
    add: Option<&str>,
    remove: Option<usize>,
    set_proxy: Option<&str>,
    clear_proxy: Option<usize>,
) -> Result<Vec<String>, String> {
    let path = Path::new(dir);
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let (mut heads, mut active, created) = match load_heads(path) {
        Some((heads, active)) => (heads, active, false),
        None => (fresh_profile_heads(), 0, true),
    };
    if let Some(label) = add {
        let label = label.trim();
        if label.is_empty() {
            return Err("identity label must not be empty".into());
        }
        let index = heads.len() as u64 + 1;
        heads.push(mint_head(label, index));
    }
    if let Some(idx) = remove {
        if idx >= heads.len() {
            return Err(format!("no identity at index {idx}"));
        }
        if heads.len() == 1 {
            return Err("cannot remove the last identity".into());
        }
        heads.remove(idx);
        if active >= heads.len() {
            active = heads.len() - 1;
        }
    }
    let mut proxy_changed = false;
    if let Some(spec) = set_proxy {
        let (idx, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("--set-proxy needs <idx>=<host:port>, got {spec:?}"))?;
        let idx: usize = idx
            .trim()
            .parse()
            .map_err(|_| format!("bad identity index in {spec:?}"))?;
        // Validate now (fail-closed): a malformed proxy must never persist.
        parse_proxy(value).map_err(|e| format!("invalid proxy {value:?}: {e:?}"))?;
        let head = heads
            .get_mut(idx)
            .ok_or_else(|| format!("no identity at index {idx}"))?;
        head.proxy = Some(value.trim().to_string());
        proxy_changed = true;
    }
    if let Some(idx) = clear_proxy {
        let head = heads
            .get_mut(idx)
            .ok_or_else(|| format!("no identity at index {idx}"))?;
        head.proxy = None;
        proxy_changed = true;
    }
    if created || add.is_some() || remove.is_some() || proxy_changed {
        save_heads(path, &heads, active).map_err(|e| e.to_string())?;
    }
    Ok(heads
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let marker = if i == active { "*" } else { " " };
            match &h.proxy {
                Some(p) => format!("{marker} [{i}] {} ({}) proxy={p}", h.label, h.instance),
                None => format!("{marker} [{i}] {} ({})", h.label, h.instance),
            }
        })
        .collect())
}

/// Headless autofill-profile administration (`cerberus-app profile`): show — or,
/// with `set` (`key=value;…`), update — the identity at `identity`'s autofill
/// profile, sealed in the encrypted vault. The vault is unlocked with
/// `passphrase` (a wrong one fails here). Secrets are redacted in the output.
pub fn profile_admin(
    dir: &str,
    identity: usize,
    set: Option<&str>,
    passphrase: &str,
) -> Result<Vec<String>, String> {
    let path = Path::new(dir);
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let heads = load_heads(path)
        .map(|(h, _)| h)
        .unwrap_or_else(default_heads);
    let head = heads
        .get(identity)
        .ok_or_else(|| format!("no identity at index {identity}"))?;
    let instance = head.instance;

    let mut env = open_profile_storage(path).map_err(|e| e.to_string())?;
    env.unlock_vault(&Secret::from_passphrase(passphrase))
        .map_err(|e| format!("vault unlock failed: {e:?}"))?;

    let load = |env: &mut StorageEnvironment| {
        env.load_blob(instance, AUTOFILL_PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|b| cerberus_autofill::Profile::from_bytes(&b))
            .unwrap_or_default()
    };

    if let Some(spec) = set {
        let mut profile = load(&mut env);
        apply_profile_fields(&mut profile, spec)?;
        env.store_blob(instance, AUTOFILL_PROFILE_KEY, &profile.to_bytes())
            .map_err(|e| format!("{e:?}"))?;
        env.save(path).map_err(|e| e.to_string())?;
    }

    Ok(profile_lines(identity, &head.label, &load(&mut env)))
}

/// The no-frills CSV template (`profile --template`): header + example rows, in
/// the chosen delimiter. Needs no vault — it is pure text.
pub fn profile_csv_template(delim: char) -> String {
    cerberus_autofill::csv_template(delim)
}

/// Export every identity's sealed autofill profile to `file` as CSV
/// (`profile --export`), using `delim`. Unlocks the vault with `passphrase`.
/// Returns the number of identities written. An identity with no stored profile
/// is written as an empty row, so the export doubles as a labeled template.
pub fn profile_export(
    dir: &str,
    file: &str,
    passphrase: &str,
    delim: char,
) -> Result<usize, String> {
    let path = Path::new(dir);
    let heads = load_heads(path)
        .map(|(h, _)| h)
        .unwrap_or_else(default_heads);
    let mut env = open_profile_storage(path).map_err(|e| e.to_string())?;
    env.unlock_vault(&Secret::from_passphrase(passphrase))
        .map_err(|e| format!("vault unlock failed: {e:?}"))?;

    let rows: Vec<(String, cerberus_autofill::Profile)> = heads
        .iter()
        .map(|h| {
            let profile = env
                .load_blob(h.instance, AUTOFILL_PROFILE_KEY)
                .ok()
                .flatten()
                .and_then(|b| cerberus_autofill::Profile::from_bytes(&b))
                .unwrap_or_default();
            (h.label.clone(), profile)
        })
        .collect();
    let csv = cerberus_autofill::profiles_to_csv(&rows, delim);
    std::fs::write(file, csv).map_err(|e| e.to_string())?;
    Ok(rows.len())
}

/// Import autofill profiles from a CSV `file` (`profile --import`), sealing each
/// in the vault. Rows map to identities by the `identity` label; a label with no
/// existing identity is **created** (minted like `identities --add`), so a filled
/// template sets up many identities at once. The delimiter is auto-detected.
/// Returns a human-readable report (one line per row).
pub fn profile_import(dir: &str, file: &str, passphrase: &str) -> Result<Vec<String>, String> {
    let path = Path::new(dir);
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let rows = cerberus_autofill::profiles_from_csv(&text)?;
    if rows.is_empty() {
        return Err("no identity rows found in the CSV".into());
    }
    // Each identity label must be unique within the file (else a later row would
    // silently clobber an earlier one).
    for (i, (label, _)) in rows.iter().enumerate() {
        if rows[..i].iter().any(|(l, _)| l == label) {
            return Err(format!("duplicate identity {label:?} in the CSV"));
        }
    }

    let (mut heads, active) = load_heads(path).unwrap_or_else(|| (fresh_profile_heads(), 0));
    let mut env = open_profile_storage(path).map_err(|e| e.to_string())?;
    env.unlock_vault(&Secret::from_passphrase(passphrase))
        .map_err(|e| format!("vault unlock failed: {e:?}"))?;

    let mut report = Vec::new();
    let mut created_any = false;
    for (label, profile) in &rows {
        let idx = match heads.iter().position(|h| &h.label == label) {
            Some(i) => i,
            None => {
                let index = heads.len() as u64 + 1;
                heads.push(mint_head(label, index));
                created_any = true;
                report.push(format!("created identity {label:?}"));
                heads.len() - 1
            }
        };
        env.store_blob(
            heads[idx].instance,
            AUTOFILL_PROFILE_KEY,
            &profile.to_bytes(),
        )
        .map_err(|e| format!("{e:?}"))?;
        report.push(format!("set profile for {label:?}"));
    }
    env.save(path).map_err(|e| e.to_string())?;
    if created_any {
        save_heads(path, &heads, active).map_err(|e| e.to_string())?;
    }
    report.push(format!("imported {} identities", rows.len()));
    Ok(report)
}

/// Apply a `key=value;key=value` spec to a profile (used by `profile --set`).
fn apply_profile_fields(p: &mut cerberus_autofill::Profile, spec: &str) -> Result<(), String> {
    for pair in spec.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad field {pair:?} (want key=value)"))?;
        let v = v.to_string();
        match k.trim() {
            "login.username" => p.login.username = v,
            "login.password" => p.login.password = v,
            "address.full_name" => p.address.full_name = v,
            "address.line1" => p.address.line1 = v,
            "address.line2" => p.address.line2 = v,
            "address.city" => p.address.city = v,
            "address.region" => p.address.region = v,
            "address.postal" => p.address.postal = v,
            "address.country" => p.address.country = v,
            "address.phone" => p.address.phone = v,
            "address.email" => p.address.email = v,
            "card.holder" => p.card.holder = v,
            "card.number" => p.card.number = v,
            "card.exp_month" => p.card.exp_month = v,
            "card.exp_year" => p.card.exp_year = v,
            "card.cvv" => p.card.cvv = v,
            // The host this profile's secrets are bound to (issue #12).
            "origin" => p.origin = v,
            other => return Err(format!("unknown field {other:?}")),
        }
    }
    Ok(())
}

/// Render a profile for display, redacting password/card secrets.
fn profile_lines(identity: usize, label: &str, p: &cerberus_autofill::Profile) -> Vec<String> {
    let redact = |s: &str| {
        if s.is_empty() {
            String::new()
        } else {
            "•".repeat(s.chars().count().min(8))
        }
    };
    let card = {
        let digits: String = p.card.number.chars().filter(char::is_ascii_digit).collect();
        if digits.len() > 4 {
            format!("•••• {}", &digits[digits.len() - 4..])
        } else {
            redact(&p.card.number)
        }
    };
    vec![
        format!("identity [{identity}] {label}"),
        format!("  login.username   : {}", p.login.username),
        format!("  login.password   : {}", redact(&p.login.password)),
        format!("  address.full_name: {}", p.address.full_name),
        format!("  address.line1    : {}", p.address.line1),
        format!("  address.city     : {}", p.address.city),
        format!("  address.postal   : {}", p.address.postal),
        format!("  address.country  : {}", p.address.country),
        format!("  address.email    : {}", p.address.email),
        format!("  card.number      : {card}"),
        format!(
            "  card.exp         : {}/{}",
            p.card.exp_month, p.card.exp_year
        ),
        format!("  card.cvv         : {}", redact(&p.card.cvv)),
        format!(
            "  origin           : {}",
            if p.origin.is_empty() {
                "(unbound — secrets won't autofill)"
            } else {
                &p.origin
            }
        ),
    ]
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
    network_client_with_proxies(system_roots, jar, proxy, HashMap::new())
}

/// Like [`network_client`], but with per-instance egress proxies (per-window
/// proxy): a fetch tagged with an instance in `proxies` tunnels through that
/// instance's own proxy; every other instance uses the default `proxy` (or
/// direct). One client serves all instances, so the mirror driver's shared
/// engine still routes each window through its own egress.
pub fn network_client_with_proxies(
    system_roots: bool,
    jar: Option<Arc<dyn CookieJar>>,
    proxy: Option<ProxyConfig>,
    proxies: HashMap<InstanceId, ProxyConfig>,
) -> Router {
    let provider = || {
        if system_roots {
            RustlsProvider::with_system_roots().unwrap_or_default()
        } else {
            RustlsProvider::new()
        }
    };
    // Multi-DoH then a system fallback (ADR-0006): try the encrypted resolvers
    // in order, and only if every one is unreachable fall back to the OS
    // resolver — so a network that blocks or mangles our DoH (e.g. answers the
    // POST with HTTP 505) can still browse. The system path is the only one that
    // exposes lookups to the local network, hence it is tried last.
    let dns = FallbackResolver::new(vec![
        Box::new(DohResolver::quad9(Box::new(provider()))),
        Box::new(DohResolver::cloudflare(Box::new(provider()))),
        Box::new(DohResolver::google(Box::new(provider()))),
        Box::new(SystemResolver),
    ]);
    // Cache positive resolutions briefly so a page's burst of same-host lookups
    // (document + every subresource + redirect hop) costs one DoH round-trip, not
    // one per connection (#39). The proxied path never resolves locally, so this
    // does not weaken that guarantee.
    let dns = CachingResolver::new(Box::new(dns));
    Router::with_proxies(Box::new(provider()), Box::new(dns), jar, proxy, proxies)
}

/// Parse each head's `proxy` string into a per-instance [`ProxyConfig`] map for
/// the network client. **Fail-closed:** a malformed proxy string is a hard
/// error rather than a silent fall-through to direct or default egress — a
/// window whose proxy is misconfigured must not quietly leak traffic around it,
/// matching the global `--proxy` behavior.
pub fn head_proxies(heads: &[Head]) -> Result<HashMap<InstanceId, ProxyConfig>, AppError> {
    let mut map = HashMap::new();
    for h in heads {
        if let Some(raw) = &h.proxy {
            let cfg = parse_proxy(raw).map_err(|e| {
                AppError::Net(format!(
                    "invalid proxy {raw:?} for identity {}: {e:?}",
                    h.label
                ))
            })?;
            map.insert(h.instance, cfg);
        }
    }
    Ok(map)
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
                .locked()
                .evaluate(instance, &origin, first_party)
                .decision;
            if decision != Decision::Allow {
                return None;
            }
        }
        let mut env = self.storage.locked();
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
            .locked()
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
            self.events.locked().push(event);
        }
        // For an accepted (Active) cookie, the user's disposition decides its
        // lifetime/persistence (Block drops it entirely). Quarantined cookies
        // keep the default until the user releases them.
        let disposition = if group == Group::Active {
            self.cookies
                .locked()
                .resolve(&first_party.site(), &cookie.name)
        } else {
            CookieDisposition::Allow
        };
        let mut env = self.storage.locked();
        let _ = env
            .instance(instance)
            .set_cookie_with(first_party, cookie, group, disposition);
    }
}

/// The first element with `tag` in this subtree (including `node`), depth-first
/// in document order.
fn find_styled_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
    if node.tag == tag {
        return Some(node);
    }
    node.children.iter().find_map(|c| match c {
        StyledChild::Element(e) => find_styled_tag(e, tag),
        StyledChild::Text(_) => None,
    })
}

/// The canvas (viewport) background. CSS propagates the root element's used
/// background to the whole canvas; when the root `<html>` paints none, the
/// `<body>`'s background propagates instead. A translucent color is composited
/// over `fallback` (the default white canvas); a fully transparent one — or the
/// absence of both — leaves `fallback` showing. Without this the body's short,
/// auto-height box paints its color only near the top and the rest of the
/// viewport stays white, unlike Chrome which fills the whole page (e.g.
/// example.com's `#f0f0f2` body background).
fn canvas_background(styled: &StyledDom, fallback: Color) -> Color {
    fn resolve(bg: Option<Color>, base: Color) -> Option<Color> {
        let c = bg?;
        match c.a {
            0 => None,
            255 => Some(c),
            a => {
                let mix = |f: u8, b: u8| {
                    ((f as u16 * a as u16 + b as u16 * (255 - a as u16)) / 255) as u8
                };
                Some(Color::rgb(
                    mix(c.r, base.r),
                    mix(c.g, base.g),
                    mix(c.b, base.b),
                ))
            }
        }
    }
    let root = &styled.root;
    let html = find_styled_tag(root, "html").unwrap_or(root);
    if let Some(bg) = resolve(html.style.background, fallback) {
        return bg;
    }
    if let Some(body) = find_styled_tag(html, "body") {
        if let Some(bg) = resolve(body.style.background, fallback) {
            return bg;
        }
    }
    fallback
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
        .locked()
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
    // Per-window proxy (ADR-0047): a one-shot render fetches under the active
    // head's instance, so it egresses through that head's own proxy if set —
    // matching the interactive browser and mirror driver. The global `--proxy`
    // above is the default for a head with none. Fail-closed on a bad string.
    let proxies = head_proxies(heads.heads())?;
    let fetch_t = Instant::now();
    let (response, active_ua, client) = if url.is_builtin() {
        let resp = BuiltinHttpClient
            .get(&url)
            .map_err(|e| AppError::Net(format!("{e:?}")))?;
        (resp, DEFAULT_USER_AGENT.to_string(), None)
    } else {
        let client =
            network_client_with_proxies(config.system_roots, Some(jar.clone()), proxy, proxies);
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
            cookie: cookie_seed(&storage, active_instance, &first_party, &first_party),
        };
        // JS fetch() rides the page's subresource context (sealed jar + consent),
        // performed synchronously here (the one-shot path already blocks).
        let fetch_ctx = FetchContext {
            instance: active_instance,
            kind: FetchKind::Subresource {
                first_party: first_party.clone(),
            },
        };
        document = match &client {
            Some(c) => {
                let mut fc = SyncFetchClient {
                    client: c,
                    base: Some(url.clone()),
                    ctx: fetch_ctx,
                };
                run_page_scripts_with_fetch(
                    engine,
                    base_realm,
                    &document,
                    document.scripts(),
                    &env,
                    &mut fc,
                )
                .map_err(|e| AppError::Js(format!("{e:?}")))?
            }
            None => run_page_scripts(engine, base_realm, &document, document.scripts(), &env)
                .map_err(|e| AppError::Js(format!("{e:?}")))?,
        };
        // Symmetric to the `cookie` seed above: persist any cookies the page's
        // scripts set via `document.cookie` into the sealed jar, through the same
        // consent gate and per-cookie disposition a network `Set-Cookie` takes.
        // First-party only (a top-level page: request origin == first party). With
        // `--data-dir` this is saved below, so a script-set token (e.g. a bot
        // challenge's) survives to a later render sharing the profile.
        if let Ok(writes) = take_cookie_writes(engine, base_realm) {
            for value in writes {
                jar.set_cookie(active_instance, &url, &first_party, &value);
            }
        }
    }
    let engine_name = engine.name().to_string();
    let realms_live = engine.realm_count();
    let engines_live = heads.engines_live();
    timings.record("scripts", scripts_t.elapsed());

    // Inline `<svg>` subtrees become synthetic replaced elements backed by the
    // existing SVG raster path (ADR-0009): serialized, content-hash keyed, and
    // rewritten to `<img src="cerb-inline-svg:…">` *before* styling, so
    // layout's replaced-element sizing (and CSS overrides) applies. The
    // payloads decode into the image store below.
    let inline_svgs = replace_inline_svgs(&mut document, config.viewport.w);

    // Subresource context (sealed jar + consent) shared by this page's external
    // CSS and images, so both carry/capture cookies under the same first party.
    let sub_ctx = FetchContext {
        instance: active_instance,
        kind: FetchKind::Subresource {
            first_party: first_party.clone(),
        },
    };

    // External `<link>` stylesheets are fetched up front (render-blocking) so the
    // cascade sees them before styling — the one-shot path is synchronous, and
    // third-party sheets are consent-gated like any other subresource (ADR-0037).
    let sheets = match &client {
        Some(client) => {
            fetch_stylesheets_sync(&document, &url, client, &sub_ctx, &consent, &first_party)
        }
        None => ExternalSheets::new(),
    };
    let style_t = Instant::now();
    // Evaluate @media against the actual render viewport, so width/height queries
    // (responsive breakpoints) select the same layout Chrome shows at this size —
    // not a hardcoded desktop default that can disagree with the layout width.
    let styled = CssEngine::with_media(config.viewport.w, config.viewport.h)
        .style_with_sheets(&document, &sheets);
    timings.record("style", style_t.elapsed());

    // The page content area (below the toolbar) is the viewport layout runs at.
    // The image fetch must resolve srcset/<picture> against this SAME viewport so
    // the fetched candidate is the one drawn (ADR-0046): `content_size` keeps the
    // width but shrinks the height by the toolbar, and <picture> `media` can key
    // on height/orientation — so passing the full window height here would let a
    // height/orientation <source> be fetched that layout never selects.
    let content = Toolbar::new(active_label.clone()).content_size(config.viewport);

    // Fetch + decode this page's images up front (the one-shot path is
    // synchronous; the interactive browser fetches them on its worker). Built-in
    // pages reference no network images.
    let image_policy = ImagePolicy {
        default: config.image_mode,
        overrides: config.text_only_images.clone(),
    };
    let mut images = match &client {
        Some(client) => fetch_images_sync(
            &document,
            &styled,
            &url,
            client,
            &sub_ctx,
            &consent,
            &first_party,
            content.w,
            content.h,
            &image_policy,
        ),
        None => HashMap::new(),
    };
    // Register the page's inline SVGs (first-party document content — no fetch,
    // no consent gate) under their synthetic keys, decoded through the same
    // codec (and byte/size ceilings) as an SVG file. A payload the decoder
    // declines still reserves its box: a transparent stand-in keeps the space
    // Chrome would reserve without painting a placeholder.
    if !inline_svgs.is_empty() {
        let codec = ImageCodec::new();
        for (key, bytes) in &inline_svgs {
            let state = match codec.decode(bytes) {
                Ok(img) => ImageState::Ready(Arc::new(img)),
                Err(_) => ImageState::Ready(Arc::new(transparent_stand_in())),
            };
            images.insert(key.clone(), state);
        }
    }
    let subresources_blocked = images
        .values()
        .filter(|s| matches!(s, ImageState::Blocked))
        .count();
    let images_text_only = images
        .values()
        .filter(|s| matches!(s, ImageState::TextOnly))
        .count();
    // Text-only images were never requested, so they don't count as requested.
    let images_requested = images.len() - subresources_blocked - images_text_only;
    let images_decoded = images
        .values()
        .filter(|s| matches!(s, ImageState::Ready(_)))
        .count();
    let provider = StoreImages {
        base: Some(&url),
        images: &images,
        policy: &image_policy,
    };

    // Cookies now resident for this page's site — captured from the real
    // responses through the sealed jar (zero for builtin/cookieless pages).
    let active_cookies = {
        let mut env = storage.locked();
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
    // `content` (the page viewport below the toolbar) was computed above and used
    // for the image fetch; layout reuses it so fetch and draw share one viewport.

    // Lay out + paint the page into the content area only. The canvas background
    // is the root/body background propagated to the viewport (CSS), not just the
    // page's default white — so a page whose `<body>` sets a color fills the
    // whole content area, matching Chrome.
    let canvas_bg = canvas_background(&styled, config.background);
    let layout_t = Instant::now();
    let mut layout = make_layout(config.layout_engine);
    let (page, laid) = render_document_laid(
        &styled,
        content,
        canvas_bg,
        &mut *layout,
        &text,
        &text,
        &provider,
        &NoForms,
    );
    timings.record("layout+paint", layout_t.elapsed());
    timings.record_page_load();

    // Paint forensics: CERB_PAINT_PROBE=x,y prints every display item whose
    // rect covers that content-coordinate pixel, in paint order — the fastest
    // way to answer "what painted this wrong pixel" on a live page.
    if let Ok(probe) = std::env::var("CERB_PAINT_PROBE") {
        if let Some((px, py)) = probe
            .split_once(',')
            .and_then(|(a, b)| Some((a.trim().parse::<i32>().ok()?, b.trim().parse::<i32>().ok()?)))
        {
            eprintln!("paint probe at ({px},{py}), canvas_bg {canvas_bg:?}:");
            for (i, item) in laid.display.items.iter().enumerate() {
                let hit = |r: &cerberus_types::Rect| {
                    px >= r.x
                        && py >= r.y
                        && px < r.x + r.w.max(1) as i32
                        && py < r.y + r.h.max(1) as i32
                };
                use cerberus_paint::DisplayItem as D;
                match item {
                    D::Rect { rect, color } if hit(rect) => {
                        eprintln!("  [{i}] Rect {rect:?} {color:?}")
                    }
                    D::RoundRect { rect, color, .. } if hit(rect) => {
                        eprintln!("  [{i}] RoundRect {rect:?} {color:?}")
                    }
                    D::Gradient { rect, start, .. } if hit(rect) => {
                        eprintln!("  [{i}] Gradient {rect:?} start {start:?}")
                    }
                    D::Image { rect, .. } if hit(rect) => eprintln!("  [{i}] Image {rect:?}"),
                    D::ClipPush { rect } if hit(rect) => eprintln!("  [{i}] ClipPush {rect:?}"),
                    _ => {}
                }
            }
        }
    }

    // Compose: page under the toolbar, toolbar painted on top.
    let mut framebuffer = Framebuffer::new(config.viewport);
    framebuffer.clear(canvas_bg);
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
        page_text: config.dump_text.then(|| visible_text(&styled.root)),
        links: laid.links,
        fields: laid.fields,
        timings: if config.timers {
            timings.as_pairs()
        } else {
            Vec::new()
        },
        framebuffer: surface.last_frame().cloned().unwrap_or(framebuffer),
    })
}

/// Vault key under which each identity's autofill `Profile` is sealed.
const AUTOFILL_PROFILE_KEY: &str = "autofill.profile";

/// Build a multi-window **mirror shell** (the `run --mirror` entry, ADR-0018):
/// every identity in the profile becomes a driven window over the shared privacy
/// stack (sealed per-instance cookies, consent, proxy). The first identity is the
/// master; the rest mirror it and catch up when focused. Reuses the same
/// storage/consent/jar setup as the interactive browser, but drives the group
/// through the synchronous network client (mirror catch-up is synchronous).
pub fn build_mirror_shell(
    options: AppOptions,
) -> Result<mirror::MirrorShell, cerberus_mirror::MirrorError> {
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
    let pending: Arc<Mutex<Vec<ConsentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let jar: Arc<dyn CookieJar> = Arc::new(SealedJar {
        storage: storage.clone(),
        policy: consent.clone(),
        cookies: cookie_policy.clone(),
        events: pending,
    });
    let heads = match &data_dir {
        Some(dir) => load_heads(dir).map(|(h, _active)| h).unwrap_or_else(|| {
            let h = fresh_profile_heads();
            if let Err(e) = save_heads(dir, &h, 0) {
                eprintln!("cerberus: cannot save heads: {e}");
            }
            h
        }),
        None => default_heads(),
    };
    let proxy = options
        .proxy
        .as_deref()
        .map(|p| parse_proxy(p).unwrap_or_else(|e| panic!("invalid --proxy {p:?}: {e:?}")));
    // Per-window proxy: each identity may egress through its own proxy while the
    // group shares one client. Fail-closed like the global proxy above — a bad
    // per-head proxy string aborts rather than silently leaking around it.
    let proxies =
        head_proxies(&heads).unwrap_or_else(|e| panic!("invalid per-identity proxy: {e:?}"));
    let client: Arc<dyn HttpClient> = Arc::new(network_client_with_proxies(
        options.system_roots,
        Some(jar),
        proxy,
        proxies,
    ));
    let source = Box::new(mirror::AppPageSource::new(client));
    let manager = HeadManager::new(heads, Box::new(QuickJsEngineFactory));

    // Load any vault-sealed autofill profiles (empty if the vault is locked), so
    // one master Fill fills every window from its own profile.
    let mut profiles = HashMap::new();
    {
        let mut env = storage.locked();
        for head in manager.heads() {
            if let Ok(Some(bytes)) = env.load_blob(head.instance, AUTOFILL_PROFILE_KEY) {
                if let Some(p) = cerberus_autofill::Profile::from_bytes(&bytes) {
                    profiles.insert(head.instance, p);
                }
            }
        }
    }

    let mut group =
        mirror::mirror_group_from_heads(&manager, source, (1280, 800), DEFAULT_USER_AGENT)?;
    if !profiles.is_empty() {
        // Keep a handle to the same provider alongside the one the group gets,
        // so a later `lock_vault()` can clear the decrypted profiles out from
        // under it (issue #17) without the group needing to know about vaults.
        let fill_provider = mirror::ProfileFillProvider::new(profiles);
        group.set_fill_provider(Box::new(fill_provider.clone()));
        return Ok(mirror::MirrorShell::with_vault(
            group,
            storage,
            fill_provider,
        ));
    }
    Ok(mirror::MirrorShell::new(group))
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
/// A request body for a POST navigation (form submission). GET navigations carry
/// `None`; the data then rides in the URL query instead.
#[derive(Clone, Debug)]
struct PostBody {
    content_type: String,
    body: Vec<u8>,
}

struct Pending {
    id: u64,
    /// If this load is an https upgrade of an `http` URL, the original URL — so a
    /// failure can offer the risk prompt.
    http_fallback: Option<String>,
    /// The POST body, if this navigation is a form POST (so the load goes through
    /// `fetch_in` with a body instead of a cacheable GET, and an https-upgrade
    /// fallback can re-POST).
    post: Option<PostBody>,
}

/// A job for the network worker. The `FetchContext` travels by value: it must
/// reflect the instance/first-party at *queue* time (a head switch mid-flight
/// must not re-attribute the fetch).
enum Job {
    Page {
        id: u64,
        url: String,
        post: Option<PostBody>,
        ctx: FetchContext,
    },
    Sub {
        url: String,
        ctx: FetchContext,
    },
    Fetch {
        id: u64,
        req: FetchRequest,
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
    Fetch {
        id: u64,
        result: Result<FetchResponse, String>,
    },
}

/// Performs page + sub-resource loads off the UI thread. Abstracted so the load
/// state machine is testable without the network (see `FakeLoader` in tests).
trait PageLoader {
    /// Queue a page navigation in an identity context. `post` is `Some` for a
    /// form POST (sent as a body), `None` for a GET.
    fn request(&self, id: u64, url: String, post: Option<PostBody>, ctx: FetchContext);
    /// Queue an image sub-resource fetch (absolute URL) in an identity context.
    fn request_subresource(&self, url: String, ctx: FetchContext);
    /// Queue a JS `fetch` (absolute URL in `req.url`) in an identity context.
    fn request_fetch(&self, id: u64, req: FetchRequest, ctx: FetchContext);
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
    _workers: Vec<JoinHandle<()>>,
}

impl NetLoader {
    fn new(
        system_roots: bool,
        jar: Option<Arc<dyn CookieJar>>,
        proxy: Option<ProxyConfig>,
        proxies: HashMap<InstanceId, ProxyConfig>,
    ) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<Job>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Done>();
        let waker: Arc<Mutex<Option<Arc<dyn Waker>>>> = Arc::new(Mutex::new(None));

        // A small pool of workers shares one job queue, so a page's many
        // subresources fetch concurrently instead of one-at-a-time (a single
        // worker made image-heavy pages crawl). Each worker owns its client
        // (rustls config) and dequeues under a short-held lock, then fetches
        // unlocked — so a burst of queued jobs runs in parallel.
        const WORKERS: usize = 4;
        let req_rx = Arc::new(Mutex::new(req_rx));
        let workers = (0..WORKERS)
            .map(|_| {
                let req_rx = req_rx.clone();
                let out_tx = out_tx.clone();
                let worker_waker = waker.clone();
                let jar = jar.clone();
                let proxy = proxy.clone();
                let proxies = proxies.clone();
                std::thread::spawn(move || {
                    let client = network_client_with_proxies(system_roots, jar, proxy, proxies);
                    loop {
                        // Hold the lock only for the dequeue (released at the `;`),
                        // then fetch unlocked so other workers proceed in parallel.
                        let job = req_rx.locked().recv();
                        let job = match job {
                            Ok(job) => job,
                            Err(_) => break, // all senders dropped
                        };
                        let done = match job {
                            Job::Page { id, url, post, ctx } => {
                                let result = fetch_page(&client, &url, post.as_ref(), &ctx);
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
                            Job::Fetch { id, req, ctx } => {
                                let result = perform_fetch(&client, &req.url, &req, &ctx);
                                Done::Fetch { id, result }
                            }
                        };
                        if out_tx.send(done).is_err() {
                            break;
                        }
                        if let Some(w) = worker_waker.locked().clone() {
                            w.wake();
                        }
                    }
                })
            })
            .collect();

        Self {
            tx: req_tx,
            rx: out_rx,
            waker,
            _workers: workers,
        }
    }
}

impl PageLoader for NetLoader {
    fn request(&self, id: u64, url: String, post: Option<PostBody>, ctx: FetchContext) {
        let _ = self.tx.send(Job::Page { id, url, post, ctx });
    }
    fn request_subresource(&self, url: String, ctx: FetchContext) {
        let _ = self.tx.send(Job::Sub { url, ctx });
    }
    fn request_fetch(&self, id: u64, req: FetchRequest, ctx: FetchContext) {
        let _ = self.tx.send(Job::Fetch { id, req, ctx });
    }
    fn try_recv(&mut self) -> Option<Done> {
        self.rx.try_recv().ok()
    }
    fn set_waker(&mut self, waker: Arc<dyn Waker>) {
        *self.waker.locked() = Some(waker);
    }
}

fn fetch_page(
    client: &Router,
    url: &str,
    post: Option<&PostBody>,
    ctx: &FetchContext,
) -> Result<FetchedPage, String> {
    let parsed = parse_url(url).map_err(|e| e.to_string())?;
    let t = std::time::Instant::now();
    // A form POST sends a body through `fetch_in` (the same path JS `fetch` uses);
    // a normal navigation is a cacheable GET.
    let resp = match post {
        Some(post) => {
            let headers = vec![("content-type".to_string(), post.content_type.clone())];
            client
                .fetch_in(&parsed, "POST", &headers, &post.body, ctx)
                .map_err(|e| format!("{e:?}"))?
        }
        None => client.get_in(&parsed, ctx).map_err(|e| format!("{e:?}"))?,
    };
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

/// Cap on a JS-`fetch` response body we keep as text (protects the RSS budget,
/// per the image-decode-budget philosophy).
const MAX_FETCH_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Perform one JS `fetch` (already-resolved absolute `abs_url`) through the
/// privacy stack and shape it into a js-dom [`FetchResponse`]. Shared by the
/// one-shot [`SyncFetchClient`] and the interactive worker (M12d / ADR-0014).
fn perform_fetch(
    client: &Router,
    abs_url: &str,
    req: &FetchRequest,
    ctx: &FetchContext,
) -> Result<FetchResponse, String> {
    let parsed = parse_url(abs_url).map_err(|e| e.to_string())?;
    let resp = client
        .fetch_in(&parsed, &req.method, &req.headers, req.body.as_bytes(), ctx)
        .map_err(|e| format!("{e:?}"))?;
    if resp.body.len() > MAX_FETCH_BODY_BYTES {
        return Err(format!(
            "response body exceeds {MAX_FETCH_BODY_BYTES} bytes"
        ));
    }
    Ok(FetchResponse {
        status: resp.status,
        status_text: reason_phrase(resp.status).to_string(),
        url: abs_url.to_string(),
        headers: resp.headers,
        body: String::from_utf8_lossy(&resp.body).into_owned(),
    })
}

/// A minimal HTTP reason phrase for `response.statusText` (empty for uncommon
/// codes — pages rarely depend on it).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// A synchronous [`FetchClient`] over the network [`Router`], used by the
/// one-shot [`render`] path (already synchronous). Relative request URLs resolve
/// against `base` (the page URL). The interactive browser routes fetches through
/// its worker instead (non-blocking; see `pump_fetches`).
struct SyncFetchClient<'a> {
    client: &'a Router,
    base: Option<cerberus_url::Url>,
    ctx: FetchContext,
}

impl FetchClient for SyncFetchClient<'_> {
    fn fetch(&mut self, req: &FetchRequest) -> Result<FetchResponse, String> {
        let abs = resolve_subresource(self.base.as_ref(), &req.url);
        if !(abs.starts_with("http://") || abs.starts_with("https://")) {
            return Err(format!("unsupported fetch URL: {abs}"));
        }
        perform_fetch(self.client, &abs, req, &self.ctx)
    }
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

#[allow(clippy::too_many_arguments)]
fn fetch_images_sync(
    document: &Document,
    styled: &StyledDom,
    base: &Url,
    client: &Router,
    ctx: &FetchContext,
    policy: &Mutex<DefaultDenyPolicy>,
    first_party: &Origin,
    viewport_w: u32,
    viewport_h: u32,
    images: &ImagePolicy,
) -> HashMap<String, ImageState> {
    // Collect <img> srcs and CSS background-image srcs separately: the text-only
    // option is an <img> feature (it has an alt/caption to show as text), so a
    // policy-matched *background* must still fetch and paint — a CSS background
    // has no text substitute and would otherwise vanish silently.
    let mut img_srcs = Vec::new();
    collect_image_urls(document.root(), &mut img_srcs, viewport_w, viewport_h);
    let img_urls: std::collections::HashSet<String> = img_srcs
        .iter()
        .map(|s| resolve_subresource(Some(base), s))
        .collect();

    let mut srcs = img_srcs;
    collect_bg_image_urls(&styled.root, &mut srcs);

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
        // Text-only images render as their alt/caption and are never fetched or
        // decoded — checked before the consent and decode-budget gates so they
        // cost no network, memory, or budget. Scoped to <img> URLs: a CSS
        // background that happens to match the policy has no text substitute and
        // must still fetch and paint (see the collector comment above).
        if img_urls.contains(&url) && images.text_only(&url) {
            out.insert(url, ImageState::TextOnly);
            continue;
        }
        // Consent gate: unruled third-party subresources never hit the network.
        let allowed = parse_url(&url)
            .ok()
            .and_then(|u| u.origin())
            .is_some_and(|origin| {
                policy
                    .locked()
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

/// A 1×1 fully transparent bitmap: the stand-in for an inline SVG the decoder
/// declined (byte-ceiling bomb, malformed markup). The synthetic `<img>`
/// carries both width/height attributes, so layout stretches this invisible
/// pixel over the exact box Chrome would reserve — space preserved, nothing
/// painted (a grey placeholder for a broken decorative icon would be noisier
/// than the blank Chrome shows).
fn transparent_stand_in() -> DecodedImage {
    DecodedImage {
        size: Size::new(1, 1),
        rgba: vec![0, 0, 0, 0],
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
    /// The user chose to render this image as text (the text-only option): its
    /// bytes were never fetched or decoded. Layout draws its alt/caption chip.
    /// Kept distinct from `Blocked` so the consent-blocked count stays honest.
    TextOnly,
}

/// Image provider over the browser's per-page store. Resolves an element's
/// `src` against the current page URL (which is how the store is keyed).
struct StoreImages<'a> {
    base: Option<&'a Url>,
    images: &'a HashMap<String, ImageState>,
    /// The text-only policy, consulted per resolved URL so the render decision
    /// matches the fetch skip exactly (and works for `data:`/non-stored images).
    policy: &'a ImagePolicy,
}

impl ImageProvider for StoreImages<'_> {
    fn get(&self, src: &str) -> Option<Arc<DecodedImage>> {
        match self.images.get(&resolve_subresource(self.base, src)) {
            Some(ImageState::Ready(img)) => Some(img.clone()),
            _ => None,
        }
    }

    fn render_as_text(&self, src: &str) -> bool {
        self.policy.text_only(&resolve_subresource(self.base, src))
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

/// The page's user-visible text (for `--dump-text`): walks the **styled** tree
/// so it honors computed `display` — a `display: none` / `[hidden]` subtree
/// contributes no text, matching what is actually painted. `<script>`/`<style>`
/// payloads are code, not page text, and are skipped too. (Walking the raw DOM
/// would leak text from hidden elements.)
fn visible_text(root: &StyledNode) -> String {
    // Block-level boxes each start on their own line, so their text must not run
    // into a sibling's (`<li>one</li><li>two</li>` reads as "one\ntwo", not
    // "onetwo"); `<br>` is a hard break. Inline content still concatenates. This
    // keeps `--dump-text` a faithful reading-order transcript for the correctness
    // oracle (#41) rather than one undifferentiated run.
    fn is_block(node: &StyledNode) -> bool {
        matches!(
            node.style.display,
            Display::Block | Display::ListItem | Display::Flex | Display::Grid
        )
    }
    // Ensure the buffer ends with exactly one newline separator (no blank runs).
    fn separate(out: &mut String) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    }
    fn walk(node: &StyledNode, out: &mut String, opacity_hidden: bool) {
        // A <select>'s options render as the control's value, not page text —
        // dumping every option's label made hidden choices read as visible.
        if node.style.display == Display::None
            || matches!(node.tag.as_str(), "script" | "style" | "select")
        {
            return;
        }
        // Paint-faithful visibility, mirroring layout: an `opacity:0` subtree is
        // gone entirely; `visibility:hidden` hides THIS node's text while a
        // descendant may still revert to visible.
        let hidden = opacity_hidden || node.style.opacity == 0.0;
        let text_visible = !hidden && node.style.visibility == cerberus_style::Visibility::Visible;
        if node.tag == "br" {
            out.push('\n');
            return;
        }
        for child in &node.children {
            match child {
                StyledChild::Text(t) => {
                    if text_visible {
                        out.push_str(t)
                    }
                }
                StyledChild::Element(e) => {
                    let block = is_block(e);
                    if block {
                        separate(out);
                    }
                    walk(e, out, hidden);
                    if block {
                        separate(out);
                    }
                }
            }
        }
    }
    let mut out = String::new();
    walk(root, &mut out, false);
    out.trim().to_string()
}

/// Collect `<img>` sources from an element subtree, resolving `srcset`/`sizes`/
/// `data-src` to the same URL layout will draw (ADR-0046), so the fetched bytes
/// are the ones the page looks up. `viewport_w` is the layout viewport width.
fn collect_image_urls(node: NodeRef<'_>, out: &mut Vec<String>, viewport_w: u32, viewport_h: u32) {
    if node.tag() == "picture" {
        // Resolve the <picture> to the one URL its direct <img> will actually
        // load (type/media selection), matching what layout draws (ADR-0046).
        // With a direct <img>, don't descend: its <source>/other children are
        // subsumed by this choice. With NO direct <img> (invalid, but possible)
        // fall through to normal recursion so nested content is still collected —
        // exactly as layout falls through to render it.
        if let Some(img) = node.children().find(|c| c.tag() == "img") {
            let sources: Vec<PictureSource<'_>> = node
                .children()
                .filter(|c| c.tag() == "source")
                .map(|s| PictureSource {
                    type_: s.attr("type"),
                    media: s.attr("media"),
                    srcset: s.attr("srcset"),
                    sizes: s.attr("sizes"),
                })
                .collect();
            if let Some(src) = pick_picture_url(&sources, |n| img.attr(n), viewport_w, viewport_h) {
                out.push(src);
            }
            return;
        }
    }
    if node.tag() == "img" {
        if let Some(src) = pick_img_url(|n| node.attr(n), viewport_w) {
            out.push(src);
        }
    }
    for child in node.children() {
        if child.is_element() {
            collect_image_urls(child, out, viewport_w, viewport_h);
        }
    }
}

/// Collect `background-image` URLs from the styled tree (they live in computed
/// style, not the DOM), so they fetch through the same image pipeline (ADR-0038).
fn collect_bg_image_urls(node: &StyledNode, out: &mut Vec<String>) {
    if let Some(url) = &node.style.background_image {
        out.push(url.clone());
    }
    for c in &node.children {
        if let StyledChild::Element(e) = c {
            collect_bg_image_urls(e, out);
        }
    }
}

/// Collect the `href`s (as written) of every `<link rel="stylesheet">`, in
/// document order. The raw href is the key the cascade looks up, so it is kept
/// verbatim here and resolved to an absolute URL only for fetching (ADR-0037).
fn collect_stylesheet_links(node: NodeRef<'_>, out: &mut Vec<String>) {
    if node.tag() == "link" && link_is_stylesheet(node) {
        if let Some(href) = node.attr("href") {
            out.push(href.to_string());
        }
    }
    for child in node.children() {
        if child.is_element() {
            collect_stylesheet_links(child, out);
        }
    }
}

/// Whether a `<link>` is a stylesheet (`rel` is a case-insensitive token list).
fn link_is_stylesheet(node: NodeRef<'_>) -> bool {
    node.attr("rel").is_some_and(|rel| {
        rel.split_whitespace()
            .any(|t| t.eq_ignore_ascii_case("stylesheet"))
    })
}

/// Collect every `<script src="…">` value in document order (external scripts —
/// inline `<script>` bodies are handled separately via [`Document::scripts`]).
/// The raw `src` is returned; the caller resolves it against the page URL.
fn collect_external_scripts(node: NodeRef<'_>, out: &mut Vec<String>) {
    if node.tag() == "script" {
        if let Some(src) = node.attr("src") {
            if !src.trim().is_empty() {
                out.push(src.to_string());
            }
        }
    }
    for child in node.children() {
        if child.is_element() {
            collect_external_scripts(child, out);
        }
    }
}

/// Synchronously fetch every `<link rel="stylesheet">` body, keyed by the link's
/// raw `href` (what the cascade looks up). Used by the one-shot [`render`] (the
/// interactive browser fetches them on its worker). Third-party sheets are
/// consent-gated like images; a blocked or failed sheet simply contributes no
/// CSS. Builds no client and returns empty when there are no http(s) links.
fn fetch_stylesheets_sync(
    document: &Document,
    base: &Url,
    client: &Router,
    ctx: &FetchContext,
    policy: &Mutex<DefaultDenyPolicy>,
    first_party: &Origin,
) -> ExternalSheets {
    let mut hrefs = Vec::new();
    collect_stylesheet_links(document.root(), &mut hrefs);
    let mut sheets = ExternalSheets::new();
    for href in hrefs {
        if sheets.contains_key(&href) {
            continue;
        }
        let abs = resolve_subresource(Some(base), &href);
        if !(abs.starts_with("http://") || abs.starts_with("https://")) {
            continue;
        }
        // Consent gate: unruled third-party stylesheets never hit the network.
        if !subresource_allowed(&abs, policy, ctx.instance, first_party) {
            continue;
        }
        if let Ok(bytes) = fetch_bytes(client, &abs, ctx) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // Resolve any `@import`ed sheets (relative to this sheet) and inline
            // them ahead of this sheet's own rules (ADR-0038).
            let text = inline_imports(&text, &abs, client, ctx, policy, first_party, 0);
            sheets.insert(href, text);
        }
    }
    sheets
}

/// Whether a subresource at `abs` is allowed under the consent policy (same-origin
/// first-party loads; cross-site needs an Allow rule).
fn subresource_allowed(
    abs: &str,
    policy: &Mutex<DefaultDenyPolicy>,
    instance: InstanceId,
    first_party: &Origin,
) -> bool {
    parse_url(abs)
        .ok()
        .and_then(|u| u.origin())
        .is_some_and(|origin| {
            policy
                .locked()
                .evaluate(instance, &origin, first_party)
                .decision
                == Decision::Allow
        })
}

/// Recursively inline `@import`ed stylesheets, resolved against `base` and
/// consent-gated, ahead of the importing sheet's rules (bounded depth). Imports
/// we can't fetch are dropped; the cascade parser skips any leftover `@import`.
///
/// Discovery is intentionally split into a pure, lex-aware scan
/// ([`inline_imports_core`] over [`prologue_import_spans`]): a raw substring hunt
/// for `@import` (issue #64) would fetch URLs sitting inside comments or string
/// values — a consent-gated request triggered by attacker-controlled *content* —
/// so fetching is threaded through a closure the scanner only calls for genuine
/// prologue at-rules.
fn inline_imports(
    css: &str,
    base: &str,
    client: &Router,
    ctx: &FetchContext,
    policy: &Mutex<DefaultDenyPolicy>,
    first_party: &Origin,
    depth: usize,
) -> String {
    if depth >= 4 {
        return css.to_string();
    }
    let base_url = parse_url(base).ok();
    inline_imports_core(css, &mut |url| {
        let abs = resolve_subresource(base_url.as_ref(), url);
        if !(abs.starts_with("http://") || abs.starts_with("https://"))
            || !subresource_allowed(&abs, policy, ctx.instance, first_party)
        {
            return None;
        }
        let bytes = fetch_bytes(client, &abs, ctx).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Some(inline_imports(
            &text,
            &abs,
            client,
            ctx,
            policy,
            first_party,
            depth + 1,
        ))
    })
}

/// Splice each legal prologue `@import` (found by [`prologue_import_spans`]) with
/// the CSS `fetch` yields for its URL, dropping the original at-rule; text around
/// the imports is emitted verbatim and in order, so a successful import's content
/// lands ahead of the sheet's own rules. Factored out of [`inline_imports`] so the
/// discovery/ordering logic is testable without a live network `Router`.
fn inline_imports_core(css: &str, fetch: &mut impl FnMut(&str) -> Option<String>) -> String {
    // Cheap bail-out: no literal `@import` anywhere means nothing to inline.
    if !css.contains("@import") {
        return css.to_string();
    }
    let spans = prologue_import_spans(css);
    if spans.is_empty() {
        return css.to_string();
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (start, end) in spans {
        out.push_str(&css[cursor..start]);
        if let Some(url) = parse_import_url(&css[start..end]) {
            if let Some(inlined) = fetch(&url) {
                out.push_str(&inlined);
                out.push('\n');
            }
        }
        cursor = end;
    }
    out.push_str(&css[cursor..]);
    out
}

/// Byte spans (`@`..just past the terminating `;`) of the `@import` statements
/// that are *valid* per the CSS spec: they sit at code positions (never inside a
/// comment or string) in the sheet's prologue, where only `@charset` and `@layer`
/// statements may precede them. The first ordinary style rule — or any other
/// at-rule or `@layer` block — closes the prologue and stops the scan, so a stray
/// `@import` deeper in the sheet is ignored (issue #64).
fn prologue_import_spans(css: &str) -> Vec<(usize, usize)> {
    let bytes = css.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    loop {
        i = skip_ws_and_comments(bytes, i);
        // Only an at-rule can extend the prologue; anything else (a selector, and
        // thus the start of a style rule) ends it.
        if i >= bytes.len() || bytes[i] != b'@' {
            break;
        }
        let kw_end = ident_end(bytes, i + 1);
        let keyword = &css[i + 1..kw_end];
        if keyword.eq_ignore_ascii_case("import") {
            let end = statement_end(bytes, kw_end);
            spans.push((i, end));
            i = end;
        } else if keyword.eq_ignore_ascii_case("charset") {
            i = statement_end(bytes, kw_end);
        } else if keyword.eq_ignore_ascii_case("layer") {
            // Only the *statement* form (`@layer a, b;`) keeps the prologue open;
            // a `@layer { … }` block is an ordinary rule that closes it.
            match statement_or_block_end(bytes, kw_end) {
                Some(end) => i = end,
                None => break,
            }
        } else {
            break;
        }
    }
    spans
}

/// Advance past ASCII whitespace and `/* … */` comments (comments may not nest in
/// CSS), returning the next code position.
fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match skip_comment(bytes, i) {
            Some(next) => i = next,
            None => return i,
        }
    }
}

/// If `bytes[i..]` opens a `/* … */` comment, return the index just past its `*/`
/// (or end-of-input for an unterminated comment); otherwise `None`.
fn skip_comment(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'/') || bytes.get(i + 1) != Some(&b'*') {
        return None;
    }
    let mut j = i + 2;
    while j + 1 < bytes.len() {
        if bytes[j] == b'*' && bytes[j + 1] == b'/' {
            return Some(j + 2);
        }
        j += 1;
    }
    Some(bytes.len())
}

/// Index just past a `"…"`/`'…'` string literal that opens at `bytes[i]`,
/// honouring backslash escapes (or end-of-input if unterminated).
fn skip_string(bytes: &[u8], i: usize) -> usize {
    let quote = bytes[i];
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            c if c == quote => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// End of an identifier (`[A-Za-z0-9_-]*`) starting at `start` — the at-keyword
/// after a leading `@`.
fn ident_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
    {
        i += 1;
    }
    i
}

/// Index just past the `;` that ends the statement beginning at `from`, skipping
/// `;`s that live inside comments or string values (or end-of-input if none).
fn statement_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        if let Some(next) = skip_comment(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(bytes, i),
            b';' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// For a `@layer` at-rule beginning at `from`: `Some(end)` (just past `;`) when it
/// is a bare *statement*, or `None` when a `{` arrives first (a `@layer` block).
/// Comments and strings are skipped so their `;`/`{` don't mislead the decision.
fn statement_or_block_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if let Some(next) = skip_comment(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(bytes, i),
            b';' => return Some(i + 1),
            b'{' => return None,
            _ => i += 1,
        }
    }
    Some(bytes.len())
}

/// Extract the URL from an `@import url("…")` / `@import "…"` statement.
fn parse_import_url(stmt: &str) -> Option<String> {
    let s = stmt.trim().strip_prefix("@import")?.trim();
    if let Some(start) = s.find("url(") {
        let inner = &s[start + 4..];
        let end = inner.find(')')?;
        let u = inner[..end]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        return (!u.is_empty()).then(|| u.to_string());
    }
    // Bare-string form: @import "x.css";
    let s = s.trim_start_matches(['"', '\'']);
    let end = s.find(['"', '\'']).unwrap_or(s.len());
    let u = s[..end].trim();
    (!u.is_empty()).then(|| u.to_string())
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
    /// The text-only image policy (global default + per-image overrides), used
    /// both to skip fetching text-only images and to render them as text.
    image_policy: ImagePolicy,
    /// Fetched external `<link>` stylesheets, keyed by the link's raw `href`
    /// (what the cascade looks up). Re-styled into `styled` as sheets arrive
    /// (ADR-0037).
    sheets: ExternalSheets,
    /// In-flight stylesheet fetches: resolved absolute URL → raw `href`, so a
    /// `Done::Sub` response can be routed to CSS (vs. an image) and stored under
    /// the href the cascade keys on.
    pending_sheets: HashMap<String, String>,
    /// In-flight external `<script src>` fetches (resolved absolute URLs), so a
    /// `Done::Sub` response can be routed to the JS engine and executed against
    /// the page realm rather than decoded as an image.
    pending_scripts: std::collections::HashSet<String>,
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
    /// Generic element hit map from the last frame (window coords): each block
    /// element's box tagged with its `NodeId`, for routing clicks on arbitrary
    /// elements to JS listeners (M12b).
    elements: Vec<ElementBox>,
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
    /// The POST body to replay if the awaiting `insecure_prompt` is confirmed
    /// (so "Load anyway" re-POSTs a form over http instead of GETting it).
    insecure_post: Option<PostBody>,
    /// Hit region of the "Load anyway" button while the prompt is shown.
    insecure_button: Option<Rect>,
    settings_open: bool,
    background: Color,
    last_size: Size,
    /// HiDPI scale factor (physical ÷ logical px). 1.0 unless the shell sets it.
    scale: f32,
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
    /// Whether the MIRC control panel overlay is open (the SYNC button).
    mirc_open: bool,
    /// Top row offset of the MIRC roster list.
    mirc_scroll: usize,
    /// A transient status line shown under the MIRC control bar (e.g. the result
    /// of a bulk action), cleared on the next panel interaction.
    mirc_status: Option<String>,
    /// Remaining script-initiated navigations before further ones are ignored.
    /// A user gesture refills it to [`SCRIPT_NAV_CAP`]; each `location.*`/
    /// `location.href =` reload spends one. Caps a page that reloads on every
    /// load (a bot challenge that never resolves) without blocking the one or two
    /// reloads a real cookie-gated handshake needs.
    script_nav_budget: u32,
}

/// How many chained script-initiated navigations are allowed between user
/// gestures. A cookie-gated reload needs one; a couple covers redirect chains.
/// Beyond this we stop following script reloads to avoid a spin loop.
const SCRIPT_NAV_CAP: u32 = 4;

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
        // Per-window proxy: the foreground browser honors each identity's own
        // proxy too, so switching heads switches egress. Fail-closed like the
        // global proxy.
        let proxies =
            head_proxies(&heads).unwrap_or_else(|e| panic!("invalid per-identity proxy: {e:?}"));
        let mut app = Self::build(
            Box::new(NetLoader::new(
                options.system_roots,
                Some(jar),
                proxy,
                proxies,
            )),
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
            image_policy: ImagePolicy {
                default: ImageDisplayMode::from_env(),
                overrides: Vec::new(),
            },
            sheets: ExternalSheets::new(),
            pending_sheets: HashMap::new(),
            pending_scripts: std::collections::HashSet::new(),
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
            elements: Vec::new(),
            node_to_js: HashMap::new(),
            forms: FormStore::default(),
            focused_field: None,
            pending: None,
            next_id: 1,
            insecure_prompt: None,
            insecure_post: None,
            insecure_button: None,
            settings_open: false,
            background: Color::WHITE,
            last_size: Size::new(800, 600),
            scale: 1.0,
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
            mirc_open: false,
            mirc_scroll: 0,
            mirc_status: None,
            script_nav_budget: SCRIPT_NAV_CAP,
        };
        // The SYNC button shows how many identities/sessions it can drive.
        app.toolbar.sync_count = app.heads.heads().len();
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

    /// The current page's rendered text content (automation/inspection hook, the
    /// live-window parallel of `render --dump-text`).
    pub fn page_text(&self) -> String {
        self.document.root().text_content()
    }

    /// Whether a navigation is in flight (no response committed yet).
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Load `url` as a top-level user navigation (automation/headless hook — the
    /// programmatic parallel of typing into the toolbar and pressing enter).
    pub fn open(&mut self, url: &str) {
        self.navigate(url);
    }

    /// Drive one round of the worker loop (headless automation hook): process any
    /// completed page/subresource/fetch results and run the JS they trigger.
    /// Returns whether a redraw is due. The windowed shell calls the [`FrameApp`]
    /// method of the same name; this is the inherent entry for headless drivers.
    pub fn drive(&mut self) -> bool {
        <Self as FrameApp>::poll(self)
    }

    /// Whether the page has fully settled: no navigation pending and no external
    /// subresource (script/stylesheet/image) still in flight. A headless driver
    /// polls until this holds (plus a few idle rounds for async JS) to know the
    /// page — including any script-driven reload — has finished loading.
    pub fn is_settled(&self) -> bool {
        self.pending.is_none()
            && self.pending_scripts.is_empty()
            && self.pending_sheets.is_empty()
            && !self
                .images
                .values()
                .any(|s| matches!(s, ImageState::Pending))
    }

    /// Click points (centers) of the current page's text fields, from the last
    /// rendered frame — an automation hook (used by the forms example).
    pub fn text_field_centers(&self) -> Vec<(i32, i32)> {
        self.form_fields
            .iter()
            .filter(|f| matches!(f.kind, FieldKind::Text | FieldKind::Textarea))
            .map(|f| {
                (
                    f.rect.x + f.rect.w as i32 / 2,
                    f.rect.y + f.rect.h as i32 / 2,
                )
            })
            .collect()
    }

    /// Begin loading `url` (a GET): built-in pages synchronously; http(s) on the
    /// worker, upgrading `http`→`https` first.
    fn start_load(&mut self, url: &str) {
        self.begin_load(url, None, true);
    }

    /// Begin loading `url` with an optional form `post` body. With `Some`, the
    /// load is a POST (the body is sent instead of a URL query); with `None`, a
    /// normal GET. Shared prelude for both navigation kinds. `user_initiated`
    /// marks a genuine user gesture (toolbar, link, form, history) as opposed to
    /// a script `location.*` reload, and refills the script-navigation budget.
    fn begin_load(&mut self, url: &str, post: Option<PostBody>, user_initiated: bool) {
        if user_initiated {
            self.script_nav_budget = SCRIPT_NAV_CAP;
        }
        self.insecure_prompt = None;
        self.insecure_post = None;
        self.insecure_button = None;
        self.toolbar.blur_url();
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
            self.load_builtin(url); // built-in pages are GET-only; ignore any body
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
        self.dispatch(target, http_fallback, post);
    }

    /// Serve from cache if fresh, else queue a background fetch. A POST (`post`
    /// is `Some`) always hits the network — it is neither read from nor written
    /// to the cache (POST is not idempotent).
    fn dispatch(&mut self, target: String, http_fallback: Option<String>, post: Option<PostBody>) {
        let instance = self.heads.active().instance;
        if post.is_none() {
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
        }
        self.toolbar.url_text = target.clone();
        self.toolbar.loading = true;
        self.set_document(loading_document(&target));
        let id = self.next_id;
        self.next_id += 1;
        self.pending = Some(Pending {
            id,
            http_fallback,
            post: post.clone(),
        });
        let ctx = FetchContext {
            instance,
            kind: FetchKind::Navigation,
        };
        self.loader.request(id, target, post, ctx);
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
        let mut doc = self.run_scripts(doc);
        self.timings.record("scripts", t.elapsed());
        // Rewrite inline `<svg>` subtrees into synthetic replaced elements and
        // decode them into the image store — after scripts (so script-built
        // SVG participates), before styling (so layout sees `<img>`).
        let viewport_w = self.toolbar.content_size(self.last_size).w;
        self.register_inline_svgs(replace_inline_svgs(&mut doc, viewport_w));
        self.page_title = doc.title();
        // New page: drop the previous page's external stylesheets. The first
        // cascade uses inline CSS only; external `<link>` sheets fetch on the
        // worker and re-style as they arrive (ADR-0037).
        self.sheets.clear();
        self.pending_sheets.clear();
        // New page: abandon any external scripts still in flight from the last one.
        self.pending_scripts.clear();
        let t = Instant::now();
        self.styled = self.style_engine.style(&doc);
        self.timings.record("style", t.elapsed());
        self.document = doc;
        // Dispatch any fetches the page scheduled at load to the worker (async).
        self.pump_fetches();
        // An inline script may have set location.* at load — follow it.
        self.pump_navigations();
    }

    /// Re-run the cascade with the external stylesheets fetched so far, splicing
    /// each in at its `<link>`'s position. Called as sheets arrive (ADR-0037).
    fn restyle_with_sheets(&mut self) {
        let t = Instant::now();
        self.styled = self
            .style_engine
            .style_with_sheets(&self.document, &self.sheets);
        self.timings.record("style", t.elapsed());
    }

    /// Run the document's inline scripts against the active head's engine and
    /// return the reconciled document. Script-less pages return untouched (and
    /// keep the engine lazy); on any bridge failure we fall back to the
    /// unscripted DOM so the page still renders.
    fn run_scripts(&mut self, doc: Document) -> Document {
        // Each navigation rebuilds the realm's model, so the previous node↔JS
        // correlation is stale. A truly script-less page (no inline AND no
        // external scripts) has no realm and no dispatch targets: keep the map
        // empty and return the DOM untouched. A page with only external scripts
        // still installs the realm here (running zero inline scripts), so the
        // fetched external bodies have a live document model to execute against.
        self.node_to_js.clear();
        let mut external = Vec::new();
        collect_external_scripts(doc.root(), &mut external);
        if doc.scripts().is_empty() && external.is_empty() {
            return doc;
        }
        let realm = RealmId(self.heads.active().id.0);
        // Seed `document.cookie` from this instance's jar for the current origin
        // (a top-level page: request origin == first party).
        let cookie = self
            .current_url
            .as_ref()
            .and_then(first_party_of)
            .map(|origin| {
                cookie_seed(
                    &self.storage,
                    self.heads.active().instance,
                    &origin,
                    &origin,
                )
            })
            .unwrap_or_default();
        let env = PageEnv {
            url: self.toolbar.url_text.clone(),
            viewport: (self.last_size.w, self.last_size.h),
            user_agent: self.active_ua.clone(),
            cookie,
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
        // Drain timers/microtasks the page scheduled, under the default caps
        // (ADR-0013), so first-paint reflects deferred work and no page can hang.
        let _ = run_event_loop(engine, realm, EventLoopBudget::default());
        let out = match serialize_dom(engine, realm) {
            Ok(rebuilt) => {
                let RebuiltDom { document, id_map } = rebuilt;
                self.node_to_js = invert_id_map(&id_map);
                document
            }
            Err(_) => doc,
        };
        // Persist any cookies the initial scripts set via `document.cookie`.
        self.capture_cookie_writes();
        out
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
        // Set the current URL *before* set_document so the page's scripts (and the
        // fetch()es they schedule at load) resolve/consent-gate against the right
        // first party (M12d).
        self.current_url = parse_url(url).ok();
        self.set_document(parse_html(&String::from_utf8_lossy(body)));
        self.toolbar.url_text = url.to_string();
        self.toolbar.loading = false;
        self.insecure_prompt = None;
        self.insecure_post = None;

        self.request_page_images();
        self.request_page_stylesheets();
        // Fetch external `<script src>` (e.g. a bot-challenge sensor) to run on
        // the worker; each executes against the page realm as it resolves.
        self.request_page_scripts();
        self.update_nav();
        // Page-load total covers fetch → parse → scripts → style (M11);
        // layout+paint is timed per frame in render_frame.
        self.timings.record_page_load();
        self.persist();
    }

    /// The directory downloads are saved to: `<data-dir>/downloads` for a
    /// persistent profile, else the OS `~/Downloads`, else a temp dir.
    fn downloads_dir(&self) -> PathBuf {
        if let Some(dir) = &self.data_dir {
            return dir.join("downloads");
        }
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join("Downloads");
        }
        std::env::temp_dir().join("cerberus-downloads")
    }

    /// Save a downloaded response body to the downloads directory and show a
    /// "download complete" page. The file is written under a unique name (never
    /// overwriting), and the response is not cached or parsed as a page.
    fn commit_download(&mut self, url: &str, filename: &str, body: &[u8]) {
        let dir = self.downloads_dir();
        let outcome = match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                let path = unique_download_path(&dir, filename);
                match std::fs::write(&path, body) {
                    Ok(()) => Ok(path),
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        };
        self.status = 200;
        self.toolbar.loading = false;
        self.current_url = parse_url(url).ok();
        match outcome {
            Ok(path) => {
                self.set_document(download_done_document(filename, body.len(), &path));
            }
            Err(e) => {
                self.set_document(error_document(url, &format!("download failed: {e}")));
            }
        }
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
        let post = pending.post.clone();
        let is_post = post.is_some();
        self.pending = None;
        match result {
            Ok(page) => {
                // Server response time for the navigation (M11).
                let verb = if is_post { "POST" } else { "GET" };
                let label = parse_url(&page.url)
                    .ok()
                    .map(|u| format!("{verb} {}", u.host))
                    .unwrap_or_else(|| verb.to_string());
                self.timings.record(label, page.elapsed);
                // A response the server marks as a download (Content-Disposition:
                // attachment, or a non-renderable content type) is saved to disk
                // instead of being parsed as a page.
                if let Some(filename) = download_target(&page.headers, &page.url) {
                    self.commit_download(&page.url, &filename, &page.body);
                } else {
                    // A POST response is not cacheable (not idempotent).
                    self.commit_response(
                        &page.url,
                        page.status,
                        &page.headers,
                        &page.body,
                        &page.user_agent,
                        !is_post,
                    )
                }
            }
            Err(err) => match http_fallback {
                // The https upgrade failed at the connection/cert layer; http may
                // still serve the page, so offer the plaintext risk prompt. A DNS
                // failure is different — the name didn't resolve, so http can't
                // help either — report the real cause instead of the misleading
                // "doesn't support HTTPS" prompt.
                Some(http_url) if !is_dns_failure(&err) => {
                    self.toolbar.loading = false;
                    self.set_document(insecure_prompt_document(&http_url, &err));
                    self.insecure_prompt = Some(http_url);
                    // Preserve any POST body so "Load anyway" re-POSTs over http.
                    self.insecure_post = post;
                }
                _ => {
                    let msg = if is_dns_failure(&err) {
                        format!("This site's address could not be resolved (DNS). {err}")
                    } else {
                        err
                    };
                    self.show_error(&requested_url, &msg);
                }
            },
        }
        true
    }

    /// Apply a completed sub-resource. A response for a pending stylesheet URL is
    /// routed to the cascade (ADR-0037); everything else is decoded as an image.
    fn handle_subresource(
        &mut self,
        url: String,
        bytes: Result<Vec<u8>, String>,
        elapsed: Duration,
    ) -> bool {
        // Subresources are aggregated into one stable row so an image-heavy
        // page doesn't flood (and reflow) the HUD.
        self.timings.add("subresources", elapsed);
        if self.pending_sheets.contains_key(&url) {
            return self.handle_stylesheet(url, bytes);
        }
        if self.pending_scripts.contains(&url) {
            return self.handle_script(url, bytes);
        }
        let state =
            match bytes.and_then(|b| self.image_codec.decode(&b).map_err(|e| format!("{e:?}"))) {
                Ok(img) => ImageState::Ready(Arc::new(img)),
                Err(_) => ImageState::Failed,
            };
        self.images.insert(url, state);
        true // a newly-decoded image changes layout — redraw
    }

    /// Store a fetched external stylesheet (under the `<link>`'s raw href) and,
    /// once every in-flight sheet has resolved, re-run the cascade so they all
    /// apply in one pass (ADR-0037). A failed fetch simply contributes no CSS.
    fn handle_stylesheet(&mut self, url: String, bytes: Result<Vec<u8>, String>) -> bool {
        let Some(href) = self.pending_sheets.remove(&url) else {
            return false;
        };
        if let Ok(b) = bytes {
            self.sheets
                .insert(href, String::from_utf8_lossy(&b).into_owned());
        }
        // Re-style once the last pending sheet arrives, so the page doesn't
        // re-cascade once per sheet on a multi-stylesheet page.
        if self.pending_sheets.is_empty() {
            self.restyle_with_sheets();
            // External CSS can introduce new background-images; fetch them now.
            self.request_page_images();
            true
        } else {
            false
        }
    }

    /// Drain the realm's JS-`fetch` queue and dispatch each request to the
    /// network worker (async — results arrive in `poll` → `handle_fetch`). A
    /// third-party fetch is consent-gated like an image subresource; a blocked
    /// or unsupported one rejects its Promise. No-op without a scripted realm.
    fn pump_fetches(&mut self) {
        if self.node_to_js.is_empty() {
            return;
        }
        let Some(first_party) = self.current_url.as_ref().and_then(first_party_of) else {
            return;
        };
        let instance = self.heads.active().instance;
        let realm = RealmId(self.heads.active().id.0);
        let reqs = {
            let engine = match self.heads.engine() {
                Ok(engine) => engine,
                Err(_) => return,
            };
            match take_fetches(engine, realm) {
                Ok(reqs) => reqs,
                Err(_) => return,
            }
        };
        let mut rejected_sync = false;
        for mut req in reqs {
            // Resolve relative URLs against the page (like image subresources).
            let abs = resolve_subresource(self.current_url.as_ref(), &req.url);
            if !(abs.starts_with("http://") || abs.starts_with("https://")) {
                self.reject_pending(req.id, "unsupported fetch URL");
                rejected_sync = true;
                continue;
            }
            // Consent gate: a third-party fetch needs an Allow rule.
            if self.gate_subresource(&abs, &first_party) != Decision::Allow {
                self.reject_pending(req.id, "blocked by consent policy");
                rejected_sync = true;
                continue;
            }
            req.url = abs;
            if std::env::var("CERBERUS_TRACE").is_ok() {
                eprintln!(
                    "[trace] JS fetch dispatched: {} {} (body {} bytes)",
                    req.method,
                    req.url,
                    req.body.len()
                );
                eprintln!(
                    "[trace]   body: {}",
                    req.body.chars().take(260).collect::<String>()
                );
            }
            let ctx = FetchContext {
                instance,
                kind: FetchKind::Subresource {
                    first_party: first_party.clone(),
                },
            };
            self.loader.request_fetch(req.id, req, ctx);
        }
        // Drain the `.catch` reactions of any synchronously-rejected fetch and
        // reflect their DOM changes now (worker-resolved fetches drain later in
        // handle_fetch instead).
        if rejected_sync {
            let realm = RealmId(self.heads.active().id.0);
            if let Ok(engine) = self.heads.engine() {
                let _ = run_event_loop(engine, realm, EventLoopBudget::default());
            }
            self.reconcile_realm();
        }
    }

    /// Reject a pending JS-`fetch` Promise by id (best-effort).
    fn reject_pending(&mut self, id: u64, message: &str) {
        let realm = RealmId(self.heads.active().id.0);
        if let Ok(engine) = self.heads.engine() {
            let _ = reject_fetch(engine, realm, id, message);
        }
    }

    /// A worker-delivered JS-`fetch` result: settle the Promise, drain the
    /// resulting microtasks/timers, reconcile the mutated DOM, and dispatch any
    /// newly-queued fetches. Returns true (redraw).
    fn handle_fetch(&mut self, id: u64, result: Result<FetchResponse, String>) -> bool {
        let realm = RealmId(self.heads.active().id.0);
        {
            let engine = match self.heads.engine() {
                Ok(engine) => engine,
                Err(_) => return false,
            };
            match &result {
                Ok(resp) => {
                    let _ = resolve_fetch(engine, realm, id, resp);
                }
                Err(message) => {
                    let _ = reject_fetch(engine, realm, id, message);
                }
            }
            let _ = run_event_loop(engine, realm, EventLoopBudget::default());
        }
        self.reconcile_realm();
        self.pump_fetches();
        // A settled fetch's handler (e.g. a bot-challenge sensor's XHR onload that
        // set the token cookie) may have called location.reload()/assign — follow
        // it now so the cookie-gated re-fetch happens.
        self.pump_navigations();
        true
    }

    /// Persist any `document.cookie =` writes a script made into the active
    /// instance's sealed jar — the same store, consent gate, and per-cookie
    /// disposition a network `Set-Cookie` goes through, so a script-set cookie
    /// (e.g. a bot challenge's token) survives to the next request. First-party
    /// only: the write targets the current page's own origin. A no-op when the
    /// realm is idle (no JS correlate) or nothing was written.
    fn capture_cookie_writes(&mut self) {
        if self.node_to_js.is_empty() {
            return;
        }
        let realm = RealmId(self.heads.active().id.0);
        let writes = {
            let Ok(engine) = self.heads.engine() else {
                return;
            };
            match take_cookie_writes(engine, realm) {
                Ok(w) if !w.is_empty() => w,
                _ => return,
            }
        };
        let Some(url) = self.current_url.clone() else {
            return;
        };
        let Some(first_party) = first_party_of(&url) else {
            return;
        };
        let instance = self.heads.active().instance;
        // Build a jar over the shared Arcs (cheap; all state lives in `storage`).
        let jar = SealedJar {
            storage: self.storage.clone(),
            policy: self.consent.clone(),
            cookies: self.cookie_policy.clone(),
            events: self.pending_consent.clone(),
        };
        for value in writes {
            jar.set_cookie(instance, &url, &first_party, &value);
        }
    }

    /// Perform a navigation the page's scripts requested (`location.assign`/
    /// `replace`/`reload`, `location.href =`, `window.location =`). The LAST
    /// request wins — a script that sets `location` repeatedly ends at its final
    /// target. This is the reload half of a cookie-gated handshake: after a bot
    /// challenge sets its token (captured into the jar) and calls `reload()`, the
    /// re-fetch carries the cookie and returns the real page. Guarded by
    /// `script_nav_budget` so a page that reloads on every load can't spin.
    fn pump_navigations(&mut self) {
        if self.node_to_js.is_empty() {
            return;
        }
        let realm = RealmId(self.heads.active().id.0);
        let navs = {
            let Ok(engine) = self.heads.engine() else {
                return;
            };
            match take_navigations(engine, realm) {
                Ok(n) if !n.is_empty() => n,
                _ => return,
            }
        };
        // Later synchronous assignments supersede earlier ones (a real browser
        // only performs the last one); drop the rest.
        let Some(nav) = navs.into_iter().next_back() else {
            return;
        };
        if std::env::var("CERBERUS_TRACE").is_ok() {
            eprintln!(
                "[trace] script navigation requested: {} (replace={})",
                nav.url, nav.replace
            );
        }
        // Resolve against the current document URL — the target may be relative
        // (`location.href = '/home'`). Fall back to the raw string if there is no
        // base or it doesn't parse (begin_load re-parses and reports errors).
        let target = self
            .current_url
            .as_ref()
            .and_then(|base| join_url(base, &nav.url).ok())
            .map(|u| u.to_string())
            .unwrap_or(nav.url);
        // Only follow navigable top-level schemes (http/https/cerberus). A
        // javascript:/data:/blob:/mailto: target is ignored — never fetched or
        // executed — matching a browser that doesn't treat those as a document
        // navigation, and denying a hostile page a way to make the host fetch an
        // arbitrary non-http scheme. Ignored before spending budget.
        if !is_navigable_scheme(&target) {
            return;
        }
        if self.script_nav_budget == 0 {
            return;
        }
        self.script_nav_budget -= 1;
        // Record history like a browser: a non-replacing navigation (assign,
        // location.href =) pushes a new entry; replace()/reload() overwrite the
        // current one (so Back doesn't return to a challenge interstitial).
        if nav.replace {
            if let Some(slot) = self.history.get_mut(self.index) {
                *slot = target.clone();
            }
        } else {
            self.history.truncate(self.index + 1);
            self.history.push(target.clone());
            self.index = self.history.len() - 1;
        }
        self.begin_load(&target, None, false);
    }

    /// Re-read the live realm into the rendered document + restyle, after async
    /// work (a settled fetch) may have mutated it. Reuses the dispatch reconcile.
    fn reconcile_realm(&mut self) {
        self.capture_cookie_writes();
        let realm = RealmId(self.heads.active().id.0);
        let dom = {
            let engine = match self.heads.engine() {
                Ok(engine) => engine,
                Err(_) => return,
            };
            match serialize_dom(engine, realm) {
                Ok(dom) => dom,
                Err(_) => return,
            }
        };
        self.reconcile_dispatched(dom);
    }

    /// Scan the current document for `<img>` sources and queue a background
    /// fetch for each new http(s) image. Lazy-loading hints are ignored — every
    /// image is fetched immediately (speed-first; see the layout `img` path).
    fn request_page_images(&mut self) {
        let first_party = self.current_url.as_ref().and_then(first_party_of);
        let instance = self.heads.active().instance;
        // Collect <img> srcs and CSS background-image srcs separately: text-only
        // is an <img> feature (it has alt/caption text to show), so a background
        // matching the policy must still fetch and paint — it has no text
        // substitute and would otherwise vanish silently.
        let mut img_srcs = Vec::new();
        // The same viewport layout uses (ADR-0046), so srcset and <picture>
        // selection at fetch time and at draw time agree — including the
        // consent-banner strip `render_frame` subtracts from the content height,
        // so a <picture> `media` keyed on height/orientation resolves the same
        // candidate at fetch and draw while a banner is up.
        let banner_h = if self.consent_prompts.is_empty() {
            0
        } else {
            BANNER_HEIGHT
        };
        let mut viewport = self.toolbar.content_size(self.last_size);
        viewport.h = viewport.h.saturating_sub(banner_h);
        let viewport_w = viewport.w;
        collect_image_urls(self.document.root(), &mut img_srcs, viewport_w, viewport.h);
        let img_urls: std::collections::HashSet<String> = img_srcs
            .iter()
            .map(|s| resolve_subresource(self.current_url.as_ref(), s))
            .collect();
        let mut srcs = img_srcs;
        collect_bg_image_urls(&self.styled.root, &mut srcs);
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
            // Text-only images render as their alt/caption; never fetch them.
            // Scoped to <img> URLs so a policy-matched CSS background still paints.
            if img_urls.contains(&abs) && self.image_policy.text_only(&abs) {
                self.images.insert(abs, ImageState::TextOnly);
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

    /// Scan the current document for `<link rel="stylesheet">` and queue a
    /// background fetch for each new http(s) sheet, consent-gated like an image.
    /// Responses route to the cascade in `handle_stylesheet` (ADR-0037).
    fn request_page_stylesheets(&mut self) {
        let first_party = self.current_url.as_ref().and_then(first_party_of);
        let instance = self.heads.active().instance;
        let mut hrefs = Vec::new();
        collect_stylesheet_links(self.document.root(), &mut hrefs);
        for href in hrefs {
            let abs = resolve_subresource(self.current_url.as_ref(), &href);
            if !(abs.starts_with("http://") || abs.starts_with("https://")) {
                continue;
            }
            // One fetch per distinct URL per page (already fetched or in flight).
            if self.sheets.contains_key(&href) || self.pending_sheets.contains_key(&abs) {
                continue;
            }
            let Some(first_party) = first_party.clone() else {
                continue;
            };
            // Consent gate: a third-party stylesheet needs an Allow rule.
            if self.gate_subresource(&abs, &first_party) != Decision::Allow {
                continue;
            }
            self.pending_sheets.insert(abs.clone(), href);
            self.loader.request_subresource(
                abs,
                FetchContext {
                    instance,
                    kind: FetchKind::Subresource { first_party },
                },
            );
        }
    }

    /// Scan the current document for `<script src>` and queue a background fetch
    /// for each new http(s) script, consent-gated like any subresource. Responses
    /// route to the JS engine in `handle_script`, which executes the fetched body
    /// against the page realm (so an external sensor script actually runs). Async
    /// by nature: a script runs whenever its fetch resolves.
    fn request_page_scripts(&mut self) {
        // No realm to run against (a truly script-less page never installed one).
        if self.node_to_js.is_empty() {
            return;
        }
        let first_party = self.current_url.as_ref().and_then(first_party_of);
        let instance = self.heads.active().instance;
        let mut srcs = Vec::new();
        collect_external_scripts(self.document.root(), &mut srcs);
        for src in srcs {
            let abs = resolve_subresource(self.current_url.as_ref(), &src);
            if !(abs.starts_with("http://") || abs.starts_with("https://")) {
                continue;
            }
            // One fetch per distinct URL per page (already fetched or in flight).
            if self.pending_scripts.contains(&abs) {
                continue;
            }
            let Some(first_party) = first_party.clone() else {
                continue;
            };
            // Consent gate: a third-party script needs an Allow rule (a first-party
            // script — same site as the page — is allowed).
            if self.gate_subresource(&abs, &first_party) != Decision::Allow {
                continue;
            }
            if std::env::var("CERBERUS_TRACE").is_ok() {
                eprintln!("[trace] queuing external script fetch: {abs}");
            }
            self.pending_scripts.insert(abs.clone());
            self.loader.request_subresource(
                abs,
                FetchContext {
                    instance,
                    kind: FetchKind::Subresource { first_party },
                },
            );
        }
    }

    /// Execute a fetched external script body against the page realm, then
    /// reconcile the DOM and dispatch any fetch/navigation it triggered (an
    /// external sensor typically XHRs a payload and later reloads). A failed fetch
    /// or bridge error simply runs nothing — the page still stands.
    fn handle_script(&mut self, url: String, bytes: Result<Vec<u8>, String>) -> bool {
        let trace = std::env::var("CERBERUS_TRACE").is_ok();
        if !self.pending_scripts.remove(&url) {
            return false;
        }
        let Ok(bytes) = bytes else {
            if trace {
                eprintln!("[trace] external script FETCH FAILED: {url}");
            }
            return false;
        };
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if trace {
            eprintln!(
                "[trace] external script fetched: {url} ({} bytes)",
                body.len()
            );
        }
        let realm = RealmId(self.heads.active().id.0);
        {
            let Ok(engine) = self.heads.engine() else {
                return false;
            };
            // Run against the already-installed realm; swallow a script throw the
            // same way inline execution does (the page must survive a bad script).
            match cerberus_js_dom::run_scripts(engine, realm, &[body]) {
                Ok(_) if trace => eprintln!("[trace]   script ran OK: {url}"),
                Err(e) if trace => eprintln!("[trace]   script ERROR: {url}: {e:?}"),
                _ => {}
            }
            let _ = run_event_loop(engine, realm, EventLoopBudget::default());
        }
        // The script may have set document.cookie, mutated the DOM, or queued a
        // fetch/navigation — persist, re-read, and pump, exactly like a settled
        // fetch handler.
        self.capture_cookie_writes();
        self.reconcile_realm();
        self.pump_fetches();
        self.pump_navigations();
        true
    }

    // ---- Cookie inspector (M10) ----

    /// Snapshot the active head's cookies as inspector rows (sorted, with
    /// values masked unless revealed).
    fn cookie_rows(&self) -> Vec<(String, String, CookieRow)> {
        let instance = self.heads.active().instance;
        let mut views = self.storage.locked().instance(instance).cookie_views();
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
            let text = self.cookie_policy.locked().serialize();
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
                let next = self.cookie_policy.locked().global().cycle();
                self.cookie_policy.locked().set_global(next);
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
                        .locked()
                        .instance(instance)
                        .delete_cookie(site, name);
                    self.cookie_policy
                        .locked()
                        .set_override(site, name, CookieDisposition::Block);
                    self.save_cookie_policy();
                }
            }
            CookieAction::Cycle(i) => {
                if let Some((site, name, _)) = rows.get(i).cloned() {
                    let current = self.cookie_policy.locked().resolve(&site, &name);
                    let next = current.cycle();
                    self.cookie_policy.locked().set_override(&site, &name, next);
                    self.storage
                        .locked()
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
        self.cookie_policy.locked().set_override(&site, &name, d);
        self.storage
            .locked()
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
            .locked()
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
                self.consent.locked().add_rule(
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
            let mut env = self.storage.locked();
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
        // A stylesheet from the newly-allowed site can now be fetched too.
        self.request_page_stylesheets();
    }

    /// Persist the standing consent rules into the profile (if any).
    fn save_consent_rules(&self) {
        let Some(dir) = &self.data_dir else { return };
        let text = self.consent.locked().serialize_rules();
        if let Err(e) = atomic_write(&dir.join(CONSENT_RULES_FILE), text.as_bytes()) {
            eprintln!("cerberus: cannot save consent rules: {e}");
        }
    }

    /// Confirm the risk prompt: load the original `http` URL in plaintext.
    fn confirm_insecure(&mut self) {
        if let Some(http_url) = self.insecure_prompt.take() {
            let post = self.insecure_post.take();
            self.dispatch(http_url, None, post);
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

    /// The `NodeId` of the deepest (smallest-area) element hit box under
    /// `(x, y)` — the most specific element the user clicked. Bubbling then
    /// carries the dispatched event up to its ancestors (M12b).
    fn element_at(&self, x: i32, y: i32) -> Option<NodeId> {
        self.elements
            .iter()
            .filter(|e| point_in_rect(e.rect, x, y))
            .min_by_key(|e| e.rect.w as u64 * e.rect.h as u64)
            .map(|e| e.node)
    }

    /// Handle a click that landed on form control `field`. Returns true (the
    /// click is always consumed once it hits a control).
    fn click_field(&mut self, field: &FormFieldBox) -> bool {
        // M12b: dispatch a real `click` to any JS listener first; the default
        // action below (focus, toggle, cycle, submit) runs only if no handler
        // called preventDefault. Script-less pages have no JS correlate, so this
        // is a no-op and the default action proceeds exactly as before.
        if let Some(node) = self.control_node_id(field.id) {
            if self.dispatch_dom(node, "click", "{}") == Some(true) {
                return true;
            }
        }
        match field.kind {
            FieldKind::Text | FieldKind::Textarea => {
                self.focused_field = Some(field.id);
                self.toolbar.blur_url();
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
        collect_controls(&self.styled.root, &self.document)
            .iter()
            .find(|c| c.id == field_id)
            .map(|c| c.el.id())
    }

    /// Dispatch DOM `event_type` at `node` (a `NodeId` in `self.document`) into
    /// the live JS realm and reconcile any mutations the handler made. Returns
    /// `None` when nothing was dispatched (no realm, or the node has no JS
    /// correlate — so non-scripted pages behave exactly as before), else
    /// `Some(default_prevented)` (M12b / ADR-0012).
    fn dispatch_dom(&mut self, node: NodeId, event_type: &str, init_json: &str) -> Option<bool> {
        let &js_id = self.node_to_js.get(&node)?;
        let realm = RealmId(self.heads.active().id.0);
        let t = Instant::now();
        let result = {
            let engine = self.heads.engine().ok()?;
            dispatch_event(engine, realm, js_id, event_type, init_json)
        };
        let dispatched = result.ok()?;
        // HUD handler row (M11): "click handler", "input handler", …
        self.timings
            .record(format!("{event_type} handler"), t.elapsed());
        let prevented = dispatched.default_prevented;
        // A handler may have set document.cookie; persist it before reconciling.
        self.capture_cookie_writes();
        self.reconcile_dispatched(dispatched.dom);
        // A handler may have called fetch(); dispatch it to the worker (async).
        self.pump_fetches();
        // A handler may have navigated (e.g. a JS link or button that sets
        // location) — follow the last requested location change.
        self.pump_navigations();
        Some(prevented)
    }

    /// Adopt the DOM read back after an event dispatch: refresh the node↔JS map,
    /// restyle, and swap in the new document (the next frame relays out and
    /// repaints). Mirrors the styling half of [`BrowserApp::set_document`].
    fn reconcile_dispatched(&mut self, dom: RebuiltDom) {
        let RebuiltDom {
            mut document,
            id_map,
        } = dom;
        self.node_to_js = invert_id_map(&id_map);
        // The realm serializes `<svg>` elements back; re-rewrite them before
        // styling. Content-hash keys make this idempotent — an icon already in
        // the store is neither re-serialized into a new key nor re-decoded.
        let viewport_w = self.toolbar.content_size(self.last_size).w;
        self.register_inline_svgs(replace_inline_svgs(&mut document, viewport_w));
        self.page_title = document.title();
        let t = Instant::now();
        self.styled = self.style_engine.style(&document);
        self.timings.record("style", t.elapsed());
        self.document = document;
    }

    /// Decode serialized inline-SVG payloads (from [`replace_inline_svgs`])
    /// into the per-page image store under their synthetic keys. Already-
    /// registered keys are skipped (reconciles re-run the rewrite). A declined
    /// payload registers a transparent stand-in so the box is still reserved.
    fn register_inline_svgs(&mut self, svgs: Vec<(String, Vec<u8>)>) {
        for (key, bytes) in svgs {
            if self.images.contains_key(&key) {
                continue;
            }
            let state = match self.image_codec.decode(&bytes) {
                Ok(img) => ImageState::Ready(Arc::new(img)),
                Err(_) => ImageState::Ready(Arc::new(transparent_stand_in())),
            };
            self.images.insert(key, state);
        }
    }

    /// Fire a DOM `input` event at the focused control after a keystroke: push
    /// the live value into the model (so a handler reads `e.target.value`),
    /// dispatch, then read any handler-made value change back into the form
    /// store. A no-op on script-less pages (no realm / JS correlate) — M12b.
    fn fire_input(&mut self, field_id: u32) {
        let Some(node) = self.control_node_id(field_id) else {
            return;
        };
        let Some(&js_id) = self.node_to_js.get(&node) else {
            return;
        };
        let value = self
            .forms
            .values
            .get(&field_id)
            .cloned()
            .unwrap_or_default();
        {
            let realm = RealmId(self.heads.active().id.0);
            if let Ok(engine) = self.heads.engine() {
                let _ = set_node_value(engine, realm, js_id, &value);
            }
        }
        self.dispatch_dom(node, "input", "{}");
        // A handler may have rewritten the value (input masking); reflect it.
        if let Some(v) = control_value(&self.styled.root, &self.document, field_id) {
            self.forms.values.insert(field_id, v);
        }
    }

    /// Check radio `id` and clear every other radio sharing its `name` in the
    /// same enclosing form (mutually-exclusive radio-group behaviour).
    fn check_radio(&mut self, id: u32) {
        let controls = collect_controls(&self.styled.root, &self.document);
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
        let controls = collect_controls(&self.styled.root, &self.document);
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
        let controls = collect_controls(&self.styled.root, &self.document);
        let Some(this) = controls.iter().find(|c| c.id == id) else {
            return;
        };
        // The enclosing <form> (if any) supplies the action/method/enctype; its
        // absence means the whole document is treated as one big form.
        let form_el = this.form;
        let action = form_el.and_then(|f| f.attr("action")).unwrap_or("");
        let is_post = form_el
            .and_then(|f| f.attr("method"))
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("post"));
        // multipart/form-data only applies to POST (HTML ignores enctype for GET).
        let multipart = is_post
            && form_el
                .and_then(|f| f.attr("enctype"))
                .is_some_and(|e| e.trim().eq_ignore_ascii_case("multipart/form-data"));

        if multipart {
            // POST a multipart body — the only encoding that carries file uploads.
            let target = self.resolve_action_base(action);
            let (content_type, body) = build_multipart(&controls, form_el, &self.forms);
            self.begin_load(&target, Some(PostBody { content_type, body }), true);
            return;
        }
        // Otherwise the controls serialize as application/x-www-form-urlencoded —
        // the URL query for GET, or the request body for POST.
        let encoded = build_query(&controls, form_el, &self.forms);
        if is_post {
            let target = self.resolve_action_base(action);
            self.begin_load(
                &target,
                Some(PostBody {
                    content_type: "application/x-www-form-urlencoded".to_string(),
                    body: encoded.into_bytes(),
                }),
                true,
            );
        } else {
            let target = self.resolve_action(action, &encoded);
            self.navigate(&target);
        }
    }

    /// Resolve a form `action` against the current URL, **without** touching its
    /// query (POST carries the data in the body, so the action's own query — if
    /// any — is preserved).
    fn resolve_action_base(&self, action: &str) -> String {
        match &self.current_url {
            Some(base) if !action.is_empty() => join_url(base, action)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| action.to_string()),
            Some(base) => base.to_string(),
            None => action.to_string(),
        }
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
    fn paint_caret(&self, page: &mut Framebuffer, origin: Point, scale: f32) {
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
        self.text.rasterize(&list.scaled(scale), page);
    }

    /// Attempt a vault unlock with the passphrase typed into the settings
    /// overlay. The input is wiped either way: `vault_input.zeroize()` scrubs
    /// the backing buffer (not just its length, unlike `String::clear()`), and
    /// the derived key (and the `Secret`'s own copy of the passphrase) zeroize
    /// on drop.
    fn try_unlock_vault(&mut self) {
        if self.vault_input.is_empty() {
            return;
        }
        let pass = Secret::from_passphrase(&self.vault_input);
        self.vault_input.zeroize();
        let result = self.storage.locked().unlock_vault(&pass);
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
        let mut env = self.storage.locked();
        if env.needs_save() {
            if let Err(e) = env.save(&dir) {
                eprintln!("cerberus: cannot persist profile: {e}");
            }
        }
    }

    fn navigate(&mut self, input: &str) {
        let url = normalize_url(input);
        // Same-document navigation — only the #fragment differs from the page
        // we're on (an in-page anchor like `#maincontent`). Record history and
        // update the address bar, but DON'T refetch the page. (Scrolling to the
        // anchor is a future enhancement; for now the page simply stays put.)
        let same_document = self.is_same_document(&url);
        if !self.history.is_empty() {
            self.history.truncate(self.index + 1);
        }
        self.history.push(url.clone());
        self.index = self.history.len() - 1;
        if same_document {
            self.current_url = parse_url(&url).ok();
            self.toolbar.url_text = url.clone();
            self.toolbar.blur_url();
            self.update_nav();
        } else {
            self.start_load(&url);
        }
    }

    /// Whether `url` targets the current document, differing only by its
    /// `#fragment` (an in-page anchor) — which needs no network refetch.
    fn is_same_document(&self, url: &str) -> bool {
        let (Some(current), Ok(next)) = (self.current_url.as_ref(), parse_url(url)) else {
            return false;
        };
        let norm = |p: &str| if p.is_empty() { "/" } else { p }.to_string();
        next.fragment.is_some()
            && next.scheme == current.scheme
            && next.host == current.host
            && next.port == current.port
            && next.opaque == current.opaque
            && norm(&next.path) == norm(&current.path)
            && next.query == current.query
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

    /// The current page's site (host), for the MIRC panel subtitle/roster.
    /// Empty for built-in pages or before the first navigation.
    fn current_site(&self) -> String {
        self.current_url
            .as_ref()
            .map(|u| u.host.clone())
            .filter(|h| !h.is_empty())
            .unwrap_or_default()
    }

    /// Build the MIRC roster from the identities ("heads"). Phase 2a is a
    /// read-only prototype: the active head is shown live and the rest dormant;
    /// `account` is the identity's stored login (when the vault is unlocked) or a
    /// sealed-session tag, and `logged_in` reflects whether that sealed session
    /// actually holds cookies for the current site.
    fn mirc_rows(&self) -> Vec<MircRow> {
        let site = self.current_site();
        let active = self.heads.active_index();
        let heads: Vec<(usize, InstanceId, String)> = self
            .heads
            .heads()
            .iter()
            .enumerate()
            .map(|(i, h)| (i, h.instance, h.label.clone()))
            .collect();
        let mut env = self.storage.locked();
        heads
            .into_iter()
            .map(|(i, instance, label)| {
                let logged_in = !site.is_empty()
                    && env
                        .instance(instance)
                        .cookie_views()
                        .iter()
                        .any(|v| v.fp_site == site);
                let username = env
                    .load_blob(instance, AUTOFILL_PROFILE_KEY)
                    .ok()
                    .flatten()
                    .and_then(|b| cerberus_autofill::Profile::from_bytes(&b))
                    .map(|p| p.login.username.clone())
                    .filter(|u| !u.is_empty());
                let account = username.unwrap_or_else(|| {
                    let b = instance.0.as_bytes();
                    format!("sealed session {:02x}{:02x}", b[14], b[15])
                });
                let state = if i == active {
                    MircState::Live
                } else {
                    MircState::Dormant
                };
                MircRow {
                    label,
                    account,
                    state,
                    logged_in,
                }
            })
            .collect()
    }

    /// Apply a MIRC panel click. Phase 2a wires the panel chrome — close,
    /// broadcast on/off, scroll, and open-a-session (which surfaces that identity
    /// in this window via the existing head switch). The bulk verbs
    /// (navigate-all / login-all) are acknowledged with a status note until the
    /// single-window app drives a live [`MirrorGroup`].
    fn apply_mirc_action(&mut self, action: MircAction) {
        match action {
            MircAction::Close => {
                self.mirc_open = false;
                self.mirc_status = None;
            }
            MircAction::ToggleBroadcast => {
                self.toolbar.broadcasting = !self.toolbar.broadcasting;
                self.mirc_status = Some(if self.toolbar.broadcasting {
                    "broadcast on — master actions will drive every session".to_string()
                } else {
                    "broadcast off — only this session is driven".to_string()
                });
            }
            MircAction::NavigateAll => {
                self.mirc_status =
                    Some("navigate all: lands with the live mirror group (next phase)".to_string());
            }
            MircAction::LoginAll => {
                self.mirc_status =
                    Some("login all: lands with the live mirror group (next phase)".to_string());
            }
            MircAction::Open(idx) => {
                // The lazy "select → render" gesture: surface that identity in
                // this window. Today that is the head switch; with a live group
                // it becomes a focus of the chosen instance.
                if idx < self.heads.heads().len() && idx != self.heads.active_index() {
                    let _ = self.heads.switch_to(idx);
                    self.toolbar.head_label = self.heads.active().label.clone();
                    let _ = self.heads.engine();
                    if let Some(dir) = self.data_dir.clone() {
                        let _ = save_heads(&dir, self.heads.heads(), self.heads.active_index());
                    }
                }
                self.mirc_open = false;
                self.mirc_status = None;
            }
            MircAction::ScrollUp => {
                self.mirc_scroll = self.mirc_scroll.saturating_sub(1);
            }
            MircAction::ScrollDown => {
                let len = self.heads.heads().len();
                let visible = MircPanel::visible_rows(self.last_size);
                let max = len.saturating_sub(visible);
                if self.mirc_scroll < max {
                    self.mirc_scroll += 1;
                }
            }
            MircAction::None => {}
        }
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
                self.toolbar.focus_url();
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
                if !self.settings_open {
                    // Closing the overlay must wipe any typed-but-unsubmitted
                    // passphrase, not just drop the reference to it (issue #30).
                    self.vault_input.zeroize();
                }
                true
            }
            ToolbarAction::OpenSync => {
                // Open the MIRC control panel (Phase 2a: a rendered roster of the
                // identities/sessions with status; broadcast/open are wired, the
                // bulk verbs land next on the MirrorGroup seam).
                self.mirc_open = true;
                self.mirc_scroll = 0;
                self.mirc_status = None;
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

/// Render a `Color` the way `getComputedStyle` reports it.
fn css_color(c: Color) -> String {
    if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else {
        format!("rgba({}, {}, {}, {})", c.r, c.g, c.b, c.a as f32 / 255.0)
    }
}

/// Serialize the computed style we track to the CSS strings `getComputedStyle`
/// exposes (ADR-0021).
fn computed_css(s: &cerberus_style::ComputedStyle) -> Vec<(String, String)> {
    let display = match s.display {
        cerberus_style::Display::Block => "block",
        cerberus_style::Display::Inline => "inline",
        cerberus_style::Display::InlineBlock => "inline-block",
        cerberus_style::Display::ListItem => "list-item",
        cerberus_style::Display::Flex => "flex",
        cerberus_style::Display::Grid => "grid",
        cerberus_style::Display::None => "none",
    };
    let text_align = match s.text_align {
        cerberus_style::TextAlign::Left => "left",
        cerberus_style::TextAlign::Center => "center",
        cerberus_style::TextAlign::WebkitCenter => "-webkit-center",
        cerberus_style::TextAlign::Right => "right",
    };
    let visibility = match s.visibility {
        cerberus_style::Visibility::Visible => "visible",
        cerberus_style::Visibility::Hidden => "hidden",
    };
    vec![
        ("color".to_string(), css_color(s.color)),
        (
            "background-color".to_string(),
            s.background
                .map_or_else(|| "rgba(0, 0, 0, 0)".to_string(), css_color),
        ),
        ("font-size".to_string(), format!("{}px", s.font_size)),
        (
            "font-weight".to_string(),
            (if s.font.bold { "700" } else { "400" }).to_string(),
        ),
        (
            "font-style".to_string(),
            (if s.font.italic { "italic" } else { "normal" }).to_string(),
        ),
        ("text-align".to_string(), text_align.to_string()),
        ("display".to_string(), display.to_string()),
        ("visibility".to_string(), visibility.to_string()),
        ("opacity".to_string(), format!("{}", s.opacity)),
        ("margin-top".to_string(), fmt_len(s.margin_top)),
        ("margin-bottom".to_string(), fmt_len(s.margin_bottom)),
        ("margin-left".to_string(), fmt_len(s.margin_left)),
    ]
}

/// Serialize a margin `Len` as a CSS string for `getComputedStyle` reporting.
fn fmt_len(len: cerberus_style::Len) -> String {
    use cerberus_style::Len;
    match len {
        Len::Auto => "auto".to_string(),
        Len::Px(p) => format!("{p}px"),
        Len::Pct(f) => format!("{f}%"),
        Len::Vw(f) => format!("{f}vw"),
        Len::Vh(f) => format!("{f}vh"),
        Len::Vmin(f) => format!("{f}vmin"),
        Len::Vmax(f) => format!("{f}vmax"),
    }
}

/// Collect `(js-id, computed-css)` for every styled element that has a live
/// realm node, so `getComputedStyle` reflects the cascade (ADR-0021).
fn collect_computed(
    styled: &cerberus_style::StyledDom,
    node_to_js: &HashMap<NodeId, u64>,
) -> Vec<(u64, Vec<(String, String)>)> {
    fn rec(
        n: &cerberus_style::StyledNode,
        map: &HashMap<NodeId, u64>,
        out: &mut Vec<(u64, Vec<(String, String)>)>,
    ) {
        if let Some(&js) = map.get(&n.node_id) {
            out.push((js, computed_css(&n.style)));
        }
        for c in &n.children {
            if let cerberus_style::StyledChild::Element(e) = c {
                rec(e, map, out);
            }
        }
    }
    let mut out = Vec::new();
    rec(&styled.root, node_to_js, &mut out);
    out
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

    fn set_scale_factor(&mut self, scale: f32) {
        self.scale = scale.max(1.0);
    }

    fn poll(&mut self) -> bool {
        let mut redraw = false;
        // Worker-side consent events (cookie capture) surface in the banner.
        let drained: Vec<ConsentEvent> = std::mem::take(self.pending_consent.locked().as_mut());
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
                Done::Fetch { id, result } => self.handle_fetch(id, result),
            };
        }
        redraw
    }

    fn render_frame(&mut self, size: Size) -> Framebuffer {
        // `size` is the physical surface. Lay out and hit-test in *logical*
        // pixels (physical / scale) and scale the paint up, so a HiDPI display
        // renders crisp (re-outlined glyphs) rather than a bitmap upscale. At
        // scale 1.0 (the default, and all tests) this is the identity.
        let scale = self.scale;
        let logical = Size::new(
            ((size.w as f32 / scale).round() as u32).max(1),
            ((size.h as f32 / scale).round() as u32).max(1),
        );
        self.last_size = logical;
        let si = |v: i32| (v as f32 * scale).round() as i32;
        let su = |v: u32| ((v as f32 * scale).round() as u32).max(1);
        let banner_h = if self.consent_prompts.is_empty() {
            0
        } else {
            BANNER_HEIGHT
        };
        let mut content = self.toolbar.content_size(logical);
        content.h = content.h.saturating_sub(banner_h);
        let mut origin = self.toolbar.content_origin();
        origin.y += banner_h as i32;

        // The canvas background is the root/body background propagated to the
        // viewport (CSS), so a page whose `<body>` sets a color fills the whole
        // content area rather than leaving white below its short box.
        let canvas_bg = canvas_background(&self.styled, self.background);

        // Time layout+paint (M11). The image provider's borrow of `self` is
        // scoped to this block so the timing record (a `&mut self` op) is free.
        let t = Instant::now();
        let (laid, mut page) = {
            let provider = StoreImages {
                base: self.current_url.as_ref(),
                images: &self.images,
                policy: &self.image_policy,
            };
            let mut layout = BlockLayout::default();
            let laid = layout.layout(&self.styled, content, &self.text, &provider, &self.forms);
            let mut page = Framebuffer::new(Size::new(su(content.w), su(content.h)));
            page.clear(canvas_bg);
            self.text.rasterize(&laid.display.scaled(scale), &mut page);
            (laid, page)
        };
        self.timings.record("layout+paint", t.elapsed());

        // Capture element geometry in content (viewport) coordinates for
        // getBoundingClientRect, before the boxes are offset into window space
        // for click hit-testing below. Scripted pages only (ADR-0021).
        let geometry: Vec<(u64, Rect)> = if self.node_to_js.is_empty() {
            Vec::new()
        } else {
            laid.elements
                .iter()
                .filter_map(|e| self.node_to_js.get(&e.node).map(|&js| (js, e.rect)))
                .collect()
        };

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

        // Generic element hit map in window coordinates (M12b dispatch targets).
        self.elements = laid
            .elements
            .into_iter()
            .map(|mut e| {
                e.rect.x += origin.x;
                e.rect.y += origin.y;
                e
            })
            .collect();

        // Make getBoundingClientRect reflect this layout in the live realm
        // (ADR-0021); no-op for script-less pages.
        if !geometry.is_empty() {
            let realm = RealmId(self.heads.active().id.0);
            let styles = collect_computed(&self.styled, &self.node_to_js);
            if let Ok(engine) = self.heads.engine() {
                let _ = cerberus_js_dom::set_geometry(engine, realm, &geometry);
                let _ = cerberus_js_dom::set_computed_styles(engine, realm, &styles);
            }
        }

        // Draw a caret at the end of the focused text field's value. The field's
        // own value is already painted by layout into `page`; we just add the bar.
        self.paint_caret(&mut page, origin, scale);

        let mut fb = Framebuffer::new(size);
        fb.clear(canvas_bg);
        fb.blit(Point::new(si(origin.x), si(origin.y)), &page);
        self.text.rasterize(
            &self.toolbar.paint(logical, &self.text).scaled(scale),
            &mut fb,
        );
        if let Some(event) = self.consent_prompts.first() {
            let banner = ConsentBanner::new(event.request.site(), self.consent_prompts.len() - 1);
            self.text
                .rasterize(&banner.paint(logical, &self.text).scaled(scale), &mut fb);
        }
        if self.insecure_prompt.is_some() {
            self.insecure_button = Some(paint_insecure_button(&mut fb, &self.text, scale));
        }
        if self.settings_open {
            let vault_locked = self.storage.locked().vault_locked();
            paint_settings_overlay(
                &mut fb,
                logical,
                &self.text,
                &self.text,
                vault_locked,
                self.vault_input.chars().count(),
                self.vault_msg.as_deref(),
                self.hud_on,
                scale,
            );
        }
        if self.cookie_manager_open {
            let global = self.cookie_policy.locked().global().label();
            let rows: Vec<CookieRow> = self.cookie_rows().into_iter().map(|(_, _, r)| r).collect();
            self.text.rasterize(
                &CookieManager::paint(logical, &self.text, &global, &rows, self.cookie_scroll)
                    .scaled(scale),
                &mut fb,
            );
            if let Some((_, _, buf)) = &self.cookie_ttl_edit {
                let p = CookieManager::panel_rect(logical);
                let mut list = DisplayList::new();
                list.push(DisplayItem::Glyphs {
                    origin: Point::new(p.x + 12, p.y + p.h as i32 - 14),
                    glyphs: self
                        .text
                        .shape(&format!("Timed seconds: {buf}_  (Enter)"), 13),
                    color: Color::rgb(0x20, 0x40, 0x70),
                    style: FontStyle::REGULAR,
                });
                self.text.rasterize(&list.scaled(scale), &mut fb);
            }
        }
        if self.mirc_open {
            let rows = self.mirc_rows();
            let site = self.current_site();
            self.text.rasterize(
                &MircPanel::paint(
                    logical,
                    &self.text,
                    self.toolbar.broadcasting,
                    &site,
                    &rows,
                    self.mirc_scroll,
                )
                .scaled(scale),
                &mut fb,
            );
            // A transient status note under the control bar (bulk-verb feedback).
            if let Some(msg) = &self.mirc_status {
                let p = MircPanel::panel_rect(logical);
                let mut list = DisplayList::new();
                list.push(DisplayItem::Glyphs {
                    origin: Point::new(p.x + 16, p.y + 122),
                    glyphs: self.text.shape(msg, 12),
                    color: Color::rgb(0x20, 0x40, 0x70),
                    style: FontStyle::REGULAR,
                });
                self.text.rasterize(&list.scaled(scale), &mut fb);
            }
        }
        // Performance HUD on top of everything, when enabled (M11).
        if self.hud_on {
            let rows = self.timings.display_rows();
            self.text.rasterize(
                &PerfHud::paint(logical, &self.text, &rows).scaled(scale),
                &mut fb,
            );
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
        if self.mirc_open {
            // The MIRC panel owns all clicks while open; a click outside it (but
            // not on a control) closes it.
            if point_in_rect(MircPanel::panel_rect(self.last_size), x, y) {
                let len = self.heads.heads().len();
                let action = MircPanel::hit_test(self.last_size, len, self.mirc_scroll, x, y);
                self.apply_mirc_action(action);
            } else {
                self.mirc_open = false;
                self.mirc_status = None;
            }
            return true;
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
                // Leaving the overlay wipes any typed-but-unsubmitted passphrase
                // (issue #30) — `zeroize()`, not `clear()`, actually scrubs it.
                self.vault_input.zeroize();
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
                self.vault_input.zeroize();
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
            // M12b: a click on any other element dispatches a real `click` to JS
            // (bubbling to ancestors and delegated handlers). preventDefault
            // consumes the click; otherwise a handler may still have mutated the
            // DOM (reconciled here), so we redraw while letting the default
            // action — link nav / dropping focus — proceed. No-op on script-less
            // pages.
            let mut ran_handler = false;
            if !self.node_to_js.is_empty() {
                if let Some(node) = self.element_at(x, y) {
                    match self.dispatch_dom(node, "click", "{}") {
                        Some(true) => return true,
                        Some(false) => ran_handler = true,
                        None => {}
                    }
                }
            }
            let had_focus = self.focused_field.take().is_some();
            if let Some(href) = self.link_at(x, y) {
                self.open_link(&href);
                return true;
            }
            if had_focus || ran_handler {
                return true; // redraw after a focus change or a handler's mutation
            }
        }
        let action = self.toolbar.hit_test(self.last_size, x, y);
        if action == ToolbarAction::None && self.toolbar.url_focused {
            self.toolbar.blur_url();
            return true;
        }
        self.handle(action)
    }

    fn text_input(&mut self, c: char) -> bool {
        // The MIRC panel is read-only (no text entry yet); it swallows typing so
        // keystrokes don't leak to the URL box or page behind it.
        if self.mirc_open {
            return true;
        }
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
                self.fire_input(id);
            }
            return true;
        }
        false
    }

    fn submit(&mut self) -> bool {
        if self.mirc_open {
            return true;
        }
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
        if self.mirc_open {
            return true;
        }
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
            self.fire_input(id);
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

/// Whether a **script-initiated** navigation may target `url`. Only web schemes
/// (`http(s)`) are allowed: a page's script must not drive the head to
/// `javascript:`/`data:`/`blob:`/`mailto:` (ignored, never fetched) nor to our
/// internal `cerberus:` pages — web content navigating to the browser's own
/// privileged scheme is something a real browser forbids (like `chrome://`).
/// User gestures (toolbar, links) reach `cerberus:` through `begin_load`
/// directly; this guard governs only the script path.
fn is_navigable_scheme(url: &str) -> bool {
    let s = url.trim_start();
    let has = |p: &str| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    has("http://") || has("https://")
}

fn first_party_of(url: &cerberus_url::Url) -> Option<Origin> {
    url.origin().or_else(|| {
        url.opaque
            .as_ref()
            .map(|o| Origin::new(url.scheme.clone(), o.clone(), None))
    })
}

/// The cookies this instance's sealed jar would expose to `document.cookie` for
/// `origin` under `first_party`, formatted as `"name=value; name=value"`.
/// Read-only in two senses: unlike the attach path it does **not** consume
/// `Allow-once` cookies (those are spent on a real request, not a script read),
/// and it drops `HttpOnly` cookies — those ride the network `Cookie` header but
/// must never be visible to page script, exactly as a real browser hides them
/// from `document.cookie`.
fn cookie_seed(
    storage: &Mutex<StorageEnvironment>,
    instance: InstanceId,
    origin: &Origin,
    first_party: &Origin,
) -> String {
    storage
        .locked()
        .instance(instance)
        .cookies_for_request(origin, first_party)
        .iter()
        .filter(|c| !c.http_only)
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether a fetch-error string (the `Debug` of a `NetError`, as stringified by
/// `fetch_page`) is a DNS-resolution failure. Switching an https upgrade to
/// plaintext http can't fix a name that never resolved, so these are reported
/// with their real cause rather than the misleading "doesn't support HTTPS"
/// prompt.
fn is_dns_failure(err: &str) -> bool {
    err.starts_with("Dns(")
}

/// Decide whether a response is a download (and its filename) rather than a page
/// to render. A download is signalled by `Content-Disposition: attachment`, or by
/// a content type we don't render (anything that isn't HTML/XML/plain text). The
/// filename comes from the `Content-Disposition` `filename=`, else the URL's last
/// path segment, else a generic name.
fn download_target(headers: &[(String, String)], url: &str) -> Option<String> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let disposition = header("content-disposition").unwrap_or("");
    let is_attachment = disposition.to_ascii_lowercase().contains("attachment");

    let content_type = header("content-type").unwrap_or("");
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // We render HTML/XHTML/XML and plain text; everything else (zip, pdf, octet-
    // stream, …) is a download. An empty/absent type defaults to renderable so a
    // misconfigured page isn't force-downloaded.
    let renderable = mime.is_empty()
        || mime == "text/html"
        || mime == "application/xhtml+xml"
        || mime == "text/plain"
        || mime == "text/xml"
        || mime == "application/xml";
    if !is_attachment && renderable {
        return None;
    }

    let from_disposition = disposition
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("filename="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .find(|v| !v.is_empty());
    let from_url = parse_url(url)
        .ok()
        .and_then(|u| u.path.rsplit('/').next().map(str::to_string))
        .filter(|s| !s.is_empty());
    Some(sanitize_filename(
        &from_disposition
            .or(from_url)
            .unwrap_or_else(|| "download".to_string()),
    ))
}

/// Strip any directory components and parent refs from a filename so a server
/// can't write outside the downloads directory.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let cleaned = cleaned.trim_matches('.').trim();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned.to_string()
    }
}

/// A path in `dir` for `filename` that doesn't exist yet: `name`, then
/// `name (1)`, `name (2)`, … so a download never overwrites an existing file.
fn unique_download_path(dir: &Path, filename: &str) -> PathBuf {
    let first = dir.join(filename);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (filename.to_string(), String::new()),
    };
    for n in 1.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// The "download complete" page shown after a file is saved.
fn download_done_document(filename: &str, size: usize, path: &Path) -> Document {
    let mut b = DocumentBuilder::new();
    let mut kids = Vec::new();
    for (tag, text) in [
        ("h1", "Download complete".to_string()),
        ("p", format!("Saved {filename} ({} bytes)", size)),
        ("p", format!("to {}", path.display())),
    ] {
        let t = b.text(text);
        kids.push(b.element(tag, [t]));
    }
    let body = b.element("body", kids);
    let root = b.element("#root", [body]);
    b.finish(root)
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

/// The `value` attribute of the control with field index `field_id` in `root`
/// (its current live value after a dispatch), `None` if not found.
fn control_value(styled_root: &StyledNode, doc: &Document, field_id: u32) -> Option<String> {
    collect_controls(styled_root, doc)
        .iter()
        .find(|c| c.id == field_id)
        .and_then(|c| c.el.attr("value"))
        .map(str::to_string)
}

/// Whether `tag` is a control that consumes a field id (the same set layout
/// counts: every `<input>`/`<textarea>`/`<select>`/`<button>`).
fn is_control_tag(tag: &str) -> bool {
    matches!(tag, "input" | "textarea" | "select" | "button")
}

/// Walk the **styled** tree in pre-order, assigning each control its field id and
/// recording its nearest enclosing `<form>`. This is the *single canonical*
/// numbering the app shares with layout, so a clicked box maps to the right
/// control and submission groups controls by their real form.
///
/// It must run over the styled tree, not the raw DOM: layout skips a
/// `display:none` subtree entirely and never issues field ids inside it, so we
/// have to skip the same controls. Numbering them here (as the old raw-DOM walk
/// did) let a hidden control consume an app-side id layout never assigned,
/// desyncing every later control's clicks and submitted values (#51). Each
/// `ControlRef` still points at the raw DOM node (resolved via `node_id`) so all
/// the attribute/value helpers below are unchanged.
fn collect_controls<'a>(styled_root: &StyledNode, doc: &'a Document) -> Vec<ControlRef<'a>> {
    let mut out = Vec::new();
    let mut next_id = 0u32;
    walk_controls(styled_root, None, doc, &mut next_id, &mut out);
    out
}

fn walk_controls<'a>(
    node: &StyledNode,
    form: Option<NodeRef<'a>>,
    doc: &'a Document,
    next_id: &mut u32,
    out: &mut Vec<ControlRef<'a>>,
) {
    // Mirror layout's skip of a `display:none` subtree so the ids stay aligned.
    if node.style.display == Display::None {
        return;
    }
    if is_control_tag(&node.tag) {
        if let Some(el) = doc.node(node.node_id) {
            out.push(ControlRef {
                id: *next_id,
                el,
                form,
            });
            *next_id += 1;
        }
    }
    // Descend; controls inside a <form> inherit it as their enclosing form.
    let inner_form = if node.tag == "form" {
        doc.node(node.node_id).or(form)
    } else {
        form
    };
    for child in &node.children {
        if let StyledChild::Element(e) = child {
            walk_controls(e, inner_form, doc, next_id, out);
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

/// Whether a control is a file input (`<input type=file>`).
fn is_file_input(el: NodeRef<'_>) -> bool {
    el.tag() == "input"
        && el
            .attr("type")
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("file"))
}

/// Build a `multipart/form-data` body for the form's successful controls,
/// returning `(content_type_with_boundary, body)`. Text controls become text
/// parts; a file input's value is read as a **filesystem path** (typed into the
/// field, or set programmatically by the mirror driver) and sent as a file part.
/// Filling-only: no file is read unless the user/automation supplied its path.
fn build_multipart(
    controls: &[ControlRef<'_>],
    form: Option<NodeRef<'_>>,
    store: &FormStore,
) -> (String, Vec<u8>) {
    // A unique boundary that won't collide with the data.
    let boundary = format!(
        "----CerberusFormBoundary{}",
        random_bytes(12)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let mut body = Vec::new();
    for c in controls.iter().filter(|c| same_form(c.form, form)) {
        let Some(name) = c.el.attr("name").filter(|n| !n.is_empty()) else {
            continue;
        };
        if is_file_input(c.el) {
            // The field value is a path; an empty path sends an empty file part
            // (matches browsers, which still send the part with a blank filename).
            let path = store
                .value(c.id)
                .map(str::to_string)
                .unwrap_or_else(|| c.el.attr("value").unwrap_or("").to_string());
            let (filename, ctype, bytes) = if path.is_empty() {
                (String::new(), "application/octet-stream", Vec::new())
            } else {
                let filename = path.rsplit(['/', '\\']).next().unwrap_or(&path).to_string();
                // An unreadable path still sends the part (empty) so the server
                // sees the field — the upload just carries no bytes.
                match std::fs::read(&path) {
                    Ok(bytes) => (
                        filename.clone(),
                        guess_upload_content_type(&filename),
                        bytes,
                    ),
                    Err(_) => (filename, "application/octet-stream", Vec::new()),
                }
            };
            write_file_part(&mut body, &boundary, name, &filename, ctype, &bytes);
        } else {
            for value in control_values(c, store) {
                write_text_part(&mut body, &boundary, name, &value);
            }
        }
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Write one text part to a multipart `body`.
fn write_text_part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
            escape_quotes(name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

/// Write one file part (with a filename + content type) to a multipart `body`.
fn write_file_part(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
            escape_quotes(name),
            escape_quotes(filename)
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

/// Escape `"` and CR/LF in a multipart header parameter (name/filename).
fn escape_quotes(s: &str) -> String {
    s.replace('"', "%22").replace(['\r', '\n'], "")
}

/// Guess an upload part's `Content-Type` from a filename extension (a small,
/// common set; anything else is the generic binary type).
fn guess_upload_content_type(filename: &str) -> &'static str {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" | "csv" | "text" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
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
fn paint_insecure_button(fb: &mut Framebuffer, text: &TextEngine, scale: f32) -> Rect {
    // `rect` is logical (returned for hit-testing, which works in logical px);
    // the painted list is scaled to the physical surface.
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
    text.rasterize(&list.scaled(scale), fb);
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
    scale: f32,
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
    // A glyph's origin.y is the TOP of the text box, so a label is vertically
    // centered in an `h`-tall row at `y + (h - px) / 2` — the same formula the
    // toolbar buttons use. (The old `+ 16` was a baseline-style offset that left
    // the 14px label hanging below the 22px row.)
    let row_label_dy = (settings_cookies_rect(size).h as i32 - 14) / 2;
    // Entry point to the cookie inspector.
    let cr = settings_cookies_rect(size);
    list.push(DisplayItem::Rect {
        rect: cr,
        color: Color::rgb(0xE6, 0xEE, 0xF6),
    });
    list.push(DisplayItem::Glyphs {
        origin: Point::new(cr.x + 8, cr.y + row_label_dy),
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
        origin: Point::new(tr.x + 8, tr.y + row_label_dy),
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
    raster.rasterize(&list.scaled(scale), fb);
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
                cookie: String::new(),
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

/// Result of [`mirror_bench`]: the large-N mirror-group gate (E3/ADR-0026).
pub struct MirrorBench {
    /// How many sealed instances the group held.
    pub instances: usize,
    /// Time to focus every instance once from cold (each rebuilds from the log).
    pub cold_sweep_ms: f64,
    /// Time to re-focus every instance — converged, resident snapshots are
    /// reused without a rebuild (E2), so this should be far below the cold sweep.
    pub warm_sweep_ms: f64,
    /// Resident set after dropping dormant snapshots, or `None` off-procfs.
    pub peak_rss_kb: Option<u64>,
}

/// Large-N mirror-group benchmark/gate (E3/ADR-0026): build a group of `n`
/// sealed instances over a built-in page, drive a fixed action, then sweep focus
/// across every instance twice — cold (each rebuilds) and warm (each reuses its
/// converged snapshot) — and read resident memory after releasing the dormant
/// snapshots. Guards the catch-up perf (E1/E2) and the bounded-memory model that
/// keeps thousands of profiles affordable (PLAN §1).
pub fn mirror_bench(n: usize) -> Result<MirrorBench, String> {
    let members: Vec<(InstanceId, String, String)> = (0..n)
        .map(|i| {
            (
                InstanceId::from_u64_pair(0, i as u64 + 1),
                format!("id{i}"),
                String::new(),
            )
        })
        .collect();
    let engine = QuickJsEngineFactory
        .instantiate()
        .map_err(|e| format!("{e:?}"))?;
    let mut group = cerberus_mirror::MirrorGroup::new(
        engine,
        Box::new(mirror::AppPageSource::builtin_only()),
        members,
        (1280, 800),
        DEFAULT_USER_AGENT,
    )
    .map_err(|e| e.to_string())?;

    group
        .act(cerberus_mirror::Action::Navigate("cerberus:about".into()))
        .map_err(|e| e.to_string())?;

    // Cold sweep: the first focus of each instance rebuilds it from the log.
    let t = Instant::now();
    for i in 0..n {
        group.focus(i).map_err(|e| e.to_string())?;
    }
    let cold_sweep_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Warm sweep: every instance is now a converged, resident snapshot, so
    // re-focusing reuses it without a realm rebuild or page reload (E2).
    let t = Instant::now();
    for i in 0..n {
        group.focus(i).map_err(|e| e.to_string())?;
    }
    let warm_sweep_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Drop dormant snapshots — the model that keeps resident memory ~one live
    // document regardless of N.
    group.release_dormant();
    let peak_rss_kb = resident_set_kb();

    Ok(MirrorBench {
        instances: n,
        cold_sweep_ms,
        warm_sweep_ms,
        peak_rss_kb,
    })
}

/// Process resident set size in kilobytes, via the [`cerberus_sysmem`] adapter
/// (procfs on Linux, the Win32 working set on Windows, `None` elsewhere —
/// ADR-0015). `mem-gate` degrades gracefully when this is `None`.
pub use cerberus_sysmem::resident_set_kb;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_policy_default_and_per_image_overrides() {
        // Default graphical: nothing is text-only until an override matches.
        let g = ImagePolicy {
            default: ImageDisplayMode::Graphical,
            overrides: vec!["logo".into()],
        };
        assert!(!g.text_only("https://x.test/photo.jpg"));
        assert!(
            g.text_only("https://x.test/logo-v2.png"),
            "override forces text-only"
        );

        // Default text-only: everything is text-only unless an override flips it
        // back to graphical.
        let t = ImagePolicy {
            default: ImageDisplayMode::TextOnly,
            overrides: vec!["hero".into()],
        };
        assert!(t.text_only("https://x.test/photo.jpg"));
        assert!(
            !t.text_only("https://x.test/hero-banner.jpg"),
            "override flips to graphical"
        );

        // Empty override strings never match (would otherwise flip everything).
        let e = ImagePolicy {
            default: ImageDisplayMode::Graphical,
            overrides: vec![String::new()],
        };
        assert!(!e.text_only("https://x.test/anything.png"));
    }

    #[test]
    fn image_display_mode_parses() {
        assert_eq!(
            ImageDisplayMode::parse("text-only"),
            ImageDisplayMode::TextOnly
        );
        assert_eq!(ImageDisplayMode::parse("text"), ImageDisplayMode::TextOnly);
        assert_eq!(
            ImageDisplayMode::parse("graphical"),
            ImageDisplayMode::Graphical
        );
        assert_eq!(
            ImageDisplayMode::parse("nonsense"),
            ImageDisplayMode::Graphical
        );
    }

    #[test]
    fn text_only_mode_skips_img_but_still_fetches_css_backgrounds() {
        // Regression: under a text-only default, an <img> renders as its
        // alt/caption and is never fetched, but a CSS `background-image` has no
        // text substitute — it must still go to the network (and, absent an
        // Allow rule, be consent-*Blocked*, not silently dropped as TextOnly).
        let doc = parse_html(
            "<html><body>\
               <img src='http://x.test/logo.png' alt='Logo'>\
               <div style='background-image:url(http://other.test/bg.png)'>x</div>\
             </body></html>",
        );
        let styled = CssEngine::new().style(&doc);
        let base = parse_url("http://x.test/").unwrap();
        let client = network_client(false, None, None);
        let first_party = Origin::new("http", "x.test", None);
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let ctx = FetchContext {
            instance,
            kind: FetchKind::Subresource {
                first_party: first_party.clone(),
            },
        };
        // Headless so the consent gate denies the third-party background
        // silently — no network is touched by either code path.
        let policy = Mutex::new(DefaultDenyPolicy::new(false));
        let images = ImagePolicy {
            default: ImageDisplayMode::TextOnly,
            overrides: Vec::new(),
        };

        let out = fetch_images_sync(
            &doc,
            &styled,
            &base,
            &client,
            &ctx,
            &policy,
            &first_party,
            800,
            600,
            &images,
        );

        // The <img> short-circuits to a text chip before any fetch.
        assert!(
            matches!(
                out.get("http://x.test/logo.png"),
                Some(ImageState::TextOnly)
            ),
            "the <img> should render as a text chip"
        );
        // The background is NOT text-only: it reaches the consent gate and, as an
        // unruled third party, is Blocked — the point is it is not swallowed as
        // TextOnly (which would erase it from the page with no text fallback).
        assert!(
            matches!(
                out.get("http://other.test/bg.png"),
                Some(ImageState::Blocked)
            ),
            "the CSS background must reach the network path, not be dropped as TextOnly"
        );
    }

    #[test]
    fn collect_image_urls_resolves_a_picture_to_its_selected_source() {
        // On a narrow (500px) viewport the mobile source wins; the fetch list
        // must carry exactly that URL — not the desktop source, and not also the
        // <img> fallback (which would double-fetch).
        let doc = parse_html(
            "<picture>\
               <source media='(min-width: 900px)' srcset='desktop.png'>\
               <source media='(max-width: 600px)' srcset='mobile.png'>\
               <img src='fallback.png' alt='hero'>\
             </picture>",
        );
        let mut out = Vec::new();
        collect_image_urls(doc.root(), &mut out, 500, 800);
        assert_eq!(out, vec!["mobile.png".to_string()]);

        // A wide viewport matches the (min-width: 900px) desktop source.
        let mut wide = Vec::new();
        collect_image_urls(doc.root(), &mut wide, 1000, 800);
        assert_eq!(wide, vec!["desktop.png".to_string()]);

        // A mid viewport matches neither source → the <img> fallback is used.
        let mut mid = Vec::new();
        collect_image_urls(doc.root(), &mut mid, 700, 800);
        assert_eq!(mid, vec!["fallback.png".to_string()]);
    }

    #[test]
    fn collect_image_urls_picture_without_a_direct_img() {
        // A <source>-only <picture> selects nothing: with no <img> to paint, a
        // matching <source> must not be fetched (it would waste network/decode
        // budget on bytes layout never draws).
        let only_source = parse_html(
            "<picture>\
               <source media='(max-width: 600px)' srcset='mobile.png'>\
             </picture>",
        );
        let mut out = Vec::new();
        collect_image_urls(only_source.root(), &mut out, 500, 800);
        assert!(out.is_empty(), "no <img> ⇒ nothing to fetch, got {out:?}");

        // But an <img> nested (invalidly) below another element still renders in
        // browsers, so the collector must fall through and reach it — matching
        // layout, which also falls through to lay it out.
        let nested = parse_html("<picture><figure><img src='/nested.png'></figure></picture>");
        let mut out2 = Vec::new();
        collect_image_urls(nested.root(), &mut out2, 500, 800);
        assert_eq!(out2, vec!["/nested.png".to_string()]);
    }

    #[test]
    fn mirror_bench_drives_many_instances_within_budget() {
        // A modest N keeps the test fast; the CLI gate uses 256/1024.
        let bench = mirror_bench(8).expect("mirror-bench runs");
        assert_eq!(bench.instances, 8);
        // Both sweeps completed; the warm sweep reuses converged snapshots, so it
        // is no slower than the cold sweep (E2) — allow slack for timer noise.
        assert!(bench.warm_sweep_ms <= bench.cold_sweep_ms + 5.0);
        // Resident memory after releasing dormant snapshots is well within
        // budget. RSS is PROCESS-wide, so under the default parallel test
        // runner other tests' live allocations (e.g. decoded SVG rasters)
        // pollute the number — only assert when running serially
        // (`RUST_TEST_THREADS=1`); the CLI bench gate (256/1024) enforces the
        // budget in isolation regardless.
        let serial = std::env::var("RUST_TEST_THREADS").is_ok_and(|v| v == "1");
        if let (true, Some(kb)) = (serial, bench.peak_rss_kb) {
            assert!(
                kb as f64 / 1024.0 <= 64.0,
                "resident {:.1} MB exceeds the 64 MB budget",
                kb as f64 / 1024.0
            );
        }
    }

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

    #[test]
    fn settings_row_labels_stay_inside_their_highlight_boxes() {
        // Regression: a glyph's origin.y is the TOP of the text, so a 14px label
        // in an `h`-tall clickable row must sit at `(h - 14)/2`, not the old `+16`
        // baseline-style offset that pushed it below the box. Paint the overlay
        // and assert the strip just below each row's highlight box is blank — no
        // label ink spilled out the bottom.
        let size = Size::new(800, 650);
        let mut fb = Framebuffer::new(size);
        fb.clear(Color::WHITE);
        let text = TextEngine::new();
        paint_settings_overlay(&mut fb, size, &text, &text, true, 3, None, false, 1.0);
        for row in [settings_cookies_rect(size), settings_timers_rect(size)] {
            let below = (row.y + row.h as i32) as u32;
            for y in below..below + 5 {
                for x in (row.x as u32)..(row.x as u32 + row.w) {
                    if let Some(c) = fb.pixel(x, y) {
                        assert!(
                            c.r > 0xC8 && c.g > 0xC8 && c.b > 0xC8,
                            "label ink spilled below the row box at ({x},{y}): #{:02x}{:02x}{:02x}",
                            c.r,
                            c.g,
                            c.b
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn visible_text_skips_hidden_and_display_none() {
        // `--dump-text` must reflect what is painted: a `[hidden]` element (UA
        // `display:none`), an inline `display:none`, and everything nested under
        // a `display:none` subtree contribute no text; `<script>`/`<style>`
        // payloads are code, not page text.
        let doc = parse_html(
            "<p>A</p>\
             <p hidden>H</p>\
             <p style='display:none'>N</p>\
             <div style='display:none'><span>NESTED</span></div>\
             <script>var s = 1;</script>\
             <style>.x { color: red }</style>\
             <p>B</p>",
        );
        let styled = CssEngine::new().style(&doc);
        // The two visible <p> blocks read on their own lines.
        assert_eq!(visible_text(&styled.root), "A\nB");
    }

    #[test]
    fn canvas_background_propagates_body_color_to_the_viewport() {
        // A `<body>` background (with no `<html>` background) propagates to the
        // whole canvas — the fix for pages like example.com whose grey body
        // filled only its short box in Cerberus while Chrome filled the page.
        let styled = CssEngine::new().style(&parse_html(
            "<body style='background:#f0f0f2'><p>hi</p></body>",
        ));
        assert_eq!(
            canvas_background(&styled, Color::WHITE),
            Color::rgb(0xf0, 0xf0, 0xf2)
        );
    }

    #[test]
    fn canvas_background_prefers_html_then_falls_back() {
        // `<html>` background wins over `<body>`'s (root-element propagation).
        let s1 = CssEngine::new().style(&parse_html(
            "<html style='background:#112233'><body style='background:#445566'></body></html>",
        ));
        assert_eq!(
            canvas_background(&s1, Color::WHITE),
            Color::rgb(0x11, 0x22, 0x33)
        );
        // Neither sets one → the fallback (white) shows.
        let s2 = CssEngine::new().style(&parse_html("<body><p>x</p></body>"));
        assert_eq!(canvas_background(&s2, Color::WHITE), Color::WHITE);
    }

    #[test]
    fn canvas_background_composites_a_translucent_body_over_white() {
        // A 50%-alpha black body background composites to mid-grey over white.
        let styled = CssEngine::new().style(&parse_html(
            "<body style='background:rgba(0,0,0,0.5)'></body>",
        ));
        let bg = canvas_background(&styled, Color::WHITE);
        assert!(
            (bg.r as i32 - 127).abs() <= 1 && bg.r == bg.g && bg.g == bg.b,
            "expected ~127 grey, got {bg:?}"
        );
    }

    #[test]
    fn visible_text_separates_blocks_and_keeps_inline_together() {
        // Block-level siblings (list items, paragraphs) each get their own line;
        // inline runs (text + <b>/<a>) stay on one line; <br> forces a break.
        let doc = parse_html(
            "<ul><li>one</li><li>two</li></ul>\
             <p>in<b>line</b><br>after break</p>",
        );
        let styled = CssEngine::new().style(&doc);
        // Inline `in` + `<b>line</b>` join with no break; `<br>` splits the line.
        // (Inter-element source spaces are dropped at parse — see #137 — so this
        // asserts only what `visible_text` itself controls.)
        assert_eq!(visible_text(&styled.root), "one\ntwo\ninline\nafter break");
    }

    #[test]
    fn mirc_panel_opens_lists_identities_broadcasts_and_closes() {
        let mut app = BrowserApp::new();
        // The SYNC button advertises one driver per identity.
        let n = app.heads.heads().len();
        assert!(n >= 1);
        assert_eq!(app.toolbar.sync_count, n);

        // The SYNC action opens the MIRC panel (it no longer toggles broadcast).
        assert!(!app.mirc_open);
        assert!(app.handle(ToolbarAction::OpenSync));
        assert!(app.mirc_open);
        assert!(
            !app.toolbar.broadcasting,
            "opening the panel doesn't broadcast"
        );

        // The roster has one row per identity; exactly the active head is live,
        // and on a built-in page (no site) nothing reads as logged in.
        let rows = app.mirc_rows();
        assert_eq!(rows.len(), n);
        assert_eq!(
            rows.iter().filter(|r| r.state == MircState::Live).count(),
            1
        );
        assert_eq!(rows[app.heads.active_index()].state, MircState::Live);
        assert!(rows.iter().all(|r| !r.account.is_empty()));
        assert!(rows.iter().all(|r| !r.logged_in));

        // The panel paints over a frame without panicking (sets last_size).
        let size = Size::new(1000, 700);
        let _ = app.render_frame(size);

        // Broadcast toggles from the panel now (it moved off the toolbar button).
        app.apply_mirc_action(MircAction::ToggleBroadcast);
        assert!(app.toolbar.broadcasting);
        app.apply_mirc_action(MircAction::ToggleBroadcast);
        assert!(!app.toolbar.broadcasting);

        // A click outside the panel dismisses it.
        let p = MircPanel::panel_rect(size);
        assert!(app.pointer_down((p.x - 5).max(0), p.y + 5));
        assert!(!app.mirc_open);

        // Opening a *different* identity surfaces it (becomes the active head)
        // and closes the panel — the lazy "select → render" gesture.
        if n >= 2 {
            app.handle(ToolbarAction::OpenSync);
            let other = (app.heads.active_index() + 1) % n;
            app.apply_mirc_action(MircAction::Open(other));
            assert_eq!(app.heads.active_index(), other);
            assert!(!app.mirc_open);
        }
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

    #[test]
    fn profile_csv_import_creates_identities_and_round_trips_export() {
        let dir = std::env::temp_dir().join(format!("cerb-csv-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_str().unwrap();

        // One existing default label ("work") + one brand-new identity.
        // (Quoting/delimiter edge cases are covered by the cerberus-autofill csv
        // unit tests; this test exercises the vault round-trip + identity creation.)
        let csv = "identity:login.username:login.password:address.city:card.number\n\
                   work:ada:pw1:London:4111111111111111\n\
                   agent-x:bob:pw2:Paris:\n";
        let in_file = dir.join("in.csv");
        std::fs::write(&in_file, csv).unwrap();

        let report = profile_import(dir_s, in_file.to_str().unwrap(), "pass").expect("import");
        assert!(report
            .iter()
            .any(|l| l.contains("created identity \"agent-x\"")));
        // The new identity was persisted as a real head.
        let (heads, _) = load_heads(&dir).expect("heads saved");
        assert!(heads.iter().any(|h| h.label == "agent-x"));

        // Export and confirm the values survived the sealed vault round-trip.
        let out_file = dir.join("out.csv");
        let n = profile_export(dir_s, out_file.to_str().unwrap(), "pass", ':').expect("export");
        assert!(n >= 4, "3 defaults + agent-x");
        let text = std::fs::read_to_string(&out_file).unwrap();
        let rows = cerberus_autofill::profiles_from_csv(&text).unwrap();
        let work = rows.iter().find(|(l, _)| l == "work").unwrap();
        assert_eq!(work.1.login.username, "ada");
        assert_eq!(work.1.login.password, "pw1");
        assert_eq!(work.1.address.city, "London");
        assert_eq!(work.1.card.number, "4111111111111111");
        let ax = rows.iter().find(|(l, _)| l == "agent-x").unwrap();
        assert_eq!(ax.1.login.username, "bob");

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
            .locked()
            .instance(instance)
            .quarantined_names(&fp)
            .is_empty());
        assert_eq!(events.locked().len(), 1);
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
    fn cookie_seed_hides_httponly_from_document_cookie() {
        // `document.cookie` (seeded via cookie_seed) must mirror a real browser:
        // HttpOnly cookies ride the network Cookie header but are invisible to
        // script. Without the http_only filter, seeding would leak session tokens
        // to page JS (including a bot-challenge or XSS payload).
        let (jar, storage, _events, _cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let url = parse_url("https://shop.example.com/").unwrap();
        let fp = url.origin().unwrap();

        jar.set_cookie(instance, &url, &fp, "vis=1; Secure");
        jar.set_cookie(instance, &url, &fp, "sess=secret; HttpOnly; Secure");

        // The read seed for document.cookie hides the HttpOnly token...
        let seed = cookie_seed(&storage, instance, &fp, &fp);
        assert!(seed.contains("vis=1"), "visible cookie is seeded: {seed:?}");
        assert!(
            !seed.contains("sess"),
            "HttpOnly cookie must NOT reach document.cookie: {seed:?}"
        );

        // ...while the network Cookie header still carries BOTH (unchanged).
        let header = jar.cookie_header(instance, &url, &fp).unwrap_or_default();
        assert!(
            header.contains("sess=secret") && header.contains("vis=1"),
            "network header keeps HttpOnly: {header:?}"
        );
    }

    #[test]
    fn jar_applies_the_cookie_disposition_policy() {
        let (jar, env, _events, cookies) = jar_with_env();
        let instance = InstanceId::from_u64_pair(0, 0x10);
        let url = parse_url("https://shop.example.com/").unwrap();
        let fp = url.origin().unwrap();
        let site = fp.site();

        // Global default Block → first-party cookie is dropped on capture.
        cookies.locked().set_global(CookieDisposition::Block);
        jar.set_cookie(instance, &url, &fp, "a=1; Secure");
        assert!(env.locked().instance(instance).cookie_views().is_empty());

        // A per-cookie Timed override wins over the global Block.
        cookies
            .locked()
            .set_override(&site, "b", CookieDisposition::Timed(120));
        jar.set_cookie(instance, &url, &fp, "b=2; Secure");
        let views = env.locked().instance(instance).cookie_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "b");
        assert_eq!(views[0].disposition, CookieDisposition::Timed(120));

        // Allow-once: attached on the first request, gone on the second.
        cookies
            .locked()
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
    fn cookie_inspector_cycles_and_deletes() {
        let mut app = fake_app(vec![(
            "cerberus:home",
            Ok(page("cerberus:home", 200, None, "<p>hi</p>")),
        )]);
        let inst = app.heads.active().instance;
        let fp = Origin::new("https", "example.com", None);
        app.storage
            .locked()
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

        // The chip is now a clear three-state cycle: Allow → Session → Block.
        app.apply_cookie_action(CookieAction::Cycle(0));
        assert_eq!(
            app.cookie_policy
                .locked()
                .resolve("https://example.com", "sid"),
            CookieDisposition::Session
        );
        assert_eq!(app.cookie_rows().len(), 1, "session keeps the cookie");

        // Delete removes the cookie and records a Block override.
        app.apply_cookie_action(CookieAction::Delete(0));
        assert!(app.cookie_rows().is_empty());
        assert_eq!(
            app.cookie_policy
                .locked()
                .resolve("https://example.com", "sid"),
            CookieDisposition::Block
        );

        // Global default cycles without panicking.
        app.apply_cookie_action(CookieAction::CycleGlobal);
        assert_ne!(
            app.cookie_policy.locked().global(),
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
        let seen = seen.locked();
        assert_eq!(seen.len(), 2, "two page loads requested");
        assert_eq!(seen[0], first_instance);
        assert_ne!(seen[1], seen[0]);
        assert_eq!(seen[1], b.heads.active().instance);
    }

    // ---- Hermetic test harness: a fake loader, no network or threads. ----

    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    /// `(url, post-body)` of each page request the fake loader saw, in order.
    type SeenRequests = Arc<Mutex<Vec<(String, Option<PostBody>)>>>;

    struct FakeLoader {
        responses: HashMap<String, Result<FetchedPage, String>>,
        images: HashMap<String, Result<Vec<u8>, String>>,
        fetches: HashMap<String, Result<FetchResponse, String>>,
        queue: RefCell<VecDeque<Done>>,
        /// Instances seen on page requests, in order (head-switch tests).
        seen_instances: Arc<Mutex<Vec<InstanceId>>>,
        /// `(url, post)` of each page request, in order (form GET/POST tests).
        seen_requests: SeenRequests,
    }

    impl FakeLoader {
        fn new(responses: Vec<(&str, Result<FetchedPage, String>)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(u, r)| (u.to_string(), r))
                    .collect(),
                images: HashMap::new(),
                fetches: HashMap::new(),
                queue: RefCell::new(VecDeque::new()),
                seen_instances: Arc::new(Mutex::new(Vec::new())),
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_images(mut self, images: Vec<(&str, Result<Vec<u8>, String>)>) -> Self {
            self.images = images
                .into_iter()
                .map(|(u, r)| (u.to_string(), r))
                .collect();
            self
        }

        fn with_fetches(mut self, fetches: Vec<(&str, Result<FetchResponse, String>)>) -> Self {
            self.fetches = fetches
                .into_iter()
                .map(|(u, r)| (u.to_string(), r))
                .collect();
            self
        }
    }

    impl PageLoader for FakeLoader {
        fn request(&self, id: u64, url: String, post: Option<PostBody>, ctx: FetchContext) {
            self.seen_instances.locked().push(ctx.instance);
            self.seen_requests.locked().push((url.clone(), post));
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
        fn request_fetch(&self, id: u64, req: FetchRequest, _ctx: FetchContext) {
            // `req.url` is already absolute (resolved by `pump_fetches`).
            let result = self
                .fetches
                .get(&req.url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no canned fetch for {}", req.url)));
            self.queue
                .borrow_mut()
                .push_back(Done::Fetch { id, result });
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

    /// A loader whose page responses are SEQUENCED per URL: each request to a URL
    /// pops the next canned response, so a reload of the same URL can return a
    /// different page (an anti-bot interstitial first, the real page after the
    /// cookie is set). External scripts are served from `scripts`.
    struct SeqLoader {
        pages: RefCell<HashMap<String, VecDeque<Result<FetchedPage, String>>>>,
        scripts: HashMap<String, Vec<u8>>,
        queue: RefCell<VecDeque<Done>>,
    }
    impl SeqLoader {
        fn new(
            pages: Vec<(&str, Vec<Result<FetchedPage, String>>)>,
            scripts: Vec<(&str, &str)>,
        ) -> Self {
            Self {
                pages: RefCell::new(
                    pages
                        .into_iter()
                        .map(|(u, rs)| (u.to_string(), rs.into_iter().collect()))
                        .collect(),
                ),
                scripts: scripts
                    .into_iter()
                    .map(|(u, s)| (u.to_string(), s.as_bytes().to_vec()))
                    .collect(),
                queue: RefCell::new(VecDeque::new()),
            }
        }
    }
    impl PageLoader for SeqLoader {
        fn request(&self, id: u64, url: String, _post: Option<PostBody>, _ctx: FetchContext) {
            let result = self
                .pages
                .borrow_mut()
                .get_mut(&url)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| Err(format!("no more canned responses for {url}")));
            self.queue.borrow_mut().push_back(Done::Page {
                id,
                requested_url: url,
                result,
            });
        }
        fn request_subresource(&self, url: String, _ctx: FetchContext) {
            let bytes = self
                .scripts
                .get(&url)
                .cloned()
                .ok_or_else(|| format!("no script for {url}"));
            self.queue.borrow_mut().push_back(Done::Sub {
                url,
                bytes,
                elapsed: Duration::from_millis(0),
            });
        }
        fn request_fetch(&self, id: u64, _req: FetchRequest, _ctx: FetchContext) {
            // The sensor's XHR/fetch submission: answer 200 with an empty body.
            self.queue.borrow_mut().push_back(Done::Fetch {
                id,
                result: Ok(FetchResponse {
                    status: 200,
                    status_text: "OK".into(),
                    url: String::new(),
                    headers: vec![],
                    body: String::new(),
                }),
            });
        }
        fn try_recv(&mut self) -> Option<Done> {
            self.queue.get_mut().pop_front()
        }
        fn set_waker(&mut self, _waker: Arc<dyn Waker>) {}
    }

    #[test]
    fn reese84_handshake_cookie_gated_reload_renders_real_page() {
        // End-to-end acceptance for the bot-challenge machinery, all hermetic:
        // an interstitial serves an external sensor script; the sensor sets a
        // challenge cookie via document.cookie and reloads; the reload (2nd hit
        // to the same URL) serves the real page, which we render.
        let interstitial = "<html><head>\
             <script src=\"/sensor.js\"></script>\
             </head><body><div id=\"x\">checking your browser</div></body></html>";
        let real = "<html><body><h1 id=\"x\">Welcome to the real page</h1></body></html>";
        let sensor = "document.cookie = 'reese84=solved; Path=/'; location.reload();";
        let loader = SeqLoader::new(
            vec![(
                "https://shop.test/",
                vec![
                    Ok(page(
                        "https://shop.test/",
                        200,
                        Some("no-store"),
                        interstitial,
                    )),
                    Ok(page("https://shop.test/", 200, Some("no-store"), real)),
                ],
            )],
            vec![("https://shop.test/sensor.js", sensor)],
        );
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate("https://shop.test/");
        let mut guard = 0;
        while b.poll() {
            guard += 1;
            assert!(guard < 40, "did not converge");
        }
        // The reload served the real page, and it rendered.
        assert!(
            b.document
                .root()
                .text_content()
                .contains("Welcome to the real page"),
            "expected the real page after the cookie-gated reload; got {:?}",
            b.document.root().text_content()
        );
        // The challenge cookie was persisted into the sealed jar.
        let instance = b.heads.active().instance;
        let origin = Origin::new("https", "shop.test", None);
        let cookies = b
            .storage
            .locked()
            .instance(instance)
            .cookies_for_request(&origin, &origin);
        assert!(
            cookies
                .iter()
                .any(|c| c.name == "reese84" && c.value == "solved"),
            "reese84 cookie should be in the jar; got {:?}",
            cookies.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
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
    fn script_hiding_an_element_removes_it_from_the_render() {
        // The core of JS-driven show/hide (RENDERING_PARITY_PLAN.md W-F): a page
        // script that sets `style.display = 'none'` (the mechanism a fundraising
        // banner uses to hide itself) must drop the element from what is painted,
        // while the node stays in the DOM. Verified against the styled tree
        // (`visible_text` honors display:none), not the raw DOM text.
        let mut b = fake_app(vec![(
            "https://hide.test/",
            Ok(page(
                "https://hide.test/",
                200,
                None,
                "<div id=\"banner\">DONATE NOW</div><p>real content</p>\
                 <script>document.getElementById('banner').style.display = 'none'</script>",
            )),
        )]);
        b.navigate("https://hide.test/");
        assert!(b.poll());
        let painted = visible_text(&b.styled.root);
        assert!(
            painted.contains("real content"),
            "visible content present; got {painted:?}"
        );
        assert!(
            !painted.contains("DONATE NOW"),
            "script-hidden banner must not be painted; got {painted:?}"
        );
        // The banner is hidden, not deleted — it remains in the DOM.
        assert!(
            b.document.root().text_content().contains("DONATE NOW"),
            "hidden element stays in the DOM"
        );
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
    fn script_location_navigates_to_the_new_url() {
        // A page whose inline script assigns location.href triggers a fresh load
        // of the target — the mechanism a cookie-gated reload rides.
        let mut b = fake_app(vec![
            (
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<script>location.href = 'https://site.test/next'</script>",
                )),
            ),
            (
                "https://site.test/next",
                Ok(page(
                    "https://site.test/next",
                    200,
                    None,
                    "<h1>Arrived</h1>",
                )),
            ),
        ]);
        b.navigate("https://site.test/");
        // Drain the interstitial load and the navigation it triggers.
        let mut guard = 0;
        while b.poll() {
            guard += 1;
            assert!(guard < 20, "did not converge");
        }
        assert!(
            b.document.root().text_content().contains("Arrived"),
            "script navigation should have loaded /next; got {:?}",
            b.document.root().text_content()
        );
        assert_eq!(b.toolbar.url_text, "https://site.test/next");
    }

    #[test]
    fn script_reload_budget_caps_a_spin_loop() {
        // A page that reloads itself on every load must not spin forever: after the
        // user gesture refills the budget, only SCRIPT_NAV_CAP script reloads are
        // followed, then further ones are ignored.
        let loader = FakeLoader::new(vec![(
            "https://spin.test/",
            Ok(page(
                "https://spin.test/",
                200,
                None,
                "<script>location.reload()</script>",
            )),
        )]);
        let seen = loader.seen_requests.clone();
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate("https://spin.test/");
        let mut guard = 0;
        while b.poll() {
            guard += 1;
            assert!(guard < 50, "reload loop did not converge");
        }
        // One user-initiated request + SCRIPT_NAV_CAP script reloads, then capped.
        assert_eq!(seen.locked().len(), 1 + SCRIPT_NAV_CAP as usize);
    }

    #[test]
    fn headless_open_drive_settles_and_reads_text() {
        // The headless automation API (open + drive + is_settled + page_text)
        // loads a page through the worker loop and settles.
        let mut b = fake_app(vec![(
            "https://site.test/",
            Ok(page(
                "https://site.test/",
                200,
                None,
                "<p>hello headless</p>",
            )),
        )]);
        b.open("https://site.test/");
        let mut guard = 0;
        while !b.is_settled() && guard < 50 {
            b.drive();
            guard += 1;
        }
        assert!(b.is_settled(), "page settled");
        assert_eq!(b.status(), 200);
        assert!(
            b.page_text().contains("hello headless"),
            "rendered text; got {:?}",
            b.page_text()
        );
    }

    #[test]
    fn external_script_is_fetched_and_executed() {
        // A page whose ONLY script is external `<script src>` still installs a
        // realm; the fetched body runs against it and its DOM mutation shows up.
        let mut b = fake_app_img(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<div id=\"x\">old</div><script src=\"/sensor.js\"></script>",
                )),
            )],
            vec![(
                "https://site.test/sensor.js",
                Ok(b"document.getElementById('x').textContent = 'from-external';".to_vec()),
            )],
        );
        b.navigate("https://site.test/");
        let mut guard = 0;
        while b.poll() {
            guard += 1;
            assert!(guard < 20, "did not converge");
        }
        assert!(
            b.document.root().text_content().contains("from-external"),
            "external script should have run and mutated the DOM; got {:?}",
            b.document.root().text_content()
        );
    }

    #[test]
    fn script_navigation_ignores_non_navigable_schemes() {
        // location.href = 'javascript:…' / location.assign('data:…') must be
        // ignored (never fetched or shown as an error page), matching a browser
        // that doesn't navigate to those schemes.
        let loader = FakeLoader::new(vec![(
            "https://site.test/",
            Ok(page(
                "https://site.test/",
                200,
                None,
                "<script>location.href = 'javascript:void(0)'; \
                 location.assign('data:text/html,x');</script>",
            )),
        )]);
        let seen = loader.seen_requests.clone();
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate("https://site.test/");
        let mut guard = 0;
        while b.poll() {
            guard += 1;
            assert!(guard < 10, "did not converge");
        }
        // Only the initial user navigation was fetched; the script schemes were
        // dropped, so we stay on site.test with no error page.
        assert_eq!(seen.locked().len(), 1, "no navigation to js:/data: schemes");
        assert_eq!(b.toolbar.url_text, "https://site.test/");
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
    fn browser_dns_failure_during_upgrade_reports_cause_not_https_prompt() {
        // A DNS failure (the name never resolved) must NOT be misreported as the
        // site lacking HTTPS — switching to plaintext can't help — so we show the
        // real cause and offer no insecure prompt.
        let mut b = fake_app(vec![(
            "https://nx.test/",
            Err("Dns(\"system DNS: no records for nx.test\")".to_string()),
        )]);
        b.navigate("http://nx.test/");
        assert!(b.poll());
        assert!(
            b.insecure_prompt.is_none(),
            "a DNS failure must not offer the plaintext prompt"
        );
        let text = b.document.root().text_content();
        assert!(text.contains("Cannot load page"), "got {text:?}");
        assert!(
            !text.contains("doesn't support HTTPS"),
            "DNS failure misreported as no-HTTPS: {text:?}"
        );
    }

    #[test]
    fn fragment_navigation_does_not_refetch() {
        let mut b = fake_app(vec![(
            "https://ex.test/",
            Ok(page("https://ex.test/", 200, None, "<h1>Home</h1>")),
        )]);
        b.navigate("https://ex.test/");
        assert!(b.poll());
        let before = b.document.root().text_content();
        // In-page anchor: same document, only the #fragment differs.
        b.navigate("https://ex.test/#section");
        assert!(b.pending.is_none(), "fragment nav must not start a fetch");
        assert_eq!(b.toolbar.url_text, "https://ex.test/#section");
        assert_eq!(
            b.document.root().text_content(),
            before,
            "document is unchanged by a fragment navigation"
        );
        assert!(b.toolbar.can_back, "fragment nav still records history");
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
        let policy = ImagePolicy::default();
        let provider = StoreImages {
            base: b.current_url.as_ref(),
            images: &b.images,
            policy: &policy,
        };
        assert!(
            provider.get("/pic.png").is_some(),
            "provider supplies the decoded image to layout"
        );
        // A frame renders without panicking now that an Image item is present.
        b.render_frame(Size::new(800, 600));
    }

    /// The `Image` display-item rects a layout of `styled` emits at 800×600.
    fn image_rects(styled: &StyledDom, images: &HashMap<String, ImageState>) -> Vec<Rect> {
        let policy = ImagePolicy::default();
        let provider = StoreImages {
            base: None,
            images,
            policy: &policy,
        };
        let text = TextEngine::new();
        let mut layout = BlockLayout::default();
        let laid = layout.layout(styled, Size::new(800, 600), &text, &provider, &NoForms);
        laid.display
            .items
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn inline_svg_renders_as_an_image_item_with_its_declared_box() {
        // End-to-end through the document-preparation seam: an inline <svg>
        // becomes a synthetic <img>, its bytes rasterize through the resvg
        // path, and layout emits an Image item at the svg's 100×40 box —
        // instead of the old `svg{display:none}` collapse.
        let mut doc = parse_html(
            "<p>hi</p><svg width='100' height='40'>\
             <rect width='100' height='40' fill='#ff0000'/></svg>",
        );
        let pairs = replace_inline_svgs(&mut doc, 800);
        let codec = ImageCodec::new();
        let mut images = HashMap::new();
        for (k, b) in &pairs {
            let img = codec.decode(b).expect("inline svg rasterizes");
            images.insert(k.clone(), ImageState::Ready(Arc::new(img)));
        }
        let styled = CssEngine::new().style(&doc);
        let rects = image_rects(&styled, &images);
        assert_eq!(rects.len(), 1, "one Image item for the inline svg");
        assert_eq!((rects[0].w, rects[0].h), (100, 40), "attr-sized box");
    }

    #[test]
    fn viewbox_only_inline_svg_sizes_like_chrome() {
        // Headless Chromium 139 measurements (2026-07-13, --headless=new
        // --dump-dom over getBoundingClientRect): <svg viewBox="0 0 400 100">
        // with no width/height stretches to its container, height from the
        // viewBox ratio — 800×200 in an explicit 800px div; **784×196** at
        // body level in an 800px window (8px body margin each side). Our
        // pipeline synthesizes the stretch as viewport-width attributes and
        // layout re-clamps to the actual containing block, landing on the
        // same 784×196.
        // The <body> matters: the UA sheet's `body{margin:8px}` supplies the
        // 8px inset Chrome shows (the engine no longer has a built-in page
        // margin), so the stretch clamps to 784 wide.
        let mut doc = parse_html(
            "<body><svg viewBox='0 0 400 100'>\
             <rect width='400' height='100' fill='#00ff00'/></svg></body>",
        );
        let pairs = replace_inline_svgs(&mut doc, 800);
        let codec = ImageCodec::new();
        let mut images = HashMap::new();
        for (k, b) in &pairs {
            let img = codec.decode(b).expect("inline svg rasterizes");
            images.insert(k.clone(), ImageState::Ready(Arc::new(img)));
        }
        let styled = CssEngine::new().style(&doc);
        let rects = image_rects(&styled, &images);
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].w, rects[0].h), (784, 196), "Chrome's stretch box");
    }

    #[test]
    fn browser_registers_inline_svg_without_a_network_fetch() {
        // The interactive path: commit → set_document rewrites the svg and
        // decodes it straight into the image store under its synthetic key —
        // the loader is never asked for it (nothing to consent-gate either).
        let mut b = fake_app(vec![(
            "https://svg.test/",
            Ok(page(
                "https://svg.test/",
                200,
                None,
                "<p>hi</p><svg width='24' height='24'>\
                 <rect width='24' height='24' fill='#0000ff'/></svg>",
            )),
        )]);
        b.navigate("https://svg.test/");
        assert!(b.poll());
        assert_eq!(b.images.len(), 1, "one store entry for the inline svg");
        let (key, state) = b.images.iter().next().unwrap();
        assert!(
            key.starts_with(inline_svg::INLINE_SVG_PREFIX),
            "synthetic key, not a URL: {key}"
        );
        assert!(matches!(state, ImageState::Ready(_)), "decoded eagerly");
        // The provider resolves the synthetic src verbatim (opaque scheme
        // round-trip), so layout finds the bitmap; a frame renders fine.
        let policy = ImagePolicy::default();
        let provider = StoreImages {
            base: b.current_url.as_ref(),
            images: &b.images,
            policy: &policy,
        };
        assert!(provider.get(key).is_some(), "provider serves the bitmap");
        b.render_frame(Size::new(800, 600));
    }

    /// The `color` of the first `<p>` in a styled tree (helper for the external
    /// stylesheet tests).
    fn styled_p_color(node: &cerberus_style::StyledNode) -> Option<Color> {
        if node.tag == "p" {
            return Some(node.style.color);
        }
        node.children.iter().find_map(|c| match c {
            cerberus_style::StyledChild::Element(e) => styled_p_color(e),
            _ => None,
        })
    }

    #[test]
    fn external_stylesheet_loads_via_worker_and_restyles() {
        // A first-party <link rel=stylesheet> is fetched on the worker, routed to
        // the cascade (not the image decoder), and re-styles the page when it
        // lands (ADR-0037).
        let mut b = fake_app_img(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head>\
                     <body><p>hi</p></body></html>",
                )),
            )],
            vec![("https://site.test/s.css", Ok(b"p{color:#ff0000}".to_vec()))],
        );
        b.navigate("https://site.test/");
        // One poll drains the page load (inline styling → UA-default black) and
        // the stylesheet it queued (re-styles to red).
        assert!(b.poll());
        assert_eq!(
            styled_p_color(&b.styled.root),
            Some(Color::rgb(0xff, 0, 0)),
            "external stylesheet applied via the async worker path"
        );
        // The sheet went to the cascade, not the image store, and none is left
        // pending.
        assert!(b.images.is_empty(), "stylesheet not stored as an image");
        assert!(b.pending_sheets.is_empty(), "no stylesheet left pending");
    }

    #[test]
    fn third_party_stylesheet_is_consent_blocked() {
        // A cross-site stylesheet is gated by default-deny: never fetched, never
        // applied — consistent with image subresources.
        let mut b = fake_app_img(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<html><head><link rel=\"stylesheet\" href=\"https://cdn.other/s.css\"></head>\
                     <body><p>hi</p></body></html>",
                )),
            )],
            vec![("https://cdn.other/s.css", Ok(b"p{color:#ff0000}".to_vec()))],
        );
        b.navigate("https://site.test/");
        assert!(b.poll());
        assert_eq!(
            styled_p_color(&b.styled.root),
            Some(Color::BLACK),
            "third-party stylesheet must not apply without consent"
        );
        assert!(
            b.pending_sheets.is_empty(),
            "blocked sheet is never dispatched"
        );
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

    fn loaded_with_fetches(
        responses: Vec<(&str, Result<FetchedPage, String>)>,
        fetches: Vec<(&str, Result<FetchResponse, String>)>,
        url: &str,
    ) -> BrowserApp {
        let loader = FakeLoader::new(responses).with_fetches(fetches);
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate(url);
        assert!(b.poll(), "page load + fetch cascade drained");
        b
    }

    #[test]
    fn js_fetch_loads_data_through_the_worker_and_rerenders() {
        // A scripted page fetches JSON and renders a field from it: the fetch is
        // routed to the (fake) worker, resolved in poll(), and the .then chain
        // re-renders the DOM — all within the load poll.
        let b = loaded_with_fetches(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<div id='x'>loading</div>\
                     <script>fetch('/api').then(function (r) { return r.json(); }) \
                       .then(function (d) { document.getElementById('x').textContent = String(d.v); });</script>",
                )),
            )],
            vec![(
                "https://site.test/api",
                Ok(FetchResponse {
                    status: 200,
                    status_text: "OK".into(),
                    url: "https://site.test/api".into(),
                    headers: vec![],
                    body: "{\"v\":7}".into(),
                }),
            )],
            "https://site.test/",
        );
        assert_eq!(
            text_of_id(b.document.root(), "x").as_deref(),
            Some("7"),
            "fetch().json() data rendered after the worker resolved the Promise"
        );
    }

    #[test]
    fn third_party_js_fetch_is_blocked_by_consent() {
        // A first-party page fetches a third-party URL with no Allow rule: the
        // fetch is rejected by the consent default-deny (never reaching the canned
        // response), and the script's .catch runs — JS fetch cannot bypass the
        // gate.
        let b = loaded_with_fetches(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<div id='x'>start</div>\
                     <script>fetch('https://tracker.evil/collect') \
                       .then(function () { document.getElementById('x').textContent = 'leaked'; }) \
                       .catch(function () { document.getElementById('x').textContent = 'blocked'; });</script>",
                )),
            )],
            vec![(
                "https://tracker.evil/collect",
                Ok(FetchResponse {
                    status: 200,
                    status_text: "OK".into(),
                    url: "https://tracker.evil/collect".into(),
                    headers: vec![],
                    body: "ok".into(),
                }),
            )],
            "https://site.test/",
        );
        assert_eq!(
            text_of_id(b.document.root(), "x").as_deref(),
            Some("blocked"),
            "third-party fetch is consent-blocked before the request, and .catch runs"
        );
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
    fn post_form_submits_a_body_not_a_query() {
        // A POST login form: the encoded controls must ride in the request body,
        // and the URL must stay query-free (the old behavior downgraded to GET).
        let responses = vec![
            (
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<form action='/login' method='POST'><input name='user'></form>",
                )),
            ),
            (
                "https://site.test/login",
                Ok(page("https://site.test/login", 200, None, "welcome")),
            ),
        ];
        let loader = FakeLoader::new(responses);
        let seen = loader.seen_requests.clone();
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate("https://site.test/");
        assert!(b.poll(), "form page loaded");

        b.render_frame(Size::new(800, 600));
        let r = b.form_fields[0].rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1));
        assert!(b.text_input('h'));
        assert!(b.text_input('i'));
        assert!(b.submit(), "submit consumed");

        // The URL is the bare action — the data is NOT in the query.
        assert_eq!(b.toolbar.url_text, "https://site.test/login");
        // The loader received a POST whose body is the urlencoded controls.
        let reqs = seen.locked().clone();
        let (url, post) = reqs.last().expect("a page request");
        assert_eq!(url, "https://site.test/login");
        let post = post.as_ref().expect("a POST body, not a GET");
        assert_eq!(post.content_type, "application/x-www-form-urlencoded");
        assert_eq!(String::from_utf8_lossy(&post.body), "user=hi");

        // The POST response renders (and is not cached).
        assert!(b.poll(), "post response drained");
        assert_eq!(b.status, 200);
    }

    #[test]
    fn shopping_flow_browse_add_to_cart_and_checkout() {
        // The e-commerce mechanics end-to-end, as a user drives them: browse a
        // storefront, click through to a product (a card link — an <a> wrapping
        // block content), submit the add-to-cart POST form, see the cart
        // reflect the item, then fill and submit the checkout form. Every hop
        // is a real click/keystroke against rendered hit boxes.
        let responses = vec![
            (
                "https://shop.test/",
                Ok(page(
                    "https://shop.test/",
                    200,
                    None,
                    "<h1>Shop</h1>\
                     <a href='/p/42'><div><h2>Ultra Widget</h2><p>$19</p></div></a>",
                )),
            ),
            (
                "https://shop.test/p/42",
                Ok(page(
                    "https://shop.test/p/42",
                    200,
                    None,
                    "<h1>Ultra Widget</h1><p>$19</p>\
                     <form action='/cart/add' method='POST'>\
                       <input type='hidden' name='sku' value='42'>\
                       Qty: <input name='qty'>\
                       <input type='submit' value='Add to cart'>\
                     </form>",
                )),
            ),
            (
                "https://shop.test/cart/add",
                Ok(page(
                    "https://shop.test/cart/add",
                    200,
                    None,
                    "<h1>Cart (1)</h1><p>2 x Ultra Widget</p>\
                     <form action='/checkout' method='POST'>\
                       Name: <input name='name'>\
                       <input type='submit' value='Place order'>\
                     </form>",
                )),
            ),
            (
                "https://shop.test/checkout",
                Ok(page(
                    "https://shop.test/checkout",
                    200,
                    None,
                    "<h1>Order placed</h1><p>Thank you.</p>",
                )),
            ),
        ];
        let loader = FakeLoader::new(responses);
        let seen = loader.seen_requests.clone();
        let mut b = BrowserApp::with_loader(Box::new(loader));
        b.navigate("https://shop.test/");
        assert!(b.poll(), "storefront loaded");
        b.render_frame(Size::new(800, 600));

        // 1. Click the product card (block content inside the anchor).
        let card = b
            .links
            .iter()
            .find(|l| l.href == "/p/42")
            .expect("product card link boxed")
            .rect;
        assert!(
            b.pointer_down(card.x + 1, card.y + 1),
            "card click consumed"
        );
        assert!(b.poll(), "product page loaded");
        assert_eq!(b.toolbar.url_text, "https://shop.test/p/42");
        assert!(b.page_text().contains("Ultra Widget"));
        b.render_frame(Size::new(800, 600));

        // 2. Type a quantity and add to cart (POST form).
        let qty = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Text))
            .expect("qty field box")
            .rect;
        assert!(b.pointer_down(qty.x + 1, qty.y + 1), "focus qty");
        assert!(b.text_input('2'));
        let add = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Button))
            .expect("add-to-cart button box")
            .rect;
        assert!(b.pointer_down(add.x + 1, add.y + 1), "add-to-cart click");
        assert_eq!(b.toolbar.url_text, "https://shop.test/cart/add");
        let reqs = seen.locked().clone();
        let (url, post) = reqs.last().expect("cart request seen");
        assert_eq!(url, "https://shop.test/cart/add");
        let post = post.as_ref().expect("add-to-cart is a POST");
        assert_eq!(
            String::from_utf8_lossy(&post.body),
            "sku=42&qty=2",
            "hidden sku + typed qty ride in the body"
        );
        assert!(b.poll(), "cart page loaded");
        assert!(b.page_text().contains("Cart (1)"), "cart shows the item");
        b.render_frame(Size::new(800, 600));

        // 3. Fill the checkout form and place the order.
        let name = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Text))
            .expect("name field box")
            .rect;
        assert!(b.pointer_down(name.x + 1, name.y + 1), "focus name");
        for c in "ada".chars() {
            assert!(b.text_input(c));
        }
        let order = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Button))
            .expect("place-order button box")
            .rect;
        assert!(b.pointer_down(order.x + 1, order.y + 1), "place order");
        let reqs = seen.locked().clone();
        let (url, post) = reqs.last().expect("checkout request seen");
        assert_eq!(url, "https://shop.test/checkout");
        assert_eq!(
            String::from_utf8_lossy(&post.as_ref().expect("checkout is a POST").body),
            "name=ada"
        );
        assert!(b.poll(), "confirmation loaded");
        assert!(b.page_text().contains("Order placed"));
    }

    #[test]
    fn collect_controls_skips_display_none_but_keeps_type_hidden() {
        // A `display:none` control (or one inside a display:none subtree) must not
        // consume a field id, because layout skips it too — otherwise every later
        // control's id desyncs from layout's numbering (#51). A `type=hidden`
        // input, which layout *does* lay out (id consumed, nothing painted), is
        // still counted.
        let doc = parse_html(
            "<input style='display:none' name='ghost'>\
             <div style='display:none'><input name='buried'></div>\
             <input type='hidden' name='h'>\
             <input name='first'><input name='second'>",
        );
        let styled = cerberus_css::CssEngine::new().style(&doc);
        let controls = collect_controls(&styled.root, &doc);
        let names: Vec<_> = controls
            .iter()
            .map(|c| c.el.attr("name").unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["h", "first", "second"],
            "display:none controls are skipped; type=hidden is kept"
        );
        // Ids are the contiguous 0..n layout also assigns to these three.
        assert_eq!(
            controls.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn multipart_builds_text_and_file_parts() {
        let dir = std::env::temp_dir().join(format!("cerb-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, b"FILE-CONTENTS").unwrap();

        let doc = parse_html(
            "<form method=\"post\" enctype=\"multipart/form-data\">\
             <input name=\"note\" value=\"hi\"><input type=\"file\" name=\"upload\"></form>",
        );
        let styled = cerberus_css::CssEngine::new().style(&doc);
        let controls = collect_controls(&styled.root, &doc);
        let form = controls[0].form;
        let mut store = FormStore::default();
        let file_id = controls.iter().find(|c| is_file_input(c.el)).unwrap().id;
        store
            .values
            .insert(file_id, file.to_string_lossy().to_string());

        let (ctype, body) = build_multipart(&controls, form, &store);
        assert!(ctype.starts_with("multipart/form-data; boundary=----CerberusFormBoundary"));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"note\""), "text part header");
        assert!(text.contains("\r\n\r\nhi\r\n"), "text part value");
        assert!(
            text.contains("name=\"upload\"; filename=\"hello.txt\""),
            "file part header"
        );
        assert!(text.contains("Content-Type: text/plain"), "guessed type");
        assert!(text.contains("FILE-CONTENTS"), "file bytes inlined");
        assert!(text.trim_end().ends_with("--"), "closing boundary");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_target_detects_attachments_and_binary_types() {
        // Content-Disposition: attachment → download, filename from the header.
        let h = vec![(
            "Content-Disposition".to_string(),
            "attachment; filename=\"report.csv\"".to_string(),
        )];
        assert_eq!(
            download_target(&h, "https://x.test/gen"),
            Some("report.csv".to_string())
        );
        // A non-renderable content type → download, filename from the URL.
        let h = vec![("content-type".to_string(), "application/zip".to_string())];
        assert_eq!(
            download_target(&h, "https://x.test/files/pack.zip"),
            Some("pack.zip".to_string())
        );
        // HTML / plain text render (not downloads).
        let h = vec![(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        )];
        assert_eq!(download_target(&h, "https://x.test/"), None);
        // Path traversal in the filename is stripped.
        let h = vec![(
            "Content-Disposition".to_string(),
            "attachment; filename=\"../../etc/passwd\"".to_string(),
        )];
        assert_eq!(
            download_target(&h, "https://x.test/x"),
            Some("passwd".to_string())
        );
    }

    #[test]
    fn attachment_response_is_saved_to_the_downloads_dir() {
        let dir = std::env::temp_dir().join(format!("cerb-dl-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let url = "https://files.test/export";
        let body = b"id,amount\n1,42\n".to_vec();
        let resp = FetchedPage {
            url: url.to_string(),
            status: 200,
            headers: vec![(
                "Content-Disposition".to_string(),
                "attachment; filename=\"claims.csv\"".to_string(),
            )],
            body: body.clone(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            elapsed: Duration::from_millis(3),
        };
        let loader = FakeLoader::new(vec![(url, Ok(resp))]);
        let mut app = BrowserApp::build(
            Box::new(loader),
            Arc::new(Mutex::new(StorageEnvironment::with_no_vault())),
            default_heads(),
            Some(dir.clone()),
        );
        app.navigate(url);
        assert!(app.poll(), "download drained");

        // The file was saved, and the page reports the download (not the bytes).
        let saved = dir.join("downloads").join("claims.csv");
        assert!(saved.exists(), "file written to downloads dir");
        assert_eq!(std::fs::read(&saved).unwrap(), body);
        assert!(app.page_text().contains("Download complete"));
        std::fs::remove_dir_all(&dir).ok();
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

    /// The `NodeId` of the first element with `id` (depth-first).
    fn node_id_of(node: NodeRef<'_>, id: &str) -> Option<NodeId> {
        if node.is_element() && node.attr("id") == Some(id) {
            return Some(node.id());
        }
        node.children().find_map(|c| node_id_of(c, id))
    }

    #[test]
    fn click_on_arbitrary_element_dispatches_to_js() {
        // A non-form element with a click handler: clicking inside its box runs
        // the handler, reached via the generic element hit map + dispatch.
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<div id='d'>hello</div>\
                     <script>document.getElementById('d').addEventListener('click', function () { \
                       this.setAttribute('data-hit', '1'); \
                     });</script>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));

        let d = node_id_of(b.document.root(), "d").expect("#d node id");
        let r = b
            .elements
            .iter()
            .find(|e| e.node == d)
            .expect("#d has an element hit box")
            .rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "click consumed");
        assert_eq!(
            attr_of_id(b.document.root(), "d", "data-hit").as_deref(),
            Some("1"),
            "the div's click handler ran via generic element dispatch"
        );
    }

    /// The text content of the first element with `id` (depth-first).
    fn text_of_id(node: NodeRef<'_>, id: &str) -> Option<String> {
        if node.is_element() && node.attr("id") == Some(id) {
            return Some(node.text_content());
        }
        node.children().find_map(|c| text_of_id(c, id))
    }

    #[test]
    fn load_time_timer_fires_before_first_paint() {
        // No interaction: the app's load path drains the bounded event loop
        // (ADR-0013), so a setTimeout scheduled at load has already mutated the
        // document by the time it is rendered.
        let b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<div id='d'>old</div>\
                     <script>setTimeout(function () { \
                       document.getElementById('d').textContent = 'timed'; }, 5000);</script>",
                )),
            )],
            "https://site.test/",
        );
        assert_eq!(
            text_of_id(b.document.root(), "d").as_deref(),
            Some("timed"),
            "a load-time timer fired during the bounded event loop"
        );
    }

    #[test]
    fn link_click_with_preventdefault_intercepts_navigation() {
        // A direct <a> onclick handler that calls preventDefault stops the
        // navigation (SPA router pattern), reached via the inline link hit box.
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<a href='/next' id='lnk'>go</a>\
                     <script>document.getElementById('lnk').addEventListener('click', function (e) { \
                       e.preventDefault(); \
                       document.getElementById('lnk').setAttribute('data-x', '1'); });</script>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let lnk = node_id_of(b.document.root(), "lnk").expect("#lnk node id");
        let r = b
            .elements
            .iter()
            .find(|e| e.node == lnk)
            .expect("#lnk has an inline link hit box")
            .rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "click consumed");
        assert_eq!(
            attr_of_id(b.document.root(), "lnk", "data-x").as_deref(),
            Some("1"),
            "the anchor's click handler ran"
        );
        assert!(
            b.pending.is_none(),
            "preventDefault intercepted the navigation"
        );
        assert_eq!(
            b.toolbar.url_text, "https://site.test/",
            "URL unchanged (no navigation)"
        );
    }

    #[test]
    fn link_click_without_preventdefault_still_navigates() {
        // A non-preventing handler runs, then the default link navigation
        // proceeds (dispatch does not swallow ordinary link clicks).
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<a href='/next' id='lnk'>go</a>\
                     <script>document.getElementById('lnk').addEventListener('click', function () { \
                       document.getElementById('lnk').setAttribute('data-x', '1'); });</script>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let lnk = node_id_of(b.document.root(), "lnk").expect("#lnk node id");
        let r = b
            .elements
            .iter()
            .find(|e| e.node == lnk)
            .expect("#lnk has an inline link hit box")
            .rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "click consumed");
        assert_eq!(
            b.toolbar.url_text, "https://site.test/next",
            "a non-prevented link click still navigates"
        );
    }

    #[test]
    fn typing_into_a_field_fires_input_and_reflects_handler_changes() {
        // A scripted input whose `input` handler uppercases its value: typing
        // fires the event, the handler reads e.target.value, and its rewrite
        // flows back into the rendered field value.
        let mut b = loaded(
            vec![(
                "https://site.test/",
                Ok(page(
                    "https://site.test/",
                    200,
                    None,
                    "<input id='t'>\
                     <script>document.getElementById('t').addEventListener('input', function (e) { \
                       e.target.value = e.target.value.toUpperCase(); });</script>",
                )),
            )],
            "https://site.test/",
        );
        b.render_frame(Size::new(800, 600));
        let r = b
            .form_fields
            .iter()
            .find(|f| matches!(f.kind, FieldKind::Text))
            .expect("text field box")
            .rect;
        assert!(b.pointer_down(r.x + 1, r.y + 1), "focus the field");
        assert!(b.text_input('h'));
        assert!(b.text_input('i'));
        assert_eq!(
            b.forms.value(0),
            Some("HI"),
            "the input handler uppercased the typed value and it round-tripped"
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

    // ---- `@import` inlining discovery (issue #64) ----
    //
    // These exercise the lex-aware, prologue-restricted discovery through
    // `inline_imports_core`, whose fetch closure stands in for the consent-gated
    // network `Router`. A closure recording every URL it is asked for lets us
    // assert that content-controlled `@import` look-alikes never trigger a fetch.

    #[test]
    fn inline_imports_ignores_commented_at_import() {
        // (a) A commented-out `@import` must neither fetch nor inline; the comment
        // is passed through untouched.
        let css = "/* @import url(\"https://evil.example/x.css\"); */\n.a { color: red }";
        let mut fetched: Vec<String> = Vec::new();
        let out = inline_imports_core(css, &mut |u| {
            fetched.push(u.to_string());
            Some("EVIL".to_string())
        });
        assert!(fetched.is_empty(), "commented @import must not fetch");
        assert_eq!(out, css, "comment (and sheet) pass through verbatim");
        assert!(!out.contains("EVIL"));
    }

    #[test]
    fn inline_imports_ignores_at_import_inside_string_value() {
        // (b) The literal `@import` inside a `content` string is a value, not an
        // at-rule: no fetch, output unchanged.
        let css = "a::after { content: \"@import url(x.css)\"; }";
        let mut fetched: Vec<String> = Vec::new();
        let out = inline_imports_core(css, &mut |u| {
            fetched.push(u.to_string());
            Some("X".to_string())
        });
        assert!(
            fetched.is_empty(),
            "@import in a string value must not fetch"
        );
        assert_eq!(out, css);
    }

    #[test]
    fn inline_imports_inlines_leading_import_ahead_of_rules() {
        // (c) A genuine leading `@import` is fetched once and its content spliced
        // in ahead of the sheet's own rules; the original at-rule is dropped.
        let css = "@import url(\"sub.css\");\n.a { color: red }";
        let mut fetched: Vec<String> = Vec::new();
        let out = inline_imports_core(css, &mut |u| {
            fetched.push(u.to_string());
            Some(".imported { color: blue }".to_string())
        });
        assert_eq!(fetched, vec!["sub.css".to_string()]);
        let imported_at = out.find(".imported").expect("imported content present");
        let rule_at = out.find(".a {").expect("own rule present");
        assert!(imported_at < rule_at, "import inlined ahead of the rules");
        assert!(!out.contains("@import"), "the at-rule itself is replaced");
    }

    #[test]
    fn inline_imports_ignores_stray_import_after_rules() {
        // (d) `@import` is only valid in the prologue; one appearing after an
        // ordinary rule is ignored (no fetch) and left as inert text.
        let css = ".a { color: red }\n@import url(\"late.css\");";
        let mut fetched: Vec<String> = Vec::new();
        let out = inline_imports_core(css, &mut |u| {
            fetched.push(u.to_string());
            Some("X".to_string())
        });
        assert!(fetched.is_empty(), "post-rule @import must not fetch");
        assert_eq!(out, css);
    }

    #[test]
    fn inline_imports_honors_charset_and_layer_prologue() {
        // `@charset` and `@layer` *statements* may precede a valid `@import`; a
        // `;` inside a string must not prematurely end a statement.
        let css = "@charset \"utf-8\";\n@layer base, theme;\n@import url(\"a;b.css\");\n.r {}";
        let mut fetched: Vec<String> = Vec::new();
        let out = inline_imports_core(css, &mut |u| {
            fetched.push(u.to_string());
            Some(".imported {}".to_string())
        });
        assert_eq!(
            fetched,
            vec!["a;b.css".to_string()],
            "url with inner ; intact"
        );
        assert!(out.contains(".imported {}"));
        // A `@layer { … }` block, by contrast, closes the prologue.
        let blocked = "@layer { .x {} }\n@import url(\"late.css\");";
        let mut fetched2: Vec<String> = Vec::new();
        inline_imports_core(blocked, &mut |u| {
            fetched2.push(u.to_string());
            Some("X".to_string())
        });
        assert!(
            fetched2.is_empty(),
            "@import after a @layer block is ignored"
        );
    }
}

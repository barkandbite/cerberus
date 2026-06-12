//! One storage environment for all data stores, with cookies **hard-sealed**
//! per instance.
//!
//! The central guarantee: an [`InstanceStore`] handle can only ever touch its
//! own partition. There is no API that takes a *foreign* `InstanceId`, so a
//! cross-instance cookie read is impossible *by construction*, not by a policy
//! check (see the `cross_instance_*` tests). On top of that, cookies are sorted
//! into groups — `Active` (available within their own instance) and
//! `Quarantined` (held encrypted in the vault, never attached to a request)
//! — driven by the quarantine state machine in [`QuarantineState`].
//!
//! Crypto primitives are injected via the `cerberus-crypto` traits; this crate
//! implements none. The on-disk format and randomized path arrive at M4; the
//! scaffold keeps blobs in memory.

use cerberus_crypto::{CryptoError, Secret};
use cerberus_types::{InstanceId, Origin};
use std::collections::HashMap;
use std::path::Path;

mod cookie;
mod disk;
mod policy;
mod rand;
mod vault;
pub use cookie::{parse_http_date, parse_set_cookie};
pub use disk::atomic_write;
pub use policy::{CookieDisposition, CookiePolicy, DEFAULT_TIMED_SECS};
pub use vault::{EncryptedVault, NoVault, Vault};

/// Random bytes from the OS CSPRNG (exported for salt/seed generation by the
/// composition root, so the entropy source has one implementation).
pub fn random_bytes(n: usize) -> Vec<u8> {
    rand::random_bytes(n)
}

/// A single cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

impl Cookie {
    /// A minimal cookie for a host (path `/`, lax, session-lived).
    pub fn host(
        name: impl Into<String>,
        value: impl Into<String>,
        domain: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
            path: "/".to_string(),
            expires: None,
            secure: true,
            http_only: false,
            same_site: SameSite::Lax,
        }
    }
}

/// `SameSite` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    None,
    Lax,
    Strict,
}

/// The user-facing classification of a cookie within its instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Always available within its own instance (e.g. first-party logins).
    Active,
    /// Held inactive in the vault until released.
    Quarantined,
}

/// The lifecycle state of a cookie/storage write (see the module docs and the
/// build prompt's quarantine state machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineState {
    /// A `Set-Cookie`/storage write was observed.
    Intercepted,
    /// Written to the encrypted vault; never attached to a request.
    Quarantined,
    /// Access would cross a site boundary; a consent event is owed.
    PendingConsent,
    /// Released, scoped to `(instance, first_party_context)`.
    Active,
    /// TTL reached.
    Expired,
    /// Revoked by the user.
    Revoked,
}

/// Errors from the storage subsystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// The vault is locked (no key derived yet).
    VaultLocked,
    /// The requested entry was not found.
    NotFound,
    /// A crypto operation failed.
    Crypto(CryptoError),
}

impl From<CryptoError> for StorageError {
    fn from(e: CryptoError) -> Self {
        StorageError::Crypto(e)
    }
}

/// An active cookie, scoped to the first-party site it was set under, with the
/// user's disposition (lifetime/persistence policy resolved at capture).
#[derive(Clone, Debug)]
struct ScopedCookie {
    fp_site: String,
    cookie: Cookie,
    disposition: CookieDisposition,
    /// Remaining sends for an `AllowOnce` cookie; `None` for all others.
    sends_left: Option<u8>,
}

/// A read-only view of one stored cookie, for the inspector / CLI. Exposes
/// everything (transparency); the UI masks the value until the user reveals it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookieView {
    pub fp_site: String,
    pub name: String,
    pub domain: String,
    pub path: String,
    pub value: String,
    pub expires: Option<u64>,
    pub disposition: CookieDisposition,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

/// Metadata for a quarantined cookie. The secret value lives in the vault.
#[derive(Clone, Debug)]
struct QuarantineEntry {
    fp_site: String,
    cookie: Cookie, // `value` is blanked; the real value is in the vault
    state: QuarantineState,
}

/// All data for one sealed instance. Never shared across instances.
#[derive(Default)]
struct Partition {
    active: Vec<ScopedCookie>,
    quarantined: Vec<QuarantineEntry>,
}

/// The single storage environment: every instance's partition plus the one
/// shared vault (itself partitioned by `InstanceId`).
pub struct StorageEnvironment {
    partitions: HashMap<InstanceId, Partition>,
    vault: Box<dyn Vault>,
    /// Unsaved cookie changes (the vault tracks its own dirtiness).
    dirty: bool,
}

impl StorageEnvironment {
    /// Create an environment backed by `vault`.
    pub fn new(vault: Box<dyn Vault>) -> Self {
        Self {
            partitions: HashMap::new(),
            vault,
            dirty: false,
        }
    }

    /// Create an environment with no usable vault (quarantine writes will fail
    /// until a real, unlocked vault is supplied). Handy for render-only paths.
    pub fn with_no_vault() -> Self {
        Self::new(Box::new(NoVault))
    }

    /// Obtain a handle scoped to a single instance.
    ///
    /// This is the *only* way to reach cookies, and the returned handle can
    /// never name another instance — hence cross-instance reads are impossible.
    pub fn instance(&mut self, id: InstanceId) -> InstanceStore<'_> {
        let partition = self.partitions.entry(id).or_default();
        InstanceStore {
            instance: id,
            partition,
            vault: self.vault.as_mut(),
            dirty: &mut self.dirty,
        }
    }

    /// Whether the vault is currently locked.
    pub fn vault_locked(&self) -> bool {
        self.vault.is_locked()
    }

    /// Unlock the vault with a passphrase.
    pub fn unlock_vault(&mut self, passphrase: &Secret) -> Result<(), StorageError> {
        self.vault.unlock(passphrase)
    }

    /// Load an environment from `dir` (layout: `instances/<id>/cookies.bin` +
    /// `vault.bin`), backed by `vault`. Missing files mean a fresh profile.
    /// Expired cookies are dropped at load. The vault's ciphertext blobs load
    /// while it is still locked.
    pub fn load(dir: &Path, mut vault: Box<dyn Vault>) -> std::io::Result<Self> {
        vault.load_from(&dir.join("vault.bin"))?;
        let mut partitions: HashMap<InstanceId, Partition> = HashMap::new();
        let instances_dir = dir.join("instances");
        if instances_dir.is_dir() {
            for entry in std::fs::read_dir(&instances_dir)? {
                let entry = entry?;
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(cerberus_types::InstanceId::from_hex)
                else {
                    continue; // not an instance dir; leave it alone
                };
                if let Some(bytes) = disk::read_if_exists(&entry.path().join("cookies.bin"))? {
                    partitions.insert(id, load_partition(&bytes)?);
                }
            }
        }
        Ok(Self {
            partitions,
            vault,
            dirty: false,
        })
    }

    /// Persist all unsaved state into `dir` (atomic per file). A no-op when
    /// nothing changed.
    pub fn save(&mut self, dir: &Path) -> std::io::Result<()> {
        if self.dirty {
            for (id, partition) in &self.partitions {
                let path = dir
                    .join("instances")
                    .join(id.to_string())
                    .join("cookies.bin");
                disk::atomic_write(&path, &save_partition(partition))?;
            }
            self.dirty = false;
        }
        if self.vault.is_dirty() {
            self.vault.save_to(&dir.join("vault.bin"))?;
            self.vault.clear_dirty();
        }
        Ok(())
    }

    /// Whether there are unsaved changes (cookies or vault blobs).
    pub fn needs_save(&self) -> bool {
        self.dirty || self.vault.is_dirty()
    }
}

/// Cookie record layouts (KIND_COOKIES). Active (9 fields):
/// `["A", fp_site, name, value, domain, path, expires, flags, disposition]` —
/// a legacy 8-field record (no disposition) loads as `Allow`. Quarantined
/// (value lives only in the vault): `["Q", state, fp_site, name, domain,
/// path, expires, flags]`. `expires` is 8B LE or empty for session cookies;
/// `flags` is bit0 secure, bit1 http_only, bits 2-3 SameSite.
fn save_partition(partition: &Partition) -> Vec<u8> {
    let mut w = disk::RecordWriter::new(disk::KIND_COOKIES);
    for sc in &partition.active {
        // Memory-only dispositions (Session/AllowOnce) and cookies without a
        // real future expiry never touch disk.
        if !sc.disposition.persistent() || sc.cookie.expires.is_none() {
            continue;
        }
        let expires = encode_expires(sc.cookie.expires);
        let disposition = sc.disposition.encode();
        w.record(&[
            b"A",
            sc.fp_site.as_bytes(),
            sc.cookie.name.as_bytes(),
            sc.cookie.value.as_bytes(),
            sc.cookie.domain.as_bytes(),
            sc.cookie.path.as_bytes(),
            &expires,
            &[encode_flags(&sc.cookie)],
            &disposition,
        ]);
    }
    for q in &partition.quarantined {
        let expires = encode_expires(q.cookie.expires);
        w.record(&[
            b"Q",
            &[encode_state(q.state)],
            q.fp_site.as_bytes(),
            q.cookie.name.as_bytes(),
            q.cookie.domain.as_bytes(),
            q.cookie.path.as_bytes(),
            &expires,
            &[encode_flags(&q.cookie)],
        ]);
    }
    w.finish()
}

fn load_partition(bytes: &[u8]) -> std::io::Result<Partition> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let now = cookie::unix_now();
    let mut partition = Partition::default();
    let mut reader = disk::RecordReader::new(bytes, disk::KIND_COOKIES)?;
    while let Some(fields) = reader.next_record()? {
        let text = |i: usize| -> std::io::Result<String> {
            String::from_utf8(fields.get(i).cloned().unwrap_or_default())
                .map_err(|_| bad("bad utf8 in cookie record"))
        };
        match fields.first().map(Vec::as_slice) {
            // 8-field = legacy (no disposition ⇒ Allow); 9-field carries it.
            Some(b"A") if fields.len() == 8 || fields.len() == 9 => {
                let expires = decode_expires(&fields[6]);
                // Sessions cookies (no expiry) do not survive a restart, and
                // expired ones are dropped at the door.
                let Some(t) = expires else { continue };
                if t <= now {
                    continue;
                }
                let (secure, http_only, same_site) = decode_flags(fields[7].first().copied());
                let disposition = fields
                    .get(8)
                    .map(|b| CookieDisposition::decode(b))
                    .unwrap_or(CookieDisposition::Allow);
                partition.active.push(ScopedCookie {
                    fp_site: text(1)?,
                    cookie: Cookie {
                        name: text(2)?,
                        value: text(3)?,
                        domain: text(4)?,
                        path: text(5)?,
                        expires,
                        secure,
                        http_only,
                        same_site,
                    },
                    disposition,
                    sends_left: None,
                });
            }
            Some(b"Q") if fields.len() == 8 => {
                let (secure, http_only, same_site) = decode_flags(fields[7].first().copied());
                partition.quarantined.push(QuarantineEntry {
                    fp_site: text(2)?,
                    cookie: Cookie {
                        name: text(3)?,
                        value: String::new(),
                        domain: text(4)?,
                        path: text(5)?,
                        expires: decode_expires(&fields[6]),
                        secure,
                        http_only,
                        same_site,
                    },
                    state: decode_state(fields[1].first().copied()),
                });
            }
            _ => return Err(bad("unknown cookie record")),
        }
    }
    Ok(partition)
}

fn encode_expires(expires: Option<u64>) -> Vec<u8> {
    expires
        .map(|t| t.to_le_bytes().to_vec())
        .unwrap_or_default()
}

fn decode_expires(bytes: &[u8]) -> Option<u64> {
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

fn encode_flags(c: &Cookie) -> u8 {
    let ss = match c.same_site {
        SameSite::None => 0u8,
        SameSite::Lax => 1,
        SameSite::Strict => 2,
    };
    (c.secure as u8) | ((c.http_only as u8) << 1) | (ss << 2)
}

fn decode_flags(byte: Option<u8>) -> (bool, bool, SameSite) {
    let b = byte.unwrap_or(0);
    let ss = match (b >> 2) & 0b11 {
        0 => SameSite::None,
        2 => SameSite::Strict,
        _ => SameSite::Lax,
    };
    (b & 1 != 0, b & 2 != 0, ss)
}

fn encode_state(s: QuarantineState) -> u8 {
    match s {
        QuarantineState::Intercepted => 0,
        QuarantineState::Quarantined => 1,
        QuarantineState::PendingConsent => 2,
        QuarantineState::Active => 3,
        QuarantineState::Expired => 4,
        QuarantineState::Revoked => 5,
    }
}

fn decode_state(b: Option<u8>) -> QuarantineState {
    match b.unwrap_or(1) {
        0 => QuarantineState::Intercepted,
        2 => QuarantineState::PendingConsent,
        3 => QuarantineState::Active,
        4 => QuarantineState::Expired,
        5 => QuarantineState::Revoked,
        _ => QuarantineState::Quarantined,
    }
}

/// A handle bound to exactly one instance's partition. All cookie access flows
/// through here, and no method accepts a foreign `InstanceId`.
pub struct InstanceStore<'a> {
    instance: InstanceId,
    partition: &'a mut Partition,
    vault: &'a mut dyn Vault,
    dirty: &'a mut bool,
}

impl InstanceStore<'_> {
    /// The instance this handle is bound to.
    pub fn instance_id(&self) -> InstanceId {
        self.instance
    }

    /// Classify and store a cookie under the default `Allow` disposition
    /// (honor the cookie's own lifetime). See [`set_cookie_with`].
    pub fn set_cookie(
        &mut self,
        first_party: &Origin,
        cookie: Cookie,
        group: Group,
    ) -> Result<QuarantineState, StorageError> {
        self.set_cookie_with(first_party, cookie, group, CookieDisposition::Allow)
    }

    /// Classify and store a cookie under an explicit disposition. `Active`
    /// lands in the partition scoped to `first_party`, with the disposition
    /// applied (`Block` drops it; `Timed` overrides the expiry;
    /// `Session`/`AllowOnce` are memory-only). `Quarantined` is encrypted into
    /// the vault and never attached until released.
    pub fn set_cookie_with(
        &mut self,
        first_party: &Origin,
        mut cookie: Cookie,
        group: Group,
        disposition: CookieDisposition,
    ) -> Result<QuarantineState, StorageError> {
        let fp_site = first_party.site();
        match group {
            Group::Active => {
                let sends_left = match disposition {
                    // Never store; treat as a no-op success.
                    CookieDisposition::Block => return Ok(QuarantineState::Revoked),
                    // The user's clock overrides the site's.
                    CookieDisposition::Timed(secs) => {
                        cookie.expires = Some(cookie::unix_now().saturating_add(secs));
                        None
                    }
                    CookieDisposition::AllowOnce => Some(1),
                    CookieDisposition::Session | CookieDisposition::Allow => None,
                };
                self.insert_active(fp_site, cookie, disposition, sends_left);
                *self.dirty = true;
                Ok(QuarantineState::Active)
            }
            Group::Quarantined => {
                let key = vault_key(&fp_site, &cookie.name);
                self.vault
                    .store(self.instance, &key, cookie.value.as_bytes())?;
                let mut meta = cookie;
                meta.value.clear();
                self.partition.quarantined.push(QuarantineEntry {
                    fp_site,
                    cookie: meta,
                    state: QuarantineState::Quarantined,
                });
                *self.dirty = true;
                Ok(QuarantineState::Quarantined)
            }
        }
    }

    /// The cookies to attach to a request for `request_origin` in the context of
    /// `first_party`. Only `Active` cookies in scope are returned — quarantined
    /// cookies are *never* included — and expired or `Secure`-over-http cookies
    /// are filtered out.
    pub fn cookies_for_request(
        &self,
        request_origin: &Origin,
        first_party: &Origin,
    ) -> Vec<Cookie> {
        let fp_site = first_party.site();
        let https = request_origin.scheme == "https";
        let now = cookie::unix_now();
        self.partition
            .active
            .iter()
            .filter(|sc| sc.fp_site == fp_site)
            .filter(|sc| domain_matches(&request_origin.host, &sc.cookie.domain))
            .filter(|sc| sc.cookie.expires.is_none_or(|t| t > now))
            .filter(|sc| !sc.cookie.secure || https)
            .filter(|sc| sc.sends_left != Some(0))
            .map(|sc| sc.cookie.clone())
            .collect()
    }

    /// Account for an `AllowOnce` send: after the cookies for this request have
    /// been attached, decrement each matching `AllowOnce` cookie and drop those
    /// that are now spent. Call exactly once per request that actually sent the
    /// header (the jar's `cookie_header`).
    pub fn consume_allow_once(&mut self, request_origin: &Origin, first_party: &Origin) {
        let fp_site = first_party.site();
        let host = &request_origin.host;
        let mut changed = false;
        for sc in &mut self.partition.active {
            if sc.fp_site == fp_site
                && domain_matches(host, &sc.cookie.domain)
                && matches!(sc.disposition, CookieDisposition::AllowOnce)
            {
                sc.sends_left = Some(sc.sends_left.unwrap_or(1).saturating_sub(1));
                changed = true;
            }
        }
        if changed {
            self.partition.active.retain(|sc| sc.sends_left != Some(0));
            *self.dirty = true;
        }
    }

    /// Release a quarantined cookie into the active set, scoped to
    /// `(instance, first_party)`.
    pub fn release_from_quarantine(
        &mut self,
        name: &str,
        first_party: &Origin,
    ) -> Result<(), StorageError> {
        let fp_site = first_party.site();
        let pos = self
            .partition
            .quarantined
            .iter()
            .position(|q| q.fp_site == fp_site && q.cookie.name == name)
            .ok_or(StorageError::NotFound)?;

        let key = vault_key(&fp_site, name);
        let value_bytes = self
            .vault
            .load(self.instance, &key)?
            .ok_or(StorageError::NotFound)?;
        let _ = self.vault.remove(self.instance, &key);

        let mut entry = self.partition.quarantined.remove(pos);
        entry.cookie.value = String::from_utf8_lossy(&value_bytes).into_owned();
        // A released cookie joins the active set under the default disposition;
        // the user can retune it from the inspector.
        self.insert_active(fp_site, entry.cookie, CookieDisposition::Allow, None);
        *self.dirty = true;
        Ok(())
    }

    /// The quarantine state of a named cookie under `first_party`, if present.
    pub fn quarantine_state(&self, name: &str, first_party: &Origin) -> Option<QuarantineState> {
        let fp_site = first_party.site();
        self.partition
            .quarantined
            .iter()
            .find(|q| q.fp_site == fp_site && q.cookie.name == name)
            .map(|q| q.state)
    }

    /// Metadata of cookies currently quarantined under `first_party` (values
    /// blank — the real values live only in the vault).
    pub fn quarantined_cookies(&self, first_party: &Origin) -> Vec<Cookie> {
        let fp_site = first_party.site();
        self.partition
            .quarantined
            .iter()
            .filter(|q| q.fp_site == fp_site)
            .map(|q| q.cookie.clone())
            .collect()
    }

    /// Names of cookies currently quarantined under `first_party`.
    pub fn quarantined_names(&self, first_party: &Origin) -> Vec<String> {
        let fp_site = first_party.site();
        self.partition
            .quarantined
            .iter()
            .filter(|q| q.fp_site == fp_site)
            .map(|q| q.cookie.name.clone())
            .collect()
    }

    fn insert_active(
        &mut self,
        fp_site: String,
        cookie: Cookie,
        disposition: CookieDisposition,
        sends_left: Option<u8>,
    ) {
        self.partition
            .active
            .retain(|sc| !(sc.fp_site == fp_site && sc.cookie.name == cookie.name));
        self.partition.active.push(ScopedCookie {
            fp_site,
            cookie,
            disposition,
            sends_left,
        });
    }

    /// Every active cookie in this instance (all sites), for the inspector/CLI.
    pub fn cookie_views(&self) -> Vec<CookieView> {
        self.partition
            .active
            .iter()
            .map(|sc| CookieView {
                fp_site: sc.fp_site.clone(),
                name: sc.cookie.name.clone(),
                domain: sc.cookie.domain.clone(),
                path: sc.cookie.path.clone(),
                value: sc.cookie.value.clone(),
                expires: sc.cookie.expires,
                disposition: sc.disposition,
                secure: sc.cookie.secure,
                http_only: sc.cookie.http_only,
                same_site: sc.cookie.same_site,
            })
            .collect()
    }

    /// Change a stored cookie's disposition (inspector). `Block` deletes it;
    /// `Timed` resets its expiry from now; `AllowOnce` refreshes its budget.
    /// Returns whether a matching cookie was found.
    pub fn set_disposition(
        &mut self,
        fp_site: &str,
        name: &str,
        disposition: CookieDisposition,
    ) -> bool {
        if matches!(disposition, CookieDisposition::Block) {
            return self.delete_cookie(fp_site, name);
        }
        let now = cookie::unix_now();
        let Some(sc) = self
            .partition
            .active
            .iter_mut()
            .find(|sc| sc.fp_site == fp_site && sc.cookie.name == name)
        else {
            return false;
        };
        sc.disposition = disposition;
        sc.sends_left = match disposition {
            CookieDisposition::AllowOnce => Some(1),
            _ => None,
        };
        if let CookieDisposition::Timed(secs) = disposition {
            sc.cookie.expires = Some(now.saturating_add(secs));
        }
        *self.dirty = true;
        true
    }

    /// Delete one active cookie. Returns whether it existed.
    pub fn delete_cookie(&mut self, fp_site: &str, name: &str) -> bool {
        let before = self.partition.active.len();
        self.partition
            .active
            .retain(|sc| !(sc.fp_site == fp_site && sc.cookie.name == name));
        let removed = self.partition.active.len() != before;
        if removed {
            *self.dirty = true;
        }
        removed
    }

    /// Delete every active cookie under one first-party site. Returns the count.
    pub fn clear_site(&mut self, fp_site: &str) -> usize {
        let before = self.partition.active.len();
        self.partition.active.retain(|sc| sc.fp_site != fp_site);
        let removed = before - self.partition.active.len();
        if removed > 0 {
            *self.dirty = true;
        }
        removed
    }
}

/// Vault key namespacing first-party site and cookie name. (Instance scoping is
/// applied by the vault itself.)
fn vault_key(fp_site: &str, name: &str) -> String {
    format!("{fp_site}|{name}")
}

/// Coarse domain match: exact host, a subdomain, or an empty (host-only)
/// cookie domain. Full cookie-domain rules arrive with the network stack.
fn domain_matches(host: &str, domain: &str) -> bool {
    if domain.is_empty() {
        return true;
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Re-exported so adapters/tests can name the crypto seam types alongside the
/// vault.
pub mod crypto {
    pub use cerberus_crypto::{Aead, CryptoError, Kdf, Key, Secret};
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_crypto::{Aead, CryptoError, Kdf, Key, Secret};

    // ---- Throwaway, test-only crypto. NEVER shipped. Not secure. ----

    struct TestKdf;
    impl Kdf for TestKdf {
        fn derive(
            &self,
            passphrase: &Secret,
            salt: &[u8],
            out_len: usize,
        ) -> Result<Key, CryptoError> {
            let mut out = Vec::with_capacity(out_len);
            let pp = passphrase.expose();
            for i in 0..out_len {
                let a = pp.get(i % pp.len().max(1)).copied().unwrap_or(0);
                let b = salt.get(i % salt.len().max(1)).copied().unwrap_or(0);
                out.push(a ^ b ^ (i as u8));
            }
            Ok(Key::from_bytes(out))
        }
    }

    struct TestAead;
    impl Aead for TestAead {
        fn key_len(&self) -> usize {
            32
        }
        fn nonce_len(&self) -> usize {
            12
        }
        fn seal(
            &self,
            key: &Key,
            nonce: &[u8],
            aad: &[u8],
            plaintext: &[u8],
        ) -> Result<Vec<u8>, CryptoError> {
            let mut out = xor_stream(key.expose(), nonce, plaintext);
            // Trivial "tag": fold of aad+plaintext. Not authentication.
            let tag = aad.iter().chain(plaintext).fold(0u8, |a, b| a ^ b);
            out.push(tag);
            Ok(out)
        }
        fn open(
            &self,
            key: &Key,
            nonce: &[u8],
            aad: &[u8],
            ciphertext: &[u8],
        ) -> Result<Vec<u8>, CryptoError> {
            let (tag, ct) = ciphertext.split_last().ok_or(CryptoError::OpenFailed)?;
            let pt = xor_stream(key.expose(), nonce, ct);
            let expect = aad.iter().chain(&pt).fold(0u8, |a, b| a ^ b);
            if expect != *tag {
                return Err(CryptoError::OpenFailed);
            }
            Ok(pt)
        }
    }

    fn xor_stream(key: &[u8], nonce: &[u8], data: &[u8]) -> Vec<u8> {
        data.iter()
            .enumerate()
            .map(|(i, b)| {
                let k = key.get(i % key.len().max(1)).copied().unwrap_or(0);
                let n = nonce.get(i % nonce.len().max(1)).copied().unwrap_or(0);
                b ^ k ^ n
            })
            .collect()
    }

    fn fp() -> Origin {
        Origin::new("https", "shop.example.com", None)
    }

    fn req() -> Origin {
        Origin::new("https", "shop.example.com", None)
    }

    fn instance_a() -> InstanceId {
        InstanceId::from_u64_pair(0, 0xA)
    }

    fn instance_b() -> InstanceId {
        InstanceId::from_u64_pair(0, 0xB)
    }

    #[test]
    fn cross_instance_cookie_is_unreadable_by_construction() {
        let mut env = StorageEnvironment::with_no_vault();
        env.instance(instance_a())
            .set_cookie(
                &fp(),
                Cookie::host("sid", "A-secret", "example.com"),
                Group::Active,
            )
            .unwrap();

        // Instance B sees nothing — there is no API to reach A's partition.
        let from_b = env
            .instance(instance_b())
            .cookies_for_request(&req(), &fp());
        assert!(from_b.is_empty());

        // Instance A sees its own cookie.
        let from_a = env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp());
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].value, "A-secret");
    }

    #[test]
    fn quarantined_cookie_is_never_sent_until_released() {
        let vault =
            EncryptedVault::new(Box::new(TestAead), Box::new(TestKdf), *b"saltsaltsaltsalt");
        let mut env = StorageEnvironment::new(Box::new(vault));
        env.unlock_vault(&Secret::from_passphrase("correct horse"))
            .unwrap();

        let state = env
            .instance(instance_a())
            .set_cookie(
                &fp(),
                Cookie::host("track", "xyz", "example.com"),
                Group::Quarantined,
            )
            .unwrap();
        assert_eq!(state, QuarantineState::Quarantined);
        assert_eq!(
            env.instance(instance_a()).quarantine_state("track", &fp()),
            Some(QuarantineState::Quarantined)
        );

        // Not attached to a request while quarantined.
        let attached = env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp());
        assert!(attached.iter().all(|c| c.name != "track"));
        assert_eq!(
            env.instance(instance_a()).quarantined_names(&fp()),
            vec!["track".to_string()]
        );

        // After release, it is attached and decrypts to the original value.
        env.instance(instance_a())
            .release_from_quarantine("track", &fp())
            .unwrap();
        let attached = env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp());
        let track = attached.iter().find(|c| c.name == "track").unwrap();
        assert_eq!(track.value, "xyz");
    }

    #[test]
    fn wrong_passphrase_fails_at_unlock_and_vault_stays_locked() {
        let vault =
            EncryptedVault::new(Box::new(TestAead), Box::new(TestKdf), *b"saltsaltsaltsalt");
        let mut env = StorageEnvironment::new(Box::new(vault));
        // First unlock seals the check sentinel.
        env.unlock_vault(&Secret::from_passphrase("correct horse"))
            .unwrap();
        // Re-lock by... there is no public lock on the environment; simulate a
        // restart by checking a *wrong* passphrase against the sentinel via a
        // fresh unlock attempt below.
        assert!(!env.vault_locked());

        let vault2 =
            EncryptedVault::new(Box::new(TestAead), Box::new(TestKdf), *b"saltsaltsaltsalt");
        let mut v: Box<dyn Vault> = Box::new(vault2);
        v.unlock(&Secret::from_passphrase("correct horse")).unwrap();
        v.lock();
        assert!(v.is_locked());
        // Wrong passphrase: the sentinel fails authentication; still locked.
        assert!(v.unlock(&Secret::from_passphrase("wrong pass!!")).is_err());
        assert!(v.is_locked());
        // Right passphrase unlocks.
        v.unlock(&Secret::from_passphrase("correct horse")).unwrap();
        assert!(!v.is_locked());
    }

    #[test]
    fn quarantine_requires_unlocked_vault() {
        let mut env = StorageEnvironment::with_no_vault();
        let err = env
            .instance(instance_a())
            .set_cookie(
                &fp(),
                Cookie::host("track", "xyz", "example.com"),
                Group::Quarantined,
            )
            .unwrap_err();
        assert_eq!(err, StorageError::VaultLocked);
    }

    fn fresh_vault() -> Box<dyn Vault> {
        Box::new(EncryptedVault::new(
            Box::new(TestAead),
            Box::new(TestKdf),
            *b"saltsaltsaltsalt",
        ))
    }

    fn persistent_cookie(name: &str, value: &str) -> Cookie {
        let mut c = Cookie::host(name, value, "example.com");
        c.expires = Some(crate::cookie::unix_now() + 3600);
        c
    }

    #[test]
    fn environment_round_trips_through_disk_with_quarantine_intact() {
        let dir = std::env::temp_dir().join(format!("cerb-env-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        // Session 1: unlock, store an active + a quarantined cookie, save.
        let mut env = StorageEnvironment::new(fresh_vault());
        env.unlock_vault(&Secret::from_passphrase("correct horse"))
            .unwrap();
        env.instance(instance_a())
            .set_cookie(&fp(), persistent_cookie("sid", "keep-me"), Group::Active)
            .unwrap();
        env.instance(instance_a())
            .set_cookie(
                &fp(),
                persistent_cookie("track", "vault-secret"),
                Group::Quarantined,
            )
            .unwrap();
        assert!(env.needs_save());
        env.save(&dir).unwrap();
        assert!(!env.needs_save());
        drop(env);

        // Session 2 (a fresh process): blobs load while locked; the metadata
        // is there, the value is not.
        let mut env = StorageEnvironment::load(&dir, fresh_vault()).unwrap();
        assert!(env.vault_locked());
        assert_eq!(
            env.instance(instance_a()).quarantined_names(&fp()),
            vec!["track".to_string()]
        );
        let active = env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp());
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].value, "keep-me");

        // Wrong passphrase: still locked (sentinel persisted).
        assert!(env.unlock_vault(&Secret::from_passphrase("wrong")).is_err());
        assert!(env.vault_locked());

        // Right passphrase: release decrypts the original value.
        env.unlock_vault(&Secret::from_passphrase("correct horse"))
            .unwrap();
        env.instance(instance_a())
            .release_from_quarantine("track", &fp())
            .unwrap();
        let active = env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp());
        let track = active.iter().find(|c| c.name == "track").unwrap();
        assert_eq!(track.value, "vault-secret");

        // Sealing survives the disk: instance B sees nothing.
        assert!(env
            .instance(instance_b())
            .cookies_for_request(&req(), &fp())
            .is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dispositions_drive_lifetime_and_persistence() {
        let dir = std::env::temp_dir().join(format!("cerb-disp-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let mut env = StorageEnvironment::with_no_vault();
        let inst = instance_a();
        // Allow with a real future expiry → persists.
        let mut allow = Cookie::host("keep", "1", "example.com");
        allow.expires = Some(crate::cookie::unix_now() + 3600);
        env.instance(inst)
            .set_cookie_with(&fp(), allow, Group::Active, CookieDisposition::Allow)
            .unwrap();
        // Session → memory-only.
        env.instance(inst)
            .set_cookie_with(
                &fp(),
                Cookie::host("sess", "1", "example.com"),
                Group::Active,
                CookieDisposition::Session,
            )
            .unwrap();
        // Timed → expiry overridden to now+secs regardless of the cookie.
        env.instance(inst)
            .set_cookie_with(
                &fp(),
                Cookie::host("timed", "1", "example.com"),
                Group::Active,
                CookieDisposition::Timed(120),
            )
            .unwrap();
        // Block → dropped at the door.
        env.instance(inst)
            .set_cookie_with(
                &fp(),
                Cookie::host("blocked", "1", "example.com"),
                Group::Active,
                CookieDisposition::Block,
            )
            .unwrap();

        let views = env.instance(inst).cookie_views();
        assert_eq!(views.len(), 3, "blocked never stored: {views:?}");
        let timed = views.iter().find(|v| v.name == "timed").unwrap();
        assert_eq!(timed.disposition, CookieDisposition::Timed(120));
        let now = crate::cookie::unix_now();
        assert!(timed.expires.unwrap() <= now + 120 && timed.expires.unwrap() > now);

        env.save(&dir).unwrap();
        // Reload: only Allow + Timed survive (Session is memory-only).
        let mut env = StorageEnvironment::load(&dir, Box::new(NoVault)).unwrap();
        let mut names: Vec<String> = env
            .instance(inst)
            .cookie_views()
            .into_iter()
            .map(|v| v.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["keep".to_string(), "timed".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allow_once_sends_exactly_once() {
        let mut env = StorageEnvironment::with_no_vault();
        let inst = instance_a();
        env.instance(inst)
            .set_cookie_with(
                &fp(),
                Cookie::host("one", "v", "example.com"),
                Group::Active,
                CookieDisposition::AllowOnce,
            )
            .unwrap();
        // First request: present.
        assert_eq!(
            env.instance(inst).cookies_for_request(&req(), &fp()).len(),
            1
        );
        env.instance(inst).consume_allow_once(&req(), &fp());
        // Second request: gone.
        assert!(env
            .instance(inst)
            .cookies_for_request(&req(), &fp())
            .is_empty());
    }

    #[test]
    fn inspector_set_disposition_and_delete() {
        let mut env = StorageEnvironment::with_no_vault();
        let inst = instance_a();
        env.instance(inst)
            .set_cookie(&fp(), Cookie::host("c", "v", "example.com"), Group::Active)
            .unwrap();
        // Retune to Timed.
        assert!(env.instance(inst).set_disposition(
            &fp().site(),
            "c",
            CookieDisposition::Timed(30)
        ));
        let v = env.instance(inst).cookie_views();
        assert_eq!(v[0].disposition, CookieDisposition::Timed(30));
        // Setting Block deletes it.
        assert!(env
            .instance(inst)
            .set_disposition(&fp().site(), "c", CookieDisposition::Block));
        assert!(env.instance(inst).cookie_views().is_empty());
        // Deleting a missing cookie returns false.
        assert!(!env.instance(inst).delete_cookie(&fp().site(), "nope"));
    }

    #[test]
    fn session_cookies_do_not_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("cerb-sess-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let mut env = StorageEnvironment::with_no_vault();
        env.instance(instance_a())
            .set_cookie(
                &fp(),
                Cookie::host("session-only", "gone", "example.com"),
                Group::Active,
            )
            .unwrap();
        env.save(&dir).unwrap();

        let mut env = StorageEnvironment::load(&dir, Box::new(NoVault)).unwrap();
        assert!(env
            .instance(instance_a())
            .cookies_for_request(&req(), &fp())
            .is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tampered_vault_blob_fails_to_open_after_reload() {
        let dir = std::env::temp_dir().join(format!("cerb-tamper-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let mut env = StorageEnvironment::new(fresh_vault());
        env.unlock_vault(&Secret::from_passphrase("correct horse"))
            .unwrap();
        env.instance(instance_a())
            .set_cookie(
                &fp(),
                persistent_cookie("track", "secret"),
                Group::Quarantined,
            )
            .unwrap();
        env.save(&dir).unwrap();

        // Flip one byte at the end of vault.bin (inside the last ciphertext).
        let vault_path = dir.join("vault.bin");
        let mut bytes = std::fs::read(&vault_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&vault_path, &bytes).unwrap();

        let mut env = StorageEnvironment::load(&dir, fresh_vault()).unwrap();
        // Either the sentinel or the blob was tampered with; in both cases the
        // data never comes back as silently-corrupt plaintext.
        let unlock = env.unlock_vault(&Secret::from_passphrase("correct horse"));
        if unlock.is_ok() {
            assert!(env
                .instance(instance_a())
                .release_from_quarantine("track", &fp())
                .is_err());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

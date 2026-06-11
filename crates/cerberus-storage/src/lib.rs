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

mod cookie;
mod vault;
pub use cookie::parse_set_cookie;
pub use vault::{EncryptedVault, NoVault, Vault};

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

/// An active cookie, scoped to the first-party site it was set under.
#[derive(Clone, Debug)]
struct ScopedCookie {
    fp_site: String,
    cookie: Cookie,
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
}

impl StorageEnvironment {
    /// Create an environment backed by `vault`.
    pub fn new(vault: Box<dyn Vault>) -> Self {
        Self {
            partitions: HashMap::new(),
            vault,
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
}

/// A handle bound to exactly one instance's partition. All cookie access flows
/// through here, and no method accepts a foreign `InstanceId`.
pub struct InstanceStore<'a> {
    instance: InstanceId,
    partition: &'a mut Partition,
    vault: &'a mut dyn Vault,
}

impl InstanceStore<'_> {
    /// The instance this handle is bound to.
    pub fn instance_id(&self) -> InstanceId {
        self.instance
    }

    /// Classify and store a cookie. `Active` lands in the partition scoped to
    /// `first_party`; `Quarantined` is encrypted into the vault and never
    /// attached to a request until released.
    pub fn set_cookie(
        &mut self,
        first_party: &Origin,
        cookie: Cookie,
        group: Group,
    ) -> Result<QuarantineState, StorageError> {
        let fp_site = first_party.site();
        match group {
            Group::Active => {
                self.insert_active(fp_site, cookie);
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
            .map(|sc| sc.cookie.clone())
            .collect()
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
        self.insert_active(fp_site, entry.cookie);
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

    fn insert_active(&mut self, fp_site: String, cookie: Cookie) {
        self.partition
            .active
            .retain(|sc| !(sc.fp_site == fp_site && sc.cookie.name == cookie.name));
        self.partition.active.push(ScopedCookie { fp_site, cookie });
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
}

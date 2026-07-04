//! The identity ("head") manager.
//!
//! Three switchable, isolated identities, used one at a time in the foreground.
//! Each head owns a sealed cookie partition (`InstanceId`) and its own farbling
//! seed, so the heads never correlate. Switching heads tears down the active
//! JS engine and (lazily) prepares a fresh one for the new head — this is how
//! Cerberus runs three identities while keeping **at most one engine live**,
//! which is the crux of the memory-first design (see PLAN.md).

use cerberus_farbling::{FarblingProvider, SeededFarbling};
use cerberus_js::{JsEngine, JsEngineFactory, JsError};
use cerberus_profile::{derive_profile, Profile, ProfileOverrides};
use cerberus_types::{HeadId, InstanceId, RealmId};

/// One identity: a sealed storage instance, a farbling seed, and a coherent
/// fingerprint profile.
pub struct Head {
    pub id: HeadId,
    pub instance: InstanceId,
    pub label: String,
    pub farbling: SeededFarbling,
    /// This head's internally-coherent fingerprint persona (navigator/screen/
    /// GPU/timezone/fonts), derived deterministically from the same seed as
    /// `farbling`. Its `profile_prologue()` is injected ahead of the farbling
    /// prologue so the DOM model and WebGL shims present one self-consistent
    /// identity per window — never a Windows UA over a Metal GPU, etc.
    pub profile: Profile,
    /// This identity's own egress proxy as a raw `host:port` string, if set.
    /// `None` means it uses whatever default the app configures (or direct).
    /// Parsed and enforced in the app/network layer so each mirror window can
    /// egress through its own proxy while sharing one engine.
    pub proxy: Option<String>,
}

impl Head {
    /// Construct a head. The `RealmId` for its base realm is derived from `id`.
    /// The fingerprint profile and farbling noise both derive from `seed`, so a
    /// head's persona and its per-head noise are coherent by construction.
    pub fn new(id: HeadId, instance: InstanceId, label: impl Into<String>, seed: u64) -> Self {
        Self {
            id,
            instance,
            label: label.into(),
            farbling: SeededFarbling::new(seed),
            profile: derive_profile(seed, &ProfileOverrides::default()),
            proxy: None,
        }
    }

    /// Set this head's egress proxy (raw `host:port`), returning `self` for
    /// chaining. `None` clears it (falls back to the app default / direct).
    pub fn with_proxy(mut self, proxy: Option<String>) -> Self {
        self.proxy = proxy;
        self
    }

    fn base_realm(&self) -> RealmId {
        // Derive the base realm id from the head id (same 128-bit value).
        RealmId(self.id.0)
    }
}

/// Errors from the identity manager.
#[derive(Debug)]
pub enum IdentityError {
    /// The head index is out of range.
    NoSuchHead(usize),
    /// A JS engine operation failed.
    Js(JsError),
}

impl From<JsError> for IdentityError {
    fn from(e: JsError) -> Self {
        IdentityError::Js(e)
    }
}

/// Owns the heads and the single live engine.
pub struct HeadManager {
    heads: Vec<Head>,
    active: usize,
    factory: Box<dyn JsEngineFactory>,
    engine: Option<Box<dyn JsEngine>>,
}

impl HeadManager {
    /// Create a manager over `heads` (must be non-empty). The engine is *not*
    /// instantiated yet — it is created lazily for the active head on first use.
    pub fn new(heads: Vec<Head>, factory: Box<dyn JsEngineFactory>) -> Self {
        assert!(!heads.is_empty(), "a head manager needs at least one head");
        Self {
            heads,
            active: 0,
            factory,
            engine: None,
        }
    }

    /// The active head.
    pub fn active(&self) -> &Head {
        &self.heads[self.active]
    }

    /// Index of the active head.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// All heads.
    pub fn heads(&self) -> &[Head] {
        &self.heads
    }

    /// Switch the active head. Tears down the live engine (dropping it frees its
    /// isolate); the new head's engine is instantiated lazily on next use.
    pub fn switch_to(&mut self, idx: usize) -> Result<(), IdentityError> {
        if idx >= self.heads.len() {
            return Err(IdentityError::NoSuchHead(idx));
        }
        if idx != self.active {
            self.engine = None; // teardown
            self.active = idx;
        }
        Ok(())
    }

    /// Number of JS engines currently live: always 0 or 1, never one-per-head.
    pub fn engines_live(&self) -> usize {
        usize::from(self.engine.is_some())
    }

    /// Borrow the active head's engine, instantiating it (and injecting the
    /// head's farbling prologue into its base realm) on first use.
    pub fn engine(&mut self) -> Result<&mut dyn JsEngine, IdentityError> {
        if self.engine.is_none() {
            let mut engine = self.factory.instantiate()?;
            let head = &self.heads[self.active];
            let realm = head.base_realm();
            engine.create_realm(realm)?;
            // Profile first (defines the persona the DOM model reads), then the
            // farbling prologue (per-head noise + the seed export the crypto
            // shim keys off). Order between the two is immaterial — the WebGL
            // shim reads the profile lazily and the crypto shim reads the seed
            // at DOM-prelude time — but this ordering matches their roles.
            engine.inject_prologue(realm, &head.profile.profile_prologue())?;
            engine.inject_prologue(realm, &head.farbling.js_prologue())?;
            self.engine = Some(engine);
        }
        Ok(self.engine.as_mut().expect("just set").as_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_js::NullJsEngineFactory;

    fn three_heads() -> Vec<Head> {
        vec![
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
            Head::new(
                HeadId::from_u64_pair(0, 3),
                InstanceId::from_u64_pair(0, 3),
                "throwaway",
                0x3333,
            ),
        ]
    }

    #[test]
    fn engine_is_lazy_and_at_most_one_lives() {
        let mut mgr = HeadManager::new(three_heads(), Box::new(NullJsEngineFactory));
        assert_eq!(mgr.engines_live(), 0, "engine must be lazy");

        // Using the active head instantiates exactly one engine.
        mgr.engine().unwrap();
        assert_eq!(mgr.engines_live(), 1);

        // Switching tears the engine down; never two at once.
        mgr.switch_to(1).unwrap();
        assert_eq!(mgr.engines_live(), 0);
        mgr.engine().unwrap();
        assert_eq!(mgr.engines_live(), 1);
        assert_eq!(mgr.active().label, "personal");
    }

    #[test]
    fn heads_have_distinct_instances_and_seeds() {
        let mgr = HeadManager::new(three_heads(), Box::new(NullJsEngineFactory));
        let seeds: Vec<u64> = mgr.heads().iter().map(|h| h.farbling.seed()).collect();
        assert_eq!(seeds, vec![0x1111, 0x2222, 0x3333]);
        // Distinct sealed partitions.
        assert_ne!(mgr.heads()[0].instance, mgr.heads()[1].instance);
    }

    #[test]
    fn each_head_carries_a_coherent_seed_derived_persona() {
        let heads = three_heads();
        for h in &heads {
            // Every head's profile is internally consistent (no Windows UA over
            // a Metal GPU, screen >= viewport, etc.) — the whole point of
            // driving the surface from one derived persona per window.
            assert!(
                h.profile.is_coherent(),
                "head {:?} has an incoherent persona",
                h.label
            );
            // The persona presents a real browser UA, not the honest token.
            assert!(
                h.profile.user_agent.starts_with("Mozilla/"),
                "expected a real-browser UA, got {:?}",
                h.profile.user_agent
            );
        }

        // Deterministic in the seed: reconstructing a head from the same seed
        // yields the same persona (so a head's identity is stable across runs).
        let again = Head::new(
            HeadId::from_u64_pair(0, 1),
            InstanceId::from_u64_pair(0, 1),
            "work",
            0x1111,
        );
        assert_eq!(again.profile.user_agent, heads[0].profile.user_agent);
    }
}

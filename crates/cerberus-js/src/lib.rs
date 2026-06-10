//! The JavaScript engine seam.
//!
//! `JsEngine` is the trait every engine adapter implements (V8 via `rusty_v8`
//! is the recommended first adapter — see ADR-0002 — with QuickJS as the leaner
//! swap-in later, no caller changes). `JsEngineFactory` exists so the identity
//! manager can instantiate exactly one engine for the *active* head and tear it
//! down on switch: dropping the `Box<dyn JsEngine>` is the teardown. This is how
//! we run three identities without paying for three engines (memory is #1).
//!
//! This crate ships only the traits plus a `NullJsEngine` so the scaffold links
//! without pulling in a megabyte-scale engine before ADR-0002 is ratified.

use cerberus_types::RealmId;
use std::collections::HashSet;
use std::fmt;

/// A minimal JS value at the FFI boundary. Real engines return far richer
/// handles; callers only ever see this neutral enum.
#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Undefined,
    Bool(bool),
    Number(f64),
    Str(String),
}

/// Errors from a JS engine.
#[derive(Clone, Debug)]
pub enum JsError {
    /// The engine could not be created.
    Instantiate(String),
    /// No such realm.
    NoSuchRealm(RealmId),
    /// Script threw or failed to compile.
    Eval(String),
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsError::Instantiate(m) => write!(f, "engine instantiate failed: {m}"),
            JsError::NoSuchRealm(r) => write!(f, "no such realm: {r}"),
            JsError::Eval(m) => write!(f, "eval failed: {m}"),
        }
    }
}

impl std::error::Error for JsError {}

/// A live JS engine instance. One instance corresponds to one active head.
///
/// Not `Send`: an engine (QuickJS today, V8 later) is bound to the thread that
/// created it — both are single-threaded VMs. The memory-first design keeps it
/// on the UI thread with the active head; moving JS off-thread would be a
/// channel-based handle (itself `Send`), not a `Send` engine.
pub trait JsEngine {
    /// A short engine name (e.g. `"v8"`, `"null"`).
    fn name(&self) -> &'static str;

    /// Create an isolated realm/context (one per tab).
    fn create_realm(&mut self, id: RealmId) -> Result<(), JsError>;

    /// Inject a prologue script (e.g. farbling shims) into a realm. Runs before
    /// any page script in that realm.
    fn inject_prologue(&mut self, id: RealmId, script: &str) -> Result<(), JsError>;

    /// Evaluate `source` in a realm.
    fn eval(&mut self, id: RealmId, source: &str) -> Result<JsValue, JsError>;

    /// Destroy a realm and free its resources.
    fn destroy_realm(&mut self, id: RealmId) -> Result<(), JsError>;

    /// Number of live realms (for diagnostics and the memory gate).
    fn realm_count(&self) -> usize;
}

/// Creates fresh engine instances. The identity manager holds one factory and
/// instantiates lazily for the active head only.
pub trait JsEngineFactory: Send + Sync {
    /// Instantiate a new engine instance.
    fn instantiate(&self) -> Result<Box<dyn JsEngine>, JsError>;
}

/// A no-op engine used by the scaffold: it tracks realms and prologues but runs
/// no JavaScript. Lets us exercise the full lifecycle (create/inject/eval/
/// destroy and instantiate/teardown) before V8 is wired.
#[derive(Debug, Default)]
pub struct NullJsEngine {
    realms: HashSet<RealmId>,
    prologues: usize,
}

impl NullJsEngine {
    /// Number of prologues injected so far (test/diagnostic helper).
    pub fn prologues_injected(&self) -> usize {
        self.prologues
    }
}

impl JsEngine for NullJsEngine {
    fn name(&self) -> &'static str {
        "null"
    }

    fn create_realm(&mut self, id: RealmId) -> Result<(), JsError> {
        self.realms.insert(id);
        Ok(())
    }

    fn inject_prologue(&mut self, id: RealmId, _script: &str) -> Result<(), JsError> {
        if !self.realms.contains(&id) {
            return Err(JsError::NoSuchRealm(id));
        }
        self.prologues += 1;
        Ok(())
    }

    fn eval(&mut self, id: RealmId, _source: &str) -> Result<JsValue, JsError> {
        if !self.realms.contains(&id) {
            return Err(JsError::NoSuchRealm(id));
        }
        Ok(JsValue::Undefined)
    }

    fn destroy_realm(&mut self, id: RealmId) -> Result<(), JsError> {
        self.realms.remove(&id);
        Ok(())
    }

    fn realm_count(&self) -> usize {
        self.realms.len()
    }
}

/// Factory for [`NullJsEngine`].
#[derive(Debug, Default)]
pub struct NullJsEngineFactory;

impl JsEngineFactory for NullJsEngineFactory {
    fn instantiate(&self) -> Result<Box<dyn JsEngine>, JsError> {
        Ok(Box::new(NullJsEngine::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_lifecycle_and_prologue() {
        let mut e = NullJsEngine::default();
        let r = RealmId::from_u64_pair(0, 7);
        assert!(e.inject_prologue(r, "x").is_err());
        e.create_realm(r).unwrap();
        e.inject_prologue(r, "/* farbling */").unwrap();
        assert_eq!(e.eval(r, "1+1").unwrap(), JsValue::Undefined);
        assert_eq!(e.realm_count(), 1);
        e.destroy_realm(r).unwrap();
        assert_eq!(e.realm_count(), 0);
    }
}

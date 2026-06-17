//! The mirror-group controller: records master intent, and reconciles followers
//! to it under the ≤1-live-engine invariant via lazy per-focus catch-up.
//!
//! Invariants this upholds (asserted by the tests):
//! - **At most one live realm.** Exactly the focused instance owns a realm;
//!   focusing another destroys the old realm before creating the new one, so
//!   [`live_realms`](MirrorGroup::live_realms) is never above 1.
//! - **Sessions stay sealed.** Every page load goes through the [`PageSource`]
//!   keyed by the instance's [`InstanceId`]; instances never share session
//!   state, only (elsewhere) immutable cached bytes.
//! - **Convergence or honest divergence.** A follower replays the log against
//!   its own DOM; an unresolvable target flags [`Divergence`] instead of
//!   guessing.

use cerberus_js::JsEngine;
use cerberus_js_dom::{
    dispatch_event, fire_load, install_page, run_event_loop, run_scripts, serialize_dom,
    set_node_value, EventLoopBudget, PageEnv, RebuiltDom,
};
use cerberus_types::{InstanceId, RealmId};

use cerberus_autofill::FillKind;
use cerberus_dom::{Document, NodeId};

use crate::action::{Action, ActionLog, Target};
use crate::instance::{Divergence, MirrorInstance};
use crate::resolve::{invert_id_map, resolve};
use crate::source::PageSource;
use crate::MirrorError;

/// Resolves what to autofill in a given instance's document. The app wires an
/// implementation mapping each identity to its profile (via `cerberus-autofill`),
/// so a single master [`Action::Fill`] fills every window with its **own**
/// credentials.
pub trait FillProvider {
    /// The `(NodeId, value)` pairs to set for `kind` in `instance`'s `doc`.
    fn fills(&self, instance: InstanceId, kind: FillKind, doc: &Document) -> Vec<(NodeId, String)>;
}

/// A set of mirrored windows of one site driven from a single master.
///
/// Construct with [`MirrorGroup::new`] (the first member is the master), drive
/// the master with [`act`](MirrorGroup::act), and surface a follower converged
/// to the master with [`focus`](MirrorGroup::focus).
pub struct MirrorGroup {
    engine: Box<dyn JsEngine>,
    source: Box<dyn PageSource>,
    log: ActionLog,
    instances: Vec<MirrorInstance>,
    master_idx: usize,
    focused_idx: usize,
    viewport: (u32, u32),
    user_agent: String,
    fill_provider: Option<Box<dyn FillProvider>>,
}

impl MirrorGroup {
    /// Build a group over `members` (`(identity, label)`, first is the master).
    ///
    /// The engine is shared and starts with no realm; the master's realm is
    /// created lazily on the first [`act`](MirrorGroup::act).
    pub fn new(
        engine: Box<dyn JsEngine>,
        source: Box<dyn PageSource>,
        members: Vec<(InstanceId, String)>,
        viewport: (u32, u32),
        user_agent: impl Into<String>,
    ) -> Result<Self, MirrorError> {
        if members.is_empty() {
            return Err(MirrorError::Empty);
        }
        let instances = members
            .into_iter()
            .map(|(id, label)| MirrorInstance::new(id, label))
            .collect();
        Ok(Self {
            engine,
            source,
            log: ActionLog::new(),
            instances,
            master_idx: 0,
            focused_idx: 0,
            viewport,
            user_agent: user_agent.into(),
            fill_provider: None,
        })
    }

    // --- accessors -------------------------------------------------------

    /// All windows, master first.
    pub fn instances(&self) -> &[MirrorInstance] {
        &self.instances
    }

    /// A window by index.
    pub fn instance(&self, idx: usize) -> Option<&MirrorInstance> {
        self.instances.get(idx)
    }

    /// The master window.
    pub fn master(&self) -> &MirrorInstance {
        &self.instances[self.master_idx]
    }

    /// The master's index (always 0 today).
    pub fn master_index(&self) -> usize {
        self.master_idx
    }

    /// The currently focused (live) window's index.
    pub fn focused_index(&self) -> usize {
        self.focused_idx
    }

    /// The shared action log.
    pub fn log(&self) -> &ActionLog {
        &self.log
    }

    /// The number of **live JS realms** — the prime-directive gauge. Never > 1.
    pub fn live_realms(&self) -> usize {
        self.engine.realm_count()
    }

    /// The number of instances marked live (a cross-check on `live_realms`).
    pub fn live_instances(&self) -> usize {
        self.instances.iter().filter(|i| i.live).count()
    }

    /// Release the resident DOM + node map of every **non-live** instance,
    /// keeping only its cursor, URL, and identity. Resident memory then stays
    /// ~one live document no matter how many profiles the group holds (the
    /// N-can-be-thousands case) — focusing a released instance re-materializes
    /// it via catch-up. Safe to call any time; the live instance is untouched.
    pub fn release_dormant(&mut self) {
        let focused = self.focused_idx;
        for (idx, instance) in self.instances.iter_mut().enumerate() {
            // Keep the focused window's snapshot even when it holds no realm — the
            // shell renders it from that snapshot (E2 leaves viewed-only windows
            // live-realm-free but resident).
            if !instance.live && idx != focused {
                instance.release();
            }
        }
    }

    /// Release one instance's resident DOM (no-op if it is the live one, which
    /// must keep its DOM to render and dispatch).
    pub fn release(&mut self, idx: usize) -> Result<(), MirrorError> {
        let instance = self
            .instances
            .get_mut(idx)
            .ok_or(MirrorError::NoSuchInstance(idx))?;
        if !instance.live {
            instance.release();
        }
        Ok(())
    }

    /// Install the autofill provider that resolves each instance's profile data
    /// for [`Action::Fill`].
    pub fn set_fill_provider(&mut self, provider: Box<dyn FillProvider>) {
        self.fill_provider = Some(provider);
    }

    // --- driving ---------------------------------------------------------

    /// Apply `action` to the master and record it for followers.
    ///
    /// Ensures the master is the live instance first (catching it up if focus
    /// had moved away), applies the action in the master's session, then appends
    /// it to the log.
    pub fn act(&mut self, action: Action) -> Result<(), MirrorError> {
        let master = self.master_idx;
        // Driving needs a populated realm, so always make the master truly live
        // (a viewing-only `focus` may have left it without one).
        self.ensure_live(master)?;
        self.apply_logged(master, &action)?;
        // Coalesce a run of keystrokes into one log entry: if this is an `Input`
        // to the same target as the trailing action, replace it instead of
        // appending. Each keystroke already carries the whole field value, so a
        // follower replaying the single coalesced entry converges identically —
        // and per-character typing does not grow the log (catch-up stays cheap).
        let coalesce = if let Action::Input { target, .. } = &action {
            matches!(
                self.log.actions().last(),
                Some(Action::Input { target: prev, .. }) if prev == target
            )
        } else {
            false
        };
        if coalesce {
            self.log.replace_last(action);
        } else {
            self.log.push(action);
        }
        self.instances[master].cursor = self.log.len();
        Ok(())
    }

    /// Apply one logged action to instance `idx`'s live realm — shared by the
    /// master (`act`) and follower catch-up.
    fn apply_logged(&mut self, idx: usize, action: &Action) -> Result<(), MirrorError> {
        match action {
            Action::Navigate(url) => {
                let url = url.clone();
                self.navigate_instance(idx, &url)
            }
            Action::Fill(kind) => self.apply_fill(idx, *kind),
            Action::Scroll { .. } => Ok(()), // no DOM effect; record only
            Action::Click(_) | Action::Input { .. } | Action::Submit(_) => {
                self.apply_interaction(idx, action)
            }
        }
    }

    /// Focus instance `idx` for **viewing**, converged to the master.
    ///
    /// A window is rendered from its serialized snapshot, not its realm, so an
    /// instance whose snapshot is already converged *and still resident* needs no
    /// work: drop any live realm and just mark it focused — no realm rebuild, no
    /// page reload (E2/ADR-0026). Otherwise fall back to a full
    /// [`ensure_live`](Self::ensure_live) catch-up. Driving the instance later
    /// (via [`act`](Self::act)) rebuilds its realm on demand.
    pub fn focus(&mut self, idx: usize) -> Result<(), MirrorError> {
        if idx >= self.instances.len() {
            return Err(MirrorError::NoSuchInstance(idx));
        }
        if self.instances[idx].live && self.focused_idx == idx {
            return Ok(());
        }
        // Fast path: a resident, converged snapshot is viewable as-is.
        if !self.instances[idx].live
            && !self.instances[idx].released
            && self.instances[idx].cursor == self.log.len()
        {
            if let Some(cur) = self.live_index() {
                let realm = self.realm_of(cur);
                let _ = self.engine.destroy_realm(realm);
                self.instances[cur].live = false;
            }
            self.focused_idx = idx;
            return Ok(());
        }
        self.ensure_live(idx)
    }

    /// Make instance `idx` the live, focused window, converged to the master.
    ///
    /// Tears down whatever realm is live (preserving the ≤1 invariant), then
    /// instantiates `idx`'s realm and fast-forwards it through the action log in
    /// its own session. Unlike [`focus`](Self::focus) this always leaves `idx`
    /// holding a populated realm — required before applying a new action to it.
    fn ensure_live(&mut self, idx: usize) -> Result<(), MirrorError> {
        if idx >= self.instances.len() {
            return Err(MirrorError::NoSuchInstance(idx));
        }
        if self.instances[idx].live && self.focused_idx == idx {
            return Ok(());
        }
        if let Some(cur) = self.live_index() {
            if cur != idx {
                let realm = self.realm_of(cur);
                let _ = self.engine.destroy_realm(realm);
                self.instances[cur].live = false;
            }
        }
        if !self.instances[idx].live {
            self.catch_up(idx)?;
        }
        self.focused_idx = idx;
        Ok(())
    }

    // --- internals -------------------------------------------------------

    /// Instantiate `idx`'s realm and replay the log from the last navigation
    /// forward, in `idx`'s own session, until it converges (or diverges).
    fn catch_up(&mut self, idx: usize) -> Result<(), MirrorError> {
        let realm = self.realm_of(idx);
        self.engine.create_realm(realm)?;
        self.instances[idx].live = true;
        self.instances[idx].released = false;
        self.instances[idx].diverged = None;

        let head = self.log.len();
        let nav = self.log.actions()[..head]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, a)| match a {
                Action::Navigate(url) => Some((i, url.clone())),
                _ => None,
            });
        if let Some((nav_pos, url)) = nav {
            self.navigate_instance(idx, &url)?;
            for i in (nav_pos + 1)..head {
                let action = self.log.actions()[i].clone();
                self.apply_logged(idx, &action)?;
                if self.instances[idx].diverged.is_some() {
                    break; // stop replaying once out of lockstep
                }
            }
        }
        self.instances[idx].cursor = head;
        Ok(())
    }

    /// Load `url` for instance `idx` and run its scripts into the live realm,
    /// reading the result back into the instance.
    fn navigate_instance(&mut self, idx: usize, url: &str) -> Result<(), MirrorError> {
        let realm = self.realm_of(idx);
        let id = self.instances[idx].id;
        let doc = self.source.load(id, url).map_err(MirrorError::Source)?;
        let env = self.env_for(url);

        let engine = &mut *self.engine;
        install_page(engine, realm, &doc, &env)?;
        run_scripts(engine, realm, doc.scripts())?;
        let _ = fire_load(engine, realm);
        let _ = run_event_loop(engine, realm, EventLoopBudget::default());
        let RebuiltDom { document, id_map } = serialize_dom(engine, realm)?;

        let inst = &mut self.instances[idx];
        inst.doc = document;
        inst.node_to_js = invert_id_map(&id_map);
        inst.url = Some(url.to_string());
        inst.diverged = None;
        Ok(())
    }

    /// Resolve an interaction's target in `idx`'s DOM and dispatch it into the
    /// live realm, reading the mutated DOM back. Unresolvable → divergence.
    fn apply_interaction(&mut self, idx: usize, action: &Action) -> Result<(), MirrorError> {
        let realm = self.realm_of(idx);
        let (event_type, value, target): (&str, Option<&str>, &Target) = match action {
            Action::Click(t) => ("click", None, t),
            Action::Input { target, text } => ("input", Some(text.as_str()), target),
            Action::Submit(t) => ("submit", None, t),
            Action::Navigate(_) | Action::Scroll { .. } | Action::Fill(_) => return Ok(()),
        };

        // Resolve target -> live JS-model id, releasing the borrow before any
        // mutation of self.
        let js_id = {
            let inst = &self.instances[idx];
            resolve(&inst.doc, target).and_then(|node| inst.node_to_js.get(&node).copied())
        };
        let js_id = match js_id {
            Some(j) => j,
            None => {
                self.instances[idx].diverged = Some(Divergence {
                    reason: "target did not resolve in this session".to_string(),
                    action: action.clone(),
                });
                return Ok(());
            }
        };

        let (dispatched, dom) = {
            let engine = &mut *self.engine;
            if let Some(v) = value {
                set_node_value(engine, realm, js_id, v)?;
            }
            let d = dispatch_event(engine, realm, js_id, event_type, "{}")?;
            (d.dispatched, d.dom)
        };

        let inst = &mut self.instances[idx];
        inst.doc = dom.document;
        inst.node_to_js = invert_id_map(&dom.id_map);
        if !dispatched {
            inst.diverged = Some(Divergence {
                reason: "target node was not present in the live realm".to_string(),
                action: action.clone(),
            });
        }
        Ok(())
    }

    /// Autofill instance `idx`'s form fields for `kind` from its **own** profile
    /// (via the [`FillProvider`]): set each detected field's value and fire
    /// `input`. Fill-only — no submit. No-op without a provider or matches.
    fn apply_fill(&mut self, idx: usize, kind: FillKind) -> Result<(), MirrorError> {
        let realm = self.realm_of(idx);
        let fills: Vec<(u64, String)> = {
            let Some(provider) = self.fill_provider.as_deref() else {
                return Ok(());
            };
            let inst = &self.instances[idx];
            provider
                .fills(inst.id, kind, &inst.doc)
                .into_iter()
                .filter_map(|(node, value)| inst.node_to_js.get(&node).map(|&js| (js, value)))
                .collect()
        };
        if fills.is_empty() {
            return Ok(());
        }
        let mut last_dom = None;
        {
            let engine = &mut *self.engine;
            for (js, value) in &fills {
                set_node_value(engine, realm, *js, value)?;
                last_dom = Some(dispatch_event(engine, realm, *js, "input", "{}")?.dom);
            }
        }
        if let Some(dom) = last_dom {
            let inst = &mut self.instances[idx];
            inst.doc = dom.document;
            inst.node_to_js = invert_id_map(&dom.id_map);
        }
        Ok(())
    }

    fn live_index(&self) -> Option<usize> {
        self.instances.iter().position(|i| i.live)
    }

    /// Each instance's realm id derives from its identity, so distinct sessions
    /// get distinct realms — though only one is ever live at a time.
    fn realm_of(&self, idx: usize) -> RealmId {
        RealmId(self.instances[idx].id.0)
    }

    fn env_for(&self, url: &str) -> PageEnv {
        PageEnv {
            url: url.to_string(),
            viewport: self.viewport,
            user_agent: self.user_agent.clone(),
        }
    }
}

//! The consent engine: detect cross-site (third-party) access, default to deny,
//! raise a consent event in headed mode, and consult a per-instance rule store.
//!
//! This is policy logic (ours), expressed behind the `ConsentPolicy` trait so
//! the UX/prompt layer and the rule persistence can be swapped independently.
//! The prompt UX itself and persistent rules are M5; the scaffold ships the
//! decision core and in-memory rules.

use cerberus_types::{InstanceId, Origin};

/// What to do with an attempted cookie/storage access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Permit the access.
    Allow,
    /// Block the access.
    Deny,
    /// Block for now and ask the user (headed mode).
    Prompt,
}

/// Raised when an access needs user confirmation (maps to `PENDING_CONSENT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentEvent {
    pub instance: InstanceId,
    pub request: Origin,
    pub first_party: Origin,
    pub reason: String,
}

/// The decision plus any event the UI must surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentOutcome {
    pub decision: Decision,
    pub event: Option<ConsentEvent>,
}

/// Evaluates whether a cross-context access is permitted.
pub trait ConsentPolicy: Send {
    /// Decide for an access to `request` while the top-level context is
    /// `first_party`, within `instance`.
    fn evaluate(
        &mut self,
        instance: InstanceId,
        request: &Origin,
        first_party: &Origin,
    ) -> ConsentOutcome;
}

/// A standing rule overriding the default for one (instance, site, site) triple.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    instance: InstanceId,
    request_site: String,
    first_party_site: String,
    allow: bool,
}

/// Default-deny policy: first-party is allowed; third-party is denied unless a
/// rule allows it, and raises a prompt event when headed.
#[derive(Default)]
pub struct DefaultDenyPolicy {
    headed: bool,
    rules: Vec<Rule>,
}

impl DefaultDenyPolicy {
    /// A new policy. `headed` enables prompt events (headless mode denies
    /// silently — see the headless non-goals in the threat model).
    pub fn new(headed: bool) -> Self {
        Self {
            headed,
            rules: Vec::new(),
        }
    }

    /// Add a standing allow/deny rule for a third-party site under a first-party
    /// site, scoped to an instance.
    pub fn add_rule(
        &mut self,
        instance: InstanceId,
        request: &Origin,
        first_party: &Origin,
        allow: bool,
    ) {
        self.rules.push(Rule {
            instance,
            request_site: request.site(),
            first_party_site: first_party.site(),
            allow,
        });
    }

    fn matching_rule(
        &self,
        instance: InstanceId,
        request: &Origin,
        first_party: &Origin,
    ) -> Option<bool> {
        let rs = request.site();
        let fps = first_party.site();
        self.rules
            .iter()
            .find(|r| r.instance == instance && r.request_site == rs && r.first_party_site == fps)
            .map(|r| r.allow)
    }
}

impl ConsentPolicy for DefaultDenyPolicy {
    fn evaluate(
        &mut self,
        instance: InstanceId,
        request: &Origin,
        first_party: &Origin,
    ) -> ConsentOutcome {
        // First-party access is always allowed.
        if !request.is_third_party_to(first_party) {
            return ConsentOutcome {
                decision: Decision::Allow,
                event: None,
            };
        }

        // A standing rule overrides the default.
        if let Some(allow) = self.matching_rule(instance, request, first_party) {
            return ConsentOutcome {
                decision: if allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                },
                event: None,
            };
        }

        // Default deny. Headed mode raises a prompt; headless denies silently.
        if self.headed {
            ConsentOutcome {
                decision: Decision::Prompt,
                event: Some(ConsentEvent {
                    instance,
                    request: request.clone(),
                    first_party: first_party.clone(),
                    reason: "third-party storage access (default deny)".to_string(),
                }),
            }
        } else {
            ConsentOutcome {
                decision: Decision::Deny,
                event: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> InstanceId {
        InstanceId::from_u64_pair(0, 1)
    }

    fn fp() -> Origin {
        Origin::new("https", "news.example.com", None)
    }

    fn third_party() -> Origin {
        Origin::new("https", "ads.tracker.net", None)
    }

    #[test]
    fn first_party_is_allowed() {
        let mut p = DefaultDenyPolicy::new(true);
        let same = Origin::new("https", "cdn.example.com", None);
        let out = p.evaluate(inst(), &same, &fp());
        assert_eq!(out.decision, Decision::Allow);
        assert!(out.event.is_none());
    }

    #[test]
    fn third_party_defaults_to_prompt_with_event_when_headed() {
        let mut p = DefaultDenyPolicy::new(true);
        let out = p.evaluate(inst(), &third_party(), &fp());
        assert_eq!(out.decision, Decision::Prompt);
        assert!(out.event.is_some());
    }

    #[test]
    fn third_party_denied_silently_when_headless() {
        let mut p = DefaultDenyPolicy::new(false);
        let out = p.evaluate(inst(), &third_party(), &fp());
        assert_eq!(out.decision, Decision::Deny);
        assert!(out.event.is_none());
    }

    #[test]
    fn rule_can_allow_a_third_party() {
        let mut p = DefaultDenyPolicy::new(true);
        p.add_rule(inst(), &third_party(), &fp(), true);
        let out = p.evaluate(inst(), &third_party(), &fp());
        assert_eq!(out.decision, Decision::Allow);
    }
}

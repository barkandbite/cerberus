//! The consent engine: detect cross-site (third-party) access, default to deny,
//! raise a consent event in headed mode, and consult a per-instance rule store.
//!
//! This is policy logic (ours), expressed behind the `ConsentPolicy` trait so
//! the UX/prompt layer and the rule persistence can be swapped independently.
//! The prompt UX itself and persistent rules are M5; the scaffold ships the
//! decision core and in-memory rules.

pub mod psl;

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
///
/// A global "stop bugging me" switch ([`set_allow_all`](Self::set_allow_all))
/// flips the default to allow-everything without prompts, while keeping the
/// privacy-first default off. Per-site exemptions
/// ([`toggle_exempt`](Self::toggle_exempt)) *invert* the global switch for one
/// first-party site, so you can allow everything except one site, or stay strict
/// everywhere except one site.
#[derive(Default)]
pub struct DefaultDenyPolicy {
    headed: bool,
    rules: Vec<Rule>,
    allow_all: bool,
    exempt_sites: Vec<String>,
}

impl DefaultDenyPolicy {
    /// A new policy. `headed` enables prompt events (headless mode denies
    /// silently — see the headless non-goals in the threat model).
    pub fn new(headed: bool) -> Self {
        Self {
            headed,
            rules: Vec::new(),
            allow_all: false,
            exempt_sites: Vec::new(),
        }
    }

    /// Set the global "allow all sites" switch (the "stop bugging me" mode).
    pub fn set_allow_all(&mut self, on: bool) {
        self.allow_all = on;
    }

    /// Whether the global allow-all switch is on.
    pub fn allow_all(&self) -> bool {
        self.allow_all
    }

    /// Toggle whether `site` (a first-party `scheme://eTLD+1` key) is exempt from
    /// the global policy; returns the new exempt state. An exempt site inverts the
    /// global switch: strict when allow-all is on, fully allowed when it is off.
    pub fn toggle_exempt(&mut self, site: &str) -> bool {
        if let Some(i) = self.exempt_sites.iter().position(|s| s == site) {
            self.exempt_sites.remove(i);
            false
        } else {
            self.exempt_sites.push(site.to_string());
            true
        }
    }

    /// Whether `site` is currently exempt from the global policy.
    pub fn is_exempt(&self, site: &str) -> bool {
        self.exempt_sites.iter().any(|s| s == site)
    }

    /// Whether third-party access is allowed by default for `first_party_site`:
    /// the global switch, inverted for exempt sites.
    fn site_allows_all(&self, first_party_site: &str) -> bool {
        self.allow_all ^ self.is_exempt(first_party_site)
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

    /// Serialize the standing rules as a human-auditable line format:
    /// `allow|deny <instance-hex> <first-party-site> <request-site>`.
    pub fn serialize_rules(&self) -> String {
        let mut out = String::from("cerberus-consent v1\n");
        if self.allow_all {
            out.push_str("mode all\n");
        }
        for site in &self.exempt_sites {
            out.push_str(&format!("exempt {site}\n"));
        }
        for r in &self.rules {
            out.push_str(&format!(
                "{} {} {} {}\n",
                if r.allow { "allow" } else { "deny" },
                r.instance,
                r.first_party_site,
                r.request_site,
            ));
        }
        out
    }

    /// Load rules previously written by [`serialize_rules`], replacing the
    /// current set. Unparseable lines are skipped (forward compatibility).
    pub fn load_rules(&mut self, text: &str) {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("cerberus-consent v1") {
            return;
        }
        self.rules.clear();
        self.allow_all = false;
        self.exempt_sites.clear();
        for line in lines {
            let mut parts = line.split_whitespace();
            let verb = match parts.next() {
                Some(v) => v,
                None => continue,
            };
            // Global switch + per-site exemptions (the "stop bugging me" state).
            if verb == "mode" {
                self.allow_all = parts.next() == Some("all");
                continue;
            }
            if verb == "exempt" {
                if let Some(site) = parts.next() {
                    self.exempt_sites.push(site.to_string());
                }
                continue;
            }
            let (Some(inst), Some(fp), Some(req)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let allow = match verb {
                "allow" => true,
                "deny" => false,
                _ => continue,
            };
            let Some(instance) = InstanceId::from_hex(inst) else {
                continue;
            };
            self.rules.push(Rule {
                instance,
                request_site: req.to_string(),
                first_party_site: fp.to_string(),
                allow,
            });
        }
    }

    /// Number of standing rules.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
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

        // A standing (per-third-party) rule overrides everything below.
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

        // "Stop bugging me": the global allow-all switch (inverted for exempt
        // sites) permits third-party access with no prompt.
        if self.site_allows_all(&first_party.site()) {
            return ConsentOutcome {
                decision: Decision::Allow,
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

    #[test]
    fn rules_round_trip_through_the_line_format() {
        let mut p = DefaultDenyPolicy::new(true);
        p.add_rule(inst(), &third_party(), &fp(), true);
        p.add_rule(
            inst(),
            &Origin::new("https", "cdn.widgets.example", None),
            &fp(),
            false,
        );
        let text = p.serialize_rules();

        let mut q = DefaultDenyPolicy::new(true);
        q.load_rules(&text);
        assert_eq!(q.rule_count(), 2);
        assert_eq!(
            q.evaluate(inst(), &third_party(), &fp()).decision,
            Decision::Allow
        );
        assert_eq!(
            q.evaluate(
                inst(),
                &Origin::new("https", "cdn.widgets.example", None),
                &fp()
            )
            .decision,
            Decision::Deny
        );
        // Rules are instance-scoped: another instance still prompts.
        let other = InstanceId::from_u64_pair(0, 99);
        assert_eq!(
            q.evaluate(other, &third_party(), &fp()).decision,
            Decision::Prompt
        );
    }

    #[test]
    fn allow_all_permits_third_party_without_an_event() {
        let mut p = DefaultDenyPolicy::new(true);
        p.set_allow_all(true);
        let out = p.evaluate(inst(), &third_party(), &fp());
        assert_eq!(out.decision, Decision::Allow);
        assert!(out.event.is_none(), "no prompt when allow-all is on");
    }

    #[test]
    fn exemption_inverts_the_global_switch() {
        let site = fp().site();
        // Strict default + exempt this site → that site is fully allowed.
        let mut p = DefaultDenyPolicy::new(true);
        assert!(p.toggle_exempt(&site));
        assert_eq!(
            p.evaluate(inst(), &third_party(), &fp()).decision,
            Decision::Allow
        );
        // Allow-all + exempt this site → that site goes back to strict (prompt).
        p.set_allow_all(true);
        assert_eq!(
            p.evaluate(inst(), &third_party(), &fp()).decision,
            Decision::Prompt
        );
        // A different site still follows the global allow-all.
        let other_fp = Origin::new("https", "other.example", None);
        assert_eq!(
            p.evaluate(inst(), &third_party(), &other_fp).decision,
            Decision::Allow
        );
        // Toggling off removes the exemption.
        assert!(!p.toggle_exempt(&site));
        assert!(!p.is_exempt(&site));
    }

    #[test]
    fn allow_all_and_exemptions_round_trip() {
        let mut p = DefaultDenyPolicy::new(true);
        p.set_allow_all(true);
        p.toggle_exempt(&fp().site());
        let text = p.serialize_rules();
        let mut q = DefaultDenyPolicy::new(true);
        q.load_rules(&text);
        assert!(q.allow_all());
        assert!(q.is_exempt(&fp().site()));
        // Exempt first-party under allow-all → strict (prompt) again.
        assert_eq!(
            q.evaluate(inst(), &third_party(), &fp()).decision,
            Decision::Prompt
        );
    }

    #[test]
    fn garbage_rule_files_load_to_empty_not_panic() {
        let mut p = DefaultDenyPolicy::new(true);
        p.load_rules("not a rules file");
        assert_eq!(p.rule_count(), 0);
        p.load_rules("cerberus-consent v1\nallow tooshort\nbogus line\n");
        assert_eq!(p.rule_count(), 0);
    }
}

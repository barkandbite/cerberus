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

/// Percent-escape whitespace (and `%` itself) in a rule-line field.
///
/// `Origin::site()` embeds the host as-is, and nothing upstream forbids
/// whitespace in a host (an opaque-scheme "host", e.g. from a `mailto:` or
/// `data:` URL, legitimately can contain spaces). `load_rules` splits each
/// line on whitespace, so an unescaped space in a site would shift the field
/// boundaries and corrupt the adjacent field. Escaping only whitespace/`%`
/// keeps the format human-auditable for the overwhelmingly common case of
/// ordinary hostnames, which round-trip completely unescaped.
///
/// Works byte-wise (not char-wise): every other byte, including any
/// multi-byte UTF-8 sequence, is copied through untouched so non-ASCII hosts
/// still round-trip.
fn escape_field(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'%' => out.extend_from_slice(b"%25"),
            b' ' => out.extend_from_slice(b"%20"),
            b'\t' => out.extend_from_slice(b"%09"),
            b'\n' => out.extend_from_slice(b"%0A"),
            b'\r' => out.extend_from_slice(b"%0D"),
            _ => out.push(b),
        }
    }
    // `s` was valid UTF-8 and every substitution above is pure ASCII, so the
    // result is valid UTF-8 too.
    String::from_utf8(out).expect("escaping preserves UTF-8 validity")
}

/// Reverse [`escape_field`]. Invalid or truncated `%XX` escapes are passed
/// through literally rather than erroring — this is a best-effort decode of
/// a file we ourselves wrote (forward-compatible with `escape_field` changes).
fn unescape_field(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A well-formed escape only ever substitutes single bytes for `%XX`
    // triples that decode ASCII whitespace/`%`, so this cannot introduce
    // invalid UTF-8 that wasn't already in `s`; fall back to a lossy
    // decode defensively rather than panic on a corrupted rule file.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
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

    /// Serialize the standing rules as a human-auditable line format:
    /// `allow|deny <instance-hex> <first-party-site> <request-site>`.
    pub fn serialize_rules(&self) -> String {
        let mut out = String::from("cerberus-consent v1\n");
        for r in &self.rules {
            out.push_str(&format!(
                "{} {} {} {}\n",
                if r.allow { "allow" } else { "deny" },
                r.instance,
                escape_field(&r.first_party_site),
                escape_field(&r.request_site),
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
        for line in lines {
            let mut parts = line.split_whitespace();
            let (Some(verb), Some(inst), Some(fp), Some(req)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
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
                request_site: unescape_field(req),
                first_party_site: unescape_field(fp),
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
    fn escape_field_round_trips_arbitrary_bytes() {
        for s in [
            "news.example.com",
            "foo bar",
            "a%b",
            "tab\ttab",
            "line\nbreak",
        ] {
            assert_eq!(unescape_field(&escape_field(s)), s);
        }
    }

    #[test]
    fn rule_with_a_whitespace_containing_site_round_trips_without_corrupting_fields() {
        // `Origin::site()` embeds the host as-is, and an opaque-scheme "host"
        // (e.g. from a `mailto:` URL) can legitimately contain a space — there
        // is no validation upstream that forbids it. Before escaping was
        // added, an unescaped space would have split into an extra
        // `split_whitespace()` token on load, shifting every field after it.
        let mut p = DefaultDenyPolicy::new(true);
        let malformed_first_party = Origin::new("mailto", "foo bar", None);
        p.add_rule(inst(), &third_party(), &malformed_first_party, true);
        // A second, well-formed rule sits right after it in the file; if the
        // first line's fields shifted, this one would be misparsed too.
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
            q.evaluate(inst(), &third_party(), &malformed_first_party)
                .decision,
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

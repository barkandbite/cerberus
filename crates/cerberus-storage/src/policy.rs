//! Per-cookie disposition and the user's cookie policy (M10).
//!
//! Disposition is *how long and whether* an accepted cookie lives — the
//! transparency/control layer on top of the consent gate (which decides
//! third-party visibility). The policy resolves a disposition for a cookie
//! from three tiers: a per-`(site, name)` override, a per-site default, then
//! the global default. It is pure data (no deps) and serializes to the
//! human-auditable `cookies.policy` file, mirroring the consent rule store.

use std::collections::HashMap;

/// What happens to an accepted cookie. `Block` is a policy outcome only — a
/// stored cookie is never `Block` (it would have been dropped at capture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieDisposition {
    /// Honor the cookie's own lifetime (Max-Age/Expires; session if neither).
    Allow,
    /// Usable this run only; never written to disk; gone on close.
    Session,
    /// Persisted, but the lifetime is the user's: expiry = now + this many
    /// seconds, overriding whatever the site asked for.
    Timed(u64),
    /// Never stored and never sent.
    Block,
    /// Sent on exactly the next matching request, then dropped.
    AllowOnce,
}

/// Default seconds for a freshly-cycled `Timed` chip (1 hour).
pub const DEFAULT_TIMED_SECS: u64 = 3600;

impl CookieDisposition {
    /// Whether a cookie with this disposition is written to disk. `Session`
    /// and `AllowOnce` are memory-only; `Block` is never stored.
    pub fn persistent(self) -> bool {
        matches!(self, Self::Allow | Self::Timed(_))
    }

    /// Binary tag for the on-disk cookie record (`Timed` carries 8 LE bytes).
    pub fn encode(self) -> Vec<u8> {
        match self {
            Self::Allow => vec![0],
            Self::Session => vec![1],
            Self::Timed(secs) => {
                let mut v = vec![2];
                v.extend_from_slice(&secs.to_le_bytes());
                v
            }
            Self::Block => vec![3],
            Self::AllowOnce => vec![4],
        }
    }

    /// Decode [`encode`]; unknown/empty falls back to `Allow` (legacy records).
    pub fn decode(bytes: &[u8]) -> Self {
        match bytes.first() {
            Some(1) => Self::Session,
            Some(2) => {
                let secs = bytes
                    .get(1..9)
                    .and_then(|s| s.try_into().ok())
                    .map(u64::from_le_bytes)
                    .unwrap_or(DEFAULT_TIMED_SECS);
                Self::Timed(secs)
            }
            Some(3) => Self::Block,
            Some(4) => Self::AllowOnce,
            _ => Self::Allow,
        }
    }

    /// Token for `cookies.policy` / the CLI (`allow`, `session`,
    /// `timed:3600`, `block`, `allow-once`).
    pub fn to_token(self) -> String {
        match self {
            Self::Allow => "allow".into(),
            Self::Session => "session".into(),
            Self::Timed(secs) => format!("timed:{secs}"),
            Self::Block => "block".into(),
            Self::AllowOnce => "allow-once".into(),
        }
    }

    /// Parse a token from [`to_token`].
    pub fn parse_token(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(secs) = s.strip_prefix("timed:") {
            return secs.parse().ok().map(Self::Timed);
        }
        match s {
            "allow" => Some(Self::Allow),
            "session" => Some(Self::Session),
            "block" => Some(Self::Block),
            "allow-once" => Some(Self::AllowOnce),
            _ => None,
        }
    }

    /// Next disposition for a UI chip click (wraps).
    pub fn cycle(self) -> Self {
        match self {
            Self::Allow => Self::Session,
            Self::Session => Self::Timed(DEFAULT_TIMED_SECS),
            Self::Timed(_) => Self::Block,
            Self::Block => Self::AllowOnce,
            Self::AllowOnce => Self::Allow,
        }
    }

    /// Short human label for the inspector chip.
    pub fn label(self) -> String {
        match self {
            Self::Timed(secs) => format!("Timed {secs}s"),
            other => other.to_token(),
        }
    }
}

/// The user's cookie policy: a global default plus per-site defaults and
/// per-`(site, name)` overrides. Resolution is override → site → global.
#[derive(Clone, Debug)]
pub struct CookiePolicy {
    global: CookieDisposition,
    site_default: HashMap<String, CookieDisposition>,
    overrides: HashMap<(String, String), CookieDisposition>,
    dirty: bool,
}

impl Default for CookiePolicy {
    fn default() -> Self {
        // Allow preserves today's behavior; the consent gate still governs
        // third-party. Users tighten the global default to Session/Block.
        Self {
            global: CookieDisposition::Allow,
            site_default: HashMap::new(),
            overrides: HashMap::new(),
            dirty: false,
        }
    }
}

impl CookiePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// The disposition for `(fp_site, name)`: override, else site default,
    /// else global.
    pub fn resolve(&self, fp_site: &str, name: &str) -> CookieDisposition {
        if let Some(d) = self.overrides.get(&(fp_site.to_string(), name.to_string())) {
            return *d;
        }
        if let Some(d) = self.site_default.get(fp_site) {
            return *d;
        }
        self.global
    }

    pub fn global(&self) -> CookieDisposition {
        self.global
    }

    pub fn set_global(&mut self, d: CookieDisposition) {
        self.global = d;
        self.dirty = true;
    }

    pub fn set_site_default(&mut self, fp_site: &str, d: CookieDisposition) {
        self.site_default.insert(fp_site.to_string(), d);
        self.dirty = true;
    }

    pub fn set_override(&mut self, fp_site: &str, name: &str, d: CookieDisposition) {
        self.overrides
            .insert((fp_site.to_string(), name.to_string()), d);
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Serialize to the `cookies.policy` line format.
    pub fn serialize(&self) -> String {
        let mut out = String::from("cerberus-cookies v1\n");
        out.push_str(&format!("global {}\n", self.global.to_token()));
        // Deterministic order for reproducible files.
        let mut sites: Vec<_> = self.site_default.iter().collect();
        sites.sort_by(|a, b| a.0.cmp(b.0));
        for (site, d) in sites {
            out.push_str(&format!("site {site} {}\n", d.to_token()));
        }
        let mut ovr: Vec<_> = self.overrides.iter().collect();
        ovr.sort_by(|a, b| a.0.cmp(b.0));
        for ((site, name), d) in ovr {
            out.push_str(&format!("cookie {site} {name} {}\n", d.to_token()));
        }
        out
    }

    /// Replace this policy from text written by [`serialize`]. Unparseable
    /// lines are skipped (forward compatibility).
    pub fn load(&mut self, text: &str) {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("cerberus-cookies v1") {
            return;
        }
        self.global = CookieDisposition::Allow;
        self.site_default.clear();
        self.overrides.clear();
        for line in lines {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("global") => {
                    if let Some(d) = parts.next().and_then(CookieDisposition::parse_token) {
                        self.global = d;
                    }
                }
                Some("site") => {
                    if let (Some(site), Some(tok)) = (parts.next(), parts.next()) {
                        if let Some(d) = CookieDisposition::parse_token(tok) {
                            self.site_default.insert(site.to_string(), d);
                        }
                    }
                }
                Some("cookie") => {
                    if let (Some(site), Some(name), Some(tok)) =
                        (parts.next(), parts.next(), parts.next())
                    {
                        if let Some(d) = CookieDisposition::parse_token(tok) {
                            self.overrides
                                .insert((site.to_string(), name.to_string()), d);
                        }
                    }
                }
                _ => {}
            }
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_tokens_round_trip() {
        for d in [
            CookieDisposition::Allow,
            CookieDisposition::Session,
            CookieDisposition::Timed(3600),
            CookieDisposition::Block,
            CookieDisposition::AllowOnce,
        ] {
            assert_eq!(CookieDisposition::parse_token(&d.to_token()), Some(d));
            assert_eq!(CookieDisposition::decode(&d.encode()), d);
        }
        assert!(CookieDisposition::Session.encode().len() == 1);
        assert!(CookieDisposition::Timed(9).encode().len() == 9);
    }

    #[test]
    fn persistence_predicate() {
        assert!(CookieDisposition::Allow.persistent());
        assert!(CookieDisposition::Timed(1).persistent());
        assert!(!CookieDisposition::Session.persistent());
        assert!(!CookieDisposition::AllowOnce.persistent());
        assert!(!CookieDisposition::Block.persistent());
    }

    #[test]
    fn cycle_visits_every_mode() {
        let mut d = CookieDisposition::Allow;
        let mut seen = vec![d];
        for _ in 0..4 {
            d = d.cycle();
            seen.push(d);
        }
        assert_eq!(d.cycle(), CookieDisposition::Allow); // wraps
        assert!(seen.contains(&CookieDisposition::Session));
        assert!(seen.contains(&CookieDisposition::Block));
        assert!(seen
            .iter()
            .any(|d| matches!(d, CookieDisposition::Timed(_))));
    }

    #[test]
    fn policy_resolution_tiers() {
        let mut p = CookiePolicy::new();
        assert_eq!(p.resolve("example.com", "sid"), CookieDisposition::Allow);
        p.set_global(CookieDisposition::Session);
        assert_eq!(p.resolve("example.com", "sid"), CookieDisposition::Session);
        p.set_site_default("example.com", CookieDisposition::Block);
        assert_eq!(p.resolve("example.com", "sid"), CookieDisposition::Block);
        assert_eq!(p.resolve("other.com", "sid"), CookieDisposition::Session);
        p.set_override("example.com", "sid", CookieDisposition::Timed(60));
        assert_eq!(
            p.resolve("example.com", "sid"),
            CookieDisposition::Timed(60)
        );
        assert_eq!(p.resolve("example.com", "other"), CookieDisposition::Block);
    }

    #[test]
    fn policy_round_trips_through_text() {
        let mut p = CookiePolicy::new();
        p.set_global(CookieDisposition::Session);
        p.set_site_default("news.example", CookieDisposition::Block);
        p.set_override("shop.example", "cart", CookieDisposition::Timed(7200));
        let text = p.serialize();

        let mut q = CookiePolicy::new();
        q.load(&text);
        assert_eq!(q.global(), CookieDisposition::Session);
        assert_eq!(q.resolve("news.example", "x"), CookieDisposition::Block);
        assert_eq!(
            q.resolve("shop.example", "cart"),
            CookieDisposition::Timed(7200)
        );
        assert!(!q.is_dirty());
    }

    #[test]
    fn garbage_policy_files_are_ignored() {
        let mut p = CookiePolicy::new();
        p.load("not a policy file");
        assert_eq!(p.global(), CookieDisposition::Allow);
    }
}

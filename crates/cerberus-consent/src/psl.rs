//! Registrable-domain (eTLD+1) extraction over a vendored Public Suffix List
//! snapshot.
//!
//! The data file `psl_snapshot.dat` is the comment-stripped
//! `public_suffix_list.dat` (VERSION 2026-06-10, ICANN **and** private
//! sections — browsers consult both for cookie-domain rejection, and the
//! private section is what separates e.g. two `github.io` user sites). The
//! list itself is MPL-2.0 data from <https://publicsuffix.org>; see NOTICE.
//!
//! Matching follows the PSL algorithm: among matching rules the longest wins,
//! exception rules (`!`) beat wildcard rules (`*.`), and when nothing matches
//! the prevailing rule is `*` (the bare TLD is the public suffix). The
//! registrable domain is the public suffix plus one label.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::OnceLock;

const PSL_SNAPSHOT: &str = include_str!("psl_snapshot.dat");

struct Rules {
    exact: HashSet<&'static str>,
    wildcard: HashSet<&'static str>,
    exception: HashSet<&'static str>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut exact = HashSet::new();
        let mut wildcard = HashSet::new();
        let mut exception = HashSet::new();
        for line in PSL_SNAPSHOT.lines() {
            let rule = line.trim();
            if rule.is_empty() {
                continue;
            }
            if let Some(rest) = rule.strip_prefix('!') {
                exception.insert(rest);
            } else if let Some(rest) = rule.strip_prefix("*.") {
                wildcard.insert(rest);
            } else {
                exact.insert(rule);
            }
        }
        Rules {
            exact,
            wildcard,
            exception,
        }
    })
}

/// The registrable domain (eTLD+1) for `host`, per the Public Suffix List.
///
/// Hosts that *are* a public suffix (or a bare TLD), IP literals, and
/// single-label hosts are returned unchanged — each is its own site bucket,
/// which is the safe behavior for cookie/consent partitioning.
pub fn registrable_domain(host: &str) -> String {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() || normalized.parse::<IpAddr>().is_ok() {
        return normalized;
    }
    // Bracketed IPv6 literals as they appear in URLs.
    if normalized.starts_with('[') {
        return normalized;
    }
    let labels: Vec<&str> = normalized.split('.').collect();
    if labels.len() <= 1 {
        return normalized;
    }

    let rules = rules();
    // Walk suffixes longest-first; the first match is the prevailing rule.
    let mut suffix_len = 1; // default rule "*": the bare TLD
    for start in 0..labels.len() {
        let candidate = labels[start..].join(".");
        if rules.exception.contains(candidate.as_str()) {
            // The exception rule itself is the registrable domain; its public
            // suffix is one label shorter.
            suffix_len = (labels.len() - start) - 1;
            break;
        }
        if rules.exact.contains(candidate.as_str()) {
            suffix_len = labels.len() - start;
            break;
        }
        // A wildcard rule `*.tail` matches `label.tail`.
        if labels.len() - start >= 2 {
            let tail = labels[start + 1..].join(".");
            if rules.wildcard.contains(tail.as_str()) {
                suffix_len = labels.len() - start;
                break;
            }
        }
    }

    if labels.len() <= suffix_len {
        // The host is itself a public suffix: its own bucket.
        return normalized;
    }
    labels[labels.len() - (suffix_len + 1)..].join(".")
}

/// True when `host` is itself a public suffix (a cookie `Domain` must never be
/// one — that would scope a cookie to e.g. all of `.co.uk`).
pub fn is_public_suffix(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    registrable_domain(&normalized) == normalized && {
        // `registrable_domain` also returns the host unchanged for single-label
        // unknown hosts and IPs; only call it a public suffix when a rule (or
        // the default TLD rule) actually says so.
        let labels: Vec<&str> = normalized.split('.').collect();
        let r = rules();
        labels.len() == 1 && normalized.parse::<IpAddr>().is_err()
            || r.exact.contains(normalized.as_str())
            || labels.len() >= 2 && r.wildcard.contains(labels[1..].join(".").as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_tld_takes_two_labels() {
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("www.example.com"), "example.com");
        assert_eq!(registrable_domain("a.b.c.example.com"), "example.com");
    }

    #[test]
    fn multi_label_public_suffixes_are_respected() {
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("shop.example.com.au"), "example.com.au");
        // The naive 2-label placeholder got both of these wrong (co.uk).
        assert_ne!(
            registrable_domain("foo.co.uk"),
            registrable_domain("bar.co.uk")
        );
    }

    #[test]
    fn private_section_separates_hosted_sites() {
        assert_eq!(registrable_domain("alice.github.io"), "alice.github.io");
        assert_eq!(registrable_domain("www.alice.github.io"), "alice.github.io");
        assert_ne!(
            registrable_domain("alice.github.io"),
            registrable_domain("bob.github.io")
        );
    }

    #[test]
    fn wildcard_rules_match_one_extra_label() {
        // `*.ck` is a wildcard rule: `bar.ck` is a public suffix.
        assert_eq!(registrable_domain("foo.bar.ck"), "foo.bar.ck");
    }

    #[test]
    fn exception_rules_punch_through_wildcards() {
        // `!www.ck` under `*.ck`: registrable domain is `www.ck` itself.
        assert_eq!(registrable_domain("www.ck"), "www.ck");
        assert_eq!(registrable_domain("sub.www.ck"), "www.ck");
        // `!city.kawasaki.jp` under `*.kawasaki.jp`.
        assert_eq!(
            registrable_domain("foo.city.kawasaki.jp"),
            "city.kawasaki.jp"
        );
        // Sibling without the exception stays under the wildcard.
        assert_eq!(
            registrable_domain("foo.bar.kawasaki.jp"),
            "foo.bar.kawasaki.jp"
        );
    }

    #[test]
    fn hosts_that_are_public_suffixes_are_their_own_bucket() {
        assert_eq!(registrable_domain("co.uk"), "co.uk");
        assert_eq!(registrable_domain("com"), "com");
        assert_eq!(registrable_domain("github.io"), "github.io");
    }

    #[test]
    fn ip_literals_and_unknown_tlds_are_sane() {
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain("[::1]"), "[::1]");
        assert_eq!(registrable_domain("localhost"), "localhost");
        // Unknown TLD: default `*` rule ⇒ last two labels.
        assert_eq!(registrable_domain("foo.bar.internal"), "bar.internal");
    }

    #[test]
    fn normalization_handles_case_and_trailing_dot() {
        assert_eq!(registrable_domain("WWW.Example.COM."), "example.com");
    }

    #[test]
    fn is_public_suffix_rejects_cookie_wide_domains() {
        assert!(is_public_suffix("com"));
        assert!(is_public_suffix("co.uk"));
        assert!(is_public_suffix("github.io"));
        assert!(!is_public_suffix("example.com"));
        assert!(!is_public_suffix("alice.github.io"));
    }
}

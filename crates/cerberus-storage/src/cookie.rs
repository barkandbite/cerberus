//! `Set-Cookie` parsing (RFC 6265 subset) — ours, no deps.
//!
//! Scope notes for v1: host-only cookies store the request host as their domain
//! and match subdomains like a `Domain` cookie would (the partition key —
//! instance × first-party site — already bounds the blast radius). Path
//! matching is recorded but not enforced on attach. `Max-Age` takes precedence
//! over `Expires`; both are honored (a cookie with neither is session-lived).

use crate::{Cookie, SameSite};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian), via Howard Hinnant's
/// `days_from_civil`. `m` is 1–12, `d` is 1–31.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn month_num(s: &str) -> Option<i64> {
    let m = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| s.eq_ignore_ascii_case(m))?;
    Some(m as i64 + 1)
}

/// Parse an HTTP-date (`Set-Cookie` `Expires`) to Unix seconds. Handles
/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), RFC-850 (2-digit year), and
/// asctime (`Sun Nov  6 08:49:37 1994`). Returns `None` on anything unparseable.
pub fn parse_http_date(s: &str) -> Option<u64> {
    // Tokenize on spaces, commas, and dashes (RFC-850 uses `06-Nov-94`).
    let toks: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',' || c == '-')
        .filter(|t| !t.is_empty())
        .collect();

    let mut day: Option<i64> = None;
    let mut month: Option<i64> = None;
    let mut year: Option<i64> = None;
    let mut hms: Option<(i64, i64, i64)> = None;

    for t in toks {
        if hms.is_none() && t.contains(':') {
            let parts: Vec<&str> = t.split(':').collect();
            if parts.len() == 3 {
                if let (Ok(h), Ok(mi), Ok(se)) = (
                    parts[0].parse::<i64>(),
                    parts[1].parse::<i64>(),
                    parts[2].parse::<i64>(),
                ) {
                    hms = Some((h, mi, se));
                    continue;
                }
            }
        }
        if month.is_none() {
            if let Some(m) = month_num(t) {
                month = Some(m);
                continue;
            }
        }
        if let Ok(n) = t.parse::<i64>() {
            if day.is_none() && (1..=31).contains(&n) {
                day = Some(n);
            } else if year.is_none() && n >= 70 {
                year = Some(n);
            }
        }
    }

    let (mut y, m, d) = (year?, month?, day?);
    let (h, mi, se) = hms?;
    // RFC-850 two-digit years: 70–99 ⇒ 1900s, 00–69 ⇒ 2000s.
    if y < 100 {
        y += if y >= 70 { 1900 } else { 2000 };
    }
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..=60).contains(&se) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let secs = days * 86_400 + h * 3_600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// True when `domain` is itself a public suffix (`com`, `co.uk`, `github.io`):
/// a cookie scoped that wide would leak across every site under the suffix.
///
/// Probes the installed PSL matcher: `domain` is a public suffix exactly when
/// a host one label below it is its own registrable domain.
fn is_public_suffix(domain: &str) -> bool {
    let probe = format!("x.{domain}");
    cerberus_types::registrable_domain(&probe) == probe
}

/// True when `host` is `domain` or a subdomain of it.
fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Parse one `Set-Cookie` header value observed on a response from
/// `request_host` (over https when `https`). Returns `None` for malformed or
/// rejected cookies (bad `Domain`, `Secure` from plaintext http, prefix-rule
/// violations) — rejection is silent, as in browsers.
pub fn parse_set_cookie(value: &str, request_host: &str, https: bool) -> Option<Cookie> {
    let mut parts = value.split(';');
    let nv = parts.next()?.trim();
    let eq = nv.find('=')?;
    let name = nv[..eq].trim();
    if name.is_empty() || name.contains(|c: char| c.is_control() || c == ' ') {
        return None;
    }
    let raw_value = nv[eq + 1..].trim();
    let cookie_value = raw_value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(raw_value);

    let request_host = request_host.to_ascii_lowercase();
    let mut domain: Option<String> = None;
    let mut path = "/".to_string();
    let mut max_age: Option<i64> = None;
    let mut expires_at: Option<u64> = None;
    let mut secure = false;
    let mut http_only = false;
    let mut same_site = SameSite::Lax;

    for attr in parts {
        let attr = attr.trim();
        let (k, v) = match attr.find('=') {
            Some(i) => (attr[..i].trim(), attr[i + 1..].trim()),
            None => (attr, ""),
        };
        match k.to_ascii_lowercase().as_str() {
            "domain" => {
                let d = v.trim_start_matches('.').to_ascii_lowercase();
                if d.is_empty() {
                    continue;
                }
                // The request host must live under the declared domain, and the
                // domain must not be a public suffix (cookie for all of .co.uk).
                if !host_matches_domain(&request_host, &d) {
                    return None;
                }
                if is_public_suffix(&d) && d != request_host {
                    return None;
                }
                domain = Some(d);
            }
            "path" => {
                if v.starts_with('/') {
                    path = v.to_string();
                }
            }
            "max-age" => {
                if let Ok(n) = v.parse::<i64>() {
                    max_age = Some(n);
                }
            }
            "expires" => {
                // `Max-Age` wins if both are present, so only record the date.
                expires_at = parse_http_date(v);
            }
            "secure" => secure = true,
            "httponly" => http_only = true,
            "samesite" => {
                same_site = match v.to_ascii_lowercase().as_str() {
                    "strict" => SameSite::Strict,
                    "none" => SameSite::None,
                    _ => SameSite::Lax,
                };
            }
            // Unknown attributes are ignored; a cookie with neither Max-Age
            // nor Expires is session-lived.
            _ => {}
        }
    }

    // A Secure cookie cannot be set over plaintext http.
    if secure && !https {
        return None;
    }
    // SameSite=None requires Secure; downgrade to Lax otherwise.
    if same_site == SameSite::None && !secure {
        same_site = SameSite::Lax;
    }
    // Cookie-prefix rules.
    if name.starts_with("__Secure-") && !secure {
        return None;
    }
    if name.starts_with("__Host-") && (!secure || domain.is_some() || path != "/") {
        return None;
    }

    // `Max-Age` takes precedence over `Expires`; neither ⇒ session cookie.
    let expires = match max_age {
        Some(n) if n <= 0 => Some(0), // already expired: an explicit delete
        Some(n) => Some(unix_now().saturating_add(n as u64)),
        None => expires_at,
    };

    Some(Cookie {
        name: name.to_string(),
        value: cookie_value.to_string(),
        domain: domain.unwrap_or(request_host),
        path,
        expires,
        secure,
        http_only,
        same_site,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_session_cookie() {
        let c = parse_set_cookie("sid=abc123", "shop.example.com", true).unwrap();
        assert_eq!(c.name, "sid");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.domain, "shop.example.com");
        assert_eq!(c.path, "/");
        assert_eq!(c.expires, None);
        assert!(!c.secure);
        assert_eq!(c.same_site, SameSite::Lax);
    }

    #[test]
    fn parses_attributes() {
        let c = parse_set_cookie(
            "sid=\"v\"; Domain=.example.com; Path=/app; Max-Age=3600; Secure; HttpOnly; SameSite=Strict",
            "shop.example.com",
            true,
        )
        .unwrap();
        assert_eq!(c.value, "v");
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/app");
        assert!(c.expires.unwrap() > unix_now());
        assert!(c.secure && c.http_only);
        assert_eq!(c.same_site, SameSite::Strict);
    }

    #[test]
    fn parses_http_dates_in_three_formats() {
        // The canonical epoch test vector: 784111777.
        let imf = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(imf, 784_111_777);
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
        assert!(parse_http_date("not a date").is_none());
    }

    #[test]
    fn expires_sets_lifetime_when_no_max_age() {
        let future = "Tue, 19 Jan 2038 03:14:07 GMT";
        let c = parse_set_cookie(&format!("a=1; Expires={future}"), "example.com", true).unwrap();
        assert_eq!(c.expires, Some(2_147_483_647));
    }

    #[test]
    fn max_age_takes_precedence_over_expires() {
        let c = parse_set_cookie(
            "a=1; Max-Age=60; Expires=Sun, 06 Nov 1994 08:49:37 GMT",
            "example.com",
            true,
        )
        .unwrap();
        // Max-Age wins: a future time, not the 1994 Expires.
        assert!(c.expires.unwrap() > unix_now());
    }

    #[test]
    fn rejects_domain_not_covering_request_host() {
        assert!(parse_set_cookie("a=1; Domain=other.com", "shop.example.com", true).is_none());
        // Sibling subdomain is also not covered.
        assert!(
            parse_set_cookie("a=1; Domain=cdn.example.com", "shop.example.com", true).is_none()
        );
    }

    #[test]
    fn rejects_public_suffix_domain() {
        // "com" is a public suffix under both the PSL and the fallback matcher.
        assert!(parse_set_cookie("a=1; Domain=com", "shop.example.com", true).is_none());
    }

    #[test]
    fn rejects_secure_cookie_over_plaintext_http() {
        assert!(parse_set_cookie("a=1; Secure", "example.com", false).is_none());
        assert!(parse_set_cookie("a=1", "example.com", false).is_some());
    }

    #[test]
    fn samesite_none_without_secure_downgrades_to_lax() {
        let c = parse_set_cookie("a=1; SameSite=None", "example.com", true).unwrap();
        assert_eq!(c.same_site, SameSite::Lax);
        let c = parse_set_cookie("a=1; SameSite=None; Secure", "example.com", true).unwrap();
        assert_eq!(c.same_site, SameSite::None);
    }

    #[test]
    fn enforces_cookie_prefixes() {
        assert!(parse_set_cookie("__Secure-a=1", "example.com", true).is_none());
        assert!(parse_set_cookie("__Secure-a=1; Secure", "example.com", true).is_some());
        assert!(parse_set_cookie(
            "__Host-a=1; Secure; Domain=example.com",
            "example.com",
            true
        )
        .is_none());
        assert!(parse_set_cookie("__Host-a=1; Secure", "example.com", true).is_some());
    }

    #[test]
    fn nonpositive_max_age_expires_immediately() {
        let c = parse_set_cookie("a=1; Max-Age=0", "example.com", true).unwrap();
        assert_eq!(c.expires, Some(0));
        let c = parse_set_cookie("a=1; Max-Age=-5", "example.com", true).unwrap();
        assert_eq!(c.expires, Some(0));
    }

    #[test]
    fn malformed_lines_are_rejected() {
        assert!(parse_set_cookie("no-equals-sign", "example.com", true).is_none());
        assert!(parse_set_cookie("=value-without-name", "example.com", true).is_none());
        assert!(parse_set_cookie("bad name=v", "example.com", true).is_none());
    }
}

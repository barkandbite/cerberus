//! Minimal URL parsing, bootstrapped with no foreign dependencies.
//!
//! This is intentionally small — enough for the M0 scaffold and the built-in
//! `cerberus:` pages. Full WHATWG URL handling remains ours to build out behind
//! this same API (no foreign URL type ever leaks to callers).

use cerberus_types::Origin;
use std::fmt;

/// A parsed URL. For opaque schemes (e.g. `cerberus:home`) `host` is empty and
/// the remainder is kept in `opaque`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub opaque: Option<String>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

/// Errors that can arise while parsing a URL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UrlError {
    /// The input was empty.
    Empty,
    /// No `scheme:` prefix was found.
    MissingScheme,
    /// The port component was not a valid `u16`.
    BadPort,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlError::Empty => write!(f, "empty URL"),
            UrlError::MissingScheme => write!(f, "URL has no scheme"),
            UrlError::BadPort => write!(f, "URL has an invalid port"),
        }
    }
}

impl std::error::Error for UrlError {}

impl Url {
    /// The origin (scheme, host, port), if this URL has an authority.
    pub fn origin(&self) -> Option<Origin> {
        if self.host.is_empty() {
            None
        } else {
            Some(Origin::new(
                self.scheme.clone(),
                self.host.clone(),
                self.port,
            ))
        }
    }

    /// True for the internal `cerberus:` scheme (built-in pages).
    pub fn is_builtin(&self) -> bool {
        self.scheme == "cerberus"
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.opaque {
            Some(opaque) => write!(f, "{}:{}", self.scheme, opaque)?,
            None => {
                write!(f, "{}://{}", self.scheme, self.host)?;
                if let Some(port) = self.port {
                    write!(f, ":{port}")?;
                }
                write!(f, "{}", self.path)?;
            }
        }
        if let Some(query) = &self.query {
            write!(f, "?{query}")?;
        }
        if let Some(fragment) = &self.fragment {
            write!(f, "#{fragment}")?;
        }
        Ok(())
    }
}

/// Parse a URL string.
pub fn parse(input: &str) -> Result<Url, UrlError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(UrlError::Empty);
    }

    let (scheme, rest) = input.split_once(':').ok_or(UrlError::MissingScheme)?;
    if scheme.is_empty() {
        return Err(UrlError::MissingScheme);
    }
    let scheme = scheme.to_ascii_lowercase();

    if let Some(after) = rest.strip_prefix("//") {
        parse_authority_form(scheme, after)
    } else {
        // Opaque form, e.g. `cerberus:home` or `mailto:foo`.
        let (before_frag, fragment) = split_off(rest, '#');
        let (opaque, query) = split_off(before_frag, '?');
        Ok(Url {
            scheme,
            host: String::new(),
            port: None,
            opaque: Some(opaque.to_string()),
            path: String::new(),
            query: query.map(str::to_string),
            fragment: fragment.map(str::to_string),
        })
    }
}

/// Resolve a possibly-relative reference (a link `href` or redirect `Location`)
/// against `base`. Handles absolute, protocol-relative, root-relative,
/// fragment-only, and path-relative references.
pub fn join(base: &Url, reference: &str) -> Result<Url, UrlError> {
    let r = reference.trim();
    if r.is_empty() {
        return Ok(base.clone());
    }
    if let Some(frag) = r.strip_prefix('#') {
        let mut u = base.clone();
        u.fragment = Some(frag.to_string());
        return Ok(u);
    }

    let absolute = if has_scheme(r) {
        r.to_string()
    } else if let Some(rest) = r.strip_prefix("//") {
        format!("{}://{}", base.scheme, rest)
    } else if r.starts_with('/') {
        format!("{}://{}{}", base.scheme, authority_of(base), r)
    } else {
        let dir = match base.path.rfind('/') {
            Some(i) => &base.path[..=i],
            None => "/",
        };
        format!("{}://{}{}{}", base.scheme, authority_of(base), dir, r)
    };
    parse(&absolute)
}

/// Whether `s` begins with a URL scheme (e.g. `https:`, `mailto:`).
fn has_scheme(s: &str) -> bool {
    match s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')) {
        Some(i) => i > 0 && s.as_bytes()[i] == b':' && s.as_bytes()[0].is_ascii_alphabetic(),
        None => false,
    }
}

fn authority_of(url: &Url) -> String {
    match url.port {
        Some(p) => format!("{}:{}", url.host, p),
        None => url.host.clone(),
    }
}

fn parse_authority_form(scheme: String, after: &str) -> Result<Url, UrlError> {
    // Split authority from the path/query/fragment tail.
    let (authority, tail) = match after.find(['/', '?', '#']) {
        Some(idx) => (&after[..idx], &after[idx..]),
        None => (after, ""),
    };

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() => {
            let port = p.parse::<u16>().map_err(|_| UrlError::BadPort)?;
            (h.to_string(), Some(port))
        }
        _ => (authority.to_string(), None),
    };

    let (before_frag, fragment) = split_off(tail, '#');
    let (path, query) = split_off(before_frag, '?');
    let path = if path.is_empty() { "/" } else { path };

    Ok(Url {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        opaque: None,
        path: path.to_string(),
        query: query.map(str::to_string),
        fragment: fragment.map(str::to_string),
    })
}

/// Split `s` at the first `sep`, returning the head and the optional tail
/// (without the separator).
fn split_off(s: &str, sep: char) -> (&str, Option<&str>) {
    match s.split_once(sep) {
        Some((head, tail)) => (head, Some(tail)),
        None => (s, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_https_url() {
        let u = parse("https://Example.com:8443/a/b?x=1#frag").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("x=1"));
        assert_eq!(u.fragment.as_deref(), Some("frag"));
        assert_eq!(u.origin().unwrap().host, "example.com");
    }

    #[test]
    fn parses_builtin_opaque_url() {
        let u = parse("cerberus:home").unwrap();
        assert!(u.is_builtin());
        assert_eq!(u.opaque.as_deref(), Some("home"));
        assert!(u.origin().is_none());
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse(""), Err(UrlError::Empty));
        assert_eq!(parse("no-scheme"), Err(UrlError::MissingScheme));
        assert_eq!(parse("http://h:notaport/"), Err(UrlError::BadPort));
    }

    #[test]
    fn joins_relative_references() {
        let base = parse("https://example.com/dir/page?q=1").unwrap();
        assert_eq!(
            join(&base, "https://other.test/x").unwrap().host,
            "other.test"
        );
        assert_eq!(join(&base, "//cdn.test/a").unwrap().host, "cdn.test");
        assert_eq!(join(&base, "/root").unwrap().path, "/root");
        assert_eq!(join(&base, "sibling").unwrap().path, "/dir/sibling");
        // Fragment-only stays on the same resource.
        let f = join(&base, "#sec").unwrap();
        assert_eq!(f.path, "/dir/page");
        assert_eq!(f.fragment.as_deref(), Some("sec"));
    }
}

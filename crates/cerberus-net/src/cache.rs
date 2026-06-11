//! A small HTTP response cache, **partitioned by `InstanceId`**.
//!
//! A cache shared across identities would be a cross-site/cross-identity tracking
//! vector (cache-timing), so entries are sealed per instance just like cookies.
//! Conservative policy: only `200` responses with an explicit `Cache-Control:
//! max-age` are stored, and never when `no-store`/`no-cache` is present.

use crate::HttpResponse;
use cerberus_types::InstanceId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Entry {
    response: HttpResponse,
    expires: Instant,
}

/// An in-memory, per-instance response cache. (On-disk caching is later work.)
#[derive(Default)]
pub struct HttpCache {
    entries: HashMap<(InstanceId, String), Entry>,
}

impl HttpCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a fresh cached response for `(instance, url)`, if any.
    pub fn get(&self, instance: InstanceId, url: &str) -> Option<HttpResponse> {
        let entry = self.entries.get(&(instance, url.to_string()))?;
        if Instant::now() >= entry.expires {
            None
        } else {
            Some(entry.response.clone())
        }
    }

    /// Store `response` for `(instance, url)` if its headers permit caching.
    pub fn store(&mut self, instance: InstanceId, url: &str, response: &HttpResponse) {
        if response.status != 200 {
            return;
        }
        let cache_control = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        if cache_control.contains("no-store") || cache_control.contains("no-cache") {
            return;
        }
        let Some(max_age) = parse_max_age(&cache_control) else {
            return;
        };
        // Never store Set-Cookie: a cookie write must happen exactly once, at
        // capture time in the engine — replaying one from cache would re-apply
        // stale cookies (and would persist them if the cache ever goes on disk).
        let mut response = response.clone();
        response
            .headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("set-cookie"));
        self.entries.insert(
            (instance, url.to_string()),
            Entry {
                response,
                expires: Instant::now() + Duration::from_secs(max_age),
            },
        );
    }

    /// Drop all entries for an instance (e.g. on identity reset).
    pub fn clear_instance(&mut self, instance: InstanceId) {
        self.entries.retain(|(i, _), _| *i != instance);
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn parse_max_age(cache_control: &str) -> Option<u64> {
    cache_control
        .split(',')
        .filter_map(|part| part.trim().strip_prefix("max-age="))
        .find_map(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(cache_control: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Cache-Control".to_string(), cache_control.to_string())],
            body: b"hi".to_vec(),
        }
    }

    fn a() -> InstanceId {
        InstanceId::from_u64_pair(0, 0xA)
    }
    fn b() -> InstanceId {
        InstanceId::from_u64_pair(0, 0xB)
    }

    #[test]
    fn caches_only_with_max_age_and_is_instance_partitioned() {
        let mut c = HttpCache::new();
        c.store(a(), "https://x/", &resp("max-age=60"));
        assert!(c.get(a(), "https://x/").is_some());
        // Sealed: another instance cannot see it.
        assert!(c.get(b(), "https://x/").is_none());

        // No directive => not cached.
        c.store(a(), "https://y/", &resp(""));
        assert!(c.get(a(), "https://y/").is_none());

        // no-store => not cached.
        c.store(a(), "https://z/", &resp("max-age=60, no-store"));
        assert!(c.get(a(), "https://z/").is_none());
    }

    #[test]
    fn expired_entry_is_not_returned() {
        let mut c = HttpCache::new();
        c.store(a(), "https://x/", &resp("max-age=0"));
        assert!(c.get(a(), "https://x/").is_none());
    }

    #[test]
    fn clear_instance_drops_only_that_instance() {
        let mut c = HttpCache::new();
        c.store(a(), "https://x/", &resp("max-age=60"));
        c.store(b(), "https://x/", &resp("max-age=60"));
        c.clear_instance(a());
        assert!(c.get(a(), "https://x/").is_none());
        assert!(c.get(b(), "https://x/").is_some());
    }
}

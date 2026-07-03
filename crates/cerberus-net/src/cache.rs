//! A small HTTP response cache, **partitioned by `InstanceId`** for privacy,
//! with **content-addressed body interning** for memory.
//!
//! A cache shared across identities would be a cross-site/cross-identity tracking
//! vector (cache-timing), so *entries* are sealed per instance just like cookies
//! (ADR-0006): one identity never sees another's hit/miss. But identical response
//! *bytes* cached by several instances of the same site need not be stored more
//! than once — so bodies are interned in a content-addressed pool shared across
//! instances (`Arc<[u8]>`, deduped by content hash, weak-referenced so a body
//! frees when its last entry drops). N instances caching identical content cost
//! one body allocation, while per-instance hit/miss behavior is unchanged
//! (ADR-0016).
//!
//! Conservative policy: only `200` responses with an explicit `Cache-Control:
//! max-age` are stored, and never when `no-store`/`no-cache` is present.

use crate::HttpResponse;
use cerberus_types::InstanceId;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

struct Entry {
    status: u16,
    headers: Vec<(String, String)>,
    /// Shared with any other instance's entry that cached identical bytes.
    body: Arc<[u8]>,
    expires: Instant,
}

/// An in-memory, per-instance response cache with shared body interning.
/// (On-disk caching is later work.)
#[derive(Default)]
pub struct HttpCache {
    /// Two-level so a lookup borrows the URL: keying a flat map on
    /// `(InstanceId, String)` forced a `url.to_string()` on every `get`, but a
    /// `HashMap<String, _>` can be probed with `&str` (via `Borrow`). Reads are
    /// the hot path (every subresource fetch checks the cache), so this drops one
    /// URL-length allocation per probe (issue #24).
    entries: HashMap<InstanceId, HashMap<String, Entry>>,
    /// Content-addressed body pool: `content-hash -> live body allocations`.
    /// Weak, so a body frees when its last [`Entry`] drops; dead weaks are pruned
    /// lazily on the next intern of the same hash (ADR-0016).
    bodies: HashMap<u64, Vec<Weak<[u8]>>>,
}

impl HttpCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a fresh cached response for `(instance, url)`, if any.
    pub fn get(&self, instance: InstanceId, url: &str) -> Option<HttpResponse> {
        let entry = self.entries.get(&instance)?.get(url)?;
        if Instant::now() >= entry.expires {
            None
        } else {
            Some(HttpResponse {
                status: entry.status,
                headers: entry.headers.clone(),
                body: entry.body.to_vec(),
            })
        }
    }

    /// Store `response` for `(instance, url)` if its headers permit caching. The
    /// body is interned, so an identical body already cached (by any instance) is
    /// stored only once.
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
        let headers: Vec<(String, String)> = response
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("set-cookie"))
            .cloned()
            .collect();
        let body = self.intern_body(&response.body);
        self.entries.entry(instance).or_default().insert(
            url.to_string(),
            Entry {
                status: response.status,
                headers,
                body,
                expires: Instant::now() + Duration::from_secs(max_age),
            },
        );
    }

    /// Return a shared handle to `bytes`, reusing an existing identical body
    /// (cached by any instance) so it is stored once. Prunes freed weak refs.
    fn intern_body(&mut self, bytes: &[u8]) -> Arc<[u8]> {
        let bucket = self.bodies.entry(body_hash(bytes)).or_default();
        let mut found: Option<Arc<[u8]>> = None;
        // Single pass: drop dead weaks, and adopt the first live identical body.
        bucket.retain(|weak| match weak.upgrade() {
            Some(arc) => {
                if found.is_none() && *arc == *bytes {
                    found = Some(arc.clone());
                }
                true
            }
            None => false,
        });
        if let Some(arc) = found {
            return arc;
        }
        let arc: Arc<[u8]> = Arc::from(bytes);
        bucket.push(Arc::downgrade(&arc));
        arc
    }

    /// Drop all entries for an instance (e.g. on identity reset). Interned bodies
    /// they held free here if no other instance still references them.
    pub fn clear_instance(&mut self, instance: InstanceId) {
        self.entries.remove(&instance);
    }

    /// Number of cached entries across all instances.
    pub fn len(&self) -> usize {
        self.entries.values().map(HashMap::len).sum()
    }

    /// Whether the cache holds no entries. An instance whose entries were all
    /// removed may leave an empty inner map, so check the entry counts rather
    /// than the outer map's emptiness.
    pub fn is_empty(&self) -> bool {
        self.entries.values().all(HashMap::is_empty)
    }

    /// Number of distinct body allocations currently live — the dedup invariant
    /// hook (ADR-0016): identical bodies across instances count once.
    #[cfg(test)]
    fn distinct_bodies(&self) -> usize {
        self.bodies
            .values()
            .flat_map(|bucket| bucket.iter())
            .filter(|w| w.strong_count() > 0)
            .count()
    }
}

fn body_hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
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

    fn resp_body(cache_control: &str, body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Cache-Control".to_string(), cache_control.to_string())],
            body: body.to_vec(),
        }
    }

    fn resp(cache_control: &str) -> HttpResponse {
        resp_body(cache_control, b"hi")
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

    #[test]
    fn identical_bodies_are_interned_once_across_instances() {
        let mut c = HttpCache::new();
        // Same body cached by two instances: two sealed entries, one allocation.
        c.store(a(), "https://x/", &resp_body("max-age=60", b"shared-bytes"));
        c.store(b(), "https://x/", &resp_body("max-age=60", b"shared-bytes"));
        assert_eq!(c.len(), 2, "per-instance entries are independent");
        assert_eq!(c.distinct_bodies(), 1, "but the body is stored once");
        assert_eq!(c.get(a(), "https://x/").unwrap().body, b"shared-bytes");
        assert_eq!(c.get(b(), "https://x/").unwrap().body, b"shared-bytes");

        // A different body is a distinct allocation.
        c.store(a(), "https://y/", &resp_body("max-age=60", b"other-bytes"));
        assert_eq!(c.distinct_bodies(), 2);
    }

    #[test]
    fn len_and_is_empty_track_entries_not_the_instance_map() {
        // With the two-level map, clearing an instance can leave an empty inner
        // map behind; `len`/`is_empty` must count entries, not instance buckets.
        let mut c = HttpCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        c.store(a(), "https://x/", &resp("max-age=60"));
        c.store(a(), "https://y/", &resp("max-age=60"));
        c.store(b(), "https://x/", &resp("max-age=60"));
        assert_eq!(c.len(), 3);
        c.clear_instance(a());
        assert_eq!(c.len(), 1, "only b's entry remains");
        c.clear_instance(b());
        assert!(c.is_empty(), "no entries left even if a bucket lingers");
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn interned_body_frees_when_last_entry_drops() {
        let mut c = HttpCache::new();
        c.store(a(), "https://x/", &resp_body("max-age=60", b"bytes"));
        c.store(b(), "https://x/", &resp_body("max-age=60", b"bytes"));
        assert_eq!(c.distinct_bodies(), 1);
        c.clear_instance(a());
        // b still holds it.
        assert_eq!(c.distinct_bodies(), 1);
        c.clear_instance(b());
        // No entry references it now — the allocation is freed (weak is dead).
        assert_eq!(c.distinct_bodies(), 0);
    }
}

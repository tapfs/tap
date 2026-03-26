use dashmap::DashMap;
use std::time::{Duration, Instant};

use crate::connector::traits::ResourceMeta;

/// The full content of a fetched resource.
#[derive(Debug, Clone)]
pub struct Resource {
    pub data: Vec<u8>,
}

// ─────────────────────────────────────────────────────────────────────

struct CacheEntry<T> {
    value: T,
    inserted_at: Instant,
    ttl: Duration,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() > self.ttl
    }
}

/// A concurrent, TTL-aware in-memory cache.
///
/// Two separate maps are maintained – one for full resource content and one
/// for metadata listings – because their access patterns and sizes differ.
/// All public methods are safe to call from multiple threads concurrently
/// thanks to `DashMap`.
pub struct Cache {
    resources: DashMap<String, CacheEntry<Resource>>,
    metadata: DashMap<String, CacheEntry<Vec<ResourceMeta>>>,
    default_ttl: Duration,
}

impl Cache {
    /// Create a new cache where entries expire after `default_ttl`.
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            resources: DashMap::new(),
            metadata: DashMap::new(),
            default_ttl,
        }
    }

    // ── Resource content ─────────────────────────────────────────

    /// Retrieve a cached resource by key.
    ///
    /// Returns `None` if the key is absent **or** the entry has expired.
    /// Expired entries are lazily removed on access.
    pub fn get_resource(&self, key: &str) -> Option<Resource> {
        let entry = self.resources.get(key)?;
        if entry.is_expired() {
            drop(entry); // release read lock before mutating
            self.resources.remove(key);
            None
        } else {
            Some(entry.value.clone())
        }
    }

    /// Insert (or replace) a resource in the cache with the default TTL.
    pub fn put_resource(&self, key: &str, resource: Resource) {
        self.resources.insert(
            key.to_string(),
            CacheEntry {
                value: resource,
                inserted_at: Instant::now(),
                ttl: self.default_ttl,
            },
        );
    }

    // ── Metadata listings ────────────────────────────────────────

    /// Retrieve cached metadata for a collection key.
    ///
    /// Returns `None` if the key is absent or expired.
    pub fn get_metadata(&self, key: &str) -> Option<Vec<ResourceMeta>> {
        let entry = self.metadata.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.metadata.remove(key);
            None
        } else {
            Some(entry.value.clone())
        }
    }

    /// Insert (or replace) metadata in the cache with the default TTL.
    pub fn put_metadata(&self, key: &str, meta: Vec<ResourceMeta>) {
        self.metadata.insert(
            key.to_string(),
            CacheEntry {
                value: meta,
                inserted_at: Instant::now(),
                ttl: self.default_ttl,
            },
        );
    }

    // ── Maintenance ──────────────────────────────────────────────

    /// Remove **both** the resource and metadata entry for a key.
    pub fn invalidate(&self, key: &str) {
        self.resources.remove(key);
        self.metadata.remove(key);
    }

    /// Walk every entry in both maps and remove any that have exceeded
    /// their TTL.  This is intended to be called periodically from a
    /// background `tokio` task so that stale data does not accumulate
    /// indefinitely.
    pub fn evict_expired(&self) {
        self.resources
            .retain(|_k: &String, v: &mut CacheEntry<Resource>| !v.is_expired());
        self.metadata
            .retain(|_k: &String, v: &mut CacheEntry<Vec<ResourceMeta>>| !v.is_expired());
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn put_and_get_resource() {
        let cache = Cache::new(Duration::from_secs(60));
        cache.put_resource("k", Resource { data: vec![1, 2, 3] });
        let r = cache.get_resource("k").unwrap();
        assert_eq!(r.data, vec![1, 2, 3]);
    }

    #[test]
    fn expired_resource_returns_none() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_resource("k", Resource { data: vec![1] });
        thread::sleep(Duration::from_millis(30));
        assert!(cache.get_resource("k").is_none());
    }

    #[test]
    fn put_and_get_metadata() {
        let cache = Cache::new(Duration::from_secs(60));
        let meta = vec![ResourceMeta {
            id: "1".into(),
            slug: "a".into(),
            title: None,
            updated_at: None,
            content_type: None,
        }];
        cache.put_metadata("col", meta);
        let m = cache.get_metadata("col").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].slug, "a");
    }

    #[test]
    fn expired_metadata_returns_none() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_metadata(
            "col",
            vec![ResourceMeta {
                id: "1".into(),
                slug: "x".into(),
                title: None,
                updated_at: None,
                content_type: None,
            }],
        );
        thread::sleep(Duration::from_millis(30));
        assert!(cache.get_metadata("col").is_none());
    }

    #[test]
    fn invalidate_removes_both() {
        let cache = Cache::new(Duration::from_secs(60));
        cache.put_resource("k", Resource { data: vec![1] });
        cache.put_metadata("k", vec![]);
        cache.invalidate("k");
        assert!(cache.get_resource("k").is_none());
        assert!(cache.get_metadata("k").is_none());
    }

    #[test]
    fn evict_expired_cleans_up() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_resource("a", Resource { data: vec![1] });
        cache.put_metadata("b", vec![]);
        thread::sleep(Duration::from_millis(30));
        // Add a fresh entry that should survive eviction.
        cache.put_resource("c", Resource { data: vec![2] });

        cache.evict_expired();

        assert!(cache.get_resource("a").is_none());
        assert!(cache.get_metadata("b").is_none());
        assert!(cache.get_resource("c").is_some());
    }

    #[test]
    fn missing_key_returns_none() {
        let cache = Cache::new(Duration::from_secs(60));
        assert!(cache.get_resource("nope").is_none());
        assert!(cache.get_metadata("nope").is_none());
    }
}

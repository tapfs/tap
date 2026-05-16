use dashmap::DashMap;
use std::time::{Duration, Instant};

use crate::connector::traits::ResourceMeta;

/// Maximum size of a resource that will be cached in memory (5 MB).
/// Larger resources are served but not cached.
pub const MAX_CACHEABLE_SIZE: usize = 5 * 1024 * 1024;

/// Cached resource content.
///
/// Stores the rendered content as `bytes::Bytes` (O(1) clone) and
/// optionally the raw JSON from the API for `tap inspect`.
#[derive(Debug, Clone)]
pub struct Resource {
    pub data: bytes::Bytes,
    pub raw_json: Option<serde_json::Value>,
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
/// Three maps are maintained — full resource content, metadata listings, and
/// per-resource "frontmatter shards" populated from list-response items when
/// a spec declares `populates`.
pub struct Cache {
    resources: DashMap<String, CacheEntry<Resource>>,
    metadata: DashMap<String, CacheEntry<Vec<ResourceMeta>>>,
    shards: DashMap<String, CacheEntry<serde_json::Value>>,
    default_ttl: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    pub resources: usize,
    pub metadata: usize,
    pub shards: usize,
}

impl Cache {
    /// Create a new cache where entries expire after `default_ttl`.
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            resources: DashMap::new(),
            metadata: DashMap::new(),
            shards: DashMap::new(),
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

    // ── Frontmatter shards ───────────────────────────────────────

    /// Canonical key shape for a shard: `connector/collection/id`. Kept
    /// here so call sites can't drift from the storage format.
    pub fn shard_key(connector: &str, collection: &str, id: &str) -> String {
        format!("{}/{}/{}", connector, collection, id)
    }

    pub fn get_shard(&self, key: &str) -> Option<serde_json::Value> {
        let entry = self.shards.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.shards.remove(key);
            None
        } else {
            Some(entry.value.clone())
        }
    }

    pub fn put_shard(&self, key: &str, shard: serde_json::Value) {
        self.shards.insert(
            key.to_string(),
            CacheEntry {
                value: shard,
                inserted_at: Instant::now(),
                ttl: self.default_ttl,
            },
        );
    }

    // ── Stats ────────────────────────────────────────────────────

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            resources: self.resources.len(),
            metadata: self.metadata.len(),
            shards: self.shards.len(),
        }
    }

    // ── Maintenance ──────────────────────────────────────────────

    /// Remove the resource, metadata, and shard entry for a key.
    pub fn invalidate(&self, key: &str) {
        self.resources.remove(key);
        self.metadata.remove(key);
        self.shards.remove(key);
    }

    /// Walk every entry in all maps and remove any that have exceeded
    /// their TTL.  This is intended to be called periodically from a
    /// background `tokio` task so that stale data does not accumulate
    /// indefinitely.
    pub fn evict_expired(&self) {
        self.resources
            .retain(|_k: &String, v: &mut CacheEntry<Resource>| !v.is_expired());
        self.metadata
            .retain(|_k: &String, v: &mut CacheEntry<Vec<ResourceMeta>>| !v.is_expired());
        self.shards
            .retain(|_k: &String, v: &mut CacheEntry<serde_json::Value>| !v.is_expired());
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
        cache.put_resource(
            "k",
            Resource {
                raw_json: None,
                data: vec![1u8, 2, 3].into(),
            },
        );
        let r = cache.get_resource("k").unwrap();
        assert_eq!(&r.data[..], &[1, 2, 3]);
    }

    #[test]
    fn expired_resource_returns_none() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_resource(
            "k",
            Resource {
                raw_json: None,
                data: vec![1u8].into(),
            },
        );
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
            group: None,
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
                group: None,
            }],
        );
        thread::sleep(Duration::from_millis(30));
        assert!(cache.get_metadata("col").is_none());
    }

    #[test]
    fn invalidate_removes_both() {
        let cache = Cache::new(Duration::from_secs(60));
        cache.put_resource(
            "k",
            Resource {
                raw_json: None,
                data: vec![1u8].into(),
            },
        );
        cache.put_metadata("k", vec![]);
        cache.invalidate("k");
        assert!(cache.get_resource("k").is_none());
        assert!(cache.get_metadata("k").is_none());
    }

    #[test]
    fn evict_expired_cleans_up() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_resource(
            "a",
            Resource {
                raw_json: None,
                data: vec![1u8].into(),
            },
        );
        cache.put_metadata("b", vec![]);
        thread::sleep(Duration::from_millis(30));
        // Add a fresh entry that should survive eviction.
        cache.put_resource(
            "c",
            Resource {
                raw_json: None,
                data: vec![2u8].into(),
            },
        );

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

    #[test]
    fn put_and_get_shard() {
        let cache = Cache::new(Duration::from_secs(60));
        let shard = serde_json::json!({"title": "hello", "state": "open"});
        cache.put_shard("k", shard.clone());
        let got = cache.get_shard("k").expect("shard should be present");
        assert_eq!(got, shard);
    }

    #[test]
    fn expired_shard_returns_none() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_shard("k", serde_json::json!({"x": 1}));
        thread::sleep(Duration::from_millis(30));
        assert!(cache.get_shard("k").is_none());
    }

    #[test]
    fn invalidate_removes_shard() {
        let cache = Cache::new(Duration::from_secs(60));
        cache.put_shard("k", serde_json::json!({"x": 1}));
        cache.invalidate("k");
        assert!(cache.get_shard("k").is_none());
    }

    #[test]
    fn evict_expired_cleans_shards() {
        let cache = Cache::new(Duration::from_millis(10));
        cache.put_shard("stale", serde_json::json!({"x": 1}));
        thread::sleep(Duration::from_millis(30));
        cache.put_shard("fresh", serde_json::json!({"x": 2}));
        cache.evict_expired();
        assert!(cache.get_shard("stale").is_none());
        assert!(cache.get_shard("fresh").is_some());
    }
}

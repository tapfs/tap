//! Persistent on-disk cache for resource bytes.
//!
//! Sits below `store::Cache` (the in-memory L1) as L2: survives restarts,
//! has no size cap, and is validated against the connector's `updated_at`
//! timestamp before being served. Two files per entry: `<hash>.bin`
//! holds the rendered content; `<hash>.meta` holds a JSON sidecar with the
//! id, freshness signal, fetched_at and (optionally) raw API JSON for
//! `tap inspect`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMeta {
    /// Resource id (slug). Stored so the file is inspectable without
    /// recovering the hash → id mapping, and so we can detect (and ignore)
    /// hash collisions on read.
    pub id: String,
    /// `ResourceMeta.updated_at` from when this entry was written. The next
    /// read compares this against the freshly-listed `updated_at` to decide
    /// whether the bytes are still valid.
    pub updated_at: Option<String>,
    /// RFC3339 wall-clock time the entry was fetched.
    pub fetched_at: String,
    /// Raw API JSON, surfaced via `tap inspect` if present.
    pub raw_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct DiskEntry {
    pub data: Bytes,
    pub meta: DiskMeta,
}

pub struct DiskCache {
    root: PathBuf,
}

impl DiskCache {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get(&self, connector: &str, collection: &str, id: &str) -> Option<DiskEntry> {
        let (_, bin, meta) = self.paths(connector, collection, id);
        let meta_str = fs::read_to_string(&meta).ok()?;
        let meta: DiskMeta = serde_json::from_str(&meta_str).ok()?;
        if meta.id != id {
            // Hash collision — different id ended up at the same path.
            return None;
        }
        let data = fs::read(&bin).ok()?;
        Some(DiskEntry {
            data: Bytes::from(data),
            meta,
        })
    }

    pub fn put(
        &self,
        connector: &str,
        collection: &str,
        id: &str,
        entry: &DiskEntry,
    ) -> io::Result<()> {
        let (dir, bin, meta) = self.paths(connector, collection, id);
        fs::create_dir_all(&dir)?;
        write_atomic(&bin, &entry.data)?;
        let json = serde_json::to_vec(&entry.meta).map_err(io::Error::other)?;
        write_atomic(&meta, &json)?;
        Ok(())
    }

    pub fn invalidate(&self, connector: &str, collection: &str, id: &str) {
        let (_, bin, meta) = self.paths(connector, collection, id);
        let _ = fs::remove_file(&bin);
        let _ = fs::remove_file(&meta);
    }

    /// Invalidate using the same flat `connector/collection/id` key the
    /// in-memory cache uses (for IPC convenience). Keys with fewer than
    /// three components are silently ignored — they refer to listings,
    /// which the disk cache does not store.
    pub fn invalidate_key(&self, key: &str) {
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() == 3 {
            self.invalidate(parts[0], parts[1], parts[2]);
        }
    }

    fn paths(&self, connector: &str, collection: &str, id: &str) -> (PathBuf, PathBuf, PathBuf) {
        let hash = stable_hash(id);
        let dir = self.root.join(safe(connector)).join(safe(collection));
        let bin = dir.join(format!("{:016x}.bin", hash));
        let meta = dir.join(format!("{:016x}.meta", hash));
        (dir, bin, meta)
    }
}

fn write_atomic(dest: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = dest.with_extension({
        let mut s = dest
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_owned();
        s.push_str(".tmp");
        s
    });
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, dest)
}

fn stable_hash(id: &str) -> u64 {
    use siphasher::sip::SipHasher13;
    use std::hash::{Hash, Hasher};
    // Distinct keys from the inode hasher so we don't accidentally entangle
    // namespaces, even though the inputs differ.
    let mut h = SipHasher13::new_with_keys(0x_6361_6368_655f_6469, 0x_736b_5f6f_6e64_6973);
    id.hash(&mut h);
    h.finish()
}

fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(data: &[u8], updated_at: Option<&str>) -> DiskEntry {
        DiskEntry {
            data: Bytes::copy_from_slice(data),
            meta: DiskMeta {
                id: "ignored-by-helper".into(),
                updated_at: updated_at.map(str::to_string),
                fetched_at: "2026-04-25T00:00:00Z".into(),
                raw_json: None,
            },
        }
    }

    #[test]
    fn put_and_get_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();

        let mut e = entry(b"hello world", Some("2026-01-01T00:00:00Z"));
        e.meta.id = "alpha".into();
        cache.put("salesforce", "accounts", "alpha", &e).unwrap();

        let got = cache.get("salesforce", "accounts", "alpha").unwrap();
        assert_eq!(&got.data[..], b"hello world");
        assert_eq!(got.meta.updated_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(got.meta.id, "alpha");
    }

    #[test]
    fn missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();
        assert!(cache.get("c", "co", "id").is_none());
    }

    #[test]
    fn invalidate_removes_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();
        let mut e = entry(b"x", None);
        e.meta.id = "id".into();
        cache.put("c", "co", "id", &e).unwrap();
        assert!(cache.get("c", "co", "id").is_some());
        cache.invalidate("c", "co", "id");
        assert!(cache.get("c", "co", "id").is_none());
    }

    #[test]
    fn invalidate_key_parses_flat_form() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();
        let mut e = entry(b"x", None);
        e.meta.id = "id".into();
        cache.put("c", "co", "id", &e).unwrap();
        cache.invalidate_key("c/co/id");
        assert!(cache.get("c", "co", "id").is_none());

        // Two-component key is a listing; should be a no-op (and not panic).
        cache.invalidate_key("c/co");
    }

    #[test]
    fn put_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();
        let mut e1 = entry(b"first", Some("v1"));
        e1.meta.id = "id".into();
        cache.put("c", "co", "id", &e1).unwrap();

        let mut e2 = entry(b"second", Some("v2"));
        e2.meta.id = "id".into();
        cache.put("c", "co", "id", &e2).unwrap();

        let got = cache.get("c", "co", "id").unwrap();
        assert_eq!(&got.data[..], b"second");
        assert_eq!(got.meta.updated_at.as_deref(), Some("v2"));
    }

    #[test]
    fn survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let cache = DiskCache::new(tmp.path()).unwrap();
            let mut e = entry(b"persist me", Some("v"));
            e.meta.id = "id".into();
            cache.put("c", "co", "id", &e).unwrap();
        }
        let cache2 = DiskCache::new(tmp.path()).unwrap();
        let got = cache2.get("c", "co", "id").unwrap();
        assert_eq!(&got.data[..], b"persist me");
    }

    #[test]
    fn unsafe_chars_in_connector_or_collection_are_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DiskCache::new(tmp.path()).unwrap();
        let mut e = entry(b"x", None);
        e.meta.id = "id".into();
        // Slashes and dots get replaced; round-trip still works.
        cache.put("a/b", "c.d", "id", &e).unwrap();
        assert!(cache.get("a/b", "c.d", "id").is_some());
    }
}

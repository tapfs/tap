use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Persistent, on-disk version snapshot storage.
///
/// Versions live under a configurable base directory (typically
/// `~/.tapfs/versions/`) and are organised as:
///
/// ```text
/// <base_dir>/<connector>/<collection>/<slug>/v<N>
/// ```
///
/// Version numbers are monotonically increasing positive integers
/// starting at 1.
pub struct VersionStore {
    base_dir: PathBuf,
}

impl VersionStore {
    /// Create a new `VersionStore` rooted at `base_dir`.
    ///
    /// The directory (and any parents) will be created if they do not
    /// already exist.
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_dir).with_context(|| {
            format!("failed to create version directory: {}", base_dir.display())
        })?;
        Ok(Self { base_dir })
    }

    /// Save a content snapshot, returning the assigned version number.
    ///
    /// The version number is determined by scanning existing versions
    /// on disk and incrementing.
    pub fn save_snapshot(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
        content: &[u8],
    ) -> Result<u32> {
        let ver = self.next_version(connector, collection, slug)?;
        let dir = self.version_dir(connector, collection, slug);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create version dir: {}", dir.display()))?;

        let file = dir.join(format!("v{}", ver));
        fs::write(&file, content)
            .with_context(|| format!("failed to write version file: {}", file.display()))?;
        Ok(ver)
    }

    /// Read the content of a specific version.
    ///
    /// Returns `Ok(None)` if the version does not exist.
    pub fn read_version(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
        version: u32,
    ) -> Result<Option<Vec<u8>>> {
        let file = self
            .version_dir(connector, collection, slug)
            .join(format!("v{}", version));
        if !file.exists() {
            return Ok(None);
        }
        let data = fs::read(&file)
            .with_context(|| format!("failed to read version file: {}", file.display()))?;
        Ok(Some(data))
    }

    /// List all version numbers for a resource, sorted ascending.
    pub fn list_versions(&self, connector: &str, collection: &str, slug: &str) -> Result<Vec<u32>> {
        let dir = self.version_dir(connector, collection, slug);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::new();
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read version dir: {}", dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num_str) = name.strip_prefix('v') {
                if let Ok(v) = num_str.parse::<u32>() {
                    versions.push(v);
                }
            }
        }
        versions.sort();
        Ok(versions)
    }

    // ── private ──────────────────────────────────────────────────

    /// Determine the next version number by finding the current maximum
    /// and adding one.  Returns 1 if no versions exist yet.
    fn next_version(&self, connector: &str, collection: &str, slug: &str) -> Result<u32> {
        let existing = self.list_versions(connector, collection, slug)?;
        Ok(existing.last().copied().unwrap_or(0) + 1)
    }

    fn version_dir(&self, connector: &str, collection: &str, slug: &str) -> PathBuf {
        self.base_dir.join(connector).join(collection).join(slug)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store() -> (VersionStore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = VersionStore::new(tmp.path().to_path_buf()).unwrap();
        (store, tmp)
    }

    #[test]
    fn first_snapshot_is_v1() {
        let (store, _tmp) = make_store();
        let v = store
            .save_snapshot("rest", "items", "item-1", b"initial")
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn successive_snapshots_increment() {
        let (store, _tmp) = make_store();
        let v1 = store
            .save_snapshot("rest", "items", "item-1", b"one")
            .unwrap();
        let v2 = store
            .save_snapshot("rest", "items", "item-1", b"two")
            .unwrap();
        let v3 = store
            .save_snapshot("rest", "items", "item-1", b"three")
            .unwrap();
        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
        assert_eq!(v3, 3);
    }

    #[test]
    fn read_existing_version() {
        let (store, _tmp) = make_store();
        store
            .save_snapshot("rest", "items", "a", b"content-v1")
            .unwrap();
        store
            .save_snapshot("rest", "items", "a", b"content-v2")
            .unwrap();

        assert_eq!(
            store.read_version("rest", "items", "a", 1).unwrap(),
            Some(b"content-v1".to_vec())
        );
        assert_eq!(
            store.read_version("rest", "items", "a", 2).unwrap(),
            Some(b"content-v2".to_vec())
        );
    }

    #[test]
    fn read_missing_version_returns_none() {
        let (store, _tmp) = make_store();
        assert_eq!(store.read_version("rest", "items", "a", 99).unwrap(), None);
    }

    #[test]
    fn list_versions_sorted() {
        let (store, _tmp) = make_store();
        store.save_snapshot("c", "col", "s", b"1").unwrap();
        store.save_snapshot("c", "col", "s", b"2").unwrap();
        store.save_snapshot("c", "col", "s", b"3").unwrap();

        let versions = store.list_versions("c", "col", "s").unwrap();
        assert_eq!(versions, vec![1, 2, 3]);
    }

    #[test]
    fn list_versions_empty() {
        let (store, _tmp) = make_store();
        let versions = store.list_versions("c", "col", "s").unwrap();
        assert!(versions.is_empty());
    }

    #[test]
    fn separate_resources_are_independent() {
        let (store, _tmp) = make_store();
        let va = store.save_snapshot("c", "col", "a", b"a1").unwrap();
        let vb = store.save_snapshot("c", "col", "b", b"b1").unwrap();
        assert_eq!(va, 1);
        assert_eq!(vb, 1); // each resource starts at 1
    }
}

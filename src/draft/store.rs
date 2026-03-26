use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Persistent, on-disk draft storage using copy-on-write semantics.
///
/// Drafts live under a configurable base directory (typically
/// `~/.tapfs/drafts/`) and are organised as:
///
/// ```text
/// <base_dir>/<connector>/<collection>/<slug>.draft
/// ```
pub struct DraftStore {
    base_dir: PathBuf,
}

impl DraftStore {
    /// Create a new `DraftStore` rooted at `base_dir`.
    ///
    /// The directory (and any parents) will be created if they do not
    /// already exist.
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_dir)
            .with_context(|| format!("failed to create draft directory: {}", base_dir.display()))?;
        Ok(Self { base_dir })
    }

    /// Create a draft from live content (copy-on-write).
    ///
    /// If a draft already exists for this resource it is **overwritten**,
    /// mirroring a "copy latest live content" workflow.
    pub fn create_draft(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
        content: &[u8],
    ) -> Result<()> {
        let path = self.draft_path(connector, collection, slug);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create draft parent dir: {}", parent.display())
            })?;
        }
        fs::write(&path, content)
            .with_context(|| format!("failed to write draft: {}", path.display()))?;
        Ok(())
    }

    /// Read draft content, returning `Ok(None)` if no draft exists.
    pub fn read_draft(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
    ) -> Result<Option<Vec<u8>>> {
        let path = self.draft_path(connector, collection, slug);
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)
            .with_context(|| format!("failed to read draft: {}", path.display()))?;
        Ok(Some(data))
    }

    /// Write (update) draft content.  Creates parent directories if needed.
    pub fn write_draft(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
        content: &[u8],
    ) -> Result<()> {
        // Delegates to create_draft – same semantics.
        self.create_draft(connector, collection, slug, content)
    }

    /// Delete a draft.  Returns `Ok(true)` if a draft was removed, or
    /// `Ok(false)` if there was nothing to delete.
    pub fn delete_draft(
        &self,
        connector: &str,
        collection: &str,
        slug: &str,
    ) -> Result<bool> {
        let path = self.draft_path(connector, collection, slug);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete draft: {}", path.display()))?;
        Ok(true)
    }

    /// Check whether a draft exists on disk.
    pub fn has_draft(&self, connector: &str, collection: &str, slug: &str) -> bool {
        self.draft_path(connector, collection, slug).exists()
    }

    /// List all draft slugs for a given connector/collection pair.
    ///
    /// Returns an empty `Vec` if the collection directory does not exist.
    pub fn list_drafts(
        &self,
        connector: &str,
        collection: &str,
    ) -> Result<Vec<String>> {
        let dir = self.base_dir.join(connector).join(collection);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut slugs = Vec::new();
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read draft dir: {}", dir.display()))?
        {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if let Some(slug) = name.strip_suffix(".draft") {
                slugs.push(slug.to_string());
            }
        }
        slugs.sort();
        Ok(slugs)
    }

    // ── private ──────────────────────────────────────────────────

    fn draft_path(&self, connector: &str, collection: &str, slug: &str) -> PathBuf {
        self.base_dir
            .join(connector)
            .join(collection)
            .join(format!("{}.draft", slug))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store() -> (DraftStore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = DraftStore::new(tmp.path().to_path_buf()).unwrap();
        (store, tmp)
    }

    #[test]
    fn create_and_read_draft() {
        let (store, _tmp) = make_store();
        store
            .create_draft("rest", "items", "item-1", b"hello")
            .unwrap();
        let data = store.read_draft("rest", "items", "item-1").unwrap();
        assert_eq!(data, Some(b"hello".to_vec()));
    }

    #[test]
    fn read_missing_draft_returns_none() {
        let (store, _tmp) = make_store();
        let data = store.read_draft("rest", "items", "nope").unwrap();
        assert_eq!(data, None);
    }

    #[test]
    fn write_overwrites_existing() {
        let (store, _tmp) = make_store();
        store
            .create_draft("rest", "items", "item-1", b"v1")
            .unwrap();
        store
            .write_draft("rest", "items", "item-1", b"v2")
            .unwrap();
        let data = store.read_draft("rest", "items", "item-1").unwrap();
        assert_eq!(data, Some(b"v2".to_vec()));
    }

    #[test]
    fn delete_existing_draft() {
        let (store, _tmp) = make_store();
        store
            .create_draft("rest", "items", "item-1", b"data")
            .unwrap();
        assert!(store.delete_draft("rest", "items", "item-1").unwrap());
        assert!(!store.has_draft("rest", "items", "item-1"));
    }

    #[test]
    fn delete_missing_draft_returns_false() {
        let (store, _tmp) = make_store();
        assert!(!store.delete_draft("rest", "items", "nope").unwrap());
    }

    #[test]
    fn has_draft_true_and_false() {
        let (store, _tmp) = make_store();
        assert!(!store.has_draft("rest", "items", "x"));
        store.create_draft("rest", "items", "x", b"").unwrap();
        assert!(store.has_draft("rest", "items", "x"));
    }

    #[test]
    fn list_drafts_returns_sorted_slugs() {
        let (store, _tmp) = make_store();
        store
            .create_draft("rest", "items", "charlie", b"")
            .unwrap();
        store
            .create_draft("rest", "items", "alpha", b"")
            .unwrap();
        store
            .create_draft("rest", "items", "bravo", b"")
            .unwrap();
        let slugs = store.list_drafts("rest", "items").unwrap();
        assert_eq!(slugs, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn list_drafts_empty_collection() {
        let (store, _tmp) = make_store();
        let slugs = store.list_drafts("rest", "nothing").unwrap();
        assert!(slugs.is_empty());
    }

    #[test]
    fn separate_collections_are_independent() {
        let (store, _tmp) = make_store();
        store
            .create_draft("rest", "items", "a", b"items-a")
            .unwrap();
        store
            .create_draft("rest", "posts", "a", b"posts-a")
            .unwrap();
        assert_eq!(
            store.read_draft("rest", "items", "a").unwrap(),
            Some(b"items-a".to_vec())
        );
        assert_eq!(
            store.read_draft("rest", "posts", "a").unwrap(),
            Some(b"posts-a".to_vec())
        );
    }
}

//! Path-resolution helpers: the on-disk slug map, slug normalization, the
//! `name → CollectionSpec` walk that handles nested collections, and the
//! `filename → (slug, ResourceVariant)` parser.
//!
//! All free functions / standalone struct. The `resolve_*_child` methods on
//! `VirtualFs` itself stay in `core.rs` because they need `&self`.

use std::path::PathBuf;

use dashmap::DashMap;

use crate::connector::spec::CollectionSpec;

use super::types::*;

// ---------------------------------------------------------------------------
// SlugMap — bidirectional api_id ↔ user_slug persistence
// ---------------------------------------------------------------------------

/// Maps api_id ↔ user_slug for readdir display and slug resolution.
/// Persisted to disk as `{data_dir}/slug-map.json` (forward map only;
/// reverse is rebuilt on load).
pub(crate) struct SlugMap {
    /// "connector/collection/api_id" → user_slug
    forward: DashMap<String, String>,
    /// "connector/collection/user_slug" → api_id  (rebuilt from forward on load)
    reverse: DashMap<String, String>,
    path: PathBuf,
}

impl SlugMap {
    pub(crate) fn load(path: PathBuf) -> Self {
        let forward = DashMap::new();
        let reverse = DashMap::new();
        if path.exists() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(map) =
                    serde_json::from_slice::<std::collections::HashMap<String, String>>(&bytes)
                {
                    for (k, v) in map {
                        // k = "connector/collection/api_id", v = user_slug
                        // Rebuild reverse: "connector/collection/user_slug" → api_id
                        if let Some((prefix, api_id)) = k.rsplit_once('/') {
                            reverse.insert(format!("{}/{}", prefix, v), api_id.to_string());
                        }
                        forward.insert(k, v);
                    }
                }
            }
        }
        Self {
            forward,
            reverse,
            path,
        }
    }

    pub(crate) fn insert(&self, connector: &str, collection: &str, api_id: &str, user_slug: &str) {
        let fwd_key = format!("{}/{}/{}", connector, collection, api_id);
        let rev_key = format!("{}/{}/{}", connector, collection, user_slug);
        // Remove stale reverse entry if api_id previously had a different slug
        if let Some(old_slug) = self.forward.get(&fwd_key) {
            let old_rev = format!("{}/{}/{}", connector, collection, old_slug.value());
            self.reverse.remove(&old_rev);
        }
        self.forward.insert(fwd_key, user_slug.to_string());
        self.reverse.insert(rev_key, api_id.to_string());
        self.save();
    }

    pub(crate) fn get_user_slug(
        &self,
        connector: &str,
        collection: &str,
        api_id: &str,
    ) -> Option<String> {
        self.forward
            .get(&format!("{}/{}/{}", connector, collection, api_id))
            .map(|v| v.clone())
    }

    /// Resolve a user-visible slug back to its API id.
    pub(crate) fn get_api_id(
        &self,
        connector: &str,
        collection: &str,
        user_slug: &str,
    ) -> Option<String> {
        self.reverse
            .get(&format!("{}/{}/{}", connector, collection, user_slug))
            .map(|v| v.clone())
    }

    /// Returns true if `user_slug` is already claimed by a *different* api_id.
    pub(crate) fn slug_taken(
        &self,
        connector: &str,
        collection: &str,
        user_slug: &str,
        api_id: &str,
    ) -> bool {
        match self
            .reverse
            .get(&format!("{}/{}/{}", connector, collection, user_slug))
        {
            Some(existing_id) => existing_id.value() != api_id,
            None => false,
        }
    }

    fn save(&self) {
        let map: std::collections::HashMap<String, String> = self
            .forward
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        if let Ok(json) = serde_json::to_string(&map) {
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Convert a human-readable title to a URL-safe slug.
/// "Fix Login Bug!" → "fix-login-bug"
pub(crate) fn title_to_slug(title: &str) -> String {
    let mut result = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !result.is_empty() {
            if !prev_dash {
                result.push('-');
            }
            prev_dash = true;
        }
    }
    // Trim trailing dash
    if result.ends_with('-') {
        result.pop();
    }
    result
}

/// Walk a (possibly path-encoded) collection name through a spec's
/// subcollection tree.
///
/// For a flat name like `"repos"`, finds the top-level collection directly.
/// For a path-encoded name like `"repos/tap/issues"`, walks:
///   repos (top-level) → skip "tap" (resource id) → issues (subcollection)
pub(crate) fn find_collection_spec_in<'a>(
    cols: &'a [CollectionSpec],
    name: &str,
) -> Option<&'a CollectionSpec> {
    let segments: Vec<&str> = name.split('/').collect();
    if segments.is_empty() {
        return None;
    }

    let mut current = cols.iter().find(|c| c.name == segments[0])?;
    let mut i = 1;

    while i < segments.len() {
        i += 1; // skip resource-id segment
        if i >= segments.len() {
            break;
        }
        let sub_name = segments[i];
        i += 1;
        current = current
            .subcollections
            .as_deref()?
            .iter()
            .find(|c| c.name == sub_name)?;
    }

    Some(current)
}

/// The slug used to store a lock in the DraftStore.
pub(crate) fn lock_slug(slug: &str) -> String {
    format!("{}.lock", slug)
}

/// Parse a filename into (resource_slug, ResourceVariant).
pub(crate) fn parse_resource_filename(name: &str) -> Result<(String, ResourceVariant), VfsError> {
    // Reject hidden/temp files (vim .swp, macOS .DS_Store, etc.)
    if name.starts_with('.') {
        return Err(VfsError::PermissionDenied);
    }
    if let Some(base) = name.strip_suffix(".lock") {
        if base.is_empty() {
            return Err(VfsError::NotFound);
        }
        return Ok((base.to_string(), ResourceVariant::Lock));
    }
    if let Some(without_md) = name.strip_suffix(".md") {
        if without_md.is_empty() {
            return Err(VfsError::NotFound);
        }
        if let Some(base) = without_md.strip_suffix(".draft") {
            if base.is_empty() {
                return Err(VfsError::NotFound);
            }
            return Ok((base.to_string(), ResourceVariant::Draft));
        }
        return Ok((without_md.to_string(), ResourceVariant::Live));
    }
    // Bare name, treat as live.
    Ok((name.to_string(), ResourceVariant::Live))
}

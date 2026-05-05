//! Aggregate-collection support: a `Collection` whose spec sets `aggregate:
//! true` is exposed as a single `.md` file rather than a directory of
//! per-resource files (see `connectors/github.yaml` `comments` for the
//! canonical example). Reading concatenates all member resources with
//! `---` separators; the append-only write flow lives inside `flush()` for
//! now and depends on `read_aggregate_collection` to compute the canonical
//! prefix it diffs against.
//!
//! `is_aggregate_collection` is the spec-driven dispatch check used by
//! `read`, `write`, and `flush` to decide whether to take the aggregate
//! path. See `docs/proposals/aggregate-snapshot.md` for the planned
//! evolution of the diff/write semantics — the current prefix-match has
//! known silent-failure modes.

use super::core::VirtualFs;
use super::path::find_collection_spec_in;
use super::types::*;

impl VirtualFs {
    pub(crate) fn is_aggregate_collection(&self, connector: &str, collection: &str) -> bool {
        self.registry
            .get_spec(connector)
            .and_then(|s| find_collection_spec_in(&s.collections, collection).cloned())
            .and_then(|c| c.aggregate)
            .unwrap_or(false)
    }

    /// Read all resources in an aggregate collection, concatenated with `---` separators.
    pub(crate) fn read_aggregate_collection(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
    ) -> Result<String, VfsError> {
        let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
        let items = match rt.block_on(conn.list_resources_with_content(collection)) {
            Ok(items) => items,
            Err(_) => {
                // Parent resource may not exist in the API yet (draft-only).
                return Ok(String::new());
            }
        };

        // Populate metadata cache so readdir can use it without a second request.
        let cache_key = format!("{}/{}", connector, collection);
        if self.cache.get_metadata(&cache_key).is_none() {
            let metas: Vec<_> = items.iter().map(|(m, _)| m.clone()).collect();
            self.cache.put_metadata(&cache_key, metas);
        }

        let mut out = String::new();
        for (i, (_, content)) in items.iter().enumerate() {
            if i > 0 {
                out.push_str("\n---\n\n");
            }
            out.push_str(std::str::from_utf8(content).unwrap_or(""));
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }

        Ok(out)
    }
}

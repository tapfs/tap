//! Platform-agnostic virtual filesystem.
//!
//! Contains ALL the filesystem logic previously in `fs/ops.rs`, but using
//! VFS types instead of fuser types.  This module has ZERO dependency on fuser.

use std::sync::Arc;

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cache::store::Cache;
use crate::connector::registry::ConnectorRegistry;
use crate::connector::traits::{CollectionInfo, ResourceMeta};
use crate::draft::store::DraftStore;
use crate::governance::audit::AuditLogger;
use crate::version::store::VersionStore;

use super::types::*;

/// Static help text returned when reading `agent.md`.
pub const AGENT_MD_CONTENT: &str = r#"---
title: tapfs -- agent help
---

# tapfs

This is a FUSE filesystem that mounts REST APIs as readable/writable files.

## Directory layout

```
/                          Root directory
/agent.md                  This help file
/<connector>/              One directory per configured API connector
/<connector>/<collection>/ One directory per collection (endpoint group)
/<connector>/<collection>/<slug>.md          Live resource (read from API)
/<connector>/<collection>/<slug>.draft.md    Draft (local edits, not yet pushed)
/<connector>/<collection>/<slug>.lock        Lock file (prevents concurrent edits)
```

## Workflow

1. **Read** a resource by opening `<slug>.md`.
2. **Edit** by creating `<slug>.draft.md` and writing your changes.
3. **Promote** by renaming `<slug>.draft.md` to `<slug>.md` (pushes to API).
4. **Lock** by creating `<slug>.lock` before editing to prevent conflicts.
5. **Unlock** by deleting `<slug>.lock` when done.

All operations are audit-logged.
"#;

// ---------------------------------------------------------------------------
// Node table
// ---------------------------------------------------------------------------

/// Thread-safe node allocation table.
///
/// Maps node IDs (u64) to their [`NodeKind`] descriptors.
/// The root node is always ID 1 and is pre-allocated at construction time.
pub struct NodeTable {
    /// Forward map: node ID -> kind.
    entries: DashMap<u64, NodeKind>,
    /// Reverse map: kind -> node ID, for fast lookup.
    reverse: DashMap<NodeKind, u64>,
    /// Monotonically increasing counter for allocating new node IDs.
    next_id: AtomicU64,
}

impl NodeTable {
    /// Create a new node table with the root node (ID 1) pre-allocated.
    pub fn new() -> Self {
        let table = Self {
            entries: DashMap::new(),
            reverse: DashMap::new(),
            next_id: AtomicU64::new(2), // 1 is reserved for root
        };
        table.entries.insert(1, NodeKind::Root);
        table.reverse.insert(NodeKind::Root, 1);
        table
    }

    /// Allocate a new node ID for the given kind.
    ///
    /// If the kind already has a node ID, returns the existing one.
    /// Otherwise, assigns the next available ID.
    pub fn allocate(&self, kind: NodeKind) -> u64 {
        // Check reverse map first.
        if let Some(existing) = self.reverse.get(&kind) {
            return *existing;
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries.insert(id, kind.clone());
        // Use `or_insert` to handle the (rare) race where another thread
        // allocated the same kind between our check and insert.
        let actual = *self.reverse.entry(kind).or_insert(id);
        if actual != id {
            // Another thread won the race; clean up our allocation.
            self.entries.remove(&id);
        }
        actual
    }

    /// Look up the node ID for a kind, if it has already been allocated.
    pub fn lookup(&self, kind: &NodeKind) -> Option<u64> {
        self.reverse.get(kind).map(|r| *r)
    }

    /// Get the kind associated with a node ID.
    pub fn get(&self, id: u64) -> Option<NodeKind> {
        self.entries.get(&id).map(|r| r.clone())
    }

    /// Remove a node and its reverse mapping.
    pub fn remove(&self, id: u64) -> Option<NodeKind> {
        if let Some((_, kind)) = self.entries.remove(&id) {
            self.reverse.remove(&kind);
            Some(kind)
        } else {
            None
        }
    }
}

impl Default for NodeTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// VirtualFs
// ---------------------------------------------------------------------------

/// The platform-agnostic virtual filesystem.
///
/// Contains all filesystem logic (lookup, readdir, read, write, create,
/// rename, unlink). Transport layers (FUSE, File Provider Extension, CLI)
/// delegate to this struct and convert between their native types and VFS types.
pub struct VirtualFs {
    pub nodes: Arc<NodeTable>,
    pub registry: Arc<ConnectorRegistry>,
    pub cache: Arc<Cache>,
    pub drafts: Arc<DraftStore>,
    pub versions: Arc<VersionStore>,
    pub audit: Arc<AuditLogger>,
    /// In-memory write buffers, keyed by inode ID. Flushed to draft store on
    /// flush/release so that small, repeated FUSE `write()` calls (e.g. 4 KB
    /// chunks) accumulate in RAM instead of doing O(n^2) read-modify-write
    /// cycles on disk.
    write_buffers: DashMap<u64, Vec<u8>>,
}

impl VirtualFs {
    pub fn new(
        registry: Arc<ConnectorRegistry>,
        cache: Arc<Cache>,
        drafts: Arc<DraftStore>,
        versions: Arc<VersionStore>,
        audit: Arc<AuditLogger>,
    ) -> Self {
        Self {
            nodes: Arc::new(NodeTable::new()),
            registry,
            cache,
            drafts,
            versions,
            audit,
            write_buffers: DashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // lookup
    // -----------------------------------------------------------------------

    /// Look up a child node by parent ID and name.
    pub fn lookup(
        &self,
        rt: &tokio::runtime::Handle,
        parent_id: u64,
        name: &str,
    ) -> Result<VfsAttr, VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;

        let child_kind = match &parent_kind {
            NodeKind::Root => self.resolve_root_child(name)?,
            NodeKind::Connector { name: conn } => {
                self.resolve_connector_child(rt, conn, name)?
            }
            NodeKind::Collection {
                connector,
                collection,
            } => self.resolve_collection_child(rt, connector, collection, name)?,
            NodeKind::TxDir { connector, collection } => {
                // Looking up a named transaction
                NodeKind::Transaction {
                    connector: connector.clone(),
                    collection: collection.clone(),
                    tx_name: name.to_string(),
                }
            }
            NodeKind::Transaction { connector, collection, tx_name } => {
                // Looking up a file inside a transaction
                let resource = name.strip_suffix(".md").unwrap_or(name);
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                if self.drafts.has_draft(connector, collection, &tx_slug) {
                    NodeKind::TxResource {
                        connector: connector.clone(),
                        collection: collection.clone(),
                        tx_name: tx_name.clone(),
                        resource: resource.to_string(),
                    }
                } else {
                    return Err(VfsError::NotFound);
                }
            }
            _ => return Err(VfsError::NotDirectory),
        };

        let id = self.nodes.allocate(child_kind.clone());
        let attr = self.kind_to_attr(id, &child_kind);
        Ok(attr)
    }

    // -----------------------------------------------------------------------
    // getattr
    // -----------------------------------------------------------------------

    /// Get attributes of a node.
    pub fn getattr(&self, id: u64) -> Result<VfsAttr, VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        Ok(self.kind_to_attr(id, &kind))
    }

    // -----------------------------------------------------------------------
    // readdir
    // -----------------------------------------------------------------------

    /// List directory contents.
    pub fn readdir(
        &self,
        rt: &tokio::runtime::Handle,
        id: u64,
    ) -> Result<Vec<VfsDirEntry>, VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        match &kind {
            NodeKind::Root => self.readdir_root(id),
            NodeKind::Connector { name } => self.readdir_connector(rt, id, name),
            NodeKind::Collection {
                connector,
                collection,
            } => self.readdir_collection(rt, id, connector, collection),
            NodeKind::TxDir { connector, collection } => {
                self.readdir_tx_dir(id, connector, collection)
            }
            NodeKind::Transaction { connector, collection, tx_name } => {
                self.readdir_transaction(id, connector, collection, tx_name)
            }
            _ => Err(VfsError::NotDirectory),
        }
    }

    // -----------------------------------------------------------------------
    // read
    // -----------------------------------------------------------------------

    /// Read file content at offset.
    pub fn read(
        &self,
        rt: &tokio::runtime::Handle,
        id: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, VfsError> {
        // If there is a pending write buffer for this inode, serve reads from
        // it so that a write-then-read sequence within the same open/close
        // cycle sees the buffered data without a round-trip to disk.
        if let Some(buf) = self.write_buffers.get(&id) {
            let data = buf.value();
            let off = offset as usize;
            if off >= data.len() {
                return Ok(Vec::new());
            }
            let end = std::cmp::min(off + size as usize, data.len());
            return Ok(data[off..end].to_vec());
        }

        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        let data = match &kind {
            NodeKind::AgentMd => AGENT_MD_CONTENT.as_bytes().to_vec(),
            NodeKind::ConnectorAgentMd { connector } => {
                self.generate_connector_agent_md(rt, connector).into_bytes()
            }
            NodeKind::CollectionAgentMd { connector, collection } => {
                self.generate_collection_agent_md(rt, connector, collection).into_bytes()
            }
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => self.read_resource_data(rt, connector, collection, resource, variant)?,
            NodeKind::Version { connector, collection, resource, version_id } => {
                if let Some(v) = version_id {
                    self.versions
                        .read_version(connector, collection, resource, *v as u32)
                        .map_err(|e| VfsError::IoError(e.to_string()))?
                        .ok_or(VfsError::NotFound)?
                } else {
                    return Err(VfsError::NotFound);
                }
            }
            NodeKind::TxResource { connector, collection, tx_name, resource } => {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                self.drafts
                    .read_draft(connector, collection, &tx_slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .ok_or(VfsError::NotFound)?
            }
            _ => return Err(VfsError::IsDirectory),
        };

        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(Vec::new());
        }
        let end = std::cmp::min(offset + size as usize, data.len());
        Ok(data[offset..end].to_vec())
    }

    // -----------------------------------------------------------------------
    // write
    // -----------------------------------------------------------------------

    /// Write data to a file at offset. Returns bytes written.
    ///
    /// Data is buffered in memory (keyed by inode ID) rather than flushed to
    /// the draft store on every call.  The buffer is written to disk when
    /// [`flush()`] is called (typically on file close).  This turns the
    /// previous O(n^2) read-modify-write pattern into O(n) for sequential
    /// writes.
    pub fn write(
        &self,
        id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        match &kind {
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => {
                let slug = match variant {
                    ResourceVariant::Lock => lock_slug(resource),
                    _ => resource.clone(),
                };

                self.buffer_write(id, connector, collection, &slug, offset, data)?;

                let _ = self.audit.record(
                    "write",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!(
                        "{} bytes at offset {} to {:?}",
                        data.len(),
                        offset,
                        variant
                    )),
                );

                Ok(data.len() as u32)
            }
            NodeKind::TxResource { connector, collection, tx_name, resource } => {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                self.buffer_write(id, connector, collection, &tx_slug, offset, data)?;

                let _ = self.audit.record(
                    "write_tx",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!(
                        "{} bytes at offset {} in tx={}",
                        data.len(),
                        offset,
                        tx_name
                    )),
                );

                Ok(data.len() as u32)
            }
            _ => Err(VfsError::PermissionDenied),
        }
    }

    // -----------------------------------------------------------------------
    // create
    // -----------------------------------------------------------------------

    /// Create a new file (draft or lock).
    pub fn create(
        &self,
        parent_id: u64,
        name: &str,
    ) -> Result<VfsAttr, VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;

        // Handle creating files inside a transaction
        if let NodeKind::Transaction { connector, collection, tx_name } = &parent_kind {
            let resource = name.strip_suffix(".md").unwrap_or(name);
            let tx_slug = format!("__tx_{}_{}", tx_name, resource);
            if !self.drafts.has_draft(&connector, &collection, &tx_slug) {
                // Try to pre-populate from live content (copy-on-write)
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                let initial_content = if let Some(cached) = self.cache.get_resource(&cache_key) {
                    cached.data.to_vec()
                } else {
                    vec![]
                };
                self.drafts
                    .create_draft(&connector, &collection, &tx_slug, &initial_content)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
            let _ = self.audit.record(
                "create_tx_resource",
                &connector,
                Some(&collection),
                Some(resource),
                "success",
                Some(format!("tx={}", tx_name)),
            );
            let kind = NodeKind::TxResource {
                connector: connector.clone(),
                collection: collection.clone(),
                tx_name: tx_name.clone(),
                resource: resource.to_string(),
            };
            let id = self.nodes.allocate(kind);
            return Ok(VfsAttr {
                id,
                size: 0,
                file_type: VfsFileType::RegularFile,
                perm: 0o644,
                mtime: None,
            });
        }

        let (connector, collection) = match &parent_kind {
            NodeKind::Collection {
                connector,
                collection,
            } => (connector.clone(), collection.clone()),
            _ => return Err(VfsError::PermissionDenied),
        };

        let (slug, variant) = parse_resource_filename(name)?;

        match variant {
            ResourceVariant::Draft => {
                if !self.drafts.has_draft(&connector, &collection, &slug) {
                    // Try to pre-populate from live content (copy-on-write)
                    let cache_key = format!("{}/{}/{}", connector, collection, slug);
                    let initial_content = if let Some(cached) = self.cache.get_resource(&cache_key) {
                        cached.data.to_vec()
                    } else {
                        vec![]
                    };
                    self.drafts
                        .create_draft(&connector, &collection, &slug, &initial_content)
                        .map_err(|e| VfsError::IoError(e.to_string()))?;
                }
                let _ = self.audit.record(
                    "create_draft",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    None,
                );
            }
            ResourceVariant::Lock => {
                let lslug = lock_slug(&slug);
                if self.drafts.has_draft(&connector, &collection, &lslug) {
                    return Err(VfsError::AlreadyExists);
                }
                let lock_content = format!("locked_at: {}\n", chrono::Utc::now().to_rfc3339());
                self.drafts
                    .create_draft(&connector, &collection, &lslug, lock_content.as_bytes())
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
                let _ = self.audit.record(
                    "lock",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    None,
                );
            }
            ResourceVariant::Live => {
                // Allow creating .md files directly — buffer as a draft.
                // Auto-promote happens on flush/release.
                if !self.drafts.has_draft(&connector, &collection, &slug) {
                    self.drafts
                        .create_draft(&connector, &collection, &slug, &[])
                        .map_err(|e| VfsError::IoError(e.to_string()))?;
                }
                let _ = self.audit.record(
                    "create_live",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    Some("buffered as draft, will promote on close".to_string()),
                );
            }
        }

        // For live files, we store as draft but present as live to the caller.
        let actual_variant = if variant == ResourceVariant::Live && self.drafts.has_draft(&connector, &collection, &slug) {
            ResourceVariant::Live // keep it as Live in the inode table
        } else {
            variant
        };

        let kind = NodeKind::Resource {
            connector,
            collection,
            resource: slug,
            variant: actual_variant,
        };
        let id = self.nodes.allocate(kind);
        Ok(VfsAttr {
            id,
            size: 0,
            file_type: VfsFileType::RegularFile,
            perm: 0o644,
            mtime: None,
        })
    }

    // -----------------------------------------------------------------------
    // rename (promote draft)
    // -----------------------------------------------------------------------

    /// Rename (promote draft to live).
    pub fn rename(
        &self,
        rt: &tokio::runtime::Handle,
        parent_id: u64,
        old_name: &str,
        new_parent_id: u64,
        new_name: &str,
    ) -> Result<(), VfsError> {
        if parent_id != new_parent_id {
            return Err(VfsError::CrossDevice);
        }

        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        let (connector, collection) = match &parent_kind {
            NodeKind::Collection {
                connector,
                collection,
            } => (connector.clone(), collection.clone()),
            _ => return Err(VfsError::PermissionDenied),
        };

        let (old_slug, old_variant) = parse_resource_filename(old_name)?;
        let (new_slug, new_variant) = parse_resource_filename(new_name)?;

        // The main use case: draft -> live (promote).
        if old_variant == ResourceVariant::Draft
            && new_variant == ResourceVariant::Live
            && old_slug == new_slug
        {
            let data = self
                .drafts
                .read_draft(&connector, &collection, &old_slug)
                .map_err(|e| VfsError::IoError(e.to_string()))?
                .ok_or(VfsError::NotFound)?;

            // Push to API.
            let conn = self.registry.get(&connector).ok_or(VfsError::NotFound)?;
            rt.block_on(conn.write_resource(&collection, &old_slug, &data))
                .map_err(|e| {
                    tracing::error!("promote write_resource error: {}", e);
                    let _ = self.audit.record(
                        "promote",
                        &connector,
                        Some(&collection),
                        Some(&old_slug),
                        "error",
                        Some(e.to_string()),
                    );
                    VfsError::IoError(e.to_string())
                })?;

            // Record a version snapshot.
            let _ = self
                .versions
                .save_snapshot(&connector, &collection, &old_slug, &data);

            // Remove the draft.
            let _ = self.drafts.delete_draft(&connector, &collection, &old_slug);

            // Invalidate the cache so the next read fetches the updated resource.
            let cache_key = format!("{}/{}/{}", connector, collection, old_slug);
            self.cache.invalidate(&cache_key);

            // Remove the draft node.
            let draft_kind = NodeKind::Resource {
                connector: connector.clone(),
                collection: collection.clone(),
                resource: old_slug.clone(),
                variant: ResourceVariant::Draft,
            };
            if let Some(draft_id) = self.nodes.lookup(&draft_kind) {
                self.nodes.remove(draft_id);
            }

            let _ = self.audit.record(
                "promote",
                &connector,
                Some(&collection),
                Some(&old_slug),
                "success",
                None,
            );

            return Ok(());
        }

        Err(VfsError::NotSupported)
    }

    // -----------------------------------------------------------------------
    // unlink
    // -----------------------------------------------------------------------

    /// Delete a file (draft or lock).
    pub fn unlink(
        &self,
        parent_id: u64,
        name: &str,
    ) -> Result<(), VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;

        // Delegate to unlink_tx for transaction-related parents
        match &parent_kind {
            NodeKind::Transaction { .. } | NodeKind::TxDir { .. } => {
                return self.unlink_tx(parent_id, name);
            }
            _ => {}
        }

        let (connector, collection) = match &parent_kind {
            NodeKind::Collection {
                connector,
                collection,
            } => (connector.clone(), collection.clone()),
            _ => return Err(VfsError::PermissionDenied),
        };

        let (slug, variant) = parse_resource_filename(name)?;

        match variant {
            ResourceVariant::Draft => {
                let deleted = self
                    .drafts
                    .delete_draft(&connector, &collection, &slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
                if !deleted {
                    return Err(VfsError::NotFound);
                }
                let _ = self.audit.record(
                    "delete",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    Some("draft removed".to_string()),
                );
            }
            ResourceVariant::Lock => {
                let lslug = lock_slug(&slug);
                let deleted = self
                    .drafts
                    .delete_draft(&connector, &collection, &lslug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
                if !deleted {
                    return Err(VfsError::NotFound);
                }
                let _ = self.audit.record(
                    "unlock",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    None,
                );
            }
            ResourceVariant::Live => {
                return Err(VfsError::PermissionDenied);
            }
        }

        // Remove the node.
        let kind = NodeKind::Resource {
            connector,
            collection,
            resource: slug,
            variant,
        };
        if let Some(id) = self.nodes.lookup(&kind) {
            self.nodes.remove(id);
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // flush (auto-promote live files with pending writes)
    // -----------------------------------------------------------------------

    /// Flush a file.
    ///
    /// 1. If there is an in-memory write buffer for this inode, persist it to
    ///    the draft store (single write, not read-modify-write).
    /// 2. For live files with pending draft content, auto-promote (push to API
    ///    and clean up the draft).
    pub fn flush(
        &self,
        rt: &tokio::runtime::Handle,
        id: u64,
    ) -> Result<(), VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;

        // Step 1: flush the in-memory write buffer to the draft store.
        if let Some((_, buf)) = self.write_buffers.remove(&id) {
            if let NodeKind::Resource { connector, collection, resource, variant } = &kind {
                let slug = match variant {
                    ResourceVariant::Lock => lock_slug(resource),
                    _ => resource.clone(),
                };
                self.drafts
                    .write_draft(connector, collection, &slug, &buf)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            } else if let NodeKind::TxResource { connector, collection, tx_name, resource } = &kind {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                self.drafts
                    .write_draft(connector, collection, &tx_slug, &buf)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
        }

        // Step 2: auto-promote live files that have a draft on disk.
        if let NodeKind::Resource {
            connector,
            collection,
            resource,
            variant: ResourceVariant::Live,
        } = &kind
        {
            if self.drafts.has_draft(connector, collection, resource) {
                let data = self
                    .drafts
                    .read_draft(connector, collection, resource)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .ok_or(VfsError::NotFound)?;

                // Push to API
                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                rt.block_on(conn.write_resource(collection, resource, &data))
                    .map_err(|e| {
                        tracing::error!("auto-promote error: {}", e);
                        VfsError::IoError(e.to_string())
                    })?;

                // Snapshot + cleanup
                let _ = self.versions.save_snapshot(connector, collection, resource, &data);
                let _ = self.drafts.delete_draft(connector, collection, resource);
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                self.cache.invalidate(&cache_key);

                let _ = self.audit.record(
                    "auto-promote",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!("{} bytes pushed to API on close", data.len())),
                );

                tracing::info!(
                    connector = %connector,
                    resource = %resource,
                    "auto-promoted live file on close"
                );
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // truncate
    // -----------------------------------------------------------------------

    /// Truncate (or extend) a file to `new_size` bytes.
    ///
    /// Operates on the in-memory write buffer when one exists, otherwise falls
    /// through to the draft store on disk.
    pub fn truncate(&self, id: u64, new_size: u64) -> Result<(), VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        let new_len = new_size as usize;

        // If there is a write buffer, truncate/extend it in place.
        if let Some(mut buf) = self.write_buffers.get_mut(&id) {
            buf.value_mut().resize(new_len, 0);
            return Ok(());
        }

        // No write buffer -- apply to the draft store directly.
        match &kind {
            NodeKind::Resource { connector, collection, resource, variant } => {
                let slug = match variant {
                    ResourceVariant::Lock => lock_slug(resource),
                    _ => resource.clone(),
                };
                let mut data = self.drafts
                    .read_draft(connector, collection, &slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .unwrap_or_default();
                data.resize(new_len, 0);
                self.drafts
                    .write_draft(connector, collection, &slug, &data)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
            NodeKind::TxResource { connector, collection, tx_name, resource } => {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                let mut data = self.drafts
                    .read_draft(connector, collection, &tx_slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .unwrap_or_default();
                data.resize(new_len, 0);
                self.drafts
                    .write_draft(connector, collection, &tx_slug, &data)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
            _ => {} // ignore truncation on non-file nodes
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Buffer a write in memory.  On the first write to an inode the buffer is
    /// seeded from the existing draft on disk (if any) so that partial
    /// overwrites preserve the untouched prefix/suffix.
    fn buffer_write(
        &self,
        id: u64,
        connector: &str,
        collection: &str,
        slug: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), VfsError> {
        let mut entry = self.write_buffers.entry(id).or_insert_with(|| {
            // Seed from existing draft content (if any) so that a partial
            // overwrite doesn't lose bytes outside the written range.
            self.drafts
                .read_draft(connector, collection, slug)
                .ok()
                .flatten()
                .unwrap_or_default()
        });
        let buf = entry.value_mut();
        let off = offset as usize;
        let needed = off + data.len();
        if buf.len() < needed {
            buf.resize(needed, 0);
        }
        buf[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn kind_to_attr(&self, id: u64, kind: &NodeKind) -> VfsAttr {
        match kind {
            NodeKind::Root | NodeKind::Connector { .. } | NodeKind::Collection { .. } => {
                VfsAttr {
                    id,
                    size: 0,
                    file_type: VfsFileType::Directory,
                    perm: 0o755,
                    mtime: None,
                }
            }
            NodeKind::TxDir { .. } | NodeKind::Transaction { .. } => {
                VfsAttr {
                    id,
                    size: 0,
                    file_type: VfsFileType::Directory,
                    perm: 0o755,
                    mtime: None,
                }
            }
            NodeKind::AgentMd => VfsAttr {
                id,
                size: AGENT_MD_CONTENT.len() as u64,
                file_type: VfsFileType::RegularFile,
                perm: 0o644,
                mtime: None,
            },
            NodeKind::ConnectorAgentMd { .. } => VfsAttr {
                id,
                size: 4096,
                file_type: VfsFileType::RegularFile,
                perm: 0o644,
                mtime: None,
            },
            NodeKind::CollectionAgentMd { .. } => VfsAttr {
                id,
                size: 4096,
                file_type: VfsFileType::RegularFile,
                perm: 0o644,
                mtime: None,
            },
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => {
                // Check write buffer first for accurate size during writes.
                let size = if let Some(buf) = self.write_buffers.get(&id) {
                    buf.value().len() as u64
                } else {
                    self.resource_size(connector, collection, resource, variant)
                };
                let perm = match variant {
                    ResourceVariant::Lock => 0o444,
                    _ => 0o644,
                };
                VfsAttr {
                    id,
                    size,
                    file_type: VfsFileType::RegularFile,
                    perm,
                    mtime: None,
                }
            }
            NodeKind::Version { connector, collection, resource, version_id } => {
                let size = if let Some(v) = version_id {
                    self.versions.read_version(connector, collection, resource, *v as u32)
                        .ok()
                        .flatten()
                        .map(|d| d.len() as u64)
                        .unwrap_or(0)
                } else {
                    0
                };
                VfsAttr {
                    id,
                    size,
                    file_type: VfsFileType::RegularFile,
                    perm: 0o444, // read-only, immutable
                    mtime: None,
                }
            }
            NodeKind::TxResource { connector, collection, tx_name, resource } => {
                // Check write buffer first for accurate size during writes.
                let size = if let Some(buf) = self.write_buffers.get(&id) {
                    buf.value().len() as u64
                } else {
                    let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                    self.drafts
                        .draft_size(connector, collection, &tx_slug)
                        .unwrap_or(0)
                };
                VfsAttr {
                    id,
                    size,
                    file_type: VfsFileType::RegularFile,
                    perm: 0o644,
                    mtime: None,
                }
            }
        }
    }

    fn resolve_root_child(&self, name: &str) -> Result<NodeKind, VfsError> {
        if name == "agent.md" {
            return Ok(NodeKind::AgentMd);
        }
        let connectors = self.registry.list();
        if connectors.iter().any(|c| c == name) {
            return Ok(NodeKind::Connector {
                name: name.to_string(),
            });
        }
        Err(VfsError::NotFound)
    }

    fn resolve_connector_child(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        name: &str,
    ) -> Result<NodeKind, VfsError> {
        if name == "agent.md" {
            return Ok(NodeKind::ConnectorAgentMd { connector: connector.to_string() });
        }
        let collections = self.get_collections_cached(rt, connector)?;
        if collections.iter().any(|c| c.name == name) {
            return Ok(NodeKind::Collection {
                connector: connector.to_string(),
                collection: name.to_string(),
            });
        }
        Err(VfsError::NotFound)
    }

    /// Fetch resources for a collection, using the metadata cache.
    fn get_resources_cached(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
    ) -> Result<Vec<ResourceMeta>, VfsError> {
        let cache_key = format!("{}/{}", connector, collection);
        if let Some(cached) = self.cache.get_metadata(&cache_key) {
            return Ok(cached);
        }
        let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
        let resources = rt
            .block_on(conn.list_resources(collection))
            .map_err(|e| VfsError::IoError(e.to_string()))?;
        self.cache.put_metadata(&cache_key, resources.clone());
        Ok(resources)
    }

    fn get_collections_cached(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
    ) -> Result<Vec<CollectionInfo>, VfsError> {
        let cache_key = format!("{}/__collections__", connector);
        if let Some(cached) = self.cache.get_metadata(&cache_key) {
            return Ok(cached
                .iter()
                .map(|r| CollectionInfo {
                    name: r.slug.clone(),
                    description: r.title.clone(),
                })
                .collect());
        }
        let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
        let cols = rt
            .block_on(conn.list_collections())
            .map_err(|e| VfsError::IoError(e.to_string()))?;
        let meta: Vec<crate::connector::traits::ResourceMeta> = cols
            .iter()
            .map(|c| crate::connector::traits::ResourceMeta {
                id: c.name.clone(),
                slug: c.name.clone(),
                title: c.description.clone(),
                updated_at: None,
                content_type: None,
            })
            .collect();
        self.cache.put_metadata(&cache_key, meta);
        Ok(cols)
    }

    fn resolve_collection_child(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
        name: &str,
    ) -> Result<NodeKind, VfsError> {
        if name == "agent.md" {
            return Ok(NodeKind::CollectionAgentMd {
                connector: connector.to_string(),
                collection: collection.to_string(),
            });
        }

        // Handle .tx directory
        if name == ".tx" {
            return Ok(NodeKind::TxDir {
                connector: connector.to_string(),
                collection: collection.to_string(),
            });
        }

        // Check for version access: resource@vN.md
        if let Some(without_md) = name.strip_suffix(".md") {
            if let Some(at_pos) = without_md.rfind("@v") {
                let base = &without_md[..at_pos];
                let ver_str = &without_md[at_pos + 2..];
                if !base.is_empty() {
                    if let Ok(v) = ver_str.parse::<u32>() {
                        return Ok(NodeKind::Version {
                            connector: connector.to_string(),
                            collection: collection.to_string(),
                            resource: base.to_string(),
                            version_id: Some(v as u64),
                        });
                    }
                }
            }
        }

        let (slug, variant) = parse_resource_filename(name)?;

        // For drafts, check the draft store.
        if variant == ResourceVariant::Draft {
            if self.drafts.has_draft(connector, collection, &slug) {
                return Ok(NodeKind::Resource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: slug,
                    variant: ResourceVariant::Draft,
                });
            }
            return Err(VfsError::NotFound);
        }

        // For lock files.
        if variant == ResourceVariant::Lock {
            let lslug = lock_slug(&slug);
            if self.drafts.has_draft(connector, collection, &lslug) {
                return Ok(NodeKind::Resource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: slug,
                    variant: ResourceVariant::Lock,
                });
            }
            return Err(VfsError::NotFound);
        }

        // Live resource -- check cache first, then API.
        let resources = self.get_resources_cached(rt, connector, collection)?;

        if resources.iter().any(|r| r.slug == slug || r.id == slug) {
            return Ok(NodeKind::Resource {
                connector: connector.to_string(),
                collection: collection.to_string(),
                resource: slug,
                variant: ResourceVariant::Live,
            });
        }

        Err(VfsError::NotFound)
    }

    fn resource_size(
        &self,
        connector: &str,
        collection: &str,
        resource: &str,
        variant: &ResourceVariant,
    ) -> u64 {
        match variant {
            ResourceVariant::Draft => self
                .drafts
                .draft_size(connector, collection, resource)
                .unwrap_or(0),
            ResourceVariant::Lock => {
                let lslug = lock_slug(resource);
                self.drafts
                    .draft_size(connector, collection, &lslug)
                    .unwrap_or(0)
            }
            ResourceVariant::Live => {
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                if let Some(cached) = self.cache.get_resource(&cache_key) {
                    return cached.data.len() as u64;
                }
                4096
            }
        }
    }

    fn readdir_root(&self, self_id: u64) -> Result<Vec<VfsDirEntry>, VfsError> {
        let mut entries = vec![
            VfsDirEntry {
                name: ".".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
            VfsDirEntry {
                name: "..".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
        ];

        // agent.md
        let agent_id = self.nodes.allocate(NodeKind::AgentMd);
        entries.push(VfsDirEntry {
            name: "agent.md".to_string(),
            id: agent_id,
            file_type: VfsFileType::RegularFile,
        });

        // Connectors
        for name in self.registry.list() {
            let kind = NodeKind::Connector { name: name.clone() };
            let id = self.nodes.allocate(kind);
            entries.push(VfsDirEntry {
                name,
                id,
                file_type: VfsFileType::Directory,
            });
        }

        Ok(entries)
    }

    fn readdir_connector(
        &self,
        rt: &tokio::runtime::Handle,
        self_id: u64,
        connector: &str,
    ) -> Result<Vec<VfsDirEntry>, VfsError> {
        let mut entries = vec![
            VfsDirEntry {
                name: ".".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
            VfsDirEntry {
                name: "..".to_string(),
                id: 1, // parent is root
                file_type: VfsFileType::Directory,
            },
        ];

        // agent.md for this connector
        let agent_kind = NodeKind::ConnectorAgentMd { connector: connector.to_string() };
        let agent_id = self.nodes.allocate(agent_kind);
        entries.push(VfsDirEntry {
            name: "agent.md".to_string(),
            id: agent_id,
            file_type: VfsFileType::RegularFile,
        });

        let collections = self.get_collections_cached(rt, connector)?;

        for col in collections {
            let kind = NodeKind::Collection {
                connector: connector.to_string(),
                collection: col.name.clone(),
            };
            let id = self.nodes.allocate(kind);
            entries.push(VfsDirEntry {
                name: col.name,
                id,
                file_type: VfsFileType::Directory,
            });
        }

        Ok(entries)
    }

    fn readdir_collection(
        &self,
        rt: &tokio::runtime::Handle,
        self_id: u64,
        connector: &str,
        collection: &str,
    ) -> Result<Vec<VfsDirEntry>, VfsError> {
        let parent_id = self
            .nodes
            .lookup(&NodeKind::Connector {
                name: connector.to_string(),
            })
            .unwrap_or(1);

        let mut entries = vec![
            VfsDirEntry {
                name: ".".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
            VfsDirEntry {
                name: "..".to_string(),
                id: parent_id,
                file_type: VfsFileType::Directory,
            },
        ];

        // agent.md for this collection
        let agent_kind = NodeKind::CollectionAgentMd {
            connector: connector.to_string(),
            collection: collection.to_string(),
        };
        let agent_id = self.nodes.allocate(agent_kind);
        entries.push(VfsDirEntry {
            name: "agent.md".to_string(),
            id: agent_id,
            file_type: VfsFileType::RegularFile,
        });

        // Fetch live resources (cached).
        let resources = self.get_resources_cached(rt, connector, collection)?;

        for res in &resources {
            // Live resource file
            let filename = format!("{}.md", res.slug);
            let kind = NodeKind::Resource {
                connector: connector.to_string(),
                collection: collection.to_string(),
                resource: res.slug.clone(),
                variant: ResourceVariant::Live,
            };
            let id = self.nodes.allocate(kind);
            entries.push(VfsDirEntry {
                name: filename,
                id,
                file_type: VfsFileType::RegularFile,
            });

            // If a draft exists for this resource, list it too.
            if self.drafts.has_draft(connector, collection, &res.slug) {
                let draft_filename = format!("{}.draft.md", res.slug);
                let draft_kind = NodeKind::Resource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: res.slug.clone(),
                    variant: ResourceVariant::Draft,
                };
                let draft_id = self.nodes.allocate(draft_kind);
                entries.push(VfsDirEntry {
                    name: draft_filename,
                    id: draft_id,
                    file_type: VfsFileType::RegularFile,
                });
            }

            // If a lock exists for this resource, list it too.
            let lslug = lock_slug(&res.slug);
            if self.drafts.has_draft(connector, collection, &lslug) {
                let lock_filename = format!("{}.lock", res.slug);
                let lock_kind = NodeKind::Resource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: res.slug.clone(),
                    variant: ResourceVariant::Lock,
                };
                let lock_id = self.nodes.allocate(lock_kind);
                entries.push(VfsDirEntry {
                    name: lock_filename,
                    id: lock_id,
                    file_type: VfsFileType::RegularFile,
                });
            }

            // List version files for this resource
            let versions = self.versions.list_versions(connector, collection, &res.slug)
                .unwrap_or_default();
            for v in versions {
                let ver_filename = format!("{}@v{}.md", res.slug, v);
                let ver_kind = NodeKind::Version {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: res.slug.clone(),
                    version_id: Some(v as u64),
                };
                let ver_id = self.nodes.allocate(ver_kind);
                entries.push(VfsDirEntry {
                    name: ver_filename,
                    id: ver_id,
                    file_type: VfsFileType::RegularFile,
                });
            }
        }

        // Also list draft-only resources.
        if let Ok(draft_slugs) = self.drafts.list_drafts(connector, collection) {
            let live_slugs: std::collections::HashSet<&str> =
                resources.iter().map(|r| r.slug.as_str()).collect();
            for slug in draft_slugs {
                if slug.ends_with(".lock") || slug.starts_with("__tx_") || live_slugs.contains(slug.as_str()) {
                    continue;
                }
                let draft_filename = format!("{}.draft.md", slug);
                let draft_kind = NodeKind::Resource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    resource: slug.clone(),
                    variant: ResourceVariant::Draft,
                };
                let draft_id = self.nodes.allocate(draft_kind);
                entries.push(VfsDirEntry {
                    name: draft_filename,
                    id: draft_id,
                    file_type: VfsFileType::RegularFile,
                });
            }
        }

        // Add .tx directory entry
        let tx_kind = NodeKind::TxDir {
            connector: connector.to_string(),
            collection: collection.to_string(),
        };
        let tx_id = self.nodes.allocate(tx_kind);
        entries.push(VfsDirEntry {
            name: ".tx".to_string(),
            id: tx_id,
            file_type: VfsFileType::Directory,
        });

        Ok(entries)
    }

    fn generate_connector_agent_md(&self, rt: &tokio::runtime::Handle, connector: &str) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", connector));

        // List collections
        if let Ok(collections) = self.get_collections_cached(rt, connector) {
            out.push_str("## Collections\n\n");
            for col in &collections {
                out.push_str(&format!("- **{}/**", col.name));
                if let Some(ref desc) = col.description {
                    out.push_str(&format!(" — {}", desc));
                }
                out.push('\n');
            }
        }

        out.push_str("\n## Workflow\n\n");
        out.push_str("```bash\n");
        out.push_str(&format!("ls /mnt/tap/{}/           # list collections\n", connector));
        out.push_str(&format!("ls /mnt/tap/{}/drive/     # list resources\n", connector));
        out.push_str(&format!("cat /mnt/tap/{}/drive/file.md  # read a resource\n", connector));
        out.push_str("```\n");

        out
    }

    fn generate_collection_agent_md(&self, rt: &tokio::runtime::Handle, connector: &str, collection: &str) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str(&format!("collection: {}\n", collection));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}/{}\n\n", connector, collection));

        // List some resources
        if let Ok(resources) = self.get_resources_cached(rt, connector, collection) {
            out.push_str(&format!("**{} resources available.**\n\n", resources.len()));
            out.push_str("## Sample resources\n\n");
            for res in resources.iter().take(10) {
                out.push_str(&format!("- `{}.md`", res.slug));
                if let Some(ref title) = res.title {
                    out.push_str(&format!(" — {}", title));
                }
                out.push('\n');
            }
            if resources.len() > 10 {
                out.push_str(&format!("\n... and {} more. Use `ls` to see all.\n", resources.len() - 10));
            }
        }

        out.push_str("\n## Operations\n\n");
        out.push_str("```bash\n");
        out.push_str(&format!("cat {}.md         # read resource\n", "resource"));
        out.push_str(&format!("echo 'x' > {}.md  # write (auto-promotes)\n", "resource"));
        out.push_str(&format!("touch {}.draft.md  # create draft\n", "resource"));
        out.push_str(&format!("touch {}.lock      # lock resource\n", "resource"));
        out.push_str("```\n");

        out
    }

    fn read_resource_data(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
        resource: &str,
        variant: &ResourceVariant,
    ) -> Result<Vec<u8>, VfsError> {
        match variant {
            ResourceVariant::Draft => self
                .drafts
                .read_draft(connector, collection, resource)
                .map_err(|e| VfsError::IoError(e.to_string()))?
                .ok_or(VfsError::NotFound),
            ResourceVariant::Lock => {
                let lslug = lock_slug(resource);
                self.drafts
                    .read_draft(connector, collection, &lslug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .ok_or(VfsError::NotFound)
            }
            ResourceVariant::Live => {
                // Check cache first.  Bytes::clone() is O(1) (refcount bump),
                // so pulling from cache no longer deep-copies the whole buffer.
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                if let Some(cached) = self.cache.get_resource(&cache_key) {
                    return Ok(cached.data.to_vec());
                }

                // Fetch from connector.
                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                let result = rt
                    .block_on(conn.read_resource(collection, resource))
                    .map_err(|e| {
                        tracing::error!("read_resource error: {}", e);
                        VfsError::IoError(e.to_string())
                    })?;

                let data = result.content;

                // Cache the result, converting Vec<u8> -> Bytes for O(1) clones.
                self.cache.put_resource(
                    &cache_key,
                    crate::cache::store::Resource {
                        data: bytes::Bytes::from(data.clone()),
                    },
                );

                Ok(data)
            }
        }
    }

    // -----------------------------------------------------------------------
    // mkdir (for transactions)
    // -----------------------------------------------------------------------

    /// Create a directory. Only supported inside `.tx/` (creating named transactions).
    pub fn mkdir(&self, parent_id: u64, name: &str) -> Result<VfsAttr, VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        match &parent_kind {
            NodeKind::TxDir { connector, collection } => {
                // Creating a named transaction
                let kind = NodeKind::Transaction {
                    connector: connector.clone(),
                    collection: collection.clone(),
                    tx_name: name.to_string(),
                };
                let id = self.nodes.allocate(kind);
                let _ = self.audit.record(
                    "create_tx",
                    connector,
                    Some(collection),
                    Some(name),
                    "success",
                    None,
                );
                Ok(VfsAttr {
                    id,
                    size: 0,
                    file_type: VfsFileType::Directory,
                    perm: 0o755,
                    mtime: None,
                })
            }
            _ => Err(VfsError::PermissionDenied),
        }
    }

    // -----------------------------------------------------------------------
    // rmdir (commit transaction)
    // -----------------------------------------------------------------------

    /// Remove a directory. For transactions, this commits (promotes all files).
    pub fn rmdir(
        &self,
        rt: &tokio::runtime::Handle,
        parent_id: u64,
        name: &str,
    ) -> Result<(), VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        match &parent_kind {
            NodeKind::TxDir { connector, collection } => {
                // Committing a transaction: promote all files
                let tx_prefix = format!("__tx_{}_", name);
                let tx_drafts = self
                    .drafts
                    .list_drafts(connector, collection)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;

                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;

                for slug in &tx_drafts {
                    if let Some(resource) = slug.strip_prefix(&tx_prefix) {
                        let data = self
                            .drafts
                            .read_draft(connector, collection, slug)
                            .map_err(|e| VfsError::IoError(e.to_string()))?
                            .ok_or(VfsError::NotFound)?;

                        // Push to API
                        rt.block_on(conn.write_resource(collection, resource, &data))
                            .map_err(|e| VfsError::IoError(e.to_string()))?;

                        // Snapshot + cleanup
                        let _ = self
                            .versions
                            .save_snapshot(connector, collection, resource, &data);
                        let _ = self.drafts.delete_draft(connector, collection, slug);
                        let cache_key =
                            format!("{}/{}/{}", connector, collection, resource);
                        self.cache.invalidate(&cache_key);

                        // Remove the tx resource node
                        let tx_res_kind = NodeKind::TxResource {
                            connector: connector.to_string(),
                            collection: collection.to_string(),
                            tx_name: name.to_string(),
                            resource: resource.to_string(),
                        };
                        if let Some(res_id) = self.nodes.lookup(&tx_res_kind) {
                            self.nodes.remove(res_id);
                        }
                    }
                }

                // Remove transaction node
                let kind = NodeKind::Transaction {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    tx_name: name.to_string(),
                };
                if let Some(tx_id) = self.nodes.lookup(&kind) {
                    self.nodes.remove(tx_id);
                }

                let _ = self.audit.record(
                    "commit_tx",
                    connector,
                    Some(collection),
                    Some(name),
                    "success",
                    None,
                );
                Ok(())
            }
            _ => Err(VfsError::PermissionDenied),
        }
    }

    // -----------------------------------------------------------------------
    // unlink_tx (abort/delete transaction files)
    // -----------------------------------------------------------------------

    /// Delete a file inside a transaction, or abort an entire transaction.
    pub fn unlink_tx(
        &self,
        parent_id: u64,
        name: &str,
    ) -> Result<(), VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        match &parent_kind {
            NodeKind::Transaction { connector, collection, tx_name } => {
                // Deleting a single file inside a transaction
                let resource = name.strip_suffix(".md").unwrap_or(name);
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                let deleted = self
                    .drafts
                    .delete_draft(connector, collection, &tx_slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
                if !deleted {
                    return Err(VfsError::NotFound);
                }
                // Remove the node
                let kind = NodeKind::TxResource {
                    connector: connector.clone(),
                    collection: collection.clone(),
                    tx_name: tx_name.clone(),
                    resource: resource.to_string(),
                };
                if let Some(id) = self.nodes.lookup(&kind) {
                    self.nodes.remove(id);
                }
                let _ = self.audit.record(
                    "delete_tx_resource",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!("tx={}", tx_name)),
                );
                Ok(())
            }
            NodeKind::TxDir { connector, collection } => {
                // Aborting (deleting) an entire transaction
                let tx_prefix = format!("__tx_{}_", name);
                let tx_drafts = self
                    .drafts
                    .list_drafts(connector, collection)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;

                for slug in &tx_drafts {
                    if slug.starts_with(&tx_prefix) {
                        let _ = self.drafts.delete_draft(connector, collection, slug);
                        // Remove the tx resource node
                        if let Some(resource) = slug.strip_prefix(&tx_prefix) {
                            let tx_res_kind = NodeKind::TxResource {
                                connector: connector.to_string(),
                                collection: collection.to_string(),
                                tx_name: name.to_string(),
                                resource: resource.to_string(),
                            };
                            if let Some(res_id) = self.nodes.lookup(&tx_res_kind) {
                                self.nodes.remove(res_id);
                            }
                        }
                    }
                }

                // Remove transaction node
                let kind = NodeKind::Transaction {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    tx_name: name.to_string(),
                };
                if let Some(tx_id) = self.nodes.lookup(&kind) {
                    self.nodes.remove(tx_id);
                }

                let _ = self.audit.record(
                    "abort_tx",
                    connector,
                    Some(collection),
                    Some(name),
                    "success",
                    None,
                );
                Ok(())
            }
            _ => Err(VfsError::PermissionDenied),
        }
    }

    // -----------------------------------------------------------------------
    // readdir helpers for transactions
    // -----------------------------------------------------------------------

    fn readdir_tx_dir(
        &self,
        self_id: u64,
        connector: &str,
        collection: &str,
    ) -> Result<Vec<VfsDirEntry>, VfsError> {
        let parent_id = self
            .nodes
            .lookup(&NodeKind::Collection {
                connector: connector.to_string(),
                collection: collection.to_string(),
            })
            .unwrap_or(1);

        let mut entries = vec![
            VfsDirEntry {
                name: ".".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
            VfsDirEntry {
                name: "..".to_string(),
                id: parent_id,
                file_type: VfsFileType::Directory,
            },
        ];

        // Find all transaction names by scanning drafts with __tx_ prefix
        let draft_slugs = self
            .drafts
            .list_drafts(connector, collection)
            .unwrap_or_default();
        let mut tx_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for slug in &draft_slugs {
            if let Some(rest) = slug.strip_prefix("__tx_") {
                if let Some(underscore_pos) = rest.find('_') {
                    let tx_name = &rest[..underscore_pos];
                    tx_names.insert(tx_name.to_string());
                }
            }
        }

        let mut sorted_names: Vec<String> = tx_names.into_iter().collect();
        sorted_names.sort();

        for tx_name in sorted_names {
            let kind = NodeKind::Transaction {
                connector: connector.to_string(),
                collection: collection.to_string(),
                tx_name: tx_name.clone(),
            };
            let id = self.nodes.allocate(kind);
            entries.push(VfsDirEntry {
                name: tx_name,
                id,
                file_type: VfsFileType::Directory,
            });
        }

        Ok(entries)
    }

    fn readdir_transaction(
        &self,
        self_id: u64,
        connector: &str,
        collection: &str,
        tx_name: &str,
    ) -> Result<Vec<VfsDirEntry>, VfsError> {
        let parent_id = self
            .nodes
            .lookup(&NodeKind::TxDir {
                connector: connector.to_string(),
                collection: collection.to_string(),
            })
            .unwrap_or(1);

        let mut entries = vec![
            VfsDirEntry {
                name: ".".to_string(),
                id: self_id,
                file_type: VfsFileType::Directory,
            },
            VfsDirEntry {
                name: "..".to_string(),
                id: parent_id,
                file_type: VfsFileType::Directory,
            },
        ];

        let tx_prefix = format!("__tx_{}_", tx_name);
        let draft_slugs = self
            .drafts
            .list_drafts(connector, collection)
            .unwrap_or_default();

        for slug in &draft_slugs {
            if let Some(resource) = slug.strip_prefix(&tx_prefix) {
                let filename = format!("{}.md", resource);
                let kind = NodeKind::TxResource {
                    connector: connector.to_string(),
                    collection: collection.to_string(),
                    tx_name: tx_name.to_string(),
                    resource: resource.to_string(),
                };
                let id = self.nodes.allocate(kind);
                entries.push(VfsDirEntry {
                    name: filename,
                    id,
                    file_type: VfsFileType::RegularFile,
                });
            }
        }

        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// The slug used to store a lock in the DraftStore.
fn lock_slug(slug: &str) -> String {
    format!("{}.lock", slug)
}

/// Parse a filename into (resource_slug, ResourceVariant).
fn parse_resource_filename(name: &str) -> Result<(String, ResourceVariant), VfsError> {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_node_is_one() {
        let table = NodeTable::new();
        assert_eq!(table.get(1), Some(NodeKind::Root));
        assert_eq!(table.lookup(&NodeKind::Root), Some(1));
    }

    #[test]
    fn allocate_returns_same_id_for_same_kind() {
        let table = NodeTable::new();
        let kind = NodeKind::Connector {
            name: "test".into(),
        };
        let id1 = table.allocate(kind.clone());
        let id2 = table.allocate(kind);
        assert_eq!(id1, id2);
    }

    #[test]
    fn allocate_different_kinds_get_different_ids() {
        let table = NodeTable::new();
        let k1 = NodeKind::Connector {
            name: "a".into(),
        };
        let k2 = NodeKind::Connector {
            name: "b".into(),
        };
        let i1 = table.allocate(k1);
        let i2 = table.allocate(k2);
        assert_ne!(i1, i2);
    }

    #[test]
    fn remove_cleans_both_maps() {
        let table = NodeTable::new();
        let kind = NodeKind::Connector {
            name: "rm".into(),
        };
        let id = table.allocate(kind.clone());
        assert!(table.get(id).is_some());
        assert!(table.lookup(&kind).is_some());

        table.remove(id);
        assert!(table.get(id).is_none());
        assert!(table.lookup(&kind).is_none());
    }

    #[test]
    fn parse_resource_filename_live() {
        let (slug, variant) = parse_resource_filename("hello.md").unwrap();
        assert_eq!(slug, "hello");
        assert_eq!(variant, ResourceVariant::Live);
    }

    #[test]
    fn parse_resource_filename_draft() {
        let (slug, variant) = parse_resource_filename("hello.draft.md").unwrap();
        assert_eq!(slug, "hello");
        assert_eq!(variant, ResourceVariant::Draft);
    }

    #[test]
    fn parse_resource_filename_lock() {
        let (slug, variant) = parse_resource_filename("hello.lock").unwrap();
        assert_eq!(slug, "hello");
        assert_eq!(variant, ResourceVariant::Lock);
    }

    #[test]
    fn parse_resource_filename_bare() {
        let (slug, variant) = parse_resource_filename("bare").unwrap();
        assert_eq!(slug, "bare");
        assert_eq!(variant, ResourceVariant::Live);
    }

    #[test]
    fn parse_resource_filename_empty_base_lock() {
        assert!(parse_resource_filename(".lock").is_err());
    }

    #[test]
    fn parse_resource_filename_empty_base_md() {
        assert!(parse_resource_filename(".md").is_err());
    }

    #[test]
    fn parse_resource_filename_empty_base_draft() {
        assert!(parse_resource_filename(".draft.md").is_err());
    }
}

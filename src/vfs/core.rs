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
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;
        let data = match &kind {
            NodeKind::AgentMd => AGENT_MD_CONTENT.as_bytes().to_vec(),
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => self.read_resource_data(rt, connector, collection, resource, variant)?,
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
                match variant {
                    ResourceVariant::Draft => {
                        let mut buf = self
                            .drafts
                            .read_draft(connector, collection, resource)
                            .map_err(|e| VfsError::IoError(e.to_string()))?
                            .unwrap_or_default();
                        let off = offset as usize;
                        let needed = off + data.len();
                        if buf.len() < needed {
                            buf.resize(needed, 0);
                        }
                        buf[off..off + data.len()].copy_from_slice(data);
                        self.drafts
                            .write_draft(connector, collection, resource, &buf)
                            .map_err(|e| VfsError::IoError(e.to_string()))?;
                    }
                    ResourceVariant::Lock => {
                        let lslug = lock_slug(resource);
                        let mut buf = self
                            .drafts
                            .read_draft(connector, collection, &lslug)
                            .map_err(|e| VfsError::IoError(e.to_string()))?
                            .unwrap_or_default();
                        let off = offset as usize;
                        let needed = off + data.len();
                        if buf.len() < needed {
                            buf.resize(needed, 0);
                        }
                        buf[off..off + data.len()].copy_from_slice(data);
                        self.drafts
                            .write_draft(connector, collection, &lslug, &buf)
                            .map_err(|e| VfsError::IoError(e.to_string()))?;
                    }
                    ResourceVariant::Live => {
                        return Err(VfsError::PermissionDenied);
                    }
                }

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
                    self.drafts
                        .create_draft(&connector, &collection, &slug, &[])
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
                return Err(VfsError::PermissionDenied);
            }
        }

        let kind = NodeKind::Resource {
            connector,
            collection,
            resource: slug,
            variant,
        };
        let id = self.nodes.allocate(kind);
        Ok(VfsAttr {
            id,
            size: 0,
            file_type: VfsFileType::RegularFile,
            perm: 0o644,
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
    // Sync connector access helpers
    // -----------------------------------------------------------------------

    /// Read content for a node that needs async connector access.
    /// This is the synchronous version - caller must ensure we're on a runtime.
    pub fn read_resource_sync(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
        resource: &str,
        variant: &ResourceVariant,
    ) -> Result<Vec<u8>, VfsError> {
        self.read_resource_data(rt, connector, collection, resource, variant)
    }

    /// List resources synchronously via connector.
    pub fn list_resources_sync(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
    ) -> Result<Vec<ResourceMeta>, VfsError> {
        let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
        rt.block_on(conn.list_resources(collection))
            .map_err(|e| VfsError::IoError(e.to_string()))
    }

    /// List collections synchronously via connector.
    pub fn list_collections_sync(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
    ) -> Result<Vec<CollectionInfo>, VfsError> {
        let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
        rt.block_on(conn.list_collections())
            .map_err(|e| VfsError::IoError(e.to_string()))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn kind_to_attr(&self, id: u64, kind: &NodeKind) -> VfsAttr {
        match kind {
            NodeKind::Root | NodeKind::Connector { .. } | NodeKind::Collection { .. } | NodeKind::Version { .. } => {
                VfsAttr {
                    id,
                    size: 0,
                    file_type: VfsFileType::Directory,
                    perm: 0o755,
                }
            }
            NodeKind::AgentMd => VfsAttr {
                id,
                size: AGENT_MD_CONTENT.len() as u64,
                file_type: VfsFileType::RegularFile,
                perm: 0o644,
            },
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => {
                let size = self.resource_size(connector, collection, resource, variant);
                VfsAttr {
                    id,
                    size,
                    file_type: VfsFileType::RegularFile,
                    perm: 0o644,
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
            return Ok(NodeKind::AgentMd);
        }
        // Use cached metadata if available, otherwise fetch.
        let cache_key = format!("{}/__collections__", connector);
        let collections = if let Some(cached) = self.cache.get_metadata(&cache_key) {
            cached
                .iter()
                .map(|r| CollectionInfo {
                    name: r.slug.clone(),
                    description: r.title.clone(),
                })
                .collect::<Vec<_>>()
        } else {
            let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
            let cols = rt
                .block_on(conn.list_collections())
                .map_err(|e| VfsError::IoError(e.to_string()))?;
            // Cache as ResourceMeta for reuse
            let meta: Vec<ResourceMeta> = cols
                .iter()
                .map(|c| ResourceMeta {
                    id: c.name.clone(),
                    slug: c.name.clone(),
                    title: c.description.clone(),
                    updated_at: None,
                    content_type: None,
                })
                .collect();
            self.cache.put_metadata(&cache_key, meta);
            cols
        };
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

    fn resolve_collection_child(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
        name: &str,
    ) -> Result<NodeKind, VfsError> {
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
                .read_draft(connector, collection, resource)
                .ok()
                .flatten()
                .map(|d| d.len() as u64)
                .unwrap_or(0),
            ResourceVariant::Lock => {
                let lslug = lock_slug(resource);
                self.drafts
                    .read_draft(connector, collection, &lslug)
                    .ok()
                    .flatten()
                    .map(|d| d.len() as u64)
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

        // Use cached collections
        let cache_key = format!("{}/__collections__", connector);
        let collections = if let Some(cached) = self.cache.get_metadata(&cache_key) {
            cached
                .iter()
                .map(|r| CollectionInfo {
                    name: r.slug.clone(),
                    description: r.title.clone(),
                })
                .collect::<Vec<_>>()
        } else {
            let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
            let cols = rt
                .block_on(conn.list_collections())
                .map_err(|e| VfsError::IoError(e.to_string()))?;
            let meta: Vec<ResourceMeta> = cols
                .iter()
                .map(|c| ResourceMeta {
                    id: c.name.clone(),
                    slug: c.name.clone(),
                    title: c.description.clone(),
                    updated_at: None,
                    content_type: None,
                })
                .collect();
            self.cache.put_metadata(&cache_key, meta);
            cols
        };

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
        }

        // Also list draft-only resources.
        if let Ok(draft_slugs) = self.drafts.list_drafts(connector, collection) {
            let live_slugs: std::collections::HashSet<&str> =
                resources.iter().map(|r| r.slug.as_str()).collect();
            for slug in draft_slugs {
                if slug.ends_with(".lock") || live_slugs.contains(slug.as_str()) {
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

        Ok(entries)
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
                // Check cache first.
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                if let Some(cached) = self.cache.get_resource(&cache_key) {
                    let _ = self.audit.record(
                        "read",
                        connector,
                        Some(collection),
                        Some(resource),
                        "success",
                        Some("from cache".to_string()),
                    );
                    return Ok(cached.data);
                }

                // Fetch from connector.
                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                let result = rt
                    .block_on(conn.read_resource(collection, resource))
                    .map_err(|e| {
                        tracing::error!("read_resource error: {}", e);
                        let _ = self.audit.record(
                            "read",
                            connector,
                            Some(collection),
                            Some(resource),
                            "error",
                            Some(e.to_string()),
                        );
                        VfsError::IoError(e.to_string())
                    })?;

                let data = result.content;

                // Cache the result.
                self.cache.put_resource(
                    &cache_key,
                    crate::cache::store::Resource {
                        data: data.clone(),
                    },
                );

                let _ = self.audit.record(
                    "read",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!("{} bytes from API", data.len())),
                );

                Ok(data)
            }
        }
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

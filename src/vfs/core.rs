//! Platform-agnostic virtual filesystem.
//!
//! Contains ALL the filesystem logic previously in `fs/ops.rs`, but using
//! VFS types instead of fuser types.  This module has ZERO dependency on fuser.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;

use crate::cache::disk::{DiskCache, DiskEntry, DiskMeta};
use crate::cache::store::Cache;
use crate::connector::registry::ConnectorRegistry;
use crate::connector::traits::{CollectionInfo, ResourceMeta};
use crate::draft::store::DraftStore;
use crate::governance::audit::AuditLogger;
use crate::version::store::VersionStore;

use super::types::*;

// ---------------------------------------------------------------------------
// tapfs frontmatter helpers
// ---------------------------------------------------------------------------

struct TapfsMeta {
    draft: bool,
    id: Option<String>,
    version: Option<u32>,
}

fn parse_tapfs_meta(data: &[u8]) -> TapfsMeta {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return TapfsMeta { draft: false, id: None, version: None },
    };

    if !text.starts_with("---") {
        return TapfsMeta { draft: false, id: None, version: None };
    }

    let after_open = &text[3..];
    let fm_text = if let Some(pos) = after_open.find("\n---") {
        &after_open[..pos]
    } else {
        return TapfsMeta { draft: false, id: None, version: None };
    };

    let mut draft = false;
    let mut id: Option<String> = None;
    let mut version: Option<u32> = None;

    for line in fm_text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("_draft:") {
            let v = val.trim();
            draft = v == "true";
        } else if let Some(val) = line.strip_prefix("_id:") {
            let v = val.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                id = Some(v.to_string());
            }
        } else if let Some(val) = line.strip_prefix("_version:") {
            let v = val.trim();
            if let Ok(n) = v.parse::<u32>() {
                version = Some(n);
            }
        }
    }

    TapfsMeta { draft, id, version }
}

fn strip_tapfs_fields(data: &[u8]) -> Vec<u8> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return data.to_vec(),
    };

    if !text.starts_with("---") {
        return data.to_vec();
    }

    let after_open = &text[3..];
    let close_pos = match after_open.find("\n---") {
        Some(p) => p,
        None => return data.to_vec(),
    };

    let fm_text = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    let filtered: Vec<&str> = fm_text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("_draft:")
                && !trimmed.starts_with("_id:")
                && !trimmed.starts_with("_version:")
        })
        .collect();

    let new_fm = filtered.join("\n");
    if new_fm.trim().is_empty() {
        // All frontmatter was tapfs fields — collapse to just body
        let result = format!("---\n---{}", body);
        result.into_bytes()
    } else {
        let result = format!("---\n{}\n---{}", new_fm, body);
        result.into_bytes()
    }
}

fn inject_tapfs_fields(data: &[u8], id: &str, version: u32) -> Vec<u8> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return data.to_vec(),
    };

    if !text.starts_with("---") {
        // No frontmatter — prepend one
        let result = format!(
            "---\n_id: {}\n_version: {}\n---\n{}",
            id, version, text
        );
        return result.into_bytes();
    }

    let after_open = &text[3..];
    let close_pos = match after_open.find("\n---") {
        Some(p) => p,
        None => {
            let result = format!("---\n_id: {}\n_version: {}\n---\n{}", id, version, text);
            return result.into_bytes();
        }
    };

    let fm_text = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    // Remove old tapfs fields and _draft, then append updated values
    let mut lines: Vec<&str> = fm_text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("_draft:")
                && !trimmed.starts_with("_id:")
                && !trimmed.starts_with("_version:")
        })
        .collect();

    lines.push(&""); // placeholder to trigger join separator trick
    let mut new_fm = lines[..lines.len() - 1].join("\n");
    if !new_fm.is_empty() && !new_fm.ends_with('\n') {
        new_fm.push('\n');
    }
    new_fm.push_str(&format!("_id: {}\n_version: {}", id, version));

    let result = format!("---\n{}\n---{}", new_fm, body);
    result.into_bytes()
}

// ---------------------------------------------------------------------------
// SlugMap — api_id → user_slug persistence
// ---------------------------------------------------------------------------

/// Maps api_id → user_slug for readdir display. Persisted to disk.
struct SlugMap {
    /// key: "connector/collection/api_id" → user_slug filename stem
    inner: DashMap<String, String>,
    path: PathBuf,
}

impl SlugMap {
    fn load(path: PathBuf) -> Self {
        let inner = DashMap::new();
        if path.exists() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(map) =
                    serde_json::from_slice::<std::collections::HashMap<String, String>>(&bytes)
                {
                    for (k, v) in map {
                        inner.insert(k, v);
                    }
                }
            }
        }
        Self { inner, path }
    }

    fn insert(&self, connector: &str, collection: &str, api_id: &str, user_slug: &str) {
        self.inner.insert(
            format!("{}/{}/{}", connector, collection, api_id),
            user_slug.to_string(),
        );
        self.save();
    }

    fn get_user_slug(&self, connector: &str, collection: &str, api_id: &str) -> Option<String> {
        self.inner
            .get(&format!("{}/{}/{}", connector, collection, api_id))
            .map(|v| v.clone())
    }

    fn save(&self) {
        let map: std::collections::HashMap<String, String> = self
            .inner
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        if let Ok(json) = serde_json::to_string(&map) {
            // Atomic write: temp file then rename
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}

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
}

impl NodeTable {
    /// Create a new node table with the root node (ID 1) pre-allocated.
    pub fn new() -> Self {
        let table = Self {
            entries: DashMap::new(),
            reverse: DashMap::new(),
        };
        table.entries.insert(1, NodeKind::Root);
        table.reverse.insert(NodeKind::Root, 1);
        table
    }

    /// Deterministic node ID derived from the NodeKind's content.
    ///
    /// The same NodeKind always produces the same ID, across restarts.
    /// This is critical for macOS File Provider, which caches item
    /// identifiers persistently. ID 1 is reserved for root.
    ///
    /// Uses SipHash-1-3 with fixed keys to guarantee stability across
    /// Rust toolchain upgrades (unlike `DefaultHasher`).
    fn stable_id(kind: &NodeKind) -> u64 {
        use siphasher::sip::SipHasher13;
        use std::hash::{Hash, Hasher};

        if *kind == NodeKind::Root {
            return 1;
        }

        // Fixed keys — MUST NEVER CHANGE or all cached File Provider
        // identifiers and NFS file handles become invalid.
        let mut hasher = SipHasher13::new_with_keys(0x_7a31_6f62_6573_7461, 0x_6964_656e_7469_6669);
        kind.hash(&mut hasher);
        let hash = hasher.finish();

        // Avoid 0 (invalid) and 1 (root).
        match hash {
            0 => 2,
            1 => 3,
            h => h,
        }
    }

    /// Allocate a node ID for the given kind.
    ///
    /// The ID is deterministic — the same NodeKind always gets the same ID.
    /// If the kind was already allocated, returns the existing ID.
    /// On hash collision (different NodeKind, same hash), linearly probes
    /// for the next free slot.
    pub fn allocate(&self, kind: NodeKind) -> u64 {
        // Fast path: already allocated.
        if let Some(existing) = self.reverse.get(&kind) {
            return *existing;
        }

        let mut id = Self::stable_id(&kind);

        // Atomic insert with linear probing on collision.
        loop {
            match self.entries.entry(id) {
                dashmap::mapref::entry::Entry::Vacant(entry) => {
                    entry.insert(kind.clone());
                    break;
                }
                dashmap::mapref::entry::Entry::Occupied(entry) => {
                    if entry.get() == &kind {
                        // Same kind stored by a concurrent thread — use it.
                        break;
                    }
                    // Genuine collision with a different NodeKind — probe next slot.
                    tracing::warn!(id = id, existing = ?entry.get(), new = ?kind, "stable_id hash collision, probing");
                    id = if id == u64::MAX { 2 } else { id + 1 };
                }
            }
        }

        // Use or_insert to handle concurrent allocations of the same kind.
        let actual = *self.reverse.entry(kind.clone()).or_insert(id);
        if actual != id {
            // Another thread won the race — remove our entry.
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
    /// Optional persistent L2 cache. Survives restarts and has no size cap;
    /// validated against `ResourceMeta.updated_at` before serving so a
    /// changed upstream resource is refetched even if the in-memory L1 has
    /// expired.
    pub disk_cache: Option<Arc<DiskCache>>,
    pub drafts: Arc<DraftStore>,
    pub versions: Arc<VersionStore>,
    pub audit: Arc<AuditLogger>,
    /// Maps api_id → user_slug for readdir display. Persisted to disk so that
    /// renames survive restarts.
    slug_map: Arc<SlugMap>,
    /// In-memory write buffers, keyed by inode ID. Flushed to draft store on
    /// flush/release so that small, repeated FUSE `write()` calls (e.g. 4 KB
    /// chunks) accumulate in RAM instead of doing O(n^2) read-modify-write
    /// cycles on disk.
    write_buffers: DashMap<u64, Vec<u8>>,
    /// Modification timestamps (RFC 3339) by inode ID, populated from
    /// `ResourceMeta.updated_at` when resources are discovered.
    resource_mtimes: DashMap<u64, String>,
    /// Known content lengths by cache key (`connector/collection/resource`),
    /// populated on first read so that subsequent `getattr` calls can report
    /// accurate sizes instead of the 4096 placeholder.
    content_lengths: DashMap<String, u64>,
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
            disk_cache: None,
            drafts,
            versions,
            audit,
            slug_map: Arc::new(SlugMap::load(PathBuf::from("/dev/null"))),
            write_buffers: DashMap::new(),
            resource_mtimes: DashMap::new(),
            content_lengths: DashMap::new(),
        }
    }

    /// Attach a persistent disk cache. Returns `self` so the call chains with
    /// `Arc::new(VirtualFs::new(...).with_disk_cache(...))`.
    pub fn with_disk_cache(mut self, disk: Arc<DiskCache>) -> Self {
        self.disk_cache = Some(disk);
        self
    }

    /// Attach a persistent slug map. Returns `self` for chaining.
    pub fn with_slug_map(mut self, path: PathBuf) -> Self {
        self.slug_map = Arc::new(SlugMap::load(path));
        self
    }

    /// Drop both the in-memory and on-disk cache entries for a single
    /// resource, used after promoting a draft so the next read sees the
    /// upstream's authoritative version.
    fn invalidate_resource_cache(&self, connector: &str, collection: &str, resource: &str) {
        let cache_key = format!("{}/{}/{}", connector, collection, resource);
        self.cache.invalidate(&cache_key);
        if let Some(disk) = &self.disk_cache {
            disk.invalidate(connector, collection, resource);
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
            NodeKind::Connector { name: conn } => self.resolve_connector_child(rt, conn, name)?,
            NodeKind::Collection {
                connector,
                collection,
            } => self.resolve_collection_child(rt, connector, collection, name)?,
            NodeKind::TxDir {
                connector,
                collection,
            } => {
                // Looking up a named transaction
                NodeKind::Transaction {
                    connector: connector.clone(),
                    collection: collection.clone(),
                    tx_name: name.to_string(),
                }
            }
            NodeKind::Transaction {
                connector,
                collection,
                tx_name,
            } => {
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
            NodeKind::TxDir {
                connector,
                collection,
            } => self.readdir_tx_dir(id, connector, collection),
            NodeKind::Transaction {
                connector,
                collection,
                tx_name,
            } => self.readdir_transaction(id, connector, collection, tx_name),
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

        // For resource data (live/draft/lock) use Bytes for O(1) slicing.
        // Other node types produce small content that doesn't benefit from
        // refcounted buffers.
        let data: bytes::Bytes = match &kind {
            NodeKind::AgentMd => bytes::Bytes::from(self.generate_root_agent_md().into_bytes()),
            NodeKind::ConnectorAgentMd { connector } => {
                bytes::Bytes::from(self.generate_connector_agent_md(rt, connector).into_bytes())
            }
            NodeKind::CollectionAgentMd {
                connector,
                collection,
            } => bytes::Bytes::from(
                self.generate_collection_agent_md(rt, connector, collection)
                    .into_bytes(),
            ),
            NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } => self.read_resource_data(rt, connector, collection, resource, variant)?,
            NodeKind::Version {
                connector,
                collection,
                resource,
                version_id,
            } => {
                if let Some(v) = version_id {
                    bytes::Bytes::from(
                        self.versions
                            .read_version(connector, collection, resource, *v as u32)
                            .map_err(|e| VfsError::IoError(e.to_string()))?
                            .ok_or(VfsError::NotFound)?,
                    )
                } else {
                    return Err(VfsError::NotFound);
                }
            }
            NodeKind::TxResource {
                connector,
                collection,
                tx_name,
                resource,
            } => {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                bytes::Bytes::from(
                    self.drafts
                        .read_draft(connector, collection, &tx_slug)
                        .map_err(|e| VfsError::IoError(e.to_string()))?
                        .ok_or(VfsError::NotFound)?,
                )
            }
            _ => return Err(VfsError::IsDirectory),
        };

        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(Vec::new());
        }
        let end = std::cmp::min(offset + size as usize, data.len());
        // Only allocate the requested slice, not the full content.
        Ok(data.slice(offset..end).to_vec())
    }

    // -----------------------------------------------------------------------
    // write
    // -----------------------------------------------------------------------

    /// Write data to a file at offset. Returns bytes written.
    ///
    /// Data is buffered in memory (keyed by inode ID) rather than flushed to
    /// the draft store on every call.  The buffer is written to disk when
    /// `flush()` is called (typically on file close).  This turns the
    /// previous O(n^2) read-modify-write pattern into O(n) for sequential
    /// writes.
    pub fn write(&self, id: u64, offset: u64, data: &[u8]) -> Result<u32, VfsError> {
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
            NodeKind::TxResource {
                connector,
                collection,
                tx_name,
                resource,
            } => {
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
    pub fn create(&self, parent_id: u64, name: &str) -> Result<VfsAttr, VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;

        // Handle creating files inside a transaction
        if let NodeKind::Transaction {
            connector,
            collection,
            tx_name,
        } = &parent_kind
        {
            let resource = name.strip_suffix(".md").unwrap_or(name);
            let tx_slug = format!("__tx_{}_{}", tx_name, resource);
            if !self.drafts.has_draft(connector, collection, &tx_slug) {
                // Pre-populate from live cache (copy-on-write). Bytes deref
                // avoids a full .to_vec() allocation.
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                let cached = self.cache.get_resource(&cache_key);
                let initial: &[u8] = cached.as_ref().map(|c| c.data.as_ref()).unwrap_or(&[]);
                self.drafts
                    .create_draft(connector, collection, &tx_slug, initial)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
            let _ = self.audit.record(
                "create_tx_resource",
                connector,
                Some(collection),
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
                    // Pre-populate from live cache (copy-on-write).
                    let cache_key = format!("{}/{}/{}", connector, collection, slug);
                    let cached = self.cache.get_resource(&cache_key);
                    let initial: &[u8] = cached.as_ref().map(|c| c.data.as_ref()).unwrap_or(&[]);
                    self.drafts
                        .create_draft(&connector, &collection, &slug, initial)
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
                    // If resource already exists in API (listing cache), inject
                    // its _id so flush uses write_resource (PATCH) not create_resource.
                    let api_id = {
                        let listing_key = format!("{}/{}", connector, collection);
                        self.cache
                            .get_metadata(&listing_key)
                            .and_then(|metas| {
                                metas
                                    .into_iter()
                                    .find(|m| m.slug == slug || m.id == slug)
                                    .map(|m| m.id)
                            })
                    };
                    let template: Vec<u8> = if let Some(ref id) = api_id {
                        format!("---\n_id: {}\n_version: 0\n---\n\n", id).into_bytes()
                    } else {
                        b"---\n_draft: true\n_id:\n_version:\n---\n\n".to_vec()
                    };
                    self.drafts
                        .create_draft(&connector, &collection, &slug, &template)
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
        let actual_variant = if variant == ResourceVariant::Live
            && self.drafts.has_draft(&connector, &collection, &slug)
        {
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
        _rt: &tokio::runtime::Handle,
        parent_id: u64,
        _old_name: &str,
        new_parent_id: u64,
        _new_name: &str,
    ) -> Result<(), VfsError> {
        if parent_id != new_parent_id {
            return Err(VfsError::CrossDevice);
        }

        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        match &parent_kind {
            NodeKind::Collection { .. } => {}
            _ => return Err(VfsError::PermissionDenied),
        };

        Err(VfsError::NotSupported)
    }

    // -----------------------------------------------------------------------
    // unlink
    // -----------------------------------------------------------------------

    /// Delete a file (draft, lock, or live resource).
    pub fn unlink(
        &self,
        rt: &tokio::runtime::Handle,
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
                // Determine the API id from frontmatter if a draft exists locally.
                let api_id = if self.drafts.has_draft(&connector, &collection, &slug) {
                    if let Ok(Some(data)) =
                        self.drafts.read_draft(&connector, &collection, &slug)
                    {
                        let meta = parse_tapfs_meta(&data);
                        if meta.id.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
                            // Never posted to API — just remove the local draft.
                            let _ = self.drafts.delete_draft(&connector, &collection, &slug);
                            let _ = self.audit.record(
                                "delete",
                                &connector,
                                Some(&collection),
                                Some(&slug),
                                "success",
                                Some("local-only draft removed (never posted)".to_string()),
                            );
                            let kind = NodeKind::Resource {
                                connector,
                                collection,
                                resource: slug,
                                variant,
                            };
                            if let Some(id) = self.nodes.lookup(&kind) {
                                self.nodes.remove(id);
                            }
                            return Ok(());
                        }
                        meta.id
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Check if the connector supports delete via the spec.
                let spec = self.registry.get_spec(&connector);
                let supports_delete = spec
                    .as_ref()
                    .and_then(|s| s.capabilities.as_ref())
                    .and_then(|c| c.delete)
                    .unwrap_or(false);

                if !supports_delete {
                    // Clean up local draft even when API delete is unsupported.
                    let _ = self.drafts.delete_draft(&connector, &collection, &slug);
                    return Err(VfsError::PermissionDenied);
                }

                let delete_id = api_id.as_deref().unwrap_or(&slug);
                let conn = self.registry.get(&connector).ok_or(VfsError::NotFound)?;
                rt.block_on(conn.delete_resource(&collection, delete_id))
                    .map_err(|e| {
                        tracing::error!("delete_resource error: {}", e);
                        let _ = self.audit.record(
                            "delete",
                            &connector,
                            Some(&collection),
                            Some(&slug),
                            "error",
                            Some(e.to_string()),
                        );
                        VfsError::IoError(e.to_string())
                    })?;

                let _ = self.drafts.delete_draft(&connector, &collection, &slug);
                self.invalidate_resource_cache(&connector, &collection, &slug);

                let _ = self.audit.record(
                    "delete",
                    &connector,
                    Some(&collection),
                    Some(&slug),
                    "success",
                    Some("live resource deleted via API".to_string()),
                );
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

    /// Flush all pending write buffers to disk.
    ///
    /// Called during graceful shutdown to ensure no data is lost. Buffers are
    /// persisted to the draft store but live files are **not** auto-promoted
    /// (no API calls are made) to keep shutdown fast and predictable.
    pub fn flush_all(&self) {
        let ids: Vec<u64> = self.write_buffers.iter().map(|r| *r.key()).collect();
        for id in ids {
            if let Some((_, buf)) = self.write_buffers.remove(&id) {
                if let Some(kind) = self.nodes.get(id) {
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
                            let _ = self.drafts.write_draft(connector, collection, &slug, &buf);
                        }
                        NodeKind::TxResource {
                            connector,
                            collection,
                            tx_name,
                            resource,
                        } => {
                            let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                            let _ = self
                                .drafts
                                .write_draft(connector, collection, &tx_slug, &buf);
                        }
                        _ => {}
                    }
                }
            }
        }
        tracing::info!("flushed all pending write buffers to disk");
    }

    /// Flush a file.
    ///
    /// 1. If there is an in-memory write buffer for this inode, persist it to
    ///    the draft store (single write, not read-modify-write).
    /// 2. For live files with pending draft content, auto-promote (push to API
    ///    and clean up the draft).
    pub fn flush(&self, rt: &tokio::runtime::Handle, id: u64) -> Result<(), VfsError> {
        let kind = self.nodes.get(id).ok_or(VfsError::NotFound)?;

        // Step 1: flush the in-memory write buffer to the draft store.
        if let Some((_, buf)) = self.write_buffers.remove(&id) {
            if let NodeKind::Resource {
                connector,
                collection,
                resource,
                variant,
            } = &kind
            {
                let slug = match variant {
                    ResourceVariant::Lock => lock_slug(resource),
                    _ => resource.clone(),
                };

                // For Live resources: if the existing draft has _id/_version
                // set (written after a previous flush/promote) and the incoming
                // write buffer doesn't carry _id, re-inject it so we don't
                // lose track of the API id across multi-packet writes.
                let to_write = if matches!(variant, ResourceVariant::Live) {
                    let existing_meta = self
                        .drafts
                        .read_draft(connector, collection, &slug)
                        .ok()
                        .flatten()
                        .map(|d| parse_tapfs_meta(&d));
                    let new_meta = parse_tapfs_meta(&buf);
                    if let Some(ex) = existing_meta {
                        if let Some(ref existing_id) = ex.id {
                            if !existing_id.trim().is_empty()
                                && new_meta
                                    .id
                                    .as_ref()
                                    .map(|s| s.trim().is_empty())
                                    .unwrap_or(true)
                            {
                                // Carry forward _id/_version so next flush
                                // uses write_resource instead of create_resource
                                inject_tapfs_fields(
                                    &buf,
                                    existing_id,
                                    ex.version.unwrap_or(0),
                                )
                            } else {
                                buf
                            }
                        } else {
                            buf
                        }
                    } else {
                        buf
                    }
                } else {
                    buf
                };

                self.drafts
                    .write_draft(connector, collection, &slug, &to_write)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            } else if let NodeKind::TxResource {
                connector,
                collection,
                tx_name,
                resource,
            } = &kind
            {
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

                let tapfs_meta = parse_tapfs_meta(&data);

                // _draft: true means user hasn't published yet — keep local, no API call
                if tapfs_meta.draft {
                    return Ok(());
                }

                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                let clean_data = strip_tapfs_fields(&data);

                // is_new: _id absent/empty means never been to the API
                let is_new = tapfs_meta
                    .id
                    .as_ref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);

                let api_id = if is_new {
                    match rt.block_on(conn.create_resource(collection, &clean_data)) {
                        Ok(meta) => {
                            let _ = self.audit.record(
                                "create",
                                connector,
                                Some(collection),
                                Some(resource),
                                "success",
                                Some(format!("{} bytes posted to API on close", data.len())),
                            );
                            let listing_key = format!("{}/{}", connector, collection);
                            self.cache.invalidate(&listing_key);
                            meta.id
                        }
                        Err(e) => {
                            if e.downcast_ref::<crate::connector::traits::ConnectorError>()
                                .is_some_and(|ce| {
                                    matches!(
                                        ce,
                                        crate::connector::traits::ConnectorError::NotSupported(_)
                                    )
                                })
                            {
                                rt.block_on(conn.write_resource(collection, resource, &clean_data))
                                    .map_err(|e| VfsError::IoError(e.to_string()))?;
                                resource.to_string()
                            } else {
                                tracing::error!("create_resource error: {}", e);
                                return Err(VfsError::IoError(e.to_string()));
                            }
                        }
                    }
                } else {
                    let id = tapfs_meta.id.as_deref().unwrap();
                    rt.block_on(conn.write_resource(collection, id, &clean_data))
                        .map_err(|e| {
                            tracing::error!("write_resource error: {}", e);
                            VfsError::IoError(e.to_string())
                        })?;
                    let _ = self.audit.record(
                        "write",
                        connector,
                        Some(collection),
                        Some(resource),
                        "success",
                        Some(format!("{} bytes", data.len())),
                    );
                    id.to_string()
                };

                // Write _id and _version back into the draft so subsequent
                // flushes treat the resource as existing and use write_resource.
                let new_version = tapfs_meta.version.unwrap_or(0) + 1;
                let updated = inject_tapfs_fields(&data, &api_id, new_version);
                let _ = self
                    .drafts
                    .write_draft(connector, collection, resource, &updated);

                // Populate in-memory cache so the next flush uses write_resource.
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                self.cache.put_resource(
                    &cache_key,
                    crate::cache::store::Resource {
                        data: bytes::Bytes::from(clean_data.clone()),
                        raw_json: None,
                    },
                );

                // Store in slug map for readdir display
                if api_id != *resource {
                    self.slug_map.insert(connector, collection, &api_id, resource);
                }

                let _ = self
                    .versions
                    .save_snapshot(connector, collection, resource, &clean_data);
                self.invalidate_resource_cache(connector, collection, resource);

                // Bump mtime so the NFS client invalidates its attribute cache
                // and re-reads the file (which now contains _id/_version).
                let now_ts = chrono::Utc::now().to_rfc3339();
                self.resource_mtimes.insert(id, now_ts);

                let _ = self.audit.record(
                    "auto-promote",
                    connector,
                    Some(collection),
                    Some(resource),
                    "success",
                    Some(format!("{} bytes pushed to API on close", data.len())),
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
                let mut data = self
                    .drafts
                    .read_draft(connector, collection, &slug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .unwrap_or_default();
                data.resize(new_len, 0);
                self.drafts
                    .write_draft(connector, collection, &slug, &data)
                    .map_err(|e| VfsError::IoError(e.to_string()))?;
            }
            NodeKind::TxResource {
                connector,
                collection,
                tx_name,
                resource,
            } => {
                let tx_slug = format!("__tx_{}_{}", tx_name, resource);
                let mut data = self
                    .drafts
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
            NodeKind::Root | NodeKind::Connector { .. } | NodeKind::Collection { .. } => VfsAttr {
                id,
                size: 0,
                file_type: VfsFileType::Directory,
                perm: 0o755,
                mtime: None,
            },
            NodeKind::TxDir { .. } | NodeKind::Transaction { .. } => VfsAttr {
                id,
                size: 0,
                file_type: VfsFileType::Directory,
                perm: 0o755,
                mtime: None,
            },
            NodeKind::AgentMd => VfsAttr {
                id,
                size: 4096, // dynamic content, actual size known on read
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
                let mtime = self.resource_mtimes.get(&id).map(|v| v.clone());
                VfsAttr {
                    id,
                    size,
                    file_type: VfsFileType::RegularFile,
                    perm,
                    mtime,
                }
            }
            NodeKind::Version {
                connector,
                collection,
                resource,
                version_id,
            } => {
                let size = if let Some(v) = version_id {
                    self.versions
                        .read_version(connector, collection, resource, *v as u32)
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
            NodeKind::TxResource {
                connector,
                collection,
                tx_name,
                resource,
            } => {
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
            return Ok(NodeKind::ConnectorAgentMd {
                connector: connector.to_string(),
            });
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

        if let Some(meta) = resources.iter().find(|r| r.slug == slug || r.id == slug) {
            let kind = NodeKind::Resource {
                connector: connector.to_string(),
                collection: collection.to_string(),
                resource: slug,
                variant: ResourceVariant::Live,
            };
            // Store mtime from API metadata so getattr can report it.
            if let Some(ts) = &meta.updated_at {
                let id = self.nodes.allocate(kind.clone());
                self.resource_mtimes.insert(id, ts.clone());
            }
            return Ok(kind);
        }

        // Also surface locally-created resources that have a pending draft
        // (e.g. _draft: true files not yet pushed to API)
        if self.drafts.has_draft(connector, collection, &slug) {
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
                // Draft store is authoritative: if a draft exists it's what
                // read_resource_data() will serve, so its size must match.
                if let Some(sz) = self.drafts.draft_size(connector, collection, resource) {
                    return sz;
                }
                let cache_key = format!("{}/{}/{}", connector, collection, resource);
                if let Some(cached) = self.cache.get_resource(&cache_key) {
                    return cached.data.len() as u64;
                }
                // Use previously recorded content length if available.
                // Fall back to 4096 for resources that haven't been read yet —
                // returning 0 would cause tools like `cat` to skip the file.
                self.content_lengths
                    .get(&cache_key)
                    .map(|v| *v)
                    .unwrap_or(4096)
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
        let agent_kind = NodeKind::ConnectorAgentMd {
            connector: connector.to_string(),
        };
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
            // Use user slug from slug map if available (set after first flush)
            let display_slug = self
                .slug_map
                .get_user_slug(connector, collection, &res.id)
                .unwrap_or_else(|| res.slug.clone());
            let filename = format!("{}.md", display_slug);
            let kind = NodeKind::Resource {
                connector: connector.to_string(),
                collection: collection.to_string(),
                resource: res.slug.clone(),
                variant: ResourceVariant::Live,
            };
            let id = self.nodes.allocate(kind);
            if let Some(ts) = &res.updated_at {
                self.resource_mtimes.insert(id, ts.clone());
            }
            entries.push(VfsDirEntry {
                name: filename,
                id,
                file_type: VfsFileType::RegularFile,
            });

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
            let versions = self
                .versions
                .list_versions(connector, collection, &res.slug)
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

        // Add locally-created resources (have a pending draft, not yet on API)
        let api_ids: std::collections::HashSet<String> = resources
            .iter()
            .flat_map(|r| [r.id.clone(), r.slug.clone()])
            .collect();
        if let Ok(draft_slugs) = self.drafts.list_drafts(connector, collection) {
            for slug in draft_slugs {
                if slug.ends_with(".lock") || slug.starts_with("__tx_") {
                    continue;
                }
                if api_ids.contains(&slug) {
                    continue;
                }
                if let Ok(Some(data)) = self.drafts.read_draft(connector, collection, &slug) {
                    let meta = parse_tapfs_meta(&data);
                    if meta.draft || meta.id.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
                        let kind = NodeKind::Resource {
                            connector: connector.to_string(),
                            collection: collection.to_string(),
                            resource: slug.clone(),
                            variant: ResourceVariant::Live,
                        };
                        let id = self.nodes.allocate(kind);
                        entries.push(VfsDirEntry {
                            name: format!("{}.md", slug),
                            id,
                            file_type: VfsFileType::RegularFile,
                        });
                    }
                }
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

    fn generate_root_agent_md(&self) -> String {
        let connectors = self.registry.list();
        let mut out = String::new();
        out.push_str("---\ntitle: tapfs\n---\n\n");

        // Connected services
        out.push_str("# Connected services\n\n");
        if connectors.is_empty() {
            out.push_str("No services connected.\n");
        } else {
            for name in &connectors {
                out.push_str(&format!("- **{}/**\n", name));
            }
        }

        // How to use — this is the skill definition for any agent
        out.push_str("\n# How to use this filesystem\n\n");
        out.push_str("Enterprise data is mounted here as plain files. ");
        out.push_str("Use standard commands to explore and modify it.\n\n");

        out.push_str("## Reading data\n\n");
        out.push_str("- `ls <service>/` — list collections (issues, repos, etc.)\n");
        out.push_str("- `ls <service>/<collection>/` — list resources\n");
        out.push_str("- `cat <resource>.md` — read a resource\n");
        out.push_str("- `grep -r \"keyword\" <service>/` — search across resources\n");

        out.push_str("\n## Making changes\n\n");
        out.push_str("- Write to `<name>.draft.md` to stage changes safely\n");
        out.push_str("- Rename `.draft.md` to `.md` to publish changes\n");
        out.push_str("- Create `<name>.lock` before editing to prevent conflicts\n");

        out.push_str("\n## Tips\n\n");
        out.push_str("- Each service directory has its own `agent.md` with details\n");
        out.push_str("- Each collection directory has an `agent.md` listing available resources\n");
        out.push_str("- `.md` files are live data — reading fetches the latest from the API\n");
        out.push_str("- `.draft.md` files are local only until promoted\n");

        out
    }

    fn generate_connector_agent_md(&self, rt: &tokio::runtime::Handle, connector: &str) -> String {
        let spec_owned = self.registry.get_spec(connector);
        let spec = spec_owned.as_ref();
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", connector));

        // Connector description from spec
        if let Some(desc) = spec.and_then(|s| s.description.as_ref()) {
            out.push_str(desc);
            out.push_str("\n\n");
        }

        // List collections with descriptions from spec
        if let Ok(collections) = self.get_collections_cached(rt, connector) {
            out.push_str("## Collections\n\n");
            for col in &collections {
                out.push_str(&format!("- **{}/**", col.name));
                // Prefer description from spec (richer), fall back to trait
                let spec_desc = spec
                    .and_then(|s| s.collections.iter().find(|c| c.name == col.name))
                    .and_then(|c| c.description.as_ref());
                if let Some(desc) = spec_desc.or(col.description.as_ref()) {
                    out.push_str(&format!(" — {}", desc));
                }
                // Show slug hint if available
                if let Some(hint) = spec
                    .and_then(|s| s.collections.iter().find(|c| c.name == col.name))
                    .and_then(|c| c.slug_hint.as_ref())
                {
                    out.push_str(&format!(" (filenames: {})", hint));
                }
                out.push('\n');
            }
        }

        // Capabilities from spec
        if let Some(caps) = spec.and_then(|s| s.capabilities.as_ref()) {
            out.push_str("\n## Capabilities\n\n");
            let mut cap_list = Vec::new();
            if caps.read.unwrap_or(true) {
                cap_list.push("read");
            }
            if caps.write.unwrap_or(false) {
                cap_list.push("write");
            }
            if caps.create.unwrap_or(false) {
                cap_list.push("create");
            }
            if caps.delete.unwrap_or(false) {
                cap_list.push("delete");
            }
            if caps.drafts.unwrap_or(true) {
                cap_list.push("drafts");
            }
            if caps.versions.unwrap_or(false) {
                cap_list.push("versions");
            }
            if !cap_list.is_empty() {
                out.push_str(&format!("Supported: {}\n", cap_list.join(", ")));
            }
            if let Some(ref rl) = caps.rate_limit {
                if let Some(rpm) = rl.requests_per_minute {
                    out.push_str(&format!("\nRate limit: {} requests/min\n", rpm));
                }
            }
        }

        // Agent tips from spec
        if let Some(tips) = spec
            .and_then(|s| s.agent.as_ref())
            .and_then(|a| a.tips.as_ref())
        {
            if !tips.is_empty() {
                out.push_str("\n## Tips\n\n");
                for tip in tips {
                    out.push_str(&format!("- {}\n", tip));
                }
            }
        }

        // Relationships from spec
        if let Some(rels) = spec
            .and_then(|s| s.agent.as_ref())
            .and_then(|a| a.relationships.as_ref())
        {
            if !rels.is_empty() {
                out.push_str("\n## Relationships\n\n");
                for rel in rels {
                    out.push_str(&format!("- {}\n", rel));
                }
            }
        }

        out.push_str("\n## Usage\n\n");
        out.push_str(&format!("- `ls {}/` — list collections\n", connector));
        out.push_str(&format!(
            "- `ls {}/<collection>/` — list resources\n",
            connector
        ));
        out.push_str(&format!(
            "- `cat {}/<collection>/<resource>.md` — read a resource\n",
            connector
        ));

        out
    }

    fn generate_collection_agent_md(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
    ) -> String {
        let spec_owned = self.registry.get_spec(connector);
        let spec = spec_owned.as_ref();
        let col_spec = spec.and_then(|s| s.collections.iter().find(|c| c.name == collection));

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str(&format!("collection: {}\n", collection));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}/{}\n\n", connector, collection));

        // Collection description from spec
        if let Some(desc) = col_spec.and_then(|c| c.description.as_ref()) {
            out.push_str(desc);
            out.push_str("\n\n");
        }

        // Operations supported
        if let Some(ops) = col_spec.and_then(|c| c.operations.as_ref()) {
            if !ops.is_empty() {
                out.push_str(&format!("**Operations:** {}\n\n", ops.join(", ")));
            }
        }

        // Slug hint
        if let Some(hint) = col_spec.and_then(|c| c.slug_hint.as_ref()) {
            out.push_str(&format!("**Filenames:** {}\n\n", hint));
        }

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
                out.push_str(&format!(
                    "\n... and {} more. Use `ls` to see all.\n",
                    resources.len() - 10
                ));
            }
        }

        // Collection-level relationships
        if let Some(rels) = col_spec.and_then(|c| c.relationships.as_ref()) {
            if !rels.is_empty() {
                out.push_str("\n## Related collections\n\n");
                for rel in rels {
                    out.push_str(&format!("- **{}/**", rel.target));
                    if let Some(ref desc) = rel.description {
                        out.push_str(&format!(" — {}", desc));
                    }
                    out.push('\n');
                }
            }
        }

        out
    }

    /// Read full resource content, returning `Bytes` for O(1) slicing.
    ///
    /// For live resources the data comes from the cache (or is fetched and
    /// cached).  Drafts and locks are read from the draft store and wrapped
    /// in `Bytes`.
    fn read_resource_data(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
        resource: &str,
        variant: &ResourceVariant,
    ) -> Result<bytes::Bytes, VfsError> {
        match variant {
            ResourceVariant::Draft => {
                let data = self
                    .drafts
                    .read_draft(connector, collection, resource)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .ok_or(VfsError::NotFound)?;
                Ok(bytes::Bytes::from(data))
            }
            ResourceVariant::Lock => {
                let lslug = lock_slug(resource);
                let data = self
                    .drafts
                    .read_draft(connector, collection, &lslug)
                    .map_err(|e| VfsError::IoError(e.to_string()))?
                    .ok_or(VfsError::NotFound)?;
                Ok(bytes::Bytes::from(data))
            }
            ResourceVariant::Live => {
                // Serve local draft if present (new resource not yet on API, or pending changes)
                if self.drafts.has_draft(connector, collection, resource) {
                    if let Some(data) = self
                        .drafts
                        .read_draft(connector, collection, resource)
                        .map_err(|e| VfsError::IoError(e.to_string()))?
                    {
                        return Ok(bytes::Bytes::from(data));
                    }
                }

                let cache_key = format!("{}/{}/{}", connector, collection, resource);

                // L1 — in-memory cache. Bytes::clone is O(1).
                if let Some(cached) = self.cache.get_resource(&cache_key) {
                    return Ok(cached.data.clone());
                }

                // L2 — disk cache, validated by `updated_at`. We only trust a
                // disk hit if we've seen the collection's listing recently
                // and the listing's `updated_at` for this resource matches
                // what's on disk. Otherwise refetch.
                let listing_key = format!("{}/{}", connector, collection);
                let listing_updated_at = self.cache.get_metadata(&listing_key).and_then(|metas| {
                    metas
                        .into_iter()
                        .find(|m| m.slug == resource || m.id == resource)
                        .and_then(|m| m.updated_at)
                });

                if let (Some(disk), Some(upstream_ts)) =
                    (self.disk_cache.as_ref(), listing_updated_at.as_ref())
                {
                    if let Some(entry) = disk.get(connector, collection, resource) {
                        if entry.meta.updated_at.as_deref() == Some(upstream_ts.as_str()) {
                            let data = entry.data.clone();
                            self.content_lengths
                                .insert(cache_key.clone(), data.len() as u64);
                            // Promote into L1 if under the in-memory size cap
                            // so subsequent reads in this TTL window stay hot.
                            if data.len() <= crate::cache::store::MAX_CACHEABLE_SIZE {
                                self.cache.put_resource(
                                    &cache_key,
                                    crate::cache::store::Resource {
                                        data: data.clone(),
                                        raw_json: entry.meta.raw_json.clone(),
                                    },
                                );
                            }
                            return Ok(data);
                        }
                    }
                }

                // Cache miss (or stale disk entry) — fetch from the connector.
                let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                let result = rt
                    .block_on(conn.read_resource(collection, resource))
                    .map_err(|e| {
                        tracing::error!("read_resource error: {}", e);
                        VfsError::IoError(e.to_string())
                    })?;

                let data = bytes::Bytes::from(result.content);

                self.content_lengths
                    .insert(cache_key.clone(), data.len() as u64);

                // L1: bound by the in-memory size cap to prevent OOM.
                if data.len() <= crate::cache::store::MAX_CACHEABLE_SIZE {
                    self.cache.put_resource(
                        &cache_key,
                        crate::cache::store::Resource {
                            data: data.clone(),
                            raw_json: result.raw_json.clone(),
                        },
                    );
                } else {
                    tracing::info!(
                        key = %cache_key,
                        size = data.len(),
                        "resource exceeds in-memory cache cap, on-disk only"
                    );
                }

                // L2: persist regardless of size, with the `updated_at` we
                // saw in the listing (if any). If no listing has been
                // populated yet, store with `None` and let the next read
                // refetch — better than handing back stale bytes.
                if let Some(disk) = &self.disk_cache {
                    let entry = DiskEntry {
                        data: data.clone(),
                        meta: DiskMeta {
                            id: resource.to_string(),
                            updated_at: listing_updated_at
                                .or_else(|| result.meta.updated_at.clone()),
                            fetched_at: chrono::Utc::now().to_rfc3339(),
                            raw_json: result.raw_json,
                        },
                    };
                    if let Err(e) = disk.put(connector, collection, resource, &entry) {
                        tracing::warn!(
                            connector = %connector,
                            collection = %collection,
                            resource = %resource,
                            error = %e,
                            "disk cache write failed"
                        );
                    }
                }

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
            NodeKind::TxDir {
                connector,
                collection,
            } => {
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
            NodeKind::TxDir {
                connector,
                collection,
            } => {
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
                        self.invalidate_resource_cache(connector, collection, resource);

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
    pub fn unlink_tx(&self, parent_id: u64, name: &str) -> Result<(), VfsError> {
        let parent_kind = self.nodes.get(parent_id).ok_or(VfsError::NotFound)?;
        match &parent_kind {
            NodeKind::Transaction {
                connector,
                collection,
                tx_name,
            } => {
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
            NodeKind::TxDir {
                connector,
                collection,
            } => {
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
        let mut tx_names: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        let k1 = NodeKind::Connector { name: "a".into() };
        let k2 = NodeKind::Connector { name: "b".into() };
        let i1 = table.allocate(k1);
        let i2 = table.allocate(k2);
        assert_ne!(i1, i2);
    }

    #[test]
    fn remove_cleans_both_maps() {
        let table = NodeTable::new();
        let kind = NodeKind::Connector { name: "rm".into() };
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

    #[test]
    fn stable_id_is_deterministic() {
        let kind = NodeKind::Resource {
            connector: "jira".into(),
            collection: "issues".into(),
            resource: "PROJ-123".into(),
            variant: ResourceVariant::Live,
        };
        let id1 = NodeTable::stable_id(&kind);
        let id2 = NodeTable::stable_id(&kind);
        assert_eq!(id1, id2);
        assert!(id1 >= 2, "IDs must be >= 2 (0=invalid, 1=root)");
    }

    #[test]
    fn collision_handling_gives_distinct_ids() {
        // Pre-occupy a slot, then allocate a different kind that would
        // hash to the same ID. The allocator must probe and return a
        // different ID for the second kind.
        let table = NodeTable::new();
        let kind_a = NodeKind::Connector { name: "a".into() };
        let id_a = table.allocate(kind_a.clone());

        // Manually insert a different kind at the same ID to simulate collision.
        let kind_b = NodeKind::Connector { name: "b".into() };
        let real_id_b = NodeTable::stable_id(&kind_b);

        // Force kind_b's slot to be occupied by kind_a (simulated collision).
        table.entries.insert(real_id_b, kind_a.clone());

        // Now allocate kind_b — it should detect the collision and probe.
        let id_b = table.allocate(kind_b.clone());

        // Both must be valid and distinct.
        assert_ne!(id_a, id_b);
        assert!(id_b >= 2);

        // Forward and reverse lookups must be consistent.
        assert_eq!(table.get(id_a), Some(kind_a));
        assert_eq!(table.get(id_b), Some(kind_b.clone()));
        assert_eq!(table.lookup(&kind_b), Some(id_b));
    }
}

#[cfg(test)]
mod disk_cache_integration {
    use super::*;
    use crate::cache::disk::DiskCache;
    use crate::connector::traits::{
        CollectionInfo, Connector, Resource as ConnResource, ResourceMeta, VersionInfo,
    };
    use crate::draft::store::DraftStore;
    use crate::governance::audit::AuditLogger;
    use crate::version::store::VersionStore;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    struct CountingConnector {
        reads: AtomicUsize,
        updated_at: Mutex<String>,
    }

    impl CountingConnector {
        fn new(updated_at: &str) -> Self {
            Self {
                reads: AtomicUsize::new(0),
                updated_at: Mutex::new(updated_at.into()),
            }
        }
        fn updated_at(&self) -> String {
            self.updated_at.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Connector for CountingConnector {
        fn name(&self) -> &str {
            "mock"
        }
        async fn list_collections(&self) -> anyhow::Result<Vec<CollectionInfo>> {
            Ok(vec![CollectionInfo {
                name: "things".into(),
                description: None,
            }])
        }
        async fn list_resources(&self, _: &str) -> anyhow::Result<Vec<ResourceMeta>> {
            Ok(vec![ResourceMeta {
                id: "alpha".into(),
                slug: "alpha".into(),
                title: None,
                updated_at: Some(self.updated_at()),
                content_type: None,
            }])
        }
        async fn read_resource(&self, _: &str, _: &str) -> anyhow::Result<ConnResource> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(ConnResource {
                meta: ResourceMeta {
                    id: "alpha".into(),
                    slug: "alpha".into(),
                    title: None,
                    updated_at: Some(self.updated_at()),
                    content_type: None,
                },
                content: b"hello world".to_vec(),
                raw_json: None,
            })
        }
        async fn write_resource(&self, _: &str, _: &str, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn resource_versions(&self, _: &str, _: &str) -> anyhow::Result<Vec<VersionInfo>> {
            Ok(vec![])
        }
        async fn read_version(&self, _: &str, _: &str, _: u32) -> anyhow::Result<ConnResource> {
            unimplemented!()
        }
    }

    fn build_vfs(disk_root: &Path) -> (Arc<VirtualFs>, Arc<CountingConnector>) {
        let conn = Arc::new(CountingConnector::new("2026-01-01T00:00:00Z"));
        let registry = ConnectorRegistry::new();
        registry.register(conn.clone() as Arc<dyn Connector>);
        let registry = Arc::new(registry);
        let cache = Arc::new(Cache::new(Duration::from_secs(60)));
        let drafts = Arc::new(DraftStore::new(disk_root.join("drafts")).unwrap());
        let versions = Arc::new(VersionStore::new(disk_root.join("versions")).unwrap());
        let audit = Arc::new(AuditLogger::new(disk_root.join("audit.log")).unwrap());
        let disk = Arc::new(DiskCache::new(disk_root.join("cache")).unwrap());
        let vfs = Arc::new(
            VirtualFs::new(registry, cache, drafts, versions, audit).with_disk_cache(disk),
        );
        (vfs, conn)
    }

    fn put_listing(vfs: &VirtualFs, updated_at: &str) {
        vfs.cache.put_metadata(
            "mock/things",
            vec![ResourceMeta {
                id: "alpha".into(),
                slug: "alpha".into(),
                title: None,
                updated_at: Some(updated_at.into()),
                content_type: None,
            }],
        );
    }

    #[test]
    fn second_read_after_l1_invalidation_hits_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let (vfs, conn) = build_vfs(tmp.path());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();

        let data1 = vfs
            .read_resource_data(&handle, "mock", "things", "alpha", &ResourceVariant::Live)
            .unwrap();
        assert_eq!(&data1[..], b"hello world");
        assert_eq!(conn.reads.load(Ordering::SeqCst), 1);

        // The first read populated the disk entry with no listing in cache,
        // so the disk meta has whatever updated_at the connector returned.
        // Pre-populate the metadata cache to match for the second read.
        put_listing(&vfs, "2026-01-01T00:00:00Z");

        // Simulate L1 TTL expiry without sleeping.
        vfs.cache.invalidate("mock/things/alpha");

        let data2 = vfs
            .read_resource_data(&handle, "mock", "things", "alpha", &ResourceVariant::Live)
            .unwrap();
        assert_eq!(&data2[..], b"hello world");
        assert_eq!(
            conn.reads.load(Ordering::SeqCst),
            1,
            "second read should be served from disk cache, not refetched"
        );
    }

    #[test]
    fn updated_at_change_forces_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let (vfs, conn) = build_vfs(tmp.path());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();

        // Warm L1 + L2 with the original updated_at via the listing.
        put_listing(&vfs, "2026-01-01T00:00:00Z");
        let _ = vfs
            .read_resource_data(&handle, "mock", "things", "alpha", &ResourceVariant::Live)
            .unwrap();
        assert_eq!(conn.reads.load(Ordering::SeqCst), 1);

        // L1 expires, listing now reports a newer updated_at than what's on
        // disk → disk entry is stale, must refetch.
        vfs.cache.invalidate("mock/things/alpha");
        put_listing(&vfs, "2026-04-01T00:00:00Z");

        let _ = vfs
            .read_resource_data(&handle, "mock", "things", "alpha", &ResourceVariant::Live)
            .unwrap();
        assert_eq!(
            conn.reads.load(Ordering::SeqCst),
            2,
            "stale disk entry must be refetched when listing reports a new updated_at"
        );
    }

    #[test]
    fn disk_cache_survives_vfs_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_owned();

        // Round 1: read once, populate disk.
        {
            let (vfs, conn) = build_vfs(&path);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            put_listing(&vfs, "2026-01-01T00:00:00Z");
            let _ = vfs
                .read_resource_data(
                    &rt.handle().clone(),
                    "mock",
                    "things",
                    "alpha",
                    &ResourceVariant::Live,
                )
                .unwrap();
            assert_eq!(conn.reads.load(Ordering::SeqCst), 1);
        }

        // Round 2: brand new VFS (fresh cache, fresh registry, same disk root).
        let (vfs, conn) = build_vfs(&path);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        put_listing(&vfs, "2026-01-01T00:00:00Z");
        let data = vfs
            .read_resource_data(
                &rt.handle().clone(),
                "mock",
                "things",
                "alpha",
                &ResourceVariant::Live,
            )
            .unwrap();
        assert_eq!(&data[..], b"hello world");
        assert_eq!(
            conn.reads.load(Ordering::SeqCst),
            0,
            "fresh-process read should be served entirely from disk"
        );
    }
}

/// Tests for VirtualFs::flush — the NFS write-then-flush path that promotes
/// new files to the API and updates existing ones without duplicate POSTs.
#[cfg(test)]
mod flush_promotion {
    use super::*;
    use crate::cache::disk::DiskCache;
    use crate::connector::traits::{
        CollectionInfo, Connector, Resource as ConnResource, ResourceMeta, VersionInfo,
    };
    use crate::draft::store::DraftStore;
    use crate::governance::audit::AuditLogger;
    use crate::version::store::VersionStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Connector that counts create_resource / write_resource calls.
    struct WritableConnector {
        creates: AtomicUsize,
        writes: AtomicUsize,
    }

    impl WritableConnector {
        fn new() -> Self {
            Self {
                creates: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Connector for WritableConnector {
        fn name(&self) -> &str {
            "mock"
        }
        async fn list_collections(&self) -> anyhow::Result<Vec<CollectionInfo>> {
            Ok(vec![CollectionInfo {
                name: "issues".into(),
                description: None,
            }])
        }
        async fn list_resources(&self, _: &str) -> anyhow::Result<Vec<ResourceMeta>> {
            Ok(vec![])
        }
        async fn read_resource(&self, _: &str, _: &str) -> anyhow::Result<ConnResource> {
            Err(anyhow::anyhow!("not found"))
        }
        async fn write_resource(&self, _: &str, _: &str, _: &[u8]) -> anyhow::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn create_resource(
            &self,
            _: &str,
            _: &[u8],
        ) -> anyhow::Result<ResourceMeta> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            Ok(ResourceMeta {
                id: "new-123".into(),
                slug: "new".into(),
                title: None,
                updated_at: None,
                content_type: None,
            })
        }
        async fn resource_versions(&self, _: &str, _: &str) -> anyhow::Result<Vec<VersionInfo>> {
            Ok(vec![])
        }
        async fn read_version(&self, _: &str, _: &str, _: u32) -> anyhow::Result<ConnResource> {
            unimplemented!()
        }
    }

    fn build_vfs(dir: &std::path::Path, conn: Arc<dyn Connector>) -> Arc<VirtualFs> {
        let registry = ConnectorRegistry::new();
        registry.register(conn);
        let registry = Arc::new(registry);
        let cache = Arc::new(Cache::new(Duration::from_secs(60)));
        let drafts = Arc::new(DraftStore::new(dir.join("drafts")).unwrap());
        let versions = Arc::new(VersionStore::new(dir.join("versions")).unwrap());
        let audit = Arc::new(AuditLogger::new(dir.join("audit.log")).unwrap());
        let disk = Arc::new(DiskCache::new(dir.join("cache")).unwrap());
        Arc::new(VirtualFs::new(registry, cache, drafts, versions, audit).with_disk_cache(disk))
    }

    fn make_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Creating a new .md file and flushing twice should POST once then PATCH.
    /// This covers the NFS write path: each NFS WRITE calls vfs.flush(), so
    /// multi-packet writes must not POST duplicate resources.
    #[test]
    fn flush_new_file_posts_once_then_patches() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Arc::new(WritableConnector::new());
        let vfs = build_vfs(tmp.path(), conn.clone() as Arc<dyn Connector>);
        let rt = make_rt();
        let handle = rt.handle().clone();

        let mock_id = vfs.lookup(&handle, 1, "mock").unwrap().id;
        let issues_id = vfs.lookup(&handle, mock_id, "issues").unwrap().id;

        // Create new.md (live resource buffered as draft)
        let attr = vfs.create(issues_id, "new.md").unwrap();
        let node_id = attr.id;

        // First write + flush → should POST (create_resource)
        vfs.write(node_id, 0, b"# My Issue\n").unwrap();
        vfs.flush(&handle, node_id).unwrap();

        assert_eq!(conn.creates.load(Ordering::SeqCst), 1, "first flush must POST");
        assert_eq!(conn.writes.load(Ordering::SeqCst), 0, "no PATCH on first flush");

        // Second write + flush → must PATCH (write_resource), not create again
        vfs.write(node_id, 0, b"# Updated\n").unwrap();
        vfs.flush(&handle, node_id).unwrap();

        assert_eq!(conn.creates.load(Ordering::SeqCst), 1, "create_resource must not fire again");
        assert_eq!(conn.writes.load(Ordering::SeqCst), 1, "second flush must PATCH");
    }

    /// Writing to a resource that already exists in the API must PATCH, never POST.
    /// The resource is identified by its _id in the frontmatter template, which is
    /// populated at create() time from the listing cache.
    #[test]
    fn flush_existing_resource_patches_not_posts() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Arc::new(WritableConnector::new());
        let vfs = build_vfs(tmp.path(), conn.clone() as Arc<dyn Connector>);
        let rt = make_rt();
        let handle = rt.handle().clone();

        let mock_id = vfs.lookup(&handle, 1, "mock").unwrap().id;
        let issues_id = vfs.lookup(&handle, mock_id, "issues").unwrap().id;

        // Simulate the resource already fetched from the API by pre-populating both
        // the resource cache and the listing cache (with matching API id).
        vfs.cache.put_resource(
            "mock/issues/existing",
            crate::cache::store::Resource {
                data: bytes::Bytes::from("# Old\n"),
                raw_json: None,
            },
        );
        vfs.cache.put_metadata(
            "mock/issues",
            vec![ResourceMeta {
                id: "existing-api-id".into(),
                slug: "existing".into(),
                title: None,
                updated_at: None,
                content_type: None,
            }],
        );

        let attr = vfs.create(issues_id, "existing.md").unwrap();
        vfs.write(attr.id, 0, b"# Edited\n").unwrap();
        vfs.flush(&handle, attr.id).unwrap();

        assert_eq!(conn.creates.load(Ordering::SeqCst), 0, "must not POST for existing resource");
        assert_eq!(conn.writes.load(Ordering::SeqCst), 1, "must PATCH existing resource");
    }
}

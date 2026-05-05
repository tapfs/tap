//! Draft state machine + write buffering: the four methods that turn user
//! writes into API calls.
//!
//! - `buffer_write` accumulates small NFS writes in memory before they hit
//!   the draft store on disk.
//! - `flush` is the heart of tapfs's write path: persist the buffer to a
//!   draft, then auto-promote Live resources (POST or PATCH) using the
//!   in-flight sentinel + idempotency key to keep the operation safe under
//!   retry / crash.
//! - `flush_all` is the daemon-shutdown sweep: persist buffers to disk, do
//!   NOT auto-promote (avoid API calls during teardown).
//! - `truncate` operates on the in-memory buffer when one exists, otherwise
//!   the draft on disk.
//!
//! Lives in its own file so the state machine reads top-to-bottom without
//! the rest of core.rs interleaved. The scattered entry points
//! (write/create/mkdir/unlink/rename) stay in core.rs for now — each is a
//! heterogeneous dispatcher, and a clean extraction would require pulling
//! their many helpers along with them.

use super::core::{max_write_buffer_size, VirtualFs};
use super::frontmatter::{
    classify_sentinel, inject_tapfs_fields, make_sentinel, parse_tapfs_meta, strip_tapfs_fields,
    SentinelState,
};
use super::path::lock_slug;
use super::types::*;

impl VirtualFs {
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
                                inject_tapfs_fields(&buf, existing_id, ex.version.unwrap_or(0))
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

                // is_new: _id absent/empty means never been to the API.
                // "__creating__@<ts>" is a sentinel written below to prevent
                // concurrent double-POSTs when NFS delivers two WRITE calls in
                // rapid succession. The embedded timestamp lets us distinguish
                // a still-in-flight POST (skip) from one whose daemon died
                // (retry — safe when the connector has idempotency_key_header
                // configured, otherwise risks a duplicate).
                let id_value = tapfs_meta.id.as_deref().unwrap_or("").trim();
                let sentinel_state = classify_sentinel(id_value);
                match sentinel_state {
                    SentinelState::Fresh => return Ok(()),
                    SentinelState::Stale | SentinelState::Legacy => {
                        tracing::warn!(
                            connector = %connector,
                            collection = %collection,
                            resource = %resource,
                            sentinel = %id_value,
                            "stale __creating__ sentinel — daemon likely crashed mid-POST. \
                             Retrying create_resource. Configure idempotency_key_header in \
                             the connector spec to make this retry safe against duplication."
                        );
                    }
                    SentinelState::NotSentinel => {}
                }
                let is_new = id_value.is_empty()
                    || matches!(
                        sentinel_state,
                        SentinelState::Stale | SentinelState::Legacy
                    );

                let api_id = if is_new {
                    // Write sentinel before the API call so a concurrent flush
                    // skips the create instead of sending a duplicate POST.
                    let sentinel = inject_tapfs_fields(&data, &make_sentinel(), 0);
                    let _ = self
                        .drafts
                        .write_draft(connector, collection, resource, &sentinel);

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
                    self.slug_map
                        .insert(connector, collection, &api_id, resource);
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

        // Aggregate collection flush: detect appended suffix and POST as new resource.
        if let NodeKind::Collection {
            connector,
            collection,
        } = &kind
        {
            if self.is_aggregate_collection(connector, collection) {
                if let Some((_, buf)) = self.write_buffers.remove(&id) {
                    let written =
                        std::str::from_utf8(&buf).map_err(|e| VfsError::IoError(e.to_string()))?;
                    let canonical = self.read_aggregate_collection(rt, connector, collection)?;
                    // Only act if content grew past the canonical prefix.
                    if written.len() > canonical.len()
                        && written.starts_with(canonical.trim_end_matches('\n'))
                    {
                        let suffix = written[canonical.trim_end_matches('\n').len()..]
                            .trim()
                            .to_string();
                        if !suffix.is_empty() {
                            let conn = self.registry.get(connector).ok_or(VfsError::NotFound)?;
                            match rt.block_on(conn.create_resource(collection, suffix.as_bytes())) {
                                Ok(_) => {
                                    self.cache
                                        .invalidate(&format!("{}/{}", connector, collection));
                                    let _ = self.audit.record(
                                        "create",
                                        connector,
                                        Some(collection),
                                        None,
                                        "success",
                                        Some(format!(
                                            "{} bytes appended to aggregate",
                                            suffix.len()
                                        )),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("aggregate append error: {}", e);
                                    return Err(VfsError::IoError(e.to_string()));
                                }
                            }
                        }
                    }
                }
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
    pub(crate) fn buffer_write(
        &self,
        id: u64,
        connector: &str,
        collection: &str,
        slug: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), VfsError> {
        // Lock discipline: never hold a `write_buffers` entry across I/O.
        // The previous shape called `drafts.read_draft(...)` inside
        // `or_insert_with`, which held the DashMap shard lock across a disk
        // read. With multiple concurrent writers to the same shard, that
        // serialized unrelated requests. Worse, if anyone later changed
        // `read_draft` to do anything async / network-y, this would silently
        // become a head-of-line block on every other writer that hashes to
        // the same shard.
        //
        // Snapshot the existing draft FIRST (no entry locked), then take the
        // entry only to mutate the in-memory buffer.
        let needs_seed = !self.write_buffers.contains_key(&id);
        let seed = if needs_seed {
            self.drafts
                .read_draft(connector, collection, slug)
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut entry = self
            .write_buffers
            .entry(id)
            .or_insert_with(|| seed);
        let buf = entry.value_mut();
        let off = offset as usize;
        let needed = off + data.len();
        // Bounded write buffer: a single resource cannot grow unbounded in
        // memory. NFS clients that try to upload a multi-GB file would
        // otherwise consume RAM and OOM the daemon — better to refuse the
        // write at the boundary with ENOSPC. Tunable for the rare case
        // where a user genuinely needs a larger buffer.
        if needed > max_write_buffer_size() {
            tracing::warn!(
                id,
                requested = needed,
                limit = max_write_buffer_size(),
                "write buffer would exceed TAPFS_MAX_WRITE_BUFFER; refusing"
            );
            return Err(VfsError::IoError(format!(
                "write buffer would exceed limit of {} bytes (set TAPFS_MAX_WRITE_BUFFER to override)",
                max_write_buffer_size()
            )));
        }
        if buf.len() < needed {
            buf.resize(needed, 0);
        }
        buf[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }
}

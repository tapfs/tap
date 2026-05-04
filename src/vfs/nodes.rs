//! NodeTable — stable u64 ↔ NodeKind allocator with deterministic IDs.
//!
//! Pulled out of `core.rs` because it's a self-contained data structure with
//! exactly one purpose. Determinism matters: the same NodeKind must produce
//! the same ID across daemon restarts so macOS File Provider's persistent
//! item-identifier cache and outstanding NFS file handles stay valid.

use dashmap::DashMap;

use super::types::*;

/// Thread-safe node allocation table.
///
/// Maps node IDs (u64) to their [`NodeKind`] descriptors.
/// The root node is always ID 1 and is pre-allocated at construction time.
pub struct NodeTable {
    /// Forward map: node ID -> kind.
    pub(crate) entries: DashMap<u64, NodeKind>,
    /// Reverse map: kind -> node ID, for fast lookup.
    pub(crate) reverse: DashMap<NodeKind, u64>,
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
    pub(crate) fn stable_id(kind: &NodeKind) -> u64 {
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

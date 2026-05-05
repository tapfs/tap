/// File type in the virtual filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsFileType {
    Directory,
    RegularFile,
}

/// A directory entry.
#[derive(Debug, Clone)]
pub struct VfsDirEntry {
    pub name: String,
    pub id: u64,
    pub file_type: VfsFileType,
}

/// File/directory attributes.
#[derive(Debug, Clone)]
pub struct VfsAttr {
    pub id: u64,
    pub size: u64,
    pub file_type: VfsFileType,
    pub perm: u16,
    pub mtime: Option<String>, // RFC3339 timestamp
}

/// What a node in the filesystem represents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Root,
    AgentMd,
    Connector {
        name: String,
    },
    Collection {
        connector: String,
        collection: String,
    },
    Resource {
        connector: String,
        collection: String,
        resource: String,
        variant: ResourceVariant,
    },
    Version {
        connector: String,
        collection: String,
        resource: String,
        version_id: Option<u64>,
    },
    /// Connector-level AGENTS.md: `/CONNECTOR/AGENTS.md`
    ConnectorAgentMd {
        connector: String,
    },
    /// Collection-level AGENTS.md: `/CONNECTOR/COLLECTION/AGENTS.md`
    CollectionAgentMd {
        connector: String,
        collection: String,
    },
    /// A synthetic directory that groups resources by a field value (e.g. GitHub org).
    /// Created when a collection spec has `group_by` set.
    GroupDir {
        connector: String,
        /// Collection whose resources are being grouped (e.g. "repos")
        collection: String,
        /// The group field value (e.g. "tapfs" for owner.login = "tapfs")
        group_value: String,
    },
    /// A resource that also acts as a directory because its collection spec has
    /// subcollections defined. Reading it as a file returns IsDirectory.
    ResourceDir {
        connector: String,
        /// Parent collection name (e.g. "repos")
        collection: String,
        /// Resource slug / api id (e.g. "tap")
        resource: String,
    },
    /// Transaction directory: `/CONNECTOR/COLLECTION/.tx/`
    TxDir {
        connector: String,
        collection: String,
    },
    /// Named transaction: `/CONNECTOR/COLLECTION/.tx/NAME/`
    Transaction {
        connector: String,
        collection: String,
        tx_name: String,
    },
    /// File inside a transaction
    TxResource {
        connector: String,
        collection: String,
        tx_name: String,
        resource: String,
    },
}

/// Resource variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceVariant {
    Live,
    Draft,
    Lock,
}

/// Errors that can occur during VFS operations.
///
/// Each variant maps cleanly to a `nfsstat3` (and an errno on FUSE) so the
/// kernel client gets the right signal — `EAGAIN` for "transient, retry me",
/// `EPERM` / `EACCES` for "stop trying", `EIO` strictly for "something is
/// genuinely broken." Collapsing everything to `IoError` (which is what an
/// earlier version of this enum did) made every failure look the same to the
/// client and broke retry behavior.
#[derive(Debug)]
pub enum VfsError {
    NotFound,
    NotDirectory,
    IsDirectory,
    PermissionDenied,
    AlreadyExists,
    CrossDevice,
    NotSupported,
    /// Operation is in flight on another path (e.g. `__creating__` sentinel
    /// is set, or a flush is mid-POST). Retry after a short delay.
    Busy,
    /// Upstream rate-limited us. The duration is the suggested wait; clients
    /// may use it to back off, but the NFS layer maps this to NFS3ERR_JUKEBOX
    /// regardless because the protocol has no way to express the duration.
    RateLimited(std::time::Duration),
    /// The fileid we were given used to be valid but no longer refers to a
    /// node we know about (e.g. the resource was deleted between the LOOKUP
    /// and the operation). Maps to NFS3ERR_STALE so the client re-resolves.
    StaleHandle,
    /// The write buffer was persisted to disk but the API call failed. The
    /// data isn't lost, but the upstream resource is out of sync. The string
    /// describes the underlying upstream failure for logs.
    PartialFlush(String),
    /// A draft on disk has invalid frontmatter or otherwise can't be parsed.
    /// Distinct from a generic IO error so callers can prompt the user to
    /// repair the draft instead of retrying it.
    DraftCorrupted(String),
    /// Buffer or quota exceeded. Maps to NFS3ERR_NOSPC / ENOSPC.
    NoSpace,
    /// Catch-all for genuinely unexpected I/O failures. New code should
    /// prefer one of the more specific variants above when it can — this
    /// variant exists because there are still many `anyhow::Error` paths
    /// in the codebase that haven't been refined yet.
    IoError(String),
}

impl std::fmt::Display for VfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::NotDirectory => f.write_str("not a directory"),
            Self::IsDirectory => f.write_str("is a directory"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::AlreadyExists => f.write_str("already exists"),
            Self::CrossDevice => f.write_str("cross-device link"),
            Self::NotSupported => f.write_str("operation not supported"),
            Self::Busy => f.write_str("resource busy — try again"),
            Self::RateLimited(d) => write!(f, "rate limited (retry after {:?})", d),
            Self::StaleHandle => f.write_str("stale file handle"),
            Self::PartialFlush(msg) => write!(f, "partial flush: {}", msg),
            Self::DraftCorrupted(msg) => write!(f, "draft corrupted: {}", msg),
            Self::NoSpace => f.write_str("no space left on device"),
            Self::IoError(msg) => write!(f, "I/O error: {}", msg),
        }
    }
}

impl std::error::Error for VfsError {}

impl From<anyhow::Error> for VfsError {
    fn from(err: anyhow::Error) -> Self {
        // Try to downcast to ConnectorError for structured mapping.
        if let Some(ce) = err.downcast_ref::<crate::connector::traits::ConnectorError>() {
            return match ce {
                crate::connector::traits::ConnectorError::NotFound(_) => VfsError::NotFound,
                crate::connector::traits::ConnectorError::PermissionDenied(_) => {
                    VfsError::PermissionDenied
                }
                crate::connector::traits::ConnectorError::NotSupported(_) => VfsError::NotSupported,
                crate::connector::traits::ConnectorError::RateLimited { retry_after, .. } => {
                    VfsError::RateLimited(retry_after.unwrap_or(std::time::Duration::from_secs(1)))
                }
                _ => VfsError::IoError(err.to_string()),
            };
        }
        VfsError::IoError(err.to_string())
    }
}

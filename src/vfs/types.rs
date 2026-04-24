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
    /// Connector-level agent.md: `/CONNECTOR/agent.md`
    ConnectorAgentMd {
        connector: String,
    },
    /// Collection-level agent.md: `/CONNECTOR/COLLECTION/agent.md`
    CollectionAgentMd {
        connector: String,
        collection: String,
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
#[derive(Debug)]
pub enum VfsError {
    NotFound,
    NotDirectory,
    IsDirectory,
    PermissionDenied,
    AlreadyExists,
    CrossDevice,
    NotSupported,
    IoError(String),
}

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
                _ => VfsError::IoError(err.to_string()),
            };
        }
        VfsError::IoError(err.to_string())
    }
}

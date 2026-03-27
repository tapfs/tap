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
    pub mtime: Option<String>,  // RFC3339 timestamp
}

/// What a node in the filesystem represents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Root,
    AgentMd,
    Connector { name: String },
    Collection { connector: String, collection: String },
    Resource { connector: String, collection: String, resource: String, variant: ResourceVariant },
    Version { connector: String, collection: String, resource: String, version_id: Option<u64> },
    /// Connector-level agent.md: /<connector>/agent.md
    ConnectorAgentMd { connector: String },
    /// Collection-level agent.md: /<connector>/<collection>/agent.md
    CollectionAgentMd { connector: String, collection: String },
    /// Transaction directory: /<connector>/<collection>/.tx/
    TxDir { connector: String, collection: String },
    /// Named transaction: /<connector>/<collection>/.tx/<name>/
    Transaction { connector: String, collection: String, tx_name: String },
    /// File inside a transaction
    TxResource { connector: String, collection: String, tx_name: String, resource: String },
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

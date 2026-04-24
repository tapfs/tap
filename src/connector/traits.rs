use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Typed connector errors for structured error handling.
///
/// Connectors can return these via `anyhow::Error::downcast()` when callers
/// need to distinguish error kinds (e.g. to map to FUSE/NFS error codes).
#[derive(Debug)]
pub enum ConnectorError {
    /// Resource or collection not found (HTTP 404).
    NotFound(String),
    /// Authentication or authorization failure (HTTP 401/403).
    PermissionDenied(String),
    /// Rate-limited by the API (HTTP 429).
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// Transient network or server error (HTTP 5xx, timeouts).
    NetworkError(String),
    /// The requested operation is not supported by this connector.
    NotSupported(String),
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {}", msg),
            Self::PermissionDenied(msg) => write!(f, "permission denied: {}", msg),
            Self::RateLimited { message, .. } => write!(f, "rate limited: {}", message),
            Self::NetworkError(msg) => write!(f, "network error: {}", msg),
            Self::NotSupported(msg) => write!(f, "not supported: {}", msg),
        }
    }
}

impl std::error::Error for ConnectorError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionInfo {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceMeta {
    pub id: String,
    pub slug: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub meta: ResourceMeta,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: u32,
    pub created_at: String,
    pub size: u64,
}

#[async_trait]
pub trait Connector: Send + Sync {
    fn name(&self) -> &str;
    async fn list_collections(&self) -> Result<Vec<CollectionInfo>>;
    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>>;
    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource>;
    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()>;
    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>>;
    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource>;
}

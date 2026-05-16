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
    /// Value of the `group_by` field from the spec (e.g. "tapfs" for owner.login).
    /// Used by the VFS to build synthetic group directories.
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub meta: ResourceMeta,
    pub content: Vec<u8>,
    /// Raw API response JSON, if available. Used by `tap inspect`.
    pub raw_json: Option<serde_json::Value>,
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

    /// Create a new resource in the given collection.
    /// Returns metadata for the newly created resource.
    /// List resources with their rendered content in one pass.
    /// Default implementation makes individual `read_resource` calls;
    /// connectors that already have full data in the listing response
    /// (e.g. REST comments) should override to avoid N+1 API calls.
    async fn list_resources_with_content(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Vec<u8>)>> {
        let metas = self.list_resources(collection).await?;
        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            match self.read_resource(collection, &meta.id).await {
                Ok(r) => out.push((meta, r.content)),
                Err(_) => out.push((meta, Vec::new())),
            }
        }
        Ok(out)
    }

    /// List resources alongside a per-item "frontmatter shard" — a partial
    /// JSON object containing fields the connector's spec says the list
    /// endpoint already populates. The VFS caches these shards so shallow
    /// reads (grep over frontmatter) can answer without firing the detail
    /// endpoint.
    ///
    /// Default impl: delegates to `list_resources` and returns `None` for
    /// every shard, preserving the conservative "detail-only" behavior.
    /// `RestConnector` overrides this to project the spec's `populates`
    /// fields from each list-response item.
    async fn list_resources_with_shards(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Option<serde_json::Value>)>> {
        let metas = self.list_resources(collection).await?;
        Ok(metas.into_iter().map(|m| (m, None)).collect())
    }

    async fn create_resource(&self, collection: &str, _content: &[u8]) -> Result<ResourceMeta> {
        Err(ConnectorError::NotSupported(format!(
            "create not supported for collection '{}'",
            collection
        ))
        .into())
    }

    /// Delete a resource from the given collection.
    async fn delete_resource(&self, collection: &str, _id: &str) -> Result<()> {
        Err(ConnectorError::NotSupported(format!(
            "delete not supported for collection '{}'",
            collection
        ))
        .into())
    }

    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>>;
    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource>;

    /// Search for resources within this connector. The default implementation
    /// returns [`ConnectorError::NotSupported`]; connectors that proxy a
    /// searchable API should override it.
    ///
    /// Used by the `upstream` search provider in [`crate::search`]. See
    /// `docs/proposals/search-providers.md`.
    async fn search_resources(
        &self,
        _collection: Option<&str>,
        _query: &str,
    ) -> Result<Vec<ResourceMeta>> {
        Err(ConnectorError::NotSupported(format!(
            "connector '{}' does not implement search",
            self.name()
        ))
        .into())
    }
}

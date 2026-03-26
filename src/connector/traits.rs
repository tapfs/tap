use async_trait::async_trait;
use anyhow::Result;
use serde::{Deserialize, Serialize};

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

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};
use crate::governance::audit::AuditLogger;

/// A wrapper around any [`Connector`] that logs every operation to an [`AuditLogger`].
pub struct AuditedConnector {
    inner: Arc<dyn Connector>,
    audit: Arc<AuditLogger>,
}

impl AuditedConnector {
    pub fn new(inner: Arc<dyn Connector>, audit: Arc<AuditLogger>) -> Self {
        Self { inner, audit }
    }
}

#[async_trait]
impl Connector for AuditedConnector {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        let connector_name = self.inner.name().to_string();
        match self.inner.list_collections().await {
            Ok(collections) => {
                let _ = self.audit.record(
                    "list",
                    &connector_name,
                    None,
                    None,
                    "success",
                    Some(format!("{} collections", collections.len())),
                );
                Ok(collections)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "list",
                    &connector_name,
                    None,
                    None,
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        let connector_name = self.inner.name().to_string();
        match self.inner.list_resources(collection).await {
            Ok(resources) => {
                let _ = self.audit.record(
                    "list",
                    &connector_name,
                    Some(collection),
                    None,
                    "success",
                    Some(format!("{} resources", resources.len())),
                );
                Ok(resources)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "list",
                    &connector_name,
                    Some(collection),
                    None,
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn list_resources_with_content(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Vec<u8>)>> {
        self.inner.list_resources_with_content(collection).await
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        let connector_name = self.inner.name().to_string();
        match self.inner.read_resource(collection, id).await {
            Ok(resource) => {
                let _ = self.audit.record(
                    "read",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "success",
                    Some(format!("{} bytes", resource.content.len())),
                );
                Ok(resource)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "read",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn create_resource(&self, collection: &str, content: &[u8]) -> Result<ResourceMeta> {
        let connector_name = self.inner.name().to_string();
        match self.inner.create_resource(collection, content).await {
            Ok(meta) => {
                let _ = self.audit.record(
                    "create",
                    &connector_name,
                    Some(collection),
                    Some(&meta.id),
                    "success",
                    Some(format!("{} bytes", content.len())),
                );
                Ok(meta)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "create",
                    &connector_name,
                    Some(collection),
                    None,
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn delete_resource(&self, collection: &str, id: &str) -> Result<()> {
        let connector_name = self.inner.name().to_string();
        match self.inner.delete_resource(collection, id).await {
            Ok(()) => {
                let _ = self.audit.record(
                    "delete",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "success",
                    None,
                );
                Ok(())
            }
            Err(e) => {
                let _ = self.audit.record(
                    "delete",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let connector_name = self.inner.name().to_string();
        match self.inner.write_resource(collection, id, content).await {
            Ok(()) => {
                let _ = self.audit.record(
                    "write",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "success",
                    Some(format!("{} bytes", content.len())),
                );
                Ok(())
            }
            Err(e) => {
                let _ = self.audit.record(
                    "write",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>> {
        let connector_name = self.inner.name().to_string();
        match self.inner.resource_versions(collection, id).await {
            Ok(versions) => {
                let _ = self.audit.record(
                    "list_versions",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "success",
                    Some(format!("{} versions", versions.len())),
                );
                Ok(versions)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "list_versions",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }

    async fn search_resources(
        &self,
        collection: Option<&str>,
        query: &str,
    ) -> Result<Vec<ResourceMeta>> {
        let connector_name = self.inner.name().to_string();
        match self.inner.search_resources(collection, query).await {
            Ok(metas) => {
                let _ = self.audit.record(
                    "search",
                    &connector_name,
                    collection,
                    None,
                    "success",
                    Some(format!("q={:?} hits={}", query, metas.len())),
                );
                Ok(metas)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "search",
                    &connector_name,
                    collection,
                    None,
                    "error",
                    Some(format!("q={:?}: {}", query, e)),
                );
                Err(e)
            }
        }
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        let connector_name = self.inner.name().to_string();
        match self.inner.read_version(collection, id, version).await {
            Ok(resource) => {
                let _ = self.audit.record(
                    "read_version",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "success",
                    Some(format!("v{}, {} bytes", version, resource.content.len())),
                );
                Ok(resource)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "read_version",
                    &connector_name,
                    Some(collection),
                    Some(id),
                    "error",
                    Some(e.to_string()),
                );
                Err(e)
            }
        }
    }
}

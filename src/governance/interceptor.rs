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

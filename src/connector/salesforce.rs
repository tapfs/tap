// Salesforce connector stub.
//
// Future implementation notes:
// - Authentication will use the `oauth2` crate to perform the OAuth 2.0
//   JWT Bearer or Web Server flow against Salesforce's token endpoint
//   (https://login.salesforce.com/services/oauth2/token).
// - Queries will be issued via SOQL through the REST API
//   (/services/data/vXX.0/query?q=...).
// - Collections will map to Salesforce SObjects (Account, Contact, etc.).
// - list_resources will execute SOQL SELECT queries.
// - read_resource / write_resource will use the SObject rows endpoint
//   (/services/data/vXX.0/sobjects/{SObject}/{id}).
// - resource_versions may leverage Salesforce's record history tracking.

use async_trait::async_trait;
use anyhow::{anyhow, Result};

use crate::connector::traits::{
    CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo,
};

pub struct SalesforceConnector {
    instance_url: String,
}

impl SalesforceConnector {
    pub fn new(instance_url: &str) -> Self {
        Self {
            instance_url: instance_url.to_string(),
        }
    }
}

#[async_trait]
impl Connector for SalesforceConnector {
    fn name(&self) -> &str {
        "salesforce"
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Err(anyhow!(
            "salesforce connector: list_collections is not yet implemented (instance: {})",
            self.instance_url
        ))
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        Err(anyhow!(
            "salesforce connector: list_resources for '{}' is not yet implemented",
            collection
        ))
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        Err(anyhow!(
            "salesforce connector: read_resource for '{}/{}' is not yet implemented",
            collection,
            id
        ))
    }

    async fn write_resource(&self, collection: &str, id: &str, _content: &[u8]) -> Result<()> {
        Err(anyhow!(
            "salesforce connector: write_resource for '{}/{}' is not yet implemented",
            collection,
            id
        ))
    }

    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>> {
        Err(anyhow!(
            "salesforce connector: resource_versions for '{}/{}' is not yet implemented",
            collection,
            id
        ))
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        Err(anyhow!(
            "salesforce connector: read_version for '{}/{} v{}' is not yet implemented",
            collection,
            id,
            version
        ))
    }
}

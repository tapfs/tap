use serde::{Deserialize, Serialize};
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSpec {
    pub name: String,
    pub base_url: String,
    pub auth: Option<AuthSpec>,
    pub collections: Vec<CollectionSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSpec {
    #[serde(rename = "type")]
    pub auth_type: String, // "bearer", "basic", "oauth2"
    pub token_env: Option<String>, // env var name for token
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSpec {
    pub name: String,
    pub list_endpoint: String,          // e.g. "/api/items"
    pub get_endpoint: String,           // e.g. "/api/items/{id}"
    pub update_endpoint: Option<String>,
    pub id_field: Option<String>,       // field name for ID, default "id"
    pub slug_field: Option<String>,     // field for slug, default "slug" or "id"
    pub title_field: Option<String>,
    pub list_root: Option<String>,      // JSON path for list results, e.g. "data" or "records"
}

impl ConnectorSpec {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(yaml)?)
    }
}

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
    pub list_endpoint: String, // e.g. "/api/items"
    pub get_endpoint: String,  // e.g. "/api/items/{id}"
    pub update_endpoint: Option<String>,
    pub id_field: Option<String>,   // field name for ID, default "id"
    pub slug_field: Option<String>, // field for slug, default "slug" or "id"
    pub title_field: Option<String>,
    pub list_root: Option<String>, // JSON path for list results, e.g. "data" or "records"
    pub render: Option<RenderSpec>,
    pub compose: Option<Vec<ComposeSpec>>,
}

/// Controls how a JSON API response is rendered into a readable markdown file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderSpec {
    /// Fields to include in YAML frontmatter. Supports dot-paths ("user.login")
    /// and renaming ("user.login as author").
    pub frontmatter: Option<Vec<String>>,
    /// JSON field whose value becomes the markdown body.
    pub body: Option<String>,
    /// Additional sections rendered after the body.
    pub sections: Option<Vec<SectionSpec>>,
    /// Field patterns to exclude from output (exact names or ".*_url" regex).
    pub exclude: Option<Vec<String>>,
}

/// A named section rendered from a JSON field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionSpec {
    pub name: String,
    pub field: String,
    /// "list" (bullet list), "table", or "text" (default).
    pub format: Option<String>,
    /// Template for each item, e.g. "{name}" or "{user.login} ({created_at})".
    pub item_template: Option<String>,
}

/// A sub-resource fetched and appended to the main resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeSpec {
    pub name: String,
    /// Endpoint template — `{id}` is replaced with the resource ID.
    pub endpoint: String,
    pub list_root: Option<String>,
    /// Template for each item.
    pub item_template: Option<String>,
}

impl ConnectorSpec {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(yaml)?)
    }
}

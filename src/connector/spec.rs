use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSpec {
    /// Spec schema version for forward compatibility (e.g. "1").
    pub spec_version: Option<String>,
    /// Connector's own semver version (e.g. "1.0.0").
    pub version: Option<String>,
    /// Human/agent-readable description — powers agent.md generation.
    pub description: Option<String>,
    pub name: String,
    pub base_url: String,
    pub auth: Option<AuthSpec>,
    /// Transport configuration (defaults to REST using base_url).
    pub transport: Option<TransportSpec>,
    /// Connector-level capability declarations.
    pub capabilities: Option<CapabilitiesSpec>,
    /// Agent guidance — tips and hints rendered into agent.md.
    pub agent: Option<AgentSpec>,
    pub collections: Vec<CollectionSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSpec {
    #[serde(rename = "type")]
    pub auth_type: String, // "bearer", "basic", "oauth2"
    pub token_env: Option<String>, // env var name for token
}

/// Transport abstraction — how to reach the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportSpec {
    /// Transport type: "rest" (default), "mcp", "graphql", "stdio".
    #[serde(rename = "type")]
    pub transport_type: String,
    /// For MCP transport: command to spawn the MCP server.
    pub command: Option<Vec<String>>,
    /// For MCP transport: environment variables for the server process.
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// Connector-level capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesSpec {
    pub read: Option<bool>,
    pub write: Option<bool>,
    pub create: Option<bool>,
    pub delete: Option<bool>,
    pub drafts: Option<bool>,
    pub versions: Option<bool>,
    pub search: Option<bool>,
    /// Pagination strategy: "cursor", "offset", "page", "link_header", "none".
    pub pagination: Option<String>,
    /// Rate limiting hints.
    pub rate_limit: Option<RateLimitSpec>,
}

/// Rate limit hints for agent.md and request throttling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSpec {
    pub requests_per_minute: Option<u32>,
    pub burst: Option<u32>,
}

/// Agent guidance section — natural language hints for agent.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Domain-specific tips rendered into the connector's agent.md.
    pub tips: Option<Vec<String>>,
    /// Cross-collection relationship descriptions for agent context.
    pub relationships: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSpec {
    pub name: String,
    /// Human/agent-readable description of this collection.
    pub description: Option<String>,
    /// Hint about how slugs map to resources (e.g. "issue number", "company name").
    pub slug_hint: Option<String>,
    /// Supported operations for this collection: "read", "write", "draft", "lock", "versions".
    pub operations: Option<Vec<String>>,
    pub list_endpoint: String, // e.g. "/api/items"
    pub get_endpoint: String,  // e.g. "/api/items/{id}"
    pub update_endpoint: Option<String>,
    pub id_field: Option<String>,   // field name for ID, default "id"
    pub slug_field: Option<String>, // field for slug, default "slug" or "id"
    pub title_field: Option<String>,
    pub list_root: Option<String>, // JSON path for list results, e.g. "data" or "records"
    pub render: Option<RenderSpec>,
    pub compose: Option<Vec<ComposeSpec>>,
    /// Declarative operations beyond CRUD (e.g. status transitions).
    pub operations_spec: Option<Vec<OperationSpec>>,
    /// Declared relationships to other collections.
    pub relationships: Option<Vec<RelationshipSpec>>,
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

/// A declarative operation beyond CRUD (e.g. status transitions, workflow triggers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationSpec {
    pub name: String,
    pub description: Option<String>,
    pub endpoint: String,
    pub method: Option<String>,
    /// How the operation is triggered: "frontmatter" (field change), "command" (explicit).
    pub trigger: Option<String>,
    /// For frontmatter triggers: which field to watch.
    pub trigger_field: Option<String>,
    /// For frontmatter triggers: the value that activates the operation.
    pub trigger_value: Option<String>,
    /// Request body template (JSON string with `{id}` substitution).
    pub body: Option<String>,
}

/// A declared relationship between collections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipSpec {
    /// Target collection name.
    pub target: String,
    /// Relationship type: "one-to-many", "many-to-many", "one-to-one".
    #[serde(rename = "type")]
    pub relationship_type: Option<String>,
    /// Human-readable description of the relationship.
    pub description: Option<String>,
}

impl ConnectorSpec {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(yaml)?)
    }
}

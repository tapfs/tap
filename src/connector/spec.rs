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
    /// URL where the user can create/find their API key.
    pub setup_url: Option<String>,
    /// Human-readable instructions for obtaining credentials.
    pub setup_instructions: Option<String>,
    /// OAuth2 authorization endpoint.
    pub auth_url: Option<String>,
    /// OAuth2 token endpoint.
    pub token_url: Option<String>,
    /// OAuth2 client ID.
    pub client_id: Option<String>,
    /// OAuth2 client secret.
    pub client_secret: Option<String>,
    /// Space-separated OAuth2 scopes.
    pub scopes: Option<String>,
    /// OAuth2 Device Flow code endpoint (e.g. https://github.com/login/device/code).
    pub device_code_url: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    pub create_endpoint: Option<String>,
    pub delete_endpoint: Option<String>,
    /// When set, delete_resource sends PATCH (instead of DELETE) to
    /// `delete_endpoint` with this JSON body. Used by APIs that soft-delete
    /// via an archive flag (e.g. Notion's `{"archived": true}`).
    pub delete_body: Option<String>,
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
    /// URL placeholder name the parent resource fills in this subcollection's endpoints.
    /// e.g. for list_endpoint "/repos/{repo}/issues", parent_param = "repo".
    pub parent_param: Option<String>,
    /// Nested collections available under each resource of this collection.
    pub subcollections: Option<Vec<CollectionSpec>>,
    /// JSON dotpath to group resources by in the VFS (e.g. "owner.login").
    /// When set, resources are shown under synthetic group directories instead
    /// of directly in the collection directory.
    pub group_by: Option<String>,
    /// When true, the collection appears as a single aggregated `.md` file
    /// rather than a directory of individual resource files.
    /// Appending to the file creates a new resource (POST).
    pub aggregate: Option<bool>,
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
        let spec: Self = serde_yaml::from_str(yaml)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Validate spec invariants that serde can't express.
    fn validate(&self) -> Result<()> {
        for coll in &self.collections {
            validate_collection(&self.name, coll)?;
        }
        Ok(())
    }
}

fn validate_collection(spec_name: &str, coll: &CollectionSpec) -> Result<()> {
    if let Some(body) = coll.delete_body.as_deref() {
        validate_delete_body_json(spec_name, &coll.name, body)?;
    }
    if let Some(subs) = coll.subcollections.as_ref() {
        for sub in subs {
            validate_collection(spec_name, sub)?;
        }
    }
    Ok(())
}

/// `delete_body` is sent verbatim (after `{id}` substitution) as a PATCH body
/// with `Content-Type: application/json`. If the spec author writes invalid
/// JSON we'd previously find out only when a user ran `rmdir` on a resource —
/// long after the spec was loaded. Validate at load time.
///
/// The placeholder is substituted with `0` (a valid JSON scalar) before
/// parsing so the template itself doesn't need to be self-parseable. This
/// catches structural problems (unclosed braces, missing commas, etc.) but
/// not all runtime substitution edge cases — e.g. `{"count": {id}}` parses
/// fine here but breaks at runtime if the id isn't numeric.
fn validate_delete_body_json(spec_name: &str, coll_name: &str, body: &str) -> Result<()> {
    let with_dummy = body.replace("{id}", "0");
    serde_json::from_str::<serde_json::Value>(&with_dummy).map_err(|e| {
        anyhow::anyhow!(
            "spec '{}' collection '{}': delete_body is not valid JSON ({}): {}",
            spec_name,
            coll_name,
            e,
            body
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::builtin::{builtin_names, builtin_spec};

    #[test]
    fn all_builtin_specs_parse() {
        for name in builtin_names() {
            let Some(yaml) = builtin_spec(name) else {
                continue;
            }; // skip native connectors
            ConnectorSpec::from_yaml(yaml)
                .unwrap_or_else(|e| panic!("failed to parse spec '{name}': {e}"));
        }
    }

    #[test]
    fn all_specs_have_required_fields() {
        for name in builtin_names() {
            let Some(yaml) = builtin_spec(name) else {
                continue;
            };
            let spec = ConnectorSpec::from_yaml(yaml).unwrap();

            assert!(!spec.name.is_empty(), "spec '{name}' has empty name");
            assert!(
                !spec.base_url.is_empty(),
                "spec '{name}' has empty base_url"
            );
            assert!(
                !spec.collections.is_empty(),
                "spec '{name}' has no collections"
            );

            for col in &spec.collections {
                assert!(
                    !col.name.is_empty(),
                    "spec '{name}' has a collection with empty name"
                );
                assert!(
                    !col.list_endpoint.is_empty(),
                    "spec '{name}' collection '{}' has empty list_endpoint",
                    col.name
                );
                assert!(
                    !col.get_endpoint.is_empty(),
                    "spec '{name}' collection '{}' has empty get_endpoint",
                    col.name
                );
            }
        }
    }

    #[test]
    fn spec_name_matches_builtin_key() {
        for key in builtin_names() {
            let Some(yaml) = builtin_spec(key) else {
                continue;
            };
            let spec = ConnectorSpec::from_yaml(yaml).unwrap();
            assert_eq!(
                spec.name, *key,
                "spec name '{}' does not match builtin key '{key}'",
                spec.name
            );
        }
    }

    #[test]
    fn get_endpoints_contain_id_placeholder() {
        for name in builtin_names() {
            let Some(yaml) = builtin_spec(name) else {
                continue;
            };
            let spec = ConnectorSpec::from_yaml(yaml).unwrap();

            for col in &spec.collections {
                assert!(
                    col.get_endpoint.contains("{id}"),
                    "spec '{name}' collection '{}' get_endpoint '{}' missing {{id}} placeholder",
                    col.name,
                    col.get_endpoint
                );
            }
        }
    }

    #[test]
    fn delete_body_invalid_json_rejected_at_load() {
        let yaml = r#"
name: test
base_url: https://example.com
collections:
  - name: pages
    list_endpoint: /pages
    get_endpoint: /pages/{id}
    delete_endpoint: /pages/{id}
    delete_body: '{ "archived": true,, }'
"#;
        let err = ConnectorSpec::from_yaml(yaml).expect_err("expected invalid-JSON error");
        let msg = format!("{:#}", err);
        assert!(msg.contains("delete_body is not valid JSON"), "got: {}", msg);
    }

    #[test]
    fn delete_body_with_id_placeholder_validates() {
        // {id} placeholder is substituted with 0 for validation, so a body
        // referencing {id} inside a string must still parse.
        let yaml = r#"
name: test
base_url: https://example.com
collections:
  - name: pages
    list_endpoint: /pages
    get_endpoint: /pages/{id}
    delete_endpoint: /pages/{id}
    delete_body: '{"archived": true, "deleted_id": "{id}"}'
"#;
        ConnectorSpec::from_yaml(yaml).expect("should accept valid body with {id}");
    }

    #[test]
    fn delete_body_validation_recurses_into_subcollections() {
        let yaml = r#"
name: test
base_url: https://example.com
collections:
  - name: parent
    list_endpoint: /parent
    get_endpoint: /parent/{id}
    subcollections:
      - name: child
        list_endpoint: /parent/{id}/child
        get_endpoint: /parent/{id}/child/{id}
        delete_endpoint: /parent/{id}/child/{id}
        delete_body: 'not json at all'
"#;
        let err =
            ConnectorSpec::from_yaml(yaml).expect_err("expected error from nested invalid body");
        assert!(format!("{:#}", err).contains("delete_body is not valid JSON"));
    }

    #[test]
    fn compose_endpoints_contain_id_placeholder() {
        for name in builtin_names() {
            let Some(yaml) = builtin_spec(name) else {
                continue;
            };
            let spec = ConnectorSpec::from_yaml(yaml).unwrap();

            for col in &spec.collections {
                if let Some(composes) = &col.compose {
                    for compose in composes {
                        assert!(
                            compose.endpoint.contains("{id}"),
                            "spec '{name}' collection '{}' compose '{}' endpoint '{}' missing {{id}} placeholder",
                            col.name,
                            compose.name,
                            compose.endpoint
                        );
                    }
                }
            }
        }
    }
}

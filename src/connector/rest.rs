use async_trait::async_trait;
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::Value;

use crate::connector::spec::{ConnectorSpec, CollectionSpec};
use crate::connector::traits::{
    CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo,
};

/// Generic REST connector driven by a ConnectorSpec.
///
/// Translates the spec's endpoint templates into HTTP calls using reqwest,
/// and renders JSON responses as Markdown (YAML frontmatter + body).
pub struct RestConnector {
    spec: ConnectorSpec,
    client: Client,
    token: Option<String>,
    /// Maps "collection/slug" → API resource ID for endpoint substitution.
    slug_to_id: dashmap::DashMap<String, String>,
}

impl RestConnector {
    /// Create a new RestConnector from a spec.
    ///
    /// If the spec defines auth with a `token_env`, the corresponding
    /// environment variable is read at construction time.
    pub fn new(spec: ConnectorSpec, client: Client) -> Self {
        let token = spec
            .auth
            .as_ref()
            .and_then(|auth| auth.token_env.as_ref())
            .and_then(|env_var| std::env::var(env_var).ok());

        Self {
            spec,
            client,
            token,
            slug_to_id: dashmap::DashMap::new(),
        }
    }

    /// Build a full URL by joining the base URL with a path.
    fn url(&self, path: &str) -> String {
        let base = self.spec.base_url.trim_end_matches('/');
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        format!("{}{}", base, path)
    }

    /// Substitute `{id}` in an endpoint template.
    fn substitute_id(endpoint: &str, id: &str) -> String {
        endpoint.replace("{id}", id)
    }

    /// Find the CollectionSpec by name.
    fn find_collection(&self, name: &str) -> Result<&CollectionSpec> {
        self.spec
            .collections
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| anyhow!("collection '{}' not found in spec '{}'", name, self.spec.name))
    }

    /// Add authentication and standard headers to a request builder.
    fn authenticate(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder
            .header("User-Agent", "tapfs/0.1")
            .header("Accept", "application/json");
        match (&self.spec.auth, &self.token) {
            (Some(auth), Some(token)) => match auth.auth_type.as_str() {
                "bearer" => builder.bearer_auth(token),
                "basic" => builder.header("Authorization", format!("Basic {}", token)),
                _ => builder.bearer_auth(token),
            },
            _ => builder,
        }
    }

    /// Extract the array of items from a JSON response.
    ///
    /// If `list_root` is specified (e.g. "data" or "records"), the array is
    /// pulled from that key. Otherwise the top-level value is expected to be
    /// an array.
    fn extract_list<'a>(json: &'a Value, list_root: Option<&str>) -> Result<&'a Vec<Value>> {
        let target = match list_root {
            Some(root) => {
                // Support dotted paths like "data.items"
                let mut current = json;
                for segment in root.split('.') {
                    current = current
                        .get(segment)
                        .ok_or_else(|| anyhow!("list_root segment '{}' not found in response", segment))?;
                }
                current
            }
            None => json,
        };

        target
            .as_array()
            .ok_or_else(|| anyhow!("expected JSON array in list response"))
    }

    /// Extract a ResourceMeta from a single JSON object using the field
    /// mappings defined in the CollectionSpec.
    fn extract_meta(item: &Value, coll: &CollectionSpec) -> ResourceMeta {
        let id_field = coll.id_field.as_deref().unwrap_or("id");
        let slug_field = coll.slug_field.as_deref().unwrap_or(id_field);
        let title_field = coll.title_field.as_deref();

        let id = get_nested(item, id_field)
            .map(|v| json_value_to_string(v))
            .unwrap_or_default();

        let slug = get_nested(item, slug_field)
            .map(|v| sanitize_slug(&json_value_to_string(v)))
            .unwrap_or_else(|| sanitize_slug(&id));

        let title = title_field.and_then(|f| get_nested(item, f).map(|v| json_value_to_string(v)));

        let updated_at = item
            .get("updated_at")
            .or_else(|| item.get("updatedAt"))
            .or_else(|| item.get("SystemModstamp"))
            .or_else(|| item.get("LastModifiedDate"))
            .map(|v| json_value_to_string(v));

        ResourceMeta {
            id,
            slug,
            title,
            updated_at,
            content_type: Some("application/json".to_string()),
        }
    }

    /// Render a JSON value as Markdown with YAML frontmatter.
    ///
    /// Layout:
    /// ```text
    /// ---
    /// id: "123"
    /// title: "My Resource"
    /// ---
    ///
    /// | Field | Value |
    /// |-------|-------|
    /// | key   | val   |
    /// ```
    ///
    /// For nested or complex values, a JSON code block is appended instead.
    fn render_markdown(meta: &ResourceMeta, json: &Value) -> Vec<u8> {
        let mut out = String::new();

        // YAML frontmatter
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", meta.id));
        out.push_str(&format!("slug: \"{}\"\n", meta.slug));
        if let Some(title) = &meta.title {
            out.push_str(&format!("title: \"{}\"\n", title));
        }
        if let Some(updated) = &meta.updated_at {
            out.push_str(&format!("updated_at: \"{}\"\n", updated));
        }
        out.push_str("---\n\n");

        // Body: render as table for flat objects, code block otherwise
        match json {
            Value::Object(map) => {
                let (simple, complex): (Vec<_>, Vec<_>) = map
                    .iter()
                    .partition(|(_, v)| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null));

                if !simple.is_empty() {
                    out.push_str("| Field | Value |\n");
                    out.push_str("|-------|-------|\n");
                    for (key, val) in &simple {
                        let display = json_value_to_string(val);
                        out.push_str(&format!("| {} | {} |\n", key, display));
                    }
                    out.push('\n');
                }

                if !complex.is_empty() {
                    for (key, val) in &complex {
                        out.push_str(&format!("### {}\n\n", key));
                        out.push_str("```json\n");
                        out.push_str(
                            &serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string()),
                        );
                        out.push_str("\n```\n\n");
                    }
                }
            }
            _ => {
                out.push_str("```json\n");
                out.push_str(
                    &serde_json::to_string_pretty(json).unwrap_or_else(|_| json.to_string()),
                );
                out.push_str("\n```\n");
            }
        }

        out.into_bytes()
    }
}

/// Sanitize a string for use as a filesystem slug.
fn sanitize_slug(s: &str) -> String {
    s.replace('/', "-")
        .replace('\\', "-")
        .replace('\0', "")
        .trim()
        .to_string()
}

/// Navigate a dotted path like "properties.name" into a JSON value.
fn get_nested<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Convert a serde_json Value to a display string (without quotes for strings).
fn json_value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[async_trait]
impl Connector for RestConnector {
    fn name(&self) -> &str {
        &self.spec.name
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Ok(self
            .spec
            .collections
            .iter()
            .map(|c| CollectionInfo {
                name: c.name.clone(),
                description: None,
            })
            .collect())
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        let coll = self.find_collection(collection)?;
        let url = self.url(&coll.list_endpoint);

        let request = self.client.get(&url);
        let request = self.authenticate(request);

        let response = request
            .send()
            .await
            .context("failed to fetch resource list")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_resources failed: HTTP {} — {}",
                status,
                body
            ));
        }

        let json: Value = response
            .json()
            .await
            .context("failed to parse list response as JSON")?;

        let items = Self::extract_list(&json, coll.list_root.as_deref())?;

        let metas: Vec<ResourceMeta> = items
            .iter()
            .map(|item| Self::extract_meta(item, coll))
            .collect();

        // Cache slug → ID mappings for read/write resolution
        for m in &metas {
            if m.slug != m.id {
                let key = format!("{}/{}", collection, m.slug);
                self.slug_to_id.insert(key, m.id.clone());
            }
        }

        Ok(metas)
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        let coll = self.find_collection(collection)?;
        let resolved_id = self.slug_to_id
            .get(&format!("{}/{}", collection, id))
            .map(|v| v.clone())
            .unwrap_or_else(|| id.to_string());
        let path = Self::substitute_id(&coll.get_endpoint, &resolved_id);
        let url = self.url(&path);

        let request = self.client.get(&url);
        let request = self.authenticate(request);

        let response = request
            .send()
            .await
            .context("failed to fetch resource")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "read_resource failed: HTTP {} — {}",
                status,
                body
            ));
        }

        let json: Value = response
            .json()
            .await
            .context("failed to parse resource response as JSON")?;

        let meta = Self::extract_meta(&json, coll);
        let content = Self::render_markdown(&meta, &json);

        Ok(Resource { meta, content })
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let coll = self.find_collection(collection)?;
        let resolved_id = self.slug_to_id
            .get(&format!("{}/{}", collection, id))
            .map(|v| v.clone())
            .unwrap_or_else(|| id.to_string());
        let endpoint = coll
            .update_endpoint
            .as_deref()
            .unwrap_or(&coll.get_endpoint);
        let path = Self::substitute_id(endpoint, &resolved_id);
        let url = self.url(&path);

        // Parse the incoming content as JSON.  If the content looks like
        // Markdown with YAML frontmatter (starts with "---"), attempt to
        // extract the JSON code block; otherwise treat the whole body as JSON.
        let body_json: Value = if content.starts_with(b"---") {
            // Try to find a ```json code block and parse that
            let text = std::str::from_utf8(content).context("content is not valid UTF-8")?;
            let json_block = text
                .split("```json")
                .nth(1)
                .and_then(|after| after.split("```").next())
                .unwrap_or(text);
            serde_json::from_str(json_block.trim())
                .context("failed to parse JSON from markdown content")?
        } else {
            serde_json::from_slice(content).context("failed to parse content as JSON")?
        };

        let request = self.client.patch(&url).json(&body_json);
        let request = self.authenticate(request);

        let response = request
            .send()
            .await
            .context("failed to send write request")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "write_resource failed: HTTP {} — {}",
                status,
                body
            ));
        }

        Ok(())
    }

    async fn resource_versions(&self, _collection: &str, _id: &str) -> Result<Vec<VersionInfo>> {
        // Versioning is not supported by the generic REST connector.
        // Individual API-specific connectors can override this.
        Ok(vec![])
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        if version == 0 {
            // Treat version 0 as "current"
            return self.read_resource(collection, id).await;
        }
        Err(anyhow!(
            "versioned reads are not supported by the generic REST connector"
        ))
    }
}

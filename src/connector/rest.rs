use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;

use crate::connector::spec::{CollectionSpec, ConnectorSpec};
use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};

const MAX_ERROR_BODY_LEN: usize = 512;

fn truncate_error_body(body: &str) -> &str {
    if body.len() <= MAX_ERROR_BODY_LEN {
        body
    } else {
        // Find a char boundary to avoid panics on multi-byte UTF-8.
        let mut end = MAX_ERROR_BODY_LEN;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        &body[..end]
    }
}

/// Generic REST connector driven by a ConnectorSpec.
///
/// Translates the spec's endpoint templates into HTTP calls using reqwest,
/// and renders JSON responses as Markdown (YAML frontmatter + body).
/// Maximum concurrent HTTP requests per connector.
const MAX_CONCURRENT_REQUESTS: usize = 10;

pub struct RestConnector {
    spec: ConnectorSpec,
    client: Client,
    token: Option<String>,
    /// Maps "collection/slug" → API resource ID for endpoint substitution.
    slug_to_id: dashmap::DashMap<String, String>,
    /// Limits concurrent HTTP requests to prevent overwhelming APIs.
    request_semaphore: tokio::sync::Semaphore,
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
            request_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS),
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
            .ok_or_else(|| {
                anyhow!(
                    "collection '{}' not found in spec '{}'",
                    name,
                    self.spec.name
                )
            })
    }

    /// Send a request with retry + exponential backoff for transient errors.
    ///
    /// Retries on:
    /// - HTTP status codes 429 (Too Many Requests), 502, 503 (server errors)
    /// - Network errors: timeouts, connection refused, DNS failures
    ///
    /// Non-retryable errors (e.g. invalid URL) are returned immediately.
    ///
    /// Acquires a permit from the concurrency semaphore before each attempt
    /// to prevent overwhelming the API with unlimited parallel requests.
    async fn send_with_retry(
        &self,
        build_request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        const MAX_RETRIES: u32 = 3;
        let mut delay = std::time::Duration::from_millis(500);

        for attempt in 0..=MAX_RETRIES {
            let _permit = self
                .request_semaphore
                .acquire()
                .await
                .map_err(|_| anyhow!("request semaphore closed"))?;
            let request = self.authenticate(build_request());
            let response = request.send().await;

            match response {
                Ok(resp)
                    if resp.status() == 429 || resp.status() == 502 || resp.status() == 503 =>
                {
                    if attempt == MAX_RETRIES {
                        return Ok(resp);
                    }
                    // Use Retry-After header if present, otherwise exponential backoff.
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(std::time::Duration::from_secs)
                        .unwrap_or(delay);
                    tracing::warn!(
                        status = resp.status().as_u16(),
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        "retrying after transient error"
                    );
                    tokio::time::sleep(wait).await;
                    delay *= 2;
                }
                Ok(resp) => return Ok(resp),
                Err(e)
                    if attempt < MAX_RETRIES
                        && (e.is_timeout() || e.is_connect() || e.is_request()) =>
                {
                    tracing::warn!(
                        error = %e,
                        attempt,
                        wait_ms = delay.as_millis() as u64,
                        "retrying after transient network error"
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e).context("HTTP request failed"),
            }
        }
        unreachable!()
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
                    current = current.get(segment).ok_or_else(|| {
                        anyhow!("list_root segment '{}' not found in response", segment)
                    })?;
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
            .map(json_value_to_string)
            .unwrap_or_default();

        let slug = get_nested(item, slug_field)
            .map(|v| sanitize_slug(&json_value_to_string(v)))
            .unwrap_or_else(|| sanitize_slug(&id));

        let title = title_field.and_then(|f| get_nested(item, f).map(json_value_to_string));

        let updated_at = item
            .get("updated_at")
            .or_else(|| item.get("updatedAt"))
            .or_else(|| item.get("SystemModstamp"))
            .or_else(|| item.get("LastModifiedDate"))
            .map(json_value_to_string);

        ResourceMeta {
            id,
            slug,
            title,
            updated_at,
            content_type: Some("application/json".to_string()),
        }
    }

    /// Render a JSON resource as Markdown, using RenderSpec if available.
    fn render_markdown(
        meta: &ResourceMeta,
        json: &Value,
        render: Option<&crate::connector::spec::RenderSpec>,
    ) -> Vec<u8> {
        match render {
            Some(spec) => Self::render_with_spec(meta, json, spec),
            None => Self::render_default(meta, json),
        }
    }

    /// Spec-driven rendering: frontmatter from selected fields, body from
    /// a named field, sections from nested data.
    fn render_with_spec(
        meta: &ResourceMeta,
        json: &Value,
        spec: &crate::connector::spec::RenderSpec,
    ) -> Vec<u8> {
        let mut out = String::new();

        // --- Frontmatter ---
        out.push_str("---\n");
        if let Some(ref fields) = spec.frontmatter {
            for field_expr in fields {
                let (path, alias) = parse_field_alias(field_expr);
                if let Some(val) = extract_dotpath(json, path) {
                    let display = format_frontmatter_value(val);
                    out.push_str(&format!("{}: {}\n", alias, display));
                }
            }
        } else {
            // Fallback: id + title
            out.push_str(&format!("id: \"{}\"\n", meta.id));
            if let Some(title) = &meta.title {
                out.push_str(&format!("title: \"{}\"\n", title));
            }
        }
        out.push_str("---\n\n");

        // --- Title heading ---
        if let Some(title) = &meta.title {
            out.push_str(&format!("# {}\n\n", title));
        }

        // --- Body ---
        if let Some(ref body_field) = spec.body {
            if let Some(val) = extract_dotpath(json, body_field) {
                let text = json_value_to_string(val);
                if !text.is_empty() {
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
            }
        }

        // --- Sections ---
        if let Some(ref sections) = spec.sections {
            for section in sections {
                if let Some(val) = extract_dotpath(json, &section.field) {
                    out.push_str(&format!("## {}\n\n", section.name));
                    let fmt = section.format.as_deref().unwrap_or("text");
                    match (fmt, val) {
                        ("list", Value::Array(items)) => {
                            if items.is_empty() {
                                out.push_str("None.\n\n");
                            } else {
                                for item in items {
                                    let text = match &section.item_template {
                                        Some(tpl) => expand_template(tpl, item),
                                        None => json_value_to_string(item),
                                    };
                                    out.push_str(&format!("- {}\n", text));
                                }
                                out.push('\n');
                            }
                        }
                        ("table", Value::Array(items)) => {
                            if let Some(first) = items.first().and_then(|v| v.as_object()) {
                                let keys: Vec<&String> = first.keys().take(5).collect();
                                out.push_str("| ");
                                out.push_str(
                                    &keys
                                        .iter()
                                        .map(|k| k.as_str())
                                        .collect::<Vec<_>>()
                                        .join(" | "),
                                );
                                out.push_str(" |\n|");
                                for _ in &keys {
                                    out.push_str("---|");
                                }
                                out.push('\n');
                                for item in items {
                                    out.push_str("| ");
                                    let vals: Vec<String> = keys
                                        .iter()
                                        .map(|k| {
                                            item.get(*k)
                                                .map(json_value_to_string)
                                                .unwrap_or_default()
                                        })
                                        .collect();
                                    out.push_str(&vals.join(" | "));
                                    out.push_str(" |\n");
                                }
                                out.push('\n');
                            }
                        }
                        _ => {
                            out.push_str(&json_value_to_string(val));
                            out.push_str("\n\n");
                        }
                    }
                }
            }
        }

        out.into_bytes()
    }

    /// Default rendering (no RenderSpec): table + JSON code blocks.
    fn render_default(meta: &ResourceMeta, json: &Value) -> Vec<u8> {
        let mut out = String::new();

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

        match json {
            Value::Object(map) => {
                let (simple, complex): (Vec<_>, Vec<_>) = map.iter().partition(|(_, v)| {
                    matches!(
                        v,
                        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                    )
                });

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

/// Parse "user.login as author" into ("user.login", "author").
/// Plain "title" returns ("title", "title").
fn parse_field_alias(expr: &str) -> (&str, &str) {
    if let Some(pos) = expr.find(" as ") {
        (&expr[..pos], &expr[pos + 4..])
    } else {
        (expr, expr.rsplit('.').next().unwrap_or(expr))
    }
}

/// Extract a value from JSON using a dot-path like "user.login" or
/// "labels[].name" (array map).
fn extract_dotpath<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    // Handle array mapping: "labels[].name" → collect names from array
    // This returns a reference, so we can't construct new values here.
    // For array mapping, the caller should use expand_template instead.
    let mut current = json;
    for segment in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Format a JSON value for YAML frontmatter output.
fn format_frontmatter_value(val: &Value) -> String {
    match val {
        Value::String(s) => format!("\"{}\"", s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(arr) => {
            let items: Vec<String> = arr
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Object(map) => {
                        // For objects in arrays, try "name" or "login" as display
                        map.get("name")
                            .or_else(|| map.get("login"))
                            .or_else(|| map.get("label"))
                            .map(json_value_to_string)
                            .unwrap_or_else(|| json_value_to_string(&Value::Object(map.clone())))
                    }
                    other => json_value_to_string(other),
                })
                .collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(_) => json_value_to_string(val),
    }
}

/// Expand a template string like "{user.login} ({created_at})" against a JSON object.
fn expand_template(template: &str, json: &Value) -> String {
    let mut result = template.to_string();
    // Find all {field.path} placeholders and replace them.
    while let Some(start) = result.find('{') {
        if let Some(end) = result[start..].find('}') {
            let path = &result[start + 1..start + end];
            let replacement = extract_dotpath(json, path)
                .map(json_value_to_string)
                .unwrap_or_default();
            result = format!(
                "{}{}{}",
                &result[..start],
                replacement,
                &result[start + end + 1..]
            );
        } else {
            break;
        }
    }
    result
}

/// Sanitize a string for use as a filesystem slug.
fn sanitize_slug(s: &str) -> String {
    s.replace(['/', '\\'], "-")
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

        let response = self.send_with_retry(|| self.client.get(&url)).await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_resources failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
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
        let resolved_id = self
            .slug_to_id
            .get(&format!("{}/{}", collection, id))
            .map(|v| v.clone())
            .unwrap_or_else(|| id.to_string());
        let path = Self::substitute_id(&coll.get_endpoint, &resolved_id);
        let url = self.url(&path);

        let response = self.send_with_retry(|| self.client.get(&url)).await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "read_resource failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
            ));
        }

        let json: Value = response
            .json()
            .await
            .context("failed to parse resource response as JSON")?;

        let meta = Self::extract_meta(&json, coll);
        let mut content = Self::render_markdown(&meta, &json, coll.render.as_ref());

        // Compose: fetch sub-resources and append as sections.
        if let Some(ref compose_specs) = coll.compose {
            for comp in compose_specs {
                let comp_path = Self::substitute_id(&comp.endpoint, &resolved_id);
                let comp_url = self.url(&comp_path);
                if let Ok(resp) = self.send_with_retry(|| self.client.get(&comp_url)).await {
                    if resp.status().is_success() {
                        if let Ok(comp_json) = resp.json::<Value>().await {
                            let items = match &comp.list_root {
                                Some(root) => comp_json
                                    .get(root)
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                                None => comp_json.as_array().cloned().unwrap_or_default(),
                            };
                            let mut section = format!("\n## {}\n\n", comp.name);
                            if items.is_empty() {
                                section.push_str("None yet.\n");
                            } else {
                                for item in &items {
                                    let text = match &comp.item_template {
                                        Some(tpl) => expand_template(tpl, item),
                                        None => json_value_to_string(item),
                                    };
                                    section.push_str(&text);
                                    section.push('\n');
                                }
                            }
                            content.extend_from_slice(section.as_bytes());
                        }
                    }
                }
            }
        }

        Ok(Resource {
            meta,
            content,
            raw_json: Some(json),
        })
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let coll = self.find_collection(collection)?;
        let resolved_id = self
            .slug_to_id
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

        let response = self
            .send_with_retry(|| self.client.patch(&url).json(&body_json))
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "write_resource failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
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

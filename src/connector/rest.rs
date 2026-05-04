use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::sync::RwLock;
use std::time::Instant;

use crate::connector::spec::{CollectionSpec, ConnectorSpec};
use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};

const MAX_ERROR_BODY_LEN: usize = 512;

/// Convert user-written markdown into the JSON object expected by the API.
///
/// Uses the collection spec to know which field is the title and which is the
/// body, so field names are never hardcoded (GitHub uses "body", others use
/// "description", "content", etc.).
///
/// Supported input formats:
///   1. YAML frontmatter (`---` … `---`) — each key/value becomes a JSON field;
///      text after the closing `---` goes into `render.body` field.
///   2. Plain markdown — a leading `# Heading` line becomes `title_field`;
///      everything else becomes `render.body` field.
///   3. Raw JSON — returned as-is so callers that already produce JSON keep working.
fn markdown_to_json(content: &[u8], coll: &CollectionSpec) -> Result<Value> {
    let body_field = coll
        .render
        .as_ref()
        .and_then(|r| r.body.as_deref())
        .unwrap_or("body");
    let title_field = coll.title_field.as_deref().unwrap_or("title");

    let text = std::str::from_utf8(content).context("content is not valid UTF-8")?;

    // Raw JSON — pass through.
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("failed to parse content as JSON");
    }

    let mut map = serde_json::Map::new();

    if let Some(after_open) = text.strip_prefix("---") {
        // YAML frontmatter
        let (fm_text, body_text) = if let Some(pos) = after_open.find("\n---") {
            (
                &after_open[..pos],
                after_open[pos + 4..].trim_start_matches('\n'),
            )
        } else {
            (after_open, "")
        };
        let fm: serde_json::Map<String, Value> =
            serde_yaml::from_str(fm_text).context("failed to parse YAML frontmatter")?;
        map.extend(fm);
        // Strip tapfs-managed fields (_draft, _id, _version) — never send to API
        map.retain(|k, _| !k.starts_with('_'));
        if !body_text.is_empty() {
            map.insert(body_field.to_string(), Value::String(body_text.to_string()));
        }
    } else {
        // Plain markdown — first `# ` line is title, rest is body.
        let mut lines = text.splitn(2, '\n');
        let first = lines.next().unwrap_or("").trim();
        let rest = lines.next().unwrap_or("").trim_start_matches('\n');
        let title = first.trim_start_matches('#').trim();
        if !title.is_empty() {
            map.insert(title_field.to_string(), Value::String(title.to_string()));
        }
        if !rest.is_empty() {
            map.insert(body_field.to_string(), Value::String(rest.to_string()));
        }
        // Strip tapfs-managed fields (_draft, _id, _version) — never send to API
        map.retain(|k, _| !k.starts_with('_'));
    }

    Ok(Value::Object(map))
}

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
    token: RwLock<Option<String>>,
    oauth2_config: Option<OAuth2Config>,
    /// Maps "collection/slug" → API resource ID for endpoint substitution.
    slug_to_id: dashmap::DashMap<String, String>,
    /// Limits concurrent HTTP requests to prevent overwhelming APIs.
    request_semaphore: tokio::sync::Semaphore,
}

/// OAuth2 configuration for automatic token refresh.
pub struct OAuth2Config {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub expiry: RwLock<Option<Instant>>,
}

impl RestConnector {
    /// Create a new RestConnector from a spec.
    ///
    /// Token resolution order:
    /// 1. Explicit token passed via `override_token` (from credentials file)
    /// 2. Environment variable specified in `auth.token_env`
    pub fn new(spec: ConnectorSpec, client: Client) -> Self {
        Self::new_with_token(spec, client, None)
    }

    /// Create a RestConnector with an explicit token override.
    pub fn new_with_token(
        spec: ConnectorSpec,
        client: Client,
        override_token: Option<String>,
    ) -> Self {
        let token = override_token.or_else(|| {
            spec.auth
                .as_ref()
                .and_then(|auth| auth.token_env.as_ref())
                .and_then(|env_var| std::env::var(env_var).ok())
        });

        Self {
            spec,
            client,
            token: RwLock::new(token),
            oauth2_config: None,
            slug_to_id: dashmap::DashMap::new(),
            request_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }
    }

    /// Create a RestConnector with OAuth2 token refresh support.
    pub fn new_with_oauth2(
        spec: ConnectorSpec,
        client: Client,
        access_token: Option<String>,
        oauth2_config: OAuth2Config,
    ) -> Self {
        Self {
            spec,
            client,
            token: RwLock::new(access_token),
            oauth2_config: Some(oauth2_config),
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

    /// Substitute `{id}` in a JSON request body template, escaping the value
    /// so the result is still valid JSON when the placeholder appears inside
    /// a string literal (`"{id}"` → `"<escaped-id>"`).
    ///
    /// Non-string positions (`{"count": {id}}`) get the raw escaped scalar,
    /// which is valid only if the id parses as a JSON value (e.g. a number).
    /// That edge case is the user's responsibility — `validate_delete_body`
    /// in the spec layer catches structural problems at load time.
    fn substitute_id_json(template: &str, id: &str) -> String {
        // serde_json renders the id as a quoted JSON string ("abc-\"foo\"").
        // We strip the surrounding quotes so the substitution still fits
        // inside a `"…"` template literal.
        let quoted = serde_json::to_string(id).expect("string serialization is infallible");
        let escaped = &quoted[1..quoted.len() - 1];
        template.replace("{id}", escaped)
    }

    /// Find the CollectionSpec by name, handling path-encoded nested collection paths.
    ///
    /// For flat collections like `"issues"`, does a direct match.
    /// For path-encoded names like `"repos/tap/issues"`, walks the spec
    /// subcollection tree and substitutes parent resource IDs into endpoints.
    fn find_collection(&self, name: &str) -> Result<CollectionSpec> {
        if let Some(c) = self.spec.collections.iter().find(|c| c.name == name) {
            return Ok(c.clone());
        }
        self.resolve_nested_collection(name)
    }

    /// Walk a path-encoded collection name like `"repos/tap/issues"` through the
    /// spec's subcollection tree, resolving slugs to API IDs and substituting
    /// them into endpoint placeholders.
    fn resolve_nested_collection(&self, path: &str) -> Result<CollectionSpec> {
        let segments: Vec<&str> = path.split('/').collect();
        if segments.is_empty() {
            return Err(anyhow!(
                "empty collection path in spec '{}'",
                self.spec.name
            ));
        }

        let mut current = self
            .spec
            .collections
            .iter()
            .find(|c| c.name == segments[0])
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "collection '{}' not found in spec '{}'",
                    segments[0],
                    self.spec.name
                )
            })?;

        let mut params: Vec<(String, String)> = Vec::new();
        let mut current_collection_path = segments[0].to_string();
        let mut i = 1;

        while i < segments.len() {
            let resource_slug = segments[i];
            i += 1;

            // Resolve user-visible slug to API id (populated by list_resources).
            let key = format!("{}/{}", current_collection_path, resource_slug);
            let api_id = self
                .slug_to_id
                .get(&key)
                .map(|v| v.clone())
                .unwrap_or_else(|| resource_slug.to_string());

            if i >= segments.len() {
                break;
            }

            let sub_name = segments[i];
            i += 1;

            let subs = current.subcollections.as_deref().unwrap_or(&[]);
            let sub = subs
                .iter()
                .find(|c| c.name == sub_name)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "subcollection '{}' not found under '{}'",
                        sub_name,
                        current.name
                    )
                })?;

            if let Some(ref param) = sub.parent_param {
                params.push((param.clone(), api_id));
            }

            current_collection_path =
                format!("{}/{}/{}", current_collection_path, resource_slug, sub_name);
            current = sub;
        }

        Ok(substitute_params(current, &params))
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
        self.ensure_token().await;
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

    /// Ensure the OAuth2 access token is fresh, refreshing if expired or missing.
    async fn ensure_token(&self) {
        let config = match &self.oauth2_config {
            Some(c) => c,
            None => return, // No OAuth2, static token
        };

        // Check if refresh is needed
        let needs_refresh = {
            let expiry = config.expiry.read().unwrap();
            match expiry.as_ref() {
                Some(exp) => Instant::now() >= *exp,
                None => self.token.read().unwrap().is_none(),
            }
        };

        if !needs_refresh {
            return;
        }

        // Refresh the token
        match self
            .client
            .post(&config.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", config.refresh_token.as_str()),
                ("client_id", config.client_id.as_str()),
                ("client_secret", config.client_secret.as_str()),
            ])
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(new_token) = json["access_token"].as_str() {
                        *self.token.write().unwrap() = Some(new_token.to_string());
                        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);
                        // Refresh at 80% of TTL to avoid edge-case expiry
                        *config.expiry.write().unwrap() = Some(
                            Instant::now() + std::time::Duration::from_secs(expires_in * 4 / 5),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!("OAuth2 token refresh failed: {}", e);
            }
        }
    }

    /// Add authentication and standard headers to a request builder.
    fn authenticate(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder
            .header("User-Agent", "tapfs/0.1")
            .header("Accept", "application/json");
        let token = self.token.read().unwrap().clone();
        match (&self.spec.auth, &token) {
            (Some(auth), Some(token)) => match auth.auth_type.as_str() {
                "bearer" | "oauth2" => builder.bearer_auth(token),
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

        let group = coll
            .group_by
            .as_deref()
            .and_then(|field| get_nested(item, field))
            .map(json_value_to_string);

        ResourceMeta {
            id,
            slug,
            title,
            updated_at,
            content_type: Some("application/json".to_string()),
            group,
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

/// Replace `{key}` placeholders in all endpoint fields of a CollectionSpec.
fn substitute_params(mut coll: CollectionSpec, params: &[(String, String)]) -> CollectionSpec {
    for (key, val) in params {
        let placeholder = format!("{{{}}}", key);
        coll.list_endpoint = coll.list_endpoint.replace(&placeholder, val);
        coll.get_endpoint = coll.get_endpoint.replace(&placeholder, val);
        if let Some(ref mut ep) = coll.update_endpoint {
            *ep = ep.replace(&placeholder, val);
        }
        if let Some(ref mut ep) = coll.create_endpoint {
            *ep = ep.replace(&placeholder, val);
        }
        if let Some(ref mut ep) = coll.delete_endpoint {
            *ep = ep.replace(&placeholder, val);
        }
    }
    coll
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
            .map(|item| Self::extract_meta(item, &coll))
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

    async fn list_resources_with_content(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Vec<u8>)>> {
        let coll = self.find_collection(collection)?;
        let url = self.url(&coll.list_endpoint);

        let response = self.send_with_retry(|| self.client.get(&url)).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_resources_with_content failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
            ));
        }

        let json: Value = response
            .json()
            .await
            .context("failed to parse list response as JSON")?;

        let items = Self::extract_list(&json, coll.list_root.as_deref())?;
        let mut out = Vec::with_capacity(items.len());

        for item in items {
            let meta = Self::extract_meta(item, &coll);
            if meta.slug != meta.id {
                let key = format!("{}/{}", collection, meta.slug);
                self.slug_to_id.insert(key, meta.id.clone());
            }
            let content = Self::render_markdown(&meta, item, coll.render.as_ref());
            out.push((meta, content));
        }

        Ok(out)
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

        let meta = Self::extract_meta(&json, &coll);
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

        let body_json = markdown_to_json(content, &coll)?;

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

    async fn create_resource(&self, collection: &str, content: &[u8]) -> Result<ResourceMeta> {
        let coll = self.find_collection(collection)?;
        let endpoint = coll
            .create_endpoint
            .as_deref()
            .unwrap_or(&coll.list_endpoint);
        // Strip query parameters from the endpoint for POST requests.
        let clean_endpoint = endpoint.split('?').next().unwrap_or(endpoint);
        let url = self.url(clean_endpoint);

        let body_json = markdown_to_json(content, &coll)?;

        let response = self
            .send_with_retry(|| self.client.post(&url).json(&body_json))
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "create_resource failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
            ));
        }

        let json: Value = response
            .json()
            .await
            .context("failed to parse create response as JSON")?;

        let meta = Self::extract_meta(&json, &coll);

        // Cache the new slug → ID mapping.
        if meta.slug != meta.id {
            let key = format!("{}/{}", collection, meta.slug);
            self.slug_to_id.insert(key, meta.id.clone());
        }

        Ok(meta)
    }

    async fn delete_resource(&self, collection: &str, id: &str) -> Result<()> {
        let coll = self.find_collection(collection)?;
        let endpoint = coll.delete_endpoint.as_deref().ok_or_else(|| {
            anyhow!(
                "collection '{}' does not declare a delete_endpoint",
                collection
            )
        })?;
        let resolved_id = self
            .slug_to_id
            .get(&format!("{}/{}", collection, id))
            .map(|v| v.clone())
            .unwrap_or_else(|| id.to_string());
        let path = Self::substitute_id(endpoint, &resolved_id);
        let url = self.url(&path);

        // If delete_body is set, the API soft-deletes via PATCH (e.g. Notion's
        // archive flag). Otherwise issue a standard HTTP DELETE.
        let response = if let Some(body_template) = coll.delete_body.as_deref() {
            let body = Self::substitute_id_json(body_template, &resolved_id);
            self.send_with_retry(|| {
                self.client
                    .patch(&url)
                    .header("Content-Type", "application/json")
                    .body(body.clone())
            })
            .await?
        } else {
            self.send_with_retry(|| self.client.delete(&url)).await?
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "delete_resource failed: HTTP {} — {}",
                status,
                truncate_error_body(&body)
            ));
        }

        // Remove the slug → ID mapping.
        self.slug_to_id.remove(&format!("{}/{}", collection, id));

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::connector::spec::{CollectionSpec, RenderSpec, SectionSpec};
    use crate::connector::traits::ResourceMeta;

    // ---------------------------------------------------------------
    // Helper: build a minimal CollectionSpec
    // ---------------------------------------------------------------
    fn minimal_collection(name: &str) -> CollectionSpec {
        CollectionSpec {
            name: name.to_string(),
            description: None,
            slug_hint: None,
            operations: None,
            list_endpoint: "/items".to_string(),
            get_endpoint: "/items/{id}".to_string(),
            update_endpoint: None,
            create_endpoint: None,
            delete_endpoint: None,
            delete_body: None,
            id_field: None,
            slug_field: None,
            title_field: None,
            list_root: None,
            render: None,
            compose: None,
            operations_spec: None,
            relationships: None,
            parent_param: None,
            subcollections: None,
            group_by: None,
            aggregate: None,
        }
    }

    fn minimal_meta() -> ResourceMeta {
        ResourceMeta {
            id: "42".to_string(),
            slug: "42".to_string(),
            title: Some("Test Title".to_string()),
            updated_at: None,
            content_type: Some("application/json".to_string()),
            group: None,
        }
    }

    // ---------------------------------------------------------------
    // parse_field_alias
    // ---------------------------------------------------------------
    #[test]
    fn parse_field_alias_with_alias() {
        let (path, alias) = parse_field_alias("user.login as author");
        assert_eq!(path, "user.login");
        assert_eq!(alias, "author");
    }

    #[test]
    fn parse_field_alias_plain() {
        let (path, alias) = parse_field_alias("title");
        assert_eq!(path, "title");
        assert_eq!(alias, "title");
    }

    #[test]
    fn parse_field_alias_dotted_no_alias() {
        let (path, alias) = parse_field_alias("user.login");
        assert_eq!(path, "user.login");
        assert_eq!(alias, "login");
    }

    // ---------------------------------------------------------------
    // extract_dotpath
    // ---------------------------------------------------------------
    #[test]
    fn extract_dotpath_flat() {
        let v = json!({"title": "hello"});
        assert_eq!(extract_dotpath(&v, "title"), Some(&json!("hello")));
    }

    #[test]
    fn extract_dotpath_nested() {
        let v = json!({"user": {"login": "bob"}});
        assert_eq!(extract_dotpath(&v, "user.login"), Some(&json!("bob")));
    }

    #[test]
    fn extract_dotpath_missing() {
        let v = json!({"title": "hello"});
        assert_eq!(extract_dotpath(&v, "missing"), None);
    }

    #[test]
    fn extract_dotpath_non_object_intermediate() {
        let v = json!({"user": "string"});
        assert_eq!(extract_dotpath(&v, "user.login"), None);
    }

    // ---------------------------------------------------------------
    // format_frontmatter_value
    // ---------------------------------------------------------------
    #[test]
    fn format_frontmatter_string() {
        assert_eq!(format_frontmatter_value(&json!("hello")), "\"hello\"");
    }

    #[test]
    fn format_frontmatter_number() {
        assert_eq!(format_frontmatter_value(&json!(42)), "42");
    }

    #[test]
    fn format_frontmatter_bool() {
        assert_eq!(format_frontmatter_value(&json!(true)), "true");
    }

    #[test]
    fn format_frontmatter_null() {
        assert_eq!(format_frontmatter_value(&json!(null)), "null");
    }

    #[test]
    fn format_frontmatter_array_objects() {
        let v = json!([{"name": "bug"}, {"name": "fix"}]);
        assert_eq!(format_frontmatter_value(&v), "[bug, fix]");
    }

    // ---------------------------------------------------------------
    // expand_template
    // ---------------------------------------------------------------
    #[test]
    fn expand_template_basic() {
        let v = json!({"name": "hello"});
        assert_eq!(expand_template("{name}", &v), "hello");
    }

    #[test]
    fn expand_template_nested() {
        let v = json!({"user": {"login": "alice"}});
        assert_eq!(expand_template("{user.login}", &v), "alice");
    }

    #[test]
    fn expand_template_missing() {
        let v = json!({"name": "hello"});
        assert_eq!(expand_template("{missing}", &v), "");
    }

    #[test]
    fn expand_template_no_placeholders() {
        let v = json!({"name": "hello"});
        assert_eq!(expand_template("plain text", &v), "plain text");
    }

    // ---------------------------------------------------------------
    // sanitize_slug
    // ---------------------------------------------------------------
    #[test]
    fn sanitize_slug_slashes() {
        assert_eq!(sanitize_slug("a/b\\c"), "a-b-c");
    }

    #[test]
    fn sanitize_slug_null_bytes() {
        assert_eq!(sanitize_slug("a\0b"), "ab");
    }

    #[test]
    fn sanitize_slug_whitespace() {
        assert_eq!(sanitize_slug("  hello  "), "hello");
    }

    #[test]
    fn sanitize_slug_clean() {
        assert_eq!(sanitize_slug("hello-world"), "hello-world");
    }

    // ---------------------------------------------------------------
    // json_value_to_string
    // ---------------------------------------------------------------
    #[test]
    fn json_value_to_string_str() {
        assert_eq!(json_value_to_string(&json!("hello")), "hello");
    }

    #[test]
    fn json_value_to_string_number() {
        assert_eq!(json_value_to_string(&json!(42)), "42");
    }

    #[test]
    fn json_value_to_string_null() {
        assert_eq!(json_value_to_string(&json!(null)), "");
    }

    // ---------------------------------------------------------------
    // truncate_error_body
    // ---------------------------------------------------------------
    #[test]
    fn truncate_body_short() {
        let body = "a".repeat(100);
        assert_eq!(truncate_error_body(&body), body.as_str());
    }

    #[test]
    fn truncate_body_long() {
        let body = "a".repeat(1000);
        let result = truncate_error_body(&body);
        assert_eq!(result.len(), 512);
    }

    // ---------------------------------------------------------------
    // RestConnector::extract_list
    // ---------------------------------------------------------------
    #[test]
    fn extract_list_with_root() {
        let v = json!({"data": [{"id": 1}]});
        let list = RestConnector::extract_list(&v, Some("data")).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], json!({"id": 1}));
    }

    #[test]
    fn extract_list_without_root() {
        let v = json!([{"id": 1}]);
        let list = RestConnector::extract_list(&v, None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], json!({"id": 1}));
    }

    #[test]
    fn extract_list_missing_root() {
        let v = json!({"other": []});
        let result = RestConnector::extract_list(&v, Some("data"));
        assert!(result.is_err());
    }

    #[test]
    fn extract_list_dotted_root() {
        let v = json!({"data": {"items": [{"id": 1}]}});
        let list = RestConnector::extract_list(&v, Some("data.items")).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], json!({"id": 1}));
    }

    // ---------------------------------------------------------------
    // RestConnector::extract_meta
    // ---------------------------------------------------------------
    #[test]
    fn extract_meta_defaults() {
        let item = json!({"id": 42, "title": "Test"});
        let coll = minimal_collection("things");
        let meta = RestConnector::extract_meta(&item, &coll);
        assert_eq!(meta.id, "42");
        assert_eq!(meta.slug, "42");
        // No title_field configured, so title is None
        assert!(meta.title.is_none());
    }

    #[test]
    fn extract_meta_custom_fields() {
        let item = json!({"number": 7, "name": "widget", "display": "My Widget"});
        let mut coll = minimal_collection("things");
        coll.id_field = Some("number".to_string());
        coll.slug_field = Some("name".to_string());
        coll.title_field = Some("display".to_string());
        let meta = RestConnector::extract_meta(&item, &coll);
        assert_eq!(meta.id, "7");
        assert_eq!(meta.slug, "widget");
        assert_eq!(meta.title, Some("My Widget".to_string()));
    }

    // ---------------------------------------------------------------
    // RestConnector::render_default
    // ---------------------------------------------------------------
    #[test]
    fn render_default_object() {
        let meta = minimal_meta();
        let v = json!({"name": "Alice", "age": 30});
        let bytes = RestConnector::render_default(&meta, &v);
        let output = String::from_utf8(bytes).unwrap();
        // Should contain frontmatter
        assert!(output.starts_with("---\n"));
        assert!(output.contains("id: \"42\""));
        // Should contain a Field|Value table
        assert!(output.contains("| Field | Value |"));
        assert!(output.contains("| name | Alice |"));
        assert!(output.contains("| age | 30 |"));
    }

    // ---------------------------------------------------------------
    // RestConnector::render_with_spec
    // ---------------------------------------------------------------
    #[test]
    fn render_with_spec_frontmatter() {
        let meta = minimal_meta();
        let v = json!({"state": "open", "user": {"login": "bob"}});
        let spec = RenderSpec {
            frontmatter: Some(vec![
                "state".to_string(),
                "user.login as author".to_string(),
            ]),
            body: None,
            sections: None,
            exclude: None,
        };
        let bytes = RestConnector::render_with_spec(&meta, &v, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("state: \"open\""));
        assert!(output.contains("author: \"bob\""));
    }

    #[test]
    fn render_with_spec_body() {
        let meta = minimal_meta();
        let v = json!({"body": "Hello world", "state": "open"});
        let spec = RenderSpec {
            frontmatter: None,
            body: Some("body".to_string()),
            sections: None,
            exclude: None,
        };
        let bytes = RestConnector::render_with_spec(&meta, &v, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("Hello world"));
    }

    #[test]
    fn render_with_spec_sections_list() {
        let meta = minimal_meta();
        let v = json!({"labels": [{"name": "bug"}, {"name": "enhancement"}]});
        let spec = RenderSpec {
            frontmatter: None,
            body: None,
            sections: Some(vec![SectionSpec {
                name: "Labels".to_string(),
                field: "labels".to_string(),
                format: Some("list".to_string()),
                item_template: Some("{name}".to_string()),
            }]),
            exclude: None,
        };
        let bytes = RestConnector::render_with_spec(&meta, &v, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("## Labels"));
        assert!(output.contains("- bug"));
        assert!(output.contains("- enhancement"));
    }

    // ---------------------------------------------------------------
    // delete_resource gating
    // ---------------------------------------------------------------

    fn build_connector(server_url: &str, collections: Vec<CollectionSpec>) -> RestConnector {
        let spec = crate::connector::spec::ConnectorSpec {
            spec_version: None,
            version: None,
            description: None,
            name: "test".to_string(),
            base_url: server_url.to_string(),
            auth: None,
            transport: None,
            capabilities: None,
            agent: None,
            collections,
        };
        RestConnector::new_with_token(spec, reqwest::Client::new(), None)
    }

    #[tokio::test]
    async fn delete_resource_errors_without_delete_endpoint() {
        let mut coll = minimal_collection("widgets");
        coll.delete_endpoint = None;
        let conn = build_connector("http://localhost:1", vec![coll]);

        let err = conn
            .delete_resource("widgets", "42")
            .await
            .expect_err("expected error when delete_endpoint is missing");
        let msg = err.to_string();
        assert!(
            msg.contains("does not declare a delete_endpoint"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn delete_resource_calls_endpoint_when_configured() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/widgets/42"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("widgets");
        coll.delete_endpoint = Some("/widgets/{id}".to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        conn.delete_resource("widgets", "42")
            .await
            .expect("delete should succeed");

        // MockServer's `.expect(1)` is verified on drop.
        drop(server);
    }

    #[tokio::test]
    async fn delete_resource_substitutes_id_into_delete_body() {
        use wiremock::matchers::{body_json_string, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/items/abc-123"))
            .and(body_json_string(
                r#"{"archived":true,"deleted_id":"abc-123"}"#,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("items");
        coll.delete_endpoint = Some("/items/{id}".to_string());
        coll.delete_body =
            Some(r#"{"archived":true,"deleted_id":"{id}"}"#.to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        conn.delete_resource("items", "abc-123")
            .await
            .expect("delete with substituted id should succeed");

        drop(server);
    }

    #[tokio::test]
    async fn delete_body_id_substitution_escapes_json_specials() {
        use wiremock::matchers::{body_json_string, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // An id containing a quote must be JSON-escaped in the body so the
        // request remains valid JSON.
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/items/quote%22danger"))
            .and(body_json_string(r#"{"deleted_id":"quote\"danger"}"#))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("items");
        coll.delete_endpoint = Some("/items/quote%22danger".to_string()); // pre-encoded
        coll.delete_body = Some(r#"{"deleted_id":"{id}"}"#.to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        // The id field doubles as the URL — but for this test we only care
        // about the body's JSON escaping, so we hardcode the path above.
        conn.delete_resource("items", "quote\"danger")
            .await
            .expect("delete with quote-containing id should succeed");

        drop(server);
    }

    #[tokio::test]
    async fn delete_resource_uses_patch_when_delete_body_set() {
        use wiremock::matchers::{body_json_string, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/pages/abc-123"))
            .and(body_json_string(r#"{"archived": true}"#))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("pages");
        coll.delete_endpoint = Some("/pages/{id}".to_string());
        coll.delete_body = Some(r#"{"archived": true}"#.to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        conn.delete_resource("pages", "abc-123")
            .await
            .expect("archive-style delete should succeed");

        drop(server);
    }

    #[test]
    fn render_with_spec_empty_section() {
        let meta = minimal_meta();
        let v = json!({"labels": []});
        let spec = RenderSpec {
            frontmatter: None,
            body: None,
            sections: Some(vec![SectionSpec {
                name: "Labels".to_string(),
                field: "labels".to_string(),
                format: Some("list".to_string()),
                item_template: None,
            }]),
            exclude: None,
        };
        let bytes = RestConnector::render_with_spec(&meta, &v, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("## Labels"));
        assert!(output.contains("None."));
    }
}

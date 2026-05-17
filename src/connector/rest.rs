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

/// Pull the `_idempotency_key: <value>` line out of the YAML frontmatter, if
/// present. Used by `create_resource` to send a per-resource idempotency key
/// as an HTTP header — so a retried POST after a lost response doesn't create
/// a duplicate (e.g. duplicate Jira comment, duplicate GitHub issue).
///
/// The key is deliberately stable across retries because it's read from the
/// content (which the VFS persisted to a draft file) rather than generated
/// per attempt. It even survives daemon restarts: the draft file on disk
/// keeps the same key.
fn extract_idempotency_key(content: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(content).ok()?;
    let after_open = text.strip_prefix("---")?;
    let fm_text = match after_open.split_once("\n---") {
        Some((fm, _)) => fm,
        None => after_open,
    };
    for line in fm_text.lines() {
        if let Some(val) = line.strip_prefix("_idempotency_key:") {
            let v = val.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
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
///
/// `client_secret` is optional: confidential clients (web apps, server-to-server
/// OAuth like Google) include it in refresh requests; **public clients using
/// PKCE (e.g. X v2 user-context) do not** — the verifier was the
/// proof-of-possession at exchange time, and the refresh request authenticates
/// via the refresh_token + client_id alone. Sending an empty client_secret
/// would silently break PKCE refreshes on providers that bind tokens to the
/// client's PKCE registration.
pub struct OAuth2Config {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub refresh_token: String,
    pub expiry: RwLock<Option<Instant>>,
}

/// Merge a collection's `default_query` into a request path, preserving any
/// query string already present on the path. Returns the path unchanged when
/// the map is `None` or empty.
///
/// The path may or may not already contain `?…` — the merge picks `?` vs `&`
/// accordingly. Iteration is over BTreeMap (sorted key order) so the produced
/// URL is deterministic; tests rely on that.
///
/// Encoding rule for *values*: anything outside `[A-Za-z0-9-._~,]` is
/// percent-encoded. Comma is preserved because X v2 uses unencoded commas as
/// the list separator inside `expansions=` / `tweet.fields=` / etc., and the
/// API will reject `%2C`. Keys are passed through verbatim — `tweet.fields`
/// must not become `tweet%2Efields`.
/// When `RenderSpec::resolve_includes` is enabled, single-resource API
/// responses arrive wrapped as `{"data": {...}, "includes": {...}}`.
/// `extract_meta` needs the unwrapped resource — meta.id / slug / title
/// come from inside `data`, never from the envelope. This helper returns
/// the right root for meta extraction without changing the renderer's
/// access to `includes` (the full envelope still flows to the renderer).
fn data_root_for_meta<'a>(
    json: &'a Value,
    render: Option<&crate::connector::spec::RenderSpec>,
) -> &'a Value {
    let resolve_includes = render.and_then(|r| r.resolve_includes).unwrap_or(false);
    if resolve_includes {
        json.get("data").unwrap_or(json)
    } else {
        json
    }
}

pub(crate) fn apply_default_query(
    path: &str,
    default_query: Option<&std::collections::BTreeMap<String, String>>,
) -> String {
    let Some(map) = default_query else {
        return path.to_string();
    };
    if map.is_empty() {
        return path.to_string();
    }

    let mut out = String::with_capacity(path.len() + map.len() * 32);
    out.push_str(path);
    let mut sep = if path.contains('?') { '&' } else { '?' };
    for (k, v) in map {
        out.push(sep);
        out.push_str(k);
        out.push('=');
        encode_query_value_into(v, &mut out);
        sep = '&';
    }
    out
}

fn encode_query_value_into(input: &str, out: &mut String) {
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                let hi = byte >> 4;
                let lo = byte & 0xF;
                out.push(hex_nibble(hi));
                out.push(hex_nibble(lo));
            }
        }
    }
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _ => unreachable!(),
    }
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

    /// Build a full URL with the collection's `default_query` merged in.
    /// Used by GET paths (list, read) where API-specific opt-in params like
    /// `expansions=` / `tweet.fields=` are mandatory for a useful payload.
    fn url_with_default_query(&self, path: &str, coll: &CollectionSpec) -> String {
        let with_query = apply_default_query(path, coll.default_query.as_ref());
        self.url(&with_query)
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
    /// Delegates to `connector::retry::execute` so the retry/backoff/
    /// Retry-After logic lives in one place. This wrapper just handles
    /// the connector-specific concerns: refreshing the OAuth token,
    /// acquiring the concurrency permit, and stamping auth/headers.
    ///
    /// The semaphore permit is acquired once for the entire retry sequence
    /// (not per-attempt) — semantically a "request" includes its retries.
    /// Acquiring per-attempt would let other callers slip in during sleeps,
    /// defeating the cap that protects upstream APIs.
    async fn send_with_retry(
        &self,
        build_request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        self.ensure_token().await;
        let _permit = self
            .request_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow!("request semaphore closed"))?;
        crate::connector::retry::execute(&crate::connector::retry::RetryPolicy::default(), || {
            self.authenticate(build_request())
        })
        .await
    }

    /// Ensure the OAuth2 access token is fresh, refreshing if expired or missing.
    ///
    /// Hardened in three ways from the previous shape:
    ///
    /// 1. The refresh request goes through `retry::execute` — a single
    ///    transient network blip used to silently fail and force every
    ///    downstream API call to fail with 401. Now we tolerate one bad
    ///    packet on the way to the OAuth provider.
    ///
    /// 2. The response is also parsed for a *new* `refresh_token`. Some
    ///    providers (Google) rotate refresh tokens on every refresh — if we
    ///    keep using the old one, the next refresh will fail with
    ///    invalid_grant. The new token replaces the in-memory copy.
    ///    (Persisting the rotated token back to disk is a follow-up — that
    ///    requires plumbing the `data_dir` through OAuth2Config, which is
    ///    a wider change than belongs in this PR.)
    ///
    /// 3. Failures are still logged (not panicked) so downstream code can
    ///    proceed and surface the inevitable 401 with a useful message.
    async fn ensure_token(&self) {
        let config = match &self.oauth2_config {
            Some(c) => c,
            None => return,
        };

        let needs_refresh = {
            let expiry = config.expiry.read().unwrap();
            let token_missing = self.token.read().unwrap().is_none();
            let past_expiry = match expiry.as_ref() {
                Some(exp) => Instant::now() >= *exp,
                None => false,
            };
            // Refresh if we have no usable access token OR the one we have
            // is past its known expiry. The token-missing branch covers a
            // pathological state where credentials.yaml's `expires_at` is
            // recorded but the keychain blob is unreadable — without this
            // check the request would go unauthenticated until expiry.
            token_missing || past_expiry
        };
        if !needs_refresh {
            return;
        }

        let policy = crate::connector::retry::RetryPolicy::default();
        let refresh_result = crate::connector::retry::execute(&policy, || {
            // PKCE refreshes (no client_secret) and confidential refreshes
            // (with client_secret) share everything except this form param.
            let mut form: Vec<(&str, &str)> = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", config.refresh_token.as_str()),
                ("client_id", config.client_id.as_str()),
            ];
            if let Some(ref secret) = config.client_secret {
                form.push(("client_secret", secret.as_str()));
            }
            self.client.post(&config.token_url).form(&form)
        })
        .await;

        let resp = match refresh_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "OAuth2 token refresh failed (network/transport)");
                return;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = status.as_u16(),
                body = truncate_error_body(&body),
                "OAuth2 token refresh failed (non-2xx)"
            );
            return;
        }

        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "OAuth2 token refresh response was not JSON");
                return;
            }
        };

        if let Some(new_token) = json["access_token"].as_str() {
            *self.token.write().unwrap() = Some(new_token.to_string());
            let expires_in = json["expires_in"].as_u64().unwrap_or(3600);
            *config.expiry.write().unwrap() =
                Some(Instant::now() + std::time::Duration::from_secs(expires_in * 4 / 5));
        }
        // Rotated refresh tokens: providers like Google issue a new
        // refresh_token on each refresh and invalidate the old one. The
        // OAuth2Config holds the refresh_token in a plain field; we can't
        // mutate it without an &mut. Log when a rotated value is returned
        // so we can plumb persistence in a follow-up. (Until then, daemon
        // restarts reuse the original token from credentials.yaml — fine
        // for non-rotating providers, eventually broken for Google.)
        if let Some(new_refresh) = json["refresh_token"].as_str() {
            if new_refresh != config.refresh_token {
                tracing::info!(
                    "OAuth2 provider rotated refresh_token; \
                     persistence not yet plumbed — daemon restart will need re-auth"
                );
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
                "bearer" | "oauth2" | "oauth2_pkce" => builder.bearer_auth(token),
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

        // When resolve_includes is enabled and the response looks like a v2
        // envelope (`{"data": {...}, "includes": {...}}`), use `data` as the
        // resource root for plain field extraction. `includes` lookups still
        // resolve against the full envelope.
        let resolve_includes = spec.resolve_includes.unwrap_or(false);
        let data_root: &Value = if resolve_includes {
            json.get("data").unwrap_or(json)
        } else {
            json
        };

        // --- Frontmatter ---
        out.push_str("---\n");
        if let Some(ref fields) = spec.frontmatter {
            for field_expr in fields {
                // Join syntax (`path via includes.array.output as alias`) is
                // honored only when resolve_includes is on; otherwise it's
                // treated as a plain dotted name (which will miss, as
                // expected — fail loud rather than silently magic-resolve).
                if resolve_includes {
                    if let Some(join) = parse_frontmatter_join(field_expr) {
                        if let Some(val) = resolve_include_join(json, &join, data_root) {
                            let display = format_frontmatter_value(val);
                            out.push_str(&format!("{}: {}\n", join.alias, display));
                        }
                        continue;
                    }
                }
                let (path, alias) = parse_field_alias(field_expr);
                if let Some(val) = extract_dotpath(data_root, path) {
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
            if let Some(val) = extract_dotpath(data_root, body_field) {
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
                if let Some(val) = extract_dotpath(data_root, &section.field) {
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

/// Parsed shape of a frontmatter join entry:
/// `"<id_path> via <includes_path>.<output> as <alias>"` →
/// (id_path, includes_path, output_field, alias).
///
/// Only valid when `resolve_includes: true`. The renderer reads the id at
/// `id_path` (relative to the unwrapped data root), looks up the matching
/// object in `includes_path` (an array), and substitutes its `output` field.
struct FrontmatterJoin<'a> {
    id_path: &'a str,
    includes_path: &'a str,
    output_field: &'a str,
    alias: &'a str,
}

/// Parse `"author_id via includes.users.username as author"`.
/// Returns None when the entry doesn't contain ` via ` (i.e. it's a plain
/// `parse_field_alias` entry).
fn parse_frontmatter_join(expr: &str) -> Option<FrontmatterJoin<'_>> {
    let via_pos = expr.find(" via ")?;
    let id_path = &expr[..via_pos];
    let rest = &expr[via_pos + 5..];

    let (lookup, alias) = if let Some(as_pos) = rest.find(" as ") {
        (&rest[..as_pos], &rest[as_pos + 4..])
    } else {
        // No alias: alias defaults to the output field name (last segment).
        let alias_default = rest.rsplit('.').next().unwrap_or(rest);
        (rest, alias_default)
    };

    // Split `includes.users.username` into ("includes.users", "username").
    let dot_pos = lookup.rfind('.')?;
    let includes_path = &lookup[..dot_pos];
    let output_field = &lookup[dot_pos + 1..];

    Some(FrontmatterJoin {
        id_path,
        includes_path,
        output_field,
        alias,
    })
}

/// Resolve the join: given an id (or array of ids), find the matching object
/// in the includes array and return its `output_field`.
///
/// Match key heuristic: prefer `id`, then `media_key`. X v2's expansion
/// arrays use one of those two as the natural key for every object type.
fn resolve_include_join<'a>(
    envelope: &'a Value,
    join: &FrontmatterJoin<'_>,
    data_root: &'a Value,
) -> Option<&'a Value> {
    // The id value at id_path within the data root. May be a scalar or an
    // array (e.g. `attachments.media_keys` is an array of media keys).
    let id_val = extract_dotpath(data_root, join.id_path)?;
    let id_scalar = match id_val {
        Value::Array(arr) => arr.first()?, // first entry; multi-id rendering is v2 of the feature
        scalar => scalar,
    };

    let array = extract_dotpath(envelope, join.includes_path)?.as_array()?;
    for item in array {
        let obj = item.as_object()?;
        let key_match = obj
            .get("id")
            .or_else(|| obj.get("media_key"))
            .map(|k| k == id_scalar)
            .unwrap_or(false);
        if key_match {
            return obj.get(join.output_field);
        }
    }
    None
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
        let url = self.url_with_default_query(&coll.list_endpoint, &coll);

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

    async fn list_resources_with_shards(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Option<serde_json::Value>)>> {
        let coll = self.find_collection(collection)?;

        let Some(populates) = coll.populates.as_deref() else {
            let metas = self.list_resources(collection).await?;
            return Ok(metas.into_iter().map(|m| (m, None)).collect());
        };

        // Parse the field exprs once; reused for every item.
        let parsed: Vec<(&str, &str)> = populates.iter().map(|e| parse_field_alias(e)).collect();

        let url = self.url_with_default_query(&coll.list_endpoint, &coll);
        let response = self.send_with_retry(|| self.client.get(&url)).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_resources_with_shards failed: HTTP {} — {}",
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

            let mut shard = serde_json::Map::new();
            for &(path, alias) in &parsed {
                if let Some(val) = extract_dotpath(item, path) {
                    shard.insert(alias.to_string(), val.clone());
                }
            }
            out.push((meta, Some(Value::Object(shard))));
        }

        Ok(out)
    }

    async fn list_resources_with_content(
        &self,
        collection: &str,
    ) -> Result<Vec<(ResourceMeta, Vec<u8>)>> {
        let coll = self.find_collection(collection)?;
        let url = self.url_with_default_query(&coll.list_endpoint, &coll);

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
        let url = self.url_with_default_query(&path, &coll);

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

        // When the spec declares `resolve_includes`, the API wraps the
        // resource in a `{"data": {...}, "includes": {...}}` envelope.
        // extract_meta needs to see the unwrapped resource so id / slug /
        // title come out non-empty; the renderer separately handles the
        // envelope for `includes` joins.
        let meta_source = data_root_for_meta(&json, coll.render.as_ref());
        let meta = Self::extract_meta(meta_source, &coll);
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

        // Read the per-resource idempotency key out of the draft frontmatter
        // *before* converting to JSON (markdown_to_json strips `_`-prefixed
        // fields). The key is stable across the whole retry loop and across
        // daemon restarts — that's the entire point.
        let idempotency_header = coll.idempotency_key_header.as_deref().and_then(|header| {
            extract_idempotency_key(content).map(|key| (header.to_string(), key))
        });

        let body_json = markdown_to_json(content, &coll)?;

        let response = self
            .send_with_retry(|| {
                let mut req = self.client.post(&url).json(&body_json);
                if let Some((ref name, ref value)) = idempotency_header {
                    req = req.header(name, value);
                }
                req
            })
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

        // Same envelope concern as `read_resource`: an X-shaped POST
        // response is `{"data": {...}}` and meta needs the unwrapped
        // resource to get a non-empty id/slug/title.
        let meta_source = data_root_for_meta(&json, coll.render.as_ref());
        let meta = Self::extract_meta(meta_source, &coll);

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
            idempotency_key_header: None,
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
            populates: None,
            default_query: None,
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
            resolve_includes: None,
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
            resolve_includes: None,
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
            resolve_includes: None,
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

    // ---------------------------------------------------------------
    // Idempotency-key handling
    // ---------------------------------------------------------------

    #[test]
    fn extract_idempotency_key_handles_present_and_missing() {
        let with_key =
            b"---\n_draft: true\n_id:\n_idempotency_key: tapfs-abc-001\ntitle: hi\n---\n\nbody";
        assert_eq!(
            extract_idempotency_key(with_key).as_deref(),
            Some("tapfs-abc-001")
        );

        let no_fm = b"plain markdown without frontmatter";
        assert!(extract_idempotency_key(no_fm).is_none());

        let fm_no_key = b"---\n_draft: true\n_id:\ntitle: hi\n---\n\nbody";
        assert!(extract_idempotency_key(fm_no_key).is_none());

        let empty_value = b"---\n_idempotency_key: \n---\n\nbody";
        assert!(
            extract_idempotency_key(empty_value).is_none(),
            "empty value must not be treated as a real key"
        );
    }

    #[tokio::test]
    async fn create_resource_sends_idempotency_header_when_configured() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/widgets"))
            .and(header("Idempotency-Key", "tapfs-fixed-123"))
            .respond_with(
                ResponseTemplate::new(201).set_body_string(r#"{"id":"new-id","slug":"hi"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("widgets");
        coll.create_endpoint = Some("/widgets".to_string());
        coll.idempotency_key_header = Some("Idempotency-Key".to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        let body = b"---\n_draft: true\n_idempotency_key: tapfs-fixed-123\ntitle: hi\n---\n\nbody";
        conn.create_resource("widgets", body)
            .await
            .expect("create with idempotency header should succeed");

        drop(server);
    }

    #[tokio::test]
    async fn create_resource_omits_header_when_spec_does_not_configure_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/widgets"))
            .respond_with(
                ResponseTemplate::new(201).set_body_string(r#"{"id":"new-id","slug":"hi"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("widgets");
        coll.create_endpoint = Some("/widgets".to_string());
        // idempotency_key_header is None — header must not be sent even if
        // the body contains an _idempotency_key field.
        let conn = build_connector(&server.uri(), vec![coll]);

        let body = b"---\n_draft: true\n_idempotency_key: tapfs-irrelevant\ntitle: hi\n---\n\nbody";
        conn.create_resource("widgets", body).await.unwrap();

        // Inspect the captured request to confirm no Idempotency-Key header.
        let received: Vec<Request> = server.received_requests().await.unwrap();
        let req = received.first().expect("a request was received");
        assert!(
            !req.headers.contains_key("idempotency-key"),
            "header should not be present when spec doesn't configure it"
        );

        drop(server);
    }

    #[tokio::test]
    async fn create_resource_omits_header_when_body_has_no_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/widgets"))
            .respond_with(
                ResponseTemplate::new(201).set_body_string(r#"{"id":"new-id","slug":"hi"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("widgets");
        coll.create_endpoint = Some("/widgets".to_string());
        coll.idempotency_key_header = Some("Idempotency-Key".to_string());
        let conn = build_connector(&server.uri(), vec![coll]);

        // No _idempotency_key in body — header should be omitted (not sent
        // empty), so the API doesn't reject the request.
        let body = b"---\n_draft: true\ntitle: hi\n---\n\nbody";
        conn.create_resource("widgets", body).await.unwrap();

        let received: Vec<Request> = server.received_requests().await.unwrap();
        let req = received.first().unwrap();
        assert!(
            !req.headers.contains_key("idempotency-key"),
            "header should not be sent when key is missing from body"
        );

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
        coll.delete_body = Some(r#"{"archived":true,"deleted_id":"{id}"}"#.to_string());
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
            resolve_includes: None,
        };
        let bytes = RestConnector::render_with_spec(&meta, &v, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("## Labels"));
        assert!(output.contains("None."));
    }

    // ---------------------------------------------------------------
    // list_resources_with_shards: spec-driven field projection
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn list_with_shards_projects_populates_fields() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"[
            {"id":"1","title":"first","state":"open","extra":"ignore-me","user":{"login":"alice"}},
            {"id":"2","title":"second","state":"closed","extra":"ignore-me","user":{"login":"bob"}}
        ]"#;
        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("items");
        coll.populates = Some(vec![
            "title".to_string(),
            "state".to_string(),
            "user.login as author".to_string(),
        ]);
        let conn = build_connector(&server.uri(), vec![coll]);

        let out = conn
            .list_resources_with_shards("items")
            .await
            .expect("list_resources_with_shards should succeed");

        assert_eq!(out.len(), 2);

        // First item: shard contains only populates fields, with the alias applied.
        let (meta1, shard1) = &out[0];
        assert_eq!(meta1.id, "1");
        let shard1 = shard1
            .as_ref()
            .expect("shard must be present when populates is set");
        assert_eq!(shard1["title"], "first");
        assert_eq!(shard1["state"], "open");
        assert_eq!(shard1["author"], "alice");
        assert!(
            shard1.get("extra").is_none(),
            "non-populates fields must be excluded from the shard"
        );

        // Second item: same projection.
        let (_meta2, shard2) = &out[1];
        let shard2 = shard2.as_ref().unwrap();
        assert_eq!(shard2["state"], "closed");
        assert_eq!(shard2["author"], "bob");

        drop(server);
    }

    #[tokio::test]
    async fn list_with_shards_returns_none_when_populates_unset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"id":"1","title":"x"}]"#))
            .mount(&server)
            .await;

        let coll = minimal_collection("items");
        let conn = build_connector(&server.uri(), vec![coll]);

        let out = conn.list_resources_with_shards("items").await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            out[0].1.is_none(),
            "shard must be None when spec does not declare populates"
        );

        drop(server);
    }

    // ---------------------------------------------------------------
    // apply_default_query — collection-level static query params
    //
    // Motivation: X v2 endpoints return a near-empty payload unless the
    // request opts in via `expansions=...&tweet.fields=...`. Inlining
    // those into every spec endpoint is brittle (they're repeated, easy
    // to drift) and we'd like other connectors (Linear, Notion) to share
    // the same mechanism.
    // ---------------------------------------------------------------
    #[test]
    fn apply_default_query_none_returns_path_unchanged() {
        assert_eq!(apply_default_query("/2/tweets/42", None), "/2/tweets/42");
    }

    #[test]
    fn apply_default_query_appends_to_path_without_existing_query() {
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        let out = apply_default_query("/2/tweets/42", Some(&q));
        assert_eq!(out, "/2/tweets/42?expansions=author_id");
    }

    #[test]
    fn apply_default_query_appends_with_ampersand_when_path_has_query() {
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        let out = apply_default_query("/2/users/me/tweets?max_results=100", Some(&q));
        assert_eq!(
            out,
            "/2/users/me/tweets?max_results=100&expansions=author_id"
        );
    }

    #[test]
    fn apply_default_query_preserves_dotted_keys_required_by_x() {
        // `tweet.fields` and `user.fields` are X v2's query-param names.
        // They must NOT be URL-encoded — X rejects `tweet%2Efields` — so the
        // key passes through verbatim. The *value* may contain commas (also
        // legal unencoded in X's parser) and is left as-is for round-trip
        // fidelity with the spec author's input.
        let mut q = std::collections::BTreeMap::new();
        q.insert(
            "tweet.fields".to_string(),
            "created_at,public_metrics".to_string(),
        );
        q.insert("user.fields".to_string(), "username,verified".to_string());
        let out = apply_default_query("/2/tweets/42", Some(&q));
        // BTreeMap iterates in sorted key order: tweet.fields < user.fields.
        assert_eq!(
            out,
            "/2/tweets/42?tweet.fields=created_at,public_metrics&user.fields=username,verified"
        );
    }

    // ---------------------------------------------------------------
    // Wiring: default_query must reach the HTTP layer
    //
    // Spec says these tests cover the integration: the helper above is
    // pure-string, but the value is in actually merging it into every
    // outbound REST call. Mock servers assert that the real HTTP request
    // carries `expansions=author_id` (etc.) — without the wiring, the
    // mocks return 404 (no match) and the connector calls fail.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn list_resources_appends_default_query_to_request_url() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/me/tweets"))
            .and(query_param("expansions", "author_id"))
            .and(query_param("tweet.fields", "created_at,public_metrics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[]"#))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("posts");
        coll.list_endpoint = "/2/users/me/tweets".to_string();
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        q.insert(
            "tweet.fields".to_string(),
            "created_at,public_metrics".to_string(),
        );
        coll.default_query = Some(q);

        let conn = build_connector(&server.uri(), vec![coll]);
        conn.list_resources("posts")
            .await
            .expect("list should hit the mock with default_query merged in");
        drop(server);
    }

    #[tokio::test]
    async fn read_resource_appends_default_query_to_request_url() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/tweets/42"))
            .and(query_param("expansions", "author_id"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"id":"42"}}"#))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("posts");
        coll.get_endpoint = "/2/tweets/{id}".to_string();
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        coll.default_query = Some(q);

        let conn = build_connector(&server.uri(), vec![coll]);
        conn.read_resource("posts", "42")
            .await
            .expect("read should hit the mock with default_query merged in");
        drop(server);
    }

    #[tokio::test]
    async fn list_resources_merges_default_query_with_existing_query_in_endpoint() {
        // Endpoint already has `?max_results=100` baked in; default_query
        // appends with `&` rather than clobbering.
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/me/tweets"))
            .and(query_param("max_results", "100"))
            .and(query_param("expansions", "author_id"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[]"#))
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("posts");
        coll.list_endpoint = "/2/users/me/tweets?max_results=100".to_string();
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        coll.default_query = Some(q);

        let conn = build_connector(&server.uri(), vec![coll]);
        conn.list_resources("posts")
            .await
            .expect("list should preserve baked-in query and append default_query");
        drop(server);
    }

    #[test]
    fn apply_default_query_url_encodes_values_with_reserved_chars() {
        // Values containing `&` or `=` would corrupt the query string if
        // passed through verbatim — they must be percent-encoded. Commas
        // are deliberately NOT encoded (X uses bare commas as list sep).
        let mut q = std::collections::BTreeMap::new();
        q.insert(
            "query".to_string(),
            "from:elonmusk AND has:links".to_string(),
        );
        let out = apply_default_query("/2/tweets/search/recent", Some(&q));
        assert_eq!(
            out,
            "/2/tweets/search/recent?query=from%3Aelonmusk%20AND%20has%3Alinks"
        );
    }

    // ---------------------------------------------------------------
    // resolve_includes — v2 envelope unwrap + includes-array join
    //
    // X v2 wraps a single resource as `{"data": {...}, "includes": {...}}`.
    // Without resolve_includes, the renderer would need `data.text`,
    // `data.author_id` everywhere — brittle and ugly. With it, the renderer
    // treats `data` as the resource root and lets frontmatter entries join
    // into `includes` to surface human-readable fields (author username,
    // media URL) instead of opaque ids.
    // ---------------------------------------------------------------
    #[test]
    fn resolve_includes_unwraps_data_envelope() {
        let json = json!({
            "data": {"text": "hi from elon", "author_id": "44196397"},
            "includes": {}
        });
        let spec = RenderSpec {
            frontmatter: Some(vec!["author_id".to_string()]),
            body: Some("text".to_string()),
            sections: None,
            exclude: None,
            resolve_includes: Some(true),
        };
        let meta = minimal_meta();
        let bytes = RestConnector::render_with_spec(&meta, &json, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(
            output.contains("author_id: \"44196397\""),
            "frontmatter author_id must read from data root, got:\n{}",
            output
        );
        assert!(
            output.contains("hi from elon"),
            "body must read from data root, got:\n{}",
            output
        );
    }

    #[test]
    fn resolve_includes_joins_author_id_to_users_username() {
        let json = json!({
            "data": {"id": "42", "text": "hi", "author_id": "44196397"},
            "includes": {
                "users": [
                    {"id": "44196397", "username": "elonmusk", "name": "Elon Musk"}
                ]
            }
        });
        let spec = RenderSpec {
            frontmatter: Some(vec![
                "id".to_string(),
                "author_id via includes.users.username as author".to_string(),
            ]),
            body: Some("text".to_string()),
            sections: None,
            exclude: None,
            resolve_includes: Some(true),
        };
        let meta = minimal_meta();
        let bytes = RestConnector::render_with_spec(&meta, &json, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(
            output.contains("id: \"42\""),
            "plain id from data root, got:\n{}",
            output
        );
        assert!(
            output.contains("author: \"elonmusk\""),
            "author_id should join into includes.users.username, got:\n{}",
            output
        );
    }

    #[test]
    fn resolve_includes_joins_media_key_to_media_url() {
        // The join key for includes.media is `media_key`, not `id` — X's
        // expansion arrays use the natural primary key for the object type.
        // The renderer should pick `media_key` when the array's items have
        // one (users → id, media → media_key, places → id, polls → id).
        let json = json!({
            "data": {
                "text": "look at this",
                "attachments": {"media_keys": ["mk-1"]}
            },
            "includes": {
                "media": [
                    {"media_key": "mk-1", "type": "photo", "url": "https://example.com/cat.jpg"}
                ]
            }
        });
        let spec = RenderSpec {
            frontmatter: Some(vec![
                "attachments.media_keys via includes.media.url as media_url".to_string(),
            ]),
            body: Some("text".to_string()),
            sections: None,
            exclude: None,
            resolve_includes: Some(true),
        };
        let meta = minimal_meta();
        let bytes = RestConnector::render_with_spec(&meta, &json, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(
            output.contains("media_url: \"https://example.com/cat.jpg\""),
            "media_keys[0] should join into includes.media.url, got:\n{}",
            output
        );
    }

    #[test]
    fn resolve_includes_off_by_default_keeps_existing_behavior() {
        // Sanity: when resolve_includes is None, GitHub-style specs continue
        // to read fields off the top-level JSON, not `data`. Regression
        // guard so we don't break every other existing connector.
        let json = json!({"state": "open", "title": "regression check"});
        let spec = RenderSpec {
            frontmatter: Some(vec!["state".to_string(), "title".to_string()]),
            body: None,
            sections: None,
            exclude: None,
            resolve_includes: None,
        };
        let meta = minimal_meta();
        let bytes = RestConnector::render_with_spec(&meta, &json, &spec);
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("state: \"open\""));
        assert!(output.contains("title: \"regression check\""));
    }

    // ---------------------------------------------------------------
    // read_resource must extract meta from `data` when resolve_includes
    // is on (PR #50 review finding P3)
    //
    // The renderer already unwraps the envelope for body/sections, but
    // `extract_meta` was called against the raw `{"data": {...}}` shape.
    // That left meta.id / slug / title empty for X tweet and user reads,
    // dropping the "# title" heading in rendered markdown and feeding
    // bad metadata to direct connector callers.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn read_resource_extracts_meta_from_data_envelope_when_resolve_includes_set() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // X-shaped single-tweet response: data + includes envelope.
        let body = r#"{
            "data": {"id": "42", "text": "hello", "author_id": "44196397"},
            "includes": {
                "users": [{"id": "44196397", "username": "elonmusk", "name": "Elon Musk"}]
            }
        }"#;
        Mock::given(method("GET"))
            .and(path("/2/tweets/42"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let mut coll = minimal_collection("posts");
        coll.list_endpoint = "/2/tweets/search/recent".to_string();
        coll.get_endpoint = "/2/tweets/{id}".to_string();
        coll.id_field = Some("id".to_string());
        coll.slug_field = Some("id".to_string());
        coll.title_field = Some("text".to_string());
        coll.render = Some(RenderSpec {
            frontmatter: Some(vec!["id".to_string()]),
            body: Some("text".to_string()),
            sections: None,
            exclude: None,
            resolve_includes: Some(true),
        });
        let conn = build_connector(&server.uri(), vec![coll]);

        let resource = conn
            .read_resource("posts", "42")
            .await
            .expect("read should succeed");
        assert_eq!(
            resource.meta.id, "42",
            "meta.id must come from data.id, not the envelope (which has no id)"
        );
        assert_eq!(
            resource.meta.title.as_deref(),
            Some("hello"),
            "meta.title must come from data.text, not the envelope"
        );
        // The rendered markdown should still include the "# hello" heading
        // that was previously dropped because meta.title was empty.
        let content = String::from_utf8(resource.content).unwrap();
        assert!(
            content.contains("# hello"),
            "rendered output must include the title heading, got:\n{}",
            content
        );
        drop(server);
    }

    // ---------------------------------------------------------------
    // list_resources_with_shards must also apply default_query
    // (PR #50 review finding P2)
    //
    // Without this, an X-shaped collection that declares BOTH populates
    // (so the VFS hydrates frontmatter shards at list time) AND
    // default_query (so the API returns useful payloads) silently sends
    // the request without `expansions=` / `tweet.fields=` — the API
    // returns the bare-minimum payload, populates pulls empty values,
    // and the shard layer caches uselessness.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn list_with_shards_appends_default_query_to_request_url() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/me/tweets"))
            .and(query_param("expansions", "author_id"))
            .and(query_param("tweet.fields", "created_at,public_metrics"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"[{"id":"1","text":"hi","public_metrics":{"like_count":3}}]"#,
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut coll = minimal_collection("posts");
        coll.list_endpoint = "/2/users/me/tweets".to_string();
        coll.populates = Some(vec![
            "text".to_string(),
            "public_metrics.like_count as likes".to_string(),
        ]);
        let mut q = std::collections::BTreeMap::new();
        q.insert("expansions".to_string(), "author_id".to_string());
        q.insert(
            "tweet.fields".to_string(),
            "created_at,public_metrics".to_string(),
        );
        coll.default_query = Some(q);

        let conn = build_connector(&server.uri(), vec![coll]);
        let out = conn
            .list_resources_with_shards("posts")
            .await
            .expect("shard listing must merge default_query");
        // populates was set → each item carries a shard, not None.
        assert_eq!(out.len(), 1);
        let (_meta, shard) = &out[0];
        let shard = shard
            .as_ref()
            .expect("populates → shard must be present, regardless of default_query path");
        assert_eq!(shard["text"], "hi");
        assert_eq!(shard["likes"], 3);
        drop(server);
    }

    // ---------------------------------------------------------------
    // PKCE refresh: ensure_token must NOT send client_secret when None
    //
    // Critical invariant: public clients (PKCE) authenticate refresh via
    // refresh_token + client_id only. Sending an empty client_secret
    // confuses some providers (silent invalid_client). Sending a stale
    // secret leaks an unrelated credential. The wiremock matcher
    // body_string_contains is used in the negative direction to confirm
    // the form body simply doesn't carry that key.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn ensure_token_pkce_refresh_omits_client_secret() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Match only requests carrying the PKCE-shaped form. The negative
        // check is below (assert the request didn't carry client_secret).
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_id=pkce-client"))
            .and(body_string_contains("refresh_token=rt-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"new-access","expires_in":7200,"token_type":"bearer"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut spec = crate::connector::spec::ConnectorSpec {
            spec_version: None,
            version: None,
            description: None,
            name: "test".to_string(),
            base_url: server.uri(),
            auth: Some(crate::connector::spec::AuthSpec {
                auth_type: "oauth2_pkce".to_string(),
                token_env: None,
                setup_url: None,
                setup_instructions: None,
                auth_url: None,
                token_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
                device_code_url: None,
            }),
            transport: None,
            capabilities: None,
            agent: None,
            collections: vec![minimal_collection("items")],
        };
        spec.collections[0].list_endpoint = "/items".to_string();

        let token_url = format!("{}/oauth/token", server.uri());
        let oauth_cfg = OAuth2Config {
            token_url,
            client_id: "pkce-client".to_string(),
            client_secret: None, // <-- PKCE: no secret
            refresh_token: "rt-abc".to_string(),
            // Force a refresh on first request: expiry already in the past.
            expiry: std::sync::RwLock::new(Some(
                Instant::now() - std::time::Duration::from_secs(1),
            )),
        };
        let conn = RestConnector::new_with_oauth2(
            spec,
            reqwest::Client::new(),
            Some("stale-token".to_string()),
            oauth_cfg,
        );

        // Set up a second mock for the actual data fetch (with Bearer of
        // the refreshed token), so the surrounding `list_resources` succeeds
        // and the refresh path runs to completion.
        Mock::given(method("GET"))
            .and(path("/items"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer new-access",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;

        conn.list_resources("items")
            .await
            .expect("refresh path should succeed and feed list_resources");

        // The .expect(1) on the refresh mock asserts the refresh happened
        // and carried the expected form fields. Confirm the form body did
        // NOT also carry a client_secret.
        let requests = server.received_requests().await.unwrap();
        let refresh_req = requests
            .iter()
            .find(|r| r.url.path().ends_with("/oauth/token"))
            .expect("refresh request missing");
        let body = String::from_utf8_lossy(&refresh_req.body);
        assert!(
            !body.contains("client_secret"),
            "PKCE refresh leaked client_secret: {}",
            body
        );

        drop(server);
    }

    #[tokio::test]
    async fn ensure_token_confidential_refresh_still_sends_client_secret() {
        // Regression guard for the existing Google/confidential-client path.
        // Same shape as the PKCE test above but with `client_secret: Some(..)`.
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_secret=top-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"new-access","expires_in":3600,"token_type":"bearer"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut spec = crate::connector::spec::ConnectorSpec {
            spec_version: None,
            version: None,
            description: None,
            name: "test".to_string(),
            base_url: server.uri(),
            auth: Some(crate::connector::spec::AuthSpec {
                auth_type: "oauth2".to_string(),
                token_env: None,
                setup_url: None,
                setup_instructions: None,
                auth_url: None,
                token_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
                device_code_url: None,
            }),
            transport: None,
            capabilities: None,
            agent: None,
            collections: vec![minimal_collection("items")],
        };
        spec.collections[0].list_endpoint = "/items".to_string();

        let token_url = format!("{}/oauth/token", server.uri());
        let oauth_cfg = OAuth2Config {
            token_url,
            client_id: "conf-client".to_string(),
            client_secret: Some("top-secret".to_string()),
            refresh_token: "rt-conf".to_string(),
            expiry: std::sync::RwLock::new(Some(
                Instant::now() - std::time::Duration::from_secs(1),
            )),
        };
        let conn = RestConnector::new_with_oauth2(
            spec,
            reqwest::Client::new(),
            Some("stale-token".to_string()),
            oauth_cfg,
        );

        Mock::given(method("GET"))
            .and(path("/items"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer new-access",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;

        conn.list_resources("items")
            .await
            .expect("confidential refresh path should still work");

        drop(server);
    }

    // ---------------------------------------------------------------
    // Refresh when access token is missing (PR #49 review finding P2)
    //
    // ensure_token previously only refreshed when expiry was in the past
    // OR (when no expiry info existed) when the access token was None.
    // The pathological middle case — `token = None` with a future expiry
    // — would skip the refresh and send the request unauthenticated.
    // This can happen if the keychain blob is unreadable / partially
    // populated while credentials.yaml still has expires_at recorded.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn ensure_token_refreshes_when_token_none_even_with_future_expiry() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt-missing-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"recovered-access","expires_in":7200,"token_type":"bearer"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut spec = crate::connector::spec::ConnectorSpec {
            spec_version: None,
            version: None,
            description: None,
            name: "test".to_string(),
            base_url: server.uri(),
            auth: Some(crate::connector::spec::AuthSpec {
                auth_type: "oauth2_pkce".to_string(),
                token_env: None,
                setup_url: None,
                setup_instructions: None,
                auth_url: None,
                token_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
                device_code_url: None,
            }),
            transport: None,
            capabilities: None,
            agent: None,
            collections: vec![minimal_collection("items")],
        };
        spec.collections[0].list_endpoint = "/items".to_string();

        let token_url = format!("{}/oauth/token", server.uri());
        let oauth_cfg = OAuth2Config {
            token_url,
            client_id: "pkce-client".to_string(),
            client_secret: None,
            refresh_token: "rt-missing-tok".to_string(),
            // Future expiry — a naive "is now past expiry?" check would say
            // no and skip the refresh. The fix must also consider whether
            // the access token itself is available.
            expiry: std::sync::RwLock::new(Some(
                Instant::now() + std::time::Duration::from_secs(3600),
            )),
        };
        let conn = RestConnector::new_with_oauth2(
            spec,
            reqwest::Client::new(),
            None, // <-- access token missing
            oauth_cfg,
        );

        Mock::given(method("GET"))
            .and(path("/items"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer recovered-access",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;

        conn.list_resources("items")
            .await
            .expect("token-None + future-expiry should still trigger refresh");

        drop(server);
    }
}

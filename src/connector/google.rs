//! Google Workspace connector for tapfs.
//!
//! Exposes Google Drive, Gmail, and Calendar as filesystem collections.
//! Authentication uses a credential chain:
//!   1. `GOOGLE_ACCESS_TOKEN` env var (raw bearer token)
//!   2. `GOOGLE_CREDENTIALS_FILE` env var -> path to JSON credentials
//!   3. `~/.config/gws/credentials.json`
//!   4. `~/.config/gcloud/application_default_credentials.json` (ADC)
//!
//! Refresh tokens are exchanged via raw HTTP POST to Google's token endpoint.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};

// ---------------------------------------------------------------------------
// Token provider
// ---------------------------------------------------------------------------

/// Credentials file format (supports authorized_user from gcloud / gws CLI).
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    #[allow(dead_code)]
    r#type: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    // For service accounts (not used yet, but parsed so we don't fail)
    #[allow(dead_code)]
    token_uri: Option<String>,
}

impl std::fmt::Debug for CredentialsFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialsFile")
            .field("type", &self.r#type)
            .field(
                "client_id",
                &self.client_id.as_deref().map(|_| "[REDACTED]"),
            )
            .field("client_secret", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .finish()
    }
}

/// Parsed credentials ready for token refresh (parsed once at init).
struct ParsedCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

/// Manages access tokens with automatic refresh and proactive expiry tracking.
struct TokenProvider {
    /// Cached access token (may be stale).
    access_token: RwLock<Option<String>>,
    /// When the cached token expires (not set for env-var tokens).
    token_expiry: RwLock<Option<std::time::Instant>>,
    /// Pre-parsed credentials from the JSON file, if one was found.
    credentials: Option<ParsedCredentials>,
    /// HTTP client for token refresh requests.
    client: Client,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

impl TokenProvider {
    /// Create a new TokenProvider using the credential chain.
    ///
    /// Credentials are parsed eagerly so that refresh calls don't need to
    /// re-read and re-parse the JSON file each time.
    fn new(client: Client) -> Self {
        // 1. Check GOOGLE_ACCESS_TOKEN env var
        let env_token = std::env::var("GOOGLE_ACCESS_TOKEN").ok();

        // 2. Find and parse credentials file
        let credentials = Self::find_and_parse_credentials();

        if credentials.is_some() {
            tracing::debug!("parsed Google credentials for token refresh");
        }

        Self {
            access_token: RwLock::new(env_token),
            token_expiry: RwLock::new(None),
            credentials,
            client,
        }
    }

    /// Walk the credential chain to find a credentials file.
    fn find_credentials_path() -> Option<String> {
        // GOOGLE_CREDENTIALS_FILE env var
        if let Ok(path) = std::env::var("GOOGLE_CREDENTIALS_FILE") {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }

        let home = dirs::home_dir()?;

        // ~/.config/gws/credentials.json
        let gws = home.join(".config/gws/credentials.json");
        if gws.exists() {
            return Some(gws.to_string_lossy().into_owned());
        }

        // ~/.config/gcloud/application_default_credentials.json (ADC)
        let adc = home.join(".config/gcloud/application_default_credentials.json");
        if adc.exists() {
            return Some(adc.to_string_lossy().into_owned());
        }

        None
    }

    /// Find and parse the credentials file once at init.
    fn find_and_parse_credentials() -> Option<ParsedCredentials> {
        // Try JSON credential files first
        if let Some(path) = Self::find_credentials_path() {
            let data = std::fs::read_to_string(&path).ok()?;
            let creds: CredentialsFile = serde_json::from_str(&data).ok()?;
            if let Some(parsed) = Self::parse_creds_file(creds) {
                return Some(parsed);
            }
        }

        // Fall back to ~/.tapfs/credentials.yaml (saved by `tap mount google` OAuth2 flow)
        let tapfs_dir = dirs::home_dir()?.join(".tapfs");
        if let Ok(store) = crate::credentials::CredentialStore::load(&tapfs_dir) {
            if let Some(cred) = store.get("google") {
                if let (Some(ref rt), Some(ref cid), Some(ref cs)) =
                    (&cred.refresh_token, &cred.client_id, &cred.client_secret)
                {
                    return Some(ParsedCredentials {
                        client_id: cid.clone(),
                        client_secret: cs.clone(),
                        refresh_token: rt.clone(),
                    });
                }
                // Also check if we just have a plain access token
                // (from device flow or similar — no refresh possible)
            }
        }

        None
    }

    fn parse_creds_file(creds: CredentialsFile) -> Option<ParsedCredentials> {
        Some(ParsedCredentials {
            client_id: creds.client_id?,
            client_secret: creds.client_secret?,
            refresh_token: creds.refresh_token?,
        })
    }

    /// Get a valid access token, refreshing proactively if close to expiry.
    async fn get_token(&self) -> Result<String> {
        // Fast path: check if we have a non-expired token cached
        {
            let guard = self.access_token.read().await;
            if let Some(ref token) = *guard {
                let expiry_guard = self.token_expiry.read().await;
                if let Some(expiry) = *expiry_guard {
                    if std::time::Instant::now() < expiry {
                        return Ok(token.clone());
                    }
                    // Token expired, fall through to refresh
                } else {
                    // No expiry tracked (env var token), return as-is
                    return Ok(token.clone());
                }
            }
        }

        // Slow path: refresh the token
        self.refresh().await
    }

    /// Exchange a refresh token for a new access token using Google's token endpoint.
    async fn refresh(&self) -> Result<String> {
        let creds = self.credentials.as_ref().ok_or_else(|| {
            anyhow!(
                "no Google credentials found. Set GOOGLE_ACCESS_TOKEN or \
                 provide a credentials file via GOOGLE_CREDENTIALS_FILE, \
                 ~/.config/gws/credentials.json, or \
                 ~/.config/gcloud/application_default_credentials.json"
            )
        })?;

        tracing::debug!("refreshing Google access token");

        let resp = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", creds.refresh_token.as_str()),
                ("client_id", creds.client_id.as_str()),
                ("client_secret", creds.client_secret.as_str()),
            ])
            .send()
            .await
            .context("token refresh request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("token refresh failed: HTTP {} - {}", status, body));
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("failed to parse token response")?;

        // Cache the token
        {
            let mut guard = self.access_token.write().await;
            *guard = Some(token_resp.access_token.clone());
        }

        // Store expiry at 80% of TTL for proactive refresh
        if let Some(expires_in) = token_resp.expires_in {
            let expiry = std::time::Instant::now() + Duration::from_secs(expires_in * 80 / 100);
            *self.token_expiry.write().await = Some(expiry);
        }

        Ok(token_resp.access_token)
    }

    /// Invalidate the cached token (called on 401 responses).
    async fn invalidate(&self) {
        let mut guard = self.access_token.write().await;
        *guard = None;
        *self.token_expiry.write().await = None;
    }
}

// ---------------------------------------------------------------------------
// Google Workspace connector
// ---------------------------------------------------------------------------

pub struct GoogleWorkspaceConnector {
    client: Client,
    token_provider: TokenProvider,
    /// Maps "collection/slug" → API resource ID (e.g., Google Drive file ID).
    /// Populated during list_resources(), used by read/write/delete.
    slug_to_id: dashmap::DashMap<String, String>,
    /// Reverse map: API resource ID → "collection/slug".
    /// Used for O(1) existence checks instead of linear scans.
    id_to_slug: dashmap::DashMap<String, String>,
}

impl GoogleWorkspaceConnector {
    /// Create a new Google Workspace connector.
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .context("building HTTP client")?;
        let token_provider = TokenProvider::new(client.clone());
        Ok(Self {
            client,
            token_provider,
            slug_to_id: dashmap::DashMap::new(),
            id_to_slug: dashmap::DashMap::new(),
        })
    }

    /// Build an authenticated GET request.
    async fn auth_get(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        let token = self.token_provider.get_token().await?;
        Ok(self.client.get(url).bearer_auth(token))
    }

    /// Build an authenticated PATCH request.
    async fn auth_patch(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        let token = self.token_provider.get_token().await?;
        Ok(self.client.patch(url).bearer_auth(token))
    }

    /// Send a GET request with retries on 401 (token refresh), 429 (rate
    /// limit), and 503 (service unavailable).  Uses exponential backoff.
    async fn send_with_retry(&self, url: &str) -> Result<reqwest::Response> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            let request = self.auth_get(url).await?;
            let resp = request
                .send()
                .await
                .with_context(|| format!("GET {}", url))?;

            match resp.status() {
                s if s == reqwest::StatusCode::UNAUTHORIZED => {
                    self.token_provider.invalidate().await;
                    last_err = Some(anyhow!("GET {} unauthorized (401)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    // Respect Retry-After header when present.
                    if let Some(retry_after) = resp.headers().get("retry-after") {
                        if let Ok(secs) = retry_after.to_str().unwrap_or("5").parse::<u64>() {
                            tokio::time::sleep(Duration::from_secs(secs)).await;
                        }
                    }
                    last_err = Some(anyhow!("GET {} rate limited (429)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    last_err = Some(anyhow!("GET {} service unavailable (503)", url));
                    continue;
                }
                s if s.is_success() => return Ok(resp),
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("GET {} failed: HTTP {} - {}", url, s, body));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("GET {} failed after {} retries", url, max_retries)))
    }

    /// Send a GET request and parse the response as JSON.
    async fn get_json(&self, url: &str) -> Result<Value> {
        let resp = self.send_with_retry(url).await?;
        resp.json().await.context("parsing JSON response")
    }

    /// Send a GET request and return raw bytes (for file downloads).
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.send_with_retry(url).await?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .context("reading response bytes")
    }

    // -----------------------------------------------------------------------
    // Slug ↔ ID mapping
    // -----------------------------------------------------------------------

    /// Cache a slug → ID mapping for a collection.
    fn cache_slug(&self, collection: &str, slug: &str, id: &str) {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id.insert(key, id.to_string());
        self.id_to_slug
            .insert(id.to_string(), format!("{}/{}", collection, slug));
    }

    /// Resolve a slug to its API ID. Falls back to using the slug as-is
    /// (which works when the slug IS the ID, e.g., for Gmail/Calendar).
    fn resolve_id(&self, collection: &str, slug: &str) -> String {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_else(|| slug.to_string())
    }

    // -----------------------------------------------------------------------
    // Collection routing helpers
    // -----------------------------------------------------------------------

    /// Parse a collection string into (service, folder_id).
    /// - "drive" -> ("drive", None)
    /// - "drive/FOLDER_ID" -> ("drive", Some("FOLDER_ID"))
    /// - "gmail" -> ("gmail", None)
    /// - "calendar" -> ("calendar", None)
    fn parse_collection(collection: &str) -> (&str, Option<&str>) {
        if let Some(rest) = collection.strip_prefix("drive/") {
            ("drive", Some(rest))
        } else {
            (collection, None)
        }
    }

    // -----------------------------------------------------------------------
    // Google Drive
    // -----------------------------------------------------------------------

    async fn drive_list(&self, folder_id: Option<&str>) -> Result<Vec<ResourceMeta>> {
        let parent = folder_id.unwrap_or("root");
        let mut all_resources = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "https://www.googleapis.com/drive/v3/files\
                 ?fields=nextPageToken,files(id,name,mimeType,modifiedTime)\
                 &q='{}'+in+parents+and+trashed=false\
                 &pageSize=100\
                 &orderBy=modifiedTime+desc",
                parent
            );
            if let Some(ref token) = page_token {
                url.push_str(&format!("&pageToken={}", token));
            }

            tracing::debug!(url = %url, "drive: listing files");
            let json = self.get_json(&url).await?;

            if let Some(files) = json.get("files").and_then(|v| v.as_array()) {
                for file in files {
                    let id = file.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let name = file
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("untitled");
                    let mime = file.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
                    let modified = file.get("modifiedTime").and_then(|v| v.as_str());

                    let slug = sanitize_slug(name);

                    // Mark folders with a trailing slash in the content_type
                    let content_type = if mime == "application/vnd.google-apps.folder" {
                        Some("inode/directory".to_string())
                    } else {
                        Some(mime.to_string())
                    };

                    all_resources.push(ResourceMeta {
                        id: id.to_string(),
                        slug,
                        title: Some(name.to_string()),
                        updated_at: modified.map(|s| s.to_string()),
                        content_type,
                        group: None,
                    });
                }
            }

            // Handle pagination
            match json.get("nextPageToken").and_then(|v| v.as_str()) {
                Some(token) => page_token = Some(token.to_string()),
                None => break,
            }
        }

        Ok(all_resources)
    }

    async fn drive_read(&self, id: &str) -> Result<Resource> {
        // First, get file metadata
        let meta_url = format!(
            "https://www.googleapis.com/drive/v3/files/{}\
             ?fields=id,name,mimeType,modifiedTime,owners,size",
            id
        );
        let meta_json = self.get_json(&meta_url).await?;

        let name = meta_json
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("untitled");
        let mime = meta_json
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let modified = meta_json
            .get("modifiedTime")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let slug = sanitize_slug(name);

        let meta = ResourceMeta {
            id: id.to_string(),
            slug,
            title: Some(name.to_string()),
            updated_at: Some(modified.to_string()),
            content_type: Some(mime.to_string()),
            group: None,
        };

        // Determine how to fetch content
        let body_content = if mime.starts_with("application/vnd.google-apps.") {
            // Google Docs/Sheets/etc -> export
            let export_mime = match mime {
                "application/vnd.google-apps.document" => "text/plain",
                "application/vnd.google-apps.spreadsheet" => "text/csv",
                "application/vnd.google-apps.presentation" => "text/plain",
                "application/vnd.google-apps.drawing" => "image/svg+xml",
                _ => "text/plain",
            };
            let export_url = format!(
                "https://www.googleapis.com/drive/v3/files/{}/export?mimeType={}",
                id, export_mime
            );
            match self.get_bytes(&export_url).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => {
                    tracing::warn!(id = %id, error = %e, "failed to export Google doc");
                    format!("[Export failed: {}]", e)
                }
            }
        } else if mime == "application/vnd.google-apps.folder" {
            // Folders: list children as summary
            match self.drive_list(Some(id)).await {
                Ok(children) => {
                    let mut lines = Vec::new();
                    for child in &children {
                        let kind = child.content_type.as_deref().unwrap_or("file");
                        let label = if kind == "inode/directory" {
                            "dir"
                        } else {
                            "file"
                        };
                        lines.push(format!(
                            "- [{}] {} ({})",
                            label,
                            child.title.as_deref().unwrap_or(&child.slug),
                            child.id
                        ));
                    }
                    if lines.is_empty() {
                        "(empty folder)".to_string()
                    } else {
                        lines.join("\n")
                    }
                }
                Err(e) => format!("[Failed to list folder contents: {}]", e),
            }
        } else {
            // Binary / regular files -> download
            let download_url =
                format!("https://www.googleapis.com/drive/v3/files/{}?alt=media", id);
            match self.get_bytes(&download_url).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => format!("[Download failed: {}]", e),
            }
        };

        // Render as markdown with frontmatter
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", id));
        out.push_str(&format!("name: \"{}\"\n", escape_yaml(name)));
        out.push_str(&format!("mimeType: \"{}\"\n", mime));
        out.push_str(&format!("modifiedTime: \"{}\"\n", modified));
        out.push_str("operations: [read, write, draft, lock]\n");
        out.push_str("---\n\n");
        out.push_str(&body_content);
        out.push('\n');

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    async fn drive_write(&self, id: &str, content: &[u8]) -> Result<()> {
        let token = self.token_provider.get_token().await?;

        // Strip YAML frontmatter if present
        let body = strip_frontmatter(content);

        // Check if this is a known file ID (exists in slug cache) or a new file
        let is_existing = self.id_to_slug.contains_key(id);

        if is_existing {
            // Update existing file via PATCH
            let url = format!(
                "https://www.googleapis.com/upload/drive/v3/files/{}?uploadType=media",
                id
            );
            let resp = self
                .client
                .patch(&url)
                .bearer_auth(&token)
                .header("Content-Type", "application/octet-stream")
                .body(body.to_vec())
                .send()
                .await
                .context("drive write (update) failed")?;

            let status = resp.status();
            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "drive write (update) failed: HTTP {} - {}",
                    status,
                    err_body
                ));
            }
        } else {
            // Create new file via POST
            // First, create the file metadata
            let metadata = serde_json::json!({
                "name": format!("{}.md", id),
                "mimeType": "text/markdown",
            });

            let url = "https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart";

            // Build multipart body
            let boundary = "tapfs_boundary_2026";
            let mut multipart = Vec::new();
            multipart.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
            multipart.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
            multipart.extend_from_slice(metadata.to_string().as_bytes());
            multipart.extend_from_slice(format!("\r\n--{}\r\n", boundary).as_bytes());
            multipart.extend_from_slice(b"Content-Type: text/markdown\r\n\r\n");
            multipart.extend_from_slice(&body);
            multipart.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());

            let resp = self
                .client
                .post(url)
                .bearer_auth(&token)
                .header(
                    "Content-Type",
                    format!("multipart/related; boundary={}", boundary),
                )
                .body(multipart)
                .send()
                .await
                .context("drive write (create) failed")?;

            let status = resp.status();
            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "drive write (create) failed: HTTP {} - {}",
                    status,
                    err_body
                ));
            }

            tracing::info!(name = %id, "created new file in Google Drive");
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Gmail
    // -----------------------------------------------------------------------

    async fn gmail_list(&self) -> Result<Vec<ResourceMeta>> {
        let mut all_resources = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = "https://gmail.googleapis.com/gmail/v1/users/me/messages\
                 ?maxResults=50&labelIds=INBOX"
                .to_string();
            if let Some(ref token) = page_token {
                url.push_str(&format!("&pageToken={}", token));
            }

            tracing::debug!(url = %url, "gmail: listing messages");
            let json = self.get_json(&url).await?;

            let messages = json
                .get("messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // For each message, fetch minimal metadata
            for msg in &messages {
                let msg_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                if msg_id.is_empty() {
                    continue;
                }

                let meta_url = format!(
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}\
                     ?format=metadata&metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=Date",
                    msg_id
                );

                match self.get_json(&meta_url).await {
                    Ok(meta_json) => {
                        let headers = extract_gmail_headers(&meta_json);
                        let subject = headers.get("Subject").cloned().unwrap_or_default();
                        let _from = headers.get("From").cloned().unwrap_or_default();
                        let date = headers.get("Date").cloned().unwrap_or_default();

                        let date_prefix = date_to_slug_prefix(&date);
                        let slug = format!("{}-{}", date_prefix, sanitize_slug(&subject));

                        all_resources.push(ResourceMeta {
                            id: msg_id.to_string(),
                            slug,
                            title: Some(subject),
                            updated_at: Some(date),
                            content_type: Some("message/rfc822".to_string()),
                            group: None,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(msg_id = %msg_id, error = %e, "failed to fetch message metadata");
                    }
                }
            }

            // Pagination
            match json.get("nextPageToken").and_then(|v| v.as_str()) {
                Some(token) if all_resources.len() < 100 => {
                    page_token = Some(token.to_string());
                }
                _ => break,
            }
        }

        Ok(all_resources)
    }

    async fn gmail_read(&self, id: &str) -> Result<Resource> {
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}?format=full",
            id
        );
        let json = self.get_json(&url).await?;

        let headers = extract_gmail_headers(&json);
        let subject = headers.get("Subject").cloned().unwrap_or_default();
        let from = headers.get("From").cloned().unwrap_or_default();
        let to = headers.get("To").cloned().unwrap_or_default();
        let date = headers.get("Date").cloned().unwrap_or_default();

        let labels: Vec<String> = json
            .get("labelIds")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let date_prefix = date_to_slug_prefix(&date);
        let slug = format!("{}-{}", date_prefix, sanitize_slug(&subject));

        let meta = ResourceMeta {
            id: id.to_string(),
            slug,
            title: Some(subject.clone()),
            updated_at: Some(date.clone()),
            content_type: Some("message/rfc822".to_string()),
            group: None,
        };

        // Extract body
        let body = extract_gmail_body(&json);

        // Render as markdown
        let labels_str = labels
            .iter()
            .map(|l| format!("\"{}\"", l))
            .collect::<Vec<_>>()
            .join(", ");

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", id));
        out.push_str(&format!("from: \"{}\"\n", escape_yaml(&from)));
        out.push_str(&format!("to: \"{}\"\n", escape_yaml(&to)));
        out.push_str(&format!("subject: \"{}\"\n", escape_yaml(&subject)));
        out.push_str(&format!("date: \"{}\"\n", escape_yaml(&date)));
        out.push_str(&format!("labels: [{}]\n", labels_str));
        out.push_str("operations: [read, draft]\n");
        out.push_str("---\n\n");
        out.push_str(&body);
        out.push('\n');

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    async fn gmail_write(&self, _id: &str, content: &[u8]) -> Result<()> {
        // Create a draft from the content
        let text = std::str::from_utf8(content).context("content is not valid UTF-8")?;
        let body_text = strip_frontmatter_str(text);

        // Base64url-encode a minimal RFC 2822 message
        let raw_message = format!(
            "Content-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{}",
            body_text
        );
        let encoded = base64_url_encode(raw_message.as_bytes());

        let draft_body = serde_json::json!({
            "message": {
                "raw": encoded
            }
        });

        let token = self.token_provider.get_token().await?;
        let resp = self
            .client
            .post("https://gmail.googleapis.com/gmail/v1/users/me/drafts")
            .bearer_auth(&token)
            .json(&draft_body)
            .send()
            .await
            .context("gmail draft creation failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("gmail write failed: HTTP {} - {}", status, body));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Calendar
    // -----------------------------------------------------------------------

    async fn calendar_list(&self) -> Result<Vec<ResourceMeta>> {
        let now = chrono::Utc::now();
        let time_min = now.to_rfc3339();
        let time_max = (now + chrono::Duration::days(7)).to_rfc3339();

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events\
             ?timeMin={}&timeMax={}&singleEvents=true&orderBy=startTime&maxResults=100",
            time_min, time_max
        );

        tracing::debug!(url = %url, "calendar: listing events");
        let json = self.get_json(&url).await?;

        let items = json
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut resources = Vec::new();
        for item in &items {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let summary = item
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
            let start = extract_calendar_time(item, "start");
            let updated = item.get("updated").and_then(|v| v.as_str());

            let date_prefix = calendar_time_to_slug(&start);
            let slug = format!("{}-{}", date_prefix, sanitize_slug(summary));

            resources.push(ResourceMeta {
                id: id.to_string(),
                slug,
                title: Some(summary.to_string()),
                updated_at: updated.map(|s| s.to_string()),
                content_type: Some("text/calendar".to_string()),
                group: None,
            });
        }

        Ok(resources)
    }

    async fn calendar_read(&self, id: &str) -> Result<Resource> {
        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}",
            id
        );
        let json = self.get_json(&url).await?;

        let summary = json
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(no title)");
        let start = extract_calendar_time(&json, "start");
        let end = extract_calendar_time(&json, "end");
        let location = json.get("location").and_then(|v| v.as_str()).unwrap_or("");
        let description = json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let updated = json.get("updated").and_then(|v| v.as_str());

        let date_prefix = calendar_time_to_slug(&start);
        let slug = format!("{}-{}", date_prefix, sanitize_slug(summary));

        let meta = ResourceMeta {
            id: id.to_string(),
            slug,
            title: Some(summary.to_string()),
            updated_at: updated.map(|s| s.to_string()),
            content_type: Some("text/calendar".to_string()),
            group: None,
        };

        // Extract attendees
        let attendees = json
            .get("attendees")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Render markdown
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", id));
        out.push_str(&format!("summary: \"{}\"\n", escape_yaml(summary)));
        out.push_str(&format!("start: \"{}\"\n", start));
        out.push_str(&format!("end: \"{}\"\n", end));
        if !location.is_empty() {
            out.push_str(&format!("location: \"{}\"\n", escape_yaml(location)));
        }
        out.push_str("operations: [read, write]\n");
        out.push_str("---\n\n");

        if !attendees.is_empty() {
            out.push_str("## Attendees\n");
            for attendee in &attendees {
                let email = attendee
                    .get("email")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let status = attendee
                    .get("responseStatus")
                    .and_then(|v| v.as_str())
                    .unwrap_or("needsAction");
                out.push_str(&format!("- {} ({})\n", email, status));
            }
            out.push('\n');
        }

        if !description.is_empty() {
            out.push_str("## Description\n");
            out.push_str(description);
            out.push('\n');
        }

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    async fn calendar_write(&self, id: &str, content: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(content).context("content is not valid UTF-8")?;

        // Try to parse frontmatter for structured fields
        let patch_body = if let Some(frontmatter) = extract_frontmatter(text) {
            // Use frontmatter fields to build a patch
            let mut body = serde_json::Map::new();
            if let Some(summary) = frontmatter.get("summary").and_then(|v| v.as_str()) {
                body.insert("summary".to_string(), Value::String(summary.to_string()));
            }
            if let Some(location) = frontmatter.get("location").and_then(|v| v.as_str()) {
                body.insert("location".to_string(), Value::String(location.to_string()));
            }
            // Extract description from body text (after frontmatter)
            let body_text = strip_frontmatter_str(text);
            if !body_text.trim().is_empty() {
                body.insert(
                    "description".to_string(),
                    Value::String(body_text.trim().to_string()),
                );
            }
            Value::Object(body)
        } else {
            serde_json::json!({
                "description": text.trim()
            })
        };

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}",
            id
        );

        let request = self.auth_patch(&url).await?.json(&patch_body);
        let resp = request
            .send()
            .await
            .context("calendar write request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("calendar write failed: HTTP {} - {}", status, body));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Connector trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Connector for GoogleWorkspaceConnector {
    fn name(&self) -> &str {
        "google"
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Ok(vec![
            CollectionInfo {
                name: "drive".to_string(),
                description: Some("Google Drive files and folders".to_string()),
            },
            CollectionInfo {
                name: "gmail".to_string(),
                description: Some("Gmail messages (inbox)".to_string()),
            },
            CollectionInfo {
                name: "calendar".to_string(),
                description: Some("Google Calendar events (next 7 days)".to_string()),
            },
        ])
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        let (service, folder_id) = Self::parse_collection(collection);
        let resources = match service {
            "drive" => self.drive_list(folder_id).await?,
            "gmail" => self.gmail_list().await?,
            "calendar" => self.calendar_list().await?,
            _ => return Err(anyhow!("unknown collection: '{}'", collection)),
        };

        // Populate slug → ID cache so read_resource can resolve slugs
        for r in &resources {
            self.cache_slug(collection, &r.slug, &r.id);
        }

        Ok(resources)
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        let resolved = self.resolve_id(collection, id);
        let (service, _) = Self::parse_collection(collection);
        match service {
            "drive" => self.drive_read(&resolved).await,
            "gmail" => self.gmail_read(&resolved).await,
            "calendar" => self.calendar_read(&resolved).await,
            _ => Err(anyhow!("unknown collection: '{}'", collection)),
        }
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let resolved = self.resolve_id(collection, id);
        let (service, _) = Self::parse_collection(collection);
        match service {
            "drive" => self.drive_write(&resolved, content).await,
            "gmail" => self.gmail_write(&resolved, content).await,
            "calendar" => self.calendar_write(&resolved, content).await,
            _ => Err(anyhow!("unknown collection: '{}'", collection)),
        }
    }

    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>> {
        let (service, _) = Self::parse_collection(collection);
        match service {
            "drive" => {
                // Google Drive supports revisions
                let url = format!(
                    "https://www.googleapis.com/drive/v3/files/{}/revisions\
                     ?fields=revisions(id,modifiedTime,size)",
                    id
                );
                let json = self.get_json(&url).await?;
                let revisions = json
                    .get("revisions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                let versions: Vec<VersionInfo> = revisions
                    .iter()
                    .enumerate()
                    .map(|(i, rev)| {
                        let created_at = rev
                            .get("modifiedTime")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let size = rev
                            .get("size")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0);
                        VersionInfo {
                            version: (i + 1) as u32,
                            created_at,
                            size,
                        }
                    })
                    .collect();

                Ok(versions)
            }
            _ => Ok(vec![]),
        }
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        if version == 0 {
            return self.read_resource(collection, id).await;
        }

        let (service, _) = Self::parse_collection(collection);
        match service {
            "drive" => {
                // List revisions to find the right one
                let url = format!(
                    "https://www.googleapis.com/drive/v3/files/{}/revisions\
                     ?fields=revisions(id,modifiedTime)",
                    id
                );
                let json = self.get_json(&url).await?;
                let revisions = json
                    .get("revisions")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("no revisions found for file {}", id))?;

                let idx = (version as usize)
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("version must be >= 1"))?;
                let rev = revisions.get(idx).ok_or_else(|| {
                    anyhow!("version {} not found (have {})", version, revisions.len())
                })?;
                let rev_id = rev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("revision has no id"))?;

                // Download revision content
                let download_url = format!(
                    "https://www.googleapis.com/drive/v3/files/{}/revisions/{}?alt=media",
                    id, rev_id
                );
                let content_bytes = self.get_bytes(&download_url).await?;

                // Get current metadata
                let meta_url = format!(
                    "https://www.googleapis.com/drive/v3/files/{}?fields=id,name,mimeType",
                    id
                );
                let meta_json = self.get_json(&meta_url).await?;
                let name = meta_json
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("untitled");
                let mime = meta_json
                    .get("mimeType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let modified = rev
                    .get("modifiedTime")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let slug = sanitize_slug(name);

                let meta = ResourceMeta {
                    id: id.to_string(),
                    slug,
                    title: Some(name.to_string()),
                    updated_at: Some(modified.to_string()),
                    content_type: Some(mime.to_string()),
                    group: None,
                };

                let mut out = String::new();
                out.push_str("---\n");
                out.push_str(&format!("id: \"{}\"\n", id));
                out.push_str(&format!("name: \"{}\"\n", escape_yaml(name)));
                out.push_str(&format!("mimeType: \"{}\"\n", mime));
                out.push_str(&format!("modifiedTime: \"{}\"\n", modified));
                out.push_str(&format!("version: {}\n", version));
                out.push_str("---\n\n");
                out.push_str(&String::from_utf8_lossy(&content_bytes));
                out.push('\n');

                Ok(Resource {
                    meta,
                    content: out.into_bytes(),
                    raw_json: None,
                })
            }
            _ => Err(anyhow!(
                "versioned reads are not supported for collection '{}'",
                collection
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Sanitize a string for use as a filesystem slug.
fn sanitize_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_ascii_control() => '-',
            c => c,
        })
        .collect();

    // Collapse multiple dashes
    let mut result = String::with_capacity(slug.len());
    let mut last_was_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !last_was_dash {
                result.push(c);
            }
            last_was_dash = true;
        } else {
            result.push(c);
            last_was_dash = false;
        }
    }

    // Truncate to 200 chars
    let truncated: String = result.chars().take(200).collect();
    let trimmed = truncated.trim_matches('-').to_string();

    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

/// Escape a string for safe YAML inclusion in double quotes.
fn escape_yaml(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Extract email headers from a Gmail message JSON.
fn extract_gmail_headers(json: &Value) -> std::collections::HashMap<String, String> {
    let mut headers = std::collections::HashMap::new();

    let payload_headers = json
        .get("payload")
        .and_then(|p| p.get("headers"))
        .and_then(|h| h.as_array());

    if let Some(hdrs) = payload_headers {
        for hdr in hdrs {
            if let (Some(name), Some(value)) = (
                hdr.get("name").and_then(|v| v.as_str()),
                hdr.get("value").and_then(|v| v.as_str()),
            ) {
                headers.insert(name.to_string(), value.to_string());
            }
        }
    }

    headers
}

/// Extract the plain-text body from a Gmail message.
fn extract_gmail_body(json: &Value) -> String {
    let payload = match json.get("payload") {
        Some(p) => p,
        None => return String::new(),
    };

    // Try to find text/plain part
    if let Some(body) = find_body_part(payload, "text/plain") {
        return body;
    }

    // Fall back to text/html (strip tags naively)
    if let Some(body) = find_body_part(payload, "text/html") {
        return strip_html_tags(&body);
    }

    // Last resort: snippet
    json.get("snippet")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Recursively find a body part matching the given mime type.
fn find_body_part(part: &Value, target_mime: &str) -> Option<String> {
    let mime = part.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");

    if mime == target_mime {
        if let Some(data) = part
            .get("body")
            .and_then(|b| b.get("data"))
            .and_then(|d| d.as_str())
        {
            return base64_url_decode(data).ok();
        }
    }

    // Check sub-parts (multipart messages)
    if let Some(parts) = part.get("parts").and_then(|p| p.as_array()) {
        for sub in parts {
            if let Some(body) = find_body_part(sub, target_mime) {
                return Some(body);
            }
        }
    }

    None
}

/// Very basic HTML tag stripping.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
}

/// Extract a time string from a Calendar event's "start" or "end" field.
fn extract_calendar_time(event: &Value, field: &str) -> String {
    event
        .get(field)
        .and_then(|t| {
            t.get("dateTime")
                .or_else(|| t.get("date"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// Convert a calendar time to a slug prefix like "2026-03-20-14-00".
fn calendar_time_to_slug(time_str: &str) -> String {
    // Try to parse ISO 8601
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(time_str) {
        return dt.format("%Y-%m-%d-%H-%M").to_string();
    }
    // For all-day events (just a date)
    if time_str.len() >= 10 {
        return time_str[..10].to_string();
    }
    "unknown-date".to_string()
}

/// Try to extract a date prefix from an email date header for slug generation.
fn date_to_slug_prefix(date_str: &str) -> String {
    // Try RFC 2822 parsing first
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(date_str) {
        return dt.format("%Y-%m-%d").to_string();
    }
    // Try RFC 3339
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_str) {
        return dt.format("%Y-%m-%d").to_string();
    }
    // Fallback: try to extract YYYY-MM-DD from the start
    if date_str.len() >= 10 {
        let prefix = &date_str[..10];
        if prefix.chars().filter(|c| *c == '-').count() == 2 {
            return prefix.to_string();
        }
    }
    "unknown-date".to_string()
}

/// Base64url decode (used for Gmail message bodies).
fn base64_url_decode(input: &str) -> Result<String> {
    // Gmail uses base64url encoding (RFC 4648 section 5) without padding
    let standardized = input.replace('-', "+").replace('_', "/");

    // Add padding if needed
    let padded = match standardized.len() % 4 {
        2 => format!("{}==", standardized),
        3 => format!("{}=", standardized),
        _ => standardized,
    };

    let bytes = base64_decode_bytes(&padded)?;
    String::from_utf8(bytes).context("decoded base64 is not valid UTF-8")
}

/// Decode base64 bytes (manual implementation to avoid adding a dependency).
fn base64_decode_bytes(input: &str) -> Result<Vec<u8>> {
    const fn build_decode_table() -> [u8; 128] {
        let mut table = [255u8; 128];
        let mut i = 0u8;
        while i < 26 {
            table[(b'A' + i) as usize] = i;
            table[(b'a' + i) as usize] = i + 26;
            i += 1;
        }
        let mut i = 0u8;
        while i < 10 {
            table[(b'0' + i) as usize] = i + 52;
            i += 1;
        }
        table[b'+' as usize] = 62;
        table[b'/' as usize] = 63;
        table[b'=' as usize] = 0; // padding
        table
    }
    static DECODE_TABLE: [u8; 128] = build_decode_table();

    let input_bytes: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'\n' && b != b'\r' && b != b' ')
        .collect();
    let mut output = Vec::with_capacity(input_bytes.len() * 3 / 4);

    for chunk in input_bytes.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let b0 = DECODE_TABLE.get(chunk[0] as usize).copied().unwrap_or(255);
        let b1 = DECODE_TABLE.get(chunk[1] as usize).copied().unwrap_or(255);
        if b0 == 255 || b1 == 255 {
            return Err(anyhow!("invalid base64 character"));
        }
        output.push((b0 << 2) | (b1 >> 4));

        if chunk.len() > 2 && chunk[2] != b'=' {
            let b2 = DECODE_TABLE.get(chunk[2] as usize).copied().unwrap_or(255);
            if b2 == 255 {
                return Err(anyhow!("invalid base64 character"));
            }
            output.push((b1 << 4) | (b2 >> 2));

            if chunk.len() > 3 && chunk[3] != b'=' {
                let b3 = DECODE_TABLE.get(chunk[3] as usize).copied().unwrap_or(255);
                if b3 == 255 {
                    return Err(anyhow!("invalid base64 character"));
                }
                output.push((b2 << 6) | b3);
            }
        }
    }

    Ok(output)
}

/// Base64url encode (used for Gmail draft creation).
fn base64_url_encode(input: &[u8]) -> String {
    static ENCODE_TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(ENCODE_TABLE[((triple >> 18) & 0x3F) as usize] as char);
        output.push(ENCODE_TABLE[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            output.push(ENCODE_TABLE[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(ENCODE_TABLE[(triple & 0x3F) as usize] as char);
        }
    }

    // Convert to URL-safe base64 (no padding)
    output.replace('+', "-").replace('/', "_")
}

/// Strip YAML frontmatter from content bytes, returning the body.
fn strip_frontmatter(content: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(content);
    strip_frontmatter_str(&text).as_bytes().to_vec()
}

/// Strip YAML frontmatter from a string, returning the body text.
fn strip_frontmatter_str(text: &str) -> &str {
    if !text.starts_with("---") {
        return text;
    }

    if let Some(end) = text[3..].find("\n---") {
        let body_start = end + 3 + 4;
        let remaining = &text[body_start..];
        return remaining.trim_start_matches('\n');
    }

    text
}

/// Try to extract YAML frontmatter as a serde_json::Value.
fn extract_frontmatter(text: &str) -> Option<Value> {
    if !text.starts_with("---") {
        return None;
    }

    let rest = &text[3..];
    let end = rest.find("\n---")?;
    let yaml_str = &rest[..end];

    serde_yaml::from_str(yaml_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_slug() {
        assert_eq!(sanitize_slug("hello world.txt"), "hello world.txt");
        assert_eq!(sanitize_slug("path/to/file"), "path-to-file");
        assert_eq!(sanitize_slug("a::b**c"), "a-b-c");
        assert_eq!(sanitize_slug(""), "untitled");
        assert_eq!(sanitize_slug("---"), "untitled");
    }

    #[test]
    fn test_escape_yaml() {
        assert_eq!(escape_yaml(r#"hello "world""#), r#"hello \"world\""#);
        assert_eq!(escape_yaml(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn test_base64_url_roundtrip() {
        let original = b"Hello, World! This is a test message.";
        let encoded = base64_url_encode(original);
        let decoded = base64_url_decode(&encoded).unwrap();
        assert_eq!(decoded.as_bytes(), original);
    }

    #[test]
    fn test_strip_frontmatter() {
        let input = "---\nid: \"123\"\ntitle: \"Test\"\n---\n\nBody content here";
        assert_eq!(strip_frontmatter_str(input), "Body content here");
    }

    #[test]
    fn test_strip_frontmatter_no_frontmatter() {
        let input = "Just plain text";
        assert_eq!(strip_frontmatter_str(input), "Just plain text");
    }

    #[test]
    fn test_date_to_slug_prefix() {
        assert_eq!(date_to_slug_prefix("2026-03-20T10:00:00Z"), "2026-03-20");
    }

    #[test]
    fn test_calendar_time_to_slug() {
        assert_eq!(
            calendar_time_to_slug("2026-03-20T14:00:00-07:00"),
            "2026-03-20-14-00"
        );
        assert_eq!(calendar_time_to_slug("2026-03-20"), "2026-03-20");
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("<p>Hello <b>world</b></p>"), "Hello world");
    }

    #[test]
    fn test_parse_collection() {
        assert_eq!(
            GoogleWorkspaceConnector::parse_collection("drive"),
            ("drive", None)
        );
        assert_eq!(
            GoogleWorkspaceConnector::parse_collection("drive/abc123"),
            ("drive", Some("abc123"))
        );
        assert_eq!(
            GoogleWorkspaceConnector::parse_collection("gmail"),
            ("gmail", None)
        );
    }
}

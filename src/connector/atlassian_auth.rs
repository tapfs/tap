//! Shared Atlassian Cloud authentication helper.
//!
//! Both Jira and Confluence use the same auth mechanism:
//!   Basic auth with `email:api_token` base64-encoded.
//!
//! Env vars:
//!   - `ATLASSIAN_DOMAIN`    (e.g., `your-company` -> `https://your-company.atlassian.net`)
//!   - `ATLASSIAN_EMAIL`     (the account email)
//!   - `ATLASSIAN_API_TOKEN` (the API token)

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::Value;

use crate::credentials::{ConnectorCredentials, CredentialStore};

/// Configuration parsed from env vars, shared by Jira and Confluence connectors.
pub struct AtlassianAuth {
    pub client: Client,
    pub base_url: String,
    pub auth_header: String,
}

impl AtlassianAuth {
    /// Build an `AtlassianAuth` from environment variables.
    pub fn from_env() -> Result<Self> {
        let domain =
            std::env::var("ATLASSIAN_DOMAIN").context("ATLASSIAN_DOMAIN env var required")?;
        let email = std::env::var("ATLASSIAN_EMAIL").context("ATLASSIAN_EMAIL env var required")?;
        let token =
            std::env::var("ATLASSIAN_API_TOKEN").context("ATLASSIAN_API_TOKEN env var required")?;

        let base_url = format!("https://{}.atlassian.net", domain);
        Self::build(&base_url, &email, &token)
    }

    /// Build an `AtlassianAuth`, preferring environment variables, then the
    /// credentials store (keyed by `connector_name`, e.g. "jira" or
    /// "confluence"). Returns an error when no source has all three of
    /// (base_url, email, token).
    pub fn load(connector_name: &str, store: &CredentialStore) -> Result<Self> {
        Self::load_with_overrides(connector_name, store, None, None)
    }

    /// Like `load`, but lets a caller substitute `base_url` and/or `token`
    /// after the credentials store has resolved its defaults. Used by the
    /// factory when `service.yaml` declares per-connector overrides for
    /// native Atlassian connectors. Precedence: service.yaml overrides >
    /// ATLASSIAN_* env vars > credentials store. The env-var fast path is
    /// only consulted when neither override is supplied.
    pub fn load_with_overrides(
        connector_name: &str,
        store: &CredentialStore,
        base_url_override: Option<&str>,
        token_override: Option<&str>,
    ) -> Result<Self> {
        if env_shortcircuit_allowed(base_url_override, token_override) {
            if let Ok(auth) = Self::from_env() {
                return Ok(auth);
            }
        }

        let creds = store.get(connector_name).ok_or_else(|| {
            anyhow!(
                "no Atlassian credentials for '{}': set ATLASSIAN_DOMAIN/EMAIL/API_TOKEN \
                 or run `tap mount {}` from a terminal to set them interactively",
                connector_name,
                connector_name
            )
        })?;

        Self::from_credentials_with_overrides(
            connector_name,
            creds,
            base_url_override,
            token_override,
        )
    }

    #[cfg(test)]
    fn from_credentials(connector_name: &str, creds: &ConnectorCredentials) -> Result<Self> {
        Self::from_credentials_with_overrides(connector_name, creds, None, None)
    }

    fn from_credentials_with_overrides(
        connector_name: &str,
        creds: &ConnectorCredentials,
        base_url_override: Option<&str>,
        token_override: Option<&str>,
    ) -> Result<Self> {
        let base_url = base_url_override
            .map(str::to_string)
            .or_else(|| creds.base_url.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no base_url stored for '{}' — re-run `tap mount {}` from a terminal to enter \
                     your Atlassian domain",
                    connector_name,
                    connector_name
                )
            })?;
        let email = creds.email.as_deref().ok_or_else(|| {
            anyhow!(
                "no email stored for '{}' — re-run `tap mount {}` from a terminal to set it",
                connector_name,
                connector_name
            )
        })?;
        let token = token_override
            .map(str::to_string)
            .or_else(|| creds.token.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no token available for '{}' — the OS keychain may be locked or unavailable. \
                     Re-run `tap mount {}` from a terminal to re-authenticate, or set \
                     TAPFS_NO_KEYCHAIN=1 to read the token from credentials.yaml",
                    connector_name,
                    connector_name
                )
            })?;

        Self::build(&base_url, email, &token)
    }

    fn build(base_url: &str, email: &str, token: &str) -> Result<Self> {
        let auth = base64_encode(format!("{}:{}", email, token).as_bytes());
        let auth_header = format!("Basic {}", auth);

        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
            auth_header,
        })
    }

    /// Returns true if either env vars or the credentials store has a complete
    /// set of Atlassian credentials for `connector_name`. Used by the connector
    /// factory to decide between proceeding and surfacing AuthRequired.
    pub fn credentials_present(connector_name: &str, store: &CredentialStore) -> bool {
        Self::credentials_present_with_overrides(connector_name, store, None, None)
    }

    /// Like `credentials_present`, but considers `base_url` and `token`
    /// overrides supplied by a `service.yaml` entry. Without this an
    /// `auth_token_env: JIRA_TOKEN` override is useless on a CI runner whose
    /// keychain has no token — the precheck would reject the mount before
    /// the override ever got a chance to apply.
    pub fn credentials_present_with_overrides(
        connector_name: &str,
        store: &CredentialStore,
        base_url_override: Option<&str>,
        token_override: Option<&str>,
    ) -> bool {
        // Mirrors load_with_overrides precedence so the precheck can't
        // approve a path load would reject. Env wins only when no override
        // is in play; otherwise we must verify the creds-plus-overrides
        // route is viable.
        if env_shortcircuit_allowed(base_url_override, token_override) {
            let env_ok = std::env::var("ATLASSIAN_DOMAIN").is_ok()
                && std::env::var("ATLASSIAN_EMAIL").is_ok()
                && std::env::var("ATLASSIAN_API_TOKEN").is_ok();
            if env_ok {
                return true;
            }
        }
        creds_complete_with_overrides(store.get(connector_name), base_url_override, token_override)
    }

    /// Persist Atlassian credentials. Domain may be passed as just the
    /// subdomain (e.g. "mycompany") or as a full URL; both are normalized
    /// to "https://mycompany.atlassian.net".
    ///
    /// The token goes to the OS keychain (or YAML fallback when
    /// `TAPFS_NO_KEYCHAIN=1` is set), `email` and `base_url` always go into
    /// the YAML index. Both YAML writes go through `write_yaml_index`, which
    /// is atomic (tempfile + rename) and propagates parse errors instead of
    /// silently overwriting a corrupt index.
    pub fn save_credentials(
        data_dir: &Path,
        connector_name: &str,
        domain: &str,
        email: &str,
        token: &str,
    ) -> Result<()> {
        let base_url = normalize_atlassian_domain(domain);
        CredentialStore::save_token(data_dir, connector_name, token)?;

        let mut entries = crate::credentials::read_yaml_index(data_dir)?;
        let entry = entries.entry(connector_name.to_string()).or_default();
        entry.email = Some(email.to_string());
        entry.base_url = Some(base_url);
        crate::credentials::write_yaml_index(data_dir, &entries)?;
        Ok(())
    }

    /// Send a GET request with retry on 401, 429, 503.
    pub async fn get_json(&self, url: &str) -> Result<Value> {
        let resp = self.send_with_retry(url).await?;
        resp.json().await.context("parsing JSON response")
    }

    /// Send a GET request with exponential backoff retries.
    pub async fn send_with_retry(&self, url: &str) -> Result<reqwest::Response> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            let resp = Self::send_request_with_network_retry(
                || {
                    self.client
                        .get(url)
                        .header("Authorization", &self.auth_header)
                        .header("Accept", "application/json")
                        .send()
                },
                url,
                "GET",
            )
            .await?;

            match resp.status() {
                s if s == reqwest::StatusCode::UNAUTHORIZED => {
                    last_err = Some(anyhow!("GET {} unauthorized (401)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
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

    /// Retry a `.send()` call up to 2 times on network / connection errors,
    /// with a 1-second delay between attempts.
    async fn send_request_with_network_retry<F, Fut>(
        mut make_request: F,
        url: &str,
        method: &str,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<reqwest::Response, reqwest::Error>>,
    {
        let network_retries = 2u32;
        let mut last_network_err = None;

        for net_attempt in 0..=network_retries {
            if net_attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            match make_request().await {
                Ok(resp) => return Ok(resp),
                Err(e) if e.is_connect() || e.is_timeout() => {
                    last_network_err = Some(e);
                    continue;
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e).context(format!("{} {}", method, url)));
                }
            }
        }

        Err(
            anyhow::Error::new(last_network_err.unwrap()).context(format!(
                "{} {} failed after {} network retries",
                method, url, network_retries
            )),
        )
    }

    /// Send a PUT request with JSON body, with retry on 401, 429, 503.
    pub async fn put_json(&self, url: &str, body: &Value) -> Result<Value> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            let body_clone = body.clone();
            let resp = Self::send_request_with_network_retry(
                || {
                    self.client
                        .put(url)
                        .header("Authorization", &self.auth_header)
                        .header("Content-Type", "application/json")
                        .header("Accept", "application/json")
                        .json(&body_clone)
                        .send()
                },
                url,
                "PUT",
            )
            .await?;

            match resp.status() {
                s if s == reqwest::StatusCode::UNAUTHORIZED => {
                    last_err = Some(anyhow!("PUT {} unauthorized (401)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(retry_after) = resp.headers().get("retry-after") {
                        if let Ok(secs) = retry_after.to_str().unwrap_or("5").parse::<u64>() {
                            tokio::time::sleep(Duration::from_secs(secs)).await;
                        }
                    }
                    last_err = Some(anyhow!("PUT {} rate limited (429)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    last_err = Some(anyhow!("PUT {} service unavailable (503)", url));
                    continue;
                }
                s if s.is_success() => {
                    let json = resp.json().await.unwrap_or(Value::Null);
                    return Ok(json);
                }
                s => {
                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("PUT {} failed: HTTP {} - {}", url, s, body_text));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("PUT {} failed after {} retries", url, max_retries)))
    }

    /// Send a POST request with JSON body, with retry on 401, 429, 503.
    pub async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            let body_clone = body.clone();
            let resp = Self::send_request_with_network_retry(
                || {
                    self.client
                        .post(url)
                        .header("Authorization", &self.auth_header)
                        .header("Content-Type", "application/json")
                        .header("Accept", "application/json")
                        .json(&body_clone)
                        .send()
                },
                url,
                "POST",
            )
            .await?;

            match resp.status() {
                s if s == reqwest::StatusCode::UNAUTHORIZED => {
                    last_err = Some(anyhow!("POST {} unauthorized (401)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(retry_after) = resp.headers().get("retry-after") {
                        if let Ok(secs) = retry_after.to_str().unwrap_or("5").parse::<u64>() {
                            tokio::time::sleep(Duration::from_secs(secs)).await;
                        }
                    }
                    last_err = Some(anyhow!("POST {} rate limited (429)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    last_err = Some(anyhow!("POST {} service unavailable (503)", url));
                    continue;
                }
                s if s.is_success() => {
                    return resp.json().await.context("parsing JSON response");
                }
                s => {
                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("POST {} failed: HTTP {} - {}", url, s, body_text));
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| anyhow!("POST {} failed after {} retries", url, max_retries)))
    }
}

/// Single source of truth for "is the ATLASSIAN_* env shortcut allowed
/// right now?" — used by both `load_with_overrides` (to choose between
/// from_env and creds) and `credentials_present_with_overrides` (so the
/// precheck mirrors the load decision). Any service.yaml override means
/// the env path is skipped.
fn env_shortcircuit_allowed(base_url_override: Option<&str>, token_override: Option<&str>) -> bool {
    base_url_override.is_none() && token_override.is_none()
}

/// Predicate behind `AtlassianAuth::credentials_present_with_overrides`,
/// extracted so the override interaction can be unit-tested without spinning
/// up a `CredentialStore`. Email is the one field overrides never supply —
/// it always has to come from `creds`.
fn creds_complete_with_overrides(
    creds: Option<&ConnectorCredentials>,
    base_url_override: Option<&str>,
    token_override: Option<&str>,
) -> bool {
    let email_ok = creds.map(|c| c.email.is_some()).unwrap_or(false);
    let base_url_ok =
        base_url_override.is_some() || creds.map(|c| c.base_url.is_some()).unwrap_or(false);
    let token_ok = token_override.is_some() || creds.map(|c| c.token.is_some()).unwrap_or(false);
    email_ok && base_url_ok && token_ok
}

/// Normalize an Atlassian domain input (e.g. "mycompany",
/// "https://mycompany.atlassian.net", "mycompany.atlassian.net") to a
/// canonical "https://mycompany.atlassian.net" base URL.
fn normalize_atlassian_domain(input: &str) -> String {
    let trimmed = input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if trimmed.contains('.') {
        format!("https://{}", trimmed)
    } else {
        format!("https://{}.atlassian.net", trimmed)
    }
}

// ---------------------------------------------------------------------------
// Base64 encoding (inline implementation, matching the Google connector style)
// ---------------------------------------------------------------------------

/// Standard base64 encode (not URL-safe).
pub fn base64_encode(input: &[u8]) -> String {
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
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ENCODE_TABLE[(triple & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
    }

    output
}

// ---------------------------------------------------------------------------
// Shared helper functions
// ---------------------------------------------------------------------------

/// Sanitize a string for use as a filesystem slug.
pub fn sanitize_slug(name: &str) -> String {
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
pub fn escape_yaml(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Strip YAML frontmatter from a string, returning the body text.
pub fn strip_frontmatter_str(text: &str) -> &str {
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
pub fn extract_frontmatter(text: &str) -> Option<Value> {
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
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello:world"), "aGVsbG86d29ybGQ=");
        assert_eq!(
            base64_encode(b"user@example.com:token123"),
            "dXNlckBleGFtcGxlLmNvbTp0b2tlbjEyMw=="
        );
    }

    #[test]
    fn test_sanitize_slug() {
        assert_eq!(sanitize_slug("hello world"), "hello world");
        assert_eq!(sanitize_slug("path/to/file"), "path-to-file");
        assert_eq!(sanitize_slug("a::b**c"), "a-b-c");
        assert_eq!(sanitize_slug(""), "untitled");
        assert_eq!(sanitize_slug("---"), "untitled");
    }

    #[test]
    fn test_escape_yaml() {
        assert_eq!(escape_yaml(r#"hello "world""#), r#"hello \"world\""#);
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
    fn normalize_subdomain_only() {
        assert_eq!(
            normalize_atlassian_domain("mycompany"),
            "https://mycompany.atlassian.net"
        );
    }

    #[test]
    fn normalize_full_host() {
        assert_eq!(
            normalize_atlassian_domain("mycompany.atlassian.net"),
            "https://mycompany.atlassian.net"
        );
    }

    #[test]
    fn normalize_full_url_with_trailing_slash() {
        assert_eq!(
            normalize_atlassian_domain("https://mycompany.atlassian.net/"),
            "https://mycompany.atlassian.net"
        );
    }

    #[test]
    fn from_credentials_builds_basic_auth_header() {
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: Some("dev@acme.com".to_string()),
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        let auth = AtlassianAuth::from_credentials("jira", &creds).unwrap();
        assert_eq!(auth.base_url, "https://acme.atlassian.net");
        // Basic dev@acme.com:api-tok
        assert_eq!(
            auth.auth_header,
            format!("Basic {}", base64_encode(b"dev@acme.com:api-tok"))
        );
    }

    #[test]
    fn from_credentials_errors_on_missing_field() {
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: None, // missing
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        let err_msg = AtlassianAuth::from_credentials("jira", &creds)
            .err()
            .expect("expected error")
            .to_string();
        assert!(
            err_msg.contains("no email stored"),
            "unexpected error: {}",
            err_msg
        );
    }

    /// Save → set → run → restore for ATLASSIAN_* env vars. Process-global
    /// state racing with parallel tests is the price; the window is small
    /// because nothing else touches these vars during the test run.
    fn with_atlassian_env<F: FnOnce()>(
        domain: Option<&str>,
        email: Option<&str>,
        token: Option<&str>,
        body: F,
    ) {
        let saved_d = std::env::var("ATLASSIAN_DOMAIN").ok();
        let saved_e = std::env::var("ATLASSIAN_EMAIL").ok();
        let saved_t = std::env::var("ATLASSIAN_API_TOKEN").ok();
        let set = |name: &str, val: Option<&str>| match val {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        };
        set("ATLASSIAN_DOMAIN", domain);
        set("ATLASSIAN_EMAIL", email);
        set("ATLASSIAN_API_TOKEN", token);
        body();
        // Restore even if body panics is overkill for this codebase's test
        // style; tests don't panic in steady state.
        set("ATLASSIAN_DOMAIN", saved_d.as_deref());
        set("ATLASSIAN_EMAIL", saved_e.as_deref());
        set("ATLASSIAN_API_TOKEN", saved_t.as_deref());
    }

    #[test]
    fn precheck_does_not_short_circuit_env_when_override_present() {
        // With complete ATLASSIAN_* env AND a service.yaml base_url override,
        // load_with_overrides skips the env path and goes to creds. The
        // precheck must do the same — otherwise it admits a mount path that
        // load will later reject for missing creds.
        let store = CredentialStore::default();
        with_atlassian_env(Some("acme"), Some("dev@acme.com"), Some("env-tok"), || {
            // Without overrides: env satisfies the precheck.
            assert!(AtlassianAuth::credentials_present_with_overrides(
                "jira", &store, None, None,
            ));
            // With a base_url override: env must NOT short-circuit; the
            // store has no jira entry so the precheck must fail.
            assert!(
                !AtlassianAuth::credentials_present_with_overrides(
                    "jira",
                    &store,
                    Some("https://other.example.com"),
                    None,
                ),
                "precheck must skip the env shortcut when overrides are present",
            );
        });
    }

    #[test]
    fn creds_complete_token_override_unblocks_missing_token() {
        // Real-world CI scenario: a Jira entry in credentials.yaml has the
        // email and base_url from a prior interactive `tap mount jira`, but
        // the token isn't in this CI runner's keychain. A service.yaml
        // `auth_token_env: JIRA_TOKEN` is supposed to make the mount succeed
        // — so the override must short-circuit the precheck, not the other
        // way around.
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: Some("dev@acme.com".to_string()),
            token: None,
            ..Default::default()
        };
        assert!(
            !creds_complete_with_overrides(Some(&creds), None, None),
            "baseline: missing token must fail when no override is supplied",
        );
        assert!(
            creds_complete_with_overrides(Some(&creds), None, Some("ci-token")),
            "token override must satisfy the precheck even when creds.token is None",
        );
    }

    #[test]
    fn creds_complete_base_url_override_unblocks_missing_base_url() {
        let creds = ConnectorCredentials {
            base_url: None,
            email: Some("dev@acme.com".to_string()),
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        assert!(
            !creds_complete_with_overrides(Some(&creds), None, None),
            "baseline: missing base_url must fail when no override is supplied",
        );
        assert!(
            creds_complete_with_overrides(Some(&creds), Some("https://acme.example.com"), None),
            "base_url override must satisfy the precheck even when creds.base_url is None",
        );
    }

    #[test]
    fn creds_complete_email_still_required_from_creds() {
        // Email is the one field overrides don't supply — the precheck must
        // still fail when it's missing, regardless of overrides.
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: None,
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        assert!(
            !creds_complete_with_overrides(
                Some(&creds),
                Some("https://override.example.com"),
                Some("override-tok")
            ),
            "missing email must fail even with both overrides present",
        );
    }

    #[test]
    fn from_credentials_with_overrides_replaces_base_url() {
        let creds = ConnectorCredentials {
            base_url: Some("https://default.atlassian.net".to_string()),
            email: Some("dev@acme.com".to_string()),
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        let auth = AtlassianAuth::from_credentials_with_overrides(
            "jira",
            &creds,
            Some("https://acme-override.example.com"),
            None,
        )
        .unwrap();
        assert_eq!(
            auth.base_url, "https://acme-override.example.com",
            "overrides.base_url must win over creds.base_url",
        );
        // Token came from creds — auth header unchanged.
        assert_eq!(
            auth.auth_header,
            format!("Basic {}", base64_encode(b"dev@acme.com:api-tok"))
        );
    }

    #[test]
    fn from_credentials_with_overrides_replaces_token() {
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: Some("dev@acme.com".to_string()),
            token: Some("old-tok".to_string()),
            ..Default::default()
        };
        let auth =
            AtlassianAuth::from_credentials_with_overrides("jira", &creds, None, Some("new-tok"))
                .unwrap();
        assert_eq!(auth.base_url, "https://acme.atlassian.net");
        assert_eq!(
            auth.auth_header,
            format!("Basic {}", base64_encode(b"dev@acme.com:new-tok")),
            "token override must win over creds.token",
        );
    }

    #[test]
    fn from_credentials_with_overrides_falls_through_when_none() {
        let creds = ConnectorCredentials {
            base_url: Some("https://acme.atlassian.net".to_string()),
            email: Some("dev@acme.com".to_string()),
            token: Some("api-tok".to_string()),
            ..Default::default()
        };
        let auth =
            AtlassianAuth::from_credentials_with_overrides("jira", &creds, None, None).unwrap();
        // Identical to from_credentials when no overrides given.
        let baseline = AtlassianAuth::from_credentials("jira", &creds).unwrap();
        assert_eq!(auth.base_url, baseline.base_url);
        assert_eq!(auth.auth_header, baseline.auth_header);
    }

    #[test]
    fn test_extract_frontmatter() {
        let input = "---\nid: \"123\"\ntitle: \"Test\"\n---\n\nBody";
        let fm = extract_frontmatter(input).unwrap();
        assert_eq!(fm.get("id").unwrap().as_str().unwrap(), "123");
        assert_eq!(fm.get("title").unwrap().as_str().unwrap(), "Test");
    }
}

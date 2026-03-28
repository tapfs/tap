//! Shared Atlassian Cloud authentication helper.
//!
//! Both Jira and Confluence use the same auth mechanism:
//!   Basic auth with `email:api_token` base64-encoded.
//!
//! Env vars:
//!   - `ATLASSIAN_DOMAIN`    (e.g., `your-company` -> `https://your-company.atlassian.net`)
//!   - `ATLASSIAN_EMAIL`     (the account email)
//!   - `ATLASSIAN_API_TOKEN` (the API token)

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::Value;

/// Configuration parsed from env vars, shared by Jira and Confluence connectors.
pub struct AtlassianAuth {
    pub client: Client,
    pub base_url: String,
    pub auth_header: String,
}

impl AtlassianAuth {
    /// Build an `AtlassianAuth` from environment variables.
    pub fn from_env() -> Result<Self> {
        let domain = std::env::var("ATLASSIAN_DOMAIN")
            .context("ATLASSIAN_DOMAIN env var required")?;
        let email = std::env::var("ATLASSIAN_EMAIL")
            .context("ATLASSIAN_EMAIL env var required")?;
        let token = std::env::var("ATLASSIAN_API_TOKEN")
            .context("ATLASSIAN_API_TOKEN env var required")?;

        let base_url = format!("https://{}.atlassian.net", domain);
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
            base_url,
            auth_header,
        })
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

            let resp = self
                .client
                .get(url)
                .header("Authorization", &self.auth_header)
                .header("Accept", "application/json")
                .send()
                .await
                .with_context(|| format!("GET {}", url))?;

            match resp.status() {
                s if s == reqwest::StatusCode::UNAUTHORIZED => {
                    last_err = Some(anyhow!("GET {} unauthorized (401)", url));
                    continue;
                }
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(retry_after) = resp.headers().get("retry-after") {
                        if let Ok(secs) =
                            retry_after.to_str().unwrap_or("5").parse::<u64>()
                        {
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

        Err(last_err
            .unwrap_or_else(|| anyhow!("GET {} failed after {} retries", url, max_retries)))
    }

    /// Send a PUT request with JSON body.
    pub async fn put_json(&self, url: &str, body: &Value) -> Result<Value> {
        let max_retries = 3u32;
        let mut last_err = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            let resp = self
                .client
                .put(url)
                .header("Authorization", &self.auth_header)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(body)
                .send()
                .await
                .with_context(|| format!("PUT {}", url))?;

            match resp.status() {
                s if s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(retry_after) = resp.headers().get("retry-after") {
                        if let Ok(secs) =
                            retry_after.to_str().unwrap_or("5").parse::<u64>()
                        {
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

        Err(last_err
            .unwrap_or_else(|| anyhow!("PUT {} failed after {} retries", url, max_retries)))
    }

    /// Send a POST request with JSON body.
    pub async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let resp = self
            .client
            .post(url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {}", url))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("POST {} failed: HTTP {} - {}", url, status, body_text));
        }

        resp.json().await.context("parsing JSON response")
    }
}

// ---------------------------------------------------------------------------
// Base64 encoding (inline implementation, matching the Google connector style)
// ---------------------------------------------------------------------------

/// Standard base64 encode (not URL-safe).
pub fn base64_encode(input: &[u8]) -> String {
    static ENCODE_TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity((input.len() + 2) / 3 * 4);

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
        assert_eq!(base64_encode(b"user@example.com:token123"), "dXNlckBleGFtcGxlLmNvbTp0b2tlbjEyMw==");
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
    fn test_extract_frontmatter() {
        let input = "---\nid: \"123\"\ntitle: \"Test\"\n---\n\nBody";
        let fm = extract_frontmatter(input).unwrap();
        assert_eq!(fm.get("id").unwrap().as_str().unwrap(), "123");
        assert_eq!(fm.get("title").unwrap().as_str().unwrap(), "Test");
    }
}

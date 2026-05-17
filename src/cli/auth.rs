//! Interactive authentication flows for connectors.
//!
//! - `prompt_api_key()` — prompts for a bearer token (Stripe, GitHub, etc.)
//! - `oauth2_browser_flow()` — browser-based OAuth2 for any connector with OAuth2 auth spec

use anyhow::{Context, Result};
use std::io::{self, Write};
use std::path::Path;

use crate::connector::spec::ConnectorSpec;
use crate::credentials::CredentialStore;

/// Prompt the user to enter an API key for a connector.
/// Returns the token string on success.
pub fn prompt_api_key(
    connector_name: &str,
    spec: Option<&ConnectorSpec>,
    data_dir: &Path,
) -> Result<String> {
    let auth = spec.and_then(|s| s.auth.as_ref());
    let token_env = auth.and_then(|a| a.token_env.as_ref());
    let setup_url = auth.and_then(|a| a.setup_url.as_ref());
    let setup_instructions = auth.and_then(|a| a.setup_instructions.as_ref());

    println!();
    println!("{} requires authentication.", connector_name);

    if let Some(url) = setup_url {
        println!("Get your API key from: {}", url);
    }
    if let Some(instructions) = setup_instructions {
        println!("  {}", instructions);
    }
    if let Some(env) = token_env {
        println!("  (or set the {} environment variable)", env);
    }

    println!();
    print!("Enter your {} API key: ", connector_name);
    io::stdout().flush()?;

    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        anyhow::bail!("No token provided");
    }

    CredentialStore::save_token(data_dir, connector_name, &token)?;
    println!("{}", saved_message());

    Ok(token)
}

/// Run OAuth2 authorization code flow for any connector.
/// Uses the auth spec's OAuth2 fields (auth_url, token_url, client_id, client_secret, scopes).
pub async fn oauth2_browser_flow(
    connector_name: &str,
    auth: &crate::connector::spec::AuthSpec,
    data_dir: &std::path::Path,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let auth_url = auth
        .auth_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 auth_url not specified in connector spec"))?;
    let token_url = auth
        .token_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 token_url not specified in connector spec"))?;
    let client_id = auth
        .client_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 client_id not specified in connector spec"))?;
    let client_secret = auth
        .client_secret
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 client_secret not specified in connector spec"))?;
    let scopes = auth.scopes.as_deref().unwrap_or("");

    // Find a free port for the local callback server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{}", port);

    // Build authorization URL
    let authorization_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        auth_url,
        percent_encode(client_id),
        percent_encode(&redirect_uri),
        percent_encode(scopes),
    );

    println!();
    println!("{} requires OAuth2 authentication.", connector_name);
    println!("Opening browser for sign-in...");
    println!();

    // Open browser
    let _ = open_browser(&authorization_url);

    println!(
        "Waiting for authorization (listening on {})...",
        redirect_uri
    );
    println!("If the browser didn't open, visit:");
    println!("  {}", authorization_url);
    println!();

    // Wait for the OAuth callback
    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the authorization code from "GET /?code=AUTH_CODE&scope=... HTTP/1.1"
    let code = extract_code_from_request(&request)
        .ok_or_else(|| anyhow::anyhow!("No authorization code in callback"))?;

    // Send a friendly response to the browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Authentication successful!</h2>\
        <p>You can close this window and return to the terminal.</p></body></html>";
    stream.write_all(response.as_bytes()).await?;
    drop(stream);
    drop(listener);

    println!("Authorization code received. Exchanging for tokens...");

    // Exchange the code for tokens
    let client = reqwest::Client::new();
    let resp = client
        .post(token_url)
        .form(&[
            ("code", code.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .error_for_status()
        .context("token exchange failed")?;

    let token_resp: serde_json::Value = resp.json().await?;
    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token in response"))?;
    let refresh_token = token_resp["refresh_token"].as_str().ok_or_else(|| {
        anyhow::anyhow!("no refresh_token in response (need access_type=offline)")
    })?;

    CredentialStore::save_oauth2(
        data_dir,
        connector_name,
        access_token,
        refresh_token,
        client_id,
        client_secret,
    )?;

    println!("{}", saved_message());

    Ok(())
}

/// Run OAuth 2.0 Authorization Code + PKCE for public clients (e.g. X v2).
///
/// Mirrors `oauth2_browser_flow` but sends `code_challenge` to the
/// authorize endpoint and `code_verifier` (not `client_secret`) to the
/// token endpoint. Stores `{access_token, refresh_token, client_id}` in
/// the existing credential store — the daemon's `OAuth2Config` builds with
/// `client_secret: None` and `RestConnector::ensure_token` handles the
/// refresh without a secret.
pub async fn oauth2_pkce_browser_flow(
    connector_name: &str,
    auth: &crate::connector::spec::AuthSpec,
    data_dir: &std::path::Path,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::cli::pkce::{
        build_authorize_url, build_token_exchange_form, parse_callback, AuthorizeParams,
        CallbackPayload, PkcePair, TokenResponse,
    };

    let auth_url = auth
        .auth_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 PKCE: auth_url not specified in connector spec"))?;
    let token_url = auth
        .token_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 PKCE: token_url not specified in connector spec"))?;
    let client_id = auth
        .client_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OAuth2 PKCE: client_id not specified in connector spec"))?;
    let scopes = auth.scopes.as_deref().unwrap_or("");

    // Bind localhost listener, pick a free port. X requires the exact
    // redirect_uri to be registered in the developer portal — using
    // 127.0.0.1 with an ephemeral port works when the developer registered
    // `http://127.0.0.1/callback` (X's docs explicitly accept loopback +
    // any port for desktop clients).
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

    let pkce = PkcePair::new();
    // State is a CSRF nonce, not a secret — but it must be unguessable so an
    // attacker can't trick the listener into accepting their callback. We
    // reuse the verifier-generation entropy by simply minting another pair
    // and using its (discarded) challenge as the state value.
    let state = PkcePair::new().challenge;

    let authorization_url = build_authorize_url(&AuthorizeParams {
        authorize_url: auth_url,
        client_id,
        redirect_uri: &redirect_uri,
        scopes,
        challenge: &pkce.challenge,
        state: &state,
    });

    println!();
    println!("{} requires OAuth 2.0 PKCE authentication.", connector_name);
    println!("Opening browser for sign-in...");
    println!();
    let _ = open_browser(&authorization_url);
    println!("Waiting for authorization on {} ...", redirect_uri);
    println!("If the browser didn't open, visit:");
    println!("  {}", authorization_url);
    println!();

    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let payload = parse_callback(&request)?;
    // Acknowledge the browser regardless of outcome so it shows something
    // useful to the user. Then evaluate.
    let body = match &payload {
        CallbackPayload::Success { .. } => {
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
             <html><body><h2>Authentication successful!</h2>\
             <p>You can close this window and return to the terminal.</p></body></html>"
        }
        CallbackPayload::Error { .. } => {
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
             <html><body><h2>Authentication failed</h2>\
             <p>See the terminal for details. You can close this window.</p></body></html>"
        }
    };
    stream.write_all(body.as_bytes()).await?;
    drop(stream);
    drop(listener);

    let code = match payload {
        CallbackPayload::Success {
            code,
            state: returned_state,
        } => {
            if returned_state.as_deref() != Some(state.as_str()) {
                anyhow::bail!(
                    "OAuth callback state mismatch — refusing to exchange code (possible CSRF)"
                );
            }
            code
        }
        CallbackPayload::Error {
            error,
            error_description,
            ..
        } => {
            let desc = error_description.unwrap_or_default();
            anyhow::bail!("OAuth authorization denied: {} {}", error, desc);
        }
    };

    println!("Authorization code received. Exchanging for tokens...");

    let client = reqwest::Client::new();
    let form = build_token_exchange_form(&code, &pkce.verifier, client_id, &redirect_uri);
    let resp = client
        .post(token_url)
        .form(&form)
        .send()
        .await?
        .error_for_status()
        .context("token exchange failed")?;
    let token_json: serde_json::Value = resp.json().await?;
    let token_resp = TokenResponse::from_json(&token_json)?;
    let refresh_token = token_resp.refresh_token.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "token response has no refresh_token — request the `offline.access` scope so the daemon can refresh after the access token expires"
        )
    })?;

    // Compute absolute expiry. Apply the same 80% safety margin
    // `RestConnector::ensure_token` uses so the in-memory and on-disk
    // expiry stay consistent across daemon restarts.
    let expires_at = token_resp.expires_in.map(|secs| {
        let margined = (secs * 4 / 5).max(60);
        chrono::Utc::now().timestamp() + margined as i64
    });

    CredentialStore::save_oauth2_pkce(
        data_dir,
        connector_name,
        &token_resp.access_token,
        refresh_token,
        client_id,
        expires_at,
    )?;

    println!("{}", saved_message());
    Ok(())
}

/// Run OAuth2 Device Flow for connectors that support it (e.g. GitHub).
/// User visits a URL and enters a code — no local server needed.
pub async fn oauth2_device_flow(
    connector_name: &str,
    auth: &crate::connector::spec::AuthSpec,
    data_dir: &std::path::Path,
) -> Result<()> {
    let device_code_url = auth
        .device_code_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("device_code_url not specified"))?;
    let token_url = auth
        .token_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("token_url not specified"))?;
    let client_id = auth
        .client_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("client_id not specified"))?;
    let scopes = auth.scopes.as_deref().unwrap_or("");

    // Step 1: Request device and user codes
    let client = reqwest::Client::new();
    let resp = client
        .post(device_code_url)
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", scopes)])
        .send()
        .await?
        .error_for_status()
        .context("device code request failed")?;

    let body: serde_json::Value = resp.json().await?;
    let device_code = body["device_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no device_code in response"))?;
    let user_code = body["user_code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no user_code in response"))?;
    let verification_uri = body["verification_uri"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no verification_uri in response"))?;
    let interval = body["interval"].as_u64().unwrap_or(5);

    // Step 2: Show the user code and open browser
    println!();
    println!("{} requires authentication.", connector_name);
    println!();
    println!("  Go to: {}", verification_uri);
    println!("  Enter code: {}", user_code);
    println!();

    let _ = open_browser(verification_uri);

    println!("Waiting for authorization...");

    // Step 3: Poll for the token
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let resp = client
            .post(token_url)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;

        if let Some(token) = body["access_token"].as_str() {
            CredentialStore::save_token(data_dir, connector_name, token)?;
            println!("Authenticated! {}", saved_message());
            return Ok(());
        }

        match body["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            Some("expired_token") => anyhow::bail!("Device code expired. Please try again."),
            Some("access_denied") => anyhow::bail!("Authorization denied by user."),
            Some(err) => anyhow::bail!("OAuth2 error: {}", err),
            None => anyhow::bail!("Unexpected response: {}", body),
        }
    }
}

/// Run the appropriate interactive auth flow for a connector that the factory
/// couldn't construct because credentials were missing.
///
/// On a non-interactive session (no TTY on stdin), prints actionable
/// instructions to stderr and returns an error so the caller can bail.
pub async fn handle_auth_required(
    auth_err: &crate::connector::factory::AuthRequired,
    data_dir: &Path,
) -> Result<()> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return Err(bail_non_interactive(auth_err));
    }

    if matches!(auth_err.connector_name.as_str(), "jira" | "confluence") {
        return prompt_atlassian_credentials(&auth_err.connector_name, data_dir);
    }

    let default_auth = default_oauth2_config(&auth_err.connector_name);
    let auth = auth_err
        .spec
        .as_ref()
        .and_then(|s| s.auth.clone())
        .unwrap_or(default_auth);

    let has_device_flow = auth.device_code_url.is_some() && auth.client_id.is_some();
    let pkce_ready = auth.auth_type == "oauth2_pkce"
        && auth.auth_url.is_some()
        && auth.token_url.is_some()
        && auth.client_id.is_some();
    let oauth2_ready = auth.auth_type == "oauth2"
        && auth.auth_url.is_some()
        && auth.token_url.is_some()
        && auth.client_id.is_some();

    if has_device_flow {
        oauth2_device_flow(&auth_err.connector_name, &auth, data_dir).await
    } else if pkce_ready {
        oauth2_pkce_browser_flow(&auth_err.connector_name, &auth, data_dir).await
    } else if oauth2_ready {
        oauth2_browser_flow(&auth_err.connector_name, &auth, data_dir).await
    } else {
        prompt_api_key(&auth_err.connector_name, auth_err.spec.as_ref(), data_dir).map(|_| ())
    }
}

/// Prompt the user for Atlassian Cloud credentials (domain, email, API token)
/// and persist them via `AtlassianAuth::save_credentials`.
pub fn prompt_atlassian_credentials(connector_name: &str, data_dir: &Path) -> Result<()> {
    println!();
    println!(
        "{} requires Atlassian Cloud authentication.",
        connector_name
    );
    println!(
        "Generate an API token at: https://id.atlassian.com/manage-profile/security/api-tokens"
    );
    println!();

    let domain = read_line("Atlassian domain (e.g. mycompany or mycompany.atlassian.net): ")?;
    if domain.is_empty() {
        anyhow::bail!("domain is required");
    }
    let email = read_line("Atlassian account email: ")?;
    if email.is_empty() {
        anyhow::bail!("email is required");
    }
    let token = read_line("API token: ")?;
    if token.is_empty() {
        anyhow::bail!("API token is required");
    }

    crate::connector::atlassian_auth::AtlassianAuth::save_credentials(
        data_dir,
        connector_name,
        &domain,
        &email,
        &token,
    )?;
    println!("{}", saved_message());
    Ok(())
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{}", prompt);
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// Print actionable guidance to stderr and return an error explaining why we
/// can't proceed. Use from non-interactive call sites (CI, daemon spawn) where
/// `handle_auth_required` would otherwise need to be called purely to surface
/// the same message — and where callers were previously hiding behind an
/// `unreachable!()` after that call.
pub fn bail_non_interactive(auth_err: &crate::connector::factory::AuthRequired) -> anyhow::Error {
    print_non_interactive_hint(&auth_err.connector_name, auth_err.spec.as_ref());
    anyhow::anyhow!(
        "connector '{}' requires authentication and stdin is not a terminal — \
         rerun from a terminal or set the connector's credentials in advance",
        auth_err.connector_name
    )
}

fn print_non_interactive_hint(connector: &str, spec: Option<&ConnectorSpec>) {
    let auth = spec.and_then(|s| s.auth.as_ref());
    eprintln!();
    eprintln!("Authentication required for connector '{}'.", connector);
    if let Some(a) = auth {
        if let Some(url) = &a.setup_url {
            eprintln!("  Get credentials at: {}", url);
        }
        if let Some(env) = &a.token_env {
            eprintln!("  Then set {}=...", env);
        }
        if let Some(instructions) = &a.setup_instructions {
            eprintln!("  {}", instructions);
        }
    }
    eprintln!(
        "  Or run `tap mount {}` from an interactive terminal to authenticate.",
        connector
    );
    eprintln!();
}

/// Return default OAuth2 config for native connectors that don't have a YAML spec.
pub fn default_oauth2_config(connector_name: &str) -> crate::connector::spec::AuthSpec {
    match connector_name {
        "google" => crate::connector::spec::AuthSpec {
            auth_type: "oauth2".to_string(),
            token_env: Some("GOOGLE_ACCESS_TOKEN".to_string()),
            setup_url: Some("https://console.cloud.google.com/apis/credentials".to_string()),
            setup_instructions: None,
            auth_url: Some("https://accounts.google.com/o/oauth2/auth".to_string()),
            token_url: Some("https://oauth2.googleapis.com/token".to_string()),
            client_id: Some("662747120817-j4s3ie5d2vmrc5manpj4di65gl3v524n.apps.googleusercontent.com".to_string()),
            client_secret: Some("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".to_string()),
            scopes: Some("https://www.googleapis.com/auth/drive.readonly https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/calendar.readonly".to_string()),
            device_code_url: None,
        },
        _ => crate::connector::spec::AuthSpec {
            auth_type: "bearer".to_string(),
            token_env: None,
            setup_url: None,
            setup_instructions: None,
            auth_url: None,
            token_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            device_code_url: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn saved_message() -> &'static str {
    if std::env::var("TAPFS_NO_KEYCHAIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        "Credentials saved to ~/.tapfs/credentials.yaml"
    } else {
        "Credentials saved to OS keychain"
    }
}

/// Minimal percent-encoding for URL query parameters.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", byte));
            }
        }
    }
    out
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
    }
    Ok(())
}

fn extract_code_from_request(request: &str) -> Option<String> {
    // Parse "GET /?code=XXXX&scope=... HTTP/1.1"
    let first_line = request.lines().next()?;
    let path = first_line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for param in query.split('&') {
        let mut parts = param.splitn(2, '=');
        if parts.next()? == "code" {
            return parts.next().map(|s| s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::factory::AuthRequired;

    #[tokio::test]
    async fn handle_auth_required_non_tty_returns_error_with_hint() {
        // `cargo test` runs with non-terminal stdin, so the non-TTY branch
        // is the path under test.
        let dir = tempfile::tempdir().unwrap();
        let auth_err = AuthRequired {
            connector_name: "github".to_string(),
            spec: None,
        };
        let result = handle_auth_required(&auth_err, dir.path()).await;
        let err = result.expect_err("expected non-TTY to return an error");
        let msg = err.to_string();
        assert!(
            msg.contains("requires authentication"),
            "unexpected error: {}",
            msg
        );
        assert!(msg.contains("not a terminal"), "unexpected error: {}", msg);
    }

    #[tokio::test]
    async fn handle_auth_required_non_tty_does_not_touch_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let auth_err = AuthRequired {
            connector_name: "linear".to_string(),
            spec: None,
        };
        let _ = handle_auth_required(&auth_err, dir.path()).await;
        // The non-TTY path must not write any credentials files.
        assert!(!dir.path().join("credentials.yaml").exists());
    }
}

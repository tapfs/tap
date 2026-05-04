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

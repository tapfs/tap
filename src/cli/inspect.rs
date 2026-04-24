use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Parse a tapfs resource path into (connector_name, collection, resource_id).
fn parse_resource_path(path: &Path, data_dir: &Path) -> Result<(String, String, String)> {
    let mounts_path = data_dir.join("mounts.json");
    let mounts_content = std::fs::read_to_string(&mounts_path)
        .context("no active mount found — is tapfs running?")?;
    let mounts: serde_json::Value = serde_json::from_str(&mounts_content)?;

    let mount_point = mounts
        .get("mount_point")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("mount_point not found in mounts.json"))?;

    // Strip mount point prefix to get relative path
    let path_str = path.display().to_string();
    let relative = path_str
        .strip_prefix(mount_point)
        .ok_or_else(|| anyhow!("path {} is not under mount point {}", path_str, mount_point))?;
    let relative = relative.trim_start_matches('/');

    // Expected: <connector>/<collection>/<resource>.md
    let parts: Vec<&str> = relative.splitn(3, '/').collect();
    if parts.len() < 3 {
        return Err(anyhow!(
            "expected path like <mount>/<connector>/<collection>/<resource>.md, got: {}",
            path_str
        ));
    }

    let resource = parts[2]
        .strip_suffix(".draft.md")
        .or_else(|| parts[2].strip_suffix(".md"))
        .unwrap_or(parts[2]);

    Ok((
        parts[0].to_string(),
        parts[1].to_string(),
        resource.to_string(),
    ))
}

/// `tap inspect <path>` — print raw API JSON for a mounted resource.
///
/// Reads from the VFS cache if the resource has been read before (via `cat`).
/// If not cached, tells the user to `cat` the file first.
pub async fn run(path: &Path, data_dir: &Path) -> Result<()> {
    let (_connector, collection, resource_id) = parse_resource_path(path, data_dir)?;

    let mounts_path = data_dir.join("mounts.json");
    let mounts_content = std::fs::read_to_string(&mounts_path)
        .context("no active mount found — is tapfs running?")?;
    let mounts: serde_json::Value = serde_json::from_str(&mounts_content)?;

    let spec_path = mounts
        .get("spec")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("spec path not found in mounts.json — remount to fix"))?;

    let spec_yaml = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading spec from {:?}", spec_path))?;
    let spec = crate::connector::spec::ConnectorSpec::from_yaml(&spec_yaml)?;

    let coll = spec
        .collections
        .iter()
        .find(|c| c.name == collection)
        .ok_or_else(|| anyhow!("collection '{}' not found in spec", collection))?;

    // Build the API URL
    let base = spec.base_url.trim_end_matches('/');
    let endpoint = coll.get_endpoint.replace("{id}", &resource_id);
    let url = if endpoint.starts_with('/') {
        format!("{}{}", base, endpoint)
    } else {
        format!("{}/{}", base, endpoint)
    };

    // Authenticate
    let token = spec
        .auth
        .as_ref()
        .and_then(|auth| auth.token_env.as_ref())
        .and_then(|env_var| std::env::var(env_var).ok());

    if token.is_none() {
        return Err(anyhow!(
            "no API token found. Set {} environment variable.",
            spec.auth
                .as_ref()
                .and_then(|a| a.token_env.as_deref())
                .unwrap_or("the appropriate token")
        ));
    }

    let client = reqwest::Client::new();
    let mut request = client
        .get(&url)
        .header("Accept", "application/json")
        .header("User-Agent", "tapfs/0.1");

    if let Some(ref tok) = token {
        request = request.bearer_auth(tok);
    }

    let response = request.send().await.context("API request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "API returned HTTP {}: {}",
            status,
            &body[..body.len().min(512)]
        ));
    }

    let json: serde_json::Value = response.json().await.context("parsing JSON response")?;
    println!("{}", serde_json::to_string_pretty(&json)?);

    Ok(())
}

use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Parse mounts.json and a resource path into (collection, resource_id, spec_path).
fn resolve_from_path(
    path: &Path,
    data_dir: &Path,
) -> Result<(String, String, String, Option<String>)> {
    let mounts_path = data_dir.join("mounts.json");
    let mounts_content = std::fs::read_to_string(&mounts_path)
        .context("no active mount found — is tapfs running?")?;
    let mounts: serde_json::Value = serde_json::from_str(&mounts_content)?;

    let mount_point = mounts
        .get("mount_point")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("mount_point not found in mounts.json"))?;

    let connector_name = mounts
        .get("connector")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("connector not found in mounts.json"))?;

    let spec_path = mounts
        .get("spec")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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
        connector_name.to_string(),
        parts[1].to_string(),
        resource.to_string(),
        spec_path,
    ))
}

/// Find and load the connector spec YAML.
fn load_spec(
    connector_name: &str,
    saved_spec_path: Option<&str>,
    data_dir: &Path,
) -> Result<String> {
    // 1. Use spec path from mounts.json if available
    if let Some(sp) = saved_spec_path {
        let p = Path::new(sp);
        if p.exists() {
            return std::fs::read_to_string(p)
                .with_context(|| format!("reading spec from {:?}", p));
        }
    }

    // 2. Search common locations
    let candidates = [
        data_dir
            .join("connectors")
            .join(format!("{}.yaml", connector_name)),
        Path::new("connectors").join(format!("{}.yaml", connector_name)),
    ];
    for p in &candidates {
        if p.exists() {
            return std::fs::read_to_string(p)
                .with_context(|| format!("reading spec from {:?}", p));
        }
    }

    Err(anyhow!("connector spec not found for '{}'", connector_name))
}

/// `tap inspect <path>` — print raw API JSON for a mounted resource.
pub async fn run(path: &Path, data_dir: &Path) -> Result<()> {
    let (connector_name, collection, resource_id, spec_path) = resolve_from_path(path, data_dir)?;

    let spec_yaml = load_spec(&connector_name, spec_path.as_deref(), data_dir)?;
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

use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Parse a tapfs resource path into (connector, collection, resource_id).
fn parse_resource_path(path: &Path, data_dir: &Path) -> Result<(String, String, String)> {
    let mounts_path = data_dir.join("mounts.json");
    let mounts_content = std::fs::read_to_string(&mounts_path)
        .context("no active mount found — is tapfs running?")?;
    let mounts: serde_json::Value = serde_json::from_str(&mounts_content)?;

    let mount_point = mounts
        .get("mount_point")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("mount_point not found in mounts.json"))?;

    let path_str = path.display().to_string();
    let relative = path_str
        .strip_prefix(mount_point)
        .ok_or_else(|| anyhow!("path {} is not under mount point {}", path_str, mount_point))?;
    let relative = relative.trim_start_matches('/');

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
/// Connects to the running tapfs process via Unix socket and reads
/// the raw JSON from the in-memory cache. No API token needed.
pub async fn run(path: &Path, data_dir: &Path) -> Result<()> {
    let (connector, collection, resource_id) = parse_resource_path(path, data_dir)?;
    let cache_key = format!("{}/{}/{}", connector, collection, resource_id);

    let socket_path = data_dir.join("tap.sock");
    let request = serde_json::json!({
        "cmd": "inspect",
        "key": cache_key,
    });

    let response = crate::ipc::send_request(&socket_path, &request).await?;

    if response.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(data) = response.get("data") {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        let err = response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow!("{}", err));
    }

    Ok(())
}

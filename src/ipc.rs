//! Unix domain socket IPC server for CLI ↔ mount process communication.
//!
//! The mount process starts a socket at `~/.tapfs/tap.sock`. CLI commands
//! like `tap inspect` connect to it to query the in-memory cache without
//! needing API credentials or making network requests.
//!
//! Protocol: newline-delimited JSON request/response.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::cache::store::Cache;

/// Start the IPC socket server in a background task.
///
/// The socket is created at `socket_path` with `0o600` permissions (owner-only).
/// Each incoming connection can send one JSON request and receives one JSON response.
pub fn start(cache: Arc<Cache>, socket_path: PathBuf) {
    // Remove stale socket from a previous run.
    let _ = std::fs::remove_file(&socket_path);

    tokio::spawn(async move {
        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("failed to bind IPC socket at {:?}: {}", socket_path, e);
                return;
            }
        };

        // Set socket permissions to owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
        }

        tracing::info!(path = %socket_path.display(), "IPC socket listening");

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!("IPC accept error: {}", e);
                    continue;
                }
            };

            let cache = cache.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                if let Ok(Some(line)) = lines.next_line().await {
                    let response = handle_request(&cache, &line);
                    let mut out = serde_json::to_string(&response).unwrap_or_default();
                    out.push('\n');
                    let _ = writer.write_all(out.as_bytes()).await;
                }
            });
        }
    });
}

/// Handle a single IPC request and return a JSON response.
fn handle_request(cache: &Cache, request: &str) -> serde_json::Value {
    let req: serde_json::Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(_) => return error_response("invalid JSON request"),
    };

    let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

    match cmd {
        "inspect" => {
            let key = match req.get("key").and_then(|v| v.as_str()) {
                Some(k) => k,
                None => return error_response("missing 'key' field"),
            };
            match cache.get_resource(key) {
                Some(resource) => match resource.raw_json {
                    Some(json) => serde_json::json!({ "ok": true, "data": json }),
                    None => error_response("resource cached but no raw JSON available"),
                },
                None => error_response("resource not in cache — cat the file first"),
            }
        }
        "status" => {
            let stats = cache.stats();
            serde_json::json!({
                "ok": true,
                "resources_cached": stats.0,
                "metadata_cached": stats.1,
            })
        }
        "invalidate" => {
            let key = match req.get("key").and_then(|v| v.as_str()) {
                Some(k) => k,
                None => return error_response("missing 'key' field"),
            };
            cache.invalidate(key);
            serde_json::json!({ "ok": true })
        }
        _ => error_response(&format!("unknown command: {}", cmd)),
    }
}

fn error_response(msg: &str) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": msg })
}

/// Connect to the IPC socket and send a request, returning the response.
pub async fn send_request(
    socket_path: &Path,
    request: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to tapfs — is it running? ({})", e))?;

    let (reader, mut writer) = stream.into_split();

    let mut req_str = serde_json::to_string(request)?;
    req_str.push('\n');
    writer.write_all(req_str.as_bytes()).await?;
    writer.shutdown().await?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("empty response from tapfs"))?;

    Ok(serde_json::from_str(&line)?)
}

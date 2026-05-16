//! Unix domain socket IPC server for CLI <-> mount process communication.
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

use crate::cache::disk::DiskCache;
use crate::cache::store::Cache;
use crate::cli::service::ServiceConfig;
use crate::connector::registry::ConnectorRegistry;
use crate::credentials::CredentialStore;
use crate::governance::audit::AuditLogger;

/// Shared state accessible to IPC command handlers.
pub struct IpcState {
    pub cache: Arc<Cache>,
    /// Optional persistent cache; when present, `invalidate` clears it too
    /// so external `tap invalidate` callers don't leave stale bytes on disk.
    pub disk_cache: Option<Arc<DiskCache>>,
    pub registry: Arc<ConnectorRegistry>,
    pub audit: Arc<AuditLogger>,
    pub credentials: CredentialStore,
    pub data_dir: PathBuf,
}

/// Start the IPC socket server in a background task.
///
/// The socket is created at `socket_path` with `0o600` permissions (owner-only).
/// Each incoming connection can send one JSON request and receives one JSON response.
pub fn start(state: Arc<IpcState>, socket_path: PathBuf) {
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

            let state = state.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                if let Ok(Some(line)) = lines.next_line().await {
                    let response = handle_request(&state, &line);
                    let mut out = serde_json::to_string(&response).unwrap_or_default();
                    out.push('\n');
                    let _ = writer.write_all(out.as_bytes()).await;
                }
            });
        }
    });
}

/// Handle a single IPC request and return a JSON response.
fn handle_request(state: &IpcState, request: &str) -> serde_json::Value {
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
            match state.cache.get_resource(key) {
                Some(resource) => match resource.raw_json {
                    Some(json) => serde_json::json!({ "ok": true, "data": json }),
                    None => error_response("resource cached but no raw JSON available"),
                },
                None => error_response("resource not in cache — cat the file first"),
            }
        }
        "status" => {
            let stats = state.cache.stats();
            serde_json::json!({
                "ok": true,
                "resources_cached": stats.resources,
                "metadata_cached": stats.metadata,
                "shards_cached": stats.shards,
            })
        }
        "invalidate" => {
            let key = match req.get("key").and_then(|v| v.as_str()) {
                Some(k) => k,
                None => return error_response("missing 'key' field"),
            };
            state.cache.invalidate(key);
            if let Some(disk) = &state.disk_cache {
                disk.invalidate_key(key);
            }
            serde_json::json!({ "ok": true })
        }
        "add_connector" => {
            let name = match req.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => return error_response("missing 'name' field"),
            };
            if state.registry.get(name).is_some() {
                return serde_json::json!({ "ok": true, "message": "already mounted" });
            }
            // Honor service.yaml overrides if the entry was edited there.
            let svc = ServiceConfig::load(&state.data_dir).ok();
            let entry = svc.as_ref().and_then(|s| s.get_connector(name));
            let overrides = crate::connector::factory::ConnectorOverrides {
                base_url: entry.and_then(|e| e.base_url()),
                auth_token_env: entry.and_then(|e| e.auth_token_env()),
            };
            match crate::connector::factory::create_connector_with_overrides(
                name,
                &state.audit,
                &state.credentials,
                &overrides,
            ) {
                Ok((connector, spec)) => {
                    if let Some(s) = spec {
                        state.registry.register_with_spec(connector, s);
                    } else {
                        state.registry.register(connector);
                    }
                    // Update service.yaml
                    if let Ok(mut svc) = ServiceConfig::load(&state.data_dir) {
                        svc.add_connector(name);
                        let _ = svc.save(&state.data_dir);
                    }
                    serde_json::json!({ "ok": true, "message": format!("mounted {}", name) })
                }
                Err(e) => error_response(&format!("failed to create connector {}: {}", name, e)),
            }
        }
        "remove_connector" => {
            let name = match req.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => return error_response("missing 'name' field"),
            };
            let removed = state.registry.deregister(name);
            if removed {
                // Update service.yaml
                if let Ok(mut svc) = ServiceConfig::load(&state.data_dir) {
                    svc.remove_connector(name);
                    let _ = svc.save(&state.data_dir);
                }
                serde_json::json!({ "ok": true, "message": format!("unmounted {}", name) })
            } else {
                error_response(&format!("connector '{}' is not mounted", name))
            }
        }
        "list_connectors" => {
            let names = state.registry.list();
            serde_json::json!({ "ok": true, "connectors": names })
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

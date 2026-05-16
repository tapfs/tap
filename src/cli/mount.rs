use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::cache::disk::DiskCache;
use crate::cache::store::Cache;
use crate::cli::service::ServiceConfig;
use crate::config::TapConfig;
use crate::connector::factory::{
    create_connector, create_connector_with_overrides, AuthRequired, ConnectorOverrides,
};
use crate::connector::registry::ConnectorRegistry;
use crate::connector::rest::RestConnector;
use crate::connector::spec::ConnectorSpec;
use crate::draft::store::DraftStore;
use crate::governance::audit::AuditLogger;
use crate::governance::interceptor::AuditedConnector;
use crate::version::store::VersionStore;
use crate::vfs::core::VirtualFs;

pub async fn run(config: TapConfig) -> Result<()> {
    let has_explicit_specs = config.connector_spec.is_some()
        || config
            .connector_specs
            .as_ref()
            .map(|specs| !specs.is_empty())
            .unwrap_or(false);

    // If not in daemon mode, either hot-add to running daemon or start the service
    if !config.daemon && !has_explicit_specs {
        let socket_path = config.socket_path();
        let data_dir = config.data_dir();

        // "Declarative" invocation: `tap mount` with no positional connector
        // and no --spec/--specs. The user expects the daemon to load whatever
        // they declared in service.yaml. We must not auto-mount the literal
        // "rest" stub in that case.
        let declarative = config.connector_spec.is_none()
            && config.connector_specs.is_none()
            && config.connector_name == "rest";
        let svc_has_entries = ServiceConfig::load(&data_dir)
            .map(|s| !s.connectors.is_empty())
            .unwrap_or(false);

        // Check if daemon is already running
        let daemon_running =
            crate::ipc::send_request(&socket_path, &serde_json::json!({"cmd": "status"}))
                .await
                .map(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
                .unwrap_or(false);

        if daemon_running {
            // Check if the NFS mount is actually active before hot-adding.
            // If the daemon is alive but the mount is dead (e.g. after a crash
            // and launchd restart where mount_nfs failed), we need to restart.
            let mount_active = is_mount_active(&config.mount_point);
            if !mount_active {
                eprintln!("tapfs daemon running but NFS mount is not active — restarting service");
                let _ = crate::cli::service::stop();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // Fall through to the install/start path below.
            } else if declarative {
                // Daemon is up and serving service.yaml; nothing to do.
                let resp = crate::ipc::send_request(
                    &socket_path,
                    &serde_json::json!({"cmd": "list_connectors"}),
                )
                .await?;
                if let Some(connectors) = resp.get("connectors").and_then(|v| v.as_array()) {
                    let names: Vec<&str> = connectors.iter().filter_map(|v| v.as_str()).collect();
                    println!("tapfs already running");
                    println!("Mounted: {}", names.join(", "));
                    println!("Mount point: {}", config.mount_point.display());
                }
                return Ok(());
            } else {
                // Hot-add via IPC
                let resp = crate::ipc::send_request(
                    &socket_path,
                    &serde_json::json!({"cmd": "add_connector", "name": config.connector_name}),
                )
                .await?;
                if let Some(msg) = resp.get("message").and_then(|v| v.as_str()) {
                    println!("{}", msg);
                } else if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
                    anyhow::bail!("{}", err);
                }
                return Ok(());
            }
        }

        // Bare `tap mount` with no positional connector AND no service.yaml
        // entries — there's nothing to do. Print an actionable hint.
        if declarative && !svc_has_entries {
            anyhow::bail!(
                "no connector specified and {} has no entries.\n\
                 Try: `tap mount github` (or any built-in connector name),\n\
                 or edit {} directly to declare connectors with per-connector overrides.",
                ServiceConfig::path(&data_dir).display(),
                ServiceConfig::path(&data_dir).display(),
            );
        }

        // No daemon running — handle auth (single-connector path), update
        // service.yaml, start service.
        if !declarative {
            let creds = crate::credentials::CredentialStore::load(&data_dir)?;
            let audit_tmp = std::sync::Arc::new(
                AuditLogger::new(config.audit_log_path()).context("creating audit logger")?,
            );
            if let Err(e) = create_connector(&config.connector_name, &audit_tmp, &creds) {
                if let Some(auth_err) = e.downcast_ref::<crate::connector::factory::AuthRequired>()
                {
                    crate::cli::auth::handle_auth_required(auth_err, &data_dir).await?;
                }
                // Other errors (not auth) — will be caught again when daemon starts
            }

            // Add connector to service.yaml (declarative path leaves it alone)
            let mut svc_config = ServiceConfig::load(&data_dir)?;
            svc_config.add_connector(&config.connector_name);
            svc_config.mount_point = config.mount_point.clone();
            svc_config.save(&data_dir)?;
        }

        // Install and start the service
        use crate::cli::service::{detect_service_manager, ServiceManager};
        match detect_service_manager() {
            ServiceManager::Launchd | ServiceManager::Systemd => {
                // Install plist/unit if not already present
                let _ = crate::cli::service::install();
                crate::cli::service::start()?;
                // Wait briefly for daemon to start, then verify
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if let Ok(resp) = crate::ipc::send_request(
                    &socket_path,
                    &serde_json::json!({"cmd": "list_connectors"}),
                )
                .await
                {
                    if let Some(connectors) = resp.get("connectors").and_then(|v| v.as_array()) {
                        let names: Vec<&str> =
                            connectors.iter().filter_map(|v| v.as_str()).collect();
                        println!("tapfs running in background");
                        println!("Mounted: {}", names.join(", "));
                        println!("Mount point: {}", config.mount_point.display());
                    }
                }
                return Ok(());
            }
            ServiceManager::None => {
                // No service manager (container/CI) — fall through to foreground mode
            }
        }
    }

    // 1. Initialize tracing
    let filter = if config.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
    tracing::info!("tapfs starting");

    // 2. Ensure data directories exist
    let data_dir = config.data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {:?}", data_dir))?;
    std::fs::create_dir_all(config.drafts_dir())?;
    std::fs::create_dir_all(config.versions_dir())?;

    // 3. Create the connector based on connector_name
    let audit =
        Arc::new(AuditLogger::new(config.audit_log_path()).context("creating audit logger")?);

    // Build connector(s) and register them
    let registry = ConnectorRegistry::new();

    // Load credentials (from ~/.tapfs/credentials.yaml if it exists)
    let creds = crate::credentials::CredentialStore::load(&config.data_dir())?;

    if config.daemon {
        // Daemon mode: load connectors from service.yaml
        let svc_config = ServiceConfig::load(&config.data_dir())?;
        for entry in &svc_config.connectors {
            let name = entry.name();
            let overrides = ConnectorOverrides {
                base_url: entry.base_url(),
                auth_token_env: entry.auth_token_env(),
            };
            match create_connector_with_overrides(name, &audit, &creds, &overrides) {
                Ok((connector, spec)) => {
                    if let Some(s) = spec {
                        registry.register_with_spec(connector, s);
                    } else {
                        registry.register(connector);
                    }
                    tracing::info!(connector = %name, "loaded connector from service.yaml");
                }
                Err(e) => {
                    tracing::warn!(connector = %name, error = %e, "failed to load connector from service.yaml, skipping");
                }
            }
        }
    } else {
        // Collect spec paths: --specs dir, --spec file, or built-in connector name
        let spec_paths = config.connector_specs.clone().unwrap_or_default();

        if !spec_paths.is_empty() {
            // Multi-connector mode: load each YAML spec
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(10)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .tcp_keepalive(Duration::from_secs(60))
                .build()?;

            for spec_path in &spec_paths {
                let yaml = std::fs::read_to_string(spec_path)
                    .with_context(|| format!("reading spec file {:?}", spec_path))?;
                let mut spec = ConnectorSpec::from_yaml(&yaml)?;

                // Apply base_url from credentials file if available
                if let Some(url) = creds.base_url(&spec.name) {
                    spec.base_url = url;
                }

                tracing::info!(name = %spec.name, base_url = %spec.base_url, "loaded connector spec");

                // Token: credentials file > env var
                let token = creds.token(&spec.name);
                let rest = RestConnector::new_with_token(spec.clone(), client.clone(), token);
                let inner: Arc<dyn crate::connector::traits::Connector> = Arc::new(rest);
                let audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
                registry.register_with_spec(audited, spec);
            }
        } else {
            // Single-connector mode — auth handled upfront, just create
            match create_connector(&config.connector_name, &audit, &creds) {
                Ok((connector, spec)) => {
                    if let Some(s) = spec {
                        registry.register_with_spec(connector, s);
                    } else {
                        registry.register(connector);
                    }
                }
                Err(e) => {
                    // Check if the factory is asking for interactive auth
                    if let Some(auth_err) = e.downcast_ref::<AuthRequired>() {
                        use std::io::IsTerminal;
                        if std::io::stdin().is_terminal() {
                            crate::cli::auth::handle_auth_required(auth_err, &config.data_dir())
                                .await?;
                            // Reload credentials and retry
                            let creds =
                                crate::credentials::CredentialStore::load(&config.data_dir())?;
                            let (connector, spec) =
                                create_connector(&config.connector_name, &audit, &creds)?;
                            if let Some(s) = spec {
                                registry.register_with_spec(connector, s);
                            } else {
                                registry.register(connector);
                            }
                        } else {
                            // Non-interactive (CI, daemon). Either the user
                            // supplied --connector-spec to bypass auth, or we
                            // bail with an actionable hint.
                            let Some(spec_path) = config.connector_spec.as_ref() else {
                                return Err(crate::cli::auth::bail_non_interactive(auth_err));
                            };
                            let yaml = std::fs::read_to_string(spec_path)
                                .with_context(|| format!("reading spec file {:?}", spec_path))?;
                            let mut spec = ConnectorSpec::from_yaml(&yaml)?;
                            if let Some(ref url) = config.base_url {
                                spec.base_url = url.clone();
                            }

                            tracing::info!(name = %spec.name, base_url = %spec.base_url, "loaded connector spec");

                            let client = reqwest::Client::builder()
                                .pool_max_idle_per_host(10)
                                .connect_timeout(Duration::from_secs(5))
                                .timeout(Duration::from_secs(30))
                                .tcp_keepalive(Duration::from_secs(60))
                                .build()?;

                            let token = creds.token(&spec.name);
                            let rest = RestConnector::new_with_token(spec.clone(), client, token);
                            let inner: Arc<dyn crate::connector::traits::Connector> =
                                Arc::new(rest);
                            let audited: Arc<dyn crate::connector::traits::Connector> =
                                Arc::new(AuditedConnector::new(inner, audit.clone()));
                            registry.register_with_spec(audited, spec);
                        }
                    } else {
                        // Non-auth factory failure — fall back to explicit spec path or bare connector
                        let spec = if let Some(ref spec_path) = config.connector_spec {
                            let yaml = std::fs::read_to_string(spec_path)
                                .with_context(|| format!("reading spec file {:?}", spec_path))?;
                            let mut spec = ConnectorSpec::from_yaml(&yaml)?;
                            if let Some(ref url) = config.base_url {
                                spec.base_url = url.clone();
                            }
                            spec
                        } else {
                            // Unknown connector — bare fallback
                            let base_url = config
                                .base_url
                                .clone()
                                .unwrap_or_else(|| "http://localhost:8080".to_string());
                            ConnectorSpec {
                                spec_version: None,
                                version: None,
                                description: None,
                                name: config.connector_name.clone(),
                                base_url,
                                auth: None,
                                transport: None,
                                capabilities: None,
                                agent: None,
                                collections: vec![],
                            }
                        };

                        tracing::info!(name = %spec.name, base_url = %spec.base_url, "loaded connector spec");

                        let client = reqwest::Client::builder()
                            .pool_max_idle_per_host(10)
                            .connect_timeout(Duration::from_secs(5))
                            .timeout(Duration::from_secs(30))
                            .tcp_keepalive(Duration::from_secs(60))
                            .build()?;

                        let token = creds.token(&spec.name);
                        let rest = RestConnector::new_with_token(spec.clone(), client, token);
                        let inner: Arc<dyn crate::connector::traits::Connector> = Arc::new(rest);
                        let audited: Arc<dyn crate::connector::traits::Connector> =
                            Arc::new(AuditedConnector::new(inner, audit.clone()));
                        registry.register_with_spec(audited, spec);
                    }
                }
            }
        }
    }

    let registry = Arc::new(registry);

    // 8. Create cache, draft store, version store
    let cache_ttl = Duration::from_secs(config.cache_ttl_secs.unwrap_or(60));
    let cache = Arc::new(Cache::new(cache_ttl));

    {
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                cache.evict_expired();
            }
        });
    }

    let drafts = Arc::new(DraftStore::new(config.drafts_dir()).context("creating draft store")?);
    let versions =
        Arc::new(VersionStore::new(config.versions_dir()).context("creating version store")?);
    let disk_cache =
        Arc::new(DiskCache::new(config.cache_dir()).context("creating on-disk resource cache")?);

    // 9. Ensure mount point directory exists
    std::fs::create_dir_all(&config.mount_point)
        .with_context(|| format!("creating mount point {:?}", config.mount_point))?;

    // 10. Write mounts status file so `tap status` can find us
    let specs_list: Vec<String> = config
        .connector_specs
        .as_ref()
        .map(|paths| paths.iter().map(|p| p.display().to_string()).collect())
        .or_else(|| {
            config
                .connector_spec
                .as_ref()
                .map(|p| vec![p.display().to_string()])
        })
        .unwrap_or_default();
    let mount_info = serde_json::json!({
        "connector": config.connector_name,
        "connectors": registry.list(),
        "mount_point": config.mount_point.display().to_string(),
        "spec": config.connector_spec.as_ref().map(|p| p.display().to_string()),
        "specs": specs_list,
        "pid": std::process::id(),
        "started_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        config.mounts_path(),
        serde_json::to_string_pretty(&mount_info)?,
    )?;

    // 11. Build VirtualFs
    let cache_for_ipc = cache.clone();
    let disk_for_ipc = disk_cache.clone();
    let slug_map_path = data_dir.join("slug-map.json");
    let vfs = Arc::new(
        VirtualFs::new(registry.clone(), cache, drafts, versions, audit.clone())
            .with_disk_cache(disk_cache)
            .with_slug_map(slug_map_path)
            .with_agents_md_dir(config.agents_md_dir()),
    );

    // 12. Start IPC socket for CLI commands (inspect, status, invalidate, add/remove connector)
    let ipc_state = Arc::new(crate::ipc::IpcState {
        cache: cache_for_ipc,
        disk_cache: Some(disk_for_ipc),
        registry: registry.clone(),
        audit,
        credentials: creds,
        data_dir: config.data_dir(),
    });
    crate::ipc::start(ipc_state, config.socket_path());

    // 13. Choose transport
    #[cfg(all(feature = "nfs", feature = "fuse"))]
    {
        #[allow(clippy::needless_return)]
        if cfg!(target_os = "macos") || std::env::var("TAPFS_NFS").is_ok() {
            return mount_nfs(vfs, &config).await;
        } else {
            return mount_fuse(vfs, &config).await;
        }
    }

    #[cfg(all(feature = "nfs", not(feature = "fuse")))]
    {
        mount_nfs(vfs, &config).await
    }

    #[cfg(all(feature = "fuse", not(feature = "nfs")))]
    {
        #[allow(clippy::needless_return)]
        return mount_fuse(vfs, &config).await;
    }

    #[cfg(not(any(feature = "fuse", feature = "nfs")))]
    anyhow::bail!("No transport available. Build with --features fuse or --features nfs");
}

/// Returns true if `path` is currently an active filesystem mount point.
fn is_mount_active(path: &std::path::Path) -> bool {
    let path_str = path.display().to_string();
    std::process::Command::new("mount")
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.contains(&path_str))
        })
        .unwrap_or(false)
}

#[cfg(feature = "nfs")]
/// Kill whatever process holds `port` and force-unmount `mount_point`.
/// Errors are intentionally swallowed — the point is best-effort cleanup
/// before we bind, not a hard failure.
async fn force_cleanup(port: u32, mount_point: &std::path::Path) {
    // Find PID via lsof and kill it.
    if let Ok(out) = tokio::process::Command::new("lsof")
        .args(["-ti", &format!(":{}", port)])
        .output()
        .await
    {
        let pids = String::from_utf8_lossy(&out.stdout);
        for pid in pids.split_whitespace() {
            if let Ok(pid_n) = pid.parse::<u32>() {
                let _ = tokio::process::Command::new("kill")
                    .args(["-9", pid])
                    .status()
                    .await;
                tracing::info!(pid = pid_n, "killed stale tapfs process");
                // Give the OS a moment to release the port.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
    // Force-unmount the stale NFS mount.
    let mp = mount_point.display().to_string();
    let _ = tokio::process::Command::new("umount")
        .args(["-f", &mp])
        .status()
        .await;
}

async fn mount_nfs(vfs: Arc<VirtualFs>, config: &TapConfig) -> Result<()> {
    use crate::nfs::server::TapNfs;
    use nfsserve::tcp::{NFSTcp, NFSTcpListener};

    let port = std::env::var("TAPFS_NFS_PORT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(11111);

    let bind_addr = format!("127.0.0.1:{}", port);

    // Kill any previous tapfs server holding the port and unmount the stale
    // mount point so the kernel doesn't serve cached NFS3ERR_STALE handles.
    force_cleanup(port, &config.mount_point).await;

    tracing::info!(
        mount_point = %config.mount_point.display(),
        port = port,
        "starting NFS server"
    );

    let vfs_for_shutdown = vfs.clone();
    let nfs = TapNfs::new(vfs, tokio::runtime::Handle::current());

    let listener = NFSTcpListener::bind(&bind_addr, nfs)
        .await
        .context("failed to bind NFS server")?;

    tracing::info!(port = port, "NFS server listening");

    // Mount in a background task (mount_nfs blocks until server responds,
    // so it must not run on the same task as the server).
    let mount_point = config.mount_point.clone();
    let mounts_path = config.mounts_path();
    tokio::spawn(async move {
        // Give the server a moment to start accepting connections
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mount_opts = format!(
            "nolocks,vers=3,tcp,rsize=131072,actimeo=2,port={},mountport={}",
            port, port
        );
        // Force-unmount any stale mount at this path before remounting.
        // Without this, a crashed daemon leaves cached NFS handles that cause
        // NFS3ERR_STALE on every access after the new server starts.
        let _ = tokio::process::Command::new("umount")
            .args(["-f", &mount_point.display().to_string()])
            .status()
            .await;

        tracing::info!(mount_point = %mount_point.display(), "running mount_nfs");

        let result = tokio::process::Command::new("mount_nfs")
            .args([
                "-o",
                &mount_opts,
                "localhost:/",
                &mount_point.display().to_string(),
            ])
            .status()
            .await;

        match result {
            Ok(status) if status.success() => {
                tracing::info!(mount_point = %mount_point.display(), "mounted via NFS");
            }
            Ok(status) => {
                tracing::error!("mount_nfs failed with exit code {:?}", status.code());
            }
            Err(e) => {
                tracing::error!("mount_nfs error: {}", e);
            }
        }

        // Signal handler — flush pending writes before exit
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received signal, flushing pending writes");
        vfs_for_shutdown.flush_all();
        tracing::info!("unmounting");
        let _ = tokio::process::Command::new("umount")
            .arg(&mount_point)
            .status()
            .await;
        let _ = std::fs::remove_file(&mounts_path);
        std::process::exit(0);
    });

    // Serve forever (this is the main loop)
    listener
        .handle_forever()
        .await
        .context("NFS server error")?;

    Ok(())
}

#[cfg(feature = "fuse")]
async fn mount_fuse(vfs: Arc<VirtualFs>, config: &TapConfig) -> Result<()> {
    use crate::fs::tapfs::TapFs;

    tracing::info!(
        mount_point = %config.mount_point.display(),
        "mounting FUSE filesystem"
    );

    #[allow(unused_mut)]
    let mut options = vec![fuser::MountOption::FSName("tapfs".into())];
    #[cfg(target_os = "macos")]
    {
        options.push(fuser::MountOption::CUSTOM("noappledouble".into()));
        options.push(fuser::MountOption::CUSTOM("noapplexattr".into()));
    }

    let mount_point = config.mount_point.clone();
    let vfs_for_shutdown = vfs.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received signal, flushing pending writes");
        vfs_for_shutdown.flush_all();
        tracing::info!("unmounting");
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("umount")
                .arg(&mount_point)
                .status();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&mount_point)
                .status();
        }
        std::process::exit(0);
    });

    let rt = tokio::runtime::Handle::current();
    let mount_point_clone = config.mount_point.clone();
    let mounts_path = config.mounts_path();

    tokio::task::spawn_blocking(move || {
        let fs = TapFs {
            vfs,
            rt,
            uid: unsafe { libc::getuid() },
        };
        if let Err(e) = fuser::mount2(fs, &mount_point_clone, &options) {
            tracing::error!("FUSE mount error: {}", e);
        }
        let _ = std::fs::remove_file(&mounts_path);
        tracing::info!("unmounted, exiting");
    })
    .await?;

    Ok(())
}

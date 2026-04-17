use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::cache::store::Cache;
use crate::config::TapConfig;
use crate::connector::confluence::ConfluenceConnector;
use crate::connector::google::GoogleWorkspaceConnector;
use crate::connector::jira::JiraConnector;
use crate::connector::registry::ConnectorRegistry;
use crate::connector::rest::RestConnector;
use crate::connector::spec::ConnectorSpec;
use crate::draft::store::DraftStore;
use crate::governance::audit::AuditLogger;
use crate::governance::interceptor::AuditedConnector;
use crate::version::store::VersionStore;
use crate::vfs::core::VirtualFs;

pub async fn run(config: TapConfig) -> Result<()> {
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
    let audit = Arc::new(
        AuditLogger::new(config.audit_log_path()).context("creating audit logger")?,
    );

    let audited: Arc<dyn crate::connector::traits::Connector> = if config.connector_name == "google" {
        tracing::info!("initializing Google Workspace connector");
        let inner: Arc<dyn crate::connector::traits::Connector> =
            Arc::new(GoogleWorkspaceConnector::new().context("creating Google connector")?);
        Arc::new(AuditedConnector::new(inner, audit.clone()))
    } else if config.connector_name == "jira" {
        tracing::info!("initializing Jira connector");
        let inner: Arc<dyn crate::connector::traits::Connector> =
            Arc::new(JiraConnector::new().context("creating Jira connector")?);
        Arc::new(AuditedConnector::new(inner, audit.clone()))
    } else if config.connector_name == "confluence" {
        tracing::info!("initializing Confluence connector");
        let inner: Arc<dyn crate::connector::traits::Connector> =
            Arc::new(ConfluenceConnector::new().context("creating Confluence connector")?);
        Arc::new(AuditedConnector::new(inner, audit.clone()))
    } else {
        // Generic REST connector from YAML spec
        let spec = if let Some(ref spec_path) = config.connector_spec {
            let yaml = std::fs::read_to_string(spec_path)
                .with_context(|| format!("reading spec file {:?}", spec_path))?;
            let mut spec = ConnectorSpec::from_yaml(&yaml)?;
            // Apply base URL override if provided
            if let Some(ref url) = config.base_url {
                spec.base_url = url.clone();
            }
            spec
        } else {
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080".to_string());
            ConnectorSpec {
                name: config.connector_name.clone(),
                base_url,
                auth: None,
                collections: vec![],
            }
        };

        tracing::info!(name = %spec.name, base_url = %spec.base_url, "loaded connector spec");

        // Create reqwest client
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;

        // Create REST connector from spec
        let rest = RestConnector::new(spec, client);
        let inner: Arc<dyn crate::connector::traits::Connector> = Arc::new(rest);
        Arc::new(AuditedConnector::new(inner, audit.clone()))
    };

    // 7. Create registry and register connector
    let mut registry = ConnectorRegistry::new();
    registry.register(audited);
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

    // 9. Ensure mount point directory exists
    std::fs::create_dir_all(&config.mount_point)
        .with_context(|| format!("creating mount point {:?}", config.mount_point))?;

    // 10. Write mounts status file so `tap status` can find us
    let mount_info = serde_json::json!({
        "connector": config.connector_name,
        "mount_point": config.mount_point.display().to_string(),
        "pid": std::process::id(),
        "started_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        config.mounts_path(),
        serde_json::to_string_pretty(&mount_info)?,
    )?;

    // 11. Build VirtualFs
    let vfs = Arc::new(VirtualFs::new(
        registry,
        cache,
        drafts,
        versions,
        audit,
    ));

    // 12. Choose transport
    #[cfg(all(feature = "nfs", feature = "fuse"))]
    {
        if cfg!(target_os = "macos") || std::env::var("TAPFS_NFS").is_ok() {
            return mount_nfs(vfs, &config).await;
        } else {
            return mount_fuse(vfs, &config).await;
        }
    }

    #[cfg(all(feature = "nfs", not(feature = "fuse")))]
    {
        return mount_nfs(vfs, &config).await;
    }

    #[cfg(all(feature = "fuse", not(feature = "nfs")))]
    {
        return mount_fuse(vfs, &config).await;
    }

    #[cfg(not(any(feature = "fuse", feature = "nfs")))]
    anyhow::bail!("No transport available. Build with --features fuse or --features nfs");
}

#[cfg(feature = "nfs")]
async fn mount_nfs(vfs: Arc<VirtualFs>, config: &TapConfig) -> Result<()> {
    use crate::nfs::server::TapNfs;
    use nfsserve::tcp::{NFSTcp, NFSTcpListener};

    let port = std::env::var("TAPFS_NFS_PORT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(11111);

    let bind_addr = format!("127.0.0.1:{}", port);

    tracing::info!(
        mount_point = %config.mount_point.display(),
        port = port,
        "starting NFS server"
    );

    let nfs = TapNfs {
        vfs,
        rt: tokio::runtime::Handle::current(),
    };

    let listener = NFSTcpListener::bind(&bind_addr, nfs)
        .await
        .context("failed to bind NFS server")?;

    tracing::info!(port = port, "NFS server listening");

    std::fs::create_dir_all(&config.mount_point)
        .with_context(|| format!("creating mount point {:?}", config.mount_point))?;

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
        tracing::info!(mount_point = %mount_point.display(), "running mount_nfs");

        let result = tokio::process::Command::new("mount_nfs")
            .args(["-o", &mount_opts, "localhost:/", &mount_point.display().to_string()])
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

        // Signal handler
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received signal, unmounting");
        let _ = tokio::process::Command::new("umount")
            .arg(&mount_point)
            .status()
            .await;
        let _ = std::fs::remove_file(&mounts_path);
        std::process::exit(0);
    });

    // Serve forever (this is the main loop)
    listener.handle_forever().await.context("NFS server error")?;

    Ok(())
}

#[cfg(feature = "fuse")]
async fn mount_fuse(vfs: Arc<VirtualFs>, config: &TapConfig) -> Result<()> {
    use crate::fs::tapfs::TapFs;

    tracing::info!(
        mount_point = %config.mount_point.display(),
        "mounting FUSE filesystem"
    );

    let mut options = vec![fuser::MountOption::FSName("tapfs".into())];
    #[cfg(target_os = "macos")]
    {
        options.push(fuser::MountOption::CUSTOM("noappledouble".into()));
        options.push(fuser::MountOption::CUSTOM("noapplexattr".into()));
    }

    let mount_point = config.mount_point.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received signal, unmounting");
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

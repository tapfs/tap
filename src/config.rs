use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapConfig {
    /// Where the FUSE filesystem will be mounted.
    pub mount_point: PathBuf,
    /// Name of the connector to use (e.g. "rest", "salesforce").
    pub connector_name: String,
    /// Path to a single YAML connector spec file.
    pub connector_spec: Option<PathBuf>,
    /// Paths to multiple YAML connector spec files (--specs mode).
    pub connector_specs: Option<Vec<PathBuf>>,
    /// Override the base URL defined in the spec.
    pub base_url: Option<String>,
    /// Cache time-to-live in seconds.
    pub cache_ttl_secs: Option<u64>,
    /// Override the default data directory (~/.tapfs).
    pub data_dir: Option<PathBuf>,
    /// Enable debug-level logging.
    pub debug: bool,
    /// Run as daemon (reads connectors from service.yaml).
    #[serde(default)]
    pub daemon: bool,
}

impl TapConfig {
    /// Return the data directory, falling back to `~/.tapfs`.
    pub fn data_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.data_dir {
            dir.clone()
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".tapfs")
        }
    }

    /// Directory used to persist drafts on disk.
    pub fn drafts_dir(&self) -> PathBuf {
        self.data_dir().join("drafts")
    }

    /// Directory used to persist version snapshots.
    pub fn versions_dir(&self) -> PathBuf {
        self.data_dir().join("versions")
    }

    /// Path to the NDJSON audit log file.
    pub fn audit_log_path(&self) -> PathBuf {
        self.data_dir().join("audit.log")
    }

    /// Path to the mounts status file.
    pub fn mounts_path(&self) -> PathBuf {
        self.data_dir().join("mounts.json")
    }

    /// Path to the Unix domain socket for CLI ↔ mount IPC.
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir().join("tap.sock")
    }
}

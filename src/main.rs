use clap::{Parser, Subcommand};
use std::path::PathBuf;

use tapfs::cli;
use tapfs::config::TapConfig;

#[derive(Parser)]
#[command(
    name = "tap",
    about = "Mount enterprise REST APIs as agent-readable files"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Mount a connector as a FUSE filesystem
    Mount {
        /// Connector name (e.g., "rest", "salesforce")
        connector: String,

        /// Mount point path
        #[arg(short, long, default_value = "/tmp/tap")]
        mount_point: PathBuf,

        /// Path to connector spec YAML
        #[arg(short = 's', long)]
        spec: Option<PathBuf>,

        /// Base URL override
        #[arg(short, long)]
        base_url: Option<String>,

        /// Cache TTL in seconds
        #[arg(long, default_value = "60")]
        cache_ttl: u64,

        /// Data directory override
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Enable debug logging
        #[arg(long)]
        debug: bool,
    },

    /// Unmount a tapfs mount
    Unmount {
        /// Mount point to unmount
        #[arg(default_value = "/tmp/tap")]
        mount_point: PathBuf,
    },

    /// View audit log
    Log {
        /// Max entries to show
        #[arg(short = 'n', long)]
        limit: Option<usize>,

        /// Filter by connector
        #[arg(short, long)]
        connector: Option<String>,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Show mount status
    Status {
        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// List versions of a resource
    Versions {
        /// Path to resource (e.g., /mnt/tap/google/drive/test.md)
        path: PathBuf,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Rollback a resource to a previous version
    Rollback {
        /// Path with version (e.g., /mnt/tap/google/drive/test@v3.md)
        path: PathBuf,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// List pending changes awaiting approval
    Pending {
        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Approve a pending change
    Approve {
        /// Path to resource (e.g., /mnt/tap/google/drive/test.md)
        path: PathBuf,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Show raw API response for a mounted resource
    Inspect {
        /// Path to resource (e.g., /tmp/tap/github/issues/18.md)
        path: PathBuf,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Install a connector from Git/GitHub
    Install {
        /// Source: GitHub shorthand (org/repo), Git URL, or local path
        source: String,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// List installed connectors
    Connectors {
        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Remove an installed connector
    Remove {
        /// Connector name
        name: String,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Update an installed connector
    Update {
        /// Connector name
        name: String,

        /// Data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// Return the default data directory.
/// Checks `TAPFS_DATA` env var first, then falls back to `~/.tapfs`.
fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TAPFS_DATA") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tapfs")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Mount {
            connector,
            mount_point,
            spec,
            base_url,
            cache_ttl,
            data_dir,
            debug,
        } => {
            let config = TapConfig {
                mount_point,
                connector_name: connector,
                connector_spec: spec,
                base_url,
                cache_ttl_secs: Some(cache_ttl),
                data_dir,
                debug,
            };
            cli::mount::run(config).await
        }
        Commands::Unmount { mount_point } => cli::unmount::run(&mount_point),
        Commands::Log {
            limit,
            connector,
            data_dir,
        } => cli::log::run(data_dir.unwrap_or_else(default_data_dir), limit, connector),
        Commands::Status { data_dir } => {
            cli::status::run(data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Versions { path, data_dir } => {
            cli::versions::run_versions(&path, &data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Rollback { path, data_dir } => {
            cli::versions::run_rollback(&path, &data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Pending { data_dir } => {
            cli::approve::run_pending(&data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Approve { path, data_dir } => {
            cli::approve::run_approve(&path, &data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Inspect { path, data_dir } => {
            cli::inspect::run(&path, &data_dir.unwrap_or_else(default_data_dir)).await
        }
        Commands::Install { source, data_dir } => {
            cli::registry::run_install(&source, &data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Connectors { data_dir } => {
            cli::registry::run_list_connectors(&data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Remove { name, data_dir } => {
            cli::registry::run_remove(&name, &data_dir.unwrap_or_else(default_data_dir))
        }
        Commands::Update { name, data_dir } => {
            cli::registry::run_update(&name, &data_dir.unwrap_or_else(default_data_dir))
        }
    }
}

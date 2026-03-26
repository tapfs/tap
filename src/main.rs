use clap::{Parser, Subcommand};
use std::path::PathBuf;

use tapfs::cli;
use tapfs::config::TapConfig;

#[derive(Parser)]
#[command(name = "tap", about = "Mount enterprise REST APIs as agent-readable files")]
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

        /// Environment variable name for auth token
        #[arg(long)]
        auth_token_env: Option<String>,

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
            auth_token_env,
            cache_ttl,
            data_dir,
            debug,
        } => {
            let config = TapConfig {
                mount_point,
                connector_name: connector,
                connector_spec: spec,
                base_url,
                auth_token_env,
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
        } => cli::log::run(
            data_dir.unwrap_or_else(default_data_dir),
            limit,
            connector,
        ),
        Commands::Status { data_dir } => {
            cli::status::run(data_dir.unwrap_or_else(default_data_dir))
        }
    }
}

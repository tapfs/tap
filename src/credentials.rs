//! Credential resolution for connectors.
//!
//! Loads credentials from `~/.tapfs/credentials.yaml` (permissions `0o600`).
//! Falls back to environment variables if no credentials file exists.
//!
//! ```yaml
//! github:
//!   token: ghp_abc123
//! jira:
//!   email: user@company.com
//!   token: ATT_xyz789
//!   base_url: https://myorg.atlassian.net
//! ```

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Credentials for a single connector.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConnectorCredentials {
    pub token: Option<String>,
    pub email: Option<String>,
    pub base_url: Option<String>,
}

/// All credentials keyed by connector name.
#[derive(Debug, Default)]
pub struct CredentialStore {
    entries: HashMap<String, ConnectorCredentials>,
}

impl CredentialStore {
    /// Load credentials from a YAML file. Returns an empty store if the file
    /// doesn't exist (falling back to env vars is handled by the caller).
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("credentials.yaml");
        if !path.exists() {
            return Ok(Self::default());
        }

        // Verify file permissions (owner-only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path)
                .context("reading credentials file metadata")?
                .permissions();
            let mode = perms.mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{:o}", mode),
                    "credentials file has overly permissive permissions — should be 0600"
                );
            }
        }

        let content = std::fs::read_to_string(&path).context("reading credentials file")?;
        let entries: HashMap<String, ConnectorCredentials> =
            serde_yaml::from_str(&content).context("parsing credentials YAML")?;

        tracing::info!(
            count = entries.len(),
            "loaded credentials for {} connector(s)",
            entries.len()
        );

        Ok(Self { entries })
    }

    /// Get credentials for a connector by name.
    pub fn get(&self, connector_name: &str) -> Option<&ConnectorCredentials> {
        self.entries.get(connector_name)
    }

    /// Get the token for a connector, if available.
    pub fn token(&self, connector_name: &str) -> Option<String> {
        self.entries
            .get(connector_name)
            .and_then(|c| c.token.clone())
    }

    /// Get the base_url override for a connector, if available.
    pub fn base_url(&self, connector_name: &str) -> Option<String> {
        self.entries
            .get(connector_name)
            .and_then(|c| c.base_url.clone())
    }
}

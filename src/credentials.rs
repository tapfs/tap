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

use serde::{Deserialize, Serialize};

/// Credentials for a single connector.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectorCredentials {
    pub token: Option<String>,
    pub email: Option<String>,
    pub base_url: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
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

    /// Save a token for a connector to credentials.yaml.
    /// Creates the file if it doesn't exist, preserves existing entries.
    pub fn save_token(data_dir: &Path, connector_name: &str, token: &str) -> Result<()> {
        let path = data_dir.join("credentials.yaml");

        // Load existing entries or start fresh
        let mut entries: HashMap<String, ConnectorCredentials> = if path.exists() {
            let content = std::fs::read_to_string(&path).context("reading credentials file")?;
            serde_yaml::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Update the token
        let entry = entries.entry(connector_name.to_string()).or_default();
        entry.token = Some(token.to_string());

        // Serialize — ConnectorCredentials needs Serialize
        let yaml = serde_yaml::to_string(&entries).context("serializing credentials")?;

        // Ensure parent dir exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&path, yaml).context("writing credentials file")?;

        // Set permissions to 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    /// Save OAuth2 credentials for a connector to credentials.yaml.
    /// Creates the file if it doesn't exist, preserves existing entries.
    pub fn save_oauth2(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<()> {
        let path = data_dir.join("credentials.yaml");

        // Load existing entries or start fresh
        let mut entries: HashMap<String, ConnectorCredentials> = if path.exists() {
            let content = std::fs::read_to_string(&path).context("reading credentials file")?;
            serde_yaml::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Update the entry with all OAuth2 fields
        let entry = entries.entry(connector_name.to_string()).or_default();
        entry.token = Some(token.to_string());
        entry.refresh_token = Some(refresh_token.to_string());
        entry.client_id = Some(client_id.to_string());
        entry.client_secret = Some(client_secret.to_string());

        let yaml = serde_yaml::to_string(&entries).context("serializing credentials")?;

        // Ensure parent dir exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&path, yaml).context("writing credentials file")?;

        // Set permissions to 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

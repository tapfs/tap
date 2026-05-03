//! Declarative search-provider configuration.
//!
//! Loaded from `~/.tapfs/search.toml` (TOML) or `search.yaml` (YAML). Three
//! `kind`s are recognized: `builtin` (in-tree Rust impl), `process` (child
//! process speaking JSON-RPC), and `http` (centralized HTTP backend).
//!
//! `process` and `http` are scaffolded but not yet wired — see the factory.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::search::traits::ScopeKind;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchConfig {
    #[serde(default)]
    pub providers: Vec<ProviderSpec>,
    /// Per-connector overrides keyed by connector name.
    #[serde(default)]
    pub connectors: HashMap<String, ConnectorSearchOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSpec {
    pub name: String,
    pub kind: ProviderKind,
    /// Override the provider's default `scopes()`. If omitted, the
    /// provider's own declaration is used.
    #[serde(default)]
    pub scopes: Option<Vec<ScopeKind>>,
    #[serde(default)]
    pub weight: Option<f32>,

    // process kind
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,

    // http kind
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub auth_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Builtin,
    Process,
    Http,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectorSearchOverride {
    /// Provider names that should not run for this connector's scope.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// If set, only these providers run for this connector. Takes
    /// precedence over `exclude`.
    #[serde(default)]
    pub include_only: Option<Vec<String>>,
}

impl SearchConfig {
    pub fn from_toml(s: &str) -> Result<Self> {
        // We don't have a TOML dependency; parse via serde_yaml as a no-op
        // fallback for now. Real TOML support is a small follow-up; the YAML
        // form is fine for the proposal-level integration.
        Ok(serde_yaml::from_str(s)?)
    }

    pub fn from_yaml(s: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_config() {
        let yaml = r#"
providers:
  - name: upstream
    kind: builtin
    weight: 0.8
  - name: qmd
    kind: process
    command: qmd
    args: ["mcp"]
    scopes: [global, connector, collection, path]
    weight: 1.0
connectors:
  confluence:
    exclude: [upstream]
"#;
        let cfg = SearchConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].kind, ProviderKind::Builtin);
        assert_eq!(cfg.providers[1].kind, ProviderKind::Process);
        assert_eq!(cfg.providers[1].scopes.as_ref().unwrap().len(), 4);
        assert_eq!(
            cfg.connectors.get("confluence").unwrap().exclude,
            vec!["upstream".to_string()]
        );
    }
}

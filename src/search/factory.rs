//! Build a [`SearchRegistry`] from a [`SearchConfig`] plus a connector
//! registry.
//!
//! Only the `builtin` provider kind is wired in this PR. `process` and `http`
//! return `Err(NotSupported)` so the seam is real and follow-up work can land
//! incrementally without breaking compatibility.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::connector::registry::ConnectorRegistry;
use crate::connector::traits::Resource;
use crate::governance::audit::AuditLogger;
use crate::search::builtin::UpstreamSearchProvider;
use crate::search::governance::GovernedSearchProvider;
use crate::search::registry::SearchRegistry;
use crate::search::spec::{ProviderKind, SearchConfig};
use crate::search::traits::{
    ProviderResult, Query, ScopeSet, SearchError, SearchProvider, SearchScope,
};

pub fn build_registry(
    cfg: &SearchConfig,
    connectors: Arc<ConnectorRegistry>,
    audit: Arc<AuditLogger>,
) -> Result<SearchRegistry> {
    let registry = SearchRegistry::new();

    for spec in &cfg.providers {
        let provider = match spec.kind {
            ProviderKind::Builtin => match spec.name.as_str() {
                "upstream" => {
                    let weight = spec.weight.unwrap_or(1.0);
                    let p = UpstreamSearchProvider::new(connectors.clone()).with_weight(weight);
                    Arc::new(p) as Arc<dyn crate::search::traits::SearchProvider>
                }
                other => {
                    return Err(anyhow!(
                        "unknown builtin search provider: {}; available: upstream",
                        other
                    ))
                }
            },
            ProviderKind::Process => Arc::new(UnsupportedSearchProvider::new(
                &spec.name,
                "process",
                spec.weight.unwrap_or(1.0),
            )) as Arc<dyn SearchProvider>,
            ProviderKind::Http => Arc::new(UnsupportedSearchProvider::new(
                &spec.name,
                "http",
                spec.weight.unwrap_or(1.0),
            )) as Arc<dyn SearchProvider>,
        };

        let provider = if let Some(scopes) = &spec.scopes {
            let configured = ScopeSet::only(scopes);
            let effective = provider.scopes().intersection(configured);
            ScopedSearchProvider::wrap(provider, effective)
        } else {
            provider
        };

        let governed = GovernedSearchProvider::wrap(provider, audit.clone());
        registry.register(governed);
    }

    // Apply per-connector excludes.
    for (connector, ov) in &cfg.connectors {
        if let Some(include_only) = &ov.include_only {
            registry.include_only_for_connector(connector, include_only);
        } else {
            for provider in &ov.exclude {
                registry.exclude_for_connector(connector, provider);
            }
        }
    }

    Ok(registry)
}

/// Build the default registry: a single audited `upstream` provider.
pub fn default_registry(
    connectors: Arc<ConnectorRegistry>,
    audit: Arc<AuditLogger>,
) -> SearchRegistry {
    let registry = SearchRegistry::new();
    let upstream = Arc::new(UpstreamSearchProvider::new(connectors));
    registry.register(GovernedSearchProvider::wrap(upstream, audit));
    registry
}

struct ScopedSearchProvider {
    inner: Arc<dyn SearchProvider>,
    scopes: ScopeSet,
}

impl ScopedSearchProvider {
    fn wrap(inner: Arc<dyn SearchProvider>, scopes: ScopeSet) -> Arc<dyn SearchProvider> {
        Arc::new(Self { inner, scopes })
    }
}

#[async_trait]
impl SearchProvider for ScopedSearchProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn scopes(&self) -> ScopeSet {
        self.scopes
    }

    fn weight(&self) -> f32 {
        self.inner.weight()
    }

    async fn query(&self, scope: &SearchScope, q: &Query) -> Result<ProviderResult> {
        self.inner.query(scope, q).await
    }

    async fn on_read(&self, resource: &Resource) -> Result<()> {
        self.inner.on_read(resource).await
    }

    async fn on_write(&self, resource: &Resource) -> Result<()> {
        self.inner.on_write(resource).await
    }
}

struct UnsupportedSearchProvider {
    name: String,
    kind: &'static str,
    weight: f32,
}

impl UnsupportedSearchProvider {
    fn new(name: &str, kind: &'static str, weight: f32) -> Self {
        Self {
            name: name.to_string(),
            kind,
            weight,
        }
    }
}

#[async_trait]
impl SearchProvider for UnsupportedSearchProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn scopes(&self) -> ScopeSet {
        ScopeSet::all()
    }

    fn weight(&self) -> f32 {
        self.weight
    }

    async fn query(&self, _scope: &SearchScope, _q: &Query) -> Result<ProviderResult> {
        Err(SearchError::NotSupported(format!(
            "search provider '{}' has kind={} — not yet implemented; see docs/proposals/search-providers.md",
            self.name, self.kind
        ))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::traits::{Query, SearchScope};

    fn audit_logger() -> Arc<AuditLogger> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(AuditLogger::new(dir.path().join("audit.log")).unwrap())
    }

    #[tokio::test]
    async fn configured_scopes_restrict_provider_eligibility() {
        let cfg = SearchConfig::from_yaml(
            r#"
providers:
  - name: upstream
    kind: builtin
    scopes: [collection]
"#,
        )
        .unwrap();

        let registry =
            build_registry(&cfg, Arc::new(ConnectorRegistry::new()), audit_logger()).unwrap();
        let result = registry
            .query(
                SearchScope::Connector {
                    connector: "github".into(),
                },
                Query::new("cats"),
            )
            .await
            .unwrap();

        assert!(result.hits.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("no search providers eligible")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn connector_include_only_takes_precedence_over_exclude() {
        let cfg = SearchConfig::from_yaml(
            r#"
providers:
  - name: upstream
    kind: builtin
connectors:
  github:
    include_only: [upstream]
    exclude: [upstream]
"#,
        )
        .unwrap();

        let registry =
            build_registry(&cfg, Arc::new(ConnectorRegistry::new()), audit_logger()).unwrap();
        let result = registry
            .query(
                SearchScope::Connector {
                    connector: "github".into(),
                },
                Query::new("cats"),
            )
            .await
            .unwrap();

        assert!(result.hits.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("connector 'github' not registered")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn unsupported_provider_transport_becomes_query_warning() {
        let cfg = SearchConfig::from_yaml(
            r#"
providers:
  - name: qmd
    kind: process
    scopes: [global]
"#,
        )
        .unwrap();

        let registry =
            build_registry(&cfg, Arc::new(ConnectorRegistry::new()), audit_logger()).unwrap();
        let result = registry
            .query(SearchScope::Global, Query::new("cats"))
            .await
            .unwrap();

        assert!(result.hits.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("qmd") && w.contains("not yet implemented")),
            "warnings: {:?}",
            result.warnings
        );
    }
}

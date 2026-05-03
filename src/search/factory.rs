//! Build a [`SearchRegistry`] from a [`SearchConfig`] plus a connector
//! registry.
//!
//! Only the `builtin` provider kind is wired in this PR. `process` and `http`
//! return `Err(NotSupported)` so the seam is real and follow-up work can land
//! incrementally without breaking compatibility.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::connector::registry::ConnectorRegistry;
use crate::governance::audit::AuditLogger;
use crate::search::builtin::UpstreamSearchProvider;
use crate::search::governance::GovernedSearchProvider;
use crate::search::registry::SearchRegistry;
use crate::search::spec::{ProviderKind, SearchConfig};

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
            ProviderKind::Process => {
                return Err(anyhow!(
                    "search provider '{}' has kind=process — not yet implemented; \
                     see docs/proposals/search-providers.md",
                    spec.name
                ));
            }
            ProviderKind::Http => {
                return Err(anyhow!(
                    "search provider '{}' has kind=http — not yet implemented; \
                     see docs/proposals/search-providers.md",
                    spec.name
                ));
            }
        };

        let governed = GovernedSearchProvider::wrap(provider, audit.clone());
        registry.register(governed);
    }

    // Apply per-connector excludes.
    for (connector, ov) in &cfg.connectors {
        for provider in &ov.exclude {
            registry.exclude_for_connector(connector, provider);
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

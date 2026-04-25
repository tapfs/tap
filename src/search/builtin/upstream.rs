//! Built-in `upstream` search provider.
//!
//! Delegates to `Connector::search_resources`, so its quality is exactly the
//! upstream API's own search (GitHub `/search/issues`, Jira JQL, Confluence
//! CQL, etc.). It can never participate in a global query, by design — we
//! don't want to fan a single user query out to every mounted API.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::connector::registry::ConnectorRegistry;
use crate::search::traits::{
    ProviderResult, Query, ScopeKind, ScopeSet, SearchHit, SearchProvider, SearchScope,
};

pub struct UpstreamSearchProvider {
    connectors: Arc<ConnectorRegistry>,
    weight: f32,
}

impl UpstreamSearchProvider {
    pub fn new(connectors: Arc<ConnectorRegistry>) -> Self {
        Self {
            connectors,
            weight: 1.0,
        }
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }
}

#[async_trait]
impl SearchProvider for UpstreamSearchProvider {
    fn name(&self) -> &str {
        "upstream"
    }

    fn scopes(&self) -> ScopeSet {
        ScopeSet::only(&[ScopeKind::Connector, ScopeKind::Collection])
    }

    fn weight(&self) -> f32 {
        self.weight
    }

    async fn query(&self, scope: &SearchScope, q: &Query) -> Result<ProviderResult> {
        let (connector_name, collection) = match scope {
            SearchScope::Connector { connector } => (connector.as_str(), None),
            SearchScope::Collection {
                connector,
                collection,
            } => (connector.as_str(), Some(collection.as_str())),
            // Structurally unreachable because of `scopes()`, but be defensive.
            _ => {
                return Ok(ProviderResult {
                    hits: vec![],
                    warnings: vec!["upstream provider skipped: unsupported scope".into()],
                })
            }
        };

        let connector = self
            .connectors
            .get(connector_name)
            .ok_or_else(|| anyhow!("connector '{}' not registered", connector_name))?;

        let metas = connector.search_resources(collection, &q.text).await?;
        let collection_str = collection.unwrap_or("").to_string();
        let hits: Vec<SearchHit> = metas
            .into_iter()
            .take(q.k)
            .enumerate()
            .map(|(rank, m)| SearchHit {
                tap_path: format!(
                    "/{}/{}/{}",
                    connector_name,
                    collection_str,
                    if m.slug.is_empty() { &m.id } else { &m.slug }
                ),
                connector: connector_name.to_string(),
                collection: collection_str.clone(),
                resource_id: m.id,
                title: m.title,
                snippet: None,
                // Upstream APIs return rank-ordered results, so synthesize a
                // monotonic score for downstream consumers that don't yet
                // call into RRF.
                score: 1.0 / ((rank + 1) as f32),
                provider: "upstream".to_string(),
            })
            .collect();

        Ok(ProviderResult {
            hits,
            warnings: vec![],
        })
    }
}

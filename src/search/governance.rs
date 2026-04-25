//! Audit decorator for search providers.
//!
//! Every provider goes through this wrapper so search activity ends up in the
//! same audit log as reads/writes/lists. Future work: redact content before
//! `on_read` / `on_write` so a centralized vector DB cannot become a leak.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::connector::traits::Resource;
use crate::governance::audit::AuditLogger;
use crate::search::traits::{ProviderResult, Query, SearchProvider, SearchScope, ScopeSet};

pub struct GovernedSearchProvider {
    inner: Arc<dyn SearchProvider>,
    audit: Arc<AuditLogger>,
}

impl GovernedSearchProvider {
    pub fn wrap(inner: Arc<dyn SearchProvider>, audit: Arc<AuditLogger>) -> Arc<dyn SearchProvider> {
        Arc::new(Self { inner, audit })
    }
}

#[async_trait]
impl SearchProvider for GovernedSearchProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn scopes(&self) -> ScopeSet {
        self.inner.scopes()
    }

    fn weight(&self) -> f32 {
        self.inner.weight()
    }

    async fn query(&self, scope: &SearchScope, q: &Query) -> Result<ProviderResult> {
        let connector = scope.connector().unwrap_or("*");
        let collection = scope.collection();
        let provider = self.inner.name().to_string();

        match self.inner.query(scope, q).await {
            Ok(r) => {
                let _ = self.audit.record(
                    "search",
                    connector,
                    collection,
                    None,
                    "success",
                    Some(format!(
                        "provider={} q={:?} hits={}",
                        provider,
                        q.text,
                        r.hits.len()
                    )),
                );
                Ok(r)
            }
            Err(e) => {
                let _ = self.audit.record(
                    "search",
                    connector,
                    collection,
                    None,
                    "error",
                    Some(format!("provider={} q={:?}: {}", provider, q.text, e)),
                );
                Err(e)
            }
        }
    }

    async fn on_read(&self, resource: &Resource) -> Result<()> {
        self.inner.on_read(resource).await
    }

    async fn on_write(&self, resource: &Resource) -> Result<()> {
        self.inner.on_write(resource).await
    }
}

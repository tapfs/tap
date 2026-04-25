//! Parallel fan-out router that dispatches a query to every eligible
//! provider and fuses the results.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;

use crate::search::fusion::rrf_fuse;
use crate::search::traits::{ProviderResult, Query, SearchProvider, SearchScope};

/// Default per-provider deadline if the query doesn't specify one.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

pub struct SearchRegistry {
    providers: DashMap<String, Arc<dyn SearchProvider>>,
    /// Per-connector exclude list: providers in this set will not run when
    /// the scope's connector matches the key.
    excludes: DashMap<String, Vec<String>>,
    /// Per-connector allow list. When present, only these providers run for
    /// the connector; this takes precedence over `excludes`.
    include_only: DashMap<String, Vec<String>>,
}

impl SearchRegistry {
    pub fn new() -> Self {
        Self {
            providers: DashMap::new(),
            excludes: DashMap::new(),
            include_only: DashMap::new(),
        }
    }

    pub fn register(&self, p: Arc<dyn SearchProvider>) {
        self.providers.insert(p.name().to_string(), p);
    }

    pub fn deregister(&self, name: &str) -> bool {
        self.providers.remove(name).is_some()
    }

    /// Exclude `provider` from queries whose scope is `connector` or below.
    pub fn exclude_for_connector(&self, connector: &str, provider: &str) {
        self.excludes
            .entry(connector.to_string())
            .or_default()
            .push(provider.to_string());
    }

    /// Restrict `connector` queries to the given providers. This takes
    /// precedence over `exclude_for_connector`.
    pub fn include_only_for_connector(&self, connector: &str, providers: &[String]) {
        self.include_only
            .insert(connector.to_string(), providers.to_vec());
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.iter().map(|p| p.key().clone()).collect();
        names.sort();
        names
    }

    /// Filter the registered providers down to those eligible for this scope.
    fn eligible(&self, scope: &SearchScope) -> Vec<Arc<dyn SearchProvider>> {
        let kind = scope.kind();
        let excluded: Vec<String> = scope
            .connector()
            .and_then(|c| self.excludes.get(c).map(|v| v.clone()))
            .unwrap_or_default();
        let included: Option<Vec<String>> = scope
            .connector()
            .and_then(|c| self.include_only.get(c).map(|v| v.clone()));

        let mut out: Vec<Arc<dyn SearchProvider>> = self
            .providers
            .iter()
            .filter(|p| p.scopes().contains(kind))
            .filter(|p| {
                if let Some(included) = &included {
                    included.iter().any(|i| i == p.key())
                } else {
                    !excluded.iter().any(|e| e == p.key())
                }
            })
            .map(|p| p.value().clone())
            .collect();
        // Deterministic ordering for tests.
        out.sort_by(|a, b| a.name().cmp(b.name()));
        out
    }

    /// Run `q` against every eligible provider in parallel and return fused
    /// results. Provider failures and timeouts are non-fatal — they bubble
    /// up as warnings.
    pub async fn query(&self, scope: SearchScope, q: Query) -> Result<ProviderResult> {
        let providers = self.eligible(&scope);
        if providers.is_empty() {
            return Ok(ProviderResult {
                hits: vec![],
                warnings: vec![format!(
                    "no search providers eligible for scope {:?}",
                    scope.kind()
                )],
            });
        }

        let deadline = q.deadline.unwrap_or(DEFAULT_DEADLINE);
        let top_k = q.k;

        let mut handles = Vec::with_capacity(providers.len());
        for p in providers {
            let scope = scope.clone();
            let q = q.clone();
            handles.push(tokio::spawn(async move {
                let name = p.name().to_string();
                let weight = p.weight();
                let res = tokio::time::timeout(deadline, p.query(&scope, &q)).await;
                (name, weight, res)
            }));
        }

        let mut per_provider: Vec<(f32, Vec<crate::search::traits::SearchHit>)> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for h in handles {
            match h.await {
                Ok((name, weight, Ok(Ok(r)))) => {
                    for w in r.warnings {
                        warnings.push(format!("{}: {}", name, w));
                    }
                    per_provider.push((weight, r.hits));
                }
                Ok((name, _, Ok(Err(e)))) => {
                    warnings.push(format!("{}: {}", name, e));
                }
                Ok((name, _, Err(_))) => {
                    warnings.push(format!("{}: deadline exceeded", name));
                }
                Err(e) => {
                    warnings.push(format!("join error: {}", e));
                }
            }
        }

        let hits = rrf_fuse(per_provider, top_k);
        Ok(ProviderResult { hits, warnings })
    }
}

impl Default for SearchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::traits::{ScopeKind, ScopeSet, SearchHit};
    use async_trait::async_trait;

    /// Minimal mock provider with configurable behavior.
    struct MockProvider {
        name: String,
        scopes: ScopeSet,
        weight: f32,
        behavior: Behavior,
    }

    enum Behavior {
        Hits(Vec<SearchHit>),
        Error(String),
        Sleep(Duration),
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn scopes(&self) -> ScopeSet {
            self.scopes
        }
        fn weight(&self) -> f32 {
            self.weight
        }
        async fn query(&self, _: &SearchScope, _: &Query) -> Result<ProviderResult> {
            match &self.behavior {
                Behavior::Hits(h) => Ok(ProviderResult {
                    hits: h.clone(),
                    warnings: vec![],
                }),
                Behavior::Error(m) => Err(anyhow::anyhow!(m.clone())),
                Behavior::Sleep(d) => {
                    tokio::time::sleep(*d).await;
                    Ok(ProviderResult::default())
                }
            }
        }
    }

    fn hit(path: &str, provider: &str) -> SearchHit {
        SearchHit {
            tap_path: path.into(),
            connector: "github".into(),
            collection: "issues".into(),
            resource_id: path.into(),
            title: None,
            snippet: None,
            score: 1.0,
            provider: provider.into(),
        }
    }

    #[tokio::test]
    async fn fans_out_to_all_eligible_providers() {
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "p1".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/a", "p1"), hit("/b", "p1")]),
        }));
        reg.register(Arc::new(MockProvider {
            name: "p2".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/c", "p2")]),
        }));

        let r = reg
            .query(SearchScope::Global, Query::new("x"))
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 3);
        assert!(r.warnings.is_empty());
    }

    #[tokio::test]
    async fn skips_providers_that_dont_support_scope() {
        // upstream-style provider: cannot answer Global scope.
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "upstream".into(),
            scopes: ScopeSet::only(&[ScopeKind::Connector, ScopeKind::Collection]),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/oops", "upstream")]),
        }));
        reg.register(Arc::new(MockProvider {
            name: "qmd".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/ok", "qmd")]),
        }));

        let r = reg
            .query(SearchScope::Global, Query::new("x"))
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].tap_path, "/ok");
    }

    #[tokio::test]
    async fn provider_error_becomes_warning() {
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "good".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/g", "good")]),
        }));
        reg.register(Arc::new(MockProvider {
            name: "bad".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Error("backend down".into()),
        }));

        let r = reg
            .query(SearchScope::Global, Query::new("x"))
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("bad"));
        assert!(r.warnings[0].contains("backend down"));
    }

    #[tokio::test]
    async fn slow_provider_times_out_without_blocking_others() {
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "fast".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/f", "fast")]),
        }));
        reg.register(Arc::new(MockProvider {
            name: "slow".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Sleep(Duration::from_secs(60)),
        }));

        let mut q = Query::new("x");
        q.deadline = Some(Duration::from_millis(100));
        let r = reg.query(SearchScope::Global, q).await.unwrap();
        assert_eq!(r.hits.len(), 1);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("slow") && w.contains("deadline")));
    }

    #[tokio::test]
    async fn per_connector_exclude_list_skips_provider() {
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "upstream".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/u", "upstream")]),
        }));
        reg.register(Arc::new(MockProvider {
            name: "qmd".into(),
            scopes: ScopeSet::all(),
            weight: 1.0,
            behavior: Behavior::Hits(vec![hit("/q", "qmd")]),
        }));
        reg.exclude_for_connector("confluence", "upstream");

        let r = reg
            .query(
                SearchScope::Connector {
                    connector: "confluence".into(),
                },
                Query::new("x"),
            )
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].tap_path, "/q");

        // For a different connector both providers should run.
        let r2 = reg
            .query(
                SearchScope::Connector {
                    connector: "github".into(),
                },
                Query::new("x"),
            )
            .await
            .unwrap();
        assert_eq!(r2.hits.len(), 2);
    }

    #[tokio::test]
    async fn empty_eligible_set_returns_warning() {
        let reg = SearchRegistry::new();
        reg.register(Arc::new(MockProvider {
            name: "upstream".into(),
            scopes: ScopeSet::only(&[ScopeKind::Connector]),
            weight: 1.0,
            behavior: Behavior::Hits(vec![]),
        }));
        let r = reg
            .query(SearchScope::Global, Query::new("x"))
            .await
            .unwrap();
        assert!(r.hits.is_empty());
        assert!(!r.warnings.is_empty());
    }
}

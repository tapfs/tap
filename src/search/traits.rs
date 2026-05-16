//! Core types and the `SearchProvider` trait.
//!
//! See `docs/proposals/search-providers.md` for design.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::connector::traits::Resource;

/// Errors specific to the search layer.
#[derive(Debug)]
pub enum SearchError {
    /// Provider does not support the requested scope kind.
    UnsupportedScope(String),
    /// Provider does not support the requested operation.
    NotSupported(String),
    /// Query was malformed.
    InvalidQuery(String),
    /// Backend failure (network, process, etc.).
    Backend(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedScope(m) => write!(f, "unsupported scope: {}", m),
            Self::NotSupported(m) => write!(f, "not supported: {}", m),
            Self::InvalidQuery(m) => write!(f, "invalid query: {}", m),
            Self::Backend(m) => write!(f, "backend error: {}", m),
        }
    }
}

impl std::error::Error for SearchError {}

/// A search scope identifies *what* the user is searching across.
///
/// Providers declare which kinds they answer via [`SearchProvider::scopes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchScope {
    /// No connector specified — fan out across every indexed provider.
    Global,
    /// One connector, all collections.
    Connector { connector: String },
    /// One collection within a connector.
    Collection {
        connector: String,
        collection: String,
    },
    /// A path prefix below a collection (e.g. `repos/foo/`).
    Path {
        connector: String,
        collection: String,
        prefix: String,
    },
}

impl SearchScope {
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Global => ScopeKind::Global,
            Self::Connector { .. } => ScopeKind::Connector,
            Self::Collection { .. } => ScopeKind::Collection,
            Self::Path { .. } => ScopeKind::Path,
        }
    }

    pub fn connector(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Connector { connector }
            | Self::Collection { connector, .. }
            | Self::Path { connector, .. } => Some(connector.as_str()),
        }
    }

    pub fn collection(&self) -> Option<&str> {
        match self {
            Self::Global | Self::Connector { .. } => None,
            Self::Collection { collection, .. } | Self::Path { collection, .. } => {
                Some(collection.as_str())
            }
        }
    }
}

/// One of the four scope kinds a provider may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    Global,
    Connector,
    Collection,
    Path,
}

/// Bitset of scope kinds a provider supports.
///
/// Providers that cannot answer a scope are *structurally* excluded from
/// queries at that scope, e.g. an `upstream` provider that only knows how to
/// hit one API at a time cannot participate in `Scope::Global`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeSet(u8);

impl ScopeSet {
    const GLOBAL: u8 = 1 << 0;
    const CONNECTOR: u8 = 1 << 1;
    const COLLECTION: u8 = 1 << 2;
    const PATH: u8 = 1 << 3;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self(Self::GLOBAL | Self::CONNECTOR | Self::COLLECTION | Self::PATH)
    }

    pub fn only(kinds: &[ScopeKind]) -> Self {
        let mut s = Self::empty();
        for k in kinds {
            s.insert(*k);
        }
        s
    }

    pub fn insert(&mut self, kind: ScopeKind) {
        self.0 |= Self::bit(kind);
    }

    pub fn contains(&self, kind: ScopeKind) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    pub fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    fn bit(kind: ScopeKind) -> u8 {
        match kind {
            ScopeKind::Global => Self::GLOBAL,
            ScopeKind::Connector => Self::CONNECTOR,
            ScopeKind::Collection => Self::COLLECTION,
            ScopeKind::Path => Self::PATH,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryMode {
    /// Keyword/BM25-style.
    Lexical,
    /// Embedding/vector.
    Semantic,
    /// Both, fused (default).
    #[default]
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub k: usize,
    pub deadline: Option<Duration>,
    pub mode: QueryMode,
    pub filters: BTreeMap<String, String>,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            k: 20,
            deadline: Some(Duration::from_secs(10)),
            mode: QueryMode::Hybrid,
            filters: BTreeMap::new(),
        }
    }
}

/// One search result row.
///
/// `tap_path` is the canonical mount-relative path (e.g.
/// `/github/issues/4521`) — the agent reads the actual content by `cat`-ing
/// through the FUSE mount, never from the hit itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub tap_path: String,
    pub connector: String,
    pub collection: String,
    pub resource_id: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub score: f32,
    /// Which provider produced this hit (for debugging / mixed-source UI).
    pub provider: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderResult {
    pub hits: Vec<SearchHit>,
    /// Non-fatal warnings from this provider (e.g. partial result, dropped
    /// filter, deprecated field). Surfaced to the user but do not fail the
    /// query.
    pub warnings: Vec<String>,
}

/// A pluggable search backend.
///
/// Implementations might be: a built-in pass-through to a connector's own
/// `/search` endpoint; a child process speaking JSON-RPC (e.g. QMD); an HTTP
/// call to a centralized vector DB; or an in-process Rust impl.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Which scope kinds this provider answers. The router will not call
    /// `query` for any scope outside this set.
    fn scopes(&self) -> ScopeSet;

    /// Multiplier applied to this provider's RRF contributions. Defaults to
    /// `1.0`. Bump it for backends you trust more.
    fn weight(&self) -> f32 {
        1.0
    }

    async fn query(&self, scope: &SearchScope, q: &Query) -> Result<ProviderResult>;

    /// Optional ingestion hook fired when the VFS reads a resource. Indexed
    /// providers may use this to incrementally update their index. Forward-
    /// only providers should leave the default no-op.
    async fn on_read(&self, _resource: &Resource) -> Result<()> {
        Ok(())
    }

    /// Optional ingestion hook fired when a draft is promoted / written.
    async fn on_write(&self, _resource: &Resource) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_kind_extraction() {
        assert_eq!(SearchScope::Global.kind(), ScopeKind::Global);
        assert_eq!(
            SearchScope::Connector {
                connector: "github".into()
            }
            .kind(),
            ScopeKind::Connector,
        );
        assert_eq!(
            SearchScope::Collection {
                connector: "github".into(),
                collection: "issues".into(),
            }
            .kind(),
            ScopeKind::Collection,
        );
    }

    #[test]
    fn scope_accessors() {
        let s = SearchScope::Collection {
            connector: "github".into(),
            collection: "issues".into(),
        };
        assert_eq!(s.connector(), Some("github"));
        assert_eq!(s.collection(), Some("issues"));

        let g = SearchScope::Global;
        assert_eq!(g.connector(), None);
        assert_eq!(g.collection(), None);
    }

    #[test]
    fn scope_set_membership() {
        let s = ScopeSet::only(&[ScopeKind::Connector, ScopeKind::Collection]);
        assert!(s.contains(ScopeKind::Connector));
        assert!(s.contains(ScopeKind::Collection));
        assert!(!s.contains(ScopeKind::Global));
        assert!(!s.contains(ScopeKind::Path));

        assert!(ScopeSet::all().contains(ScopeKind::Global));
        assert!(!ScopeSet::empty().contains(ScopeKind::Global));
    }

    #[test]
    fn scope_set_intersection() {
        let a = ScopeSet::only(&[ScopeKind::Global, ScopeKind::Connector]);
        let b = ScopeSet::only(&[ScopeKind::Connector, ScopeKind::Collection]);
        let both = a.intersection(b);

        assert!(both.contains(ScopeKind::Connector));
        assert!(!both.contains(ScopeKind::Global));
        assert!(!both.contains(ScopeKind::Collection));
    }
}

//! Pluggable search providers and a fan-out router.
//!
//! See `docs/proposals/search-providers.md` for design and rationale.
//!
//! The shape, in one paragraph: any backend (a local index like QMD, a shared
//! vector DB, an upstream API's own `/search`, …) implements
//! [`SearchProvider`]. Providers self-declare which scope kinds they answer
//! via [`ScopeSet`], so e.g. an `upstream` provider that needs a specific
//! connector cannot be asked to handle a `Global` query. The
//! [`SearchRegistry`] fans every query out to all eligible providers in
//! parallel under a deadline, fuses results via Reciprocal Rank Fusion, and
//! deduplicates by canonical tap path. Provider failures and timeouts are
//! non-fatal — they bubble up as warnings.

pub mod builtin;
pub mod factory;
pub mod fusion;
pub mod governance;
pub mod registry;
pub mod spec;
pub mod traits;

pub use registry::SearchRegistry;
pub use traits::{
    ProviderResult, Query, QueryMode, ScopeKind, ScopeSet, SearchError, SearchHit, SearchProvider,
    SearchScope,
};

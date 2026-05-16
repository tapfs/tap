# Pluggable search providers

Status: initial implementation
Tracking branch: `claude/integrate-qmd-search-uKO02`

## Why

`tap` mounts heterogeneous APIs as files. Agents using the mount today fall back
to `grep -r` because there is no `tap search` and no index. We want search, but
the right search engine depends on the user: a developer on a laptop wants
something local like [QMD](https://github.com/tobi/qmd); a team wants a shared
vector DB; a thin deployment wants to forward queries to the upstream API
(GitHub's `/search/issues`, Jira JQL, Confluence CQL). Baking in any one of
those is the wrong shape.

This proposal adds a thin plugin seam — `SearchProvider` — and a router that
fans queries out across multiple providers in parallel and fuses the results.
**No specific backend ships in this PR.** A trivial `upstream` provider is the
only built-in, exclusively for connectors that already declare
`capabilities.search` in their spec.

## Non-goals

- No embeddings, no SQLite index, no vector DB, no LLM rerank.
- No QMD adapter. QMD is meant to plug in via the same trait at a later date,
  out of tree if desired.
- No write-side ingestion in this PR; the trait has the hooks but they are
  no-ops.

## Shape

Three pieces, each ~150 LOC:

```
src/search/
  traits.rs      SearchProvider, SearchScope, Query, SearchHit, ScopeSet
  fusion.rs      Reciprocal Rank Fusion
  registry.rs    parallel fan-out + fusion + dedup + per-scope filtering
  governance.rs  GovernedSearchProvider — audits every query
  builtin/
    upstream.rs  delegates to Connector::search_resources
```

### The trait

```rust
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    fn scopes(&self) -> ScopeSet;          // which scopes this provider answers
    fn weight(&self) -> f32 { 1.0 }
    async fn query(&self, scope: &SearchScope, q: &Query) -> Result<ProviderResult>;
    async fn on_read(&self, _: &Resource) -> Result<()> { Ok(()) }   // optional
    async fn on_write(&self, _: &Resource) -> Result<()> { Ok(()) }  // optional
}
```

The key detail: `scopes()` is the structural gate. The `upstream` provider
declares `{Connector, Collection}` and is therefore *unable* to participate in
`Scope::Global` — no config flag required. A QMD provider would declare all
four scope kinds and answer everything.

### Scopes

```rust
pub enum SearchScope {
    Global,
    Connector  { connector: String },
    Collection { connector: String, collection: String },
    Path       { connector: String, collection: String, prefix: String },
}
```

`tap search "cats"` → `Global`. `tap search -t github "cats"` →
`Connector`. `tap search -t github/issues "cats"` → `Collection`.

### The router

```text
query(scope, q):
    eligible  = providers.filter(p => p.scopes().contains(scope.kind()))
                          .filter(not in connector exclude list)
    results   = parallel_join(p.query(scope, q) with deadline)   // failures non-fatal
    fused     = rrf(results, weights)
    deduped   = collapse_by_tap_path(fused)
    return deduped[..q.k] + warnings
```

Reciprocal Rank Fusion is the right primitive here precisely because providers'
scores aren't comparable: BM25, cosine, and a remote API's relevance score live
on different scales, but ranks fuse cleanly. Each provider contributes
`weight / (k + rank + 1)` to a doc's fused score, so a `corp-pinecone` provider
with `weight = 1.5` outranks a `weight = 1.0` upstream provider on the same
position.

Dedup keys on the canonical tap path (`/<connector>/<collection>/<slug>`) so a
hit returned by both QMD and Pinecone collapses into one row with combined
score and the best snippet.

### Failure & latency

Each provider call inherits the query's deadline (`tokio::time::timeout`). A
slow Pinecone doesn't block QMD's hits — its results just don't make it in,
and the reason bubbles up in `ProviderResult.warnings`. Only an empty merged
set with all-providers-failed becomes a user-visible error.

### Governance

Every registered provider is wrapped in `GovernedSearchProvider`, which audits
queries through the existing `AuditLogger`. Future work: redact the materialized
content before it reaches `on_read` / `on_write` so a centralized vector DB
shared across agents cannot become a secrets leak.

### Configuration

Providers come from `~/.tapfs/search.yaml`:

```yaml
providers:
  - name: qmd
    kind: process            # child process transport, not wired yet
    command: qmd
    args: ["mcp"]
    scopes: [global, connector, collection, path]
    weight: 1.0
  - name: corp-pinecone
    kind: http               # HTTP transport, not wired yet
    endpoint: https://search.internal/v1/query
    auth_env: PINECONE_TOKEN
    scopes: [global, connector]
    weight: 1.5
  - name: upstream
    kind: builtin            # delegates to Connector::search_resources
    weight: 0.8

connectors:
  confluence:
    exclude: [upstream]      # confluence's CQL search is poor; skip it
```

`include_only` is also available per connector and takes precedence over
`exclude`. The `process` and `http` provider kinds are scaffolded as
warning-producing `NotSupported` providers in this PR so mixed configs remain
usable while their wire protocols are left for a follow-up.

## What changes in the connector layer

Just one method, with a default impl:

```rust
async fn search_resources(
    &self,
    _collection: Option<&str>,
    _query: &str,
) -> Result<Vec<ResourceMeta>> {
    Err(ConnectorError::NotSupported(...).into())
}
```

No existing connector needs to implement it. As we add `search_endpoint:`
declarations to `CollectionSpec`, `RestConnector::search_resources` will pick
them up — that's a follow-up PR.

## CLI

```text
tap search "cats"                       # global, fans out to indexed providers
tap search -t github "cats"             # all eligible + upstream
tap search -t github/issues "cats"      # narrower
tap search --json --limit 20 ...
```

## What ships in this PR

- `SearchProvider` trait, `SearchScope`, `Query`, `SearchHit`, `ScopeSet`
- RRF fusion with weights and dedup-by-path
- `SearchRegistry` with parallel fan-out, deadlines, partial-failure tolerance
- `GovernedSearchProvider` audit decorator
- `upstream` built-in provider
- `Connector::search_resources` default impl returning `NotSupported`
- `tap search` CLI
- Unit tests for fusion, scope filtering, dedup, fan-out, partial failure

## What does not ship

- No QMD, no Pinecone, no embeddings, no vector store
- `process` and `http` provider transports are recognized but return warnings
- No `.search/<query>` virtual VFS path (follow-up)
- No `search_resources` impl on `RestConnector` yet (follow-up)

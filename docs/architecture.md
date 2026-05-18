# tapfs Architecture

A reference for what tapfs *is*, where the pieces live, and the handful
of design decisions that aren't obvious from `grep`. Skim this first; the
codebase will feel smaller.

> **Maintainer's note:** keep this in sync with the code. Any PR that
> changes module boundaries, adds a NodeKind, changes the draft state
> machine, or introduces a new cross-cutting concern (rate limit, retry
> shape, error variant) should update the relevant section here.
> CLAUDE.md links here as the single source of truth for architecture.

## What tapfs is

tapfs mounts SaaS APIs as a filesystem. Reads return rendered markdown,
writes flow back to the API. The mount is served over NFSv3 (macOS) or
FUSE (Linux); both transports are thin adapters over a shared platform-
agnostic VFS layer.

The supported transport surface is intentionally just NFS/FUSE plus the
`tap` CLI. The earlier macOS File Provider prototype and its Rust C FFI
bridge were removed so unsupported Finder-specific code does not drift from
the production NFS path.

The product bet: **filesystems are the universal API for LLM agents**.
Any tool that reads and writes files (cat, vim, `Edit`, etc.) becomes a
GitHub/Jira/Notion client.

## High-level data flow

```
   NFS client (kernel)        FUSE client (kernel)
           │                          │
   src/nfs/server.rs            src/fs/...
   (TapNfs adapter)             (FUSE adapter)
           │                          │
           └────────────┬─────────────┘
                        ▼
                src/vfs/core.rs
                 (VirtualFs)
                        │
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
  src/draft/        src/cache/       src/connector/
   (write           (read-side       (HTTP client per
    buffer +         memo: 60s        SaaS, async)
    on-disk         in-memory +
    drafts)         disk L2)
                        │
                        ▼
                  reqwest → API
```

Reads: NFS/FUSE → VirtualFs → cache (memo) → connector → API → render →
cache → return.

Writes: NFS WRITE → VirtualFs::write (in-memory buffer) → on close/flush
→ draft store on disk → if not `_draft: true`, auto-promote → connector
→ POST/PATCH → cache invalidate.

## Module map

| Path | LoC | What it owns |
|---|---|---|
| `src/vfs/core.rs` | 4655 | The VFS. **God-object** — split planned in `docs/proposals/vfs-core-split.md`. |
| `src/vfs/types.rs` | 134 | `VfsAttr`, `VfsDirEntry`, `NodeKind`, `VfsError`, `From<anyhow::Error>` |
| `src/nfs/server.rs` | 403 | `TapNfs` — implements `nfsserve::NFSFileSystem`, marshals between fattr3 and VfsAttr |
| `src/fs/...` | — | FUSE adapter (Linux). Same shape as NFS, different transport. |
| `src/connector/traits.rs` | 122 | `Connector` async trait + `ConnectorError` typed errors |
| `src/connector/rest.rs` | 1547 | Spec-driven generic REST connector (~80% of integrations) |
| `src/connector/spec.rs` | 311 | YAML spec types: `ConnectorSpec`, `CollectionSpec`, `RenderSpec`, etc. |
| `src/connector/{jira,google,confluence}.rs` | 4000+ | Native connectors with API-specific quirks the spec can't express |
| `src/connector/atlassian_auth.rs` | 424 | Shared Basic-auth helper for Jira + Confluence |
| `src/connector/factory.rs` | 151 | Decides which connector impl to construct from a name |
| `src/connector/registry.rs` | 71 | Runtime map of name → `Arc<dyn Connector>` |
| `src/connector/retry.rs` | — | Shared `execute(&RetryPolicy, Fn() -> RequestBuilder)` (PR10) |
| `src/search/...` | — | Pluggable search-provider seam: scopes, provider registry, RRF fusion, audited upstream provider, and config factory |
| `src/credentials.rs` | 528 | OS keychain (primary) + `~/.tapfs/credentials.yaml` index for non-secret metadata |
| `src/draft/store.rs` | — | Per-resource on-disk drafts. The persistence backbone of writes. |
| `src/cache/{store,disk}.rs` | — | L1 in-memory (60s TTL) + L2 on-disk. Read-only memo. |
| `src/version/store.rs` | — | Optional per-resource version history for connectors that support it |
| `src/governance/audit.rs` | — | Append-only audit log of reads, writes, deletes, searches, and connector operations |
| `src/cli/{mount,auth,search,...}.rs` | — | `tap` CLI — daemon mode, interactive auth, mount management, search |
| `src/ipc.rs` | — | Unix socket so `tap mount X` can hot-add a connector to a running daemon |
| `src/main.rs` | — | Binary entry point |

## Core abstractions

### `Connector` trait (`src/connector/traits.rs`)

The minimum shape every backend implements:

```rust
async fn list_collections() -> Result<Vec<CollectionInfo>>;
async fn list_resources(collection: &str) -> Result<Vec<ResourceMeta>>;
async fn list_resources_with_shards(collection)
    -> Result<Vec<(ResourceMeta, Option<serde_json::Value>)>>;  // default: None shards
async fn read_resource(collection: &str, id: &str) -> Result<Resource>;
async fn write_resource(collection, id, content) -> Result<()>;
async fn create_resource(collection, content) -> Result<ResourceMeta>;
async fn delete_resource(collection, id) -> Result<()>;
async fn resource_versions(collection, id) -> Result<Vec<VersionInfo>>;
async fn read_version(collection, id, version) -> Result<Resource>;
async fn search_resources(collection, query) -> Result<Vec<ResourceMeta>>; // default: NotSupported
```

`list_resources_with_shards` is the hook for the frontmatter shard cache
(see [Frontmatter shards](#frontmatter-shards) below). The default impl
just pairs each `ResourceMeta` with `None`, so connectors that don't
care behave exactly as before. `RestConnector` overrides it to project
the spec's `populates` fields out of each list-response item.

`search_resources` is the narrow upstream-search hook used by the built-in
search provider. Connectors that have a native search API can override it;
the default returns `ConnectorError::NotSupported`, which the search registry
surfaces as a warning instead of failing the whole query.

Errors are returned as `anyhow::Error`. The VFS layer downcasts to
`ConnectorError` for typed mapping (NotFound, PermissionDenied,
RateLimited{retry_after}, NetworkError, NotSupported).

### Search providers (`src/search`)

`tap search` is implemented outside the VFS as a pluggable fan-out layer.
The CLI builds a `SearchRegistry` from `~/.tapfs/search.yaml`, or a default
registry containing only the audited `upstream` provider when no config exists.

Core types:

| Type | Role |
|---|---|
| `SearchScope` | Query target: `Global`, `Connector`, `Collection`, or `Path` |
| `SearchProvider` | Backend seam for local indexes, HTTP/vector services, process adapters, or upstream API search |
| `ScopeSet` | Provider-declared scope kinds; the registry will not call a provider outside this set |
| `SearchRegistry` | Filters eligible providers, applies per-connector `include_only`/`exclude`, runs queries in parallel under a deadline, and fuses results |
| `rrf_fuse` | Reciprocal Rank Fusion over provider-ranked result lists, deduped by canonical `tap_path` |

Provider failures and timeouts are non-fatal: they become warnings on
`ProviderResult`. The configured `process` and `http` provider kinds are
recognized but not wired yet; they register as warning-producing
`NotSupported` providers so mixed configs keep working while those transports
are developed. The built-in `upstream` provider only supports connector and
collection scopes and delegates to `Connector::search_resources`.

### `NodeKind` (`src/vfs/types.rs`)

The discriminator for what a node *is*. Drives all dispatch in
`VirtualFs`:

| Variant | What it represents |
|---|---|
| `Root` | `/` |
| `Connector { name }` | `/github/` |
| `Collection { connector, collection }` | `/github/issues/` |
| `Resource { connector, collection, resource, variant }` | `/github/issues/42.md` |
| `ResourceDir { connector, collection, resource }` | `/github/repos/tapfs/` (a resource that's *also* a directory because the spec declares subcollections) |
| `GroupDir { connector, collection, group_value }` | `/github/repos/tapfs-org/` (synthetic group from `group_by` in spec) |
| `Version { connector, collection, resource, version_id }` | `/github/issues/42.versions/3.md` |
| `AgentMd`, `ConnectorAgentMd`, `CollectionAgentMd` | `AGENTS.md` at root / connector / collection levels — generated on first read, then writable. User edits are buffered like normal writes and persisted to `<data_dir>/agents-md/` on flush, overlaying the generated content on subsequent reads (and surviving daemon restarts). |
| `TxDir`, `Transaction`, `TxResource` | Transaction directory + named transactions for atomic multi-write workflows |
| `ResourceVariant` | `Live`, `Draft` (`.draft.md`), or `Lock` (`.lock`) |

Most readers get to a node via `lookup` → `getattr`/`read`/`readdir`.
Each NodeKind has its own `resolve_*_child` method in `core.rs`.

### Draft state machine (frontmatter-driven)

Every resource file (markdown) has YAML frontmatter that tapfs interprets.
The state machine for promotion to API:

| `_draft` | `_id` | What flush does |
|---|---|---|
| `true` | any | **Skip** — kept local |
| absent/false | empty | **POST** (new resource) |
| absent/false | `__creating__@<ts>` (fresh) | **Skip** (concurrent flush in flight) |
| absent/false | `__creating__@<ts>` (>5 min) | **Retry POST** (PR8 — daemon crashed mid-flight) |
| absent/false | `<real id>` | **PATCH** (update existing) |

After a successful POST, replace `__creating__@<ts>` with the real id
and increment `_version`.

`_idempotency_key: tapfs-<nanos>-<counter>` (PR7) is seeded at `mkdir`
and sent as an HTTP header on POST so retried creates don't duplicate.

## Cross-cutting concerns

### Error types

- `ConnectorError` — typed at the connector boundary (`traits.rs:11`).
- `VfsError` — typed at the VFS boundary (`types.rs:107`). Variants include
  `Busy`, `RateLimited(Duration)`, `StaleHandle`, `NoSpace`,
  `PartialFlush(String)`, `DraftCorrupted(String)`, plus the classics
  (NotFound, NotDirectory, AlreadyExists, etc.). Adding a new failure
  mode? Add a variant — don't collapse into `IoError(String)`.
- **Two adapters, one mapping**: `nfs/server.rs::vfs_err_to_nfs` →
  `nfsstat3`, and `fs/tapfs.rs::to_errno` → libc errno. **Both must be
  exhaustive**. Local `--no-default-features --features nfs` builds
  only exercise the NFS adapter; CI's Linux job builds with FUSE on
  and will catch a missed FUSE arm. When adding a `VfsError` variant,
  update **both** files in the same PR.

### Lock discipline

`VirtualFs` uses `DashMap` (sharded locking, lock-free reads) for
`write_buffers`, `nodes`, `slug_map`, `resource_mtimes`,
`content_lengths`. Two rules (full doc-comment in `vfs/core.rs:1`):

1. **Never hold a `DashMap` entry across `block_on(...)`.** `flush()` uses
   `write_buffers.remove()`, which returns the value by ownership and
   drops the lock before the connector call. Don't change this.
2. **Snapshot before mutate.** When seeding a buffer from disk, read the
   disk first (no shard lock), then take the entry only for the in-memory
   mutation. See `buffer_write` for the canonical shape.

### Retry + idempotency

- All HTTP retry goes through `connector/retry.rs::execute(&RetryPolicy, F)`.
  Retries on 429/502/503 and transient network errors. Honors `Retry-After`.
  Caller owns auth and token-refresh decisions.
- POSTs are idempotent via `_idempotency_key` from frontmatter (PR7) +
  the `idempotency_key_header` field in `CollectionSpec`.

### Backpressure

- `TapNfs` has a per-instance `Semaphore` (default 64 permits, override
  via `TAPFS_MAX_CONCURRENT_REQUESTS`). Each handler takes a permit; on
  exhaustion returns `NFS3ERR_JUKEBOX` so the kernel backs off.
- `RestConnector` has its own per-instance `Semaphore` (10 permits) for
  outbound HTTP. Held per-request (not per-attempt).

### Frontmatter shards

REST APIs typically return shallow objects on `/collection` (list) and
the full object on `/collection/:id` (detail). An agent doing
`grep priority: P0 issues/*/index.md` over 500 issues would, naively,
fire 500 detail GETs — the N+1 problem. The shard cache exists to make
those shallow scans free.

**Spec field** (`CollectionSpec.populates`):

```yaml
collections:
  - name: issues
    list_endpoint: /repos/{owner}/{repo}/issues
    get_endpoint:  /repos/{owner}/{repo}/issues/{id}
    populates:                       # what the list endpoint already returns
      - title
      - state
      - "user.login as author"       # same dot-path / alias syntax as render.frontmatter
```

**Flow:**

1. `readdir` on a collection calls `get_resources_cached`
   (`src/vfs/core.rs:1230`), which delegates to
   `Connector::list_resources_with_shards`.
2. `RestConnector` (`src/connector/rest.rs`) does one GET against
   `list_endpoint`, then for each item projects the `populates` fields
   into a `serde_json::Value` shard.
3. `get_resources_cached` writes each shard into `Cache::shards`
   (`src/cache/store.rs`) keyed by `format!("{connector}/{collection}/{id}")`.

**Status:** the plumbing above lands the shards in cache. The read-path
consumption — serving `index.md` from the shard when L1/L2 are cold and
the shard is hot — is intentionally deferred to a follow-up. Doing so
correctly requires deciding *how an agent reaches the body once the
shard is hot* (separate `body.md` vs lazy-promotion vs explicit
`tap fetch`); the design intent is to settle that in its own PR rather
than fold it into this one.

**Tradeoff to be aware of:** shards are sized by what the list endpoint
returns, not by what fits in memory. A 1k-issue collection with rich
list responses (GitHub returns ~3 KB per item) costs ~3 MB of in-memory
cache. If this becomes a problem, gate shard storage by spec
declaration (already implicit: no `populates` → no shard) or add a size
cap analogous to `MAX_CACHEABLE_SIZE`.

### Credentials

- Secrets (`token`, `refresh_token`, `client_secret`) → OS keychain
  (`KEYCHAIN_SERVICE = "tapfs"`, user = connector name).
- Non-secrets (`email`, `base_url`, `client_id`, `expires_at`) →
  `~/.tapfs/credentials.yaml` (mode 0600, atomic write via tempfile + rename).
- `TAPFS_NO_KEYCHAIN=1` puts secrets back in YAML for headless / CI.
- `TAPFS_STRICT_PERMS=1` makes loose-perms credentials.yaml a hard error.
- `Debug` impl on `ConnectorCredentials` redacts secret fields — safe
  to `tracing::debug!(?creds)`.

### Auth flows (which one runs)

The spec's `auth.type` selects the flow `handle_auth_required` dispatches
to when a mount needs credentials and none are stored:

| `auth.type` | Flow | When to use |
|---|---|---|
| `bearer` | `prompt_api_key` — stdin prompt | Static tokens (Stripe, Linear) |
| `basic` | `prompt_api_key` (paste user:pass) | Legacy basic-auth APIs |
| `oauth2` + `device_code_url` | `oauth2_device_flow` — print code, poll | GitHub-style device flow |
| `oauth2` + `auth_url` + `client_secret` | `oauth2_browser_flow` — confidential client | Google Workspace, server-to-server flows |
| `oauth2_pkce` + `auth_url` | `oauth2_pkce_browser_flow` — public client, no secret | X v2, any provider requiring PKCE |

PKCE (RFC 7636) is the right flow for user-context auth on public clients
— CLI / desktop apps that can't safely embed a `client_secret`. The math
(verifier generation, S256 challenge, callback parsing, token-exchange and
refresh form bodies) lives in `src/cli/pkce.rs` as pure functions so each
piece tests in isolation. The browser-and-listener half lives next to the
existing `oauth2_browser_flow` in `src/cli/auth.rs`. Storage goes through
`CredentialStore::save_oauth2_pkce` — which records `expires_at` and
explicitly stores `client_secret: None`, so a daemon restart after the
access token's lifetime correctly triggers a refresh via `ensure_token`
instead of hitting 401 on first call.

The refresh path in `RestConnector::ensure_token` is shared between
confidential and PKCE flows: it dispatches on `OAuth2Config.client_secret`
being `Some` vs `None` and omits the form param accordingly. **Critical
invariant: never send `client_secret` on a PKCE refresh** — empty-string
will fail invalid_client on some providers, and a stale secret leaks
unrelated credentials.

## Spec-driven REST connector

`CollectionSpec` (in `src/connector/spec.rs`) declares enough to drive
HTTP for ~80% of APIs without bespoke code:

```yaml
collections:
  - name: issues
    list_endpoint: /repos/{owner}/{repo}/issues
    get_endpoint: /repos/{owner}/{repo}/issues/{id}
    create_endpoint: /repos/{owner}/{repo}/issues
    delete_endpoint: /repos/{owner}/{repo}/issues/{id}
    delete_body: '{"archived": true}'              # for soft-delete APIs
    idempotency_key_header: Idempotency-Key       # for safe POST retry
    title_field: title
    render:
      frontmatter: [title, state, "html_url as url"]
      body: body
      sections: [{ name: comments, ... }]
    populates: [title, state, "user.login as author"]  # cached from list response
    subcollections: [...]                          # nested collection tree
    parent_param: repo                             # URL placeholder name
    aggregate: true                                # single-file append-only view
    group_by: owner.login                          # synthetic GroupDir
```

`RestConnector` walks path-encoded collection names like
`"repos/tapfs/issues"` (alternating collection-name / resource-id) via
`resolve_nested_collection`, substituting parent IDs into endpoint
placeholders.

Native connectors (`jira.rs`, `google.rs`, `confluence.rs`) exist where
the spec model isn't enough (auth quirks, GraphQL, multi-step workflows).
Most have a `send_with_retry` of their own — these will migrate to the
shared `connector/retry.rs` helper once PR10b lands.

## NFS protocol invariants (load-bearing)

These are sharp edges that took packet captures to find. Don't regress:

1. **`nfsserve` always replies WRITE with `committed: FILE_SYNC`**, so
   macOS never sends COMMIT. **`vfs.flush()` must be called inside
   `TapNfs::write`** — it's the only flush trigger on the NFS path. (FUSE
   uses `release`/`flush` callbacks instead.)
2. **`fattr3.mode` must include the Unix file-type bits** (`S_IFDIR | perm`
   for directories, `S_IFREG | perm` for files). Without them macOS
   Sequoia's NFS client fails `S_ISDIR()` checks and returns EPERM.
3. **Synthetic nodes return mtime=0** (Root, Connector, Collection,
   AgentMd, etc.) — *not* the wall clock. A changing mtime causes macOS
   to constantly re-validate directory attributes and eventually return
   EPERM.
4. **After successful `create_resource`**, populate `cache.put_resource`
   so subsequent flushes use `write_resource` (PATCH) instead of POST.
5. **`rm -rf` on a `ResourceDir`**: virtual children (`index.md`,
   `comments.md`, `AGENTS.md`) must return `Ok(())` from `unlink`
   unconditionally. The API delete gate is in `rmdir_resource_dir`,
   triggered by the final RMDIR. If any earlier REMOVE returns EPERM,
   the whole operation aborts before reaching RMDIR.
6. **`nfstime3.seconds` is u32**. `clamp_to_u32_seconds` (PR14) prevents
   2038 truncation; pre-1970 → 0, post-2106 → u32::MAX.
7. **READDIR cookie validity**: NFSv3 paginates by passing back the
   fileid of the last entry. If the directory shifted between calls,
   that fileid may be gone. `nfs/server.rs::readdir` walks first to
   detect the missing-cookie case and returns `NFS3ERR_BAD_COOKIE` so
   the client restarts.
8. **GETATTR `size` must equal what READ produces**, byte-for-byte. The
   macOS NFS client honors the GETATTR-reported size and zero-pads READ
   responses up to that length when the server returns less. Files that
   look like text in `cat` then trip `grep`'s binary heuristic and are
   skipped. The synthetic `AGENTS.md` nodes used to hardcode `size: 4096`
   and render content lazily on read; they now pre-render in `lookup`
   and the readdir paths and cache the bytes in
   `VirtualFs::agent_md_cache` so `kind_to_attr` reports the actual
   length. Same rule applies to any future synthetic node — never use a
   placeholder size larger than the smallest possible payload.

## Testing patterns

- Unit tests live in `#[cfg(test)] mod tests` blocks at the bottom of
  each module. `vfs/core.rs` has 4 nested test modules — see
  `docs/proposals/vfs-core-split.md` for the planned reorganization.
- `tempfile::tempdir()` for all disk-backed stores.
- Sync tests on the VFS use `Builder::new_current_thread().enable_all().build()`
  and pass `rt.handle()` to VFS methods. **Don't use `#[tokio::test]`** —
  VFS methods are sync, calling `Handle::block_on` internally.
- Async tests on connectors use `#[tokio::test]` with `wiremock` for the
  HTTP server. See `connector/retry.rs::tests` for the canonical shape.
- Mock connectors implement the full `Connector` trait inline. Use
  `AtomicUsize` to count API calls and assert exact invocation counts —
  that's how to verify idempotency.
- NFS attribute tests call `vfs_attr_to_fattr` directly; use `rt.enter()`
  to satisfy the tokio handle requirement for `TapNfs::new`.

## Where to look first

| If you're working on… | Start with |
|---|---|
| A new SaaS integration | `connectors/<name>.yaml` (spec-driven) or `src/connector/<name>.rs` (native) |
| Rendering / frontmatter | `src/connector/rest.rs::markdown_to_json`, `src/vfs/core.rs::parse_tapfs_meta` |
| A new VfsError variant | `src/vfs/types.rs:107` then `src/nfs/server.rs::vfs_err_to_nfs` |
| Draft / promotion logic | `src/vfs/core.rs::flush` (~line 1278), `make_sentinel`, `classify_sentinel` |
| Cache invalidation | `src/cache/store.rs` + every `cache.invalidate(...)` callsite |
| HTTP retry behavior | `src/connector/retry.rs` |
| Auth flows | `src/cli/auth.rs` (interactive) + `src/connector/factory.rs` (dispatch) |
| NFS protocol weirdness | `src/nfs/server.rs` + the invariants section above |

## Open design proposals

- `docs/proposals/vfs-core-split.md` — split the 4655-LoC `core.rs` into
  6 focused modules. Plan-only; execute last.
- `docs/proposals/aggregate-snapshot.md` — fix the prefix-match silent-
  failure mode in aggregate-collection writes (comments). Three options
  documented; recommended fix is hash-based snapshot in the draft store.
- `docs/proposals/writable-backend.md` — pluggable writable backend for
  `.scratch/` (S3, GCS, etc.) so general-purpose tools can write inside
  the mount.
- `docs/proposals/managed-agents.md` — design for hosted/managed agents
  on top of tapfs.
- `docs/proposals/ios-mobile.md` — work in progress.

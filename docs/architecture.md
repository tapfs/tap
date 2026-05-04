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
| `src/credentials.rs` | 528 | OS keychain (primary) + `~/.tapfs/credentials.yaml` index for non-secret metadata |
| `src/draft/store.rs` | — | Per-resource on-disk drafts. The persistence backbone of writes. |
| `src/cache/{store,disk}.rs` | — | L1 in-memory (60s TTL) + L2 on-disk. Read-only memo. |
| `src/version/store.rs` | — | Optional per-resource version history for connectors that support it |
| `src/governance/audit.rs` | — | Append-only audit log of every write/create/delete |
| `src/cli/{mount,auth,...}.rs` | — | `tap` CLI — daemon mode, interactive auth, mount management |
| `src/ipc.rs` | — | Unix socket so `tap mount X` can hot-add a connector to a running daemon |
| `src/main.rs` | — | Binary entry point |

## Core abstractions

### `Connector` trait (`src/connector/traits.rs`)

The minimum shape every backend implements:

```rust
async fn list_collections() -> Result<Vec<CollectionInfo>>;
async fn list_resources(collection: &str) -> Result<Vec<ResourceMeta>>;
async fn read_resource(collection: &str, id: &str) -> Result<Resource>;
async fn write_resource(collection, id, content) -> Result<()>;
async fn create_resource(collection, content) -> Result<ResourceMeta>;
async fn delete_resource(collection, id) -> Result<()>;
async fn resource_versions(collection, id) -> Result<Vec<VersionInfo>>;
async fn read_version(collection, id, version) -> Result<Resource>;
```

Errors are returned as `anyhow::Error`. The VFS layer downcasts to
`ConnectorError` for typed mapping (NotFound, PermissionDenied,
RateLimited{retry_after}, NetworkError, NotSupported).

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
| `AgentMd`, `ConnectorAgentMd`, `CollectionAgentMd` | Generated `agent.md` at root / connector / collection levels |
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
- `nfsstat3` mapping in `nfs/server.rs::vfs_err_to_nfs` — this is the
  errno the kernel sees. Each new VfsError variant needs a clean mapping.

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

### Credentials

- Secrets (`token`, `refresh_token`, `client_secret`) → OS keychain
  (`KEYCHAIN_SERVICE = "tapfs"`, user = connector name).
- Non-secrets (`email`, `base_url`, `client_id`) → `~/.tapfs/credentials.yaml`
  (mode 0600, atomic write via tempfile + rename).
- `TAPFS_NO_KEYCHAIN=1` puts secrets back in YAML for headless / CI.
- `TAPFS_STRICT_PERMS=1` makes loose-perms credentials.yaml a hard error.
- `Debug` impl on `ConnectorCredentials` redacts secret fields — safe
  to `tracing::debug!(?creds)`.

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
   `comments.md`, `agent.md`) must return `Ok(())` from `unlink`
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

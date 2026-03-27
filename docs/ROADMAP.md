# tapfs Roadmap

> Based on architecture reviews by domain experts. Priorities ordered by impact on adoption.

---

## Phase 0: Open Source Ready

Ship the basics that every open-source project needs. Without these, contributors can't find, trust, or contribute to the project.

- [ ] **README.md** — project description, quick start, architecture diagram, badges
- [ ] **LICENSE** — Apache 2.0
- [ ] **CI pipeline** — GitHub Actions: build (Linux + macOS), test, clippy, rustfmt
- [ ] **Cargo.toml metadata** — license, repository, homepage, keywords, categories
- [ ] **CONTRIBUTING.md** — dev setup, running tests, submitting PRs
- [ ] **CODE_OF_CONDUCT.md** — Contributor Covenant
- [ ] **SECURITY.md** — vulnerability reporting process
- [ ] **.github/ISSUE_TEMPLATE/** — bug report, feature request templates
- [ ] **.github/PULL_REQUEST_TEMPLATE.md**
- [ ] **Fix 23 clippy warnings** — `# Safety` docs on FFI functions, needless borrows
- [ ] **Fix 10 cargo doc warnings** — escape angle brackets in doc comments
- [ ] **Add crate-level docs** (`//!` in lib.rs) and doc comments on public types
- [ ] **rust-toolchain.toml** — pin Rust version
- [ ] **.gitignore additions** — `.env`, `.DS_Store`, `*.pem`, `credentials.json`
- [ ] **Fix Docker entrypoint** — default should match landing page (Google connector)
- [ ] **Remove `libparser.rlib`** from git history

---

## Phase 1: Fix the Data Path

The filesystem must behave like a filesystem. These issues make tapfs unusable for non-trivial workloads.

### Read path
- [ ] **Stop cloning full content on every FUSE read chunk** — `read()` fetches entire resource, clones `Vec<u8>`, then slices. Every 128KB chunk triggers a multi-MB clone. Switch `Resource.data` to `bytes::Bytes` (refcount bump, not deep copy) or cache with offset-aware access.

### Write path
- [ ] **Buffer writes in memory, flush to disk on close** — current `write()` does full read-modify-write cycle per 4KB FUSE chunk (O(n^2) disk I/O). Buffer in a `HashMap<u64, Vec<u8>>` and flush on `flush()`/`release()`.

### Stat correctness
- [ ] **Report real file sizes** — dynamic content (agent.md, live resources without cache) returns hardcoded 4096. Generate content on `getattr` or cache the size after first read.
- [ ] **Use real timestamps** — all `atime`/`mtime`/`ctime` are `SystemTime::now()`. Use `updated_at` from `ResourceMeta` for `mtime`. Breaks `make`, `rsync`, `ls -lt`.
- [ ] **Handle truncation in `setattr`** — `echo "new" > file` sends `setattr(size=0)` before writing. Currently ignored, causing Frankenstein files (old content + new content overlaid).

### Access control
- [ ] **Check requesting UID in `access()`** — currently returns OK for everything. At minimum verify UID matches mounting user.

---

## Phase 2: Networking & Resilience

The connector makes HTTP calls from FUSE callbacks. These must be robust.

- [ ] **Connection pooling config** — set `pool_max_idle_per_host`, `connect_timeout`, `timeout`, `tcp_keepalive` on reqwest Client
- [ ] **Retry with backoff on 429/503** — currently only retries 401. Use `reqwest-middleware` + `reqwest-retry` or hand-roll with exponential backoff.
- [ ] **Request concurrency limiter** — `tokio::sync::Semaphore` per connector to prevent `cat *.md` from firing unlimited concurrent HTTP requests
- [ ] **Proactive token refresh** — use `expires_in` from token response. Refresh at 80% of TTL instead of waiting for 401 failure.
- [ ] **Cache credentials file at init** — `TokenProvider` re-reads and re-parses `credentials.json` on every refresh. Parse once at construction.

---

## Phase 3: Connector Contract

The `Connector` trait is the API that the ecosystem builds on. It must be expressive enough for real-world APIs.

### Trait additions
- [ ] **`configure(&mut self, config: Value) -> Result<()>`** — lifecycle hook for validation, health check
- [ ] **`create_resource(collection, content) -> Result<ResourceMeta>`** — distinct from `write_resource` (POST vs PATCH)
- [ ] **`delete_resource(collection, id) -> Result<()>`** — enables `rm` on live resources
- [ ] **`schema(collection) -> Result<CollectionSchema>`** — capabilities introspection (read-only? supports drafts? field list?)
- [ ] **Pagination on `list_resources`** — return `Stream` or accept `ListOptions { page_size, cursor }` instead of `Vec<ResourceMeta>`
- [ ] **Typed errors** — replace `anyhow::Result` with connector-specific error enum (NotFound, PermissionDenied, RateLimited { retry_after }, NetworkError, etc.)
- [ ] **`close()` / `shutdown()`** — cleanup lifecycle hook

### YAML spec expressiveness
- [ ] **OAuth2 flows** — authorization_code, client_credentials, PKCE, refresh
- [ ] **Pagination strategies** — cursor, offset, link-header, nextPageToken
- [ ] **Rate limiting config** — requests per minute, burst, retry-after header
- [ ] **Custom HTTP methods** — configurable per endpoint (GET/POST/PUT/PATCH/DELETE)
- [ ] **Custom headers** — per-connector and per-endpoint
- [ ] **Response transforms** — JSONPath extraction, field renaming, list_root with nested paths
- [ ] **Delete endpoint** — per collection

---

## Phase 4: Observability

Enterprise deployments need visibility into what tapfs is doing.

- [ ] **Prometheus metrics** — `tapfs_api_request_duration_seconds{connector,collection,method}`, `tapfs_cache_hits_total`, `tapfs_cache_misses_total`, `tapfs_fuse_op_duration_seconds{op}`, `tapfs_active_drafts`, `tapfs_node_table_size`
- [ ] **OpenTelemetry tracing** — propagate trace context through HTTP headers, correlate FUSE ops with API calls
- [ ] **Audit log rotation** — currently grows unbounded. Add size-based rotation or `tracing-appender` integration.
- [ ] **`tap status` improvements** — show token health + expiry, cache hit rate, last API error, active drafts count
- [ ] **Health check endpoint** — HTTP endpoint on a side port for Docker/Kubernetes liveness probes

---

## Phase 5: Transactions

Replace the `.tx/` directory approach with transparent session-based transactions.

- [ ] **`tap tx start <name>`** — activate transaction, all writes buffer locally
- [ ] **`tap tx status`** — show pending changes in active transaction
- [ ] **`tap tx commit <name>`** — push all buffered changes atomically (with rollback on partial failure)
- [ ] **`tap tx abort <name>`** — discard all buffered changes
- [ ] **Transaction isolation** — reads within a transaction see buffered writes overlaid on live data
- [ ] **Remove `.tx/` directory approach** — replace with session-based model

---

## Phase 6: Configuration & Multi-Connector

- [ ] **Declarative config file** — `tapfs.yaml` declaring connectors, mount points, auth, cache TTL. Checkable into source control.
- [ ] **Multi-connector mounting** — single `tap mount` reads config and mounts all connectors under one mount point
- [ ] **Connect install to mount** — installed connectors (`~/.tapfs/connectors/`) auto-discoverable by `tap mount`
- [ ] **Global flags** — `--data-dir` as a global flag or always from `TAPFS_DATA`, not repeated per subcommand
- [ ] **CLI subgroups** — `tap connector install/list/remove/update` instead of flat `tap install/connectors/remove/update`

---

## Phase 7: Production Hardening

- [ ] **Graceful shutdown** — stop `process::exit(0)` in signal handler. Drain pending writes, flush buffers, close connections.
- [ ] **File locking** — `flock()` or advisory locks on DraftStore and VersionStore for cross-process safety
- [ ] **Atomic version writes** — write to temp file, rename. Prevents corruption on crash.
- [ ] **Enforce `.lock` files** — currently advisory only. Block writes when lock is held by another agent.
- [ ] **Memory bounds** — size-limited LRU cache (byte-count cap), LRU eviction for NodeTable
- [ ] **Multithreaded FUSE** — single-threaded `fuser` serializes all ops. One slow API call blocks everything.
- [ ] **Slug collision handling** — two Drive files with similar names can produce the same slug. Add suffix disambiguation.

---

## Phase 8: Platform & Distribution

- [ ] **macOS File Provider Extension** — end-to-end testing, code signing, app bundle distribution
- [ ] **Kubernetes CSI driver** — DaemonSet with bind mounts, no `--privileged` needed in agent pods
- [ ] **crates.io publishing** — `cargo install tapfs`
- [ ] **Docker Hub publishing** — `docker pull tapfs/tap`
- [ ] **Homebrew formula** — `brew install tapfs`
- [ ] **CLI fallback mode** — `tap cat`, `tap ls` for environments without FUSE (Workers, WASM)

---

## Phase 9: Ecosystem (Ongoing)

- [ ] **`tap init <name>`** — scaffold new connector project from template
- [ ] **`tap validate`** — lint and test connector spec locally
- [ ] **`tap dev mount ./`** — mount local connector for live testing
- [ ] **Connector test framework** — `smoke.yaml` runner for automated connector validation
- [ ] **Auto-generated `agent.md`** — generate from connector spec schema, not hand-written
- [ ] **WASM plugin support** — for connectors that need more than YAML (complex auth, transforms)
- [ ] **Community connectors** — Jira, Notion, Slack, GitHub, Salesforce, HubSpot, ServiceNow, Snowflake

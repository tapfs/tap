# AGENTS.md

(`CLAUDE.md` in this repo is a symlink to this file.)

## Read this first

**`docs/architecture.md`** is the source of truth for tapfs architecture
— module map, data flow, NodeKind dispatch, the draft state machine,
cross-cutting concerns (errors, lock discipline, retry, idempotency,
backpressure), spec-driven REST connector shape, NFS protocol invariants,
and "where to look first" by topic. Skim it before exploring the code;
it will save you several rediscovery passes.

**Maintenance rule:** any PR that changes module boundaries, adds a
NodeKind, changes the draft state machine, introduces a new VfsError
variant, changes the retry/auth flow, or touches NFS protocol invariants
**must update `docs/architecture.md` in the same commit**. The doc is
load-bearing — out-of-date is worse than absent. Treat updating it as
part of "done," not a follow-up.

The protocol-level invariants below are the ones that took packet
captures to discover and need to be visible *every time* I open the
project — they stay duplicated here on purpose.

## Workflow

### Test-driven development (mandatory)

Every behavioral change follows the red → green → refactor cycle:

1. Write a failing test that captures the bug or new behavior. Run it.
   Confirm it fails for the **right** reason (not a compile error, not a
   typo).
2. Implement the smallest change that makes it pass. Run the focused
   test, then the full suite (`cargo test --no-default-features --features nfs`).
3. Refactor only with green tests.

Applies to bug fixes, features, refactors with observable behavior, and
spec changes that affect parsing. Does **not** apply to: docs-only PRs,
typo / comment-only edits, build-system tweaks with no behavioral
surface, or a chore like moving items to satisfy clippy.

If the behavior is genuinely untestable from Rust (NFS kernel protocol
edge cases, FUSE mount-time errors), say so explicitly in the commit
message and explain how it was verified manually — never silently skip.

### Guardrails (require explicit human approval before proceeding)

Stop and ask before doing any of these. A previous approval for the same
action in the same conversation does **not** carry forward to a new
context:

- Skipping the failing-test-first step for a behavioral change
- Merging or pushing code that lacks a test for the new behavior
- Bypassing hooks or signing (`--no-verify`, `--no-gpg-sign`)
- Destructive git ops on shared history (`reset --hard` past pushed
  commits, `push --force`, `branch -D` on a remote-tracked branch,
  `clean -fdx`)
- Editing CI workflows, deploy scripts, branch protection, or other
  shared infrastructure
- Publishing artifacts, cutting releases, force-pushing to `main`
- Disabling tests, lowering coverage gates, or `#[ignore]`ing a failing
  test (the only acceptable reason to silence a test is to delete it
  along with the code it covered)

### Git worktrees (default for non-trivial work)

All non-trivial work happens in a dedicated worktree so parallel tasks
don't step on each other and `main` stays clean:

- Single-file, single-commit fix: OK in the main checkout.
- Anything multi-file, multi-commit, or that needs a build that might
  take minutes: spin up a worktree.

```bash
git worktree add ~/p/tapfs-worktrees/<topic> -b <topic>
cd ~/p/tapfs-worktrees/<topic>
# work, commit, push, PR
cd ~/p/tapfs && git worktree remove ~/p/tapfs-worktrees/<topic>
```

When delegating to sub-agents, pass `isolation: "worktree"` so they get
their own copy and don't fight over the file tree.

### Reference docs (part of "done")

The maintenance rule above (mandatory `docs/architecture.md` updates for
invariant-touching changes) extends to *all* reference docs under
`docs/`:

- **New CLI flag, config field, or spec field**: update `docs/architecture.md`
  or add a topical doc under `docs/` linked from architecture.md.
- **New use case visible to humans or agents**: add a row to (or a new
  file in) `tests/use-cases/` — these are the contract for what tapfs
  promises end users.
- **Schema or wire-format change**: document the schema in the same
  commit.

Out-of-date docs are worse than absent — they actively mislead.

## Commit conventions

- Use `Assisted-by: Claude:claude-opus-4-7` as the trailer (not `Co-Authored-By`)

## NFS transport (macOS)

- `nfsserve` always replies to WRITE with `committed: FILE_SYNC`, so macOS never sends COMMIT. **`vfs.flush()` must be called inside `TapNfs::write`** — it is the only flush trigger on the NFS path. (FUSE uses `release`/`flush` callbacks instead.)
- `fattr3.mode` must include the Unix file-type bits (`S_IFDIR | perm` for directories, `S_IFREG | perm` for files). Without them macOS Sequoia's NFS client fails `S_ISDIR()` checks and returns EPERM after attribute re-validation.
- Nodes that have no real mtime (Root, Connector, Collection, AgentMd, etc.) must return `nfstime3 { seconds: 0, nseconds: 0 }`, **not** the current wall clock. A changing mtime causes macOS to constantly re-validate directory attributes and, after enough re-validations, refuse access with EPERM.
- After a successful `create_resource` call, populate the in-memory cache (`cache.put_resource`) so that subsequent flushes of the same resource use `write_resource` (PATCH) instead of `create_resource` (POST) again.

## VFS flush semantics

- `VirtualFs::flush(rt, id)`: step 1 — persists write buffer to draft store; step 2 — if the node is a Live resource with a pending draft, auto-promotes to API (POST for new, PATCH for existing).
- "New" is determined by `cache.get_resource().is_none() && disk_cache.get().is_none()`. Populate the cache after a successful create to flip this flag.
- `flush_all()` (called on daemon shutdown) only persists buffers to disk; it does **not** auto-promote live resources to avoid API calls during shutdown.

## Nested collections and node hierarchy

- **Path-encoded collection names**: `"repos/tapfs/issues"` — alternating collection-name / resource-id segments. This string is treated as opaque by the `Connector` trait, cache keys, draft paths, and slug map. `RestConnector::resolve_nested_collection` walks the segments, resolves slugs to API IDs, and substitutes `{placeholders}` in endpoint templates.
- **`NodeKind::ResourceDir`**: a resource that is also a directory because its collection has `subcollections` in the spec. `kind_to_attr` returns `VfsFileType::Directory`. `readdir` returns `index.md` + one entry per subcollection from the spec — **no API call**. The resource body is read via `index.md` inside the directory.
- **`NodeKind::GroupDir`**: a synthetic directory for a `group_by` value (e.g. GitHub org name). Has no API backing; its children are `ResourceDir` nodes whose `group` field matches. Shown at the connector level instead of the raw collection.
- **`parent_param` in spec**: explicit URL placeholder name per subcollection (e.g. `parent_param: repo`). `resolve_nested_collection` substitutes the resolved parent ID into `{repo}` in every endpoint string.

## rm -rf semantics on ResourceDir

Virtual children of a `ResourceDir` (`index.md`, `comments.md`, `AGENTS.md`, etc.) **must return `Ok(())` from `unlink` unconditionally**. The actual API delete gate lives in `rmdir_resource_dir`, which is called when the directory itself is removed. This is critical because `rm -rf` sends `REMOVE` for every file before sending `RMDIR` for the directory — if any file returns EPERM, the whole operation aborts before the directory step.

- For a local-only draft (empty `_id`): `unlink index.md` cleans up the draft eagerly; `rmdir_resource_dir` is a no-op.
- For an API-backed resource: `unlink index.md` does nothing (preserves the draft with `_id`); `rmdir_resource_dir` calls `delete_resource` if `capabilities.delete: true`, otherwise returns EPERM.

## Draft state machine (_draft / _id / _version)

The frontmatter fields that control flush behavior:

| `_draft` | `_id` | Flush behavior |
|----------|-------|----------------|
| `true` | any | **Skip** — kept local, never sent to API |
| absent/false | empty | **POST** — new resource; write `_id: __creating__` sentinel before the call to block concurrent flushes |
| absent/false | `__creating__` | **Skip** — another flush is mid-flight; bail to avoid duplicate POST |
| absent/false | `<real id>` | **PATCH** — update existing resource |

After a successful POST, replace `__creating__` with the real API id and increment `_version`.

## Aggregate collections

Collections with `aggregate: true` appear as a single `.md` file (not a directory). Key invariants:

- **Read**: concatenates all resources with `---\n` separators. If the parent resource doesn't exist in the API yet (draft-only), returns empty bytes — **not** an error.
- **Write / flush**: reads the canonical content from the API, compares with the buffer. Only the suffix beyond the canonical prefix is POSTed as a new resource. Overwriting with identical-prefix + new text is idempotent up to the append point.
- The GitHub `comments` subcollection uses this mode: `cat comments.md` shows the thread; `echo "LGTM" >> comments.md` posts a reply.

## mkdir template seeding

When `mkdir` is called inside a collection that has subcollections, `index.md` is seeded with a draft template. Field inclusion rules (from spec's `render.frontmatter`):

- **Excluded**: fields with `as` alias (`"html_url as url"`) — these are read-only API projections
- **Excluded**: fields with dot notation (`"user.login"`) — nested, not directly writable
- **Excluded**: `state`, `created_at`, `updated_at`, `url` — managed by the API
- **Included**: plain writable fields like `title`, `description`, `body`
- Always prepends `_draft: true\n_id:\n_version:\n`

## Cross-platform libc mode bits

`libc::S_IFDIR` and `libc::S_IFREG` are `u16` on macOS and `u32` on Linux. Both `as u32` and `u32::from()` trigger clippy on Linux. Define constants once:

```rust
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
const MODE_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
const MODE_IFREG: u32 = libc::S_IFREG as u32;
```

## Testing patterns

- VFS unit tests live in `#[cfg(test)]` modules at the bottom of `src/vfs/core.rs`.
- Use `tempfile::tempdir()` for all disk-backed stores (DraftStore, VersionStore, DiskCache, AuditLogger).
- Create a minimal tokio runtime with `Builder::new_current_thread().enable_all().build()` and pass `rt.handle()` to VFS methods. Do **not** use `#[tokio::test]` — tests are sync, using `Handle::block_on` internally via VFS methods.
- Mock connectors implement the full `Connector` trait inline in the test module. Use `AtomicUsize` to count API calls (creates, writes) and assert counts after flush.
- NFS attribute tests (`src/nfs/server.rs`) call `vfs_attr_to_fattr` directly; use `rt.enter()` to satisfy the tokio handle requirement for `TapNfs::new`.
- Tests run against VFS directly — the NFS layer is a thin wrapper. Test the VFS behavior; NFS-specific bugs (wrong REMOVE/RMDIR mapping, stale handles) require manual mount testing.

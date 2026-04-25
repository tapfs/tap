# Pluggable Writable Backend (Tier B scratch overlay)

## Context

tapfs mounts are currently read-mostly: connectors expose SaaS records as markdown files,
drafts live on local disk, and writes flow back to the originating API. This works for
human editing and agent workflows that follow the draft → promote pattern. It does not work
for tools that need a general-purpose writable directory inside the mount — `git init`,
`npm install`, `cargo build`, any tool that needs to scribble metadata alongside the files
it processes.

The ask: expose a writable subtree (`.scratch/<name>/` or similar) inside the mount whose
backing storage is pluggable. Local disk is the obvious first backend. S3 (and compatible
stores: R2, GCS, MinIO) is the motivating second backend — it provides persistence across
machines, across sessions, and without requiring a shared filesystem.

## Design

### WritableBackend trait

```rust
#[async_trait]
pub trait WritableBackend: Send + Sync {
    /// Human-readable name ("local", "s3://bucket/prefix", …).
    fn uri(&self) -> &str;

    async fn stat(&self, path: &Path) -> Result<FileMeta>;
    async fn readdir(&self, path: &Path) -> Result<Vec<DirEntry>>;
    async fn read(&self, path: &Path) -> Result<Bytes>;
    async fn write(&self, path: &Path, data: &[u8]) -> Result<()>;
    async fn mkdir(&self, path: &Path, recursive: bool) -> Result<()>;
    async fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    async fn unlink(&self, path: &Path) -> Result<()>;
    async fn symlink(&self, target: &Path, link: &Path) -> Result<()>;
}
```

Supporting types:

```rust
pub struct FileMeta {
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub kind: FileKind,
    pub perm: u16,
}

pub enum FileKind { File, Directory, Symlink }
```

### Planned implementations

| Backend | URI form | Notes |
|---|---|---|
| `LocalBackend` | `local://<abs-path>` | Default. Full POSIX via `std::fs`. |
| `S3Backend` | `s3://<bucket>/<prefix>` | Object store. See limitations below. |
| `InMemoryBackend` | `mem://` | Ephemeral; for tests and agent sessions that don't need persistence. |

### Mount layout

A scratch backend is attached to a named path inside the mount:

```
/tap/
  salesforce/accounts/…   ← connector, read-mostly
  .scratch/               ← reserved; lists registered backends
    workspace/            ← backed by LocalBackend or S3Backend
      .git/               ← writable, full POSIX
      notes.md            ← writable
    session/              ← another backend, e.g. InMemoryBackend
```

The VFS routes any inode under `.scratch/<name>/` entirely through the corresponding
`WritableBackend`. Connector governance (drafts, audit log, version snapshots) does not
apply to scratch paths — they are unmanaged by design.

### Config

Mount config gains a `scratch` section:

```yaml
# ~/.tapfs/config.yaml  (or --scratch flag)
scratch:
  workspace:
    backend: local://~/.tapfs/scratch/workspace
  session:
    backend: mem://
  s3work:
    backend: s3://my-bucket/tapfs-scratch
    region: us-east-1
```

The corresponding CLI flag for ad-hoc use:

```
tap mount --scratch workspace=local://~/.tapfs/scratch/workspace salesforce
```

### VFS integration

`NodeKind` gains a new variant:

```rust
NodeKind::Scratch {
    name: String,   // backend name, e.g. "workspace"
    path: PathBuf,  // path within the backend, relative to its root
}
```

`VirtualFs` carries a `scratch: HashMap<String, Arc<dyn WritableBackend>>`. All VFS
operations (`lookup`, `getattr`, `readdir`, `read`, `write`, `create`, `mkdir`, `rename`,
`unlink`) check the inode's `NodeKind` first; if it is `Scratch`, they delegate to the
corresponding backend rather than the connector registry.

## S3 limitations

S3 does not support atomic rename or advisory locks. This means:

- `git` commit/index operations (`rename` of `.git/index.lock` → `.git/index`) are
  unreliable over a naive S3 backend.
- `flock`-based locking (used by many Unix tools) is unavailable.
- Eventual consistency on older S3 configurations may cause stale reads after rapid
  write-then-read cycles.

Mitigations for the S3 backend:

1. **Local metadata cache** — maintain a write-through cache of directory listings and
   small files (≤ 64 KB) on local disk, similar to how s3fs/goofys work. Reduces
   read amplification and makes listing fast.
2. **Rename emulation** — implement `rename` as copy + delete for files; for directories,
   recursively copy then delete (not atomic, but acceptable for agent workflows that don't
   rely on rename atomicity across crash boundaries).
3. **Lock emulation** — store `.lock` files as S3 objects with a conditional PUT (via
   `If-None-Match: *`); this is not a true flock but serializes concurrent writers.

These mitigations are explicitly not POSIX-complete. The S3 backend is suitable for:
agent working directories, artifact storage, cross-machine scratch that does not run
lock-heavy tooling (git, npm). It is **not** a drop-in for tools that depend on POSIX
rename atomicity or flock.

## Explicit non-goals

- POSIX-complete S3 (that's goofys/s3fs territory — deep OS integration we don't need).
- Encryption or access control on scratch data (use S3 bucket policies / IAM).
- Replication or conflict resolution across concurrent scratch writers.

## Implementation order

1. **WritableBackend trait + LocalBackend** — the trait is the contract; LocalBackend is
   a thin wrapper around `std::fs` (+ `tokio::fs` for the async surface). This is the
   milestone that unlocks `git init` inside a tapfs mount.
2. **VFS plumbing** — `NodeKind::Scratch`, routing in `lookup`/`readdir`/`read`/`write`/
   `mkdir`/`rename`/`unlink`/`symlink`. Config parsing + `--scratch` flag.
3. **InMemoryBackend** — needed for tests; small.
4. **S3Backend** — add `aws-sdk-s3` dep, implement the trait with the mitigations above,
   add integration test against a local MinIO instance.

## Verification

- **LocalBackend / git:** `cd /tmp/tap/.scratch/workspace && git init && git add . && git commit -m "init"` succeeds; `.git/` is present under `~/.tapfs/scratch/workspace/`.
- **LocalBackend / rsync:** `rsync -av /tmp/tap/salesforce/accounts/ /tmp/tap/.scratch/workspace/accounts-backup/` completes without error; second run transfers nothing.
- **S3Backend / basic I/O:** write a file, unmount, remount with same S3 uri, read it back.
- **S3Backend / agent workflow:** Claude Code agent reads from a connector collection, writes a summary to `.scratch/s3work/summary.md`, remounts from a different machine, reads it back.

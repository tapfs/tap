# Aggregate Collection Snapshot Semantics

## Context

Some collections — most notably GitHub issue comments — are exposed as a
single aggregated `.md` file rather than a directory of per-resource files.
The contract today (in `vfs/core.rs:1496–1529`):

- `cat comments.md` returns `<comment1>\n---\n<comment2>\n---\n...` —
  freshly fetched and rendered every time.
- `echo "LGTM" >> comments.md` triggers a flush. The flush:
  1. Re-reads the canonical content from the API.
  2. Strips trailing whitespace from the canonical.
  3. Checks whether the user's buffer *starts with* that canonical prefix.
  4. POSTs whatever's after the prefix as a new resource.

This works in the happy case: user opens, types at the end, saves. It fails
silently — or worse, posts wrong things — in several common cases:

- **Trailing-whitespace mismatch.** The API returns `"foo\n"`, gets stripped
  to `"foo"`. The editor saves with `"foo  \n"` (extra trailing spaces from
  reflow). Prefix check fails; the entire content is treated as "user wrote
  this from scratch", and either nothing happens (no detectable suffix) or
  the whole thing posts as a new comment.

- **Server-side reorder.** GitHub may reorder by reaction count or recency.
  Between the user's read and their save, comment order on the server
  changed. Prefix mismatch; user's append silently dropped.

- **Re-rendering.** A connector spec change (or a markdown library update)
  changes how a comment is rendered. The user's editor still has the OLD
  rendering; the next read returns the NEW one. Prefix mismatch.

- **Non-UTF-8 content.** The current read uses `from_utf8_lossy` (line 2733),
  which silently replaces invalid bytes. Round-trip is not byte-identical.

A senior reviewer would flag this as the kind of "works in demo, fails in
production" pattern that erodes trust in the whole product. This proposal
sketches a fix and the tradeoffs of three viable options.

## Why this is plan-only

The fix is genuinely a design problem, not a coding problem. It needs:

- A decision about *where* the snapshot lives (memory, draft store, both).
- A decision about *what* "appended" means in the presence of server-side
  reorders (do we reject the whole write? Try to merge?).
- A decision about how to surface the "your view is stale, please re-read"
  error through NFS WRITE — there's no clean errno for "your local copy
  diverged from the server."

Until those questions have answers the team is happy with, code would be
premature.

## Goals

1. A user write to `comments.md` either appends correctly or fails loudly
   with an actionable error. No silent drop. No unexpected duplicate post.
2. The current happy-path flow (open / type at end / save) keeps working
   without ceremony.
3. Concurrent users editing the same aggregate file get a correct outcome
   (last-writer-wins is acceptable; lost-update is not).
4. The mechanism is opt-in per collection so APIs with stable ordering
   (chronological append-only) don't pay the cost.

## Three options considered

### Option A — Per-fd snapshot in memory

When a client opens `comments.md` for write, capture the bytes returned by
the read at open time. On flush, diff the user's buffer against that
snapshot (not against a fresh API fetch). Reject the write with `EAGAIN` if
the snapshot is older than a TTL (e.g. 30 seconds) and force the client to
re-read.

**Pros:** Cheap. No persistent storage. Works correctly for the common
pattern (open, edit, save-quickly).

**Cons:** NFSv3 doesn't have a stable file-handle-to-fd mapping — every
WRITE op carries the fileid, not an opaque session token. Storing per-fd
state is awkward; we'd have to key the snapshot by `(fileid, client-IP,
process-id)` or similar, none of which is fully reliable.

### Option B — Hash-based snapshot in the draft store

When `comments.md` is read, store `(canonical_bytes, sha256(canonical_bytes))`
in the draft store under a special key (e.g. `__snapshot/<connector>/<collection>`).
On flush, recompute the hash of what we read and compare. If they match, the
prefix-comparison is safe; if they don't, the API drifted under us — surface
`EAGAIN` and the next read updates the snapshot.

**Pros:** Survives daemon restart. No fd-tracking gymnastics. Lets us
detect drift even when the user takes hours between read and save.

**Cons:** Adds a write to the draft store on every aggregate read (cheap,
but non-zero). Snapshot can grow large for popular comment threads —
needs eviction. The "stale" window is the time between any two reads
to the same aggregate, which for active threads can be very short.

### Option C — Make aggregate writes structured (accept JSON or markdown
fragments instead of full-file replacement)

Change the contract: instead of "edit the aggregated view and save," the
user creates a new resource by writing only their fragment to a sibling
file (e.g. `comments.draft.md`). On flush, that becomes a POST. The
aggregated view stays read-only.

**Pros:** Eliminates the entire prefix-matching problem. Aggregates become
a presentation concern, not a write contract.

**Cons:** Breaks the convenient "type at the end of comments.md and save"
UX that's the primary value proposition. The existing GitHub workflow
people would have to learn a new pattern. Would need user-facing docs +
an audit of which connectors rely on the current behavior.

## Recommendation

**Option B** for the next implementation pass — it preserves the happy-path
UX while catching all three failure modes the original review identified.
Option A is too fragile under NFSv3's session model. Option C is the right
*long-term* shape but breaks today's UX in ways that need product input
before code.

### Sketch of Option B implementation

1. Add `_aggregate_snapshot` storage to the draft store, keyed by
   `(connector, collection)`. Stores `(timestamp, sha256, canonical_bytes)`.

2. On `read_aggregate_collection`, after fetching, write the snapshot
   atomically (tempfile + rename, like `write_yaml_index` from PR1).

3. On flush of an aggregate write, look up the snapshot:
   - Recompute `sha256(written.starts_with prefix)` — does it match what we
     stored at read time?
   - If yes: the prefix is canonical; the suffix is the user's append.
     POST the suffix.
   - If no: surface `VfsError::Busy` (PR5 added the variant), which maps
     to `NFS3ERR_JUKEBOX`. Client retries; next read refreshes the snapshot.

4. Snapshot eviction: TTL of 5 minutes from last read. Beyond that the
   user has to re-read first. Logged at `tracing::warn` so we can see
   how often this fires in production.

5. Spec opt-in: `aggregate: true` in the collection spec is the existing
   trigger. Add an optional `aggregate_snapshot: false` to disable for
   APIs known to be stable (chronological-only append, no reorder), so
   they keep the cheaper happy-path-only behavior.

## Critical files

- `src/vfs/core.rs:1496–1529` — current prefix-match flush.
- `src/vfs/core.rs:2706–2740` — `read_aggregate_collection` + rendering.
- `src/draft/store.rs` — needs a `write_snapshot` / `read_snapshot` API.
- `src/connector/spec.rs` — add `aggregate_snapshot: Option<bool>` to
  `CollectionSpec`.

## Verification (when implemented)

- Unit tests for the snapshot diff logic in isolation.
- Integration test: mock connector that reorders comments between reads;
  user append → expect `EAGAIN`, second attempt after re-read → expect
  POST of just the suffix.
- Integration test: snapshot age > TTL → `EAGAIN` even if content matches.
- Manual smoke: `cat comments.md > /tmp/copy.md && echo "LGTM" >>
  /tmp/copy.md && cp /tmp/copy.md comments.md` — the standard "edit
  in vim" round-trip — must still work.

## Open questions for product

1. Is "force the user to re-read after 5 min of stale snapshot" acceptable
   UX? Or do we accept lost-update for older snapshots?
2. Should `aggregate_snapshot` default to `true` or `false`? If `true`, all
   existing aggregate collections opt in by default — safer but slightly
   slower. If `false`, only Comments + ones we explicitly enable get the
   protection.
3. Does the comment thread need to surface a "reload required" file (e.g.
   `comments.md.stale`) for the user to see, or is the kernel's `EAGAIN` →
   editor's "file changed on disk" warning enough?

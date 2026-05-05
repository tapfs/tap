# Splitting `vfs/core.rs` into Focused Modules

## Context

`src/vfs/core.rs` is currently 4655 lines containing 89 methods, one `SlugMap`,
one `NodeTable`, four free helper functions, and four nested test modules. It
is the largest single file in the codebase by a factor of ~3, and every meaningful
PR to the VFS layer touches it. Three concrete problems:

- **Cognitive load.** A reader looking for the draft state machine has to grep
  past readdir code, frontmatter parsing, the slug map, and four kinds of
  generated `AGENTS.md`.
- **Merge conflicts.** Independent PRs that touch unrelated VFS concerns
  (e.g. NFS error mapping vs idempotency keys) collide on the same file.
- **Test discoverability.** Tests for sentinel handling, aggregate flush,
  nested collections, and disk-cache integration all live in this file under
  four different `#[cfg(test)]` modules. New contributors don't know where
  to add their test.

This proposal is plan-only. The actual refactor is high-churn and should land
*after* every other open PR has merged so the line moves don't conflict with
unrelated logic changes.

## Design

### Target module layout

| Module | LoC target | Contents |
|---|---|---|
| `vfs/frontmatter.rs` | ~350 | `parse_tapfs_meta`, `strip_tapfs_fields`, `inject_tapfs_fields`, `make_sentinel`, `classify_sentinel`, `generate_idempotency_key`, frontmatter unit tests |
| `vfs/path.rs` | ~500 | `SlugMap`, `resolve_root_child`, `resolve_connector_child`, `resolve_collection_child`, `resolve_resource_dir_child`, `resolve_group_dir_child`, `parse_resource_filename`, `lock_slug`, `title_to_slug`, `find_collection_spec_in` |
| `vfs/nodes.rs` | ~450 | `NodeTable`, `kind_to_attr`, `resource_size`, `readdir_root`, `readdir_connector`, `readdir_collection`, `readdir_resource_dir`, `readdir_group_dir` |
| `vfs/write.rs` | ~700 | `buffer_write`, `flush`, `flush_all`, `write`, `truncate`, `create`, `unlink`, `mkdir`, `rename`, draft state machine, sentinel handling |
| `vfs/aggregate.rs` | ~250 | `read_aggregate_collection`, aggregate-collection write/flush, `is_aggregate_collection` |
| `vfs/agent_md.rs` | ~300 | `generate_root_agent_md`, `generate_connector_agent_md`, `generate_collection_agent_md` |
| `vfs/read.rs` | ~400 | `read`, `read_resource_data`, `getattr`, `lookup`, `readdir` (the public dispatch surface for read-side ops) |
| `vfs/core.rs` | ~700 | `VirtualFs` struct definition, `new`, `with_disk_cache`, `with_slug_map`, `invalidate_resource_cache`, the lock-discipline doc-comment, integration tests that span modules |

Total: ~3650 LoC. The drop from 4655 comes from removing repeated module-level
imports and a handful of large doc-comments that only need to live in one place.

### Visibility

All cross-module helpers go to `pub(crate)` or `pub(super)`. The `VirtualFs`
public API surface stays exactly the same — this is a pure refactor; any
external behavior change is a bug.

### Test layout

Each module gets its own `#[cfg(test)] mod tests` block for unit-level checks
of helpers in that file. The four existing nested test modules in `core.rs`
move to:

- `mod tests` (unit) → split per-module
- `mod disk_cache_integration` → stays in `core.rs` (spans modules)
- `mod flush_promotion` → moves with `flush` to `vfs/write.rs`
- `mod nested_collections` → stays in `core.rs` (spans modules)

## Migration steps

The refactor is mechanical but error-prone if done in one shot. Six small
commits, each independently buildable and testable:

1. **Extract `frontmatter.rs`.** Pure functions, no `VirtualFs` reference.
   Smallest risk. Touches one file; changes one `use` statement in `core.rs`.
2. **Extract `path.rs` (`SlugMap` + free helpers).** Move the standalone
   struct and the path-resolution free functions. The `resolve_*_child`
   methods on `VirtualFs` stay put for now — they need `&self`.
3. **Extract `nodes.rs` (`NodeTable`).** Same shape as step 2: standalone
   struct moves cleanly.
4. **Extract `agent_md.rs`.** The `generate_*_agent_md` methods become free
   functions taking `&VirtualFs` (or specific dependencies, like
   `&ConnectorRegistry`) so they don't require `&mut self`. Self-contained.
5. **Extract `aggregate.rs`.** `read_aggregate_collection` and the aggregate
   write path move. This one needs care because aggregate flush touches the
   write buffer — keep it close to step 6.
6. **Split the methods on `VirtualFs` between `read.rs` and `write.rs`.**
   The hardest step because both files need access to private fields. Two
   options:
   - **(a)** Keep all methods in `inherent impl VirtualFs` blocks in their
     respective files. Rust allows multiple `impl VirtualFs` blocks across
     files in the same crate; visibility stays as-is.
   - **(b)** Make the read/write surfaces traits and have `VirtualFs`
     implement them. More elegant but rippling.

   Recommend **(a)** — minimal change, idiomatic Rust, no trait gymnastics.

After step 6, `core.rs` only contains the `VirtualFs` struct, constructors,
and the cross-module integration tests.

## Critical files

Only `src/vfs/core.rs` exists today; the proposal creates seven sibling files.
No external file (NFS adapter, FUSE adapter, CLI) should need to change —
the public API is stable.

## Verification

- `cargo build --no-default-features --features nfs` clean after each of
  the 6 steps (each step lands as its own commit so bisection works).
- `cargo test --no-default-features --features nfs --lib` — same passing
  count after each step.
- `cargo build --features fuse` (where FUSE deps are installed) — public
  surface unchanged, must compile.
- After step 6: run `cargo test --workspace` plus a manual NFS smoke-mount
  (mount, `ls`, write a draft, `cat`, `rm -rf`) to make sure nothing regressed
  during the file moves.

## Non-goals

- No behavior change. If a method's signature must change to make the split
  work, that's a separate PR landing first.
- No new tests. The existing tests cover this code; reorganizing them is the
  only test-side change.
- No `mod.rs` re-exports beyond what's needed to keep external callers
  compiling. The split is internal organization, not a public API change.

## Why this is plan-only

A naive "move all the things at once" PR would conflict with every other
open branch (PRs 1-10 all touch `core.rs` somewhere). Landing the refactor
last, in 6 small bisectable commits, gives reviewers a chance to follow
each move and catch any accidental visibility tweak. Estimated effort: one
focused half-day session per the 6 steps.

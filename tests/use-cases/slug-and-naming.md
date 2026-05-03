# Slug and Naming

API resources are exposed with human-readable filenames derived from their title field. Slugs are persisted in a local map so they survive daemon restarts. Collision with an existing slug appends the API id.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | See title-based filenames instead of numeric IDs | `ls .../issues/` | Files named `fix-login-bug/`, `add-oauth/` — not `42/`, `43/` |
| 2 | Slug is stable across remounts | Stop and restart daemon; `ls .../issues/` | Same filenames as before; slugs loaded from persisted map |
| 3 | Collision handled by appending ID | Two issues titled "Bug Fix" | Files named `bug-fix/` and `bug-fix-43/`; no overwrite |
| 4 | Special characters stripped from slug | Issue titled "Fix: Auth & Login!" | Filename becomes `fix-auth-login/` |
| 5 | Numeric-only IDs used as fallback | Issue with no title | Filename is the raw API id (e.g. `99/`) |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 6 | Resolve filename to API id for PATCH | Write to `fix-login-bug/index.md` | VFS looks up slug → API id `42`; PATCH sent to correct endpoint |
| 7 | Navigate without knowing numeric IDs | `cat .../issues/fix-login-bug/index.md` | Returns issue content; no knowledge of id `42` required |
| 8 | Grep by semantic name | `grep -r "auth" .../issues/` | Matches in files with meaningful names, not opaque IDs |
| 9 | Create resource with chosen slug | `mkdir .../issues/my-task` | Draft created with slug `my-task`; slug→id mapping written after POST |
| 10 | Slug survives write-back | Read `index.md`, edit, save | Slug unchanged; `_id` still maps correctly for PATCH |

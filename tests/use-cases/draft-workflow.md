# Draft Workflow

Resources start as drafts (`_draft: true`) that live only on disk. Removing `_draft: true` and saving promotes them to the API. The `_id` field tracks whether a resource has ever been POSTed; `_version` increments after each successful flush.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Keep a work-in-progress local | Write `index.md` with `_draft: true` in frontmatter | File persists across daemon restarts; never sent to API |
| 2 | Publish a draft | Open `index.md`, delete the `_draft: true` line, save | Next flush POSTs to API; `_id:` populated with API-assigned id |
| 3 | Distinguish a draft from a live resource | `cat index.md` | Draft shows `_draft: true` and empty `_id:`; live shows numeric `_id:` |
| 4 | See draft directory in listing before publish | `ls .../issues/` after `mkdir new-bug` | `new-bug/` directory appears even though it hasn't been POSTed |
| 5 | Edit a published resource (PATCH, not POST) | Edit `index.md` after `_id:` is set, save | Flush sends PATCH to existing resource; no duplicate created |
| 6 | Check version after successive saves | `cat index.md` after multiple saves | `_version:` increments each time; confirms flush succeeded |
| 7 | Survive daemon restart with draft intact | Stop and restart `tap mount`; read draft | `_draft: true` file present, contents unchanged |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 8 | Stage a resource before committing | Write with `_draft: true`; inspect; then remove `_draft: true` | Two-step workflow; API call only at publish step |
| 9 | Detect whether resource is published | Read `_id:` field from frontmatter | Empty `_id` → draft only; non-empty → live in API |
| 10 | Avoid re-posting on multi-flush | Write resource, flush, write again, flush again | `_id` set after first flush; second flush uses PATCH |
| 11 | Batch-create several drafts then publish | `mkdir` multiple directories, fill `index.md` files, remove `_draft: true` from each | Each flushes independently; API receives one POST per resource |
| 12 | Read version to confirm write landed | Check `_version:` after saving | Incremented version confirms flush succeeded and data reached API |
| 13 | Template fields guide structured creation | `cat index.md` immediately after `mkdir` | Frontmatter skeleton contains writable field keys from spec (e.g. `title:`, `body:`) with no values |

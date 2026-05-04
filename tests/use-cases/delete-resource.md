# Deleting Resources

Deletion behavior depends on whether the resource is local-only (draft, never POSTed) or API-backed, and whether the connector spec declares `capabilities.delete: true`.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Discard a local draft (never published) | `rm -rf /tmp/tap/github/tapfs/tap/issues/my-new-bug` | Directory and draft file removed locally; no API call made |
| 2 | Delete an API-backed Linear issue | `rm -rf /tmp/tap/linear/issues/fix-auth` | `DELETE /rest/v2/issues/{id}` sent; resource removed from listing |
| 3 | Delete a GitHub label | `rm -rf /tmp/tap/github/tapfs/tap/repos/tap/labels/wontfix` | `DELETE /repos/{owner}/{repo}/labels/wontfix` sent; label removed |
| 3a | Archive a Notion page (soft delete) | `rm -rf /tmp/tap/notion/pages/<uuid>` | `PATCH /v1/pages/{id}` with `{"archived": true}` body; page hidden from listing |
| 3b | Attempt delete on a non-deletable collection | `rm -rf /tmp/tap/github/tapfs/tap/issues/fix-login-bug` | Returns `Permission denied`; GitHub issues don't expose DELETE (no `delete_endpoint` in spec) |
| 4 | Remove files inside a resource dir before rmdir | `rm /tmp/tap/.../issues/my-bug/index.md` (part of `rm -rf`) | Returns `Ok`; individual virtual files always accept unlink |
| 5 | Remove comments.md inside a resource dir | `rm /tmp/tap/.../issues/my-bug/comments.md` (part of `rm -rf`) | Returns `Ok`; aggregate file accepts unlink so rmdir can proceed |
| 6 | Abort a draft-only resource by deleting directory | `rm -rf .../issues/my-draft-bug` (no _id) | Draft file deleted; directory gone; no network call |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 7 | Clean up a draft the agent created | `rm -rf .../issues/scratch-analysis` (local only) | Draft removed without API side-effect |
| 8 | Delete via connector with delete support | `rm -rf .../linear/issues/done-task` | HTTP DELETE sent; resource purged from API and listing |
| 8a | Soft-delete via archive PATCH (Notion) | `rm -rf .../notion/pages/<uuid>` | PATCH archived: true sent; agent treats archived pages as removed |
| 9 | Handle non-deletable collection gracefully | `rm -rf .../github/tapfs/tap/issues/26` | EPERM returned; agent treats as "cannot delete via filesystem" — different from "not found" |
| 10 | Verify deletion cleared listing | `ls .../issues/` after successful delete | Deleted resource no longer appears in directory listing |
| 11 | Recursive rm on ResourceDir with children | `rm -rf .../issues/my-bug/` | Virtual children unlinked without error; final rmdir triggers delete gate |
| 12 | No double-delete on retry | `rm -rf` called twice on same path | Second call returns NotFound (already removed); no duplicate DELETE to API |

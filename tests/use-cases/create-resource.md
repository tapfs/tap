# Creating Resources

New resources are created with `mkdir` (for resources with subcollections) or by writing a new `.md` file (for leaf resources). The VFS seeds a template with spec-derived field placeholders. Removing `_draft: true` from the frontmatter and saving triggers a POST to the API on the next flush.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Create a new GitHub issue | `mkdir /tmp/tap/github/tapfs/tap/issues/my-new-bug` | Directory created immediately; `index.md` seeded with `---\n_draft: true\n_id:\ntitle:\n---` |
| 2 | Fill in issue title and body | Open `index.md` in editor, fill `title: Fix auth bug`, write body | File saves without error |
| 3 | Publish the issue to GitHub | Remove `_draft: true` line, save file | On next flush, POST to `/repos/tapfs/tap/issues`; `_id` field populated with GitHub issue number |
| 4 | Verify issue was created | `cat index.md` after save | `_id:` field contains real GitHub issue number (e.g. `42`) |
| 5 | Create an issue comment | Append text to `comments.md` | POST to `/repos/tapfs/tap/issues/42/comments`; comment appears on GitHub |
| 6 | See new issue in listing | `ls /tmp/tap/github/tapfs/tap/issues/` | New directory appears alongside existing ones |
| 7 | Create a draft and keep it local | Create `index.md` and keep `_draft: true` | File persists across remounts; never sent to GitHub API |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 8 | Create issue via shell commands | `mkdir .../issues/new-bug && printf '---\ntitle: New bug\n---\nDetails' > .../issues/new-bug/index.md` | Draft created, ready to publish |
| 9 | Publish immediately (no draft) | Write frontmatter without `_draft: true` | Flush triggers POST; `_id` written back to file |
| 10 | Create a comment on an issue | `printf '\n## New comment\nLooks good!\n' >> comments.md` | Appended suffix POSTed as new comment; existing comments unchanged |
| 11 | Avoid duplicate creates on retry | Write then flush twice without changes | API called exactly once; second flush uses PATCH |
| 12 | Detect publish success | Read `_id:` field from `index.md` after write | Non-empty `_id` confirms resource was created in API |
| 13 | Create resource in correct collection path | `mkdir /tmp/tap/github/acme/myrepo/issues/task-1` | Issue created under `acme/myrepo`, not under wrong org |

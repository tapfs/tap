# Nested Collections

Resources that have subcollections defined in the spec appear as directories rather than files. Navigation follows the REST hierarchy: connector → org (group) → repo → collection → resource → subcollection.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Browse repos by org | `ls /tmp/tap/github/` | Directories named after GitHub orgs (e.g. `tapfs/`, `acme/`), no flat `repos/` entry |
| 2 | Navigate into a repo | `ls /tmp/tap/github/tapfs/tap/` | Shows `issues/`, `pulls/`, `index.md` — not a `.md` file |
| 3 | Browse issues in a repo | `ls /tmp/tap/github/tapfs/tap/issues/` | One directory per issue named by title slug (e.g. `fix-login-bug/`) |
| 4 | Read an issue | `cat /tmp/tap/github/tapfs/tap/issues/fix-login-bug/index.md` | Frontmatter with `title`, `state`, `url`; body is issue description |
| 5 | Navigate comments on an issue | `ls /tmp/tap/github/tapfs/tap/issues/fix-login-bug/` | Shows `index.md` and `comments.md` (aggregate file) |
| 6 | Navigate PRs | `ls /tmp/tap/github/tapfs/tap/pulls/` | One directory per open PR |
| 7 | Use tab completion through the hierarchy | `cd /tmp/tap/github/` then `TAB` | Shell completes org names; further TAB completes repo names |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 8 | Discover all repos for an org | `ls /tmp/tap/github/tapfs/` | Returns list of repo directories for the org |
| 9 | Find open issues across a repo | `ls /tmp/tap/github/tapfs/tap/issues/` | Lists all open issues as navigable directories |
| 10 | Read issue body and metadata | `cat /tmp/tap/github/tapfs/tap/issues/fix-login-bug/index.md` | Structured frontmatter + body in one read |
| 11 | Grep issues for a keyword | `grep -r "auth" /tmp/tap/github/tapfs/tap/issues/` | Matches inside `index.md` files under each issue directory |
| 12 | List PR review status | `cat /tmp/tap/github/tapfs/tap/pulls/add-oauth/index.md` | Frontmatter contains `branch`, `target`, `status` fields |
| 13 | Traverse hierarchy without prior knowledge | `find /tmp/tap/github/tapfs/tap -name "*.md"` | Returns all readable files; no permission errors on traversal |

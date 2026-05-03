# Aggregate Collections

Collections marked `aggregate: true` in the spec appear as a single `.md` file instead of a directory. Reading it returns all resources concatenated with `---` separators. Appending new content POSTs it as a new resource.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Read all comments on an issue | `cat /tmp/tap/github/tapfs/tap/issues/fix-login-bug/comments.md` | All comments rendered sequentially, separated by `---` |
| 2 | Post a new comment by appending | Open `comments.md` in editor, add text at end, save | New comment POSTed to GitHub; appears in next `cat` |
| 3 | No directory noise for small subcollections | `ls .../issues/fix-login-bug/` | Shows `index.md` and `comments.md`; no per-comment files |
| 4 | Read comments on a draft-only issue | `cat .../issues/my-new-bug/comments.md` | Returns empty file (no error); parent doesn't exist in API yet |
| 5 | Use shell append operator to comment | `echo "Looks good!" >> comments.md` | Single-line comment POSTed to API |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 6 | Read full comment thread | `cat .../issues/26/comments.md` | Single read returns entire thread; agent parses with `---` separator |
| 7 | Summarize and reply | Read `comments.md`, generate reply, append to file | Appended suffix POSTed as new comment |
| 8 | Check for prior discussion before commenting | `grep "already fixed" comments.md` | Searches existing comments without listing individual resources |
| 9 | Handle missing parent gracefully | Read `comments.md` when parent issue is draft-only | Empty content returned; no error; agent interprets as no comments yet |
| 10 | Append multiple comments in sequence | Append text, flush, append more text, flush | Two separate POSTs; each appended chunk treated as one comment |
| 11 | No accidental overwrite | Write entire file (not append) with all existing comments + new | Idempotent: only suffix beyond canonical content is POSTed |

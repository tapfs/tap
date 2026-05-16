# Multi-connector config (`service.yaml`)

`~/.tapfs/service.yaml` is the declarative source of truth for which connectors the daemon mounts. Two entry shapes are accepted and may be mixed in the same file:

- **Bare string** — the form `tap mount <name>` writes automatically.
- **Detailed map** — `{ name, base_url?, auth_token_env? }` for per-connector overrides a human edits in by hand.

Precedence for resolved settings is `service.yaml override > ~/.tapfs/credentials.yaml > built-in spec default`.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | First-mount appends bare entry | `tap mount github` (fresh install) | `service.yaml` now has `connectors: [github]`; daemon installed and running |
| 2 | Subsequent mount is a no-op | `tap mount github` again | Daemon already running with github mounted; prints "already mounted" |
| 3 | Bare `tap mount` with no args reads config | `tap mount` (with service.yaml populated) | Daemon starts, loads every connector listed in service.yaml |
| 4 | Bare `tap mount` with empty config | `tap mount` (fresh install, no entries) | Exits non-zero with hint: `Try: tap mount github, or edit ~/.tapfs/service.yaml` |
| 5 | Hand-edit per-connector base_url | Edit service.yaml: `- name: jira\n  base_url: https://acme.atlassian.net`; restart | Jira connector uses acme.atlassian.net, not the spec default |
| 6 | Hand-edit auth_token_env | Edit service.yaml: `- name: linear\n  auth_token_env: LINEAR_CI_TOKEN`; restart | Linear connector reads bearer token from `$LINEAR_CI_TOKEN` instead of the keychain |
| 7 | `tap mount <name>` preserves overrides | Existing detailed entry for jira; `tap mount jira` | Returns "already mounted" without rewriting the entry to bare-string form |
| 8 | Remove a connector | `tap unmount jira` | IPC tells daemon to deregister; service.yaml entry removed; remaining detailed entries untouched |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 9 | Reproducible CI mount | Check `service.yaml` into a repo + `TAPFS_NO_KEYCHAIN=1` + per-connector `auth_token_env` | A fresh CI runner runs `tap mount` once and gets the full agent filesystem with no interactive auth |
| 10 | Hot-add via IPC honors overrides | Daemon up; edit service.yaml to add `- name: foo\n  base_url: https://x`; `tap mount foo` | Hot-add reads service.yaml's override; foo connector talks to `https://x` |
| 11 | Per-connector base_url survives round-trip | Hand-edit detailed entry; restart daemon; `service.yaml` rewritten by some mount op | Override preserved (only bare-string adds are auto-managed; detailed entries are never downgraded) |
| 12 | Unknown override field doesn't crash | Hand-edit detailed entry with an extra `foo_bar: 1` field | Parse fails with a clear yaml error (deny-unknown) OR the field is ignored — never a silent miscoercion |

# Authentication

`tap mount` resolves credentials in a fixed order: env var → OS keychain → interactive prompt. Secrets (token, refresh_token, client_secret) live in the OS keychain by default; non-secret metadata (email, base_url, client_id) lives in `~/.tapfs/credentials.yaml` (mode `0600`).

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | First-time mount with no creds (device flow) | `tap mount github` | OAuth2 device flow opens browser; user enters code; access + refresh tokens stored in OS keychain |
| 2 | First-time mount with no creds (browser flow) | `tap mount google` | OAuth2 web flow opens browser, exchanges code on `localhost:<random>` callback; refresh token stored in keychain |
| 3 | Subsequent mount | `tap mount github` (after #1) | Mount succeeds without prompting; token loaded from keychain |
| 4 | Override with env var | `GITHUB_TOKEN=ghp_xxx tap mount github` | Env var wins; keychain entry not consulted for token |
| 5 | Atlassian first-time mount (API token) | `tap mount jira` | Prompts for domain (e.g. `mycompany`), email, API token; saved with token in keychain, email + base_url in YAML |
| 6 | Atlassian env-var path | `ATLASSIAN_DOMAIN=acme ATLASSIAN_EMAIL=u@acme.com ATLASSIAN_API_TOKEN=t tap mount jira` | Env wins; no prompt |
| 7 | Headless / CI use | `TAPFS_NO_KEYCHAIN=1 tap mount github` | Plaintext `~/.tapfs/credentials.yaml` (mode `0600`) used for everything, including secrets |
| 8 | Mount in non-TTY without creds | `tap mount github < /dev/null` | Prints setup URL + env var hint to stderr; exits non-zero — no prompt issued |
| 9 | Credentials migration from legacy YAML | Existing `~/.tapfs/credentials.yaml` with plaintext token | First `tap mount` copies secret to keychain, leaves YAML in place so user can audit / downgrade |
| 10 | Inspect what's in the keychain (macOS) | `security find-generic-password -s tapfs -a github` | Returns the JSON blob `{"token":"...","refresh_token":"..."}` for the github connector |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 11 | Detect missing auth on a connector | Mount fails with auth error (CI / agent shell) | Agent surfaces clear instructions from stderr (setup URL, env var) instead of a stack trace |
| 12 | No token bytes in audit log | Mount with valid token, then `tap log -n 5` | Mount-related entries don't include the secret token in any field |
| 13 | Per-connector keychain isolation | `tap mount github` then `tap mount linear` | Two distinct keychain entries under service `tapfs`, users `github` and `linear`; revoking one leaves the other intact |
| 14 | Token rotation | User revokes old GitHub token, runs `tap mount github` again | Auth flow re-runs (device flow); new token replaces old keychain entry |
| 15 | Atlassian metadata persists across runs | `tap mount jira`, restart machine, `tap mount jira` | Domain + email reread from YAML; token from keychain; no re-prompt |

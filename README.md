# tapfs

**Mount any REST API as a filesystem. Read with `cat`, write with `echo`, create with `mkdir` — versioned, audited, agent-ready.**

[![CI](https://github.com/tapfs/tap/actions/workflows/ci.yml/badge.svg)](https://github.com/tapfs/tap/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

```bash
$ tap mount github

$ ls /mnt/tap/github/tapfs/tap/issues/
fix-auth-bug/   add-dark-mode/   api-rate-limit/

$ cat /mnt/tap/github/tapfs/tap/issues/fix-auth-bug/index.md   # GET
---
_id: 42
title: Fix auth bug
state: open
url: https://github.com/tapfs/tap/issues/42
---
Session tokens expire too early on mobile.

$ cat /mnt/tap/github/tapfs/tap/issues/fix-auth-bug/comments.md  # all comments
Found the root cause — clock skew in the token validator.
---
PR up: #87

$ mkdir /mnt/tap/github/tapfs/tap/issues/my-new-bug    # draft — no API call yet
$ vi /mnt/tap/github/tapfs/tap/issues/my-new-bug/index.md  # fill in title, body
$ # remove _draft: true from frontmatter, save → POST to GitHub

$ tap log -n 1                                         # every operation audited
2026-04-24 20:14 [create] github/tapfs/tap/issues/my-new-bug  success
```

**Any REST API becomes a navigable directory tree.** Reads hit the live API. Writes patch it. `mkdir` creates (on connectors that support it) — starting as a local draft until you publish. Every operation is versioned and audit-logged. No SDK. No framework.

## Install

```bash
curl -fsSL https://tapfs.dev/install.sh | sh
```

Or: `brew install tapfs/tap/tapfs` | [Docker](#docker)

## Try it now

```bash
# No API key needed — public demo API
tap mount rest -s connectors/rest.yaml
ls /mnt/tap/rest/items/
cat /mnt/tap/rest/items/1.md

# Then connect a real API — opens a browser / device flow on first run,
# then stores the token in the OS keychain.
tap mount github
```

## What you get

tapfs mounts REST APIs as a filesystem and adds transactional semantics through path conventions:

| Path | What it does |
|------|-------------|
| `cat resource/index.md` | Live read (GET) |
| `echo "..." > resource/index.md` | Live write (PATCH) |
| `mkdir collection/new-resource` | Draft new resource — local until published |
| `cat resource/comments.md` | Read aggregate subcollection (all items in one file) |
| `echo "reply" >> comments.md` | Append = POST new item (on supporting connectors) |
| `cat resource@v3.md` | Read immutable version snapshot |
| `cat resource.lock` | Check or acquire a transaction lock |
| `tap log` | Full audit trail of every operation |
| `tap rollback resource@v1.md` | Roll back to any version |

Drafts start with `_draft: true` in frontmatter — remove it and save to publish to the API. Versions are automatic on every write. Audit logging is always on. Create and delete are per-connector capabilities (`capabilities.create/delete` plus `delete_endpoint` per collection); today GitHub labels, Linear issues, Notion pages (archive), and Stripe customers can be deleted via `rm -rf`.

## Why

LLMs are trained on Unix. They know `ls`, `cat`, `grep`, and `diff`. Enterprise data lives behind REST APIs that need SDKs, auth wrappers, and custom tool definitions per service.

tapfs sits at the OS layer — below MCP, below LangChain, below orchestrators. Any agent that reads files gets sandboxed transactions, versioning, and governance on every API it touches. No integration code. No framework lock-in.

|                    | **tapfs**          | MCP            | API Gateways   | LangChain Tools  |
|--------------------|--------------------|----------------|----------------|------------------|
| Abstraction        | OS (filesystem)    | Protocol       | Network        | Application      |
| Agent integration  | Zero (POSIX)       | SDK required   | SDK required   | Framework-locked |
| Transactions       | Built-in (drafts)  | No             | No             | No               |
| Versioning         | Built-in           | No             | No             | No               |
| Audit trail        | Built-in           | No             | Partial        | No               |
| Discovery          | `ls`               | Tool listing   | API catalog    | Code             |

## Connectors

20 connectors ship out of the box. Most are a YAML file — no code.

GitHub | GitLab | Google Workspace | Salesforce | Jira | Confluence | Slack | Discord | Notion | Stripe | Shopify | HubSpot | Linear | Asana | ClickUp | Zendesk | PagerDuty | ServiceNow | Mailchimp | SendGrid | Cloudflare | + generic REST template

```bash
tap connectors              # list available
tap mount salesforce        # mount one
tap mount github jira       # mount several
```

### Authentication

`tap mount <connector>` checks for credentials in this order:

1. The connector's environment variable (e.g. `GITHUB_TOKEN`, `LINEAR_API_TOKEN`).
2. The OS keychain (macOS Keychain, Linux Secret Service, Windows Credential Manager).
3. An interactive prompt — OAuth2 device flow / browser flow when the connector supports it, or an API-key prompt when it doesn't. Whatever you enter is saved to the keychain for next time.

Set `TAPFS_NO_KEYCHAIN=1` to use a plaintext `~/.tapfs/credentials.yaml` instead — useful in CI or headless containers.

### Declarative multi-connector config

`tap mount <name>` appends to `~/.tapfs/service.yaml`. You can hand-edit it to add per-connector overrides:

```yaml
mount_point: /tmp/tap
connectors:
  - github
  - name: jira
    base_url: https://acme.atlassian.net
  - name: linear
    auth_token_env: LINEAR_CI_TOKEN
```

Running bare `tap mount` (no positional arg, no `--spec`) loads everything in `service.yaml`. Check it into a repo + `TAPFS_NO_KEYCHAIN=1` + `auth_token_env` overrides for a reproducible CI mount.

### Add your own

```yaml
name: my-api
base_url: https://api.example.com
auth:
  type: bearer
  token_env: MY_API_TOKEN
collections:
  - name: items
    list_endpoint: /items
    get_endpoint: /items/{id}
    slug_field: name
```

```bash
tap mount rest -s ./my-connector.yaml
```

## Agent setup

Every mount includes an `AGENTS.md` that teaches the agent what's available — collections, operations, draft conventions, and tips. The agent discovers the API by reading, not by being pre-configured.

```bash
tap setup claude --append   # Claude Code
```

Also: [Codex](plugins/codex/) | [OpenClaw](plugins/openclaw/) | [E2B](templates/e2b/)

## Docker

```bash
# Demo (no API key)
docker run --rm -it --privileged tapfs/tap

# Real API
docker run --rm -it --privileged \
  -e TAPFS_CONNECTOR=github \
  -e GITHUB_TOKEN="ghp_..." \
  tapfs/tap
```

## How it works

```
Agent ──read/write──▶ virtual filesystem ──▶ VirtualFs ──▶ Connector ──▶ REST API
                                       |
                                  DraftStore (copy-on-write sandboxing)
                                  VersionStore (immutable snapshots)
                                  GovernanceLayer (audit log, approval gates)
                                  Cache (TTL-based, sparse hydration)
```

macOS | Linux | Docker (via NFS)

## Status

v0.1 alpha. Core read/write/draft/version path works. [Issues](https://github.com/tapfs/tap/issues) track what's next.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache 2.0

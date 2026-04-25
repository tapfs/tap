# tapfs

**Transactions, drafts, versioning, and audit trails for enterprise APIs — through plain filesystem operations.**

[![CI](https://github.com/tapfs/tap/actions/workflows/ci.yml/badge.svg)](https://github.com/tapfs/tap/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

```bash
$ tap mount salesforce

$ ls /mnt/tap/salesforce/accounts/
acme-corp.md
globex.md

$ cat acme-corp.md                        # GET — live from the API
---
id: 001xx00001ABC
type: salesforce/account
modified: 2026-04-10T14:22:00Z
owner: jane@acme.com
---
# Acme Corp
Industry: Technology
Annual Revenue: $4.2M
...

$ cp acme-corp.md acme-corp.draft.md      # sandbox — no API call
$ vi acme-corp.draft.md                   # edit freely, still no API call
$ mv acme-corp.draft.md acme-corp.md      # promote — one atomic PATCH
$ cat acme-corp@v1.md                     # previous version, auto-saved
$ tap log -n 1                            # every operation audited
2026-04-24 20:14 [write] salesforce/accounts/acme-corp.md  promote-draft  847→912 bytes
```

**One flow. Six filesystem operations. You get**: live API reads, sandboxed drafts, atomic writes, automatic versioning, and an audit trail. No SDK. No framework. Any agent or script that can read and write files gets all of this for free.

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

# Then connect a real API
export GITHUB_TOKEN="ghp_..."
tap mount github
```

## What you get

tapfs mounts REST APIs as a filesystem and adds transactional semantics through path conventions:

| Path | What it does |
|------|-------------|
| `cat resource.md` | Live read (GET) |
| `echo "..." > resource.md` | Live write (PATCH) |
| `cp resource.md resource.draft.md` | Create sandboxed copy — edits stay local |
| `mv resource.draft.md resource.md` | Promote draft to live ��� atomic API write |
| `cat resource@v3.md` | Read immutable version snapshot |
| `cat resource.lock` | Check or acquire a transaction lock |
| `tap log` | Full audit trail of every operation |
| `tap rollback resource@v1.md` | Roll back to any version |

Drafts are copy-on-write. Versions are automatic on every promote. Audit logging is always on. The agent doesn't need to know any of this — it just reads and writes files, and tapfs handles the rest.

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

14 connectors ship out of the box. Each is a YAML file — no code.

GitHub | Google Workspace | Salesforce | Jira | Slack | Notion | Stripe | HubSpot | Linear | Zendesk | PagerDuty | ServiceNow | + generic REST template

```bash
tap connectors              # list available
tap mount salesforce        # mount one
tap mount github jira       # mount several
```

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

Every mount includes an `agent.md` that teaches the agent what's available — collections, operations, draft conventions, and tips. The agent discovers the API by reading, not by being pre-configured.

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
Agent ──read/write──▶ FUSE/NFS ──▶ VirtualFs ──▶ Connector ──▶ REST API
                                       |
                                  DraftStore (copy-on-write sandboxing)
                                  VersionStore (immutable snapshots)
                                  GovernanceLayer (audit log, approval gates)
                                  Cache (TTL-based, sparse hydration)
```

Linux (FUSE) | macOS (NFS) | Docker

## Status

v0.1 alpha. Core read/write/draft/version path works. [Issues](https://github.com/tapfs/tap/issues) track what's next.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache 2.0

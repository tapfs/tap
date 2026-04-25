# tapfs

**Mount enterprise APIs as files. Your agent already knows how.**

[![CI](https://github.com/tapfs/tap/actions/workflows/ci.yml/badge.svg)](https://github.com/tapfs/tap/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

tapfs is a FUSE filesystem that mounts enterprise REST APIs as agent-readable files. No SDKs. No wrappers. Just `ls`, `cat`, and `grep`.

```
/tap/
  salesforce/
    accounts/
      acme-corp.md           # live resource (GET on read, PATCH on write)
      acme-corp.draft.md     # sandboxed copy, isolated from live
      acme-corp@v3.md        # pinned immutable snapshot
      acme-corp.lock         # transaction lock
    opportunities/
      q3-renewal.md
  google/
    drive/
      design-doc.md
    calendar/
      team-standup.md
```

Agents don't need new APIs. They already know files. tapfs sits at the OS layer, below frameworks and orchestrators, and works with any agent that can read and write files.

## Quick Start

### From source

```bash
git clone https://github.com/tapfs/tap.git
cd tap
cargo build --release
```

### Docker

```bash
# Try the demo (no API key needed)
docker run --rm -it --privileged tapfs/tap

# Mount a real API
docker run --rm -it --privileged \
  -e TAPFS_CONNECTOR=github \
  -e GITHUB_TOKEN="ghp_..." \
  tapfs/tap

# Interactive shell
docker run --rm -it --privileged tapfs/tap shell
```

### Mount a connector

```bash
# Mount with a connector spec
tap mount github -s connectors/github.yaml

# Mount Google Workspace (native connector)
tap mount google

# Mount with explicit base URL
tap mount rest -s connectors/stripe.yaml --base-url https://api.stripe.com
```

### Use it

```bash
ls /mnt/tap/google/drive/          # list files
cat /mnt/tap/google/drive/doc.md   # read a resource (triggers GET)
echo "update" > /mnt/tap/google/drive/doc.draft.md  # sandboxed write
tap approve /mnt/tap/google/drive/doc.md             # push draft to live
tap log -n 10                      # view audit trail
```

## How It Works

tapfs translates filesystem operations into API calls:

| Operation | API Effect |
|-----------|-----------|
| `cat file.md` | `GET /resource` |
| `echo "data" > file.md` | `PATCH /resource` |
| `cat file.draft.md` | Read sandboxed draft |
| `echo "data" > file.draft.md` | Write to sandbox (no API call) |
| `tap approve file.md` | Push draft to live (`PATCH`) |
| `cat file@v3.md` | Read immutable snapshot |
| `cat file.lock` | Check/acquire lock |

Every operation is audited, versioned, and sandboxed by default. Compliance is structural, not instructed.

## Connectors

tapfs ships with YAML connector specs for common APIs:

| Connector | APIs |
|-----------|------|
| Google Workspace | Drive, Gmail, Calendar |
| Salesforce | Accounts, Opportunities, Cases |
| Jira | Issues, Projects, Sprints |
| GitHub | Repos, Issues, Pull Requests |
| Slack | Channels, Messages |
| Notion | Pages, Databases |
| Stripe | Customers, Charges, Invoices |
| Zendesk | Tickets, Users |
| PagerDuty | Incidents, Services |
| Linear | Issues, Projects |
| ServiceNow | Incidents, Changes |
| HubSpot | Contacts, Companies, Deals |

Custom connectors are YAML files. See [`connectors/`](connectors/) for examples.

## Agent Integrations

tapfs works with AI coding agents out of the box:

- **Claude Code** -- [`plugins/claude-code/`](plugins/claude-code/)
- **Codex** -- [`plugins/codex/`](plugins/codex/)
- **OpenClaw** -- [`plugins/openclaw/`](plugins/openclaw/)
- **E2B** -- [`templates/e2b/`](templates/e2b/)

## CLI Reference

```
tap mount <connector> -m <path>    Mount a connector
tap unmount <path>                 Unmount
tap status                         Show mount status
tap log [-n N]                     View audit log
tap versions <path>                List resource versions
tap rollback <path@vN>             Rollback to version
tap pending                        List pending approvals
tap approve <path>                 Approve a draft
tap install <source>               Install connector from Git
tap connectors                     List installed connectors
tap remove <name>                  Remove a connector
tap update <name>                  Update a connector
```

## Architecture

```
  Agent (Claude, Codex, any process)
    |
    |  read() / write() / readdir()
    v
  +-----------+
  |   FUSE    |  (Linux) or NFS loopback (macOS)
  +-----------+
    |
    v
  +-----------+
  | VirtualFs |  Platform-agnostic virtual filesystem core
  +-----------+
    |
    +-- DraftStore      Sandboxed writes (copy-on-write)
    +-- VersionStore    Immutable snapshots
    +-- GovernanceLayer Audit logging, approval gates
    +-- Cache           TTL-based response cache
    |
    v
  +-----------+
  | Connector |  REST connector (YAML-driven) or native (Google, Jira)
  +-----------+
    |
    v
  Enterprise API (Salesforce, Google, Jira, ...)
```

## Platform Support

| Platform | Transport | Status |
|----------|-----------|--------|
| Linux | FUSE | Supported |
| macOS | NFS loopback | Supported |
| macOS | File Provider Extension | Experimental ([`macos/`](macos/)) |
| Docker | FUSE (privileged) | Supported |

## Known Limitations

tapfs is v0.1 alpha software. Known limitations include:

- **Read performance**: Full resource content is cloned per FUSE read chunk
- **Write performance**: Each 4KB FUSE write triggers a full read-modify-write cycle
- **File sizes**: Dynamic content reports hardcoded sizes (breaks `rsync`, `make`)
- **No pagination**: Large collections (100K+ resources) may exhaust memory
- **No delete**: `rm` on live resources is not yet supported
- **No concurrency limit**: Concurrent reads can fire unlimited HTTP requests

See the [Roadmap](docs/ROADMAP.md) for the full list of planned improvements.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

Apache 2.0. See [LICENSE](LICENSE).

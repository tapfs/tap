# tapfs

**Tap into enterprise APIs. Your agent already knows how.**

An open source FUSE layer that mounts any enterprise API as a transactional, agent-native filesystem.

> *Origin: tapfs started as PILL — POSIX Interface for Legacy Layers. The acronym stuck internally; the name shipped as tap.*

---

## The Problem

Agents live in sandboxes and think in files. Enterprise systems speak REST, GraphQL, proprietary SDKs. Every integration requires custom wiring, auth, error handling — and produces no audit trail, no rollback, no isolation by default. Skill proliferation is the inevitable result: one skill per API, per auth scheme, per error contract.

## What tapfs Is

A FUSE-based mount layer. Point it at enterprise backends and agents get a unified POSIX surface. No new abstractions. No SDKs. `ls`, `cat`, `sed`, `grep`, `diff` work out of the box.

```
/tap/
  salesforce/
    accounts/
      acme-corp.md              ← live resource (GET on read, PATCH on write)
      acme-corp.draft.md        ← sandboxed copy, isolated from live
      acme-corp@v3.md           ← pinned immutable snapshot
      acme-corp.lock            ← transaction lock
    opportunities/
      q3-renewal.md
  hubspot/
    contacts/
      onboarding.md
      onboarding.draft.md
    lists/
      high-value.md
  linear/
    tasks/
      issue-456.md
  servicenow/
    incidents/
      inc-7890.md
```

## What tapfs Is Not

Not an agent framework. Not an API gateway. Not a database. Not competing with LangChain, MCP, or orchestrators. tapfs sits below all of them — at the OS layer. Works with any agent that can read and write files, which is all of them.

---

## Guiding Principles

### 1. Floor, not ceiling
Operates at the lowest possible system level. FUSE, not middleware. The agent never knows tapfs exists — it just sees files.

### 2. POSIX is the API
No new abstractions. If an LLM was trained on Unix, it already knows how to use tapfs.

### 3. Enterprise is pre-mounted
Auth, permissions, rate limits, multi-tenancy — resolved transparently at the mount layer. The agent never negotiates credentials.

### 4. Discovery is progressive
`ls /tap` shows what's available. `cat` a resource and its context comes with it. The agent builds understanding by exploring, not by being pre-configured.

### 5. Context is structural
Knowledge, metadata, relationships aren't separate retrieval steps — they're part of the filesystem. The journey file contains its context. The segment file contains its lineage.

### 6. The filesystem is self-describing
Every mount point contains an `agent.md` — a natural language file that teaches the agent what's available, how to navigate, and how to use tapfs conventions like drafts, locks, and transactions. The agent discovers capabilities by reading, the way it reads any file. `cat /tap/salesforce/agent.md` is the onboarding.

### 7. Transparent governance
Every operation is audited, versioned, sandboxed — without the agent doing anything. Compliance is structural, not instructed.

### 8. Resources are documents, not API responses
A file is not a JSON dump of an API endpoint. It's a readable document — with a title, narrative body, and structured metadata — that an agent (or human) can understand without knowing the underlying API schema. Connectors define how raw API data is rendered into meaningful files. Multiple API calls may be composed into a single resource when that's what makes the document coherent (e.g., an issue file includes its comments).

---

## Transactional Semantics

Transactions are expressed through path conventions — no transaction API, no commit commands. The filesystem *is* the transaction protocol.

| Path Pattern | Semantic |
|---|---|
| `resource.md` | Live read/write. READ = GET, WRITE = PATCH. |
| `resource.draft.md` | Isolated sandbox copy. Agent works here freely. Nothing hits live until promoted. |
| `resource@v3.md` | Immutable pinned snapshot. Read-only. |
| `resource.lock` | Optimistic/pessimistic transaction lock. Presence = held. |
| `.tx/` folder | Open transaction context with multi-resource atomicity. |

### Draft → Commit Flow

```
1. Agent creates draft:     cp journey.md journey.draft.md
2. Agent edits draft:       sed -i 's/old/new/' journey.draft.md
3. Agent (or human) promotes: mv journey.draft.md journey.md
4. tapfs translates promotion → API PATCH/PUT to live backend
5. Previous live version auto-snapshotted as journey@v{n}.md
```

Draft creation is **copy-on-write** — zero cost until first edit. Promotion is a pointer swap internally; only the delta hits the API.

### Multi-Resource Transactions

```
mkdir /tap/hubspot/.tx/campaign-update
cp contacts/onboarding.md .tx/campaign-update/
cp lists/high-value.md .tx/campaign-update/
# Agent edits both inside .tx/
# Commit = rmdir .tx/campaign-update → atomic API calls
# Abort  = rm -rf .tx/campaign-update → discard all
```

---

## Filesystem Architecture

### Sparse Hydration (iCloud / Dropbox model)

The entire enterprise is mounted. The agent sees everything. Bytes only move when intent is clear.

- **List without fetch**: `ls /tap/salesforce/accounts/` returns 50k entries instantly from cached metadata. Zero API calls for discovery.
- **Demand-fetch on read**: `cat acme-corp.md` triggers the actual API call. Pay-per-access.
- **Metadata-first**: Filename, schema, last modified, owner, permissions available before content is fetched. Agents can reason about what exists without API cost.

### Cache & Eviction

- API responses cached locally with configurable TTL per connector
- Stale read → tapfs silently re-fetches (transparent to agent)
- Memory pressure → evict cold resources, re-fetch on demand
- Cache coherence is tapfs's problem, never the agent's

### Copy-on-Write Drafts (APFS model)

- `cp resource.md resource.draft.md` — instant, no bytes copied
- Only deltas stored on first edit
- Promoting draft back to live = pointer swap + API commit

### Point-in-Time Snapshots

```bash
tap snapshot                    # freeze consistent view across all mounts
tap snapshot --scope salesforce # freeze single backend
```

Agent operates on snapshot while live data changes underneath. Critical for multi-step workflows requiring a stable world view.

### Version Timeline

```bash
ls onboarding@              # list all versions
cat onboarding@v3.md        # read specific version
diff onboarding@v3.md onboarding@v5.md  # compare versions
```

Versions reconstructed from tapfs's write log or backend change history.

### Offline / Disconnected Operation

- Agent writes to local cache; tapfs queues commits
- If backend is unreachable, agent keeps working on draft layer
- Conflict resolution on reconnect (last-write-wins default, configurable)

---

## Connector System

### Connector Spec

A connector defines how a REST/GraphQL/proprietary API maps to filesystem paths. Declarative YAML + optional LLM-assisted mapping.

```yaml
# connectors/salesforce.yaml
name: salesforce
base_url: https://{instance}.salesforce.com
auth:
  type: oauth2
  flow: client_credentials
  
mounts:
  accounts:
    list: GET /services/data/v{api_version}/sobjects/Account
    read: GET /services/data/v{api_version}/sobjects/Account/{id}
    write: PATCH /services/data/v{api_version}/sobjects/Account/{id}
    format: markdown   # tapfs renders API JSON → agent-readable .md
    
  opportunities:
    list: GET /services/data/v{api_version}/sobjects/Opportunity
    read: GET /services/data/v{api_version}/sobjects/Opportunity/{id}
    write: PATCH /services/data/v{api_version}/sobjects/Opportunity/{id}
```

### agent.md — The Agent's Onboarding File (Principle 6)

Every mount point and every subdirectory can contain an `agent.md`. This is a natural language file — markdown — that teaches the agent how to work with what's in front of it.

```
ls /tap/                                 # what backends are mounted?
cat /tap/agent.md                        # how does tapfs work?
ls /tap/salesforce/                      # what resource types exist?
cat /tap/salesforce/agent.md             # how do I work with Salesforce here?
ls /tap/salesforce/accounts/             # what accounts exist?
cat /tap/salesforce/accounts/acme-corp.md  # read a specific resource
```

#### Root agent.md (`/tap/agent.md`)

Generated by tapfs core. Teaches the agent the filesystem conventions:

```markdown
# tapfs

You have access to enterprise systems mounted as files. Use standard Unix commands.

## Mounted backends
- salesforce/ — CRM (accounts, opportunities, contacts)

## Reading
- `ls` to browse, `cat` to read any .md file
- Each directory has its own `agent.md` with specifics

## Writing safely
- Never write directly to a live resource
- Instead: `cp resource.md resource.draft.md` → edit the draft → `mv resource.draft.md resource.md` to promote
- tapfs handles the API call on promotion
- Previous version is auto-saved as resource@v{n}.md

## Transactions across resources
- `mkdir .tx/my-change` → copy resources in → edit → `rmdir .tx/my-change` to commit atomically
- `rm -rf .tx/my-change` to abort

## Locking
- `touch resource.lock` before editing to prevent concurrent writes
- `rm resource.lock` when done

## Versions
- `ls resource@` to see all versions
- `cat resource@v3.md` to read a specific version
- `diff resource@v3.md resource@v5.md` to compare
```

#### Connector agent.md (`/tap/salesforce/agent.md`)

Generated from the connector spec. Teaches the agent what this specific backend offers:

```markdown
# Salesforce

Authenticated as svc-agent@acme.com (OAuth2)

## Resources
- accounts/      — CRM accounts (read, write, draft, versions)
- opportunities/ — Sales opportunities (read, write, draft)
- contacts/      — Contact records (read, write)
- cases/         — Support cases (read-only)

## Rate limits
100 requests/min. Bulk operations supported for batch reads.

## Relationships
Accounts contain related opportunities and contacts.
When reading an account, check `related:` in the frontmatter for linked resources.

## Tips
- Account filenames use slugified company names: `acme-corp.md`
- Opportunities use deal names: `q3-renewal.md`
- To find a specific account, `ls accounts/ | grep acme`
```

#### Resource frontmatter

Every resource file includes a YAML frontmatter header with inline context:

```markdown
---
id: 001xx00001ABC
type: salesforce/account
modified: 2026-03-18T14:22:00Z
owner: jane.doe@acme.com
related:
  - opportunities/q3-renewal.md
  - contacts/john-smith.md
operations: [read, write, draft, lock]
---

# Acme Corp

Industry: Technology
Annual Revenue: $45M
...
```

#### Why agent.md matters

The agent builds understanding by reading `agent.md` files as it navigates — just like a developer reading a README. This is the natural extension point: connector authors can teach agents domain-specific patterns, best practices, and guardrails without any framework integration. And because it's markdown, it's versionable, diffable, and human-reviewable.

Future: `agent.md` is where agentic intelligence eventually lives — dynamic context, learned patterns, connector-specific reasoning hints — all in a format every LLM already understands.

### Tap Registry

Connectors are distributed through a registry system inspired by Terraform providers and Homebrew taps.

#### Registry Architecture

```
registry.tapfs.dev/
  official/                     # maintained by tapfs core team
    salesforce/
    jira/
    servicenow/
    rest/                       # generic REST adapter
  verified/                     # third-party, reviewed & signed
    snowflake/warehouse
    stripe/payments
  community/                    # anyone can publish
    acme-corp/internal-crm
    jdoe/custom-erp
```

Three tiers:

| Tier | Namespace | Trust | Review |
|---|---|---|---|
| **Official** | `tap install salesforce` | Core team maintained | Full |
| **Verified** | `tap install snowflake/warehouse` | Vendor-signed, registry-reviewed | Schema + security audit |
| **Community** | `tap install jdoe/custom-erp` | As-is, community-flagged | Automated checks only |

#### Connector Package Format

Each connector is a self-contained package with a manifest, schema, agent.md, and tests:

```
salesforce/
  tap.yaml              # manifest: name, version, dependencies, auth requirements
  agent.md              # natural language guide for agents using this connector
  schema/
    accounts.yaml       # path mappings, CRUD ops, field definitions
    opportunities.yaml
    contacts.yaml
  tests/
    smoke.yaml          # basic connectivity & CRUD validation
  README.md             # human-facing docs
  CHANGELOG.md
```

#### Manifest (`tap.yaml`)

```yaml
name: salesforce
namespace: official
version: 1.2.0
description: Salesforce CRM connector for tapfs
license: Apache-2.0

requires:
  tapfs: ">=0.2.0"
  auth: oauth2

config:
  instance:
    type: string
    required: true
    description: "Salesforce instance (e.g. na1, eu5, or custom domain)"
  api_version:
    type: string
    default: "v59.0"

mounts:
  - accounts
  - opportunities
  - contacts
  - leads
  - cases

capabilities:
  read: true
  write: true
  draft: true
  versions: true          # backend supports change history
  bulk: true              # supports batch operations
  webhooks: true          # can subscribe to change events
```

#### CLI — Registry Operations

```bash
# Discovery
tap search crm                          # full-text search across registry
tap search --tier official              # filter by trust tier
tap browse                              # open registry.tapfs.dev in browser
tap info salesforce                     # show manifest, versions, dependencies

# Installation
tap install salesforce                  # latest official
tap install salesforce@1.2.0            # pinned version
tap install snowflake/warehouse          # verified, namespaced
tap install jdoe/custom-erp             # community
tap install ./my-connector              # local development

# Management
tap list                                # installed connectors
tap outdated                            # check for updates
tap upgrade salesforce                  # upgrade single connector
tap upgrade --all                       # upgrade everything
tap remove servicenow                   # uninstall
tap pin salesforce@1.2.0               # lock version, skip upgrades

# Publishing
tap init                                # scaffold new connector project
tap validate                            # lint & test connector locally
tap publish                             # push to registry (requires auth)
tap publish --tier community            # explicit tier (default)
```

#### Lockfile (`tap.lock`)

Pinned dependency resolution, checked into source control — same pattern as `package-lock.json` or `terraform.lock.hcl`:

```yaml
# tap.lock — auto-generated, do not edit
lockfile_version: 1
connectors:
  salesforce:
    version: 1.2.0
    tier: official
    sha256: "a1b2c3d4..."
    source: registry.tapfs.dev/official/salesforce
  snowflake/warehouse:
    version: 0.8.3
    tier: verified
    sha256: "e5f6g7h8..."
    source: registry.tapfs.dev/verified/snowflake/warehouse
```

#### Private Registries

Enterprises run internal connectors that never hit the public registry:

```bash
# Configure private registry
tap registry add internal https://tap-registry.acme-corp.com
tap registry list
tap registry set-default internal

# Install from private registry
tap install internal::legacy-mainframe
tap install internal::sap-custom

# Priority: local → private → public
```

Private registries use the same package format and API. Ship as a Docker container or a static file server — no special infrastructure required.

#### Connector Development

```bash
tap init my-connector                   # scaffold project
cd my-connector
# edit tap.yaml and schema/*.yaml
tap validate                            # lint, schema check, dry-run mount
tap test                                # run smoke tests against a sandbox
tap dev mount ./                        # mount local connector for live testing
tap publish                             # push to registry
```

The `tap init` scaffold includes a generic REST template. For most SaaS APIs, building a connector is editing YAML — no code required. Complex APIs (GraphQL, gRPC, custom auth) can include a plugin binary.

#### The Flywheel

The connector registry is the moat. Once the community writes connectors, tapfs becomes the standard mount layer for enterprise agents — the way ODBC became the standard for databases, or Terraform providers became the standard for infrastructure.

---

## Governance Layer

Governance is structural, not instructed. Every filesystem operation is an interception point.

| Concern | Mechanism |
|---|---|
| **Audit** | Every `open`, `read`, `write`, `unlink` logged with agent ID, timestamp, resource path |
| **Access control** | Enterprise permissions mapped to POSIX file permissions. Agent can't read what the human can't read. |
| **Rate limiting** | Per-agent, per-backend throttling at the FUSE layer |
| **Rollback** | Any write is reversible via version timeline |
| **Sandbox isolation** | `.draft` copies are agent-local. No cross-agent contamination. |
| **Approval gates** | Promotion from `.draft` → live can require human approval (configurable per path) |

---

## CLI

```bash
tap mount salesforce --scope accounts,opportunities
tap mount hubspot
tap ls                          # show all mounted backends
tap status                      # connection health, cache stats
tap snapshot                    # point-in-time freeze
tap log                         # audit trail
tap connectors                  # list installed connectors
tap install <connector>         # add from registry
tap unmount salesforce
```

---

## Architecture Overview

```
┌─────────────────────────────────────────────┐
│               Agent Sandbox                  │
│   (Claude, GPT, LangChain, any POSIX agent) │
│                                              │
│   cat /tap/salesforce/accounts/acme.md       │
└──────────────────┬──────────────────────────┘
                   │ POSIX syscall (read)
┌──────────────────▼──────────────────────────┐
│               tapfs FUSE Layer               │
│                                              │
│  ┌───────────┐ ┌──────────┐ ┌─────────────┐ │
│  │ Path      │ │ Cache &  │ │ Transaction │ │
│  │ Router    │ │ Hydration│ │ Manager     │ │
│  │           │ │ (CoW)    │ │ (.draft/    │ │
│  │           │ │          │ │  .lock/.tx) │ │
│  └─────┬─────┘ └────┬─────┘ └──────┬──────┘ │
│        │             │              │         │
│  ┌─────▼─────────────▼──────────────▼──────┐ │
│  │           Governance Interceptor         │ │
│  │   (audit, ACL, rate-limit, approval)     │ │
│  └─────────────────┬───────────────────────┘ │
│                    │                          │
│  ┌─────────────────▼───────────────────────┐ │
│  │         Connector Abstraction            │ │
│  │  ┌────────┐ ┌─────┐ ┌──────────┐       │ │
│  │  │Salesfrc│ │Strpe│ │ServiceNow│ ...   │ │
│  │  └────────┘ └─────┘ └──────────┘       │ │
│  └─────────────────┬───────────────────────┘ │
└────────────────────┼─────────────────────────┘
                     │ REST / GraphQL / gRPC
         ┌───────────▼───────────┐
         │   Enterprise APIs     │
         └───────────────────────┘
```

---

## Positioning

| | tapfs | MCP | API Gateways | LangChain Tools |
|---|---|---|---|---|
| Abstraction level | OS (FUSE) | Protocol | Network | Application |
| Agent integration | Zero (POSIX) | SDK required | SDK required | Framework-locked |
| Transactional | Built-in | No | No | No |
| Governance | Structural | Bolt-on | Partial | None |
| Discovery | `ls` | Tool listing | API catalog | Code |
| Offline capable | Yes | No | No | No |

tapfs doesn't compete with MCP — it complements it. MCP exposes resources; tapfs gives them POSIX semantics. A tapfs connector *can be* an MCP server underneath.

---

## Open Source Strategy

- **Core**: FUSE driver, transaction manager, cache layer, connector spec — Apache 2.0
- **Connectors**: Community-contributed, registry-hosted
- **Governance extensions**: Approval workflows, compliance templates — potential commercial tier

The moat is the connector registry. Ship core + 5 connectors (Salesforce, ServiceNow, Jira, a generic REST adapter, one Snowflake connector). Community builds the rest.

---

## What Ships First (v0.1)

1. FUSE mount with single REST connector (generic)
2. `.draft` / `.lock` / `@version` path conventions working
3. `cat` / `ls` / `cp` / `mv` translating to API calls
4. Local cache with TTL eviction
5. Basic audit log (`tap log`)
6. `tap mount` / `tap unmount` CLI
7. One real connector (Salesforce or Jira) as proof of concept

---

*tapfs doesn't teach agents new tricks. It reveals enterprise reality in the language they already speak.*

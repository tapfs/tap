---
name: tapfs
description: Browse and modify enterprise API resources mounted as local files. Use when the user asks about Jira issues, GitHub repos, Salesforce accounts, or any REST API data.
---

## How it works

Enterprise API resources are mounted as a local filesystem at `/tmp/tap`. You can browse them with standard file tools — no API calls needed.

## Quick start

1. Read the help file first:
   ```
   cat /tmp/tap/agent.md
   ```

2. List available connectors:
   ```
   ls /tmp/tap/
   ```

3. Browse a connector's collections:
   ```
   ls /tmp/tap/<connector>/
   ```

4. List resources in a collection:
   ```
   ls /tmp/tap/<connector>/<collection>/
   ```

5. Read a resource:
   ```
   cat /tmp/tap/<connector>/<collection>/<slug>.md
   ```

## Directory layout

```
/tmp/tap/
  agent.md                              Help file
  <connector>/                          One dir per API connector
    agent.md                            Connector-specific help
    <collection>/                       One dir per resource type
      <slug>.md                         Live resource (read from API)
      <slug>.draft.md                   Local draft (not yet pushed)
      <slug>.lock                       Lock file (prevents conflicts)
```

## Writing changes

1. Create a draft: write to `<slug>.draft.md`
2. Promote to API: `mv <slug>.draft.md <slug>.md`
3. Lock before editing: `touch <slug>.lock`
4. Unlock when done: `rm <slug>.lock`

## Search across resources

```
grep -r "keyword" /tmp/tap/<connector>/<collection>/
```

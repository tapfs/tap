---
name: tapfs
description: Browse and modify enterprise API resources mounted as local files. Use when the user asks about Jira issues, GitHub repos, Salesforce accounts, or any REST API data.
---

## When to use this skill

When the user asks about issues, tickets, pull requests, documents, accounts, contacts, or any enterprise data — check the tapfs mount first.

**You do NOT need the user to mention tapfs, paths, or filenames.** Questions like "what bugs do we have?", "summarize open PRs", or "find overdue tickets" should trigger you to explore the mount.

## How to start

tapfs is already running. Read the help file for what's available:

```
cat ${user_config.mountPoint}/agent.md
```

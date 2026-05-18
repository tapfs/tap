# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- Crate-level documentation in `lib.rs`
- `# Safety` docs on all FFI functions
- `ConnectorError` typed error enum for structured error handling
- Audit log rotation (10 MB size-based, keeps 3 rotated files)
- Graceful shutdown: signal handlers flush pending write buffers before exit
- Retry with exponential backoff for transient HTTP errors (429, 502, 503)
- Request concurrency limiter (semaphore) per REST connector
- `resource_mtimes` tracking for real modification timestamps in `getattr`
- `content_lengths` tracking for accurate file sizes after first read
- Zero-copy read path using `bytes::Bytes` throughout VFS core

### Changed
- `read_resource_data()` returns `bytes::Bytes` instead of `Vec<u8>`
- Live resource sizes report 0 (unknown) instead of misleading 4096 placeholder
- Credential structs use custom `Debug` impl that redacts secrets
- Error response bodies truncated to 512 bytes
- Cloudflare Account ID moved from hardcoded value to GitHub secret
- reqwest Client configured with connection pooling and timeouts

### Fixed
- 31 clippy warnings
- 11 cargo doc warnings
- `strip_prefix` usage in Confluence connector
- Doc link and HTML tag warnings in VFS types

## [0.1.0] - 2026-03-27

### Added
- FUSE filesystem (Linux) and NFS loopback (macOS) transports
- VirtualFs platform-agnostic core with lookup, read, write, readdir
- REST connector driven by YAML specs
- Native connectors for Google Workspace, Jira, Confluence
- 14 YAML connector specs (Salesforce, GitHub, Slack, Stripe, etc.)
- Draft store with copy-on-write sandboxing
- Version store with immutable snapshots
- Governance layer with NDJSON audit logging and approval gates
- TTL-based response cache with `bytes::Bytes`
- CLI: mount, unmount, status, log, approve, versions, rollback
- Connector registry: install, list, remove, update from Git
- Transaction directories (`.tx/`) for grouped operations
- `agent.md` auto-generated help files per connector and collection
- Plugins for Claude Code, Codex, OpenClaw, E2B
- Cloudflare Pages landing page at tapfs.dev

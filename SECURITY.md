# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in tapfs, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, please email: **security@tapfs.dev**

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

## Response Timeline

- **Acknowledgment**: Within 48 hours
- **Initial assessment**: Within 1 week
- **Fix and disclosure**: Coordinated with reporter

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |

## Scope

The following are in scope:
- tapfs core (Rust crate)
- Connector specifications (YAML files in `connectors/`)
- Official plugins (`plugins/`)
- CI/CD workflows

The following are out of scope:
- Third-party connectors not maintained in this repository
- Issues in upstream dependencies (report these to the respective projects)

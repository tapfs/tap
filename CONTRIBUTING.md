# Contributing to tapfs

Thank you for your interest in contributing to tapfs! This guide will help you get started.

## Development Setup

### Prerequisites

- Rust 1.81+ (pinned via `rust-toolchain.toml`)
- Linux: `libfuse-dev` / `fuse3` package
- macOS: No additional dependencies (uses NFS transport)

### Build

```bash
git clone https://github.com/tapfs/tap.git
cd tap
cargo build
```

### Run Tests

```bash
cargo test
```

### Lint

```bash
cargo clippy --all-targets
cargo fmt -- --check
```

## Making Changes

1. Fork the repository and create a feature branch from `main`
2. Make your changes
3. Ensure `cargo clippy` passes with no warnings
4. Ensure `cargo fmt -- --check` passes
5. Run `cargo test` and confirm all tests pass
6. Submit a pull request

## What to Work On

Check the [Roadmap](docs/ROADMAP.md) for planned work organized by priority. Issues labeled `good first issue` are a great starting point.

## Code Style

- Follow standard Rust conventions (`rustfmt` enforced)
- Use `anyhow::Result` for fallible functions
- Prefer `tracing` over `println!` for diagnostic output (CLI user-facing output is fine with `println!`)
- Keep `unsafe` minimal and always document with `# Safety` sections

## Connector Development

To add a new connector:

1. **YAML connector** (most cases): Create a YAML file in `connectors/` following the format in existing specs
2. **Native connector** (complex auth/transforms): Implement the `Connector` trait in `src/connector/`

See [`connectors/rest.yaml`](connectors/rest.yaml) for the simplest YAML example.

## Commit Messages

Write clear, concise commit messages. Use imperative mood ("Add feature" not "Added feature").

## Reporting Bugs

Open an issue with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- OS, Rust version, and tapfs version

## Security

If you discover a security vulnerability, please follow the process in [SECURITY.md](SECURITY.md). Do not open a public issue.

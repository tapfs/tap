# Running tapfs with Anthropic Managed Agents

Anthropic's [Managed Agents](https://platform.claude.com/docs/en/managed-agents/overview) run Claude in a cloud sandbox with bash, file access, and unrestricted networking. This makes them a good fit for testing tapfs installation and running FUSE mounts without touching your local machine.

## Prerequisites

```bash
pip install 'anthropic>=0.97'
export ANTHROPIC_API_KEY=sk-ant-...
```

## Architecture

Managed Agents have three primitives:

| Concept | What it is | Lifecycle |
|---------|-----------|-----------|
| **Environment** | Config template (OS packages, networking) | Persistent, reusable across sessions |
| **Agent** | Model + system prompt + tools | Persistent, versioned |
| **Session** | Running container bound to agent + environment | Ephemeral, fresh each time |

Key behaviors we discovered:

- **Environments are stateless templates.** Each session starts a fresh container. Nothing installed during a session persists to the next one.
- **apt packages declared in the environment config are pre-installed** in every session (e.g., `fuse3`).
- **cargo packages declared in the environment config may fail silently** if build dependencies are missing (e.g., `libfuse-dev` headers for compiling `fuser`).
- **Killing the local process does NOT stop the agent.** You must send a `user.interrupt` event, wait for idle, then delete the session.
- **The Rust toolchain (cargo, rustc) is pre-installed** in the base image.
- **Vaults are MCP-only.** They inject tokens into MCP server connections, not as shell env vars. For API tokens like `GITHUB_TOKEN`, pass them in the user message.

## Quick start

### 1. Create an environment

Environments define what packages are pre-installed. Create one with FUSE support:

```python
import anthropic

client = anthropic.Anthropic()

env = client.beta.environments.create(
    name="tapfs-test",
    config={
        "type": "cloud",
        "networking": {"type": "unrestricted"},
        "packages": {
            "type": "packages",
            "apt": ["fuse3", "libfuse-dev"],
        },
    },
)
print(env.id)  # env_01ABC...
```

Reuse this environment across sessions -- no need to create a new one each time.

### 2. Create an agent and session

```python
agent = client.beta.agents.create(
    name="tapfs-installer",
    model="claude-sonnet-4-6",
    system="You are a systems engineer. Follow the user's instructions exactly.",
    tools=[{"type": "agent_toolset_20260401"}],
)

session = client.beta.sessions.create(
    agent={"type": "agent", "id": agent.id, "version": agent.version},
    environment_id=env.id,
)
```

### 3. Stream events

The stream must be opened **before** sending the user message:

```python
with client.beta.sessions.events.stream(session.id) as stream:
    client.beta.sessions.events.send(
        session.id,
        events=[{
            "type": "user.message",
            "content": [{"type": "text", "text": "Install tapfs and test it."}],
        }],
    )

    for event in stream:
        match event.type:
            case "agent.message":
                for block in event.content:
                    if hasattr(block, "text"):
                        print(block.text, end="", flush=True)
            case "agent.tool_use":
                print(f"\n  [tool: {event.name}]", flush=True)
            case "session.status_idle":
                print("\nAgent finished.")
                break
            case "session.error":
                print(f"\nError: {getattr(event, 'error', '')}")
                break
```

## Event types

| Event | Meaning |
|-------|---------|
| `agent.message` | Text output (has `.content` list of blocks with `.text`) |
| `agent.tool_use` | Agent calling a tool (has `.name`, `.input`) |
| `agent.tool_result` | Tool result (has `.is_error`) |
| `session.status_idle` | Agent finished -- break the stream loop |
| `session.status_running` | Agent is working |
| `session.error` | Error occurred (has `.error`) |
| `session.status_terminated` | Session was terminated |

## Stopping a session

Killing your local process only closes the SSE stream. The agent keeps running (and billing) on Anthropic's servers. To actually stop it:

```python
# 1. Send interrupt
client.beta.sessions.events.send(
    session_id,
    events=[{"type": "user.interrupt"}],
)

# 2. Wait for it to stop
import time
for _ in range(30):
    session = client.beta.sessions.retrieve(session_id)
    if session.status != "running":
        break
    time.sleep(1)

# 3. Delete
client.beta.sessions.delete(session_id)
```

You cannot delete a running session directly -- the API returns a 400.

## Cleanup

```python
# Sessions: interrupt + delete
client.beta.sessions.delete(session_id)

# Agents: archive (no hard delete)
client.beta.agents.archive(agent_id)

# Environments: delete
client.beta.environments.delete(env_id)
```

## Passing secrets

Managed Agent [vaults](https://platform.claude.com/docs/en/managed-agents/vaults) only support MCP server credentials (OAuth or static bearer tokens bound to an `mcp_server_url`). They do not expose secrets as environment variables.

For tokens like `GITHUB_TOKEN`, pass them in the user message:

```python
import os

github_token = os.environ.get("GITHUB_TOKEN", "")
message = f"export GITHUB_TOKEN={github_token}\n\nNow run: tap mount github"

client.beta.sessions.events.send(
    session.id,
    events=[{
        "type": "user.message",
        "content": [{"type": "text", "text": message}],
    }],
)
```

## tapfs installation in a session

The fastest method is the install script (downloads a prebuilt binary, ~5 seconds):

```
curl -fsSL https://github.com/tapfs/tap/raw/main/site/install.sh | sh
export PATH="$HOME/.tapfs/bin:$PATH"
tap mount github
```

`cargo install tapfs` also works (tapfs is [published on crates.io](https://crates.io/crates/tapfs)) but builds from source (~3-5 minutes). The environment's `cargo: ["tapfs"]` package config may fail if `libfuse-dev` is not also included in `apt` packages.

## Scripts

| Script | Purpose |
|--------|---------|
| `scripts/test-install-agent.py` | Full install test with streaming output, `--quick` check mode, `--stop` / `--cleanup` |
| `scripts/env-create.py` | Create an environment with FUSE + tapfs packages |
| `scripts/env-check.py` | Probe an environment to see what's pre-installed |

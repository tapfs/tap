#!/usr/bin/env python3
"""Launch an Anthropic Managed Agent to test if FUSE and tapfs can be installed.

Usage:
    python scripts/test-install-agent.py https://github.com/tapfs/tap
    python scripts/test-install-agent.py https://github.com/tapfs/tap --quick
    python scripts/test-install-agent.py --stop sesn_...
    python scripts/test-install-agent.py --cleanup sesn_... agent_... env_...

Requires:
    pip install 'anthropic>=0.97'
    export ANTHROPIC_API_KEY=sk-...
"""

import argparse
import os
import sys
import time

import anthropic

SYSTEM_PROMPT_FULL = """\
You are a systems engineer testing whether FUSE (Filesystem in Userspace) and \
tapfs can be installed on this machine.

Run each step below, reporting the result clearly. Do NOT skip steps even if \
earlier ones fail — we want a complete picture.

## Step 1 — Environment info
- Print OS, kernel version, architecture (`uname -a`)
- Print distro info (`cat /etc/os-release` or equivalent)
- Check if running in a container (`cat /proc/1/cgroup` or similar)
- Check if running as root or if sudo is available

## Step 2 — FUSE availability
FUSE packages (fuse3, libfuse-dev) are pre-installed via the environment config. \
Just verify they're working:
- Check if /dev/fuse exists
- Check if fusermount / fusermount3 is available and its version
- Check if the fuse kernel module is loaded (`lsmod | grep fuse` or \
  `grep fuse /proc/filesystems`)

## Step 3 — Install tapfs from the provided URL
- Try the install script: `curl -fsSL <URL>/raw/main/site/install.sh | sh`
- If that fails, try building from source:
  - Install Rust if needed: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`
  - Source cargo env: `. "$HOME/.cargo/env"`
  - Clone the repo and build: `cargo build --release`
- Verify the `tap` binary runs: `tap --help` or `~/.tapfs/bin/tap --help`

## Step 4 — Test a mount (if possible)
- Try running: `tap mount github`
  (it will mount to /tmp/tap by default; it may fail without a token, but we want
  to see if FUSE mounting itself works)
- If it fails, report the exact error

## Step 5 — Summary
Write a clear PASS/FAIL summary table:
| Check                  | Result  | Notes |
|------------------------|---------|-------|
| OS / Kernel            | ...     | ...   |
| FUSE kernel module     | ...     | ...   |
| /dev/fuse              | ...     | ...   |
| fusermount             | ...     | ...   |
| tapfs binary installed | ...     | ...   |
| tapfs mount works      | ...     | ...   |

End with an overall verdict: CAN INSTALL / CANNOT INSTALL / PARTIAL (with explanation).
"""

SYSTEM_PROMPT_QUICK = """\
You are a systems engineer. Check whether FUSE and tapfs are already installed.

IMPORTANT: Do NOT install, download, build, or modify ANYTHING. \
Only run read-only commands. No apt-get, no curl, no cargo, no pip. \
If something is missing, just report it as missing.

Run these checks in parallel:
- `uname -a`
- `which fusermount3 || which fusermount`
- `ls -l /dev/fuse`
- `which tap || ls ~/.tapfs/bin/tap`
- `dpkg -l | grep fuse` or `rpm -qa | grep fuse`

End with:
| Check          | Result |
|----------------|--------|
| FUSE installed | YES/NO |
| /dev/fuse      | YES/NO |
| tapfs binary   | YES/NO |

Overall: READY / NOT READY
"""


def parse_args():
    parser = argparse.ArgumentParser(
        description="Launch a Managed Agent to test FUSE/tapfs installation"
    )
    parser.add_argument(
        "--stop",
        metavar="SESSION_ID",
        help="Stop a running session, e.g. --stop sesn_...",
    )
    parser.add_argument(
        "--cleanup",
        nargs="+",
        metavar="ID",
        help="Archive/delete agents, environments, sessions by ID",
    )
    parser.add_argument(
        "url",
        nargs="?",
        help="GitHub repo URL, e.g. https://github.com/tapfs/tap",
    )
    parser.add_argument(
        "--model",
        default="claude-sonnet-4-6",
        help="Claude model to use (default: claude-sonnet-4-6)",
    )
    parser.add_argument(
        "--name",
        default=None,
        help="Agent name (default: tapfs-install-test)",
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="Quick smoke test instead of full 5-step report",
    )
    parser.add_argument(
        "--env",
        metavar="ENV_ID",
        help="Reuse an existing environment by ID",
    )
    args = parser.parse_args()
    if not args.stop and not args.cleanup and not args.url:
        parser.error("url is required (unless using --stop or --cleanup)")
    return args


ENV_NAME = "tapfs-test"


def get_or_create_environment(client, env_id=None):
    """Reuse an existing environment by ID, find one by name, or create a new one."""
    if env_id:
        print(f"Reusing environment {env_id}...", flush=True)
        return client.beta.environments.retrieve(env_id)

    # Look for an existing tapfs-test environment
    for env in client.beta.environments.list():
        if env.name == ENV_NAME and env.archived_at is None:
            print(f"Reusing environment: {env.id} ({env.name})", flush=True)
            return env

    # Create a new one with fuse3 pre-installed
    print("Creating environment...", flush=True)
    env = client.beta.environments.create(
        name=ENV_NAME,
        config={
            "type": "cloud",
            "networking": {"type": "unrestricted"},
            "packages": {
                "type": "packages",
                "apt": ["fuse3", "libfuse-dev"],
            },
        },
    )
    print(f"  Environment: {env.id}")
    return env


def stop_session(session_id):
    client = anthropic.Anthropic()
    print(f"Interrupting session {session_id}...", flush=True)
    client.beta.sessions.events.send(
        session_id,
        events=[{"type": "user.interrupt"}],
    )
    # Wait for the session to become idle
    for _ in range(30):
        session = client.beta.sessions.retrieve(session_id)
        if session.status != "running":
            break
        time.sleep(1)
    print(f"Deleting session...", flush=True)
    client.beta.sessions.delete(session_id)
    print("Session deleted. Agent stopped.")


def cleanup(ids):
    client = anthropic.Anthropic()
    for resource_id in ids:
        try:
            if resource_id.startswith("sesn_"):
                stop_session(resource_id)
            elif resource_id.startswith("agent_"):
                print(f"Archiving agent {resource_id}...", flush=True)
                client.beta.agents.archive(resource_id)
                print("Agent archived.")
            elif resource_id.startswith("env_"):
                print(f"Deleting environment {resource_id}...", flush=True)
                client.beta.environments.delete(resource_id)
                print("Environment deleted.")
            else:
                print(f"Unknown ID prefix: {resource_id}", file=sys.stderr)
        except anthropic.APIError as e:
            print(f"Failed on {resource_id}: {e}", file=sys.stderr)


def main():
    args = parse_args()

    if args.stop:
        stop_session(args.stop)
        return

    if args.cleanup:
        cleanup(args.cleanup)
        return

    url = args.url.rstrip("/")
    agent_name = args.name or "tapfs-install-test"

    client = anthropic.Anthropic()

    # 1. Reuse or create environment
    environment = get_or_create_environment(client, args.env)

    # 2. Create agent with built-in toolset (bash, files, etc.)
    print(f"Creating agent ({args.model})...", flush=True)
    agent = client.beta.agents.create(
        name=agent_name,
        model=args.model,
        system=SYSTEM_PROMPT_QUICK if args.quick else SYSTEM_PROMPT_FULL,
        tools=[{"type": "agent_toolset_20260401"}],
    )
    print(f"  Agent: {agent.id}")

    # 3. Create session binding agent to environment
    print("Creating session...", flush=True)
    session = client.beta.sessions.create(
        agent={"type": "agent", "id": agent.id, "version": agent.version},
        environment_id=environment.id,
    )
    print(f"  Session: {session.id}")

    # 4. Open stream, then send the user message
    github_token = os.environ.get("GITHUB_TOKEN", "")
    token_line = ""
    if github_token:
        token_line = (
            f"\nBefore running any tap commands, export the GitHub token:\n"
            f"  export GITHUB_TOKEN={github_token}\n"
        )

    user_message = (
        f"Test whether FUSE and tapfs can be installed on this machine.\n\n"
        f"The tapfs repository URL is: {url}\n"
        f"The install script URL is: {url}/raw/main/site/install.sh\n"
        f"{token_line}\n"
        f"Follow all the steps in your instructions. Go."
    )

    print(f"\n{'='*60}")
    print("Agent running — streaming output...")
    print(f"{'='*60}\n", flush=True)

    try:
        with client.beta.sessions.events.stream(session.id) as stream:
            # Send user message after stream is open
            client.beta.sessions.events.send(
                session.id,
                events=[
                    {
                        "type": "user.message",
                        "content": [{"type": "text", "text": user_message}],
                    }
                ],
            )

            for event in stream:
                match event.type:
                    case "agent.message":
                        for block in event.content:
                            if hasattr(block, "text"):
                                print(block.text, end="", flush=True)
                    case "agent.tool_use":
                        print(f"\n  [tool: {event.name}]", flush=True)
                    case "agent.tool_result":
                        if getattr(event, "is_error", False):
                            print("  [tool error]", flush=True)
                    case "session.status_idle":
                        print(f"\n\n{'='*60}")
                        print("Agent finished.")
                        print(f"{'='*60}")
                        break
                    case "session.error":
                        error = getattr(event, "error", None)
                        print(f"\n[session error: {error}]", file=sys.stderr)
                        break
                    case "session.status_terminated":
                        print(f"\n\n{'='*60}")
                        print("Session terminated.")
                        print(f"{'='*60}")
                        break

    except KeyboardInterrupt:
        print("\n\nInterrupted — stopping session...", flush=True)
        try:
            stop_session(session.id)
        except anthropic.APIError as e:
            print(f"Failed to stop session: {e}", file=sys.stderr)
        sys.exit(1)
    except anthropic.APIError as e:
        print(f"\nAPI error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

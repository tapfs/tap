#!/usr/bin/env python3
"""Spawn a session on an environment and check if tapfs is pre-installed."""
import sys
import anthropic

if len(sys.argv) < 2:
    print("Usage: python scripts/env-check.py <env_id>")
    sys.exit(1)

env_id = sys.argv[1]
client = anthropic.Anthropic()

agent = client.beta.agents.create(
    name="env-check",
    model="claude-sonnet-4-6",
    system="Run the commands the user gives you. Report output exactly. Nothing else.",
    tools=[{"type": "agent_toolset_20260401"}],
)

session = client.beta.sessions.create(
    agent={"type": "agent", "id": agent.id, "version": agent.version},
    environment_id=env_id,
)

print(f"Session: {session.id}")
print(f"Checking environment {env_id}...\n")

with client.beta.sessions.events.stream(session.id) as stream:
    client.beta.sessions.events.send(
        session.id,
        events=[
            {
                "type": "user.message",
                "content": [{"type": "text", "text": (
                    "Run these commands and show me the raw output:\n"
                    "1. which tap || find / -name tap -type f 2>/dev/null\n"
                    "2. which cargo && cargo --version\n"
                    "3. which rustc && rustc --version\n"
                    "4. ls -la ~/.cargo/bin/ 2>/dev/null || echo 'no .cargo/bin'\n"
                    "5. which fusermount3\n"
                    "6. ls /dev/fuse\n"
                    "7. dpkg -l | grep -i fuse\n"
                    "That's it. Don't install anything."
                )}],
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
                print(f"\n  [{event.name}]", flush=True)
            case "session.status_idle":
                print("\n\nDone.")
                break
            case "session.error":
                print(f"\nError: {getattr(event, 'error', '')}", file=sys.stderr)
                break

# Cleanup
client.beta.sessions.delete(session.id)
client.beta.agents.archive(agent.id)
print("Cleaned up session + agent.")

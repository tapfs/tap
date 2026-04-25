#!/usr/bin/env python3
"""Create a managed agent environment with tapfs pre-installed via cargo."""
import anthropic

client = anthropic.Anthropic()

env = client.beta.environments.create(
    name="tapfs-ready",
    config={
        "type": "cloud",
        "networking": {"type": "unrestricted"},
        "packages": {
            "type": "packages",
            "apt": ["fuse3"],
            "cargo": ["tapfs"],
        },
    },
)

print(f"Environment created: {env.id}")
print(f"Name: {env.name}")

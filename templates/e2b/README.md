# tapfs E2B Template

Pre-built E2B sandbox template with tapfs installed. Mount REST APIs as files inside your AI agent's sandbox.

## Build the template

```bash
npx e2b template build
```

## Usage

```python
from e2b import Sandbox

sandbox = Sandbox(template="tapfs")

# Start tapfs with your connector
sandbox.process.exec(
    "GITHUB_TOKEN=$TOKEN tap mount github --mount-point /tmp/tap &"
)

# Wait for mount
import time
time.sleep(2)

# Read API resources as files
files = sandbox.filesystem.list("/tmp/tap/github/repos/")
content = sandbox.filesystem.read("/tmp/tap/github/repos/my-repo.md")
```

## Supported connectors

github, jira, salesforce, slack, notion, hubspot, stripe, zendesk, pagerduty, linear, servicenow, confluence, or any custom REST API via YAML spec.

## Environment variables

| Variable | Description |
|----------|-------------|
| `TAPFS_MOUNT_POINT` | Mount path (default: `/tmp/tap`) |
| Connector-specific token env vars (e.g. `GITHUB_TOKEN`, `JIRA_API_TOKEN`) | API authentication |

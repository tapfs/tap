# Mount and Recovery

The daemon manages NFS mounts automatically. It detects stale mounts left by crashed processes, force-unmounts them, and re-establishes a clean state before serving.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Mount a connector for the first time | `tap mount github` | `/tmp/tap/github/` populated; `mounted github` printed |
| 2 | Add a second connector to running daemon | `tap mount linear` (daemon already running) | Linear added without remounting GitHub; both visible under `/tmp/tap/` |
| 3 | Recover from crashed daemon | Previous process died; run `tap mount github` | Stale port and mount cleared automatically; fresh mount established |
| 4 | Recover from stale NFS handle | Mount path exists but returns `NFS3ERR_STALE` | `tap mount github` detects dead mount, force-unmounts, remounts cleanly |
| 5 | Check mounted connectors | `ls /tmp/tap/` | Shows only actively mounted connectors |
| 6 | Stop daemon | `tap umount` or process exit | All mounts cleanly unmounted; no stale handles left |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 7 | Verify mount is live before operations | Check that `/tmp/tap/github/` lists entries | Non-empty listing confirms mount is healthy |
| 8 | Detect stale mount state | `ls /tmp/tap/github/` returns empty or errors | Agent re-invokes `tap mount github` to recover |
| 9 | Mount does not affect other connectors | Adding `tap mount linear` while using GitHub | GitHub operations uninterrupted; no remount needed |
| 10 | Filesystem available immediately after mount | `tap mount github` then immediately `ls .../issues/` | Resources listed without delay; no warm-up period |

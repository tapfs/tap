# Search providers

`tap search` fans a query out across configured providers, fuses ranked results,
and keeps provider failures non-fatal so agents can still use partial results.
Provider config lives in `~/.tapfs/search.yaml`.

## Human

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 1 | Global search with no indexed providers | `tap search "incident"` on a fresh install | Prints `(no results)` and a warning that no providers are eligible for global scope; exits 0 |
| 2 | Connector-scoped upstream search | `tap search -t github "release blocker"` | Loads the github connector and queries the audited `upstream` provider; unsupported connector search is surfaced as a warning, not a crash |
| 3 | JSON output for tooling | `tap search --json -t github/issues "auth"` | Emits a serialized `ProviderResult` with `hits` and `warnings` fields |
| 4 | Per-connector provider exclusion | `search.yaml` excludes `upstream` for confluence, then `tap search -t confluence "roadmap"` | The excluded provider is not queried for that connector |

## Agent

| # | Use Case | Workflow Example | Acceptance Outcome |
|---|----------|-----------------|-------------------|
| 5 | Mixed-provider partial failure | Configure a not-yet-wired `process` provider plus a working provider | The unsupported provider contributes a warning and the working provider's hits still return |
| 6 | Scope-limited provider | Configure `upstream` with `scopes: [collection]`; run `tap search -t github "x"` | Connector-scope query does not call upstream and reports no eligible providers |
| 7 | Provider allow-list | Configure `include_only: [qmd]` and `exclude: [qmd]` for a connector | The allow-list wins over exclusion so only `qmd` is eligible |
| 8 | Audit search activity | Run any provider-backed search, then `tap log -n 5` | Audit log contains a `search` entry with provider, connector scope, outcome, and hit count |

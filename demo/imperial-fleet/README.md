# Imperial Fleet Command Registry Demo

This demo creates a fictional enterprise REST API and mounts the safe role view
through TapFS as a role-scoped filesystem.

## Run the API

```bash
node demo/imperial-fleet/server.mjs
```

The server listens on `http://127.0.0.1:7077`.

Swagger UI is available at:

```text
http://127.0.0.1:7077/docs
```

Useful direct API checks:

```bash
curl http://127.0.0.1:7077/health
curl http://127.0.0.1:7077/api/v1
curl http://127.0.0.1:7077/api/v1/missions
curl http://127.0.0.1:7077/api/v1/fleet/isd-devastator
curl http://127.0.0.1:7077/api/v1/intelligence
curl http://127.0.0.1:7077/api/v1/superweapon
```

The default role is `sector-ops-analyst`, so `intelligence` is hidden and
`superweapon` is denied. For direct API exploration only:

```bash
curl -H "Authorization: Bearer moff-clearance" http://127.0.0.1:7077/api/v1
```

The sector operations role can perform one write operation: update the
`commander` field on a fleet asset.

```bash
curl -X PATCH http://127.0.0.1:7077/api/v1/fleet/isd-devastator \
  -H "Content-Type: application/json" \
  -d '{"commander":"Luke Skywalker"}'
```

## Mount with TapFS

```bash
cargo run --no-default-features --features nfs -- mount rest \
  --spec demo/imperial-fleet/connector.yaml \
  --mount-point /tmp/tap-imperial \
  --data-dir /tmp/tapfs-imperial
```

Then inspect the mounted filesystem:

```bash
ls /tmp/tap-imperial/imperial-fleet
ls /tmp/tap-imperial/imperial-fleet/missions
cat /tmp/tap-imperial/imperial-fleet/missions/resupply-endor-garrison.md
cat /tmp/tap-imperial/imperial-fleet/fleet/lambda-shuttle-779.md
cat /tmp/tap-imperial/imperial-fleet/procurement/hyperdrive-motivator-assemblies.md
tap log --data-dir /tmp/tapfs-imperial -n 5
```

To demo an authorized write, edit:

```text
/tmp/tap-imperial/imperial-fleet/fleet/isd-devastator.md
```

Change:

```yaml
commander: "Darth Vader"
```

to:

```yaml
commander: "Luke Skywalker"
```

On close, TapFS PATCHes `/api/v1/fleet/{id}`. The API accepts the commander
assignment and rejects changes to other fleet fields for this role.

The mounted connector exposes only:

- `missions`
- `fleet`
- `maintenance`
- `procurement`
- `personnel`

The server also has restricted `intelligence` and `superweapon` APIs, but the
TapFS connector does not mount them for the sector operations role.

## Demo narration

```text
This is a fictional Imperial Fleet Command Registry: an enterprise API with
missions, fleet assets, maintenance tickets, procurement requests, personnel
records, and restricted systems.

I could expose this through MCP. I could build a custom CLI. Instead I define
one TapFS connector and mount the API as a filesystem.

The agent sees files and directories. The enterprise sees a role-scoped control
surface: broad read access for operational data, one narrow write for commander
assignment, and audit logs for every file operation.

Restricted APIs exist, but they are not in the mounted filesystem for this role.
```

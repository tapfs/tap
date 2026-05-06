#!/usr/bin/env node
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const host = process.env.HOST ?? "127.0.0.1";
const port = Number(process.env.PORT ?? "7077");
const here = path.dirname(fileURLToPath(import.meta.url));

const data = {
  missions: [
    {
      id: "MIS-001",
      slug: "resupply-endor-garrison",
      title: "Resupply Endor garrison",
      status: "blocked",
      priority: "high",
      theater: "Outer Rim",
      clearance: "sector-ops",
      owner: "Commander Iden Versio",
      updated_at: "2026-05-04T12:00:00Z",
      brief:
        "Move replacement shield projector coils and field rations to the Endor moon garrison before the next inspection window.",
      linked_assets: [
        { type: "fleet", id: "LMB-779" },
        { type: "procurement", id: "REQ-1138" },
      ],
      blockers: [
        {
          severity: "high",
          summary: "Hyperdrive motivator assemblies are pending quartermaster approval",
        },
      ],
    },
    {
      id: "MIS-002",
      slug: "escort-kessel-convoy",
      title: "Escort Kessel convoy",
      status: "ready",
      priority: "medium",
      theater: "Kessel Run",
      clearance: "sector-ops",
      owner: "Captain Lorth Needa",
      updated_at: "2026-05-03T16:30:00Z",
      brief:
        "Provide escort coverage for a tibanna gas convoy crossing a high-interdiction route near Kessel.",
      linked_assets: [{ type: "fleet", id: "TIE-441" }],
      blockers: [],
    },
    {
      id: "MIS-003",
      slug: "inspect-hoth-probe-relay",
      title: "Inspect Hoth probe relay",
      status: "open",
      priority: "low",
      theater: "Anoat Sector",
      clearance: "sector-ops",
      owner: "Lieutenant Cabbel",
      updated_at: "2026-05-02T08:45:00Z",
      brief:
        "Validate telemetry from remote probe relay stations after repeated signal drift reports.",
      linked_assets: [
        { type: "fleet", id: "PRB-021" },
        { type: "maintenance", id: "MX-7781" },
      ],
      blockers: [],
    },
  ],
  fleet: [
    {
      id: "ISD-001",
      slug: "isd-devastator",
      title: "ISD Devastator",
      class: "Imperial I-class Star Destroyer",
      status: "deployed",
      readiness: 91,
      location: "Tatooine system",
      commander: "Darth Vader",
      updated_at: "2026-05-04T09:00:00Z",
      summary:
        "Flagship assigned to high-priority interdiction and command presence operations.",
      capabilities: ["orbital blockade", "tractor beam", "fighter launch"],
    },
    {
      id: "LMB-779",
      slug: "lambda-779",
      title: "Lambda shuttle 779",
      class: "Lambda-class T-4a shuttle",
      status: "maintenance_hold",
      readiness: 62,
      location: "Endor staging yard",
      commander: "Flight Officer Tarsus",
      updated_at: "2026-05-04T11:10:00Z",
      summary:
        "Utility shuttle assigned to personnel movement and priority cargo hops.",
      capabilities: ["cargo transfer", "personnel transport"],
    },
    {
      id: "TIE-441",
      slug: "tie-squadron-441",
      title: "TIE squadron 441",
      class: "TIE/ln fighter squadron",
      status: "ready",
      readiness: 87,
      location: "Kessel escort group",
      commander: "Major Rhymer",
      updated_at: "2026-05-03T18:00:00Z",
      summary: "Line fighter squadron staged for convoy escort coverage.",
      capabilities: ["escort", "interception"],
    },
    {
      id: "PRB-021",
      slug: "probe-relay-021",
      title: "Probe relay 021",
      class: "Viper probe relay",
      status: "degraded",
      readiness: 48,
      location: "Hoth perimeter",
      commander: "Automated relay",
      updated_at: "2026-05-02T07:20:00Z",
      summary: "Remote telemetry relay reporting intermittent packet loss.",
      capabilities: ["long-range scan", "telemetry relay"],
    },
  ],
  maintenance: [
    {
      id: "MX-7781",
      slug: "probe-relay-signal-drift",
      title: "Probe relay signal drift",
      asset_id: "PRB-021",
      status: "open",
      severity: "medium",
      assigned_to: "Chief Technician Venka",
      updated_at: "2026-05-02T10:15:00Z",
      description:
        "Relay diagnostics show repeated carrier drift during cold-cycle startup.",
      steps: [
        "Replace external sensor coupling",
        "Run cold-cycle startup test",
        "Verify telemetry checksum stability",
      ],
    },
    {
      id: "MX-7782",
      slug: "lambda-779-motivator-swap",
      title: "Lambda 779 motivator swap",
      asset_id: "LMB-779",
      status: "waiting_parts",
      severity: "high",
      assigned_to: "Dockmaster Pell",
      updated_at: "2026-05-04T10:05:00Z",
      description:
        "Primary hyperdrive motivator failed pre-flight checks and requires replacement assemblies.",
      steps: [
        "Receive motivator assemblies from procurement",
        "Swap primary motivator",
        "Run jump readiness simulation",
      ],
    },
  ],
  procurement: [
    {
      id: "REQ-1138",
      slug: "hyperdrive-motivator-assemblies",
      title: "Hyperdrive motivator assemblies",
      status: "pending_approval",
      requested_by: "Dockmaster Pell",
      needed_by: "2026-05-06",
      updated_at: "2026-05-04T10:30:00Z",
      description:
        "Two replacement motivator assemblies required to release Lambda shuttle 779 from maintenance hold.",
      items: [
        { sku: "HYP-MOT-22", quantity: 2, priority: "expedite" },
        { sku: "COUPLING-7B", quantity: 4, priority: "normal" },
      ],
    },
    {
      id: "REQ-1140",
      slug: "tie-wing-panel-spares",
      title: "TIE wing panel spares",
      status: "approved",
      requested_by: "Major Rhymer",
      needed_by: "2026-05-09",
      updated_at: "2026-05-03T13:00:00Z",
      description:
        "Standard replacement wing panels for TIE squadron 441 after convoy escort readiness inspection.",
      items: [{ sku: "TIE-PANEL-LN", quantity: 6, priority: "normal" }],
    },
  ],
  personnel: [
    {
      id: "PER-0421",
      slug: "tk-421",
      title: "TK-421",
      rank: "Stormtrooper",
      station: "Death Star docking bay",
      status: "unresponsive",
      clearance: "public-duty-roster",
      updated_at: "2026-05-04T07:00:00Z",
      briefing_notes:
        "Duty roster entry only. Medical and disciplinary files are not exposed to the sector operations role.",
    },
    {
      id: "PER-1001",
      slug: "iden-versio",
      title: "Iden Versio",
      rank: "Commander",
      station: "Endor garrison",
      status: "active",
      clearance: "public-duty-roster",
      updated_at: "2026-05-04T12:05:00Z",
      briefing_notes:
        "Mission owner for Endor garrison resupply. Contact through command dispatch.",
    },
  ],
  intelligence: [
    {
      id: "INT-0007",
      slug: "rebel-cell-patterns",
      title: "Rebel cell movement patterns",
      classification: "restricted",
      summary: "Hidden from sector operations analysts.",
    },
  ],
  superweapon: [
    {
      id: "DS-001",
      slug: "thermal-exhaust-port-review",
      title: "Thermal exhaust port design review",
      classification: "need-to-know",
      summary: "Explicitly denied unless the role has Moff-level clearance.",
    },
  ],
};

const publicCollections = new Set([
  "missions",
  "fleet",
  "maintenance",
  "procurement",
  "personnel",
]);
const hiddenCollections = new Set(["intelligence"]);
const deniedCollections = new Set(["superweapon"]);

function roleFromRequest(req) {
  const auth = req.headers.authorization ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice("Bearer ".length) : "";
  if (token === "moff-clearance") {
    return "moff";
  }
  return "sector-ops-analyst";
}

function sendJson(res, status, body) {
  const payload = JSON.stringify(body, null, 2);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
  });
  res.end(`${payload}\n`);
}

function sendText(res, status, body, contentType = "text/plain; charset=utf-8") {
  res.writeHead(status, {
    "content-type": contentType,
    "cache-control": "no-store",
  });
  res.end(body);
}

function swaggerUiHtml() {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Imperial Fleet Command Registry API</title>
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
    <style>
      body { margin: 0; background: #f7f7f7; }
      .topbar { display: none; }
    </style>
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.onload = () => {
        window.ui = SwaggerUIBundle({
          url: "/openapi.yaml",
          dom_id: "#swagger-ui",
          deepLinking: true,
          presets: [SwaggerUIBundle.presets.apis],
          layout: "BaseLayout"
        });
      };
    </script>
  </body>
</html>`;
}

function catalogFor(role) {
  const visible = [...publicCollections];
  if (role === "moff") {
    visible.push("intelligence", "superweapon");
  }
  return visible.map((name) => ({
    name,
    href: `/api/v1/${name}`,
    access:
      name === "fleet" && publicCollections.has(name)
        ? "readwrite:commander"
        : publicCollections.has(name)
          ? "readonly"
          : "restricted",
  }));
}

function canRead(role, collection) {
  return role === "moff" || publicCollections.has(collection);
}

function canPatchFleetCommander(role) {
  return role === "sector-ops-analyst" || role === "moff";
}

function notFound(res) {
  sendJson(res, 404, { error: "not_found" });
}

function writeDenied(res, role, collection) {
  sendJson(res, 403, {
    error: "write_not_allowed",
    role,
    collection,
    message: "This role cannot perform that write in the Imperial Fleet Command Registry demo.",
  });
}

function restrictedDenied(res, role, collection) {
  sendJson(res, 403, {
    error: "restricted_collection",
    role,
    collection,
    message: "This collection is not available to the mounted TapFS demo role.",
  });
}

function findResource(collection, idOrSlug) {
  return (data[collection] ?? []).find(
    (item) => item.id === idOrSlug || item.slug === idOrSlug,
  );
}

function badRequest(res, message, details = undefined) {
  sendJson(res, 400, {
    error: "bad_request",
    message,
    ...(details ? { details } : {}),
  });
}

async function readJsonBody(req) {
  let raw = "";
  for await (const chunk of req) {
    raw += chunk;
    if (raw.length > 64 * 1024) {
      throw new Error("request body is too large");
    }
  }
  if (!raw.trim()) {
    return {};
  }
  return JSON.parse(raw);
}

async function patchFleetAsset(req, res, role, id) {
  if (!canPatchFleetCommander(role)) {
    writeDenied(res, role, "fleet");
    return;
  }

  const resource = findResource("fleet", id);
  if (!resource) {
    notFound(res);
    return;
  }

  let body;
  try {
    body = await readJsonBody(req);
  } catch (error) {
    badRequest(res, error.message);
    return;
  }

  const immutableFields = [
    "id",
    "slug",
    "title",
    "class",
    "status",
    "readiness",
    "location",
  ];
  const changedImmutable = immutableFields.filter(
    (field) => Object.hasOwn(body, field) && body[field] !== resource[field],
  );
  if (changedImmutable.length > 0) {
    badRequest(res, "Only fleet commander assignment can be changed by this role.", {
      immutable_fields: changedImmutable,
    });
    return;
  }

  if (!Object.hasOwn(body, "commander")) {
    badRequest(res, "Missing required field: commander");
    return;
  }

  if (typeof body.commander !== "string" || body.commander.trim().length === 0) {
    badRequest(res, "commander must be a non-empty string");
    return;
  }

  resource.commander = body.commander.trim();
  resource.updated_at = new Date().toISOString();
  sendJson(res, 200, resource);
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://${req.headers.host}`);
  const parts = url.pathname.split("/").filter(Boolean);
  const role = roleFromRequest(req);

  if (url.pathname === "/health") {
    sendJson(res, 200, {
      ok: true,
      name: "Imperial Fleet Command Registry",
      role,
    });
    return;
  }

  if (url.pathname === "/openapi.yaml") {
    try {
      const spec = await readFile(path.join(here, "openapi.yaml"), "utf8");
      sendText(res, 200, spec, "application/yaml; charset=utf-8");
    } catch {
      notFound(res);
    }
    return;
  }

  if (url.pathname === "/docs" || url.pathname === "/docs/") {
    sendText(res, 200, swaggerUiHtml(), "text/html; charset=utf-8");
    return;
  }

  if (url.pathname === "/api/v1" || url.pathname === "/api/v1/") {
    if (req.method !== "GET") {
      writeDenied(res, role, "catalog");
      return;
    }
    sendJson(res, 200, {
      name: "Imperial Fleet Command Registry",
      role,
      collections: catalogFor(role),
    });
    return;
  }

  if (parts[0] !== "api" || parts[1] !== "v1" || !parts[2]) {
    notFound(res);
    return;
  }

  const collection = parts[2];
  const id = parts[3];

  if (req.method === "PATCH") {
    if (collection === "fleet" && id) {
      await patchFleetAsset(req, res, role, id);
      return;
    }
    writeDenied(res, role, collection);
    return;
  }

  if (req.method !== "GET") {
    writeDenied(res, role, collection);
    return;
  }

  if (hiddenCollections.has(collection) && role !== "moff") {
    notFound(res);
    return;
  }

  if (deniedCollections.has(collection) && role !== "moff") {
    restrictedDenied(res, role, collection);
    return;
  }

  if (!data[collection] || !canRead(role, collection)) {
    notFound(res);
    return;
  }

  if (!id) {
    sendJson(res, 200, data[collection]);
    return;
  }

  const resource = findResource(collection, id);
  if (!resource) {
    notFound(res);
    return;
  }

  sendJson(res, 200, resource);
});

server.on("error", (error) => {
  console.error(`Unable to start Imperial Fleet demo server: ${error.message}`);
  process.exitCode = 1;
});

server.listen(port, host, () => {
  console.log(`Imperial Fleet Command Registry listening on http://${host}:${port}`);
  console.log("Default role: sector-ops-analyst (fleet commander write allowed)");
  console.log("Optional admin token for direct API exploration: moff-clearance");
});

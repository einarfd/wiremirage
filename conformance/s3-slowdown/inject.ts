// Reusable latency / fault INJECTION engine.
//
// The generic core — read rules, match a request, inject latency, throttle
// with recovery — is API-agnostic. The only API-aware part is the two small
// functions at the bottom (`successResponse` / `throttleResponse`) that shape
// the bytes for the API being mocked; here, AWS S3. To reuse this for another
// API (e.g. a Vertex `(model, region)` selective slowdown) you keep the core
// and the rule format, and swap just those two functions + the rules.
//
// Rules live in GROUP state under "inject:rules", seeded via the writable-state
// API (ADR-0025: `PUT /api/groups/{group}/state`). Each rule:
//
//   { "match": { "path_prefix"?, "method"?, "query"?: {k:v}, "header"?: {k:v} },
//     "delay_ms"?: number, "throttle_first"?: number }
//
// First matching rule wins. `throttle_first: N` makes the first N requests for
// a given resource return a throttling error, then it recovers — so it's the
// real client's retry/backoff that makes the call ultimately succeed.

export function handle(req, routeStore, groupStore) {
  const rule = firstMatch(readRules(groupStore), req);

  if (rule && rule.delay_ms) host.sleep(rule.delay_ms);

  if (rule && rule.throttle_first) {
    // Per-resource attempt counter (route-private state). Distinct keys get
    // independent budgets, so the Nth retry of one key recovers without
    // affecting another.
    const n = Number(routeStore.incr("att:" + req.method + " " + req.path, 1n));
    if (n <= rule.throttle_first) return throttleResponse();
  }
  return successResponse(req);
}

function readRules(groupStore) {
  try {
    const raw = groupStore.get("inject:rules");
    if (!raw) return [];
    return JSON.parse(new TextDecoder().decode(new Uint8Array(raw)));
  } catch (_e) {
    return [];
  }
}

function firstMatch(rules, req) {
  if (!Array.isArray(rules)) return null;
  for (const r of rules) {
    const m = r.match || {};
    if (m.method && String(m.method).toUpperCase() !== req.method) continue;
    if (m.path_prefix && !String(req.path).startsWith(m.path_prefix)) continue;
    if (m.query && !pairsMatch(req.query, m.query, false)) continue;
    if (m.header && !pairsMatch(req.headers, m.header, true)) continue;
    return r;
  }
  return null;
}

// req.query / req.headers are [name, value][]; match every wanted k=v.
function pairsMatch(pairs, want, lowerKeys) {
  const have = new Map(
    (pairs || []).map(([k, v]) => [lowerKeys ? String(k).toLowerCase() : k, v]),
  );
  for (const k of Object.keys(want)) {
    if (have.get(lowerKeys ? k.toLowerCase() : k) !== want[k]) return false;
  }
  return true;
}

// --- S3-specific shapes (the only API-aware part) ---

function successResponse(req) {
  // GetObject success: the object bytes + an ETag. Deterministic so the
  // client can assert on it.
  return {
    status: 200,
    headers: [
      ["content-type", "application/octet-stream"],
      ["etag", '"wiremirage-mock-etag"'],
    ],
    body: new TextEncoder().encode("wiremirage-mock-object: " + req.path),
  };
}

function throttleResponse() {
  // S3's native rate-limit error. The AWS SDK classifies `SlowDown` (and the
  // 503 status) as retryable and backs off automatically.
  const xml =
    '<?xml version="1.0" encoding="UTF-8"?>' +
    "<Error><Code>SlowDown</Code>" +
    "<Message>Please reduce your request rate.</Message>" +
    "<RequestId>wiremirage-mock</RequestId><HostId>wiremirage</HostId></Error>";
  return {
    status: 503,
    headers: [["content-type", "application/xml"]],
    body: new TextEncoder().encode(xml),
  };
}

// A handler that exercises the whole typed surface — copy it as a starting
// point, and note that `just typecheck` compiles this file to prove
// `wiremirage-handler.d.ts` still describes something a real handler can be
// written against.

import type {
  WireMirageRequest,
  WireMirageResponse,
  WireMirageStore,
} from "./wiremirage-handler";

export function handle(
  req: WireMirageRequest,
  routeStore: WireMirageStore,
  groupStore: WireMirageStore,
): WireMirageResponse {
  // Counters are bigint: the WIT type is s64. `incr("calls", 1)` is the
  // single most common handler bug, and it is a type error here.
  const calls = routeStore.incr("calls", 1n);
  log.info(`call ${calls} for ${req.method} ${req.path}`);

  // Config seeded from outside (`wm routes state --set`), read as bytes.
  const mode = routeStore.get("mode");
  if (mode !== null && new TextDecoder().decode(mode) === "degraded") {
    return {
      status: 503,
      headers: [["retry-after", "1"]],
      body: new Uint8Array(),
    };
  }

  // Accessors beat scanning the tuple arrays by hand.
  const who = req.queryParam("who") ?? req.header("x-caller") ?? "world";
  const id = req.pathParam("id");

  // Cross-route state lives in the group store.
  groupStore.setAdd("seen-callers", who);

  host.sleep(5);

  const body = JSON.stringify({
    hello: who,
    id: id ?? null,
    calls: Number(calls),
    seenBefore: groupStore.setContains("seen-callers", who),
    at: host.wallTimeMs(),
  });

  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}

/** Streaming variant (ADR-0022): commit the head, then write chunks. */
export function handleStreaming(req: WireMirageRequest): void {
  const stream = host.responseStream({
    status: 200,
    headers: [["content-type", "text/event-stream"]],
  });
  for (const token of ["Hello", " ", req.path]) {
    if (!stream.write(`data: ${token}\n\n`)) break;
    host.sleep(50);
  }
  stream.close();
}

//! Handler-API capabilities, surfaced to agents through three routes:
//!
//! 1. **MCP** — `get_capabilities` tool, for clients connected via MCP
//!    (Claude Desktop, Cursor, MCP Inspector).
//! 2. **REST** — `GET /api/capabilities[/{topic}]`, for the `wm`
//!    CLI and any other bearer-token caller.
//! 3. **Skill** — `skill/wiremirage/SKILL.md` ships the same shape
//!    for agents driving via Bash. The skill is a static file; this
//!    module is the dynamic-via-API counterpart.
//!
//! Topics are split so a caller can fetch just the section relevant
//! to the task at hand instead of always paying for the full markdown
//! body. The no-arg form returns an overview that doubles as a topic
//! index. Unknown topic → fall back to overview (so typos don't error).
//!
//! Content here is deliberately JS-side and concrete — the WIT contract
//! at `wit/wiremirage.wit` is the canonical spec but is shaped for
//! protocol readers; the agent writing a TS handler needs
//! `routeStore.incr("count", 1n)` in front of them, not kebab-case
//! WIT type signatures.

/// One topic, indexed by its name (used as the URL segment / query
/// argument). Order matters — the overview lists topics in this order.
pub const TOPICS: &[(&str, &str)] = &[
    ("overview", OVERVIEW),
    ("request", REQUEST),
    ("response", RESPONSE),
    ("store", STORE),
    ("log", LOG),
    ("clock", CLOCK),
    ("streaming", STREAMING),
    ("gotchas", GOTCHAS),
];

/// Resolve a topic key to `(topic_name, content)`. Unknown / empty
/// keys fall back to the overview. The returned name reflects the
/// resolved topic — useful for callers that want to show the user
/// what they actually got (vs. what they asked for).
pub fn lookup(topic: Option<&str>) -> (&'static str, &'static str) {
    let key = topic.unwrap_or("overview").to_lowercase();
    TOPICS
        .iter()
        .find(|(k, _)| *k == key)
        .copied()
        .unwrap_or(("overview", OVERVIEW))
}

/// List of all known topic names. Same order as [`TOPICS`].
pub fn topic_names() -> Vec<&'static str> {
    TOPICS.iter().map(|(k, _)| *k).collect()
}

const OVERVIEW: &str = r#"# WireMirage handler API — overview

A WireMirage route's `source` field is a TypeScript or JavaScript module
that exports a single `handle` function. The host calls `handle` once
per matched HTTP request, in a fresh wasmtime instance. State that
should persist between calls goes through the per-route and per-group
stores.

## Minimal example

```ts
export function handle(req, routeStore, groupStore) {
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({ ok: true })),
  };
}
```

That's a complete handler. Drop it into `create_route`'s `source` field
with `language: "typescript"` (or `"javascript"`) and it'll serve the
declared `path` and methods.

## Available topics

Fetch each via the appropriate surface:

- **MCP**: `get_capabilities(topic: "<name>")`
- **CLI**: `wm capabilities <name>`
- **REST**: `GET /api/capabilities/<name>`

Topics:

- **`request`** — the request object's fields and types
- **`response`** — the response shape and reserved headers
- **`store`** — per-route and per-group state (kv + lists + hashes + sets)
- **`log`** — emitting log lines that attach to the journal entry
- **`clock`** — `host.sleep`, wall-time, monotonic time (ADR-0021)
- **`streaming`** — `host.responseStream` for SSE / chunked responses (ADR-0022)
- **`gotchas`** — bigint quirks, camelCase field names, and other footguns

## Key design points

- **Stores are typed bytes** (`Uint8Array` / `list<u8>`). Encode with
  `TextEncoder`, decode with `TextDecoder`. JSON is conventional but
  the runtime is value-agnostic.
- **Fresh instance per request.** Nothing in JS module-scope survives.
  Use the stores for any continuity.
- **`routeStore` is private** to this route. `groupStore` is shared
  with every other route in the same group. Use the group store for
  cross-route invariants (rate limits, session-like data).
- **The body is bytes.** `req.body` is a `Uint8Array`; the response's
  `body` must also be `Uint8Array`. Use TextEncoder/TextDecoder.
"#;

const REQUEST: &str = r#"# Request

The first parameter to `handle`. Field names are camelCase (the WIT
contract is kebab-case but the JS binding camelCases everything).

```ts
{
  method: string,          // "GET", "POST", etc. Always uppercase.
  path: string,            // Full literal path. e.g., "/users/123/posts/456"
  matchedPattern: string,  // The route pattern that matched. e.g., "/users/{id}/posts/{post-id}"
  pathParams: [string, string][],  // [["id","123"],["post-id","456"]]
  query: [string, string][],       // Parsed query parameters. Names lowercased.
  headers: [string, string][],     // Request headers. Names lowercased. Multi-valued repeat.
  body: Uint8Array,        // Raw body bytes. May be empty. Capped at 10 MiB.
}
```

## Reading the body

```ts
const bodyText = new TextDecoder().decode(req.body);
const parsed = JSON.parse(bodyText);  // if you expect JSON
```

## Reading a header

```ts
// Headers are tuples, not a map — iterate or filter.
const ct = req.headers.find(([k]) => k === "content-type")?.[1];
```

## Reading a path parameter

```ts
// Route registered with path "/users/{id}/posts/{post-id}"
const userId = req.pathParams.find(([k]) => k === "id")?.[1];
const postId = req.pathParams.find(([k]) => k === "post-id")?.[1];
```
"#;

const RESPONSE: &str = r#"# Response

The handler returns one object. Same field-name conventions as request.

```ts
{
  status: number,           // Any 100–599. Non-standard values allowed.
  headers: [string, string][],  // See "Reserved headers" below.
  body: Uint8Array,         // Raw bytes. Empty array for no body.
}
```

## Minimal returns

```ts
// Empty 204
return { status: 204, headers: [], body: new Uint8Array() };

// JSON 200
return {
  status: 200,
  headers: [["content-type", "application/json"]],
  body: new TextEncoder().encode(JSON.stringify({ ok: true })),
};

// Plain text error
return {
  status: 503,
  headers: [["content-type", "text/plain; charset=utf-8"]],
  body: new TextEncoder().encode("upstream timeout"),
};
```

## Reserved headers

A small set of headers the host computes regardless of what you set —
trying to set them gets a warning logged on the journal entry and your
value dropped:

- `Content-Length` — always computed from the body bytes.
- `Transfer-Encoding` — managed by the host.
- `Connection` — managed by the host.
- `Date` — auto-set if you didn't provide one.

Everything else passes through verbatim, including malformed values
and non-standard headers — mocks need to be able to simulate real
APIs' quirks.
"#;

const STORE: &str = r#"# Store

Two stores are passed to every handler: `routeStore` (private to this
route) and `groupStore` (shared with every route in the same group).
Both expose the same API.

## Basic key-value

```ts
routeStore.get("key");                    // Uint8Array | null
routeStore.set("key", new TextEncoder().encode("value"));
routeStore.delete("key");                 // no-op if absent
routeStore.listKeys("prefix" /* or null for everything */);  // string[]
```

## Atomic counter (returns bigint!)

```ts
// Both args matter: the increment amount is the SECOND.
// Note `1n` not `1` — the WIT type is s64 → bigint on the JS side.
const n = routeStore.incr("calls", 1n);
// n is a bigint. Convert for JSON: Number(n).
```

## Lists (queues, ordered logs)

```ts
routeStore.listPush("queue", new TextEncoder().encode("item"));
routeStore.listPop("queue");              // Uint8Array | null (leftmost)
routeStore.listRange("queue", 0n, -1n);   // entire list as Uint8Array[]
routeStore.listLength("queue");           // bigint
```

## Hashes (record-shaped data)

```ts
routeStore.hashSet("user:42", "name", new TextEncoder().encode("Alice"));
routeStore.hashGet("user:42", "name");    // Uint8Array | null
routeStore.hashDelete("user:42", "name");
routeStore.hashKeys("user:42");           // string[]
```

## Sets (unique members)

```ts
routeStore.setAdd("seen-ips", "10.0.0.1");
routeStore.setContains("seen-ips", "10.0.0.1");  // boolean
routeStore.setRemove("seen-ips", "10.0.0.1");
```

## When to use routeStore vs groupStore

- **`routeStore`** — counters per route, last-seen payload, simulated
  upstream state for *this specific endpoint*.
- **`groupStore`** — anything that crosses routes within a logical
  test scenario: rate-limit windows, session tokens issued by an `auth`
  route and validated by a `me` route, customer records shared across
  `charges`/`refunds`/`customers`.

State survives until the group expires (default 24h sliding TTL) or
you call `wm groups state GROUP --clear` (CLI) / `clear_group_state`
(MCP).
"#;

const LOG: &str = r#"# Log

The host imports a logging interface so handlers can attach
structured lines to the request's journal entry.

```ts
// `log` is a global the host injects. Levels: debug | info | warn | error.
log.emit("info", "received request for /v1/charges");
log.emit("warn", `unexpected field: ${field}`);
log.emit("error", `panic: ${e.message}`);

// Level-named conveniences and the familiar console.* methods work too —
// both route to the same journal channel (console.log → info,
// console.warn → warn, console.error → error, console.debug → debug):
log.info("same as log.emit(\"info\", ...)");
console.log("received", req.method, req.path);  // joined with spaces
```

Logs attach to the journal record for this request and show up in:

- `wm journal show <group>/<n>` (CLI)
- the `/ui/journal/{group}/{n}` page (web UI)
- the `handler_logs` array in a `dry_run_route` result

Logs do NOT go to stdout; the wasm sandbox doesn't expose stdio. `log.*`
and `console.*` are the only way to surface anything from a handler.
"#;

const CLOCK: &str = r#"# Clock — host.sleep, wall-time, monotonic time

A `host` global exposes three time primitives (ADR-0021).

## host.sleep(ms)

Block the handler for `ms` milliseconds before returning. Counts against
the wasm sandbox's per-request wall-clock budget — ~30s for JS/TS
handlers (the typical case), ~1s for AOT-compiled wasm components. A
sleep that exceeds the budget traps the handler.

```ts
export function handle(req, routeStore, groupStore) {
  host.sleep(200);   // 200ms before responding
  return { status: 200, headers: [], body: new Uint8Array() };
}
```

## host.wallTimeMs() — wall-clock UTC milliseconds since Unix epoch

```ts
const now = host.wallTimeMs();   // e.g. 1716800000000
```

May jump backwards on NTP correction. **Don't use for measuring
elapsed time** — use `monotonicMs` for that.

## host.monotonicMs() — monotonic counter for measuring durations

Opaque counter anchored at host process start. Only useful as a
*difference*: store the value, read it again later, subtract.

```ts
export function handle(req, routeStore, groupStore) {
  const start = host.monotonicMs();
  // ... do some work ...
  const elapsedMs = host.monotonicMs() - start;
  log.emit("info", `request took ${elapsedMs}ms`);
  return { status: 200, headers: [], body: new Uint8Array() };
}
```

Never decreases, never jumps backward. Survives across requests within
the same host process so you can compute "how long has this group
been active" by storing the first-seen value in the store.

## Latency simulation pattern

Combine `monotonicMs` + the route store + `sleep` to make response time
grow over a test run — useful for reproducing API-gateway
cascading-failure modes that depend on response time creeping past a
timeout threshold:

```ts
export function handle(_req, routeStore, _group) {
  const now = host.monotonicMs();
  const startBytes = routeStore.get("first_seen_ms");
  let start;
  if (startBytes === null) {
    start = now;
    routeStore.set("first_seen_ms", new TextEncoder().encode(String(now)));
  } else {
    start = Number(new TextDecoder().decode(startBytes));
  }
  const elapsedSec = (now - start) / 1000;
  // Ramp up: +50ms per second of elapsed test time, capped at 30s.
  const delay = Math.min(30000, Math.trunc(50 + 50 * elapsedSec));
  host.sleep(delay);
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({ ok: true, delay_ms: delay })),
  };
}
```
"#;

const STREAMING: &str = r#"# Streaming responses — host.responseStream

By default `handle` returns one buffered response. To stream a response
incrementally — Server-Sent Events, chunked bodies, anything where the
client should see bytes as they're produced — call `host.responseStream`
instead of returning a response (ADR-0022). This is what you reach for to
mock streaming LLM APIs (Vertex `streamGenerateContent`, OpenAI
`chat/completions` with `stream: true`, Anthropic `messages`) and the MCP
streamable-HTTP transport.

## API

```ts
const out = host.responseStream({
  status: 200,
  headers: [["content-type", "text/event-stream"]],
});
out.write("data: hello\n\n");   // string or Uint8Array; returns false if the client left
out.close();                    // end the body
```

- **`host.responseStream({status, headers})`** commits the response head
  immediately and returns a writer. After this the status + headers are
  on the wire and can't be changed.
- **`.write(chunk)`** flushes one chunk to the client right away. `chunk`
  is a string (UTF-8 encoded for you) or a `Uint8Array`. Returns `false`
  once the client has disconnected — stop writing and return.
- **`.close()`** ends the stream.
- A streaming handler **doesn't need to return anything** — the host
  uses the streamed body, not the return value.

## SSE example — token-by-token, paced

```ts
export function handle(req, routeStore, groupStore) {
  const out = host.responseStream({
    status: 200,
    headers: [["content-type", "text/event-stream"]],
  });
  const tokens = ["Hello", " from", " a", " streamed", " mock"];
  for (const tok of tokens) {
    const chunk = JSON.stringify({ choices: [{ delta: { content: tok } }] });
    if (!out.write(`data: ${chunk}\n\n`)) return;  // client gone
    host.sleep(40);                                 // inter-token latency
  }
  out.write("data: [DONE]\n\n");
  out.close();
}
```

## Notes

- **Backpressure is automatic.** A slow client makes `.write` block until
  the buffer drains; you don't manage it.
- **Pace with `host.sleep`** between writes (see the `clock` topic) — that
  is how you reproduce inter-token latency or a slow upstream.
- **Budget.** A streaming handler may run up to ~5 minutes (vs the ~30s
  buffered cap); past that it's trapped. Long enough for realistic LLM
  streams and MCP long-polls.
- **Source-language only.** `host.responseStream` is available to
  TypeScript / JavaScript handlers. Pre-compiled wasm components don't
  have it (they return a buffered response).
- **Journal.** Streamed responses are journaled with a `[stream] N chunks,
  M bytes, <disposition>` summary line; the body itself isn't captured.
- **Dry-run** (`wm routes test` / `dry_run_route`) collects the streamed
  chunks in-process and returns the concatenated body, so you can inspect
  a streaming handler without a real client.
"#;

const GOTCHAS: &str = r#"# Gotchas

Things that surprise people writing their first handler.

## `incr` returns a bigint, not a number

The WIT type is `s64` (signed 64-bit integer), which maps to JS
`bigint`, not `number`. Two consequences:

```ts
// Both must use the `n` suffix:
const n = routeStore.incr("count", 1n);   // n is bigint
if (n % 3n === 0n) { /* ... */ }

// Convert for JSON:
JSON.stringify({ count: Number(n) });     // not just `count: n`
// (JSON.stringify can't serialize bigint directly.)
```

Same applies to `listLength` — also returns `bigint`. And `listRange`
takes bigint indices.

## Field names are camelCase, not snake_case

The WIT contract is kebab-case (`path-params`, `matched-pattern`,
`route-store`). The JS binding produces camelCase (`pathParams`,
`matchedPattern`). Get this wrong and you get a runtime trap that's
hard to read.

## The body is Uint8Array on both sides

Request `body` and response `body` are bytes (`Uint8Array` /
`list<u8>`). The runtime is value-agnostic — JSON is conventional but
not required. Encode with `TextEncoder` before returning, decode with
`TextDecoder` when reading.

```ts
// Reading
const text = new TextDecoder().decode(req.body);
// Writing
body: new TextEncoder().encode(JSON.stringify(data));
```

## Mock traffic is unauthenticated by design

The `/api/*`, `/ui/*`, `/auth/*` paths require auth. Everything
else (including your handler's path) is intentionally open — system-
under-test code typically doesn't carry tokens. Don't put secrets in
the URL.

## No state reset from inside handlers

A handler can read/write its store but can't bulk-clear it. Use
`clear_route_state` / `clear_group_state` MCP tools (or `wm groups
state GROUP --clear` from the CLI) between test phases.

## host.sleep eats into the wasm budget

The 30-second epoch deadline is wall-clock — a `host.sleep(28000)`
leaves only ~2 seconds for the rest of the handler to run. For
latency-simulation that wants delays close to the limit, do the sleep
LAST in the handler so the budget isn't exhausted before you return.

## Simulating an upstream that hangs (past 30s)

To test how a client/SDK behaves against a connection that's accepted
and then never answers — a true upstream hang beyond ~30s — use a
**streaming** handler, not a buffered one. A buffered handler is capped
at the ~30s epoch and traps if it sleeps longer, so it *can't* hold a
connection open for a minute. A streaming handler raises the budget to
~5 minutes: commit the head with `host.responseStream({status: 200,
headers: []})` and then `host.sleep(120000)` without ever calling
`.write` / `.close` to keep the socket open and silent far past the
buffered limit. See the `streaming` topic.

## Fresh instance per request — no JS module-scope persistence

Anything in JS top-level scope (variables declared outside `handle`,
caches, module-level computations) resets between requests. The fresh
wasmtime instance per call is by design — it gives clean isolation as
a property of the architecture. Use `routeStore` / `groupStore` for
any continuity.

## No network access — `fetch` and friends throw

Handlers have no outbound network: the sandbox imports are store / log /
clock / response-stream only. The JS engine still *exposes* web globals
like `fetch`, `WebSocket`, `EventSource`, and `XMLHttpRequest`, but calling
them throws a catchable `Error` ("network access is not available in
WireMirage handlers") rather than doing anything. Don't try to reach a real
upstream from a handler — mock that upstream as another route and point your
system-under-test at it.
"#;

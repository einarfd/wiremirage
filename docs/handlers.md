# Writing handlers

A handler is a small TypeScript or JavaScript function. The host transpiles
TypeScript in-process (swc) and runs both languages inside a shared
WebAssembly engine component, one fresh instance per request
([ADR-0020](adr/0020-shared-wasm-engine-for-interpreted-languages.md)). There
is no build step and no toolchain to install: you hand the host source, it
hands back a live route.

The contract handlers program against is `wit/wiremirage.wit`. The live,
always-current version of everything below is `wm capabilities [topic]`
(topics: `overview`, `request`, `response`, `store`, `log`, `clock`,
`streaming`, `callbacks`, `gotchas`), or the `get_capabilities` MCP tool —
both read from the running host rather than a static copy.

## The shape

```ts
export function handle(req, routeStore, groupStore) {
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({ ok: true })),
  };
}
```

`req` is:

```ts
{
  method: string,              // "GET", "POST", … always uppercase
  path: string,                // "/users/123/posts/456"
  matchedPattern: string,      // "/users/{id}/posts/{post-id}"
  pathParams: [string, string][],
  query: [string, string][],   // names lowercased
  headers: [string, string][], // names lowercased
  body: Uint8Array,            // may be empty
}
```

The response is `{ status, headers, body }` — any status code, headers as
pairs, body as bytes.

**Two binding quirks**, both inherited from the WIT-to-JS conversion:

- **Field names are camelCase.** The WIT contract is kebab-case
  (`path-params`, `matched-pattern`); the JS binding produces `pathParams`,
  `matchedPattern`. Getting this wrong is a runtime trap with an unhelpful
  message.
- **`incr` returns a `bigint`** — the WIT type is `s64`. Write
  `routeStore.incr("count", 1n)`, compare with `n % 3n === 0n`, and convert
  with `Number(n)` before putting it in JSON.

## Path patterns

A route matches on `(method, path-pattern)` within its group. Patterns are:

| Form | Matches |
|---|---|
| `/v1/charges` | exactly that path |
| `/users/{id}` | one segment, captured as `id` in `pathParams` |
| `/v1/{rest...}` | the remaining segments, joined — including **zero** of them, so `/v1` matches too |

The trailing `{name...}` form is only valid as the final segment, and it has
the **lowest** match precedence: a literal or `{param}` route always wins over
it ([ADR-0028](adr/0028-trailing-segment-path-matcher.md)). Overlapping
patterns within one group are rejected at create time; different groups are
different namespaces and can't collide.

## State

Both stores expose the same interface. `routeStore` is private to the route;
`groupStore` is shared by every route in the group. State survives between
requests until the group expires or you reset it.

```ts
routeStore.get("key");                   // Uint8Array | null
routeStore.set("key", bytes);
routeStore.delete("key");
routeStore.incr("calls", 1n);            // atomic, returns bigint
routeStore.listKeys("prefix");           // string[]; null for everything

routeStore.listPush("queue", bytes);     // queues / ordered logs
routeStore.listPop("queue");             // leftmost, or null
routeStore.listRange("queue", 0n, -1n);  // Uint8Array[]; -1 is the last item
routeStore.listLength("queue");          // bigint

routeStore.hashSet("user:42", "name", bytes);
routeStore.hashGet("user:42", "name");   // Uint8Array | null
routeStore.hashDelete("user:42", "name");
routeStore.hashKeys("user:42");          // string[]

routeStore.setAdd("seen", "10.0.0.1");
routeStore.setContains("seen", "10.0.0.1");  // boolean
routeStore.setRemove("seen", "10.0.0.1");
```

> **`incr` can't take a negative delta yet.** Lowering a negative 64-bit
> signed integer out of JavaScript trips a defect in the toolchain that builds
> the engine ([ComponentizeJS#343](https://github.com/bytecodealliance/ComponentizeJS/issues/343)),
> so `incr(key, -1n)` throws with an explanation instead of decrementing.
> Count upwards, or keep the value with `set()`. Negative `listRange` indices
> *do* work — the engine resolves them before they cross the boundary.

Handlers work a key at a time — bulk seeding and clearing are *external*
operations (`wm routes state`, `wm groups state`, the equivalent REST/MCP
calls). That's how you configure a mock without driving traffic through it
first: seed a config key, have the handler read it
([ADR-0025](adr/0025-writable-handler-state.md)). Externally-written values
are UTF-8 strings (or `{"base64": "..."}` for binary), capped at 1 MiB per
key.

Each request gets a fresh instance, so module-level JavaScript variables do
**not** persist. Anything you want to remember goes through a store.

## Logging

The handler imports a `log` interface. Lines emitted from a handler attach to
that request's journal entry and show up in `wm journal show <group>/<n>` —
which is how you debug a handler you can't attach a debugger to.

## Time

A `host` global exposes three time primitives
([ADR-0021](adr/0021-time-primitives-in-handler-wit.md)):

- `host.sleep(ms)` — block the handler. Counts against the wall-clock budget
  (see [limits](#limits)); exceeding it traps. This is how you simulate a slow
  upstream.
- `host.wallTimeMs()` — Unix epoch milliseconds. May jump on NTP correction.
- `host.monotonicMs()` — opaque monotonic counter; useful only as a
  difference.

## Streaming responses

For SSE, chunked bodies, streaming LLM APIs, or an MCP transport
([ADR-0022](adr/0022-streaming-http-responses.md)):

```ts
export function handle(req, routeStore, groupStore) {
  const w = host.responseStream({
    status: 200,
    headers: [["content-type", "text/event-stream"]],
  });
  for (const token of ["Hello", " ", "world"]) {
    if (!w.write(new TextEncoder().encode(`data: ${token}\n\n`))) break;
    host.sleep(60);
  }
  w.close();
}
```

`write()` flushes each chunk to the client immediately and returns `false`
once the client has disconnected. A streaming handler returns nothing — the
host uses the streamed body. Streaming handlers get a much longer budget (~5
minutes) than buffered ones, which is also how you simulate an upstream that
hangs: commit the head, then sleep without writing.

## Outbound callbacks (webhooks)

To mock a service that calls *back* into the system under test — a Stripe
`charge.succeeded`, a GitHub webhook
([ADR-0034](adr/0034-outbound-callbacks.md)):

```ts
host.scheduleCallback({
  url: "https://sut.internal/webhooks/stripe",
  method: "POST",
  headers: [["content-type", "application/json"]],
  body: new TextEncoder().encode(JSON.stringify({ type: "charge.succeeded" })),
  delayMs: 500,
});
```

The host fires it **once**, on a background task, after your response is sent.
Single-attempt, best-effort, no retries — WireMirage is a mock, not a delivery
system. It is the only network egress out of the sandbox and is gated twice:
the host must run with `WM_EGRESS` on, and the group must opt in
(`wm groups update <group> --callout`). If either is off, `scheduleCallback`
throws — catch it. Delivery outcomes can't ride the response (already sent),
so they land in the group's callback journal: `wm callbacks list <group>`, MCP
`list_callbacks`, `GET /api/groups/{group}/callbacks`, or the group's UI page.
Up to 16 callbacks per request; `delayMs` caps at 5 minutes. Dry-runs never
fire real callbacks.

## Limits

The sandbox is bounded on several axes at once; whichever fires first traps
the call, and the journal records which ([ADR-0002](adr/0002-wasm-sandbox.md),
[ADR-0032](adr/0032-sandbox-limits-multi-tenant.md)):

| Limit | Value |
|---|---|
| Wall clock, buffered handler | ~30 s |
| Wall clock, streaming handler | ~5 min |
| Memory per call | 64 MiB (256 MiB for the shared engine instance) |
| Request body (mock traffic) | 10 MiB — larger returns 413 before the handler runs, and isn't journaled |
| Journalled request/response body | truncated at 16 KiB (4 KiB for unmatched) |
| External state value | 1 MiB per key |
| Handler `sleep` | 30 s per call |

Fuel metering runs alongside the wall clock as a runaway-loop backstop. The
per-call fuel, memory peak, and wall time are recorded on every journal entry
and exported as metrics — see [observability](observability.md).

## Reserved headers

Some response headers are managed by the host (framing, content-length,
transfer-encoding). Setting them from a handler is ignored, and the journal
entry shows which ones were dropped. `wm capabilities response` lists the
current set.

## Testing a handler without a SUT

`wm routes test <group>/<n>` (REST `POST /api/routes/{group}/{n}/dry-run`, MCP
`dry_run_route`) runs the handler against a synthetic request. State reads see
a point-in-time snapshot and writes are discarded, so a dry-run never mutates
real state and never writes a journal entry. `--kv key=value` / `--gkv
key=value` seed the snapshot, which is how you exercise a state-dependent
branch ("the fourth call returns 503") without driving three real requests
first.

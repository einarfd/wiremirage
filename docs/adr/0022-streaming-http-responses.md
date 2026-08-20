# ADR-0022: Streaming HTTP responses (writer resource on a generic byte-stream substrate)

**Status:** Accepted

**Amendment (2026-05-26) — shipped shape differs from the Decision below; the Decision text is kept as the original reasoning.** Slice 1 shipped streaming as **additive engine-world host imports**, not the generic `byte-sink` + `response-writer` + reshaped `handle` export described under "Decision". What landed:

- A `response-stream` interface (`start(status, headers)` / `write-chunk(bytes) -> bool` / `finish()`) imported by the `engine` world (the source-language dispatch path). The js-engine shim exposes it as `host.responseStream({status, headers}) → { write, close }`.
- The host runs the handler on its existing `spawn_blocking` thread and `select!`s head-vs-completion: `start` sends the head over a oneshot so dispatch returns the axum response immediately, `write-chunk` does a bounded `blocking_send` (backpressure) into a `Body::from_stream`, a dropped receiver makes `write-chunk` return `false` (client gone), `finish` ends the body. Incremental wire delivery is verified by a timing test.

Why this instead of the byte-sink design: the host-imports shape is simpler, componentize-js bound it without trouble, and it required **no `handle` export reshape** — so the contract break the Decision/​Depends-on worry about never materialised (the change is purely additive; no `bindings_version` gate, no migration). Per the project's "don't ship speculative abstraction" preference, the generic `byte-sink` substrate (whose only payoff is WebSocket/gRPC reuse) is **deferred until WebSocket actually lands**; it can be introduced then by re-layering these imports onto it.

Consequences of the shipped shape: only the **source/engine path** streams today — per-route AOT/component handlers (the user-facing `world handler`) are unchanged and still buffered; a streaming `world handler` is future work if an AOT language needs it. Stream duration is bounded by the existing ~30 s engine epoch deadline (slice 2 extends it). The byte-sink/response-writer design and the contract-break discussion below are retained as the original record.

**Depends on:** [0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md) should land first. It removes the pre-compiled-wasm upload surface — the *only* public path that breaks when this ADR reshapes the `handle` export. With 0023 in place, the contract-break handling below (the `bindings_version` rejection gate and the migration story) is unnecessary: source-language handlers are insulated from WIT changes by the engine shim, so there is nothing left to version-gate. The contract-break discussion is retained for the case where the two ADRs are sequenced the other way.

**Context:**

The handler contract (../script-api-wit.md) is request-in, response-out: `handle` returns a single `response { status, headers, body: list<u8> }`. The host buffers that body fully and writes it to the wire in one shot. `script-api-wit.md`'s "what's deliberately not in v0.1" section already flags this as a known gap ("Streaming request and response bodies … if we ever need to mock APIs with … long-lived event streams, we'll add a streaming variant"), and [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) named streaming responses as a follow-up for which `sleep` is half the prerequisite.

The forcing functions are now concrete:

1. **LLM API mocks that stream (the motivating case).** Google Vertex `streamGenerateContent`, OpenAI `chat/completions` with `stream: true`, and Anthropic `messages` SSE all respond with `Content-Type: text/event-stream` and push `data: {...}` chunks over the life of the response. A buffered 200 won't even parse on the SUT side — the client is an SSE reader expecting framed events as they arrive. This is not "slow response" (which `sleep` + a buffered body already covers); it is *inter-token latency* — the connection stays alive chunk-by-chunk and each chunk can be slow. Reproducing a gateway's cascading-failure behaviour when inter-token latency ramps up requires a mock that actually streams.

2. **MCP servers over the streamable-HTTP transport.** WireMirage's own MCP server uses this transport; mocking *other* people's MCP servers (to test an MCP client / agent harness) needs it too. The transport lets the server answer a JSON-RPC POST with either a single `application/json` response *or* a `text/event-stream` that carries multiple messages over time — server-initiated notifications, progress events, and long-running tool results. Only the synchronous-JSON half is expressible today; the SSE half needs streaming.

3. **General SSE / chunked-HTTP APIs.** Anything that holds the response open and emits events (log tails, change feeds, server-push notifications) is currently un-mockable. WireMock, mountebank, and Prism all either support or are adding this; a programmable mock server that can't do SSE is an outlier.

The shape of the problem is friendlier than general async I/O: the response is **unidirectional** (after the head is sent, only the server writes), the handler is operator-authored (not adversarial, per [0002-wasm-sandbox.md](0002-wasm-sandbox.md)), and `sleep` already exists to pace chunks. What's missing is a way for the handler to (a) commit a status + headers, then (b) emit body bytes incrementally while staying parked between emissions, and (c) learn when the client has gone away.

A design constraint worth stating up front: we expect to want **WebSocket** and eventually **bidirectional gRPC** mocks later (both came up as "keep the door open" cases). Those are out of scope here, but the streaming-response primitive should not be shaped so HTTP-specifically that it has to be reinvented for them.

**Decision:**

Add streaming responses as a **push-based writer resource layered on a generic byte-stream substrate**, opt-in per request, with buffered responses remaining the unchanged default.

**1. A generic byte-sink resource (the substrate).** The reusable primitive is protocol-agnostic — it knows about bytes, flushing, completion, and cancellation, and nothing about HTTP:

```wit
interface streams {
  // A write-only byte stream the handler pushes into. Backpressure is
  // expressed by `write` awaiting the host's flush; cancellation by
  // `write` returning `closed`.
  resource byte-sink {
    // Write bytes. Returns once the host has accepted them for delivery
    // (awaits downstream flush — this is where backpressure lives).
    // Returns `closed` if the peer has gone away; the handler should
    // stop and return.
    write: func(bytes: list<u8>) -> result<_, sink-error>;

    // Signal end-of-stream. After this the sink is spent.
    finish: func();
  }

  enum sink-error {
    closed,        // peer disconnected; stop writing
    too-large,     // cumulative bytes exceeded the streaming byte budget
  }
}
```

**2. An HTTP response-writer that wraps a byte-sink.** The HTTP-specific layer is thin — it adds exactly "commit the head, then give me the body sink":

```wit
interface http-response {
  use http.{header};
  use streams.{byte-sink};

  resource response-writer {
    // Buffered fast-path: send a complete response. Mutually exclusive
    // with `start`. This is what non-streaming handlers use.
    send: func(status: u16, headers: list<header>, body: list<u8>);

    // Streaming path: commit status + headers, get the body sink.
    // The head goes on the wire immediately; reserved-header rules
    // (see script-api-wit.md) apply as for buffered responses, except
    // Content-Length is omitted and chunked transfer-encoding is used.
    // Calling `start` twice, or `start` after `send`, traps.
    start: func(status: u16, headers: list<header>) -> byte-sink;
  }
}
```

**3. The handler world passes the writer as an out-parameter.** The `handle` export is reshaped to receive a `response-writer` rather than return a `response`:

```wit
export handle: func(
  req: request,
  route-store: borrow<bucket>,
  group-store: borrow<bucket>,
  response-out: response-writer,
);
```

This is a contract bump (handler@0.1.0 → @0.2.0). The buffered case is one `response-out.send(...)` call; the streaming case is `start(...)` then `write(...)*` then `finish()`. **Language SDKs hide the reshape**: the js-engine shim keeps the author-facing `export function handle(req, rs, gs) { return {status, headers, body} }` working by calling `send(...)` with the returned record, exactly as the `bucket` resource is hidden behind `routeStore` today. Streaming handlers get an ergonomic object — shape TBD in the shim slice, e.g.:

```js
export function handle(req, routeStore, groupStore) {
  const out = responseStream({ status: 200, headers: [["content-type","text/event-stream"]] });
  for (const tok of tokens) {
    host.sleep(40);                       // inter-token latency
    if (!out.write(`data: ${JSON.stringify(tok)}\n\n`)) return; // peer gone
  }
  out.write("data: [DONE]\n\n");
  out.close();
}
```

Existing **source-language** handlers (TS/JS) keep working untouched — they're re-instantiated against the new engine and the shim's buffered adapter. Existing **pre-compiled wasm** components built against handler@0.1.0 do not satisfy the @0.2.0 world; the `bindings_version` gate refuses to load them with an actionable error (the operator re-uploads source or rebuilds). We do not keep a parallel 0.1.0 instantiation path — per the project's no-dead-scaffolding convention.

**4. Streaming gets its own resource budget.** Buffered responses keep the tight slice-46 bounds (~30s epoch deadline, 10 MB `handler-value-size` body cap). Streaming responses need a different envelope and get a host-configured one:

- **Max stream wall-clock duration** (default e.g. 300s) — replaces the epoch deadline once `start` is called; the epoch ticker is suspended for the streaming phase and the duration is enforced by a separate timer.
- **Max total streamed bytes** (default e.g. 100 MB) — `write` returns `too-large` past it.
- **Fuel** still applies and is largely irrelevant — a handler that mostly sleeps between chunks burns little fuel; a handler that spins burns its budget and traps as today.

This resolves, for the streaming case, the "per-route resource-limit overrides" item deferred in `script-api-wit.md`: the first real split is streaming-vs-buffered at the host-config level, not yet per-route. Per-route overrides remain deferred.

**5. Semantics for the hard cases.**

- **Backpressure.** `write` awaits the host's downstream flush (the async wasmtime + tokio machinery already used for `sleep` parks the handler). A slow consumer slows the handler instead of growing an unbounded buffer.
- **Cancellation.** When the SUT disconnects, the next `write` returns `closed`. Handlers are expected to check and return. The host also hard-stops a handler that ignores `closed` and keeps writing past a small grace.
- **Mid-stream trap.** Once `start` has put the head on the wire, status is committed. A subsequent trap (or budget exhaustion) closes the connection abruptly — there is no way to "change the status to 500" after the fact. This is inherent to streaming and matches how real upstreams fail mid-stream (which is itself a useful failure to mock).
- **Journal.** Streaming entries record status + headers + chunk count + total bytes + duration + a head/tail body sample — **not** every chunk. Full chunk capture would bloat the journal and the 1h-TTL'd records; the sample plus counts is enough to confirm what happened. A `streamed: true` flag and a terminal disposition (`finished` / `client-disconnected` / `trapped` / `budget-exceeded`) go on the record.
- **Dry-run.** This is the one surface where chunk-level capture *is* worth it and is cheap (no real client, no backpressure): `dry_run` returns the full ordered sequence of `(chunk-bytes, ms-since-previous-chunk)` plus the terminal disposition, so an agent can assert on framing *and* timing without driving real traffic. The existing snapshot-isolation of `kv:`/`gkv:` is unchanged.

**Consequences:**

- **LLM streaming mocks become writeable**, including the inter-token-latency-ramp reproduction that motivated [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) — `monotonic-ms` to measure elapsed time, `sleep` to pace chunks, the writer to emit them.
- **MCP-server mocks become writeable** end to end (both response modes of the streamable-HTTP transport), unblocking testing of MCP clients / agent harnesses against controlled server behaviour.
- **The substrate generalizes.** Because `byte-sink` is HTTP-agnostic, a future WebSocket handler reuses it for the outbound direction and adds an inbound `byte-source` of the same shape; bidirectional gRPC reuses both. The HTTP-specific knowledge stays in the thin `response-writer`. We don't have to re-litigate the streaming primitive when those land — only add the transport-specific wrapper and (for WebSocket) a different handler lifetime model.
- **One contract break, mostly absorbed by SDKs.** Source-language handlers are unaffected at author level; only the WIT export and the js-engine shim change, plus pre-compiled-wasm uploads must rebuild against @0.2.0. Acceptable pre-1.0; the `bindings_version` gate makes the failure explicit rather than silent.
- **Two resource envelopes to reason about.** Buffered and streaming routes now have different limits, which is more operational surface. Accepted: a single envelope can't be both "tight enough to bound a runaway buffered handler" and "loose enough for a 5-minute SSE stream."
- **Dispatch path grows a streaming branch.** Today dispatch awaits a `response` and writes it; it now must, when the handler calls `start`, begin an axum streaming body and pump the sink while the handler runs. The handler's lifetime extends across the whole response rather than ending before the first byte. More moving parts in the hottest code path.
- **Determinism / observability trade-off** is the same shape as ADR-0021's: streaming handlers are inherently time-dependent and produce journal entries that summarize rather than fully capture the response. We accept the summary; dry-run is the full-fidelity escape hatch.

**Alternatives considered:**

- **Native component-model `stream<u8>` return** (`response { status, headers, body: stream<u8> }`). The "correct" component-model shape and avoids a host-import-driven writer. Rejected for now on toolchain maturity: componentize-js's `stream` support is young and we'd be early adopters fighting the bundler, and `stream<T>` is HTTP/return-shaped in a way that doesn't obviously give us the reusable byte-sink substrate we want for WebSocket. Revisit if componentize-js's stream support matures and the substrate argument turns out not to matter.
- **Adopt `wasi:http/types` response-body shape wholesale** (`response-outparam` + `outgoing-body` + `output-stream`). Well-trodden, and StarlingMonkey already implements wasi:http, so the JS side might come nearly free. Rejected for the same reasons [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) rejected `wasi:clocks`: it couples our contract to a WASI version and pulls in a larger, HTTP-specific surface, and — decisively here — its body stream is HTTP-shaped, so it wouldn't serve as the cross-protocol substrate WebSocket/gRPC want. We can still let the js-engine shim *bridge* to whatever StarlingMonkey uses internally without exposing wasi:http in our contract.
- **Separate `handle-stream` export alongside an unchanged buffered `handle`.** Avoids touching the buffered signature. Rejected: a component must satisfy every export its world declares, so this forces either two worlds (two engine bundles / conditional instantiation) or every handler implementing both. The out-parameter reshape gives one export that does both, with the SDK preserving buffered DX.
- **Declarative SSE template on the route record** (e.g. a list of `{delay_ms, data}` events configured at create time). Covers canned token streams without a WIT change, and is genuinely simpler for the static case. Rejected for the same reason ADR-0021 rejected the declarative `response_delay_ms` field: it can't condition the stream on request content, state, or elapsed time, so it walks away from the dynamic latency-ramp and stateful-MCP scenarios that are the whole point. Once the writer exists, a canned stream is a trivial handler loop.
- **Do nothing; approximate with `sleep` + buffered body.** Reproduces total-response-time growth but not inter-token latency, and cannot speak SSE/chunked at all — so it can mock neither streaming LLM endpoints nor the MCP SSE transport. The buffered approximation stays available and is the right tool for "slow but non-streaming upstream"; it just doesn't cover the cases in the Context.

**Implementation order:**

1. **Slice 1 — WIT + host dispatch + buffered-compat + tests.** Add `streams` + `http-response` interfaces and reshape the `handle` export in `wit/wiremirage.wit` and `compiler/js-engine/wit/`. Implement the writer in `wm-host`: `send` keeps today's path; `start` opens an axum streaming body and pumps the `byte-sink` (backpressure via the existing async wasmtime/tokio plumbing, cancellation via a disconnect signal → `closed`). Update `bindings_version` gating to reject @0.1.0 wasm uploads with an actionable error. js-engine shim: buffered adapter so existing TS/JS handlers are untouched. Tier-2: an SSE fixture that emits N chunks with `sleep` between them; assert the client sees framed events arrive over time, plus a client-disconnect test asserting `write` returns `closed`.
2. **Slice 2 — streaming budget + journal.** Host-config streaming duration / byte / chunk limits; suspend the epoch ticker during the streaming phase and enforce duration via timer. Journal: `streamed` flag, counts, head/tail sample, terminal disposition. Tier-2: budget-exceeded → `too-large`; over-duration → trap + journal disposition.
3. **Slice 3 — dry-run + surfaces.** `dry_run` returns the `(chunk, delay)` sequence + disposition across REST / wm-core / CLI / MCP. UI dry-run page renders the chunk timeline. Capabilities content (`crate::capabilities`) gains a `streaming` topic. Update the wiremirage skill with an SSE recipe and a "mock a streaming LLM endpoint" script; update `skill/wiremirage-debug` for the new journal disposition field.
4. **Deferred, explicitly out of scope here:** WebSocket (needs a per-connection handler lifetime + inbound `byte-source`), bidirectional gRPC (needs HTTP/2 enabled in axum + the WebSocket-shaped substrate), and streaming *request* bodies (large uploads). Each is its own ADR; this one only establishes the substrate they will reuse.

**See also:**

- [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) — `sleep` / `monotonic-ms` are the pacing prerequisite; it named streaming responses as this follow-up.
- [0002-wasm-sandbox.md](0002-wasm-sandbox.md) — the epoch/fuel/memory bounds that the streaming budget extends rather than replaces.
- [0003-component-model.md](0003-component-model.md) — why the contract is WIT-first; informs the "roll our own vs adopt wasi:http" call.
- [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md) — the js-engine shim that absorbs the export reshape so source-language handlers don't break.
- ../script-api-wit.md — canonical contract; the "Streaming request and response bodies" deferral resolved here, and the WIT must be updated in lockstep with `wit/wiremirage.wit`.
- ../route-model.md — response-shape and reserved-header rules that `response-writer.start` inherits.
- ../storage-model.md — journal record shape that gains the streaming summary fields.

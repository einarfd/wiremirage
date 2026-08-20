# Observing the host

Set `OTEL_EXPORTER_OTLP_ENDPOINT` and the host exports **both traces and
metrics** over OTLP/gRPC. Unset, it logs to stderr and nothing else. See
[configuration](configuration.md#observability) for the env vars.

Inbound W3C `traceparent` is extracted on every request and used as the
dispatch span's parent, so the host's spans chain under whatever upstream
traced the request.

## Metrics vs traces: which answers what

Metrics split by audience. The **mock-traffic** families (`wm.dispatch.*`,
`wm.handler.*`, `wm.streaming.*`) are keyed only by small enums plus HTTP
method and status — never by route, group, user, or trace ID, because a mock
server's route count is set by its users and would blow up a series budget.
The **control-plane** family (`http.server.*`) covers the internal API / UI /
auth surfaces, where the route set is bounded by code so `http.route` is safe
to label.

For **per-route** mock questions ("p95 latency or fuel for `/v1/charges`"),
query **traces**. The dispatch span carries `route.matched_pattern`,
`route.id`, `outcome`, `handler.fuel_consumed`, `handler.memory_peak_bytes`,
`handler.wall_ms`, and `streaming.head_latency_ms`. Trace backends are built
to slice by those high-cardinality dimensions; metrics aren't. The
at-a-glance "did it fire / when last / how many times" lives on the route
record itself (`hits_total`, `last_hit_at`) via the UI, CLI, or MCP.

Histogram aggregation is explicit-bucket, not exponential, and is consumed for
sum/count (rate and mean) — metric-side percentiles are not relied upon.

## Mock dispatch

- `wm.dispatch.duration_ms` (histogram, ms) — by `http.request.method`,
  `http.response.status_code`, `wm.dispatch.outcome` ∈ `ok` / `handler_error`
  / `unmatched_404` / `streaming`. **The latency signal.** For streaming
  dispatches this is time-to-head; the post-head clock is
  `wm.streaming.duration_ms`.
- `wm.dispatch.active_requests` (UpDownCounter) — by `http.request.method`.
  **Rising in step with `duration_ms` is the cascading-failure signature** —
  pin both on the same dashboard.
- `wm.dispatch.request_body_bytes` (histogram, bytes) — spot-check for
  oversized inputs the 10 MiB cap is letting through.

## Handler resources (wasm sandbox)

- `wm.handler.fuel_consumed` (histogram) — by `wm.dispatch.outcome`. **p99
  climbing toward the cap is the early warning** that handlers are running
  close to budget.
- `wm.handler.memory_peak_bytes` (histogram, bytes) — 64 MiB is the per-call
  cap ([ADR-0002](adr/0002-wasm-sandbox.md)).
- `wm.handler.wall_ms` (histogram, ms) — handler-only wall time, separate from
  dispatch overhead.
- `wm.handler.traps_total` (counter) — by `wm.trap.reason` ∈ `fuel` / `epoch`
  / `memory` / `other`. **Non-zero means the sandbox is firing — alert on
  this.**

## Streaming

- `wm.streaming.head_latency_ms` (histogram, ms) — dispatch start to head
  delivery.
- `wm.streaming.duration_ms` (histogram, ms) — by `wm.streaming.disposition`.
  Post-head pumping time.
- `wm.streaming.chunks_total` / `wm.streaming.bytes_total` (counters).
- `wm.streaming.terminations_total` (counter) — by `wm.streaming.disposition`
  ∈ `finished` / `client_disconnected` / `trapped`. **Rising
  `client_disconnected` means consumers are abandoning streams.**

## Control plane (API / UI / auth)

- `http.server.request.duration` (histogram, **seconds** — OTel HTTP semconv)
  — by `http.request.method`, `http.response.status_code`, `http.route` (the
  route *template*, e.g. `/api/groups/{group}`), and `wm.surface` ∈ `api` /
  `auth` / `ui`. Slice by `wm.surface` for an API-vs-UI breakdown, by
  `http.route` to find a slow endpoint.
- `http.server.active_requests` (UpDownCounter) — by `http.request.method`,
  `wm.surface`.
- `http.server.request.body.size` (histogram, bytes) — from `Content-Length`;
  absent for chunked bodies.

Control-plane requests also open an `http.server.request` span (method, route
template, surface, status), so a trace backend can answer "p95 for
`/api/groups/{group}`". Static assets get the metric but no span.

## MCP

- `wm.mcp.tool.calls_total` (counter) and `wm.mcp.tool.duration_ms`
  (histogram) — by `wm.mcp.tool` and outcome. The tool label is bounded by the
  router's registered names; anything unrecognised collapses to `unknown`, so
  the router itself is the cardinality allowlist. Each call also opens an
  `mcp.tool` span.

The `/health` and `/ready` probes are deliberately not recorded.

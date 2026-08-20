# ADR-0024: Metrics via OTLP — dispatch, handler resources, and streaming

**Status:** Accepted

**Amendments:**

- *2026-05-28 — MCP per-tool + control-plane request spans landed (two of the deferred follow-ups).* The slice-2 amendment listed MCP per-tool metrics and internal-HTTP instrumentation as deferred; both are now implemented (demand-confirmed by the operator). (1) **MCP per-tool**: a hand-written `call_tool` on the `ServerHandler` impl (rmcp's `#[tool_handler]` macro only generates one when absent) wraps the same `tool_router.call(tcc)` dispatch the macro would, adding an `mcp.tool` span and `wm.mcp.tool.calls_total{tool,outcome}` / `wm.mcp.tool.duration_ms{tool}` metrics. The tool label is bounded by `tool_router.has_route` — a registered name passes through, anything else collapses to `"unknown"`, so the router itself is the cardinality allowlist (no hardcoded tool list to drift). (2) **Control-plane request spans**: the `internal_http_metrics` middleware (which already emits the `http.server.*` metrics) now also opens an `http.server.request` span carrying method / route-template / `wm.surface` / status, so API/UI/auth traffic appears in traces with per-route latency — the arkiv-parity piece that lets a trace backend answer "p95 for `/__api/groups/{group}`", which the routeless aggregate metrics can't. Static-asset routes get the metric but no span (volume control, mirroring arkiv excluding its static traffic). Still deferred: auth audit events, exemplars, and a committed dashboard definition.

- *2026-05-28 — slice 2 landed, and pulled two "out of scope" items forward.* The original implementation order deferred internal control-plane HTTP metrics and reserved per-route resource detail for the product surface. Slice 2 implemented both, with one design refinement informed by the three-pillars (traces + metrics + logs) framing: **per-route resource detail is a trace concern, not a product-surface or metric concern.** Concretely — (1) control-plane metrics now exist as `http.server.*` (OTel HTTP semconv) keyed by `{method, status, http.route, wm.surface}`, recorded by a `route_layer` middleware on the api/auth/ui sub-routers; `http.route` is the matched *template* (bounded route set, operator-safe), the mock fallback is excluded, and the MCP streamable endpoint + `/__health` / `/__ready` probes are not recorded. (2) The dispatch span gained `handler.fuel_consumed` / `memory_peak_bytes` / `wall_ms` (buffered) and `streaming.head_latency_ms` (streaming) as attributes, so "p95 fuel for `/v1/charges`" is a span query against trace backends (which are built to slice high-cardinality dimensions) rather than a forbidden per-route metric label. This *narrows* the original "per-route detail lives on the route record" consequence: the route record keeps the at-a-glance counters (`hits_total`, `last_hit_at`), but distributional per-route questions are answered by traces. The speculative product-UI enrichment (`hits_by_outcome`, `recent_p95_ms`, etc.) is consequently dropped from the roadmap unless a concrete UI workflow demands it.

- *2026-05-28 — histograms are explicit-bucket, not exponential (corrects the original Decision text).* The Decision claimed "histograms are exponential." The implementation used the SDK-default explicit-bucket aggregation, and on review that is the right call to keep — not a bug to fix toward exponential — for a concrete backend reason: Logfire stores `sum`/`count` columns only for explicit-bucket histograms (`metric_type='histogram'`); exponential histograms land in bucket-array columns with **no `sum` and no total `count`**. The control-plane dashboard panels compute request rate and mean latency from `histogram_count` / `histogram_sum`, so exponential would break them with no clean rebuild. Exponential's upside — high-fidelity percentiles over a wide range — isn't consumed here: mock percentiles come from the `dispatch` span, and the control plane is mean-by-design (no internal-handler spans yet). Accepted consequence: with default bucket bounds the histogram *distributions* are not meaningful (our values span seconds / ms / fuel-ticks / bytes, none matching the default `0…10000` boundaries) — they're useful for `sum`/`count` aggregates only. If aggregate fuel/memory *distribution from the metric* ever becomes a real need, the targeted fix is per-instrument custom bounds (or selectively exponential for just those instruments, keeping the latency histograms explicit so the dashboard's mean/rate math stays intact), not a blanket switch.

**Context:**

[0017-observability-tracing.md](0017-observability-tracing.md) established the host's tracing and log story (always-on stderr JSON; opt-in OTLP/gRPC spans via `OTEL_EXPORTER_OTLP_ENDPOINT`; W3C `traceparent` propagation in both directions) and explicitly deferred metrics: *"Spans + propagation are the higher-value first cut for agent debugging and SRE visibility; metrics … add SDK surface and configuration complexity without immediate justification. We'll revisit when we feel the lack."* This is that revisit, with two forcing functions:

1. **Latency-ramp use cases are now first-class.** [0022-streaming-http-responses.md](0022-streaming-http-responses.md) makes WireMirage capable of mocking long-lived streaming LLM APIs (Vertex `streamGenerateContent`, OpenAI `chat/completions`, MCP streamable-HTTP transports). The motivating workload — reproducing cascading-failure modes where p50/p95 dispatch latency climbs while in-flight request count grows — is *fundamentally* a metrics question. Spans tell you what one request did; they don't show "median latency rose from 200 ms to 5 s over 30 s while concurrent requests climbed from 12 to 48," which is what an operator (or an agent running an experiment) actually needs to see during an incident.

2. **Wasm sandbox limits are now real.** [0002-wasm-sandbox.md](0002-wasm-sandbox.md)'s fuel / epoch / memory caps fire in production today (slice 46 wired the limiter; the journal captures `fuel_consumed`, `memory_peak_bytes`, `wall_clock_ms` per request). These are *the* signals for "handlers are approaching their budget" — exactly the kind of distribution-over-time the journal cannot summarize and spans cannot aggregate. Without histograms an operator can only ask "did one specific request trip a limit?", not "what fraction of handlers are within 20 % of fuel exhaustion right now?"

The existing tracing pipeline is the natural home for metrics: `opentelemetry-otlp` already pulls in the gRPC/tonic dependency cluster, the OTel SDK ships matching `MeterProvider` + OTLP metrics exporter, and Logfire / Tempo / Honeycomb / Datadog all ingest OTLP metrics over the same connection as traces. Adding metrics here costs no new transport and reuses the operator's existing collector.

**Two audiences, two surfaces.** WireMirage has two distinct observability audiences and the metrics story has to respect that:

- The **operator** of the host (deployer / SRE / the agent driving a latency-ramp experiment) cares about aggregate behavior over time — rate, errors, latency distribution, sandbox-limit headroom. Their tools are OTel-shaped: traces in a tracing backend, metrics in a TSDB, alerts off both. This is what's missing today.
- The **user** of the mock — the developer or agent who registered a route — cares about *their own* route's behavior: did it fire, how often, when last, what status. Their tools are the product surfaces: the `wm` CLI, the web UI, the MCP server. This is already partly built — slice 17 added `hits_total` and `last_hit_at` per route plus `last_activity_at` per group, surfaced via REST, CLI, MCP, and the UI's list pages.

The mistake to avoid is conflating these: pushing per-route counters into OTel as labeled metric series, or pushing aggregate operator-facing distributions into the product surface where users have no use for them. Each audience gets the signals it actually consumes, on the tools it actually uses.

This separation also resolves a cardinality concern that would otherwise bite hard. A mock server is *expected* to host many routes — that's the product. The user-facing route count is a design parameter, not pathological. Pinning per-route counters onto OTel histograms would multiply every dispatch metric by the route count, which for a multi-tenant deployment runs straight into the series budget of any backend. Per-route detail in the product surface (already keyed by route, slice 17) bypasses the problem entirely: storage scales naturally with route count and users only look at their own routes.

**Decision:**

Add an OTLP/gRPC metrics pipeline alongside the existing traces pipeline, sharing endpoint configuration and lifecycle, exporting a focused initial catalog covering dispatch, handler resources, and streaming.

Concretely:

- **One env-var family, mirroring traces.** When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, the host exports *both* traces and metrics through it. Unset → stderr-only, no metrics export (same posture as traces — fail-fast, no localhost fallback). The standard SDK env vars `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (split-endpoint override) and `OTEL_METRIC_EXPORT_INTERVAL` (default 60 s) are honored by the SDK; we add no WireMirage-specific knobs. **No new env var introduced for the metrics-on/off choice** — coupling traces and metrics to a single endpoint keeps the operator surface minimal and matches the OTLP collector deployment shape.

- **Lifecycle mirrors the `TelemetryGuard`.** The `MeterProvider` is created in the same `telemetry::init()` path as the `TracerProvider`, held on the existing `TelemetryGuard`, and explicitly drained on shutdown so the last metric batch flushes before the process exits.

- **Metric naming uses a `wm.*` namespace, not OTel HTTP semconv.** Semconv (`http.server.request.duration`, etc.) is the obvious-looking choice, but it presupposes a bounded `http.route` label that mock-server workloads don't have. We could keep semconv names and drop `http.route` — but that produces a semconv-named metric without the labels backends expect, which is more misleading than honest custom naming. `wm.dispatch.*` is honest: this is mock dispatch, the labels are what they are, and a downstream dashboard or alert template will be written against these names deliberately rather than auto-derived from a semconv pattern that doesn't quite fit. Internal HTTP traffic (`/__api/*`, `/__ui/*`, `/__auth/*`) is bounded-route and *would* fit semconv cleanly, but it's deferred to a separate slice (see "out of scope" below).

- **Initial catalog** (full list; everything not on this list is deferred):

  *Mock dispatch (`wm.dispatch.*`):*
  - `wm.dispatch.duration_ms` (histogram, **ms**) — dispatch wall time, *including* match overhead, body-buffering, handler invocation, and response build. Attributes: `http.request.method`, `http.response.status_code`, `wm.dispatch.outcome` (custom enum, see below). **No `http.route` label** — per-route detail belongs to the product surface (see below).
  - `wm.dispatch.active_requests` (UpDownCounter) — concurrent mock dispatches in flight. Attribute: `http.request.method`.
  - `wm.dispatch.request_body_bytes` (histogram, **bytes**) — payload size after buffering. Attribute: `http.request.method`.

  *Handler resources (`wm.*`):*
  - `wm.handler.fuel_consumed` (histogram, units) — wasmtime fuel ticks per handler invocation. Attribute: `wm.dispatch.outcome` ∈ `ok`/`handler_error`.
  - `wm.handler.memory_peak_bytes` (histogram, bytes) — peak linear memory observed per call (via `HandlerLimits`).
  - `wm.handler.wall_ms` (histogram, ms) — handler-only wall time (excludes dispatch overhead, distinct from `http.server.request.duration`).
  - `wm.handler.traps_total` (counter) — wasmtime traps. Attribute: `wm.trap.reason` ∈ `fuel`/`epoch`/`memory`/`other`. (The signal that says "limits are firing.")

  *Streaming (`wm.streaming.*`, only recorded on streaming dispatches):*
  - `wm.streaming.head_latency_ms` (histogram, ms) — time-to-first-byte: dispatch start → head delivery.
  - `wm.streaming.duration_ms` (histogram, ms) — head delivery → stream completion (or termination).
  - `wm.streaming.chunks_total` (counter, summed at stream end).
  - `wm.streaming.bytes_total` (counter, summed at stream end).
  - `wm.streaming.terminations_total` (counter) — every stream emits exactly one termination event. Attribute: `wm.streaming.disposition` ∈ `finished`/`client_disconnect`/`budget_exceeded`/`handler_trap`.

- **Cardinality rules** (encoded as a per-metric attribute allowlist in the implementation, not just convention):
  - **Allowed** as labels: bounded enums (`wm.dispatch.outcome`, `wm.streaming.disposition`, `wm.trap.reason`), HTTP method (~10), HTTP status (~30).
  - **Forbidden** as labels — *anything route-specific or per-actor*: `http.route`, `route.id`, `route.matched_pattern`, `group`, `group_id`, `trace_id`, `user_id`, `owner_id`, raw URL `path`, query string, request/response body content, header values.
  - The per-route detail an operator could in principle want (which routes are hot, which are slow) is the **product audience**'s data and lives on the route record (slice 17's `hits_total` / `last_hit_at`), surfaced through the CLI, UI, and MCP. The ADR's stance is that we do not duplicate it into OTel; if a deployment grows a real operator-grade per-route need, the answer is to revisit *that* surface (e.g. add per-route p95 to the route record), not to expand OTel labels into the unbounded space.

- **`wm.dispatch.outcome` enum (custom attribute)** matches the existing dispatch-span attribute exactly: `ok` / `handler_error` / `unmatched` / `reserved_path` / `body_too_large` / `root_redirect` / `streaming`. Single enum across spans and metrics so an operator drilling from a metric dimension to traces gets a clean filter.

- **Histograms use the SDK-default explicit-bucket aggregation** (superseding this ADR's original "exponential" intent — see the 2026-05-28 amendment for why). They are consumed for their `sum`/`count` only (rate and mean); metric-side percentiles are *not* relied upon — mock-dispatch percentiles come from the `dispatch` span, and the control plane is mean-by-design. The default bucket *boundaries* don't resolve our actual value ranges (seconds, ms, fuel ticks, bytes all differ), so the bucket distribution is not meaningful; tuning it is deferred until a consumer needs metric-side distributions.

- **Recording sites are surgical, not pervasive.** Dispatch metrics record at the end of `dispatch_inner` (one place, all outcomes flow through it). Active-requests in/out wraps the same scope. Handler-resource metrics record where `ResourceUsage` is already captured for the journal (zero new measurement plumbing). Streaming metrics record at head-emit and at `StreamSummary` finalization (where the journal write already happens). No new per-request data is collected — we're aggregating data that already flows.

- **Operator docs.** README gains an "Observing the host" section: which metrics to watch for which symptom (rising p95 + rising `active_requests` → backpressure; high `handler.fuel_consumed` near the cap → fuel exhaustion; rising `streaming.terminations_total{disposition=client_disconnect}` → consumer-side cancellation; non-zero `handler.traps_total` → sandbox limit firing). One short paragraph each, not a dashboard book.

**Consequences:**

- **Direct support for the original streaming-LLM motivation.** Operators (and the agents running latency-ramp experiments against the mock) get the standard SRE primitives — rate, errors, duration, in-flight — to characterize cascading-failure shapes that prompted [0022-streaming-http-responses.md](0022-streaming-http-responses.md) in the first place. Spans tell the per-request story; metrics tell the over-time story.

- **Visibility into the sandbox.** Fuel / memory / epoch budgets stop being "hope the journal catches it" and become continuously observable distributions. An operator can see "p99 fuel-consumed is at 80 % of the cap" before any handler actually traps.

- **No new exporter, no new env var.** Reuses the [0017-observability-tracing.md](0017-observability-tracing.md) OTLP/gRPC pipeline. Deployments that wired up traces get metrics for free; deployments that didn't get neither (stderr-only stays stderr-only).

- **Trace-metric correlation by `wm.dispatch.outcome`.** A spike in `http.server.request.duration{wm.dispatch.outcome="handler_error"}` filters cleanly to the matching span population using the same enum value. No exemplars in this slice — the SDK supports them; we add them only if a real drill-down workflow needs them.

- **Cost: real-but-bounded dependency growth.** `opentelemetry-otlp`'s `metrics` feature pulls in the metrics SDK + exponential-histogram aggregation code. Build-time only; the runtime cost when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset is the few `OnceLock` instrument handles and arithmetic on `record()` calls into a `NoopMeter` — measurable in nanoseconds per request.

- **Cost: another opinionated set of metric names to maintain.** Renaming a metric is a breaking change for downstream dashboards/alerts. We accept that by anchoring to semconv where it exists (the semconv group owns those names) and being deliberate about `wm.*` naming.

- **Audience split is encoded in the design, not just docs.** Aggregate operator-facing distributions live in OTel with cardinality bounded by small enums × HTTP method × status. Per-route detail lives on the route record (slice 17) and surfaces through the product UIs (CLI, UI, MCP). The two never mix, so adding routes is free for OTel and rich for users.

- **Loss: no per-route slicing in the operator's TSDB.** An operator wanting to ask "which route was slow" can't answer it from metrics alone — they correlate via the dispatch span (`route.matched_pattern` is on the span) or the product surface (per-route `hits_total` / `last_hit_at`). Trace-to-product navigation by `trace_id` already works (slice 17 onwards). We accept this tradeoff because the alternative is per-route cardinality unbounded by operator policy in a system whose product *is* hosting many routes.

- **Reversible.** A future "drop OTLP metrics in favor of \[Prometheus / vendor SDK / nothing]" reverses cleanly: the recording sites are surgical, the instruments are read once from the `MeterProvider`, and removing them is a deletion-only change.

**Alternatives considered:**

- **Prometheus HTTP scrape exporter (`metrics-exporter-prometheus` or `opentelemetry-prometheus`).** Tempting because Prometheus is the de-facto self-hosted standard and the host already serves HTTP. Rejected: it forks the observability story into two transports (OTLP for traces, scrape for metrics) and forces operators with an existing OTLP collector to either run both pipelines or set up a Prometheus-to-OTLP bridge. Operators who want Prometheus today can run an OTel Collector with the `prometheusremotewrite`/`prometheus` exporter — the standard pattern. We don't earn anything by skipping that hop.

- **Keep metrics-as-spans (rely on tracing backends' span-derived metrics).** Honeycomb, Datadog, and others can derive rate/duration metrics from spans. Rejected: it requires backend-specific configuration (per-attribute breakdowns, sampling-aware derivations), depends on the operator's backend choice, and *fundamentally cannot capture handler-resource histograms* (fuel/memory/wall) without bolting per-request attributes onto every span — pushing span cardinality up and re-creating the metrics SDK from the wrong end.

- **Custom JSON metrics on a `/__metrics` HTTP endpoint.** A "no SDK needed" approach where the host serializes counters/histograms to a small JSON document. Rejected for the same reason 0017 rejected custom JSON-spans: nobody else parses our format, operators end up writing custom scrapers, and the moment a feature like exponential histograms or exemplars matters we're reinventing OTel badly.

- **OTel Logs SDK for metrics-as-events.** Counterproductive — the metrics SDK is the mature path, and conflating signal types pushes work onto every downstream backend.

- **Wait for adopter demand before adding metrics.** The 0017 deferral position. Rejected now because (a) the streaming work in 0022 makes the demand concrete *for the project author's own use case*, not just speculative adopter demand, and (b) handler-resource limits in 0002 fire in production today and the journal alone cannot surface "what fraction of handlers are near the cap."

- **Include `http.route` (matched pattern) on mock-dispatch metrics.** The semconv-aligned choice and what the v1 draft of this ADR proposed. Rejected after re-framing: WireMirage's product *is* routes, and the route count is set by users, not bounded by host design. Pinning route as a metric label puts the series budget of every operator's TSDB at the mercy of the host's user population, which is the wrong direction of coupling. Per-route observation belongs to the per-route product surface (slice 17 onward), not to OTel; cross-referencing remains available via the dispatch span's `route.matched_pattern` attribute.

- **Use OTel HTTP semconv names (`http.server.request.duration` etc.) even without `http.route`.** Tempting because backends auto-recognize them. Rejected: a semconv-named histogram without the conventional labels is misleading — auto-derived dashboards built for HTTP servers assume route-level breakdowns, and dropping them silently degrades those defaults rather than failing them. Honest custom naming (`wm.dispatch.*`) signals "treat this as wm-specific" to anyone reading the metric, and a deliberately-built dashboard is no harder to write against `wm.dispatch.duration_ms` than against `http.server.request.duration`. Internal HTTP traffic (a separate slice) *would* fit semconv cleanly and will use those names if/when instrumented.

- **Per-route detailed metrics opt-out (`WM_METRICS_ROUTE_LABEL=off`).** A middle ground keeping `http.route` on by default with an opt-out. Rejected: it shifts the cardinality-blowup risk onto every operator's first-deploy moment, before they've tuned anything. The cleaner default is "operator metrics are coarse-grained and bounded; per-route is in the product surface where it can scale naturally with the route count."

- **Exemplars on every histogram observation.** OTel-native trace-metric correlation. Deferred — the basic `wm.dispatch.outcome` label gives clean span-population filtering, and exemplars add per-observation `trace_id`/`span_id` plumbing we'll lift only when a drill-down workflow actually needs to follow them.

**Implementation order:**

1. **Slice 1 — mock-dispatch metrics pipeline + handler-resource + streaming.** Extend `telemetry.rs` with a `MeterProvider` alongside the `TracerProvider`, sharing the OTLP gRPC channel and `TelemetryGuard`. Add a `metrics.rs` module that constructs all instruments (histograms, counter, UpDownCounter) once from the meter and exposes typed `record_*` helpers. Wire the recording sites in `server.rs::dispatch_inner` (one site for `wm.dispatch.*` + handler-resource at outcome time), wrap the mock-dispatch scope with the active-requests UpDownCounter, and add the streaming recording sites at head-emit and `StreamSummary` finalization. **Mock traffic only — `/__api/*`, `/__auth/*`, `/__ui/*`, `/__api/mcp` do not record into these instruments.** README operator section ("Observing the host" — what each metric is for, what symptom each detects). Tier-2 test that an OTLP-configured host emits the expected metric families and respects the cardinality allowlist (i.e., no route-shaped labels ever leak in).

2. **Out of scope for slice 1 (each its own follow-up if/when it earns its keep):**
   - **Internal HTTP metrics for `/__api/*`, `/__auth/*`, `/__ui/*`, `/__api/mcp`.** Bounded-route, semconv-aligned (`http.server.request.duration` with `http.route`), different audience (operator drilling into control-plane health). Worth doing once the mock-dispatch metrics have shaken out and we know what to instrument at the control-plane boundary.
   - **Auth audit events.** Successful login / logout / token-use as structured `tracing::info!` events. Pair with the obs audit's "auth events are silent on success" finding. One slice; small.
   - **MCP per-tool spans + per-tool metrics.** `#[tracing::instrument]` on every tool handler + `wm.mcp.tool.duration_ms` by `tool` (~25 tools, bounded). One slice covering both signals; the spans address the obs audit's "MCP tool invocations not individually spanned" gap.
   - **Per-route product-surface metrics.** If the per-route detail in slice 17 (`hits_total`, `last_hit_at`) turns out insufficient for user-side observation, the right place to extend is the route record itself — e.g. `hits_by_outcome`, a recent-window p95, recent error count. Not OTel labels.
   - **Exemplars.** Per-observation `trace_id`/`span_id` plumbing to enable backend drill-down from metric data point to representative trace. Lift when a workflow asks for it.
   - **Sample Grafana / Logfire dashboard JSON.** Once we've actually dogfooded the metrics during a latency-ramp run, the dashboard shape will be obvious; writing it ahead is speculative.

**See also:**

- [0017-observability-tracing.md](0017-observability-tracing.md) — the foundation; this ADR extends 0017's deferred-metrics decision.
- [0022-streaming-http-responses.md](0022-streaming-http-responses.md) — the streaming workload that makes time-distribution observation a first-class need.
- [0002-wasm-sandbox.md](0002-wasm-sandbox.md) — the fuel / epoch / memory caps whose distribution this slice exposes.
- ../architecture-overview.md, ../storage-model.md — the journal (per-request, product surface) remains the agent-debugging signal; OTel metrics (aggregate, ops surface) are deliberately a separate audience, see 0017's "Implementation notes."

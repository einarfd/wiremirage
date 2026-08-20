# ADR-0017: Host observability via tracing + opt-in OTLP

**Status:** Accepted

**Amendments:**

- *2026-05-23 — interpreted-language path no longer uses a sidecar.* Per [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md), TS / JS source-language routes dispatch through an embedded `js-engine.wasm` (and TS goes through pure-Rust `swc` in the host process at create time). The `CompilerClient::compile` span, the "outbound `traceparent` injection is forward-compatible" rationale, and the "instrumenting the Node compiler sidecar" alternative below apply only to AOT-language compiler sidecars (TinyGo, Rust, Zig) when those land. In place of the slice's `compiler.compile` outbound span, the host carries a `swc.transpile` in-process span on TS create / patch; the WIT-bindgen dispatch into the shared engine is covered by the existing `wasmtime.instantiate` and `wasmtime.call_handle` spans. Trace-context propagation across HTTP is unchanged for the AOT case.

**Context:** ../storage-model.md's "Logs and trace IDs" section already commits WireMirage to W3C Trace Context propagation and structured stderr logs, but it doesn't say *how* the host implements them — what telemetry library, what export protocol, what the experience is for operators who don't run a collector. As we approached implementation we needed concrete answers, with three constraints in mind:

- **Backend agnosticism.** WireMirage is intended for open-source release. Tying observability to a specific vendor (Datadog, New Relic) or even a specific open backend (Tempo, Honeycomb) would push that cost onto every adopter.
- **Local-development experience.** The default `docker compose up` story should not require an OpenTelemetry collector, a Loki instance, or any other observability stack. People should be able to run the host and read its logs on stderr.
- **Fail-fast on missing config.** Earlier ADRs and the project's stated convention are clear: when configuration is required, the host fails loudly rather than silently falling back. Observability should follow the same rule — no quiet localhost-4317 connect attempts that look healthy until they aren't.

OpenTelemetry was the obvious candidate to evaluate. It's the de facto standard for backend-agnostic instrumentation, has a mature Rust SDK, integrates cleanly with the existing `tracing` crate via `tracing-opentelemetry`, and a shared OTLP collector pipeline already exists in the broader infrastructure WireMirage will deploy alongside (notably alongside Arkiv).

**Decision:** Use Rust's `tracing` crate as the always-on diagnostic surface, layered with an opt-in OpenTelemetry exporter:

- **Structured stderr is always on.** `tracing_subscriber::fmt::layer().json()` formats every `tracing::info!` / `warn!` / `error!` record as one JSON line on stderr. Standard log shippers (Loki, Vector, Fluent Bit, Datadog agent) consume this without further configuration.
- **OTLP/gRPC span export is opt-in via `OTEL_EXPORTER_OTLP_ENDPOINT`.** When the env var is set and non-empty, the host adds `tracing-opentelemetry`'s layer wrapping an `opentelemetry-otlp` (gRPC/tonic) span exporter. When unset, the host stays stderr-only — *no localhost:4317 fallback*. This matches the project's fail-fast posture: a deployment that intends to ship spans says so explicitly.
- **Resource attributes** default to `service.name=wm-host` and `service.version=$CARGO_PKG_VERSION`. The standard `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` env vars are honored by the SDK, so operators can override or extend without per-app config.
- **Span surface** keeps cardinality finite. The `dispatch` span carries `http.method`, `route.matched_pattern`, `route.id`, and `outcome` — *not* the raw URL `path`, since path-param values would multiply unique attribute values per route. Child spans cover `wasmtime.instantiate` and `wasmtime.call_handle`. Light spans wrap `Auth::authenticate`, `Registry::create_route` / `delete_route`, and `CompilerClient::compile`.
- **W3C `traceparent` propagation in both directions.** Inbound headers are extracted via a `HeaderExtractor` adapter and applied as the dispatch span's parent so the host's spans chain under whatever upstream traced the request. Outbound calls to the compiler sidecar inject `traceparent` via a `HeaderInjector` adapter, so a future-instrumented sidecar's spans will chain under ours without further coordination.
- **Logs ride spans automatically.** `tracing-opentelemetry` turns `tracing::info!` records emitted inside an active span into OTel span events; backends that support trace-log correlation (Tempo, Honeycomb, Datadog APM) show them attached to the trace. Records emitted outside any span (startup, shutdown, bootstrap warnings) stay stderr-only — that's appropriate, since they're host-lifecycle, not per-request.
- **Graceful shutdown drains the exporter.** The host holds a `TelemetryGuard` for the process lifetime; SIGTERM/Ctrl-C triggers `axum::serve::with_graceful_shutdown`, which finishes in-flight requests, then `main` calls the guard's explicit shutdown so the OTLP batch flushes before the process exits.

**Consequences:**

- **No telemetry stack required for local dev.** Stderr-always means `docker compose up` and `cargo run` produce useful, structured output without any additional services. The `OTEL_EXPORTER_OTLP_ENDPOINT` knob is purely additive.
- **Fail-fast posture preserved.** A misconfigured deployment doesn't silently retry localhost; it either points at a collector that exists or doesn't try at all. Operators see clearly-labeled stderr output stating which mode they're in.
- **Trace-log correlation works without extra wiring.** Operators don't have to plumb a correlation ID through `tracing` calls — the existing `tracing::error!(error = %e, ...)` patterns automatically appear as span events on whichever request span was active.
- **Cardinality stays manageable.** Restricting span attributes to bounded values (method, matched pattern, route ULID) keeps the per-trace cost finite. Backends with attribute-based indexing (Honeycomb, Datadog) won't blow up on path-param explosion.
- **Outbound `traceparent` injection is forward-compatible.** When the compiler sidecar gains its own instrumentation, no host change is needed; the sidecar's spans will simply start appearing as children of `compiler.compile`.
- **Cost: an `opentelemetry`/`opentelemetry-otlp`/`tonic` dependency cluster.** This pulls in gRPC machinery (tonic, prost) even when the exporter is disabled. The build cost is real but acceptable given the value, and the runtime cost when disabled is just the unused code paths.
- **Cost: gRPC-only initially.** OTLP/HTTP-protobuf is supported by the SDK behind a different feature; we ship gRPC because the in-house collector accepts it. If an OSS adopter needs HTTP, that's a feature flag flip, not an architectural change.
- **Cost: span storage in the backend is per-deployment.** Adopters running the host with OTLP enabled need a collector + backend they're paying for (or self-hosting). The default-off design keeps that cost opt-in rather than forced.

**Alternatives considered:**

- **OTLP always-on with localhost fallback.** Rejected because it breaks fail-fast: a developer with no collector running gets background reconnect storms and confusing log lines, and the "is the exporter actually running?" question becomes impossible to answer from outside. Default-off is unambiguous.
- **HTTP/protobuf instead of gRPC for the exporter.** Deferred. Gives a slightly smaller dependency surface and dodges some corporate-network HTTP/2 issues, but our deployment uses gRPC and the gRPC exporter is the default-stable Rust path. Easy to revisit if an adopter has a HTTP-only constraint.
- **Custom JSON-spans-on-stderr only, no OTLP.** Rejected. Anyone running the host alongside other services would want to consolidate observability in a single backend; emitting our own span format would force adopters to write a custom parser. OTLP is the lingua franca; lean on it.
- **Auto-instrumented metrics in this slice.** Deferred. Spans + propagation are the higher-value first cut for agent debugging and SRE visibility; metrics (request counts, duration histograms, fuel consumed) add SDK surface and configuration complexity without immediate justification. We'll revisit when we feel the lack.
- **OpenTelemetry logs export.** Deferred. The Rust OTel logs SDK is younger than traces and metrics, and the stderr-to-aggregator path is mature and works without it. Trace-attached log events (via `tracing-opentelemetry`) cover the highest-value "what happened during this request" case. Revisit if adopters want a single OTel pipeline for all signals.
- **Instrumenting the Node compiler sidecar in the same slice.** Out of scope. The sidecar is two endpoints (`/compile`, `/health`); its latency and error surface are dominated by the actual compile work, which is captured by the host's `compiler.compile` span. Outbound `traceparent` injection means a future instrumented sidecar will chain cleanly without further host changes.

**Implementation notes:**

The per-request *journal* (in Valkey, agent-debugging surface, see ../storage-model.md "What the journal stores per request") and OTel observability are intentionally separate. The journal targets agents and humans debugging mocked traffic via the `wm` CLI / MCP; OTel targets operators monitoring host health. They're complementary signals with different audiences and storage. Conflating them — for instance, exporting journal records as OTel logs — would force adopters to operate the same backend for both purposes and would push journal retention semantics (which are product-driven, with TTLs in minutes-to-days) into ops-side cost models. Keep them separate.

The propagation adapters (`HeaderExtractor`, `HeaderInjector`) are tiny `http::HeaderMap` shims that bridge the OTel `Extractor` / `Injector` traits to axum's request headers and reqwest's outbound headers. They live alongside `init()` in the host's `telemetry` module so anything in the codebase reaching for HTTP propagation has one place to find them.

See also: ../storage-model.md, ../architecture-overview.md, [0001-rust-host.md](0001-rust-host.md), [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md).

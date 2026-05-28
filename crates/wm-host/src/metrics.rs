//! ADR-0024 metrics — operator-facing OTLP metrics for mock dispatch,
//! handler resources, and streaming.
//!
//! Instruments are constructed once from the global `MeterProvider` at
//! first call to [`metrics`]. When `OTEL_EXPORTER_OTLP_ENDPOINT` is unset
//! the global provider is a `NoopMeterProvider`, so the `record_*`
//! helpers compile down to indirection through the no-op meter — no
//! measurable overhead per request.
//!
//! Cardinality is policed at the call sites: every helper here takes
//! only the attributes the ADR allows (small enums, HTTP method, status)
//! and forbids the route-shaped ones (`http.route`, `route.id`,
//! `route.matched_pattern`, `group`, `user_id`). Adding a new label here
//! is the design check — if you reach for a per-request identifier,
//! you're putting per-route data into the operator surface; that
//! belongs on the route record / product UI instead. See ADR-0024.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};

const METER_NAME: &str = "wm-host";

static METRICS: OnceLock<Metrics> = OnceLock::new();

struct Metrics {
    // Mock dispatch — keyed by {method, status, outcome}.
    dispatch_duration_ms: Histogram<u64>,
    dispatch_active: UpDownCounter<i64>,
    dispatch_request_body_bytes: Histogram<u64>,

    // Handler resources — keyed by {outcome}.
    handler_fuel_consumed: Histogram<u64>,
    handler_memory_peak_bytes: Histogram<u64>,
    handler_wall_ms: Histogram<u64>,
    handler_traps_total: Counter<u64>,

    // Streaming — terminations keyed by {disposition}; others unlabeled
    // (or labeled inside the helper).
    streaming_head_latency_ms: Histogram<u64>,
    streaming_duration_ms: Histogram<u64>,
    streaming_chunks_total: Counter<u64>,
    streaming_bytes_total: Counter<u64>,
    streaming_terminations_total: Counter<u64>,

    // MCP per-tool (ADR-0024 follow-up). Keyed by {tool, outcome} — the
    // tool name is bounded to the known tool set (+ "unknown" for an
    // unrecognized request), so it's an operator-safe label.
    mcp_tool_calls_total: Counter<u64>,
    mcp_tool_duration_ms: Histogram<u64>,

    // Internal control-plane HTTP (ADR-0024 slice 2). OTel HTTP-server
    // semconv names — the internal surface has a bounded route set
    // (~60 templates), so `http.route` is a safe label here, unlike the
    // mock surface. Keyed by {method, status, http.route, wm.surface}.
    // `http.server.request.duration` is in SECONDS per semconv (the
    // `wm.dispatch.duration_ms` mock metric stays ms — different
    // namespace, different convention).
    internal_request_duration_s: Histogram<f64>,
    internal_active_requests: UpDownCounter<i64>,
    internal_request_body_bytes: Histogram<u64>,
}

fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let meter = global::meter(METER_NAME);
        // Histograms use the SDK-default explicit-bucket aggregation (NOT
        // exponential — ADR-0024's original text said exponential; see its
        // 2026-05-28 amendment for why explicit-bucket is the deliberate
        // keep). They're consumed for `sum`/`count` only — rate and mean —
        // because Logfire stores no `sum`/`count` for exponential
        // histograms, and the dashboard's control-plane panels need them.
        // Metric-side percentiles are NOT relied upon (mock percentiles
        // come from the `dispatch` span; the control plane is mean-by-
        // design). The default bucket bounds don't resolve our value ranges
        // (seconds / ms / fuel ticks / bytes), so the distributions aren't
        // meaningful — per-instrument bounds are a deferred change for if a
        // consumer ever needs metric-side distributions.
        Metrics {
            dispatch_duration_ms: meter
                .u64_histogram("wm.dispatch.duration_ms")
                .with_unit("ms")
                .with_description("Mock dispatch wall time, from match through response build.")
                .build(),
            dispatch_active: meter
                .i64_up_down_counter("wm.dispatch.active_requests")
                .with_unit("{request}")
                .with_description("Concurrent mock dispatches in flight.")
                .build(),
            dispatch_request_body_bytes: meter
                .u64_histogram("wm.dispatch.request_body_bytes")
                .with_unit("By")
                .with_description("Mock dispatch request body size after buffering.")
                .build(),
            handler_fuel_consumed: meter
                .u64_histogram("wm.handler.fuel_consumed")
                .with_unit("{tick}")
                .with_description("Wasmtime fuel ticks consumed per handler invocation.")
                .build(),
            handler_memory_peak_bytes: meter
                .u64_histogram("wm.handler.memory_peak_bytes")
                .with_unit("By")
                .with_description("Peak linear memory observed per handler invocation.")
                .build(),
            handler_wall_ms: meter
                .u64_histogram("wm.handler.wall_ms")
                .with_unit("ms")
                .with_description("Handler-only wall time (excludes dispatch overhead).")
                .build(),
            handler_traps_total: meter
                .u64_counter("wm.handler.traps_total")
                .with_unit("{trap}")
                .with_description("Wasmtime traps by reason (fuel/epoch/memory/other).")
                .build(),
            streaming_head_latency_ms: meter
                .u64_histogram("wm.streaming.head_latency_ms")
                .with_unit("ms")
                .with_description("Time to first byte: dispatch start → response head delivery.")
                .build(),
            streaming_duration_ms: meter
                .u64_histogram("wm.streaming.duration_ms")
                .with_unit("ms")
                .with_description("Streaming duration: head delivery → stream completion.")
                .build(),
            streaming_chunks_total: meter
                .u64_counter("wm.streaming.chunks_total")
                .with_unit("{chunk}")
                .with_description("Total streamed chunks (summed at stream end).")
                .build(),
            streaming_bytes_total: meter
                .u64_counter("wm.streaming.bytes_total")
                .with_unit("By")
                .with_description("Total streamed bytes (summed at stream end).")
                .build(),
            streaming_terminations_total: meter
                .u64_counter("wm.streaming.terminations_total")
                .with_unit("{stream}")
                .with_description("Streams completed, by disposition.")
                .build(),
            mcp_tool_calls_total: meter
                .u64_counter("wm.mcp.tool.calls_total")
                .with_unit("{call}")
                .with_description("MCP tool invocations, by tool + outcome.")
                .build(),
            mcp_tool_duration_ms: meter
                .u64_histogram("wm.mcp.tool.duration_ms")
                .with_unit("ms")
                .with_description("MCP tool invocation wall time, by tool.")
                .build(),
            internal_request_duration_s: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description(
                    "Control-plane (API/UI/auth) HTTP request duration, by surface + route.",
                )
                .build(),
            internal_active_requests: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_unit("{request}")
                .with_description("Concurrent control-plane HTTP requests in flight.")
                .build(),
            internal_request_body_bytes: meter
                .u64_histogram("http.server.request.body.size")
                .with_unit("By")
                .with_description("Control-plane HTTP request body size (from Content-Length).")
                .build(),
        }
    })
}

/// Record a mock-dispatch completion. Call exactly once per mock
/// request, after the outcome is known. `duration_ms` is end-to-end
/// dispatch time (or head-emit time for streaming dispatches).
pub fn record_dispatch(method: &str, status: u16, outcome: &str, duration_ms: u64) {
    let attrs = [
        KeyValue::new("http.request.method", method.to_owned()),
        KeyValue::new("http.response.status_code", status as i64),
        KeyValue::new("wm.dispatch.outcome", outcome.to_owned()),
    ];
    metrics().dispatch_duration_ms.record(duration_ms, &attrs);
}

/// Record the buffered request body size for a mock dispatch. Skipped
/// on outcomes where we don't know the path is mock yet (body_too_large
/// happens before path inspection).
pub fn record_request_body_bytes(method: &str, bytes: u64) {
    metrics().dispatch_request_body_bytes.record(
        bytes,
        &[KeyValue::new("http.request.method", method.to_owned())],
    );
}

/// Acquire an in-flight slot for a mock dispatch. The returned guard
/// drops the slot when it goes out of scope, so callers don't need to
/// worry about decrementing on error paths.
pub fn dispatch_in_flight(method: &str) -> InFlightGuard {
    let attrs = vec![KeyValue::new("http.request.method", method.to_owned())];
    metrics().dispatch_active.add(1, &attrs);
    InFlightGuard { attrs }
}

/// Drop-guard for the active-requests UpDownCounter. Decrements on drop,
/// including unwind. Owned by `dispatch_inner` for buffered responses
/// and moved into the streaming journal task for streaming responses so
/// the gauge tracks streams while they're actively pumping bytes.
pub struct InFlightGuard {
    attrs: Vec<KeyValue>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics().dispatch_active.add(-1, &self.attrs);
    }
}

/// Record an MCP tool invocation. `tool` MUST already be bounded to the
/// known tool set (or "unknown") by the caller — never pass a raw
/// client-supplied name straight through, or the label space is
/// unbounded. `outcome` is `ok` / `error`.
pub fn record_mcp_tool(tool: &str, outcome: &str, duration_ms: u64) {
    let m = metrics();
    let tool_kv = KeyValue::new("wm.mcp.tool", tool.to_owned());
    m.mcp_tool_calls_total.add(
        1,
        &[
            tool_kv.clone(),
            KeyValue::new("wm.mcp.outcome", outcome.to_owned()),
        ],
    );
    m.mcp_tool_duration_ms.record(duration_ms, &[tool_kv]);
}

/// Record handler-resource histograms after the handler has finished.
/// `outcome` is `ok` or `handler_error` — the resource histograms aren't
/// useful for non-handler outcomes (unmatched, reserved-path, etc.) so
/// don't call this for those.
pub fn record_handler_resources(outcome: &str, fuel: u64, memory_peak: u64, wall_ms: u64) {
    let attrs = [KeyValue::new("wm.dispatch.outcome", outcome.to_owned())];
    let m = metrics();
    m.handler_fuel_consumed.record(fuel, &attrs);
    m.handler_memory_peak_bytes.record(memory_peak, &attrs);
    m.handler_wall_ms.record(wall_ms, &attrs);
}

/// Increment the trap counter for a handler error. `reason` is one of
/// `fuel`, `epoch`, `memory`, `other` (see [`classify_trap`]).
pub fn record_handler_trap(reason: &str) {
    metrics()
        .handler_traps_total
        .add(1, &[KeyValue::new("wm.trap.reason", reason.to_owned())]);
}

/// Record the streaming head-latency (TTFB). Call once per streaming
/// dispatch when the response head reaches the wire.
pub fn record_streaming_head(latency_ms: u64) {
    metrics().streaming_head_latency_ms.record(latency_ms, &[]);
}

/// Record streaming-completion totals. Call once per streaming dispatch
/// when the handler-task join finishes (in `spawn_streaming_journal`).
/// `duration_ms` is head-delivery to completion — NOT total dispatch
/// time.
pub fn record_streaming_completion(chunks: u64, bytes: u64, disposition: &str, duration_ms: u64) {
    let m = metrics();
    let disp = [KeyValue::new(
        "wm.streaming.disposition",
        disposition.to_owned(),
    )];
    m.streaming_duration_ms.record(duration_ms, &disp);
    m.streaming_chunks_total.add(chunks, &[]);
    m.streaming_bytes_total.add(bytes, &[]);
    m.streaming_terminations_total.add(1, &disp);
}

/// Record a control-plane HTTP request completion (ADR-0024 slice 2).
/// `route` is the matched axum route template (e.g.
/// `/__api/groups/{group}`), NOT the resolved path — path-param values
/// would explode cardinality. `surface` is one of the bounded
/// [`Surface`] labels. `duration_s` is in seconds per HTTP semconv.
pub fn record_internal_http(
    surface: &str,
    method: &str,
    route: &str,
    status: u16,
    duration_s: f64,
) {
    metrics().internal_request_duration_s.record(
        duration_s,
        &[
            KeyValue::new("wm.surface", surface.to_owned()),
            KeyValue::new("http.request.method", method.to_owned()),
            KeyValue::new("http.route", route.to_owned()),
            KeyValue::new("http.response.status_code", status as i64),
        ],
    );
}

/// Record a control-plane request body size (from Content-Length).
/// Keyed by surface + method only — body size shouldn't multiply by
/// route. Skip the call entirely when Content-Length is absent rather
/// than record a misleading zero.
pub fn record_internal_request_body_bytes(surface: &str, method: &str, bytes: u64) {
    metrics().internal_request_body_bytes.record(
        bytes,
        &[
            KeyValue::new("wm.surface", surface.to_owned()),
            KeyValue::new("http.request.method", method.to_owned()),
        ],
    );
}

/// Acquire an in-flight slot for a control-plane request, keyed by
/// surface + method. Drops the slot when the guard goes out of scope.
pub fn internal_in_flight(surface: &str, method: &str) -> InternalInFlightGuard {
    let attrs = vec![
        KeyValue::new("wm.surface", surface.to_owned()),
        KeyValue::new("http.request.method", method.to_owned()),
    ];
    metrics().internal_active_requests.add(1, &attrs);
    InternalInFlightGuard { attrs }
}

/// Drop-guard for `http.server.active_requests`. Decrements on drop,
/// including the response-stream-still-draining and unwind paths.
pub struct InternalInFlightGuard {
    attrs: Vec<KeyValue>,
}

impl Drop for InternalInFlightGuard {
    fn drop(&mut self) {
        metrics().internal_active_requests.add(-1, &self.attrs);
    }
}

/// Classify a handler error into one of the trap-reason buckets the ADR
/// catalog allows: `fuel` / `epoch` / `memory` / `other`. Cheap
/// substring match on the formatted error — slice 1 doesn't try to
/// downcast `wasmtime::Trap` directly; the wasmtime error strings carry
/// the limit name reliably enough for operator-side bucketing.
pub fn classify_trap(error: &wasmtime::Error) -> &'static str {
    let msg = format!("{error:#}").to_ascii_lowercase();
    if msg.contains("fuel") {
        "fuel"
    } else if msg.contains("epoch") || msg.contains("interrupt") {
        "epoch"
    } else if msg.contains("memory") || msg.contains("allocation") {
        "memory"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each classify-by-substring case is its own test so failures point
    // at the exact pattern that didn't match — debugger-friendly when
    // the wasmtime error format changes between minor versions.

    #[test]
    fn classify_trap_fuel() {
        let e = wasmtime::Error::msg("all fuel consumed by wasm");
        assert_eq!(classify_trap(&e), "fuel");
    }

    #[test]
    fn classify_trap_epoch() {
        let e = wasmtime::Error::msg("wasm trap: interrupt");
        assert_eq!(classify_trap(&e), "epoch");
    }

    #[test]
    fn classify_trap_memory() {
        let e = wasmtime::Error::msg("memory minimum size of N pages exceeds allocation");
        assert_eq!(classify_trap(&e), "memory");
    }

    #[test]
    fn classify_trap_other() {
        let e = wasmtime::Error::msg("unreachable code executed");
        assert_eq!(classify_trap(&e), "other");
    }
}

//! Tracing + OTel wiring.
//!
//! The host always logs to stderr (JSON format). When
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans are also exported via OTLP
//! (gRPC) to the configured collector. Default-off — there is no
//! localhost:4317 fallback, so deployments without an OTel collector get
//! pure stderr logs.
//!
//! Resource attributes default to `service.name=wm-host`,
//! `service.version=$CARGO_PKG_VERSION`. Operators override or extend
//! via the standard `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`
//! env vars — see [`resource`] for why the precedence is hand-rolled
//! rather than left to the default builder.
//!
//! Returns a `TelemetryGuard` that the caller must hold for the lifetime
//! of the process and drop on shutdown to flush in-flight spans.

use std::env;

use anyhow::Context;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::EnvResourceDetector;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Adapter that lets the OTel propagator read W3C `traceparent` /
/// `tracestate` from an `http::HeaderMap`. Used on inbound axum
/// requests so the host's dispatch span chains under whatever upstream
/// caller traced the request.
pub struct HeaderExtractor<'a>(pub &'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Adapter that lets the OTel propagator write `traceparent` /
/// `tracestate` into an outbound `http::HeaderMap`. Used when the host
/// calls the compiler sidecar so the sidecar's spans (if it gains
/// instrumentation later) chain under our request.
pub struct HeaderInjector<'a>(pub &'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

const SERVICE_NAME: &str = "wm-host";
const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Held for the lifetime of the process. Drop to flush exporters.
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// Flush exporters and tear down the tracer + meter providers.
    /// Idempotent — subsequent calls are no-ops. Calling on shutdown
    /// ensures the last batch of spans + metrics actually reaches the
    /// collector.
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            // SdkTracerProvider::shutdown returns Result; on a clean
            // shutdown we want to surface flush errors as a warning,
            // not panic.
            if let Err(e) = provider.shutdown() {
                tracing::warn!(error = %e, "OTel tracer provider shutdown failed");
            }
        }
        if let Some(provider) = self.meter_provider.take()
            && let Err(e) = provider.shutdown()
        {
            tracing::warn!(error = %e, "OTel meter provider shutdown failed");
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the global tracing subscriber.
///
/// - Always: JSON formatter to stderr, level filter from `RUST_LOG`
///   (default `info`).
/// - When `OTEL_EXPORTER_OTLP_ENDPOINT` is set: OTLP/gRPC exporter
///   layered on. The W3C Trace Context propagator is installed so
///   incoming `traceparent` headers are honoured.
pub fn init() -> anyhow::Result<TelemetryGuard> {
    // Default to `info`, but quiet the `rmcp` MCP-transport crate to `warn`.
    // At `info` it emits ~7 session-lifecycle spans per MCP connection
    // (`streamable_http_session`, `serve_inner`, `client initialized`, …) —
    // pure library bookkeeping with no WireMirage diagnostic value, and on a
    // host with an OTLP exporter wired up they dominate exported-span volume
    // (and the bill) for nothing. Our own `mcp.tool` spans live on the
    // `wm_host` target, so they're unaffected. Operators can override the
    // whole filter via `RUST_LOG` (e.g. `RUST_LOG=info,rmcp=info` to restore).
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(std::io::stderr);

    // Install the W3C Trace Context propagator unconditionally. The
    // host wants inbound traceparent headers extracted even when OTLP
    // export is disabled — the journal stamps trace_ids on records, and
    // that's useful for log correlation regardless of whether spans
    // are also being exported.
    install_propagator();

    let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

    // ADR-0024: one env-var family for both signals. When the endpoint
    // is set we build BOTH the tracer and meter providers against it;
    // when unset, neither is installed and the global OTel handles stay
    // at their no-op defaults so `record_*` calls in the metrics module
    // remain zero-cost.
    let (tracer_provider, meter_provider) = match otlp_endpoint.as_deref() {
        Some(endpoint) if !endpoint.is_empty() => (
            Some(build_tracer_provider(endpoint)?),
            Some(build_meter_provider(endpoint)?),
        ),
        _ => (None, None),
    };

    if let Some(provider) = tracer_provider.as_ref() {
        global::set_tracer_provider(provider.clone());

        let tracer = provider.tracer(SERVICE_NAME);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(otel_layer)
            .init();
        tracing::info!(
            endpoint = otlp_endpoint.as_deref().unwrap_or(""),
            "OTel OTLP exporter enabled (traces + metrics)"
        );
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .init();
        tracing::debug!(
            "OTEL_EXPORTER_OTLP_ENDPOINT is not set; OTel exporter disabled, stderr-only"
        );
    }

    if let Some(provider) = meter_provider.as_ref() {
        global::set_meter_provider(provider.clone());
    }

    Ok(TelemetryGuard {
        tracer_provider,
        meter_provider,
    })
}

/// Install the W3C Trace Context propagator on the OTel global. Called
/// from `init()` in production; tests that exercise traceparent
/// extraction without going through `init()` (because the global
/// subscriber is set-once) call it explicitly. Idempotent.
pub fn install_propagator() {
    global::set_text_map_propagator(TraceContextPropagator::new());
}

fn build_tracer_provider(endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("build OTLP span exporter")?;

    Ok(SdkTracerProvider::builder()
        .with_resource(resource())
        .with_batch_exporter(exporter)
        .build())
}

fn build_meter_provider(endpoint: &str) -> anyhow::Result<SdkMeterProvider> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("build OTLP metric exporter")?;

    // The reader drives periodic export on a tokio task. The interval
    // follows the standard OTel SDK env var
    // `OTEL_METRIC_EXPORT_INTERVAL` (default 60s), so operators tune
    // it the same way they tune trace batching.
    let reader = PeriodicReader::builder(exporter).build();

    Ok(SdkMeterProvider::builder()
        .with_resource(resource())
        .with_reader(reader)
        .build())
}

/// Shared OTel `Resource` for traces + metrics. `service.name` /
/// `service.version` are the wm defaults; the standard SDK env vars
/// `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` override them.
///
/// Precedence matters and is easy to get wrong: the obvious
/// `Resource::builder().with_attributes([service.name = "wm-host"])`
/// does the OPPOSITE of what the doc above promises. `builder()` already
/// applies the SDK + env detectors, and `with_attributes` merges on top
/// with the new value winning — so a hardcoded default *clobbers*
/// `OTEL_SERVICE_NAME`. (Until 2026-05 that bug silently mislabeled the
/// deployed host as `wm-host` instead of its configured `service.name`,
/// which a per-service metrics-routing collector then dropped on the
/// floor.) So we seed the defaults into an EMPTY builder first, layer
/// the env detector on top (so `OTEL_RESOURCE_ATTRIBUTES` overrides),
/// then apply `OTEL_SERVICE_NAME` last — giving it top precedence per
/// the OTel spec.
fn resource() -> Resource {
    let mut builder = Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", SERVICE_NAME),
            KeyValue::new("service.version", SERVICE_VERSION),
        ])
        .with_detector(Box::new(EnvResourceDetector::new()));
    if let Some(name) = service_name_override(std::env::var("OTEL_SERVICE_NAME").ok()) {
        builder = builder.with_attributes([KeyValue::new("service.name", name)]);
    }
    builder.build()
}

/// The `service.name` override from `OTEL_SERVICE_NAME`: the env value
/// when present and non-empty, else `None` (keep the default). Pulled
/// out as a pure fn so the precedence is unit-testable without touching
/// process-global env vars.
fn service_name_override(env_value: Option<String>) -> Option<String> {
    env_value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// We can't call `init()` twice in one process because the global
    /// subscriber is set-once — instead, exercise `build_tracer_provider`
    /// directly with a non-routable endpoint. The tonic exporter spawns
    /// a background task on the current tokio runtime, so this test
    /// needs `#[tokio::test]`.
    #[tokio::test]
    async fn build_tracer_provider_with_non_routable_endpoint_succeeds() {
        // The gRPC channel is lazy, so building the provider should not
        // fail just because nothing is listening yet.
        let provider = build_tracer_provider("http://127.0.0.1:1").expect("build");
        provider.shutdown().expect("shutdown");
    }

    #[test]
    fn service_name_override_prefers_nonempty_env() {
        // The regression guard for the 2026-05 clobber bug: a set,
        // non-empty OTEL_SERVICE_NAME must override the wm-host default;
        // unset or empty falls back to the default (None = keep default).
        assert_eq!(
            service_name_override(Some("wiremirage".to_string())),
            Some("wiremirage".to_string())
        );
        assert_eq!(service_name_override(None), None);
        assert_eq!(service_name_override(Some(String::new())), None);
    }

    #[test]
    fn header_extractor_reads_known_keys() {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", HeaderValue::from_static("abc"));
        let ex = HeaderExtractor(&headers);
        assert_eq!(ex.get("traceparent"), Some("abc"));
        assert_eq!(ex.get("missing"), None);
        assert!(ex.keys().contains(&"traceparent"));
    }

    #[test]
    fn header_injector_writes_propagation_keys() {
        let mut headers = HeaderMap::new();
        let mut inj = HeaderInjector(&mut headers);
        inj.set("traceparent", "00-foo-bar-01".to_string());
        inj.set("tracestate", "vendor=value".to_string());
        // Bad header names are silently dropped — we don't want a
        // misbehaving propagator to take down the request.
        inj.set("not a valid header name", "x".to_string());
        assert_eq!(headers.get("traceparent").unwrap(), "00-foo-bar-01");
        assert_eq!(headers.get("tracestate").unwrap(), "vendor=value");
        assert_eq!(headers.len(), 2);
    }
}

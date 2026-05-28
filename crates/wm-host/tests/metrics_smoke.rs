//! Tier-2 smoke test for ADR-0024 metrics.
//!
//! Wires an in-memory metric exporter into a test host, drives a few
//! mock requests through it (matched success, unmatched 404), and
//! asserts:
//!
//! - The expected metric families fire (`wm.dispatch.duration_ms`,
//!   `wm.dispatch.active_requests`, `wm.dispatch.request_body_bytes`,
//!   `wm.handler.*`).
//! - The cardinality allowlist holds — no route-shaped labels
//!   (`http.route`, `route.id`, `route.matched_pattern`, `group`,
//!   `user_id`, etc.) appear anywhere in the recorded attributes.
//!
//! The cardinality assertion is the highest-value part: ADR-0024's
//! audience split rests on it, and a future careless attribute add at
//! a recording site would silently expand the operator-facing series
//! space without breaking the rest of the test suite. This test fails
//! loudly if any such label leaks in.
//!
//! Note: `opentelemetry::global::set_meter_provider` is process-wide
//! and effectively set-once for the OnceLock in `wm_host::metrics`.
//! Each integration-test file is its own binary, so the global is
//! freshly initialized here and the `metrics()` OnceLock latches onto
//! our `InMemoryMetricExporter`-backed provider.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_bytes() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

/// Allowlist of attribute keys we are willing to see on any wm metric.
/// Anything outside this set is a cardinality leak per ADR-0024.
const ALLOWED_ATTRS: &[&str] = &[
    "http.request.method",
    "http.response.status_code",
    "wm.dispatch.outcome",
    "wm.trap.reason",
    "wm.streaming.disposition",
];

/// Set up a meter provider backed by an in-memory exporter, install it
/// as the global. Returns the exporter for collection + the provider
/// so callers can `force_flush` it.
fn install_in_memory_metrics() -> (InMemoryMetricExporter, SdkMeterProvider) {
    let exporter = InMemoryMetricExporter::default();
    // 100h export interval — we drive collection with `force_flush()`
    // explicitly so tests don't race a periodic timer.
    let reader = PeriodicReader::builder(exporter.clone())
        .with_interval(std::time::Duration::from_secs(360_000))
        .build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    opentelemetry::global::set_meter_provider(provider.clone());
    (exporter, provider)
}

async fn start_with_seeded_route(
    methods: Vec<&str>,
    path: &str,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: methods.into_iter().map(String::from).collect(),
            path: path.into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_bytes(),
            source: None,
            owner_id: "test-owner".into(),
        })
        .expect("create route");
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes, auth, journal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    (addr, server)
}

fn walk<T: Copy>(data: &opentelemetry_sdk::metrics::data::MetricData<T>, out: &mut Vec<String>) {
    use opentelemetry_sdk::metrics::data::MetricData;
    let mut push = |attrs: Vec<KeyValue>| {
        for kv in attrs {
            out.push(kv.key.as_str().to_string());
        }
    };
    match data {
        MetricData::Gauge(g) => {
            for dp in g.data_points() {
                push(dp.attributes().cloned().collect());
            }
        }
        MetricData::Sum(s) => {
            for dp in s.data_points() {
                push(dp.attributes().cloned().collect());
            }
        }
        MetricData::Histogram(h) => {
            for dp in h.data_points() {
                push(dp.attributes().cloned().collect());
            }
        }
        MetricData::ExponentialHistogram(eh) => {
            for dp in eh.data_points() {
                push(dp.attributes().cloned().collect());
            }
        }
    }
}

/// Pull all metric names + their attribute-key sets out of the
/// exporter's accumulated batches. Returns a Vec of
/// `(metric_name, attr_keys_seen)` across every data point.
fn collected_metric_views(exporter: &InMemoryMetricExporter) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let batches = exporter
        .get_finished_metrics()
        .expect("get finished metrics");
    for batch in batches {
        for scope in batch.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name().to_string();
                let mut keys: Vec<String> = Vec::new();
                match metric.data() {
                    AggregatedMetrics::U64(d) => walk(d, &mut keys),
                    AggregatedMetrics::I64(d) => walk(d, &mut keys),
                    AggregatedMetrics::F64(d) => walk(d, &mut keys),
                }
                keys.sort();
                keys.dedup();
                out.push((name, keys));
            }
        }
    }
    out
}

#[tokio::test]
async fn matched_dispatch_records_dispatch_and_handler_metrics() {
    let (exporter, provider) = install_in_memory_metrics();
    let (addr, _server) = start_with_seeded_route(vec!["GET"], "/bump").await;

    // Drive two successful dispatches to populate histograms.
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let resp = client
            .get(format!("http://{addr}/bump"))
            .send()
            .await
            .expect("send");
        assert!(resp.status().is_success(), "echo handler should return 2xx");
    }

    // Drive one unmatched 404 so the `unmatched_404` outcome shows up.
    let resp = client
        .get(format!("http://{addr}/no/such/route"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);

    provider.force_flush().expect("force flush");
    let collected = collected_metric_views(&exporter);
    let names: Vec<&str> = collected.iter().map(|(n, _)| n.as_str()).collect();

    // Catalog is present.
    for expected in [
        "wm.dispatch.duration_ms",
        "wm.dispatch.active_requests",
        "wm.dispatch.request_body_bytes",
        "wm.handler.fuel_consumed",
        "wm.handler.memory_peak_bytes",
        "wm.handler.wall_ms",
    ] {
        assert!(
            names.contains(&expected),
            "missing metric {expected}; got {names:?}"
        );
    }

    // Cardinality allowlist: no route/group/user-shaped label leaked in.
    for (name, keys) in &collected {
        for key in keys {
            assert!(
                ALLOWED_ATTRS.contains(&key.as_str()),
                "metric {name} carries disallowed attribute {key:?}; \
                 allowed = {ALLOWED_ATTRS:?}"
            );
        }
    }
}

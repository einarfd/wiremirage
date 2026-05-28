//! Tier-2 smoke test for ADR-0024 metrics.
//!
//! Wires an in-memory metric exporter into a test host, drives mock
//! traffic (matched success, unmatched 404) AND a control-plane API
//! call through it, and asserts:
//!
//! - The mock + handler catalog fires (`wm.dispatch.*`, `wm.handler.*`).
//! - The control-plane catalog fires (`http.server.*`).
//! - Per-family cardinality holds: the mock `wm.*` families carry NO
//!   route-shaped label (the unbounded-route invariant), while the
//!   `http.server.*` family may carry `http.route` + `wm.surface`
//!   (bounded internal route set).
//! - `http.route` is the matched route *template* (`/__api/groups/{group}`),
//!   never the resolved path — the property that keeps internal-metric
//!   cardinality bounded.
//!
//! The cardinality assertions are the highest-value part: ADR-0024's
//! audience split rests on them, and a careless attribute add at a
//! recording site would silently expand the series space without
//! breaking the rest of the suite. This test fails loudly if it does.
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

/// Allowlist for the mock-surface `wm.*` metric families. STRICT — no
/// route/group/user-shaped label may appear, because the mock route
/// count is unbounded (set by users). This is the ADR-0024 audience
/// split's load-bearing invariant.
const MOCK_ALLOWED_ATTRS: &[&str] = &[
    "http.request.method",
    "http.response.status_code",
    "wm.dispatch.outcome",
    "wm.trap.reason",
    "wm.streaming.disposition",
];

/// Allowlist for the control-plane `http.server.*` metric family.
/// `http.route` IS permitted here — the internal surface is a bounded
/// set of ~60 route templates fixed in code, so per-route cardinality
/// is operator-safe (ADR-0024 slice 2). `wm.surface` is the bounded
/// api/auth/ui/mcp enum.
const INTERNAL_ALLOWED_ATTRS: &[&str] = &[
    "http.request.method",
    "http.response.status_code",
    "http.route",
    "wm.surface",
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

/// One recorded data point: every (attribute-key, attribute-value)
/// pair, value rendered to string for comparison.
type Attrs = Vec<(String, String)>;

fn walk<T: Copy>(data: &opentelemetry_sdk::metrics::data::MetricData<T>, out: &mut Vec<Attrs>) {
    use opentelemetry_sdk::metrics::data::MetricData;
    let mut push = |kvs: Vec<KeyValue>| {
        out.push(
            kvs.into_iter()
                .map(|kv| (kv.key.as_str().to_string(), kv.value.to_string()))
                .collect(),
        );
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

/// One collected metric: its name and the attribute sets of every data
/// point recorded against it.
struct CollectedMetric {
    name: String,
    data_points: Vec<Attrs>,
}

/// Pull all metrics + their per-data-point attributes out of the
/// exporter's accumulated batches.
fn collected_metrics(exporter: &InMemoryMetricExporter) -> Vec<CollectedMetric> {
    let mut out = Vec::new();
    let batches = exporter
        .get_finished_metrics()
        .expect("get finished metrics");
    for batch in batches {
        for scope in batch.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name().to_string();
                let mut data_points: Vec<Attrs> = Vec::new();
                match metric.data() {
                    AggregatedMetrics::U64(d) => walk(d, &mut data_points),
                    AggregatedMetrics::I64(d) => walk(d, &mut data_points),
                    AggregatedMetrics::F64(d) => walk(d, &mut data_points),
                }
                out.push(CollectedMetric { name, data_points });
            }
        }
    }
    out
}

#[tokio::test]
async fn metrics_cover_mock_and_internal_surfaces_within_cardinality_rules() {
    let (exporter, provider) = install_in_memory_metrics();
    let (addr, _server) = start_with_seeded_route(vec!["GET"], "/bump").await;
    let client = reqwest::Client::new();

    // --- Mock surface: two successful dispatches + one unmatched 404. ---
    for _ in 0..2 {
        let resp = client
            .get(format!("http://{addr}/bump"))
            .send()
            .await
            .expect("send");
        assert!(resp.status().is_success(), "echo handler should return 2xx");
    }
    let resp = client
        .get(format!("http://{addr}/no/such/route"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);

    // --- Internal surface: an authed API call against a param route, so
    // we can verify `http.route` is the *template* not the resolved
    // path. `GET /__api/groups/{group}` resolves a group name into the
    // path; the recorded `http.route` must stay `/__api/groups/{group}`.
    // (The group doesn't exist → 404, which is fine; the route still
    // matched and the metric still records the template.) ---
    let resp = client
        .get(format!("http://{addr}/__api/groups/nonexistent-group"))
        .header("authorization", "Bearer wmt_test")
        .send()
        .await
        .expect("send");
    // Either 404 (no such group) or 200 — both took the matched route.
    assert!(resp.status() == 404 || resp.status() == 200);

    provider.force_flush().expect("force flush");
    let collected = collected_metrics(&exporter);
    let names: Vec<&str> = collected.iter().map(|m| m.name.as_str()).collect();

    // Mock + handler catalog present.
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
            "missing mock/handler metric {expected}; got {names:?}"
        );
    }
    // Internal control-plane catalog present.
    for expected in [
        "http.server.request.duration",
        "http.server.active_requests",
    ] {
        assert!(
            names.contains(&expected),
            "missing internal metric {expected}; got {names:?}"
        );
    }

    // Per-family cardinality allowlist.
    for metric in &collected {
        let allowed: &[&str] = if metric.name.starts_with("http.server.") {
            INTERNAL_ALLOWED_ATTRS
        } else if metric.name.starts_with("wm.") {
            MOCK_ALLOWED_ATTRS
        } else {
            panic!("unexpected metric namespace: {}", metric.name);
        };
        for dp in &metric.data_points {
            for (key, _) in dp {
                assert!(
                    allowed.contains(&key.as_str()),
                    "metric {} carries disallowed attribute {key:?}; allowed = {allowed:?}",
                    metric.name
                );
            }
        }
    }

    // The cardinality-critical property: `http.route` is the route
    // TEMPLATE, never the resolved path. If MatchedPath weren't used,
    // we'd see `/__api/groups/nonexistent-group` here and the series
    // space would scale with group count.
    let internal_routes: Vec<String> = collected
        .iter()
        .filter(|m| m.name == "http.server.request.duration")
        .flat_map(|m| m.data_points.iter())
        .flat_map(|dp| dp.iter())
        .filter(|(k, _)| k == "http.route")
        .map(|(_, v)| v.clone())
        .collect();
    assert!(
        internal_routes.iter().any(|r| r == "/__api/groups/{group}"),
        "expected templated http.route `/__api/groups/{{group}}`, got {internal_routes:?}"
    );
    assert!(
        !internal_routes
            .iter()
            .any(|r| r.contains("nonexistent-group")),
        "http.route leaked a resolved path param: {internal_routes:?}"
    );

    // And `wm.surface` is the bounded enum we expect.
    let surfaces: Vec<String> = collected
        .iter()
        .filter(|m| m.name.starts_with("http.server."))
        .flat_map(|m| m.data_points.iter())
        .flat_map(|dp| dp.iter())
        .filter(|(k, _)| k == "wm.surface")
        .map(|(_, v)| v.clone())
        .collect();
    assert!(
        surfaces
            .iter()
            .all(|s| matches!(s.as_str(), "api" | "auth" | "ui" | "mcp")),
        "unexpected wm.surface value: {surfaces:?}"
    );
}

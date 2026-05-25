//! Tier-2 dispatch test for slice 57 (shared-engine route dispatch).
//!
//! Boots a real wm-host with the vendored js-engine.wasm wired in,
//! registers a `language: "javascript"` route directly via the
//! registry (no compile path involved — the source-language route
//! stores source verbatim), and hits the mock-traffic path with a
//! real HTTP request. Asserts the response came back through the
//! shared engine.

use std::path::PathBuf;
use std::sync::Arc;

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

fn vendored_engine_path() -> Option<PathBuf> {
    // build.rs sets WM_JS_ENGINE_WASM to the OUT_DIR path of the
    // engine wasm. Pre-slice-59 this used to point at
    // crates/wm-host/vendored/, which is gone — the engine is now
    // built at cargo build time per ADR-0020 slice C.
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

async fn start_with_js_route(
    source: &str,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let engine_path = vendored_engine_path()?;
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Runtime::new(storage.clone())
        .expect("runtime")
        .with_js_engine(&engine_path)
        .expect("attach engine");
    let runtime = Arc::new(runtime);
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/echo".into(),
            language: "javascript".into(),
            bindings_version: "0.1.0".into(),
            // Engine routes have no per-route component; the
            // compiled_wasm field stays empty.
            compiled_wasm: Vec::new(),
            source: Some(source.to_string()),
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
    Some((addr, server))
}

#[tokio::test]
async fn javascript_route_dispatches_through_shared_engine() {
    let source = r#"
        function handle(req, route, group) {
          const body = new TextEncoder().encode(
            "engine: " + req.method + " " + req.path
          );
          return {
            status: 200,
            headers: [["content-type", "text/plain; charset=utf-8"]],
            body,
          };
        }
    "#;
    let Some((addr, server)) = start_with_js_route(source).await else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "engine: POST /v1/echo");
    server.abort();
}

#[tokio::test]
async fn javascript_route_can_persist_kv_state_via_shared_engine() {
    // Round-trip route.kv via the engine. Proves the bucket
    // resource is plumbed through correctly: the engine's host
    // import for store still reaches our backing Storage.
    let source = r#"
        function handle(req, route, group) {
          const n = route.incr("hits", 1n);
          const body = new TextEncoder().encode("hits=" + n.toString());
          return { status: 200, headers: [], body };
        }
    "#;
    let Some((addr, server)) = start_with_js_route(source).await else {
        return;
    };

    let client = reqwest::Client::new();
    for expected in 1..=3 {
        let resp = client
            .post(format!("http://{addr}/v1/echo"))
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.expect("body");
        assert_eq!(body, format!("hits={expected}"));
    }
    server.abort();
}

#[tokio::test]
async fn javascript_route_handler_error_surfaces_as_500_with_message() {
    let source = r#"
        function handle(req, route, group) {
          throw new Error("intentional");
        }
    "#;
    let Some((addr, server)) = start_with_js_route(source).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("intentional"),
        "error body surfaces the message: {body}"
    );
    server.abort();
}

// -- ADR-0021: clock primitives ---------------------------------------------

#[tokio::test]
async fn javascript_route_can_use_clock_primitives() {
    // Single handler exercises all three clock imports:
    //   - host.sleep blocks for ~250ms; the test measures elapsed
    //     time below to confirm the host actually slept.
    //   - host.monotonicMs is called before and after the sleep;
    //     the second value must be >= the first + 250 (allow for a
    //     little jitter — we assert >= 200ms of delta).
    //   - host.wallTimeMs returns a value past Jan 2023
    //     (1672531200000ms = 2023-01-01) — sanity that the host is
    //     plumbing real time.
    //
    // The handler returns the values as JSON; the test parses them
    // and asserts on each piece.
    let source = r#"
        function handle(req, route, group) {
          const mono1 = host.monotonicMs();
          host.sleep(250);
          const mono2 = host.monotonicMs();
          const wall = host.wallTimeMs();
          const body = new TextEncoder().encode(JSON.stringify({
            mono1: mono1,
            mono2: mono2,
            mono_delta: mono2 - mono1,
            wall: wall,
          }));
          return {
            status: 200,
            headers: [["content-type", "application/json"]],
            body,
          };
        }
    "#;
    let Some((addr, server)) = start_with_js_route(source).await else {
        return;
    };

    let started = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .send()
        .await
        .expect("send");
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");

    let mono_delta = body["mono_delta"].as_u64().expect("mono_delta u64");
    let wall = body["wall"].as_u64().expect("wall u64");

    // monotonic-ms increased by at least ~200ms across the sleep.
    // Allowing for jitter on a busy test runner.
    assert!(
        mono_delta >= 200,
        "monotonic delta across host.sleep(250) was {mono_delta}ms (expected >= 200)"
    );
    // The actual request elapsed time should be >= the slept time
    // too (it's the host clock measuring the same span).
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "request took {elapsed:?} which is less than the requested sleep"
    );
    // Wall time is plausible — past 2023-01-01.
    assert!(
        wall > 1_672_531_200_000,
        "wall_time_ms returned {wall}, which is before 2023-01-01"
    );

    server.abort();
}

#[tokio::test]
async fn javascript_route_monotonic_ms_is_non_decreasing_across_requests() {
    // Two requests against the same route. Each request returns
    // host.monotonicMs(); the second request's value must be >= the
    // first's. Catches any regression where monotonic accidentally
    // resets per-request or per-instance.
    let source = r#"
        function handle(req, route, group) {
          const v = host.monotonicMs();
          return {
            status: 200,
            headers: [["content-type", "text/plain"]],
            body: new TextEncoder().encode(String(v)),
          };
        }
    "#;
    let Some((addr, server)) = start_with_js_route(source).await else {
        return;
    };

    let client = reqwest::Client::new();
    let first: u64 = client
        .post(format!("http://{addr}/v1/echo"))
        .send()
        .await
        .expect("send 1")
        .text()
        .await
        .expect("body 1")
        .parse()
        .expect("u64 1");
    // Tiny tokio sleep just to ensure wall-time has actually moved
    // forward between requests — without it the two monotonic
    // readings could be sub-ms-apart and equal (which is still
    // valid for "non-decreasing", but makes the assertion weaker).
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let second: u64 = client
        .post(format!("http://{addr}/v1/echo"))
        .send()
        .await
        .expect("send 2")
        .text()
        .await
        .expect("body 2")
        .parse()
        .expect("u64 2");

    assert!(
        second >= first,
        "monotonic should be non-decreasing across requests; got {first} then {second}"
    );
    server.abort();
}

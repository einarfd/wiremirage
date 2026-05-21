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

const VENDORED_ENGINE: &str = "vendored/js-engine.wasm";

fn vendored_engine_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(VENDORED_ENGINE);
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

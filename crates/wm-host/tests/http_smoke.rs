//! Tier-2 HTTP-level integration test: boot the host on a random port,
//! register the echo-handler component as a route, and exercise the
//! request → wasmtime → response path through axum.

use std::sync::Arc;

use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_bytes() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

#[tokio::test]
async fn echo_via_http() {
    let storage = Storage::in_memory();
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_bytes(),
        })
        .expect("create route");
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let app = router(AppState::new(runtime, routes));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/charges"))
        .body(r#"{"amount":1000}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.text().await.expect("body");

    assert_eq!(content_type.as_deref(), Some("text/plain"));
    assert_eq!(body, "echo: POST /v1/charges");

    server.abort();
}

#[tokio::test]
async fn unmatched_path_returns_404() {
    let storage = Storage::in_memory();
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_bytes(),
        })
        .expect("create route");
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let app = router(AppState::new(runtime, routes));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let resp = reqwest::get(format!("http://{addr}/nope"))
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    server.abort();
}

#[tokio::test]
async fn reserved_prefix_returns_404() {
    let storage = Storage::in_memory();
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let app = router(AppState::new(runtime, routes));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    // `/__api/routes` is mounted by the API router; verify that an
    // unhandled reserved-prefix path doesn't fall through to the user
    // route table (which is empty here, but in principle could match a
    // user route at `/__api/typo` if reserved-path enforcement were off).
    let resp = reqwest::get(format!("http://{addr}/__api/typo"))
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    server.abort();
}

#[tokio::test]
async fn matched_pattern_reaches_handler() {
    // Build a fixture that captures path-params would be ideal, but the
    // echo handler only reflects method + literal path, not pattern. So
    // here we just verify a parametrised route matches multiple concrete
    // paths — proof that the router does extract path-param-style URLs.
    let storage = Storage::in_memory();
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/users/{id}".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_bytes(),
        })
        .expect("create route");
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let app = router(AppState::new(runtime, routes));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    for id in ["123", "456", "me"] {
        let body = reqwest::get(format!("http://{addr}/users/{id}"))
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("echo: GET /users/{id}"));
    }

    server.abort();
}

//! Tier-2 HTTP-level integration test: boot the host on a random port,
//! register the echo-handler component as a route, and exercise the
//! request → wasmtime → response path through axum.
//!
//! Mock-traffic dispatch (everything not under a reserved prefix) is open
//! by design — SUTs don't have tokens. These tests therefore use a plain
//! `reqwest::Client` for the call paths.

use std::sync::Arc;

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_bytes() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

/// Build an in-memory app pre-seeded with one route. Returns the bound
/// address and a JoinHandle for the spawned server.
async fn start_with_seeded_route(
    methods: Vec<&str>,
    path: &str,
) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
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
    // The route's auto-named group is the mock-traffic virtual host
    // (ADR-0030): `{group}.localhost`.
    let group = route.group_name.clone();
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
    (addr, group, server)
}

async fn start_empty() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
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

#[tokio::test]
async fn echo_via_http() {
    let (addr, group, server) = start_with_seeded_route(vec!["POST"], "/v1/charges").await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/charges"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
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
    let (addr, group, server) = start_with_seeded_route(vec!["POST"], "/v1/charges").await;
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/nope"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    server.abort();
}

#[tokio::test]
async fn reserved_prefix_returns_404() {
    let (addr, server) = start_empty().await;
    // `/__api/routes` is mounted by the API router; verify that an
    // unhandled reserved-prefix path doesn't fall through to the user
    // route table.
    let resp = reqwest::get(format!("http://{addr}/__api/typo"))
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    server.abort();
}

#[tokio::test]
async fn health_endpoint_returns_ok_without_auth() {
    let (addr, server) = start_empty().await;
    let resp = reqwest::get(format!("http://{addr}/__health"))
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    assert!(body.get("version").is_some(), "expected version field");
    server.abort();
}

#[tokio::test]
async fn ready_endpoint_reports_dependencies_without_auth() {
    let (addr, server) = start_empty().await;
    let resp = reqwest::get(format!("http://{addr}/__ready"))
        .await
        .expect("get");
    // In-memory storage is trivially ok.
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], "ready");
    assert_eq!(body["valkey"], "ok");
    server.abort();
}

#[tokio::test]
async fn mcp_endpoint_requires_bearer_token() {
    let (addr, server) = start_empty().await;
    // No Authorization header → 401. Confirms the MCP route shares the
    // bearer-token gate with the rest of /__api/*.
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/__api/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body("{}")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
    server.abort();
}

#[tokio::test]
async fn matched_pattern_reaches_handler() {
    // The echo handler reflects method + literal path. Verify a
    // parametrised route matches multiple concrete paths — proof that the
    // router extracts path-param-style URLs correctly.
    let (addr, group, server) = start_with_seeded_route(vec!["GET"], "/users/{id}").await;
    for id in ["123", "456", "me"] {
        let body = reqwest::Client::new()
            .get(format!("http://{addr}/users/{id}"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("echo: GET /users/{id}"));
    }
    server.abort();
}

#[tokio::test]
async fn dispatch_rejects_body_above_limit_with_413() {
    // Slice 45 / F-2: an unauthenticated mock-dispatch POST with a
    // body bigger than the configured limit should be rejected
    // before the handler runs. 12 MiB is comfortably over the 10 MiB
    // limit and below anything an actual mock-test body would carry.
    let (addr, group, server) = start_with_seeded_route(vec!["POST"], "/v1/charges").await;
    let body = vec![b'x'; 12 * 1024 * 1024];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/charges"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body(body)
        .send()
        .await
        .expect("dispatch send");
    assert_eq!(resp.status().as_u16(), 413, "payload too large");
    let text = resp.text().await.unwrap_or_default();
    assert!(
        text.contains("exceeds") || text.contains("limit"),
        "body cites the limit: {text}",
    );
    server.abort();
}

#[tokio::test]
async fn dispatch_accepts_body_below_limit() {
    // Sibling to the rejection test: a body just under the cap should
    // still go through. Echo's response repeats the body size, so we
    // can assert the round-trip worked.
    let (addr, group, server) = start_with_seeded_route(vec!["POST"], "/v1/charges").await;
    // 1 MiB — well under the 10 MiB dispatch cap.
    let body = vec![b'x'; 1024 * 1024];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/charges"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body(body)
        .send()
        .await
        .expect("dispatch send");
    assert!(
        resp.status().is_success(),
        "1 MiB body accepted: {}",
        resp.status()
    );
    server.abort();
}

// -- /__api/capabilities (ADR-0021 follow-up) -------------------------------

#[tokio::test]
async fn capabilities_endpoint_lists_overview_and_topics() {
    // GET /__api/capabilities → overview + topic list. Same content
    // the MCP `get_capabilities` tool returns, since both back to
    // `crate::capabilities`. Bearer-token gated like the rest of
    // /__api/*; unauth gets 401.
    let (addr, server) = start_empty().await;

    // Unauth → 401.
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/__api/capabilities"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);

    // Authed → 200 with overview content.
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/__api/capabilities"))
        .header("authorization", "Bearer wmt_test")
        .send()
        .await
        .expect("get authed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["topic"], "overview");
    let content = body["content"].as_str().expect("content");
    assert!(content.contains("function handle"));
    let topics: Vec<&str> = body["available_topics"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for needle in [
        "overview",
        "request",
        "response",
        "store",
        "log",
        "clock",
        "streaming",
        "gotchas",
    ] {
        assert!(
            topics.contains(&needle),
            "available_topics should contain `{needle}`: {topics:?}"
        );
    }

    server.abort();
}

#[tokio::test]
async fn capabilities_endpoint_returns_specific_topic() {
    // GET /__api/capabilities/clock → the clock section, naming the
    // three time primitives.
    let (addr, server) = start_empty().await;

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/__api/capabilities/clock"))
        .header("authorization", "Bearer wmt_test")
        .send()
        .await
        .expect("get clock");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["topic"], "clock");
    let content = body["content"].as_str().expect("content");
    for needle in ["host.sleep", "host.wallTimeMs", "host.monotonicMs"] {
        assert!(
            content.contains(needle),
            "clock topic should mention `{needle}`: {content}"
        );
    }

    server.abort();
}

#[tokio::test]
async fn capabilities_endpoint_unknown_topic_falls_back_to_overview() {
    // Unknown topic → overview, not 404. The MCP tool does the same;
    // exploratory typos shouldn't punish the caller.
    let (addr, server) = start_empty().await;

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/__api/capabilities/nonexistent"))
        .header("authorization", "Bearer wmt_test")
        .send()
        .await
        .expect("get nonexistent");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["topic"], "overview");

    server.abort();
}

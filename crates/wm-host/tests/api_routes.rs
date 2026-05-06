//! Tier-2 end-to-end tests for the slice-3 REST API.
//!
//! Boots the host on a random port, drives it via reqwest, exercises
//! POST/GET/DELETE on `/__api/routes`, and verifies that mock-traffic
//! requests get routed to the registered components.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::Client;
use serde_json::json;
use wm_host::compiler::CompilerClient;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");
const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

fn echo_b64() -> String {
    B64.encode(std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture"))
}

fn counter_b64() -> String {
    B64.encode(std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture"))
}

struct Harness {
    addr: String,
    client: Client,
    server: tokio::task::JoinHandle<()>,
    mock_compiler: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with_compiler(None).await
    }

    /// Spin up a mock compiler that returns `canned_component` for every
    /// `/compile` call, then start the host pointed at it.
    async fn start_with_mock_compiler(canned_component: Vec<u8>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind compiler");
        let mock_addr = listener.local_addr().unwrap();
        let canned_b64 = B64.encode(&canned_component);
        let app = Router::new()
            .route(
                "/compile",
                post(move |State(state): State<Arc<String>>| {
                    let body = (*state).clone();
                    async move {
                        axum::Json(json!({
                            "compiled_wasm": body,
                            "bindings_version": "0.1.0",
                        }))
                    }
                }),
            )
            .with_state(Arc::new(canned_b64));
        let mock = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve");
        });
        let url = format!("http://{mock_addr}");
        let mut h = Self::start_with_compiler(Some(CompilerClient::new(url))).await;
        h.mock_compiler = Some(mock);
        h
    }

    /// Mock compiler that always returns a `compile_failed` error.
    async fn start_with_failing_compiler(message: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind compiler");
        let mock_addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/compile",
            post(move || async move {
                use axum::http::StatusCode;
                use axum::response::IntoResponse;
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": {
                            "code": "compile_failed",
                            "message": message,
                            "diagnostics": ["expected `;` here"],
                        }
                    })),
                )
                    .into_response()
            }),
        );
        let mock = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve");
        });
        let url = format!("http://{mock_addr}");
        let mut h = Self::start_with_compiler(Some(CompilerClient::new(url))).await;
        h.mock_compiler = Some(mock);
        h
    }

    async fn start_with_compiler(compiler: Option<CompilerClient>) -> Self {
        let storage = Storage::in_memory();
        let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
        let registry = Arc::new(Registry::new(storage));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let mut state = AppState::new(runtime, routes);
        if let Some(c) = compiler {
            state = state.with_compiler(c);
        }
        let app = router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr").to_string();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve");
        });

        Harness {
            addr,
            client: Client::new(),
            server,
            mock_compiler: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    async fn create_route_body(&self, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url("/__api/routes"))
            .json(&body)
            .send()
            .await
            .expect("post")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(c) = self.mock_compiler.take() {
            c.abort();
        }
    }
}

// -- Happy path ---------------------------------------------------------------

#[tokio::test]
async fn create_then_call_then_delete_then_404() {
    let h = Harness::start().await;

    // Create a route via the API.
    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .expect("location header");
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"]
        .as_str()
        .expect("group name")
        .to_string();
    let number = body["number"].as_u64().expect("number");
    assert_eq!(location, format!("/__api/routes/{group}/{number}"));

    // Call the route — verifies the dispatcher sees it.
    let resp = h
        .client
        .post(h.url("/v1/charges"))
        .body(r#"{"x":1}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.expect("body"), "echo: POST /v1/charges");

    // Show the route via GET.
    let show = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show.status().as_u16(), 200);

    // Delete it.
    let del = h
        .client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 204);

    // Mock traffic now 404s.
    let resp = h
        .client
        .post(h.url("/v1/charges"))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 404);

    // GET 404s too.
    let show2 = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show2.status().as_u16(), 404);
}

#[tokio::test]
async fn list_routes_returns_created() {
    let h = Harness::start().await;

    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/a",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": echo_b64(),
    }))
    .await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/b",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": echo_b64(),
    }))
    .await;

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().expect("routes").len(), 2);
}

#[tokio::test]
async fn path_params_extracted_for_user_routes() {
    let h = Harness::start().await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/users/{id}",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": echo_b64(),
    }))
    .await;

    for id in ["123", "me", "abc-def"] {
        let body = h
            .client
            .get(h.url(&format!("/users/{id}")))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("echo: GET /users/{id}"));
    }
}

#[tokio::test]
async fn counter_state_persists_across_calls_in_memory() {
    let h = Harness::start().await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/bump",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": counter_b64(),
    }))
    .await;
    for expected in 1..=3u32 {
        let body = h
            .client
            .get(h.url("/bump"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("count={expected}"));
    }
}

// -- Validation / errors -------------------------------------------------------

#[tokio::test]
async fn rejects_source_request_when_no_compiler_configured() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "typescript",
            "source": "export default async function handle() { return new Response('hi'); }"
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "compile_failed");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("compiler sidecar not configured"),
        "got: {}",
        body["error"]["message"]
    );
}

#[tokio::test]
async fn source_path_with_mock_compiler_creates_callable_route() {
    // Mock compiler returns the echo fixture's bytes; the host should
    // accept them and dispatch normally.
    let h = Harness::start_with_mock_compiler(echo_bytes()).await;

    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "typescript",
            "source": "export function handle(req, _r, _g) { return { status: 200, headers: [], body: new Uint8Array() }; }",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["language"], "typescript");

    // The route now dispatches via the (echo-fixture) component.
    let resp = h
        .client
        .post(h.url("/v1/charges"))
        .body("ignored")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.expect("body"), "echo: POST /v1/charges");
}

#[tokio::test]
async fn source_path_surfaces_compiler_diagnostics() {
    let h = Harness::start_with_failing_compiler("transpile failed").await;

    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/bad",
            "language": "typescript",
            "source": "??? not valid",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "compile_failed");
    assert_eq!(body["error"]["message"], "transpile failed");
    let diags = body["error"]["diagnostics"]
        .as_array()
        .expect("diagnostics");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0], "expected `;` here");
}

fn echo_bytes() -> Vec<u8> {
    std::fs::read(env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT")).expect("read echo fixture")
}

#[tokio::test]
async fn rejects_reserved_path() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/__api/sneaky",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn rejects_unsupported_bindings_version() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "wasm",
            "bindings_version": "9.9.9",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn rejects_malformed_compiled_wasm() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": B64.encode(b"definitely not wasm"),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "compile_failed");
}

#[tokio::test]
async fn rejects_invalid_path_pattern() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "no-leading-slash",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn rejects_pattern_shape_conflict() {
    let h = Harness::start().await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/users/{id}",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": echo_b64(),
    }))
    .await;
    // /users/me has the same shape as /users/{id} — must conflict.
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/users/me",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

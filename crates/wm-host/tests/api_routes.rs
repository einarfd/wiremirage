//! Tier-2 end-to-end tests for the slice-3 REST API.
//!
//! Boots the host on a random port, drives it via reqwest, exercises
//! POST/GET/DELETE on `/__api/routes`, and verifies that mock-traffic
//! requests get routed to the registered components.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::Client;
use serde_json::json;
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
}

impl Harness {
    async fn start() -> Self {
        let storage = Storage::in_memory();
        let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
        let registry = Arc::new(Registry::new(storage));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let app = router(AppState::new(runtime, routes));

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
async fn rejects_source_based_request_until_compiler_lands() {
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

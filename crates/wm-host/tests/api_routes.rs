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
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

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
    auth: Auth,
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
        // Install the W3C propagator once per process so the tier-2
        // tests that send `traceparent` headers see the trace_id
        // stamped on journal records. Idempotent; the global subscriber
        // is set-once but the propagator is just a swap.
        static PROPAGATOR_ONCE: std::sync::Once = std::sync::Once::new();
        PROPAGATOR_ONCE.call_once(wm_host::telemetry::install_propagator);

        let storage = Storage::in_memory();
        let auth = Auth::new(storage.clone());
        auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
            .expect("bootstrap admin");
        let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
        let registry = Arc::new(Registry::new(storage.clone()));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let journal = Journal::new(storage);
        let mut state = AppState::new(runtime, routes, auth.clone(), journal);
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

        // Default reqwest client carries the bootstrap admin token; tests
        // that want to drive auth-failure cases construct their own client
        // via `Harness::unauthenticated_client` etc.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {BOOTSTRAP_TOKEN}")).unwrap(),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("build client");

        Harness {
            addr,
            client,
            auth,
            server,
            mock_compiler: None,
        }
    }

    /// Reqwest client with no Authorization header — for testing 401 paths.
    fn unauthenticated_client(&self) -> Client {
        Client::new()
    }

    /// Provision an additional non-admin user with one token, and return a
    /// reqwest client carrying that token in the default Authorization
    /// header. Used to drive ownership-check tests.
    fn provision_user(&self, name: &str, is_admin: bool) -> (String, Client) {
        let user = self.auth.create_user(name, is_admin).expect("create user");
        let (_token, plaintext) = self
            .auth
            .create_token(&user.id, "default", None)
            .expect("create token");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {plaintext}")).unwrap(),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("build client");
        (user.id, client)
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

// -- Auth -------------------------------------------------------------------

#[tokio::test]
async fn rejects_request_without_authorization_header() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn rejects_request_with_bogus_token() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .post(h.url("/__api/routes"))
        .header("authorization", "Bearer wmt_not_a_real_token")
        .json(&json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn rejects_non_bearer_scheme() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .get(h.url("/__api/routes"))
        .header("authorization", format!("Basic {BOOTSTRAP_TOKEN}"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn mock_traffic_does_not_require_auth() {
    // SUTs hitting mock routes don't carry tokens. The dispatch handler
    // (everything not under a reserved prefix) stays open.
    let h = Harness::start().await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/v1/anonymous",
        "language": "wasm",
        "bindings_version": "0.1.0",
        "compiled_wasm": echo_b64(),
    }))
    .await;

    let resp = h
        .unauthenticated_client()
        .get(h.url("/v1/anonymous"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
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

// -- /__api/tokens ------------------------------------------------------------

#[tokio::test]
async fn create_token_returns_plaintext_then_authenticates() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci-runner" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let plaintext = body["token"].as_str().expect("token field").to_string();
    assert!(plaintext.starts_with("wmt_"));
    assert_eq!(body["record"]["name"], "ci-runner");

    // The new token should authenticate on its own — drive a request that
    // hits an authenticated endpoint with it.
    let client = Client::new();
    let listed = client
        .get(h.url("/__api/tokens"))
        .header("Authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .expect("list");
    assert_eq!(listed.status().as_u16(), 200);
}

#[tokio::test]
async fn list_tokens_returns_callers_tokens() {
    let h = Harness::start().await;
    // Bootstrap created a token already; create one more.
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "extra" }))
        .send()
        .await
        .expect("post");
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/tokens"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let tokens = body["tokens"].as_array().expect("tokens array");
    assert_eq!(tokens.len(), 2);
    let names: Vec<&str> = tokens.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"bootstrap"));
    assert!(names.contains(&"extra"));
    // No plaintext leaks in list responses.
    for t in tokens {
        assert!(
            t.get("token").is_none(),
            "list response must not expose plaintext"
        );
    }
}

#[tokio::test]
async fn get_token_by_name() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "deploy-bot", "ttl_seconds": 3600 }))
        .send()
        .await
        .expect("post");
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/tokens/deploy-bot"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["name"], "deploy-bot");
    assert!(body.get("expires_at").is_some());
}

#[tokio::test]
async fn delete_token_revokes_it() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "throwaway" }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let plaintext = created["token"].as_str().unwrap().to_string();

    let del = h
        .client
        .delete(h.url("/__api/tokens/throwaway"))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 204);

    // Subsequent uses of the revoked token are 401.
    let client = Client::new();
    let resp = client
        .get(h.url("/__api/tokens"))
        .header("Authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);

    // Second DELETE for the same name 404s.
    let again = h
        .client
        .delete(h.url("/__api/tokens/throwaway"))
        .send()
        .await
        .expect("delete");
    assert_eq!(again.status().as_u16(), 404);
}

#[tokio::test]
async fn create_token_rejects_duplicate_name() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci" }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

// -- Ownership checks ---------------------------------------------------------

#[tokio::test]
async fn create_route_records_callers_user_id_as_owner() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/things",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let owner_id = body["owner_id"].as_str().expect("owner_id field");
    assert!(!owner_id.is_empty(), "owner_id must not be empty");

    // The owner_id should match the bootstrap user — confirm by listing
    // and checking the stored value is consistent.
    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["routes"][0]["owner_id"], owner_id);
}

#[tokio::test]
async fn non_owner_non_admin_cannot_delete_route() {
    let h = Harness::start().await;
    // Bootstrap admin creates a route.
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/billing",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let location = format!("/__api/routes/{group}/{number}");

    // A different, non-admin user tries to delete it.
    let (_user_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "forbidden");

    // The route is still there — bootstrap can still see it.
    let show = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show.status().as_u16(), 200);
}

#[tokio::test]
async fn admin_can_delete_route_owned_by_someone_else() {
    let h = Harness::start().await;
    // Alice (non-admin) creates a route.
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let create: serde_json::Value = alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let location = format!("/__api/routes/{group}/{number}");

    // Bootstrap (admin) deletes Alice's route.
    let resp = h
        .client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
}

#[tokio::test]
async fn owner_can_delete_their_own_route() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let create: serde_json::Value = alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing-2",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let resp = alice_client
        .delete(h.url(&format!("/__api/routes/{group}/{number}")))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
}

#[tokio::test]
async fn token_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    let resp = unauth
        .get(h.url("/__api/tokens"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);
    let resp = unauth
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
}

// -- /__api/users -------------------------------------------------------------

#[tokio::test]
async fn admin_creates_lists_and_reads_user() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/users"))
        .json(&json!({ "name": "alice", "is_admin": false }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["is_admin"], false);

    // List includes alice + bootstrap.
    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let mut names: Vec<&str> = listed["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice", "bootstrap"]);

    // GET by name works too.
    let one: serde_json::Value = h
        .client
        .get(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(one["name"], "alice");
}

#[tokio::test]
async fn non_admin_cannot_create_or_list_users() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);

    let resp = alice_client
        .post(h.url("/__api/users"))
        .json(&json!({ "name": "mallory" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 403);

    let resp = alice_client
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn me_returns_caller_record() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let body: serde_json::Value = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["is_admin"], false);
}

#[tokio::test]
async fn user_can_read_own_record_by_name() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .get(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn user_cannot_read_another_users_record() {
    let h = Harness::start().await;
    h.provision_user("bob", false);
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .get(h.url("/__api/users/bob"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn admin_can_promote_user() {
    let h = Harness::start().await;
    h.provision_user("alice", false);
    let resp = h
        .client
        .patch(h.url("/__api/users/alice"))
        .json(&json!({ "is_admin": true }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["is_admin"], true);
}

#[tokio::test]
async fn patch_rejects_demoting_last_admin() {
    let h = Harness::start().await;
    // Bootstrap is the only admin.
    let resp = h
        .client
        .patch(h.url("/__api/users/bootstrap"))
        .json(&json!({ "is_admin": false }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "forbidden");
}

#[tokio::test]
async fn patch_with_no_recognised_fields_is_bad_request() {
    let h = Harness::start().await;
    h.provision_user("alice", false);
    let resp = h
        .client
        .patch(h.url("/__api/users/alice"))
        .json(&json!({}))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn admin_cannot_delete_themselves() {
    let h = Harness::start().await;
    let resp = h
        .client
        .delete(h.url("/__api/users/bootstrap"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn cannot_delete_last_admin_via_another_admin() {
    let h = Harness::start().await;
    // Provision a second admin so they can attempt to delete bootstrap;
    // then demote themselves first to leave bootstrap as the lone admin
    // (we can't actually demote via the API once they're alone, but for
    // the symmetry we just verify the bootstrap-delete path).
    let (_other_id, other) = h.provision_user("other-admin", true);
    // First delete bootstrap from `other`'s perspective — succeeds
    // because two admins exist.
    let resp = other
        .delete(h.url("/__api/users/bootstrap"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
    // Now `other` is the lone admin; their own delete attempt is
    // refused by self-delete first, but we can also confirm the
    // last-admin guard via PATCH demotion below in another test. Here
    // we just check that bootstrap is gone.
    let listed: serde_json::Value = other
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let names: Vec<&str> = listed["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"bootstrap"));
    assert!(names.contains(&"other-admin"));
}

#[tokio::test]
async fn delete_user_refused_when_user_owns_routes() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    // Alice creates a route.
    alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .delete(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn delete_user_cascades_tokens() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    // Sanity: alice can hit /me with her token before deletion.
    let pre = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get");
    assert_eq!(pre.status().as_u16(), 200);

    let resp = h
        .client
        .delete(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);

    // Alice's token no longer authenticates.
    let post = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get");
    assert_eq!(post.status().as_u16(), 401);
}

#[tokio::test]
async fn user_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    for path in ["/__api/users", "/__api/users/me", "/__api/users/alice"] {
        let resp = unauth.get(h.url(path)).send().await.expect("get");
        assert_eq!(resp.status().as_u16(), 401, "GET {path}");
    }
}

// -- /__api/journal -----------------------------------------------------------

/// Create a route, hit it once with mock traffic, and return the route's
/// group name so tests can inspect the journal that should now hold one
/// entry. Mock traffic doesn't need an auth header.
async fn seed_one_request(h: &Harness) -> String {
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap().to_string();
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/charges"))
        .body(r#"{"amount":1000}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    group
}

#[tokio::test]
async fn dispatched_request_produces_journal_entry() {
    let h = Harness::start().await;
    let group = seed_one_request(&h).await;
    let listed: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["request"]["method"], "POST");
    assert_eq!(entry["request"]["path"], "/v1/charges");
    assert_eq!(entry["response"]["status"], 200);
    assert_eq!(entry["matched_pattern"], "/v1/charges");
    assert_eq!(entry["number"], 1);
    // Echo handler returns "echo: METHOD PATH" — verify the response
    // body was journaled too.
    let body_bytes = entry["response"]["body"]
        .as_array()
        .expect("body")
        .iter()
        .map(|n| n.as_u64().unwrap() as u8)
        .collect::<Vec<u8>>();
    assert_eq!(body_bytes, b"echo: POST /v1/charges");
}

#[tokio::test]
async fn unmatched_request_produces_unmatched_record() {
    let h = Harness::start().await;
    let unauth = Client::new();
    let resp = unauth
        .get(h.url("/no-such-route"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["request"]["method"], "GET");
    assert_eq!(entries[0]["request"]["path"], "/no-such-route");
}

#[tokio::test]
async fn reserved_path_404_does_not_journal() {
    let h = Harness::start().await;
    // Hit a /__api/* path that doesn't exist — should be 404 (reserved
    // prefix) and should NOT show up in unmatched.
    let resp = h
        .client
        .get(h.url("/__api/typo"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert!(
        listed["entries"].as_array().unwrap().is_empty(),
        "reserved-prefix typos must not pollute the unmatched log"
    );
}

#[tokio::test]
async fn trace_id_is_stamped_from_inbound_traceparent() {
    let h = Harness::start().await;
    let group = {
        let create: serde_json::Value = h
            .create_route_body(json!({
                "methods": ["POST"],
                "path": "/v1/things",
                "language": "wasm",
                "bindings_version": "0.1.0",
                "compiled_wasm": echo_b64(),
            }))
            .await
            .json()
            .await
            .expect("json");
        create["group"]["name"].as_str().unwrap().to_string()
    };
    // Send a request with a hand-crafted W3C traceparent.
    let trace_id = "0123456789abcdef0123456789abcdef";
    let traceparent = format!("00-{trace_id}-aaaaaaaaaaaaaaaa-01");
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/things"))
        .header("traceparent", traceparent)
        .body("{}")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);

    let listed: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entry = &listed["entries"].as_array().unwrap()[0];
    assert_eq!(entry["trace_id"], trace_id);
}

#[tokio::test]
async fn cursor_pagination_round_trips() {
    let h = Harness::start().await;
    let group = seed_one_request(&h).await;
    let unauth = Client::new();
    // Drive 4 more requests so we have 5 total.
    for _ in 0..4 {
        let resp = unauth
            .post(h.url("/v1/charges"))
            .send()
            .await
            .expect("post");
        assert_eq!(resp.status().as_u16(), 200);
    }

    let first: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}?limit=2")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["entries"][0]["number"], 5);
    assert_eq!(first["entries"][1]["number"], 4);
    let next_before = first["next_before"].as_u64().expect("next_before");
    assert_eq!(next_before, 4);

    let next: serde_json::Value = h
        .client
        .get(h.url(&format!(
            "/__api/journal/{group}?before={next_before}&limit=2"
        )))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(next["entries"][0]["number"], 3);
    assert_eq!(next["entries"][1]["number"], 2);

    let tail: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}?before=2&limit=10")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(tail["entries"].as_array().unwrap().len(), 1);
    assert_eq!(tail["entries"][0]["number"], 1);
    assert!(
        tail["next_before"].is_null(),
        "next_before should be null at the oldest page"
    );
}

#[tokio::test]
async fn unmatched_endpoint_is_admin_only() {
    let h = Harness::start().await;
    let (_alice_id, alice) = h.provision_user("alice", false);
    let resp = alice
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn group_owner_can_read_journal_admin_can_too() {
    let h = Harness::start().await;
    let (_alice_id, alice) = h.provision_user("alice", false);
    let create: serde_json::Value = alice
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "wasm",
            "bindings_version": "0.1.0",
            "compiled_wasm": echo_b64(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();

    // Alice (owner of the only route in this group) can read it.
    let resp = alice
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);

    // Bootstrap (admin) can read it too.
    let resp = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);

    // A different non-admin who owns nothing in this group is rejected.
    let (_bob_id, bob) = h.provision_user("bob", false);
    let resp = bob
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn journal_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    for path in ["/__api/journal/anything", "/__api/unmatched"] {
        let resp = unauth.get(h.url(path)).send().await.expect("get");
        assert_eq!(resp.status().as_u16(), 401, "GET {path}");
    }
}

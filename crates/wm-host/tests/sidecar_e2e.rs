//! Tier-3 end-to-end test against the real TypeScript compiler sidecar.
//!
//! Spins up `wiremirage/compiler-typescript:dev` (built locally via
//! `compiler/typescript/Dockerfile`), boots the host pointed at it, and
//! exercises the full source → componentize → register → dispatch path.
//!
//! Gated behind the `sidecar-tests` feature so plain `cargo test` doesn't
//! require Docker or a pre-built image.

#![cfg(feature = "sidecar-tests")]

use std::sync::Arc;

use serde_json::json;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const SIDECAR_IMAGE: &str = "wiremirage/compiler-typescript";
const SIDECAR_TAG: &str = "dev";
const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

fn authorized_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {BOOTSTRAP_TOKEN}"))
            .expect("header value"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("build client")
}

async fn start_sidecar() -> (ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new(SIDECAR_IMAGE, SIDECAR_TAG)
        .with_exposed_port(9100.tcp())
        .with_wait_for(WaitFor::message_on_stdout("listening"))
        .start()
        .await
        .expect("start sidecar (was the image built?)");
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(9100.tcp())
        .await
        .expect("get_host_port_ipv4");
    let url = format!("http://{host}:{port}");
    (container, url)
}

#[tokio::test]
async fn typescript_source_compiles_and_dispatches_end_to_end() {
    let (_container, sidecar_url) = start_sidecar().await;

    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal)
        .with_compiler(CompilerClient::new(sidecar_url));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    let client = authorized_client();

    let source = r#"
        export function handle(req, _route, _group) {
          const msg = `compiled-then-dispatched: ${req.method} ${req.path}`;
          return {
            status: 200,
            headers: [["content-type", "text/plain"]],
            body: new TextEncoder().encode(msg),
          };
        }
    "#;

    let create = client
        .post(format!("http://{addr}/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "typescript",
            "source": source,
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(create.status().as_u16(), 201);

    let resp = client
        .post(format!("http://{addr}/v1/charges"))
        .body("body-is-ignored-by-the-fixture")
        .send()
        .await
        .expect("post");
    let status = resp.status().as_u16();
    let err_header = resp
        .headers()
        .get("x-wiremirage-error")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.text().await.expect("body");
    assert_eq!(
        status, 200,
        "expected 200, got {status}; X-Wiremirage-Error={err_header:?}; body={body}"
    );
    assert_eq!(body, "compiled-then-dispatched: POST /v1/charges");

    server.abort();
}

#[tokio::test]
async fn malformed_typescript_surfaces_diagnostics() {
    let (_container, sidecar_url) = start_sidecar().await;

    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal)
        .with_compiler(CompilerClient::new(sidecar_url));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    let client = authorized_client();

    let resp = client
        .post(format!("http://{addr}/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "typescript",
            // Missing close paren — tsc reports a syntax error.
            "source": "export function handle(req, _route, _group { return null; }",
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "compile_failed");
    let diags = body["error"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(!diags.is_empty(), "expected at least one diagnostic");

    server.abort();
}

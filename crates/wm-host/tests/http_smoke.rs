//! Tier-2 HTTP-level integration test: boot the host on a random port,
//! point it at the echo-handler fixture, and assert that the HTTP round-trip
//! works end to end.

use std::path::PathBuf;
use std::sync::Arc;

use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

#[tokio::test]
async fn echo_via_http() {
    let runtime = Arc::new(Runtime::new(Storage::in_memory()).expect("runtime"));
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");
    let app = router(AppState::new(runtime, component));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let client = reqwest::Client::new();
    let resp = client
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
async fn echo_root_path() {
    let runtime = Arc::new(Runtime::new(Storage::in_memory()).expect("runtime"));
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");
    let app = router(AppState::new(runtime, component));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let resp = reqwest::get(format!("http://{addr}/")).await.expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "echo: GET /");

    server.abort();
}

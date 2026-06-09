//! Regression test for the slice-24 graceful-shutdown bug.
//!
//! Symptom that motivated this test: pressing Ctrl-C while a browser
//! tab was pointed at `/ui/journal/live` logged "received Ctrl-C,
//! shutting down" but the process hung indefinitely. The cause was
//! `axum::serve(...).with_graceful_shutdown()` waiting for all
//! in-flight requests to drain, while the SSE journal tail held the
//! connection open forever. The fix wires a `watch` channel into the
//! SSE handler so it ends the response stream when the host fires
//! its shutdown signal.
//!
//! This test reproduces the wiring at the unit level: spin up the
//! host with a shutdown receiver, open an SSE connection to
//! `/api/journal/tail`, fire the shutdown, and assert the SSE
//! response body completes within a small bound. Without the fix,
//! the response stays open and the test times out.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::watch;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

#[tokio::test]
async fn sse_tail_ends_when_shutdown_fires() {
    // Spin up a minimal host with the shutdown receiver attached.
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let admin = auth.create_user("admin", true).expect("admin");
    let (_record, plaintext) = auth
        .create_token(&admin.id, "test", None)
        .expect("create token");

    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState::new(runtime, routes, auth, journal).with_shutdown(shutdown_rx);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Open the SSE tail. Admin host-wide is admin-only on the host,
    // and we authenticated as admin via the bearer token above.
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{addr}/api/journal/tail"))
        .header("authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .expect("open SSE");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/event-stream"),
        "expected SSE response"
    );

    // Spawn a task that fires the shutdown after a short delay; in
    // production this happens inside `axum::serve.with_graceful_shutdown`
    // after the signal handler completes.
    let shutdown_at = tokio::time::Instant::now() + Duration::from_millis(150);
    tokio::spawn(async move {
        tokio::time::sleep_until(shutdown_at).await;
        let _ = shutdown_tx.send(true);
    });

    // The response body should complete within a generous bound. Pre-fix
    // it would never complete — `bytes()` collects until EOF, and the
    // SSE stream never ended.
    let collected = tokio::time::timeout(Duration::from_secs(3), resp.bytes()).await;
    assert!(
        collected.is_ok(),
        "SSE response should close after shutdown; instead it hung"
    );
}

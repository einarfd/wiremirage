//! Tier-2 tests for the DCR registration guards (security-audit finding
//! #4): `POST /auth/oauth/register` is unauthenticated and every attempt
//! costs an argon2id hash, so the endpoint is rate-limited per source IP
//! and capped host-wide on registered clients.

use std::sync::Arc;

use serde_json::Value;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start(max_oauth_clients: usize) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state =
        AppState::new(runtime, routes, auth, journal).with_max_oauth_clients(max_oauth_clients);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    Harness { addr, server }
}

fn register_body() -> Value {
    serde_json::json!({
        "client_name": "test-client",
        "redirect_uris": ["http://127.0.0.1:54321/cb"],
    })
}

async fn register(h: &Harness, client: &reqwest::Client) -> reqwest::Response {
    client
        .post(format!("http://{}/auth/oauth/register", h.addr))
        .json(&register_body())
        .send()
        .await
        .expect("register")
}

#[tokio::test]
async fn register_succeeds_under_the_limits() {
    let h = start(1000).await;
    let client = reqwest::Client::new();
    let resp = register(&h, &client).await;
    assert_eq!(resp.status(), 201, "first registration should succeed");
    let body: Value = resp.json().await.expect("json");
    assert!(body["client_id"].as_str().unwrap().starts_with("wmc_"));
    assert!(body["client_secret"].as_str().unwrap().starts_with("wmcs_"));
}

#[tokio::test]
async fn register_is_rate_limited_after_five_attempts_from_one_source() {
    // WM_TRUSTED_PROXY is unset in the harness, so every request keys to
    // the same placeholder IP — this exercises the shared-bucket path:
    // five attempts from that one bucket, then the sixth is locked out.
    let h = start(1000).await;
    let client = reqwest::Client::new();
    for i in 0..5 {
        let resp = register(&h, &client).await;
        assert_eq!(resp.status(), 201, "attempt {} should succeed", i + 1);
    }
    let resp = register(&h, &client).await;
    assert_eq!(resp.status(), 429, "sixth attempt should be locked out");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "too_many_requests");
}

#[tokio::test]
async fn register_cap_rejects_beyond_max_clients() {
    let h = start(2).await;
    let client = reqwest::Client::new();
    assert_eq!(register(&h, &client).await.status(), 201);
    assert_eq!(register(&h, &client).await.status(), 201);
    let resp = register(&h, &client).await;
    assert_eq!(
        resp.status(),
        429,
        "attempt past the cap should be rejected"
    );
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "registration_limit_reached");
}

#[tokio::test]
async fn register_cap_zero_disables_registration() {
    let h = start(0).await;
    let client = reqwest::Client::new();
    let resp = register(&h, &client).await;
    assert_eq!(resp.status(), 429);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "registration_limit_reached");
}

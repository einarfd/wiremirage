//! Tier-2 tests for slice 51 (MCP-OAuth discovery).
//!
//! Both well-known endpoints return JSON shaped per their RFCs. The
//! interesting assertion is that the embedded URLs (issuer,
//! authorization_endpoint, ...) are derived from the request's Host
//! header rather than hardcoded, so a host behind a TLS edge can
//! advertise the public URL.

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

async fn start(trust_forwarded: bool) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state =
        AppState::new(runtime, routes, auth, journal).with_trust_forwarded_headers(trust_forwarded);
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

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn protected_resource_metadata_names_the_mcp_endpoint() {
    let h = start(false).await;
    let resp = client()
        .get(format!(
            "http://{}/.well-known/oauth-protected-resource",
            h.addr
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["resource"],
        Value::String(format!("http://{}/__api/mcp", h.addr))
    );
    assert!(
        body["authorization_servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(&format!("http://{}", h.addr))),
        "authorization_servers includes the host base URL: {body:#?}"
    );
}

#[tokio::test]
async fn authorization_server_metadata_advertises_every_required_field() {
    let h = start(false).await;
    let resp = client()
        .get(format!(
            "http://{}/.well-known/oauth-authorization-server",
            h.addr
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    let base = format!("http://{}", h.addr);
    assert_eq!(body["issuer"], Value::String(base.clone()));
    assert_eq!(
        body["authorization_endpoint"],
        Value::String(format!("{base}/__auth/oauth/authorize"))
    );
    assert_eq!(
        body["token_endpoint"],
        Value::String(format!("{base}/__auth/oauth/token"))
    );
    assert_eq!(
        body["registration_endpoint"],
        Value::String(format!("{base}/__auth/oauth/register"))
    );
    assert_eq!(
        body["revocation_endpoint"],
        Value::String(format!("{base}/__auth/oauth/revoke"))
    );
    // PKCE with S256 is mandatory per ADR-0019; explicitly advertise it.
    assert_eq!(
        body["code_challenge_methods_supported"],
        Value::Array(vec![Value::String("S256".into())])
    );
    // Grants we'll support in slice 52 + 53.
    let grants: Vec<&str> = body["grant_types_supported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(grants.contains(&"authorization_code"));
    assert!(grants.contains(&"refresh_token"));
}

#[tokio::test]
async fn forwarded_proto_is_ignored_unless_trust_flag_is_set() {
    // X-Forwarded-Proto must not influence the advertised base URL
    // unless the operator opted into trusting headers — otherwise a
    // directly-reachable host could be tricked into telling clients
    // to use https for endpoints it actually serves over http.
    let h = start(false).await;
    let resp = client()
        .get(format!(
            "http://{}/.well-known/oauth-authorization-server",
            h.addr
        ))
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .expect("send");
    let body: Value = resp.json().await.unwrap();
    let issuer = body["issuer"].as_str().unwrap();
    assert!(
        issuer.starts_with("http://"),
        "issuer should ignore X-Forwarded-Proto when trust flag off, got: {issuer}"
    );
}

#[tokio::test]
async fn forwarded_proto_is_honored_when_trust_flag_is_set() {
    let h = start(true).await;
    let resp = client()
        .get(format!(
            "http://{}/.well-known/oauth-authorization-server",
            h.addr
        ))
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .expect("send");
    let body: Value = resp.json().await.unwrap();
    let issuer = body["issuer"].as_str().unwrap();
    assert!(
        issuer.starts_with("https://"),
        "issuer should honor X-Forwarded-Proto when trust flag on, got: {issuer}"
    );
}

#[tokio::test]
async fn mcp_endpoint_401_carries_www_authenticate_discovery_hint() {
    // ADR-0019 slice D: an unauth'd request to /__api/mcp gets a
    // 401 with `WWW-Authenticate: Bearer resource_metadata="..."` so
    // native MCP clients can run discovery against the AS without
    // pre-configuration.
    let h = start(false).await;
    let resp = client()
        .post(format!("http://{}/__api/mcp", h.addr))
        // No Authorization header — should trigger the discovery hint.
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
    let www = resp
        .headers()
        .get("www-authenticate")
        .expect("WWW-Authenticate header present")
        .to_str()
        .expect("header is ascii");
    assert!(
        www.starts_with("Bearer"),
        "WWW-Authenticate should start with Bearer: {www}"
    );
    assert!(
        www.contains("resource_metadata="),
        "WWW-Authenticate should carry resource_metadata: {www}"
    );
    assert!(
        www.contains("/.well-known/oauth-protected-resource"),
        "resource_metadata URL should point at the well-known: {www}"
    );
}

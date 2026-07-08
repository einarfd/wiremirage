//! Tier-2 end-to-end test for generic OIDC login (ADR-0035).
//!
//! Boots a real wm-host plus a small in-process axum server that
//! plays the IdP: serves `/.well-known/openid-configuration`,
//! `/token`, and `/userinfo`. The OidcProvider wired into the host
//! points at the mock so no network is involved.
//!
//! What this exercises:
//!   * `OidcConfig::discover()` against a real (mock) discovery
//!     document, including the issuer-mismatch refusal.
//!   * `/auth/start/oidc` → 302 carrying state + PKCE challenge.
//!   * `/auth/callback/oidc` with a valid state + code → token
//!     exchange (the mock rejects a missing `code_verifier`, so the
//!     happy path proves the PKCE round-trip) → userinfo → allow-rule
//!     pass → user upsert → session cookie → 302 to `next`.
//!   * State replay → 400 (one-shot).
//!   * `error=access_denied` callback → 401.
//!
//! Allow-rule denial, unverified-email handling, and claim parsing
//! are unit-tested in `oidc::tests`; we don't re-walk the whole HTTP
//! flow for them here (same split as `github_oauth_e2e.rs`).

use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::oidc::OidcConfig;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::{COOKIE_NAME, SessionStore};
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"this-is-a-thirty-two-byte-secret";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
    _mock_issuer: MockIssuer,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct MockIssuer {
    base: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockIssuer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Boot a mock IdP whose `/userinfo` returns `claims`. The discovery
/// document advertises `issuer_override` when given (for the
/// issuer-mismatch test), else the mock's own base URL.
async fn start_mock_issuer(
    claims: serde_json::Value,
    issuer_override: Option<String>,
) -> MockIssuer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock issuer");
    let base = format!("http://{}", listener.local_addr().expect("local_addr"));
    let issuer = issuer_override.unwrap_or_else(|| base.clone());

    let discovery = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "userinfo_endpoint": format!("{base}/userinfo"),
        "jwks_uri": format!("{base}/jwks"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    });

    async fn token(form: String) -> axum::response::Response {
        // Reject a missing PKCE verifier so the happy-path e2e test
        // proves the verifier survived the state round-trip. Real
        // values aren't validated — that's the IdP's job, not the
        // mock's.
        if !form.contains("code_verifier=") {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "invalid_grant", "error_description": "missing code_verifier" })),
            )
                .into_response();
        }
        if !form.contains("grant_type=authorization_code") {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "unsupported_grant_type" })),
            )
                .into_response();
        }
        axum::Json(json!({
            "access_token": "mock_access_token_v1",
            "token_type": "Bearer",
            "expires_in": 3600
        }))
        .into_response()
    }

    let userinfo_claims = Arc::new(claims);
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let doc = discovery.clone();
                async move { axum::Json(doc) }
            }),
        )
        .route("/token", post(token))
        .route(
            "/userinfo",
            get(move || {
                let claims = userinfo_claims.clone();
                async move { axum::Json((*claims).clone()) }
            }),
        );

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock issuer serve");
    });
    MockIssuer { base, server }
}

fn config_for(issuer: &str) -> OidcConfig {
    OidcConfig {
        issuer: issuer.to_string(),
        client_id: "test-cid".into(),
        client_secret: "test-csec".into(),
        display_name: "Pocket ID".into(),
        allow_emails: vec![],
        allow_domains: vec!["kindly.example".into()],
        allow_groups: vec![],
        admin_emails: vec!["einar@kindly.example".into()],
        admin_groups: vec![],
        groups_claim: "groups".into(),
        extra_scopes: vec![],
    }
}

/// Boot a wm-host wired to a mock issuer returning `claims` from
/// userinfo. Uses `discover()` for the endpoint resolution so every
/// harness run also exercises the discovery path.
async fn start(claims: serde_json::Value) -> Harness {
    let mock_issuer = start_mock_issuer(claims, None).await;

    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());

    let provider = config_for(&mock_issuer.base)
        .discover()
        .await
        .expect("discovery against mock issuer");

    let state = AppState::new(runtime, routes, auth, journal)
        .with_sessions(SessionStore::new(storage, SECRET).expect("session store"))
        .with_oidc(provider);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind host");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    Harness {
        addr,
        server,
        _mock_issuer: mock_issuer,
    }
}

fn einar_claims() -> serde_json::Value {
    json!({
        "sub": "pid-42",
        "preferred_username": "einar",
        "name": "Einar F",
        "email": "einar@kindly.example",
        "email_verified": true,
        "groups": ["mockers"]
    })
}

fn no_redirect_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("build client")
}

fn url(h: &Harness, path: &str) -> String {
    format!("http://{}{}", h.addr, path)
}

fn pick_cookie_value(resp: &reqwest::Response, name: &str) -> Option<String> {
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        let s = v.to_str().ok()?;
        for pair in s.split(';') {
            let trimmed = pair.trim();
            if let Some(rest) = trimmed.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

#[tokio::test]
async fn discovery_resolves_endpoints() {
    let mock = start_mock_issuer(einar_claims(), None).await;
    let provider = config_for(&mock.base).discover().await.expect("discover");
    assert_eq!(
        provider.endpoints.authorization_endpoint,
        format!("{}/authorize", mock.base)
    );
    assert_eq!(
        provider.endpoints.token_endpoint,
        format!("{}/token", mock.base)
    );
    assert_eq!(
        provider.endpoints.userinfo_endpoint,
        format!("{}/userinfo", mock.base)
    );
    assert!(provider.endpoints.token_auth_basic);
}

#[tokio::test]
async fn discovery_rejects_issuer_mismatch() {
    let mock = start_mock_issuer(einar_claims(), Some("https://other.example".into())).await;
    let err = config_for(&mock.base)
        .discover()
        .await
        .expect_err("issuer mismatch must refuse");
    let msg = format!("{err:#}");
    assert!(msg.contains("issuer mismatch"), "{msg}");
    assert!(msg.contains("other.example"), "{msg}");
}

#[tokio::test]
async fn start_oidc_redirects_with_state_and_pkce() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/auth/start/oidc"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303);
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(location.contains("/authorize"), "Location was {location}");
    assert!(location.contains("state="), "carries state: {location}");
    assert!(
        location.contains("code_challenge="),
        "carries PKCE challenge: {location}"
    );
    assert!(
        location.contains("code_challenge_method=S256"),
        "S256 method: {location}"
    );
    assert!(
        location.contains("scope=openid+profile+email"),
        "OIDC scopes: {location}"
    );
}

#[tokio::test]
async fn full_callback_flow_mints_session_and_creates_user() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();

    let start_resp = client
        .get(url(&h, "/auth/start/oidc?next=/ui/groups"))
        .send()
        .await
        .expect("send start");
    assert_eq!(start_resp.status(), 303);
    let location = start_resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap();
    let state = reqwest::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find_map(|(k, v)| (k == "state").then(|| v.into_owned()))
        .expect("state in authorize URL");

    // Invoke the callback as the IdP would. The mock token endpoint
    // rejects a missing code_verifier, so a 303 here proves the PKCE
    // verifier made the round-trip through the server-side state.
    let cb = client
        .get(url(
            &h,
            &format!("/auth/callback/oidc?code=fakecode&state={state}"),
        ))
        .send()
        .await
        .expect("send callback");
    assert_eq!(
        cb.status(),
        303,
        "callback should 303 on success; got {} body: {}",
        cb.status(),
        cb.text().await.unwrap_or_default()
    );
    let location = cb
        .headers()
        .get("location")
        .expect("redirect Location")
        .to_str()
        .unwrap();
    assert_eq!(location, "/ui/groups", "post-login redirect honours next");
    let cookie = pick_cookie_value(&cb, COOKIE_NAME).expect("session cookie set");
    assert!(!cookie.is_empty());

    // The session actually authenticates: /api/users/me resolves to
    // the provisioned user, named after preferred_username, admin via
    // WM_OIDC_ADMIN_EMAILS.
    let me = client
        .get(url(&h, "/api/users/me"))
        .header("cookie", format!("{COOKIE_NAME}={cookie}"))
        .send()
        .await
        .expect("send me");
    assert_eq!(me.status(), 200);
    let body: serde_json::Value = me.json().await.expect("json");
    assert_eq!(body["name"], "einar");
    assert_eq!(body["is_admin"], true, "admin email promotes: {body}");
}

#[tokio::test]
async fn state_is_one_shot() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();

    let start_resp = client
        .get(url(&h, "/auth/start/oidc"))
        .send()
        .await
        .expect("send start");
    let location = start_resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap();
    let state = reqwest::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find_map(|(k, v)| (k == "state").then(|| v.into_owned()))
        .expect("state in authorize URL");

    let first = client
        .get(url(
            &h,
            &format!("/auth/callback/oidc?code=fakecode&state={state}"),
        ))
        .send()
        .await
        .expect("first callback");
    assert_eq!(first.status(), 303);

    let replay = client
        .get(url(
            &h,
            &format!("/auth/callback/oidc?code=fakecode&state={state}"),
        ))
        .send()
        .await
        .expect("replayed callback");
    assert_eq!(replay.status(), 400, "state replay is rejected");
}

#[tokio::test]
async fn callback_with_unknown_state_is_rejected() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();
    let cb = client
        .get(url(&h, "/auth/callback/oidc?code=x&state=nonsense"))
        .send()
        .await
        .expect("send");
    assert_eq!(cb.status(), 400);
    let body = cb.text().await.unwrap_or_default();
    assert!(body.contains("state"), "error body mentions state: {body}");
}

#[tokio::test]
async fn callback_surfaces_idp_error() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();
    let cb = client
        .get(url(
            &h,
            "/auth/callback/oidc?error=access_denied&error_description=user+cancelled",
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(cb.status(), 401);
    let body = cb.text().await.unwrap_or_default();
    assert!(body.contains("access_denied"), "names the error: {body}");
}

#[tokio::test]
async fn login_page_shows_oidc_button_when_configured() {
    let h = start(einar_claims()).await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/auth/login"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Continue with Pocket ID"),
        "OIDC button rendered with display name"
    );
    assert!(
        body.contains("/auth/start/oidc"),
        "OIDC button links to start endpoint"
    );
}

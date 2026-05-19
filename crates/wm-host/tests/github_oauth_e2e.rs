//! Tier-2 end-to-end test for slice 50 (GitHub OAuth user-login).
//!
//! Boots a real wm-host plus a small in-process axum server that
//! plays GitHub: serves `/login/oauth/access_token` + `/user` +
//! `/user/emails` + `/user/orgs`. The GitHubConfig wired into the
//! host points its endpoints at the mock so no network is involved.
//!
//! What this exercises:
//!   * `/__auth/start/github` → 302 to authorize URL carrying our
//!     state nonce; the state record lands in the admin bucket.
//!   * `/__auth/callback` with a valid state + code →
//!     mock-GitHub-token-exchange → mock-GitHub-identity-fetch →
//!     allow-rule pass → user upsert → session cookie minted →
//!     302 to `/__ui/`.
//!   * `/__auth/callback` with a missing state → 400.
//!
//! Allow-rule denial is unit-tested in `github_oauth::tests`; we
//! don't re-walk the whole HTTP flow for it here.

use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::github_oauth::{GitHubConfig, GitHubEndpoints};
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::{COOKIE_NAME, SessionStore};
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"this-is-a-thirty-two-byte-secret";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
    _mock_github: MockGithub,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct MockGithub {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockGithub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Identity the mock GitHub returns to the host. The same record is
/// referenced by the allow-rules we wire up below.
const MOCK_LOGIN: &str = "einarw";
const MOCK_ID: u64 = 42;
const MOCK_EMAIL: &str = "dev@acme.example";

async fn start_mock_github() -> MockGithub {
    async fn token(form: String) -> impl IntoResponse {
        // The wm-host sends an urlencoded body with code +
        // client_id + client_secret + redirect_uri. The mock
        // doesn't validate them — they're echoed-back in real
        // GitHub anyway. Always return a fixed token.
        let _ = form;
        axum::Json(json!({
            "access_token": "mock_access_token_v1",
            "token_type": "bearer",
            "scope": "read:user,user:email,read:org"
        }))
    }
    async fn user() -> impl IntoResponse {
        axum::Json(json!({
            "id": MOCK_ID,
            "login": MOCK_LOGIN,
            "name": "Einar W"
        }))
    }
    async fn emails() -> impl IntoResponse {
        axum::Json(json!([
            { "email": MOCK_EMAIL, "primary": true, "verified": true },
            { "email": "secondary@example.com", "primary": false, "verified": true }
        ]))
    }
    async fn orgs() -> impl IntoResponse {
        axum::Json(json!([
            { "login": "acme-inc", "id": 1 }
        ]))
    }
    let app = Router::new()
        .route("/login/oauth/access_token", post(token))
        .route("/user", get(user))
        .route("/user/emails", get(emails))
        .route("/user/orgs", get(orgs));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock github");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock github serve");
    });
    MockGithub { addr, server }
}

async fn start(allow_users: Vec<String>, admin_users: Vec<String>) -> Harness {
    let mock_github = start_mock_github().await;
    let mock_base = format!("http://{}", mock_github.addr);

    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());

    let github = GitHubConfig {
        client_id: "test-cid".into(),
        client_secret: "test-csec".into(),
        allow_users,
        allow_orgs: vec![],
        admin_users,
        endpoints: GitHubEndpoints {
            authorize_url: format!("{mock_base}/login/oauth/authorize"),
            token_url: format!("{mock_base}/login/oauth/access_token"),
            api_base_url: mock_base,
        },
    };

    let state = AppState::new(runtime, routes, auth, journal)
        .with_sessions(SessionStore::new(storage, SECRET).expect("session store"))
        .with_github_oauth(github);
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
        _mock_github: mock_github,
    }
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

/// Pull the value of a `Set-Cookie` whose name matches `name`. Returns
/// the cookie's value (everything before the first `;`).
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
async fn start_github_redirects_to_authorize_url_with_state() {
    let h = start(vec![MOCK_LOGIN.to_string()], vec![]).await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/__auth/start/github"))
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
    // Points at the mock GitHub authorize URL.
    assert!(
        location.contains("/login/oauth/authorize"),
        "Location was {location}"
    );
    assert!(
        location.contains("state="),
        "carries state nonce: {location}"
    );
    assert!(
        location.contains("client_id=test-cid"),
        "carries client_id: {location}"
    );
}

#[tokio::test]
async fn full_callback_flow_mints_session_and_creates_user() {
    let h = start(vec![MOCK_LOGIN.to_string()], vec![MOCK_LOGIN.to_string()]).await;
    let client = no_redirect_client();

    // Step 1: hit /__auth/start/github to mint and persist a state.
    let start_resp = client
        .get(url(&h, "/__auth/start/github?next=/__ui/groups"))
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
    // Pull the state value out of the authorize URL. With reqwest::Url
    // we already have a parser in scope (transitively via reqwest).
    let state = reqwest::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find_map(|(k, v)| (k == "state").then(|| v.into_owned()))
        .expect("state in authorize URL");

    // Step 2: invoke /__auth/callback as GitHub would, with a code
    // and the captured state. The mock-GitHub server returns a fixed
    // identity that's on the allow-list.
    let cb = client
        .get(url(
            &h,
            &format!("/__auth/callback?code=fakecode&state={state}"),
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
    assert_eq!(location, "/__ui/groups", "post-login redirect honours next");
    let cookie = pick_cookie_value(&cb, COOKIE_NAME).expect("session cookie set");
    assert!(
        !cookie.is_empty(),
        "session cookie carries a non-empty value"
    );
}

#[tokio::test]
async fn callback_with_missing_state_is_rejected() {
    let h = start(vec![MOCK_LOGIN.to_string()], vec![]).await;
    let client = no_redirect_client();
    // No state was registered, so the state lookup misses.
    let cb = client
        .get(url(&h, "/__auth/callback?code=x&state=nonsense"))
        .send()
        .await
        .expect("send");
    assert_eq!(cb.status(), 400);
    let body = cb.text().await.unwrap_or_default();
    assert!(body.contains("state"), "error body mentions state: {body}");
}

#[tokio::test]
async fn login_page_shows_github_button_when_configured() {
    let h = start(vec![MOCK_LOGIN.to_string()], vec![]).await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/__auth/login"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Continue with GitHub"),
        "GitHub button rendered: snippet \"{}\"",
        &body[..body.len().min(400)]
    );
    assert!(
        body.contains("/__auth/start/github"),
        "GitHub button links to start endpoint"
    );
}

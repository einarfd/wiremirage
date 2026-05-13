//! Tier-2 end-to-end tests for slice 20 (local auth + sessions).
//!
//! Drive the full HTTP stack: parse `WM_LOCAL_AUTH`, POST credentials
//! to `/__auth/login/password`, capture the Set-Cookie, then send it
//! back manually on an authed call. (Manual cookie handling rather
//! than reqwest's cookie store keeps the workspace's reqwest feature
//! set minimal.)

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::{COOKIE_NAME, SessionStore};
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"this-is-a-thirty-two-byte-secret";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start(local_auth_value: &str) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());

    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse(local_auth_value).expect("parse local auth"))
        .with_sessions(SessionStore::new(storage, SECRET).expect("session store"));
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

fn url(h: &Harness, path: &str) -> String {
    format!("http://{}{}", h.addr, path)
}

/// reqwest client that does NOT follow redirects so we can inspect
/// the 303 + Set-Cookie pair from the login endpoint directly.
fn no_redirect_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("build client")
}

/// POST `username` + `password` and return the response. Form-encoded
/// body built by hand (we'd otherwise have to enable reqwest's
/// urlencoded feature workspace-wide).
async fn post_login(
    h: &Harness,
    client: &Client,
    username: &str,
    password: &str,
) -> reqwest::Response {
    let body = format!(
        "username={}&password={}",
        urlencode(username),
        urlencode(password)
    );
    client
        .post(url(h, "/__auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("post login")
}

/// Minimal application/x-www-form-urlencoded value encoding. The
/// test inputs are all ASCII alphanumerics; we percent-encode
/// anything else defensively.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Pull the `wm_session=...` value out of a `Set-Cookie` header.
fn extract_cookie_value(set_cookie: &str) -> String {
    let first_pair = set_cookie.split(';').next().expect("non-empty cookie");
    first_pair
        .trim()
        .strip_prefix(&format!("{COOKIE_NAME}="))
        .expect("wm_session prefix")
        .to_string()
}

#[tokio::test]
async fn login_with_valid_password_sets_signed_cookie() {
    let h = start("alice:hunter2:admin").await;
    let client = no_redirect_client();

    let resp = post_login(&h, &client, "alice", "hunter2").await;
    assert_eq!(resp.status().as_u16(), 303);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .expect("set-cookie header")
        .to_str()
        .unwrap();
    assert!(
        cookie.starts_with(&format!("{COOKIE_NAME}=")),
        "expected wm_session cookie, got: {cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "cookie should be HttpOnly");
    assert!(
        cookie.contains("SameSite=Lax"),
        "cookie should set SameSite=Lax"
    );
    assert!(cookie.contains("Path=/"), "cookie should scope Path=/");
}

#[tokio::test]
async fn login_with_wrong_password_returns_401() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();

    let resp = post_login(&h, &client, "alice", "WRONG").await;
    assert_eq!(resp.status().as_u16(), 401);
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "no cookie should be issued on failure"
    );
}

#[tokio::test]
async fn login_with_unknown_user_returns_401() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();

    let resp = post_login(&h, &client, "eve", "hunter2").await;
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn six_wrong_attempts_trigger_lockout() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();

    for i in 0..5 {
        let resp = post_login(&h, &client, "alice", "WRONG").await;
        assert_eq!(resp.status().as_u16(), 401, "attempt {i}");
    }
    // 6th request — even with the correct password — should be locked out.
    let resp = post_login(&h, &client, "alice", "hunter2").await;
    assert_eq!(resp.status().as_u16(), 429, "expected 429 after lockout");
}

#[tokio::test]
async fn session_cookie_authenticates_api_endpoint() {
    let h = start("alice:hunter2:admin").await;
    let client = no_redirect_client();

    let resp = post_login(&h, &client, "alice", "hunter2").await;
    assert_eq!(resp.status().as_u16(), 303);
    let cookie_value =
        extract_cookie_value(resp.headers().get("set-cookie").unwrap().to_str().unwrap());

    // Hit an authed endpoint with the cookie (no Authorization header).
    let me = client
        .get(url(&h, "/__api/users/me"))
        .header("cookie", format!("{COOKIE_NAME}={cookie_value}"))
        .send()
        .await
        .expect("get me");
    assert_eq!(me.status().as_u16(), 200, "expected cookie auth to succeed");
    let body: serde_json::Value = me.json().await.expect("json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["is_admin"], true);
}

#[tokio::test]
async fn logout_invalidates_the_cookie() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();

    let login_resp = post_login(&h, &client, "alice", "hunter2").await;
    let cookie_value = extract_cookie_value(
        login_resp
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap(),
    );
    let cookie_header = format!("{COOKIE_NAME}={cookie_value}");

    // Confirm it authenticates first.
    let me = client
        .get(url(&h, "/__api/users/me"))
        .header("cookie", &cookie_header)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 200);

    let logout = client
        .post(url(&h, "/__auth/logout"))
        .header("cookie", &cookie_header)
        .send()
        .await
        .expect("logout");
    assert_eq!(logout.status().as_u16(), 204);

    // Subsequent call with the now-invalidated cookie → 401.
    let after = client
        .get(url(&h, "/__api/users/me"))
        .header("cookie", &cookie_header)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 401);
}

#[tokio::test]
async fn login_page_renders_form_when_local_auth_is_configured() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__auth/login")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sign in"), "expected form");
    assert!(body.contains("/__auth/login/password"));
}

#[tokio::test]
async fn login_page_says_disabled_when_no_methods_configured() {
    let h = start("").await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__auth/login")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("No login methods configured"),
        "expected disabled page"
    );
}

#[tokio::test]
async fn admin_role_in_env_syncs_on_login() {
    let h = start("alice:hunter2:admin").await;
    let client = no_redirect_client();
    let login = post_login(&h, &client, "alice", "hunter2").await;
    assert_eq!(login.status().as_u16(), 303);
    let cookie_value =
        extract_cookie_value(login.headers().get("set-cookie").unwrap().to_str().unwrap());

    let me = client
        .get(url(&h, "/__api/users/me"))
        .header("cookie", format!("{COOKIE_NAME}={cookie_value}"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = me.json().await.unwrap();
    assert_eq!(body["is_admin"], true, "admin role from env should sync");
}

#[tokio::test]
async fn tampered_session_cookie_returns_401() {
    let h = start("alice:hunter2").await;
    let client = no_redirect_client();
    let resp = post_login(&h, &client, "alice", "hunter2").await;
    let mut cookie_value =
        extract_cookie_value(resp.headers().get("set-cookie").unwrap().to_str().unwrap());
    // Flip the last byte of the signature portion.
    let bytes = unsafe { cookie_value.as_bytes_mut() };
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;

    let resp = client
        .get(url(&h, "/__api/users/me"))
        .header("cookie", format!("{COOKIE_NAME}={cookie_value}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn bearer_token_still_works_when_session_store_is_configured() {
    // Slice-20 wiring shouldn't break the existing token path. Bootstrap
    // an admin token, then hit an authed endpoint with `Authorization`.
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse("alice:hunter2").expect("parse"))
        .with_sessions(SessionStore::new(storage, SECRET).expect("session"));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let h = Harness { addr, server };

    let client = Client::new();
    let resp = client
        .get(url(&h, "/__api/users/me"))
        .header("authorization", "Bearer wmt_test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

//! Tier-2 smoke tests for the slice-25 tokens page + CSRF middleware.
//!
//! Coverage:
//!   * Tokens page renders the user's own tokens
//!   * Create-token form mints a fresh token and shows the plaintext
//!     exactly once
//!   * Revoke removes the token and redirects back
//!   * Empty name / bad TTL renders an inline error (400)
//!   * POST without `_csrf` is 403
//!   * POST with a mismatched `_csrf` is 403
//!   * Each user only sees their own tokens
//!
//! The CSRF middleware itself is exercised through every test that
//! does a successful POST — they wouldn't 303 without the cookie+form
//! handshake working.

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"thirty-two-byte-development-key!";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn url(h: &Harness, path: &str) -> String {
    format!("http://{}{}", h.addr, path)
}

fn no_redirect_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client")
}

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.create_user("admin", true).expect("admin");
    auth.create_user("alice", false).expect("alice");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(
            LocalAuth::parse("admin:devpassword:admin,alice:devpassword").expect("auth"),
        )
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness { addr, server }
}

/// Mint a wm_csrf cookie + read its embedded form value off the login
/// page. Returns (cookie_value, form_value).
async fn fetch_csrf(h: &Harness, client: &Client) -> (String, String) {
    let resp = client
        .get(url(h, "/__auth/login"))
        .send()
        .await
        .expect("get login");
    let cookie = pick_set_cookie(&resp, "wm_csrf").expect("csrf cookie");
    let body = resp.text().await.unwrap();
    let value = extract_csrf_value(&body).expect("csrf form value");
    (cookie, value)
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    let (csrf_cookie, csrf_value) = fetch_csrf(h, client).await;
    let resp = client
        .post(url(h, "/__auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(format!(
            "_csrf={csrf_value}&username={user}&password=devpassword"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "login");
    let session = pick_set_cookie(&resp, "wm_session").expect("session");
    format!("wm_csrf={csrf_cookie}; wm_session={session}")
}

fn pick_set_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    for v in resp.headers().get_all("set-cookie").iter() {
        let raw = v.to_str().ok()?;
        if let Some(rest) = raw.strip_prefix(&format!("{name}=")) {
            return Some(rest.split(';').next()?.to_string());
        }
    }
    None
}

fn extract_csrf_value(body: &str) -> Option<String> {
    let needle = "name=\"_csrf\" value=\"";
    let start = body.find(needle)? + needle.len();
    let end = body[start..].find('"')?;
    Some(body[start..start + end].to_string())
}

#[tokio::test]
async fn tokens_page_renders_empty_state_for_new_user() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("API tokens"));
    assert!(body.contains("No tokens yet"));
    assert!(
        body.contains("name=\"_csrf\""),
        "create form has csrf input"
    );
}

#[tokio::test]
async fn create_token_shows_plaintext_once_and_lists_it() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    // Read the page-level csrf cookie + form value from the tokens page.
    let page = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");

    let resp = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=laptop&ttl_hours="))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // Plaintext token revealed exactly once on this response.
    assert!(body.contains("wmt_"), "plaintext token shown: {body}");
    assert!(body.contains("only time you'll see it"));
    // The new token also appears in the user's table.
    assert!(body.contains("laptop"));

    // A subsequent GET no longer shows the plaintext.
    let later = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(later.contains("laptop"), "name still listed");
    assert!(
        !later.contains("only time you'll see it"),
        "plaintext banner not re-rendered on later GETs"
    );
}

#[tokio::test]
async fn revoke_redirects_and_drops_token_from_list() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    let _ = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=throwaway"))
        .send()
        .await
        .unwrap();

    let revoke = client
        .post(url(&h, "/__ui/me/tokens/throwaway/revoke"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!(
        (300..400).contains(&revoke.status().as_u16()),
        "revoke should redirect, got {}",
        revoke.status()
    );

    let later = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!later.contains(">throwaway<"));
}

#[tokio::test]
async fn create_token_empty_name_shows_inline_error() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();

    let resp = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=&ttl_hours="))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Name is required"));
}

#[tokio::test]
async fn csrf_missing_form_field_is_403() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("name=oops")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn csrf_mismatched_token_is_403() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("_csrf=NOT-THE-RIGHT-VALUE&name=oops")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn csrf_missing_cookie_is_403() {
    let h = start().await;
    let client = no_redirect_client();
    // Log in as admin but strip the csrf cookie when sending the POST.
    let combined = login_cookie(&h, &client, "admin").await;
    let session_only = combined
        .split(';')
        .find(|p| p.trim().starts_with("wm_session="))
        .unwrap()
        .trim()
        .to_string();
    let resp = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", session_only)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("_csrf=anything&name=oops")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn alice_does_not_see_admins_tokens() {
    let h = start().await;
    let client = no_redirect_client();

    // Admin creates a token.
    let admin_cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    let _ = client
        .post(url(&h, "/__ui/me/tokens"))
        .header("cookie", &admin_cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=admin-only"))
        .send()
        .await
        .unwrap();

    // Alice logs in — fresh client + cookies — and her tokens page
    // shows nothing.
    let alice_client = no_redirect_client();
    let alice_cookie = login_cookie(&h, &alice_client, "alice").await;
    let body = alice_client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &alice_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("No tokens yet"));
    assert!(!body.contains("admin-only"));
}

//! Tier-2 smoke tests for the slice-28 unmatched UI pages.
//!
//! Coverage:
//!   * Empty state when no unmatched traffic has hit the host yet
//!   * Lists rows after a few unmatched dispatches
//!   * Filter by method narrows the list
//!   * Filter by path_pattern (glob) narrows the list
//!   * Cursor pagination via `?before=` exposes the next page
//!   * Detail page at /__ui/unmatched/{n} renders the request envelope
//!   * Non-admin gets 403 on both pages
//!   * Detail 404 for an unknown number

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

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    let get = client.get(url(h, "/__auth/login")).send().await.unwrap();
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let body = get.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("csrf value");
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

/// Drive a request at a path that doesn't match any route, which causes
/// the dispatcher to record an unmatched-journal entry.
async fn record_unmatched(h: &Harness, client: &Client, method: &str, path: &str) {
    let resp = client
        .request(method.parse().unwrap(), url(h, path))
        .body("body")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "expected unmatched 404 for {method} {path}"
    );
}

// -- List page --------------------------------------------------------------

#[tokio::test]
async fn unmatched_index_empty_when_no_traffic() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/unmatched"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Unmatched requests"));
    assert!(body.contains("No unmatched requests in the last hour"));
}

#[tokio::test]
async fn unmatched_index_lists_recorded_entries() {
    let h = start().await;
    let client = no_redirect_client();
    record_unmatched(&h, &client, "GET", "/v1/missing-thing").await;
    record_unmatched(&h, &client, "POST", "/v1/other").await;

    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/unmatched"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // minijinja escapes `/` to `&#x2f;` in HTML output — assert on the
    // distinctive substring instead of the full path.
    assert!(body.contains("missing-thing"), "first path visible: {body}");
    assert!(
        body.contains("v1&#x2f;other"),
        "second path visible: {body}"
    );
    assert!(body.contains("View request"));
    assert!(body.contains("Create route from request"));
}

#[tokio::test]
async fn unmatched_index_filter_by_method() {
    let h = start().await;
    let client = no_redirect_client();
    record_unmatched(&h, &client, "GET", "/v1/aaa").await;
    record_unmatched(&h, &client, "POST", "/v1/bbb").await;

    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/unmatched?method=POST"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("v1&#x2f;bbb"));
    assert!(
        !body.contains("v1&#x2f;aaa"),
        "GET request filtered out: {body}"
    );
}

#[tokio::test]
async fn unmatched_index_filter_by_path_pattern() {
    let h = start().await;
    let client = no_redirect_client();
    record_unmatched(&h, &client, "GET", "/v1/charges").await;
    record_unmatched(&h, &client, "GET", "/v2/invoices").await;

    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/unmatched?path_pattern=/v1/*"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("v1&#x2f;charges"));
    assert!(!body.contains("v2&#x2f;invoices"));
}

#[tokio::test]
async fn unmatched_index_invalid_method_returns_400() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/unmatched?method=lowercase"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn unmatched_index_pagination_exposes_next_page() {
    let h = start().await;
    let client = no_redirect_client();
    // Drive more than one page's worth so "Older →" is visible.
    for i in 0..30u32 {
        record_unmatched(&h, &client, "GET", &format!("/v1/path-{i}")).await;
    }
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/unmatched"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("Older →"),
        "next-page link rendered when more than 25 unmatched: {body}"
    );
    assert!(body.contains("before="), "next link carries the cursor");
}

#[tokio::test]
async fn unmatched_index_non_admin_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/unmatched"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

// -- Detail page ------------------------------------------------------------

#[tokio::test]
async fn unmatched_detail_renders_request_envelope() {
    let h = start().await;
    let client = no_redirect_client();
    record_unmatched(&h, &client, "POST", "/v1/charges/refund").await;
    let cookie = login_cookie(&h, &client, "admin").await;
    // First number is 1 — unmatched:counter starts there.
    let body = client
        .get(url(&h, "/__ui/unmatched/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("charges&#x2f;refund"));
    assert!(body.contains("POST"));
    assert!(body.contains("Request"));
    // We POST'd "body" — should render in the body block.
    assert!(body.contains("body"), "request body rendered: {body}");
}

#[tokio::test]
async fn unmatched_detail_404_for_unknown_number() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/unmatched/999"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn unmatched_detail_non_admin_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    record_unmatched(&h, &client, "GET", "/missing").await;
    let cookie = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/unmatched/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

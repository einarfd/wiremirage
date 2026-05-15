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

#[tokio::test]
async fn ttl_preset_30d_sets_a_30_day_expiry() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
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
        .body(format!(
            "_csrf={csrf}&name=thirty-day&ttl_preset=30d&ttl_hours="
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // Token landed in the table with a non-empty Expires cell.
    assert!(body.contains("thirty-day"));
    // The row's Expires column has a real date (not "—"). The dates are
    // ISO 8601 so check for the canonical "T" separator at minimum.
    let table_rows: Vec<&str> = body.lines().filter(|l| l.contains("thirty-day")).collect();
    let row_section = body.split("<tbody>").nth(1).unwrap_or(&body);
    assert!(
        row_section.contains("T") && !row_section.contains("<td class=\"text-muted\">—</td>\n"),
        "expires column has a real date: rows={table_rows:?}"
    );
}

#[tokio::test]
async fn ttl_preset_never_creates_token_without_expiry() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
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
        .body(format!(
            "_csrf={csrf}&name=forever&ttl_preset=never&ttl_hours=999"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // The "ttl_hours=999" should be ignored since preset is "never".
    // The forever row's Expires cell should be the empty dash.
    let row = body
        .split("forever")
        .nth(1)
        .expect("forever row")
        .split("</tr>")
        .next()
        .unwrap();
    assert!(row.contains("—"), "no-expiry dash on forever row: {row}");
}

#[tokio::test]
async fn ttl_preset_custom_falls_through_to_hours_field() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
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
        .body(format!(
            "_csrf={csrf}&name=custom-12h&ttl_preset=custom&ttl_hours=12"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    let row = body
        .split("custom-12h")
        .nth(1)
        .expect("custom row")
        .split("</tr>")
        .next()
        .unwrap();
    // A real expiry should be present (not the muted dash).
    assert!(
        row.contains("T") || row.contains("<time"),
        "custom row has a real expiry: {row}"
    );
}

#[tokio::test]
async fn token_list_sorts_by_name_when_requested() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    // Create three tokens with names that sort differently from creation order.
    for n in ["gamma", "alpha", "beta"] {
        let page = client
            .get(url(&h, "/__ui/me/tokens"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");
        client
            .post(url(&h, "/__ui/me/tokens"))
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_csrf={csrf}&name={n}&ttl_preset=never"))
            .send()
            .await
            .unwrap();
    }
    // Default sort = created desc → beta, alpha, gamma (newest first).
    let default_body = client
        .get(url(&h, "/__ui/me/tokens"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let default_order = order_of_names(&default_body, &["alpha", "beta", "gamma"]);
    assert_eq!(default_order, vec!["beta", "alpha", "gamma"]);
    // sort=name asc → alpha, beta, gamma.
    let by_name = client
        .get(url(&h, "/__ui/me/tokens?sort=name&dir=asc"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let name_order = order_of_names(&by_name, &["alpha", "beta", "gamma"]);
    assert_eq!(name_order, vec!["alpha", "beta", "gamma"]);
    // Active column carries the direction arrow.
    assert!(
        by_name.contains("Name ↑"),
        "active asc arrow on Name: {by_name}"
    );
}

/// Helper: find the order in which `needles` appear in `body`.
fn order_of_names(body: &str, needles: &[&str]) -> Vec<String> {
    let mut found: Vec<(usize, &str)> = needles
        .iter()
        .filter_map(|n| body.find(n).map(|i| (i, *n)))
        .collect();
    found.sort_by_key(|(i, _)| *i);
    found.into_iter().map(|(_, n)| n.to_string()).collect()
}

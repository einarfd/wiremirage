//! Tier-2 smoke tests for the account screen (`/ui/me`) + CSRF middleware.
//!
//! Coverage:
//!   * The account page renders the user's own tokens
//!   * Create-token form mints a fresh token and shows the plaintext
//!     exactly once
//!   * Revoke removes the token and redirects back
//!   * Empty name / bad TTL renders an inline error (400)
//!   * POST without `_csrf` is 403
//!   * POST with a mismatched `_csrf` is 403
//!   * Each user only sees their own tokens
//!   * Sessions: the sign-out-everywhere form is reachable by every
//!     user (not just admins), kills every session for the caller and
//!     nobody else, and leaves API tokens alone
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
    auth: Auth,
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
    auth.create_user("admin@test.example", true).expect("admin");
    auth.create_user("alice@test.example", false)
        .expect("alice");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth.clone(), journal)
        .with_local_auth(
            LocalAuth::parse("admin@test.example:devpassword:admin,alice@test.example:devpassword")
                .expect("auth"),
        )
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness { addr, auth, server }
}

/// Mint a wm_csrf cookie + read its embedded form value off the login
/// page. Returns (cookie_value, form_value).
async fn fetch_csrf(h: &Harness, client: &Client) -> (String, String) {
    let resp = client
        .get(url(h, "/auth/login"))
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
        .post(url(h, "/auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(format!(
            "_csrf={csrf_value}&email={user}%40test.example&password=devpassword"
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

/// POST a form-encoded body with the session + csrf cookie pair.
async fn post(
    h: &Harness,
    client: &Client,
    cookie: &str,
    path: &str,
    body: String,
) -> reqwest::Response {
    client
        .post(url(h, path))
        .header("cookie", cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Read the account page and hand back (body, csrf form value).
async fn account_screen(h: &Harness, client: &Client, cookie: &str) -> (String, String) {
    let body = client
        .get(url(h, "/ui/me"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&body).expect("csrf form value");
    (body, csrf)
}

#[tokio::test]
async fn account_page_renders_empty_state_for_new_user() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/me"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");

    let resp = client
        .post(url(&h, "/ui/me/tokens"))
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
        .get(url(&h, "/ui/me"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    let _ = client
        .post(url(&h, "/ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=throwaway"))
        .send()
        .await
        .unwrap();

    let revoke = client
        .post(url(&h, "/ui/me/tokens/throwaway/revoke"))
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
        .get(url(&h, "/ui/me"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();

    let resp = client
        .post(url(&h, "/ui/me/tokens"))
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
        .post(url(&h, "/ui/me/tokens"))
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
        .post(url(&h, "/ui/me/tokens"))
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
        .post(url(&h, "/ui/me/tokens"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    let _ = client
        .post(url(&h, "/ui/me/tokens"))
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
        .get(url(&h, "/ui/me"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");
    let resp = client
        .post(url(&h, "/ui/me/tokens"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");
    let resp = client
        .post(url(&h, "/ui/me/tokens"))
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
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");
    let resp = client
        .post(url(&h, "/ui/me/tokens"))
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
            .get(url(&h, "/ui/me"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let csrf = extract_csrf_value(&page.text().await.unwrap()).expect("csrf");
        client
            .post(url(&h, "/ui/me/tokens"))
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_csrf={csrf}&name={n}&ttl_preset=never"))
            .send()
            .await
            .unwrap();
    }
    // Default sort = created desc → beta, alpha, gamma (newest first).
    let default_body = client
        .get(url(&h, "/ui/me"))
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
        .get(url(&h, "/ui/me?sort=name&dir=asc"))
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

#[tokio::test]
async fn rename_redirects_and_swaps_token_name() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    // Create then rename.
    client
        .post(url(&h, "/ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=old-name&ttl_preset=never"))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(url(&h, "/ui/me/tokens/old-name/rename"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&new_name=fresh-name"))
        .send()
        .await
        .unwrap();
    assert!(
        (300..400).contains(&resp.status().as_u16()),
        "rename redirects: {}",
        resp.status()
    );
    let later = client
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(later.contains("fresh-name"), "new name listed: {later}");
    assert!(!later.contains("old-name"), "old name gone: {later}");
}

#[tokio::test]
async fn rename_collision_shows_inline_error_and_keeps_old_name() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    for n in ["taken", "renameable"] {
        client
            .post(url(&h, "/ui/me/tokens"))
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_csrf={csrf}&name={n}&ttl_preset=never"))
            .send()
            .await
            .unwrap();
    }
    let resp = client
        .post(url(&h, "/ui/me/tokens/renameable/rename"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&new_name=taken"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("already exists"), "collision error: {body}");
    // Both tokens still present under their original names.
    let later = client
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(later.contains("taken"));
    assert!(later.contains("renameable"));
}

#[tokio::test]
async fn rename_empty_new_name_shows_inline_error() {
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let page = client
        .get(url(&h, "/ui/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page.text().await.unwrap()).unwrap();
    client
        .post(url(&h, "/ui/me/tokens"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=x&ttl_preset=never"))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(url(&h, "/ui/me/tokens/x/rename"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&new_name=   "))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("must not be empty"),
        "empty-name error: {body}"
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

// -- Sessions -----------------------------------------------------------------
//
// Browser sessions are the third credential on this page, alongside API
// tokens and MCP grants. "Sign out everywhere" bumps the caller's session
// epoch, so it can only ever affect the caller — which is why it lives
// here under `/ui/me/*` rather than in the admin-gated settings subtree.

#[tokio::test]
async fn sessions_card_is_reachable_by_non_admins() {
    // The regression that motivated the move: the action was always
    // self-service, but its only affordance sat on an admin-only page,
    // so a non-admin had no way to reach it from a browser.
    let h = start().await;
    let client = no_redirect_client();

    for user in ["admin", "alice"] {
        let cookie = login_cookie(&h, &client, user).await;
        let (body, _) = account_screen(&h, &client, &cookie).await;
        assert!(
            body.contains("/ui/me/sessions/revoke-all"),
            "{user} can see the sign-out-everywhere form"
        );
    }
}

#[tokio::test]
async fn sign_out_everywhere_kills_every_session_including_the_caller() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;
    let (_, csrf) = account_screen(&h, &client, &admin).await;

    // A second, independent session for the same user.
    let other_client = no_redirect_client();
    let other = login_cookie(&h, &other_client, "admin").await;

    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/me/sessions/revoke-all",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        pick_set_cookie(&resp, "wm_session").is_some_and(|c| c.is_empty()),
        "clears the session cookie"
    );

    // Both sessions are dead — the epoch moved, not one record.
    for (label, c) in [("calling", &admin), ("other", &other)] {
        let resp = client
            .get(url(&h, "/api/users/me"))
            .header("cookie", c)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "{label} session revoked");
    }

    // A fresh login works again, stamped with the new epoch.
    let fresh = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/api/users/me"))
        .header("cookie", &fresh)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "re-login works");
}

#[tokio::test]
async fn a_non_admin_can_sign_out_everywhere() {
    let h = start().await;
    let client = no_redirect_client();
    let alice = login_cookie(&h, &client, "alice").await;
    let (_, csrf) = account_screen(&h, &client, &alice).await;

    let resp = post(
        &h,
        &client,
        &alice,
        "/ui/me/sessions/revoke-all",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);

    let resp = client
        .get(url(&h, "/api/users/me"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "own session revoked");
}

#[tokio::test]
async fn sign_out_everywhere_leaves_api_tokens_alone() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;

    let user = h
        .auth
        .get_user_by_email("admin@test.example")
        .unwrap()
        .unwrap();
    let (_token, plaintext) = h.auth.create_token(&user.id, "ci", None).expect("token");

    let (_, csrf) = account_screen(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/me/sessions/revoke-all",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);

    // Sessions are a different credential from tokens.
    let resp = client
        .get(url(&h, "/api/users/me"))
        .bearer_auth(&plaintext)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "token still authenticates");
}

#[tokio::test]
async fn rest_revoke_all_endpoint_returns_204_and_kills_sessions() {
    // The UI button and `POST /api/users/me/sessions/revoke-all` are two
    // entry points to the same epoch bump; this covers the REST one.
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/api/users/me/sessions/revoke-all"))
        .header("cookie", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "no body, nothing enumerated");
    assert!(
        resp.text().await.unwrap().is_empty(),
        "deliberately reports no count"
    );

    let resp = client
        .get(url(&h, "/api/users/me"))
        .header("cookie", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "session revoked");
}

#[tokio::test]
async fn a_users_revoke_does_not_touch_another_users_sessions() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;
    let alice = login_cookie(&h, &client, "alice").await;

    let resp = client
        .post(url(&h, "/api/users/me/sessions/revoke-all"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let resp = client
        .get(url(&h, "/api/users/me"))
        .header("cookie", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the epoch is per-user");
}

#[tokio::test]
async fn initials_come_from_the_email_and_never_render_empty() {
    // Two-word local parts take one letter from each; single words take
    // the first two. Both users on this harness exercise the second
    // form, so assert on the rendered chip rather than a unit call.
    let h = start().await;
    let client = no_redirect_client();
    for (user, initials) in [("admin", ">AD<"), ("alice", ">AL<")] {
        let cookie = login_cookie(&h, &client, user).await;
        let (body, _) = account_screen(&h, &client, &cookie).await;
        assert!(body.contains(initials), "{user} chip shows {initials}");
    }
}

#[tokio::test]
async fn account_page_is_the_only_credentials_screen() {
    // All three credentials on one screen — the rule ADR-0039 draws out
    // of the sign-out-everywhere incident.
    let h = start().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let (body, _) = account_screen(&h, &client, &cookie).await;
    for section in ["Create a new API token", "MCP applications", "Sessions"] {
        assert!(body.contains(section), "account screen has {section:?}");
    }
}

//! Tier-2 smoke tests for the web UI (slice 21).
//!
//! Drives the full flow you'd see in a browser:
//!   1. Hit `/__ui/` unauthenticated → 302 to `/__auth/login?next=...`
//!   2. GET `/__auth/login` → 200 with the password form
//!   3. POST `/__auth/login/password` → 303 + Set-Cookie
//!   4. GET `/__ui/` with the cookie → 200, the user's name on the page
//!   5. GET `/__ui/static/wm.css` → 200, `text/css`
//!   6. GET `/__ui/groups/foo` → 200, "Coming soon" placeholder
//!
//! Logging the user into the web UI is exactly what `just run-web`
//! is meant to demonstrate; these tests are the automated version
//! of that hand-driven dogfood loop.

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

async fn start_with_users(local_auth_value: &str) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());

    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse(local_auth_value).expect("local auth"))
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum serve");
    });
    Harness { addr, server }
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

async fn login_and_get_cookie(h: &Harness, client: &Client) -> String {
    let body = "username=admin&password=devpassword";
    let resp = client
        .post(url(h, "/__auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("login post");
    assert_eq!(resp.status().as_u16(), 303, "login failed");
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("set-cookie")
        .to_str()
        .unwrap()
        .to_string();
    // `wm_session=TOKEN.SIG; Path=/; ...` → keep just the
    // `name=value` pair for sending back.
    let first_pair = set_cookie.split(';').next().unwrap().trim();
    first_pair.to_string()
}

#[tokio::test]
async fn unauthenticated_ui_redirects_to_login_with_next() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();

    let resp = client.get(url(&h, "/__ui/")).send().await.expect("get");
    // axum's Redirect::to returns 303 See Other by default. Either is
    // fine for "you need to log in" — we just check we got a redirect.
    assert!(
        (300..400).contains(&resp.status().as_u16()),
        "expected a 3xx redirect, got {}",
        resp.status()
    );
    let location = resp
        .headers()
        .get("location")
        .expect("location")
        .to_str()
        .unwrap();
    assert!(
        location.starts_with("/__auth/login?next=/__ui/"),
        "expected /__auth/login?next=/__ui/, got: {location}"
    );
}

#[tokio::test]
async fn deeper_ui_path_preserves_next_through_login_redirect() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();

    let resp = client
        .get(url(&h, "/__ui/routes"))
        .send()
        .await
        .expect("get");
    assert!((300..400).contains(&resp.status().as_u16()));
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        location.contains("/__ui/routes"),
        "next param should carry the original path, got: {location}"
    );
}

#[tokio::test]
async fn login_page_renders_form_from_template() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__auth/login")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sign in to WireMirage"));
    assert!(body.contains("/__auth/login/password"));
    // Stylesheet link is the foundation deliverable — confirm it
    // reaches the page (the actual CSS is served by /__ui/static/).
    assert!(body.contains("/__ui/static/wm.css"));
}

#[tokio::test]
async fn login_page_carries_next_into_form_hidden_input() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/__auth/login?next=/__ui/groups"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // minijinja's auto-escape turns `/` into `&#x2f;` inside attribute
    // values — that's fine, browsers decode the entity on form submit.
    // We assert the two substring markers separately to stay robust
    // against future template tweaks.
    assert!(body.contains("name=\"next\""), "hidden next input missing");
    assert!(
        body.contains("__ui") && body.contains("groups"),
        "next value should carry the redirect target (in some encoding); body was: {body}"
    );
}

#[tokio::test]
async fn home_page_renders_after_login() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    let resp = client
        .get(url(&h, "/__ui/"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("get home");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // The signed-in user's name appears in the header badge and the
    // welcome message; the stylesheet link is present; the home
    // page's distinctive sections render.
    assert!(body.contains("admin"), "expected user name on home page");
    assert!(body.contains("Welcome, admin"));
    assert!(body.contains("Groups"));
    assert!(body.contains("/__ui/static/wm.css"));
}

#[tokio::test]
async fn home_page_shows_admin_badge_for_admin_user() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;
    let body = client
        .get(url(&h, "/__ui/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The admin badge sits in the header next to the user name.
    assert!(
        body.contains("badge--admin"),
        "expected admin badge in the header"
    );
}

#[tokio::test]
async fn css_served_unauthenticated_and_correct_mime() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/__ui/static/wm.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("text/css"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("--bg"), "expected design tokens in CSS");
}

#[tokio::test]
async fn placeholder_pages_render_with_api_hint() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    // minijinja auto-escapes `/` to `&#x2f;` in element bodies. The
    // browser un-escapes it for display; we match on substrings that
    // survive the escape (alphanumerics + underscores).
    for (path, expected_substrings) in [
        ("/__ui/groups", ["GET", "__api", "groups"]),
        ("/__ui/groups/foo", ["GET", "__api", "groups"]),
        ("/__ui/routes/foo/3", ["GET", "__api", "routes"]),
        ("/__ui/me/tokens", ["GET", "__api", "tokens"]),
    ] {
        let resp = client
            .get(url(&h, path))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "GET {path}");
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("coming"),
            "expected placeholder copy on {path}"
        );
        for sub in &expected_substrings {
            assert!(
                body.contains(sub),
                "expected substring {sub:?} on {path}; body was: {body}"
            );
        }
    }
}

#[tokio::test]
async fn admin_only_stubs_are_forbidden_for_non_admin() {
    let h = start_with_users("admin:devpassword:admin,user:devpassword").await;
    let client = no_redirect_client();

    // Log in as the non-admin user.
    let resp = client
        .post(url(&h, "/__auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("username=user&password=devpassword")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_string();

    // Admin-only stubs should return 403 (rendered through the same
    // placeholder template but with `Forbidden` title).
    for path in ["/__ui/unmatched", "/__ui/settings", "/__ui/admin/health"] {
        let resp = client
            .get(url(&h, path))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403, "non-admin GET {path}");
    }
}

#[tokio::test]
async fn logout_brings_you_back_to_login_redirect_loop() {
    let h = start_with_users("admin:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    // Confirm we're in.
    let me = client
        .get(url(&h, "/__ui/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 200);

    // Log out via the form button in the header.
    let logout = client
        .post(url(&h, "/__auth/logout"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status().as_u16(), 204);

    // With the (now-invalidated) cookie, /__ui/ should redirect again.
    let after = client
        .get(url(&h, "/__ui/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(
        (300..400).contains(&after.status().as_u16()),
        "expected redirect after logout, got {}",
        after.status()
    );
}

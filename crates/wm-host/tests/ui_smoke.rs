//! Tier-2 smoke tests for the web UI (slice 21).
//!
//! Drives the full flow you'd see in a browser:
//!   1. Hit `/ui/` unauthenticated → 302 to `/auth/login?next=...`
//!   2. GET `/auth/login` → 200 with the password form
//!   3. POST `/auth/login/password` → 303 + Set-Cookie
//!   4. GET `/ui/` with the cookie → 200, the user's name on the page
//!   5. GET `/ui/static/wm.css` → 200, `text/css`
//!   6. GET `/ui/groups/foo` → 200, "Coming soon" placeholder
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
    // Slice 25 added CSRF middleware. The login POST now requires a
    // matching wm_csrf cookie and `_csrf` form field. Fetch the login
    // page first to mint the cookie and read the embedded value, then
    // POST with both. Return the combined cookie string the rest of
    // the test will send back on subsequent requests.
    let get = client
        .get(url(h, "/auth/login"))
        .send()
        .await
        .expect("get login");
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let body = get.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("_csrf hidden input");

    let resp = client
        .post(url(h, "/auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(format!(
            "_csrf={csrf_value}&email=admin%40test.example&password=devpassword"
        ))
        .send()
        .await
        .expect("login post");
    assert_eq!(resp.status().as_u16(), 303, "login failed");
    let session_cookie = pick_set_cookie(&resp, "wm_session").expect("session cookie");
    format!("wm_csrf={csrf_cookie}; wm_session={session_cookie}")
}

/// Extract a Set-Cookie value for `name` from any of the response's
/// Set-Cookie headers. Returns just the cookie value (right of the
/// first `=`, up to but not including the first `;`).
fn pick_set_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    for v in resp.headers().get_all("set-cookie").iter() {
        let raw = v.to_str().ok()?;
        if let Some(rest) = raw.strip_prefix(&format!("{name}=")) {
            return Some(rest.split(';').next()?.to_string());
        }
    }
    None
}

/// Pull the `value=` of the `<input type="hidden" name="_csrf">` out
/// of a rendered HTML page so a test can echo it back in a POST. Slow
/// but correct for our well-formed templates; tests don't care.
fn extract_csrf_value(body: &str) -> Option<String> {
    let needle = "name=\"_csrf\" value=\"";
    let start = body.find(needle)? + needle.len();
    let end = body[start..].find('"')?;
    Some(body[start..start + end].to_string())
}

#[tokio::test]
async fn unauthenticated_ui_redirects_to_login_with_next() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();

    let resp = client.get(url(&h, "/ui/")).send().await.expect("get");
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
        location.starts_with("/auth/login?next=/ui/"),
        "expected /auth/login?next=/ui/, got: {location}"
    );
}

#[tokio::test]
async fn deeper_ui_path_preserves_next_through_login_redirect() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();

    let resp = client.get(url(&h, "/ui/routes")).send().await.expect("get");
    assert!((300..400).contains(&resp.status().as_u16()));
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        location.contains("/ui/routes"),
        "next param should carry the original path, got: {location}"
    );
}

#[tokio::test]
async fn login_page_renders_form_from_template() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/auth/login")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sign in to WireMirage"));
    assert!(body.contains("/auth/login/password"));
    // Stylesheet link is the foundation deliverable — confirm it
    // reaches the page (the actual CSS is served by /ui/static/).
    assert!(body.contains("/ui/static/wm.css"));
}

#[tokio::test]
async fn login_page_carries_next_into_form_hidden_input() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/auth/login?next=/ui/groups"))
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
        body.contains("ui") && body.contains("groups"),
        "next value should carry the redirect target (in some encoding); body was: {body}"
    );
}

#[tokio::test]
async fn home_page_renders_after_login() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    let resp = client
        .get(url(&h, "/ui/"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("get home");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // The signed-in user's name appears in the header badge and the
    // welcome message; the stylesheet link is present; the home
    // page's distinctive sections render.
    assert!(
        body.contains("admin@test.example"),
        "expected user email on home page"
    );
    assert!(body.contains("Welcome, admin"));
    assert!(body.contains("Groups"));
    assert!(body.contains("/ui/static/wm.css"));
}

#[tokio::test]
async fn home_page_shows_admin_badge_for_admin_user() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;
    let body = client
        .get(url(&h, "/ui/"))
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
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let resp = client
        .get(url(&h, "/ui/static/wm.css"))
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
async fn ace_editor_assets_served_with_js_mime() {
    // Slice 41 vendored a fixed list of Ace files under
    // /ui/static/ace/. Just spot-check that the core script plus
    // one mode + one theme + the wm-ace bootstrap come back as JS,
    // and that an unknown filename under the same prefix still 404s
    // (the handler is an enum match, not a passthrough to the filesystem).
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    for path in [
        "/ui/static/ace/ace.js",
        "/ui/static/ace/mode-typescript.js",
        "/ui/static/ace/theme-github_light_default.js",
        "/ui/static/wm-ace.js",
    ] {
        let resp = client.get(url(&h, path)).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path} 200");
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.starts_with("application/javascript"),
            "{path} mime: {ct}"
        );
    }
    let resp = client
        .get(url(&h, "/ui/static/ace/mode-cobol.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "unknown asset still 404s");
}

#[tokio::test]
async fn placeholder_pages_render_with_api_hint() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    // minijinja auto-escapes `/` to `&#x2f;` in element bodies. The
    // browser un-escapes it for display; we match on substrings that
    // survive the escape (alphanumerics + underscores).
    //
    // Each successive UI slice converts more of these stubs to real
    // pages and removes them from this list. After slice 28's unmatched
    // page landed, only `/ui/settings` (and the not-yet-implemented
    // `/ui/admin/health` and `/ui/routes/new`) remain placeholders.
    // If a future slice adds back a placeholder route, add a check
    // here for it.
    let placeholders: &[(&str, &[&str])] = &[("/ui/settings", &["GET", "api", "users"])];
    for &(path, expected_substrings) in placeholders.iter() {
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
        for sub in expected_substrings {
            assert!(
                body.contains(sub),
                "expected substring {sub:?} on {path}; body was: {body}"
            );
        }
    }
}

#[tokio::test]
async fn admin_only_stubs_are_forbidden_for_non_admin() {
    let h = start_with_users("admin@test.example:devpassword:admin,user@test.example:devpassword")
        .await;
    let client = no_redirect_client();

    // Log in as the non-admin user. Slice-25 CSRF: GET login page
    // first to mint the cookie + extract form value.
    let get = client.get(url(&h, "/auth/login")).send().await.unwrap();
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let csrf_value = extract_csrf_value(&get.text().await.unwrap()).expect("csrf");
    let resp = client
        .post(url(&h, "/auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(format!(
            "_csrf={csrf_value}&email=user%40test.example&password=devpassword"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303);
    let session_cookie = pick_set_cookie(&resp, "wm_session").expect("session");
    let cookie = format!("wm_csrf={csrf_cookie}; wm_session={session_cookie}");

    // Admin-only stubs should return 403 (rendered through the same
    // placeholder template but with `Forbidden` title). `/ui/unmatched`
    // is no longer here: ADR-0030 made it owner-scoped (a non-admin gets a
    // 200 with only their own groups' entries) — see ui_unmatched_pages.rs.
    for path in ["/ui/settings", "/ui/admin/health"] {
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
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    // Confirm we're in and read the CSRF token embedded in the page
    // so we can post a valid logout request.
    let me = client
        .get(url(&h, "/ui/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status().as_u16(), 200);
    let body = me.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("csrf in logout form");

    // Log out via the form button in the header.
    let logout = client
        .post(url(&h, "/auth/logout"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf_value}"))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status().as_u16(), 303, "logout redirects to login");
    assert_eq!(
        logout.headers()["location"].to_str().unwrap(),
        "/auth/login?signed_out=1"
    );

    // With the (now-invalidated) cookie, /ui/ should redirect again.
    let after = client
        .get(url(&h, "/ui/"))
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

#[tokio::test]
async fn connect_page_shows_mcp_endpoint_and_configs() {
    let h = start_with_users("admin@test.example:devpassword:admin").await;
    let client = no_redirect_client();
    let cookie = login_and_get_cookie(&h, &client).await;

    let resp = client
        .get(url(&h, "/ui/connect"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("get connect");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // The live MCP endpoint (derived from the request) + paste-ready configs.
    // minijinja HTML-escapes the URL's `/` to `&#x2f;` (correct — the
    // Host-derived value must not be marked `safe`), so assert on the
    // escaping-stable path component, not the literal `/api/mcp`.
    assert!(
        body.contains("api&#x2f;mcp"),
        "shows the MCP endpoint: {body}"
    );
    assert!(
        body.contains("claude mcp add"),
        "shows the Claude Code command"
    );
    assert!(body.contains("mcpServers"), "shows the config-file JSON");
}

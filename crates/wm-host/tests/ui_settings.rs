//! Tier-2 tests for the Settings page (admin user management) and the
//! session-epoch "sign out everywhere" behind it.
//!
//! Coverage:
//!   * Settings renders every user, not just admins; non-admins get 403
//!   * Create / promote / demote / delete mirror the `wm users` verbs
//!   * The three host guards surface as inline 400s, not generic errors:
//!     last-admin demote, last-admin delete, self-delete, owns-routes
//!   * "Sign out everywhere" kills the calling session immediately, and
//!     leaves API tokens working
//!   * The nav exposes Settings to admins only

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"thirty-two-byte-development-key!";
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

struct Harness {
    addr: String,
    auth: Auth,
    registry: Arc<Registry>,
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
    let routes = RouteTable::warm(registry.clone(), runtime.engine().clone()).expect("table");
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
    Harness {
        addr,
        auth,
        registry,
        server,
    }
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

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    let resp = client
        .get(url(h, "/auth/login"))
        .send()
        .await
        .expect("get login");
    let csrf_cookie = pick_set_cookie(&resp, "wm_csrf").expect("csrf cookie");
    let csrf_value = extract_csrf_value(&resp.text().await.unwrap()).expect("csrf value");
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

/// GET the settings page and return (body, csrf form value).
async fn settings(h: &Harness, client: &Client, cookie: &str) -> (String, String) {
    let body = client
        .get(url(h, "/ui/settings"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&body).expect("csrf on settings page");
    (body, csrf)
}

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

#[tokio::test]
async fn settings_lists_every_user_and_is_admin_only() {
    let h = start().await;
    let client = no_redirect_client();

    let admin = login_cookie(&h, &client, "admin").await;
    let (body, _) = settings(&h, &client, &admin).await;
    // Every user, not only admins — matches `wm users list`.
    assert!(body.contains("admin@test.example"), "admin row: {body}");
    assert!(body.contains("alice@test.example"), "non-admin row listed");
    assert!(body.contains("Identity providers"));
    assert!(body.contains("Sign out everywhere"));

    // Non-admins are refused outright.
    let alice = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/ui/settings"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "non-admin blocked");
}

#[tokio::test]
async fn nav_shows_settings_to_admins_only() {
    let h = start().await;
    let client = no_redirect_client();

    let admin = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/"))
        .header("cookie", &admin)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("/ui/settings"), "admin sees the nav entry");

    let alice = login_cookie(&h, &client, "alice").await;
    let body = client
        .get(url(&h, "/ui/"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !body.contains("/ui/settings"),
        "non-admin sees no nav entry: {body}"
    );
}

#[tokio::test]
async fn create_promote_demote_delete_round_trip() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;
    let (_, csrf) = settings(&h, &client, &admin).await;

    // Create.
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings",
        format!("_csrf={csrf}&email=bob%40test.example"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303, "create redirects");
    let (body, csrf) = settings(&h, &client, &admin).await;
    assert!(body.contains("bob@test.example"), "bob listed");
    assert!(
        !h.auth
            .get_user_by_email("bob@test.example")
            .unwrap()
            .unwrap()
            .is_admin,
        "created without --admin"
    );

    // Promote.
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/bob@test.example/admin",
        format!("_csrf={csrf}&is_admin=true"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        h.auth
            .get_user_by_email("bob@test.example")
            .unwrap()
            .unwrap()
            .is_admin,
        "promoted"
    );

    // Demote (safe now — there are two admins).
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/bob@test.example/admin",
        format!("_csrf={csrf}&is_admin=false"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        !h.auth
            .get_user_by_email("bob@test.example")
            .unwrap()
            .unwrap()
            .is_admin,
        "demoted"
    );

    // Delete.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/bob@test.example/delete",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        h.auth
            .get_user_by_email("bob@test.example")
            .unwrap()
            .is_none(),
        "deleted"
    );
}

#[tokio::test]
async fn create_user_with_admin_checkbox_sets_the_flag() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;
    let (_, csrf) = settings(&h, &client, &admin).await;

    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings",
        format!("_csrf={csrf}&email=root%40test.example&is_admin=on"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        h.auth
            .get_user_by_email("root@test.example")
            .unwrap()
            .unwrap()
            .is_admin
    );
}

#[tokio::test]
async fn guards_render_inline_errors() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;

    // Last admin cannot be demoted.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/admin@test.example/admin",
        format!("_csrf={csrf}&is_admin=false"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("last admin"), "names the guard: {body}");

    // An admin cannot delete themselves.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/admin@test.example/delete",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("cannot delete themselves")
    );

    // Unknown email.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/nobody@test.example/delete",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(resp.text().await.unwrap().contains("No user with email"));

    // Duplicate email on create.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings",
        format!("_csrf={csrf}&email=alice%40test.example"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(resp.text().await.unwrap().contains("already exists"));

    // Malformed email on create.
    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings",
        format!("_csrf={csrf}&email=notanemail"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    assert!(resp.text().await.unwrap().contains("email address"));
}

#[tokio::test]
async fn settings_post_without_csrf_is_rejected() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;

    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings",
        "email=nocsrf%40test.example".to_string(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403, "csrf middleware refuses");
    assert!(
        h.auth
            .get_user_by_email("nocsrf@test.example")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sign_out_everywhere_kills_the_calling_session() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;
    let (_, csrf) = settings(&h, &client, &admin).await;

    // A second, independent session for the same user.
    let other_client = no_redirect_client();
    let other = login_cookie(&h, &other_client, "admin").await;
    let resp = client
        .get(url(&h, "/ui/settings"))
        .header("cookie", &other)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "second session works first");

    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/sessions/revoke-all",
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

    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/sessions/revoke-all",
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
async fn revoke_all_is_available_to_non_admins() {
    // The page is admin-only, but the action only ever affects the
    // caller — gating it would misrepresent its blast radius.
    let h = start().await;
    let client = no_redirect_client();
    let alice = login_cookie(&h, &client, "alice").await;

    // Non-admins can't read the settings page, so take the csrf pair
    // from a page they can read.
    let page = client
        .get(url(&h, "/ui/me/tokens"))
        .header("cookie", &alice)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = extract_csrf_value(&page).expect("csrf");

    let resp = post(
        &h,
        &client,
        &alice,
        "/ui/settings/sessions/revoke-all",
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
async fn cannot_delete_a_user_who_still_owns_routes() {
    let h = start().await;
    let client = no_redirect_client();
    let admin = login_cookie(&h, &client, "admin").await;

    let alice = h
        .auth
        .get_user_by_email("alice@test.example")
        .unwrap()
        .unwrap();
    h.registry
        .create_route(NewRoute {
            // None auto-creates an implicit group owned by the route's
            // owner, which is all this guard needs.
            group: None,
            methods: vec!["GET".into()],
            path: "/thing".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: std::fs::read(ECHO_COMPONENT_PATH).expect("echo fixture"),
            source: None,
            owner_id: alice.id.clone(),
        })
        .expect("route");

    let (_, csrf) = settings(&h, &client, &admin).await;
    let resp = post(
        &h,
        &client,
        &admin,
        "/ui/settings/users/alice@test.example/delete",
        format!("_csrf={csrf}"),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("still owns 1 route"),
        "names the guard and the count: {body}"
    );
    assert!(
        h.auth
            .get_user_by_email("alice@test.example")
            .unwrap()
            .is_some(),
        "user survived the refused delete"
    );
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

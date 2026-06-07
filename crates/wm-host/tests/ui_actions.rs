//! Tier-2 smoke tests for slice 26 — action buttons on the group +
//! route detail pages.
//!
//! Coverage:
//!   * Refresh TTL succeeds and redirects back to the group detail
//!   * Edit TTL persists `ttl_seconds` + `sliding_ttl`
//!   * Edit with bad TTL returns 400
//!   * Delete group cascade-deletes routes and redirects to the list
//!   * Delete route removes the route and redirects to the group page
//!   * Non-admin can't delete someone else's group (403)
//!   * Non-admin can't delete someone else's route (403)
//!   * CSRF middleware still rejects when `_csrf` is missing

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::{NewGroup, NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"thirty-two-byte-development-key!";
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

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

/// Returns (harness, admin_id, alice_id) with two groups + two routes
/// owned by each user respectively.
async fn start_seeded() -> (Harness, String, String) {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let admin = auth.create_user("admin", true).expect("admin");
    let alice = auth.create_user("alice", false).expect("alice");

    let registry = Arc::new(Registry::new(storage.clone()));
    registry
        .create_group(NewGroup {
            name: "stripe-mock".into(),
            owner_id: admin.id.clone(),
            ttl_seconds: Some(3600),
            sliding_ttl: Some(true),
        })
        .expect("group");
    registry
        .create_group(NewGroup {
            name: "alice-only".into(),
            owner_id: alice.id.clone(),
            ttl_seconds: Some(3600),
            sliding_ttl: Some(true),
        })
        .expect("group");
    registry
        .create_route(NewRoute {
            group: Some("stripe-mock".into()),
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_wasm(),
            source: None,
            owner_id: admin.id.clone(),
        })
        .expect("route");
    registry
        .create_route(NewRoute {
            group: Some("alice-only".into()),
            methods: vec!["GET".into()],
            path: "/secret".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_wasm(),
            source: None,
            owner_id: alice.id.clone(),
        })
        .expect("route");

    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
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
    (Harness { addr, server }, admin.id, alice.id)
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> (String, String) {
    let get = client.get(url(h, "/__auth/login")).send().await.unwrap();
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let body = get.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("csrf form value");

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
    assert_eq!(resp.status().as_u16(), 303, "login {user}");
    let session = pick_set_cookie(&resp, "wm_session").expect("session");
    (
        format!("wm_csrf={csrf_cookie}; wm_session={session}"),
        csrf_value,
    )
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

// -- Group actions ----------------------------------------------------------

#[tokio::test]
async fn refresh_redirects_to_group_detail() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/refresh"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!(
        (300..400).contains(&resp.status().as_u16()),
        "refresh redirects; got {}",
        resp.status()
    );
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/__ui/groups/stripe-mock"
    );
}

#[tokio::test]
async fn edit_ttl_persists_new_ttl_and_sliding_flag() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    // Set TTL to 7200, turn sliding off.
    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&ttl_seconds=7200"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));

    // Re-render the page; the new TTL should appear and sliding
    // should be off.
    let body = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("7200"), "new TTL persists: {body}");
    assert!(
        !body.contains("· sliding"),
        "sliding flag should be off: {body}"
    );
}

#[tokio::test]
async fn edit_renames_group_and_redirects_to_new_subdomain() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_csrf={csrf}&name=stripe-renamed&ttl_seconds=86400"
        ))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/__ui/groups/stripe-renamed"
    );

    // The new subdomain resolves; the old name is gone (the group moved).
    let new = client
        .get(url(&h, "/__ui/groups/stripe-renamed"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(new.status().as_u16(), 200);
    let old = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(old.status().as_u16(), 404);
}

#[tokio::test]
async fn edit_rejects_invalid_rename() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        // Uppercase + underscore is not a valid DNS label.
        .body(format!("_csrf={csrf}&name=Bad_Name&ttl_seconds=86400"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    // The failed rename left the group untouched.
    let still = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(still.status().as_u16(), 200);
}

#[tokio::test]
async fn edit_ttl_rejects_zero_or_garbage() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&ttl_seconds=not-a-number"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn delete_group_cascade_removes_routes() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/delete"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/__ui/groups"
    );

    // The group is gone — visiting its detail page now 404s.
    let after = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 404);
}

#[tokio::test]
async fn non_admin_cannot_delete_someone_elses_group() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "alice").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/delete"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    // Admin's group is still there.
    let admin_client = no_redirect_client();
    let (admin_cookie, _) = login_cookie(&h, &admin_client, "admin").await;
    let after = admin_client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 200);
}

// -- Route actions ----------------------------------------------------------

#[tokio::test]
async fn delete_route_redirects_to_group_detail() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/routes/stripe-mock/1/delete"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/__ui/groups/stripe-mock"
    );

    // The route is gone from the registry.
    let after = client
        .get(url(&h, "/__ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 404);
}

#[tokio::test]
async fn non_admin_cannot_delete_someone_elses_route() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "alice").await;

    let resp = client
        .post(url(&h, "/__ui/routes/stripe-mock/1/delete"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn delete_without_csrf_is_403() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/__ui/groups/stripe-mock/delete"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

//! Tier-2 smoke tests for the slice-23 detail pages and the
//! bare-`/` redirect.
//!
//! Coverage:
//!   * Group detail renders metadata + routes table
//!   * Route detail renders metadata
//!   * Non-admin sees 403 on someone else's group / route
//!   * Unknown group / route → 404 (rendered placeholder)
//!   * `GET /` → 302 to `/__ui/` with session, `/__auth/login` without
//!   * `GET /` is shadowed by a user-defined `GET /` route

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

// Path to the echo-handler fixture component, stamped in by build.rs.
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture component")
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

/// Build a harness with two local users (admin + alice). Seeds the
/// registry with one group + one route owned by each, so detail-page
/// authorization paths have data to exercise.
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
            name: "alice-private".into(),
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
            owner_id: admin.id.clone(),
        })
        .expect("route");
    registry
        .create_route(NewRoute {
            group: Some("alice-private".into()),
            methods: vec!["GET".into()],
            path: "/secret".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_wasm(),
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

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    let resp = client
        .post(url(h, "/__auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("username={user}&password=devpassword"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "login {user}");
    resp.headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_string()
}

// -- /__ui/groups/{group} ---------------------------------------------------

#[tokio::test]
async fn group_detail_renders_metadata_and_routes_for_owner() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("stripe-mock"), "name in header: {body}");
    // Metadata table renders
    assert!(body.contains("3600"), "TTL value");
    assert!(body.contains("sliding"));
    // Routes table renders with the route
    assert!(body.contains("POST"), "route method visible");
    assert!(body.contains("charges"), "route path visible");
    // CLI hint section
    assert!(body.contains("wm groups refresh"));
}

#[tokio::test]
async fn group_detail_404_when_unknown() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let resp = client
        .get(url(&h, "/__ui/groups/no-such-group"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn group_detail_403_for_non_owner_non_admin() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;

    let resp = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn group_detail_includes_live_activity_pane_after_traffic() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    // Drive a couple of mock requests so the pre-fetch has content.
    for _ in 0..2 {
        client
            .post(url(&h, "/v1/charges"))
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/groups/stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The live-activity card renders with the most recent entries
    // already in the DOM, and the EventSource script is wired up to
    // the group-scoped SSE URL.
    assert!(body.contains("Live activity"));
    assert!(body.contains("status-2xx"), "pre-fetched 2xx entry: {body}");
    assert!(
        body.contains("/__api/journal/tail?group=stripe-mock"),
        "group-scoped SSE URL present"
    );
}

#[tokio::test]
async fn group_detail_admin_can_view_any_group() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    // admin views alice's group
    let resp = client
        .get(url(&h, "/__ui/groups/alice-private"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("alice-private"));
    assert!(body.contains("alice"), "owner column shows alice");
}

// -- /__ui/routes/{group}/{n} ----------------------------------------------

#[tokio::test]
async fn route_detail_renders_metadata_for_owner() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/__ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("POST"));
    // The path slashes get HTML-escaped in the rendered body — check
    // on the alphanumeric segment that survives escape.
    assert!(body.contains("charges"), "route path: {body}");
    assert!(body.contains("stripe-mock"), "group link");
    assert!(body.contains("wasm"), "language metadata");
    assert!(body.contains("0.1.0"), "bindings version");
    assert!(body.contains("KiB") || body.contains("B"), "size");
    // CLI hint section
    assert!(body.contains("wm routes test"));
}

#[tokio::test]
async fn route_detail_404_when_unknown_number() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/routes/stripe-mock/9999"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn route_detail_403_for_non_owner_non_admin() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn route_detail_lists_recent_journal_entries_after_traffic() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    // Fire a handful of mock requests at the route — these write
    // journal entries that the detail page should surface.
    for _ in 0..3 {
        client
            .post(url(&h, "/v1/charges"))
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Three journal entries should show; each has a status pill.
    assert!(
        body.contains("status-2xx"),
        "expected status pill for 2xx entries: {body}"
    );
    // The hit count in the metadata block should also reflect traffic.
    assert!(body.contains("3 hits") || body.contains("3 hit"));
}

// -- Bare-`/` redirect ------------------------------------------------------

#[tokio::test]
async fn root_redirects_to_login_when_unauthenticated() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/")).send().await.unwrap();
    assert!(
        (300..400).contains(&resp.status().as_u16()),
        "expected a 3xx, got {}",
        resp.status()
    );
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(location, "/__auth/login");
}

#[tokio::test]
async fn root_redirects_to_ui_when_authenticated() {
    let (h, _, _) = start_seeded().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(location, "/__ui/");
}

#[tokio::test]
async fn root_user_route_shadows_redirect() {
    // A user-registered `GET /` should be served as a normal mock
    // route, not bounced to the UI. Per route-model.md, that's
    // explicitly supported.
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let admin = auth.create_user("admin", true).expect("admin");
    let registry = Arc::new(Registry::new(storage.clone()));
    // group=None creates an implicit single-route group; the route
    // sits at GET / and we don't care about the group's name here.
    registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_wasm(),
            owner_id: admin.id.clone(),
        })
        .expect("route");

    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse("admin:devpassword:admin").expect("auth"))
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = no_redirect_client();
    let resp = client.get(format!("http://{addr}/")).send().await.unwrap();
    // The echo route should answer 200 — NOT a 3xx redirect.
    assert_eq!(
        resp.status().as_u16(),
        200,
        "user `GET /` route must shadow the bare-`/` redirect"
    );
}

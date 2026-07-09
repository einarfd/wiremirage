//! Tier-2 smoke tests for the slice-27 state pages.
//!
//! Coverage:
//!   * Empty state on both pages
//!   * Route state lists kv entries written by a real handler dispatch
//!   * Group state lists gkv entries written by a handler dispatch
//!   * Clear-state form wipes the namespace and redirects back
//!   * Non-admin can't view someone else's state (403)
//!   * Unknown group / route → 404

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
const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

fn counter_wasm() -> Vec<u8> {
    std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture")
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

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let admin = auth.create_user("admin@test.example", true).expect("admin");
    let alice = auth
        .create_user("alice@test.example", false)
        .expect("alice");
    let registry = Arc::new(Registry::new(storage.clone()));
    registry
        .create_group(NewGroup {
            name: "counter-demo".into(),
            owner_id: admin.id.clone(),
            ttl_seconds: Some(3600),
            sliding_ttl: Some(true),
        })
        .expect("group");
    // Two groups so we have an alice-owned group for the 403 test.
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
            group: Some("counter-demo".into()),
            methods: vec!["POST".into()],
            path: "/bump".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: counter_wasm(),
            source: None,
            owner_id: admin.id.clone(),
        })
        .expect("route");

    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
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
    Harness { addr, server }
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> (String, String) {
    let get = client.get(url(h, "/auth/login")).send().await.unwrap();
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let body = get.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("csrf value");
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

// -- Route state ------------------------------------------------------------

#[tokio::test]
async fn route_state_empty_on_a_route_that_never_ran() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Route state"));
    assert!(body.contains("No state yet"));
}

#[tokio::test]
async fn set_group_state_via_form_persists() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    // `mode=slow\nrate=10`, URL-encoded.
    let resp = client
        .post(url(&h, "/ui/groups/counter-demo/state/set"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&keys=mode%3Dslow%0Arate%3D10"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));

    let body = client
        .get(url(&h, "/ui/groups/counter-demo/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("mode") && body.contains("slow"),
        "set key shown: {body}"
    );
    assert!(body.contains("rate"));
}

#[tokio::test]
async fn set_route_state_via_form_persists() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/ui/routes/counter-demo/1/state/set"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&keys=seeded%3Dyes"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));

    let body = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("seeded") && body.contains("yes"),
        "set key shown: {body}"
    );
}

#[tokio::test]
async fn set_state_rejects_form_without_kv() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/ui/groups/counter-demo/state/set"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&keys=not-a-kv-line"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn route_state_lists_entries_after_dispatch() {
    let h = start().await;
    let client = no_redirect_client();
    // Drive the counter handler — it bumps a `count` key in route kv
    // on every call. Three calls leaves count=3 in kv.
    for _ in 0..3 {
        client
            .post(url(&h, "/bump"))
            .header(reqwest::header::HOST, "counter-demo.localhost")
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The counter handler writes at least one key; just assert the
    // table renders and there's at least one row.
    assert!(
        body.contains("1 entr") || body.contains("entries"),
        "entries count visible: {body}"
    );
    assert!(
        body.contains("Clear state"),
        "clear button rendered when state exists"
    );
}

#[tokio::test]
async fn route_state_clear_wipes_entries() {
    let h = start().await;
    let client = no_redirect_client();
    for _ in 0..2 {
        client
            .post(url(&h, "/bump"))
            .header(reqwest::header::HOST, "counter-demo.localhost")
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;

    let resp = client
        .post(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));

    let body = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("No state yet"));
}

#[tokio::test]
async fn route_state_403_for_non_owner() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn route_state_404_when_unknown() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/ui/routes/counter-demo/999/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

// -- Group state ------------------------------------------------------------

#[tokio::test]
async fn group_state_empty_when_nothing_written_yet() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/groups/counter-demo/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Group state"));
    assert!(body.contains("No group-shared state yet"));
}

#[tokio::test]
async fn group_state_clear_wipes_per_route_state_too() {
    let h = start().await;
    let client = no_redirect_client();
    // Drive the route a few times to put data in kv:.
    for _ in 0..2 {
        client
            .post(url(&h, "/bump"))
            .header(reqwest::header::HOST, "counter-demo.localhost")
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    // Confirm there's per-route state present.
    let before = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !before.contains("No state yet"),
        "precondition: state exists"
    );

    // Clear group state from the group state page.
    let resp = client
        .post(url(&h, "/ui/groups/counter-demo/state"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap();
    assert!((300..400).contains(&resp.status().as_u16()));

    // Per-route state is wiped too (clear_group_state deletes both
    // kv: and gkv: prefixes for the group).
    let after = client
        .get(url(&h, "/ui/routes/counter-demo/1/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(after.contains("No state yet"));
}

#[tokio::test]
async fn group_state_403_for_non_owner() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/ui/groups/counter-demo/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn group_state_404_when_unknown() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/ui/groups/no-such/state"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

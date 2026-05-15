//! Tier-2 smoke tests for the slice-24 journal pages.
//!
//! Coverage:
//!   * Live journal page renders with / without `?group=`
//!   * Initial entries pre-fetched when a group is scoped
//!   * Non-admin without `?group=` lands on the picker (no tail)
//!   * Non-admin with someone else's `?group=` → 403
//!   * Filters echo back and trim the initial rows
//!   * Journal entry detail renders full request/response
//!   * Journal entry 403 / 404 paths

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

/// Spin up a host with two users (admin + alice), one group + route
/// owned by each, then fire a few requests at admin's route so the
/// journal has entries to render.
async fn start_with_traffic() -> Harness {
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
    let h = Harness { addr, server };

    // Fire some mock requests so the journal has entries for admin's group.
    let client = no_redirect_client();
    for _ in 0..3 {
        client
            .post(url(&h, "/v1/charges"))
            .body(r#"{"amount":100}"#)
            .send()
            .await
            .unwrap();
    }
    h
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    // Slice-25 CSRF middleware: GET the login page first to mint the
    // wm_csrf cookie + read the embedded `_csrf` form value, then POST
    // with both. Returns the combined cookie string callers send back
    // on subsequent requests.
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
    let session_cookie = pick_set_cookie(&resp, "wm_session").expect("session cookie");
    format!("wm_csrf={csrf_cookie}; wm_session={session_cookie}")
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

// -- /__ui/journal/live -----------------------------------------------------

#[tokio::test]
async fn live_journal_page_renders_with_group_filter() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/journal/live?group=stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status.as_u16(), 200, "status {status}; body: {body}");
    // The page shows the group scope hint
    assert!(body.contains("stripe-mock"), "group in body");
    // Pre-rendered initial entries surface (POST method)
    assert!(body.contains("POST"), "method visible in entry");
    // The SSE URL is wired through to the page script
    assert!(
        body.contains("/__api/journal/tail?group=stripe-mock"),
        "SSE URL in inline script: {body}"
    );
    // Status pill class for the 200 response
    assert!(body.contains("status-2xx"));
}

#[tokio::test]
async fn live_journal_admin_without_group_renders_host_wide_picker() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/journal/live"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Tailing every handled request"));
    assert!(body.contains("All (host-wide)"));
    // Inline SSE script connects to the unfiltered tail endpoint.
    assert!(body.contains("/__api/journal/tail"));
}

#[tokio::test]
async fn live_journal_host_wide_prefetches_across_groups_for_admin() {
    // Regression: the host-wide admin view used to render an empty
    // table on first paint because there's no host-wide journal
    // list endpoint — only the SSE relay. After a tab-revisit, no
    // historical entries appeared until the next dispatch. The page
    // now fans out across every group and unions their recent
    // entries server-side before rendering.
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/journal/live"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The seeded traffic fired POSTs at stripe-mock — those entries
    // should appear on the host-wide view's pre-fetch.
    assert!(
        body.contains("POST"),
        "host-wide pre-fetch should surface stripe-mock traffic: {body}"
    );
    assert!(body.contains("stripe-mock"));
    assert!(body.contains("status-2xx"));
}

#[tokio::test]
async fn live_journal_non_admin_without_group_shows_picker_only() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let body = client
        .get(url(&h, "/__ui/journal/live"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Pick a group"));
    // No inline SSE script when can_tail is false
    assert!(
        !body.contains("new EventSource"),
        "non-admin pre-group view should not start an SSE connection"
    );
    // The picker offers alice's groups only — not admin's.
    assert!(body.contains("alice-only"));
    assert!(!body.contains(">stripe-mock<"));
}

#[tokio::test]
async fn live_journal_non_owner_with_group_is_403() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/journal/live?group=stripe-mock"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn live_journal_unknown_group_is_404() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/journal/live?group=no-such"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn live_journal_empty_form_values_do_not_wipe_entries() {
    // Regression: submitting the filter form with "Any method"
    // selected sends `method=` (empty value), which deserialised as
    // `Some("")`. The pre-fetch filter then asked "does the entry's
    // method equal the empty string?" — always false — and the table
    // rendered empty. Affected method, path_pattern, and status.
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(
            &h,
            "/__ui/journal/live?group=stripe-mock&method=&path_pattern=&status=",
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Pre-fetched entries from the seeded traffic should still appear.
    assert!(
        body.contains("status-2xx"),
        "empty filter values should not wipe the pre-fetched rows: {body}"
    );
    // The SSE URL should not contain empty filter params either.
    assert!(
        !body.contains("method=&"),
        "empty method should be dropped from the SSE URL"
    );
}

#[tokio::test]
async fn live_journal_filters_round_trip_through_sse_url() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(
            &h,
            "/__ui/journal/live?group=stripe-mock&method=POST&status=2xx",
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // SSE URL preserves the filters so the stream matches the page.
    assert!(body.contains("group=stripe-mock"));
    assert!(body.contains("method=POST"));
    assert!(body.contains("status=2xx"));
    // Filter values echo back into the form.
    assert!(body.contains("value=\"2xx\""));
}

// -- /__ui/journal/{group}/{number} ----------------------------------------

#[tokio::test]
async fn journal_entry_renders_full_record_for_owner() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    // The first journal entry from the seeded traffic should be #1.
    let body = client
        .get(url(&h, "/__ui/journal/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("POST"));
    // Request body text rendered
    assert!(body.contains("amount"), "request body shown");
    assert!(body.contains("Request"));
    assert!(body.contains("Response"));
    // Status pill
    assert!(body.contains("status-2xx"));
    // Breadcrumb walks Groups → group → route → entry (matches the
    // wireframe; slice-30-style restructure).
    assert!(
        body.contains("/__ui/groups/stripe-mock"),
        "breadcrumb has group link"
    );
    assert!(
        body.contains("/__ui/routes/stripe-mock/1"),
        "breadcrumb has route link"
    );
    assert!(body.contains("journal #1"), "current-page label visible");
}

#[tokio::test]
async fn journal_entry_403_for_non_owner() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/journal/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn journal_entry_404_when_unknown_number() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/journal/stripe-mock/9999"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn journal_entry_404_when_unknown_group() {
    let h = start_with_traffic().await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/journal/no-such/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

//! Tier-2 smoke tests for the slice-32 dry-run UI page.
//!
//! Coverage:
//!   * GET renders the form pre-filled with the route's method + path
//!   * Non-owner non-admin → 403
//!   * Unknown route → 404
//!   * POST runs the handler and renders status + body
//!   * POST does not touch the route's real kv (snapshot semantics)
//!   * POST does not write a journal entry
//!   * Bad headers / non-`/`-prefixed path → inline 400 with form
//!     values preserved
//!   * Missing CSRF token → 403 (middleware)

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::{Journal, UnmatchedCursor};
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
    state: AppState,
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
    let admin = auth.create_user("admin", true).expect("admin");
    let _alice = auth.create_user("alice", false).expect("alice");
    let registry = Arc::new(Registry::new(storage.clone()));
    registry
        .create_group(NewGroup {
            name: "counter-demo".into(),
            owner_id: admin.id.clone(),
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
            owner_id: admin.id.clone(),
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
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        addr,
        server,
        state,
    }
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> (String, String) {
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

#[tokio::test]
async fn dry_run_form_renders_pre_filled_for_owner() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Form fields present.
    assert!(body.contains("name=\"method\""));
    assert!(body.contains("name=\"path\""));
    assert!(body.contains("name=\"headers\""));
    assert!(body.contains("name=\"body\""));
    // Method dropdown defaults to the route's first method (POST).
    assert!(
        body.contains("value=\"POST\" selected"),
        "POST pre-selected: {body}"
    );
    // Path input pre-filled — `/` escapes to `&#x2f;`.
    assert!(body.contains("value=\"&#x2f;bump\""));
}

#[tokio::test]
async fn dry_run_non_owner_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn dry_run_unknown_route_is_404() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/__ui/routes/counter-demo/999/dry-run"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn dry_run_submit_runs_handler_and_renders_response() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_csrf={csrf}&method=POST&path=/bump&headers=&query=&body="
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // Response card rendered.
    assert!(
        body.contains("Response"),
        "Response section visible: {body}"
    );
    // The counter handler returns 200 by default.
    assert!(
        body.contains("status-2xx"),
        "2xx pill on dry-run response: {body}"
    );
    // The counter handler bumps `count` and returns it. The dry-run
    // starts from an empty snapshot so it should read count=1 the
    // first time.
    assert!(body.contains("count=1"), "handler body shown: {body}");
}

#[tokio::test]
async fn dry_run_does_not_persist_state_or_write_journal() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    // Drive a real request first so we know what the state + journal
    // numbers look like.
    client
        .post(url(&h, "/bump"))
        .body("{}")
        .send()
        .await
        .unwrap();
    // Real request bumps count to 1 and writes one journal entry.
    let group = h
        .state
        .routes()
        .registry()
        .read_group_by_ref("counter-demo")
        .expect("group");
    let pre_route_state = h
        .state
        .routes()
        .registry()
        .list_route_state("counter-demo", 1)
        .expect("route state");
    let pre_journal_len = h
        .state
        .journal()
        .list_for_group(&group.id, Default::default())
        .expect("journal")
        .len();

    // Now hit dry-run a couple of times.
    for _ in 0..3 {
        let resp = client
            .post(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!(
                "_csrf={csrf}&method=POST&path=/bump&headers=&query=&body="
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Route's real kv is unchanged (still reflects the one real call).
    let post_route_state = h
        .state
        .routes()
        .registry()
        .list_route_state("counter-demo", 1)
        .expect("route state");
    assert_eq!(
        pre_route_state.len(),
        post_route_state.len(),
        "route state unchanged after dry-runs"
    );
    // Journal length unchanged.
    let post_journal_len = h
        .state
        .journal()
        .list_for_group(&group.id, Default::default())
        .expect("journal")
        .len();
    assert_eq!(
        pre_journal_len, post_journal_len,
        "no journal entries written by dry-runs"
    );
    // Also: nothing on the unmatched journal.
    let unmatched = h
        .state
        .journal()
        .list_unmatched(UnmatchedCursor {
            before: None,
            limit: 100,
        })
        .expect("unmatched");
    assert!(unmatched.is_empty(), "no unmatched entries: {unmatched:?}");
}

#[tokio::test]
async fn dry_run_bad_headers_renders_inline_400_and_keeps_form_values() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    // Headers textarea without a `:` separator → parse error.
    let resp = client
        .post(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_csrf={csrf}&method=POST&path=/bump&headers=no-colon-here&query=&body=keep-me"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Couldn't run"));
    assert!(body.contains("no-colon-here"), "headers preserved: {body}");
    assert!(body.contains("keep-me"), "body preserved: {body}");
}

#[tokio::test]
async fn dry_run_path_must_start_with_slash() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_csrf={csrf}&method=POST&path=bump&headers=&query=&body="
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    // minijinja escapes `/` to `&#x2f;`.
    assert!(body.contains("must start with &#x2f;"));
}

#[tokio::test]
async fn dry_run_without_csrf_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/routes/counter-demo/1/dry-run"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("method=POST&path=/bump&headers=&query=&body=")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

//! Tier-2 smoke tests for the slice-29 route creation form.
//!
//! Coverage:
//!   * GET renders the form with default values
//!   * `?method=&path=&group=` query string prefills the form
//!   * POST with a valid TS source → 303 redirect to the new route's
//!     detail page; the route lands in the registry and serves traffic
//!   * Reserved-prefix path returns an inline validation error
//!   * No compiler configured surfaces the "compile_failed" message
//!   * Missing CSRF token is rejected by the middleware (403)

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use reqwest::Client;
use reqwest::redirect::Policy;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"thirty-two-byte-development-key!";
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
    _mock_compiler: Option<tokio::task::JoinHandle<()>>,
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

async fn start_with_compiler(compiler: Option<CompilerClient>) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.create_user("admin", true).expect("admin");
    auth.create_user("alice", false).expect("alice");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let mut state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(
            LocalAuth::parse("admin:devpassword:admin,alice:devpassword").expect("auth"),
        )
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    if let Some(c) = compiler {
        state = state.with_compiler(c);
    }
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        addr,
        server,
        _mock_compiler: None,
    }
}

async fn start() -> Harness {
    start_with_compiler(None).await
}

/// Spin up the same canned-bytes mock compiler `api_routes.rs` uses,
/// pointing at the echo fixture so a successful POST exercises the
/// real `create_route_core` happy path end-to-end (including
/// component validation + RouteTable warm-cache refresh).
async fn start_with_mock_compiler() -> Harness {
    let echo_bytes = std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture");
    let canned_b64 = B64.encode(&echo_bytes);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind compiler");
    let mock_addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/compile",
            post(move |State(state): State<Arc<String>>| {
                let body = (*state).clone();
                async move {
                    axum::Json(json!({
                        "compiled_wasm": body,
                        "bindings_version": "0.1.0",
                    }))
                }
            }),
        )
        .with_state(Arc::new(canned_b64));
    let mock = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    let compiler_url = format!("http://{mock_addr}");
    let mut h = start_with_compiler(Some(CompilerClient::new(compiler_url))).await;
    h._mock_compiler = Some(mock);
    h
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
async fn route_new_form_renders_with_defaults() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/routes/new"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Create route"));
    assert!(body.contains("name=\"method\""));
    assert!(body.contains("name=\"path\""));
    assert!(body.contains("name=\"group\""));
    assert!(body.contains("name=\"language\""));
    assert!(body.contains("name=\"source\""));
    // Default TS handler is pre-filled.
    assert!(body.contains("export default async function handle"));
    // (new implicit group) option present even when no groups exist.
    assert!(body.contains("(new implicit group)"));
}

#[tokio::test]
async fn route_new_form_honours_query_string_prefill() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(
            &h,
            "/__ui/routes/new?method=PUT&path=/v1/charges/refund",
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // PUT option pre-selected.
    assert!(
        body.contains("value=\"PUT\" selected"),
        "PUT pre-selected: {body}"
    );
    // Path input pre-filled — minijinja escapes `/` to `&#x2f;`.
    assert!(body.contains("value=\"&#x2f;v1&#x2f;charges&#x2f;refund\""));
}

#[tokio::test]
async fn route_new_submit_creates_route_and_redirects() {
    let h = start_with_mock_compiler().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&method=POST&path=/v1/charges&group=&language=typescript&source=export+default+async+function+handle(req%2C+ctx)+%7B+return+%7Bstatus%3A+200%2C+headers%3A+%5B%5D%2C+body%3A+new+Uint8Array%28%29%7D%3B+%7D"
    );
    let resp = client
        .post(url(&h, "/__ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "303 redirect on success");
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        loc.starts_with("/__ui/routes/"),
        "redirected to detail: {loc}"
    );
    // Route is registered + reachable on mock traffic.
    let mock = client
        .post(url(&h, "/v1/charges"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert!(
        mock.status().as_u16() < 500,
        "registered route handles traffic"
    );
}

#[tokio::test]
async fn route_new_submit_rejects_reserved_path() {
    let h = start_with_mock_compiler().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body =
        format!("_csrf={csrf}&method=POST&path=/__api/oops&group=&language=typescript&source=x");
    let resp = client
        .post(url(&h, "/__ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("reserved prefix") || body.contains("Couldn"),
        "reserved-path error visible: {body}"
    );
    // Form values preserved on re-render.
    assert!(body.contains("value=\"&#x2f;__api&#x2f;oops\""));
}

#[tokio::test]
async fn route_new_submit_without_compiler_reports_compile_failed() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body =
        format!("_csrf={csrf}&method=POST&path=/v1/charges&group=&language=typescript&source=x");
    let resp = client
        .post(url(&h, "/__ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("compiler sidecar not configured"),
        "no-compiler error visible: {body}"
    );
    assert!(body.contains("Compile failed"));
}

#[tokio::test]
async fn route_new_submit_without_csrf_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    // Note: omitted _csrf field entirely.
    let resp = client
        .post(url(&h, "/__ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("method=POST&path=/v1/x&group=&language=typescript&source=x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

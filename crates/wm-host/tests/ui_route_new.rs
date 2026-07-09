//! Tier-2 smoke tests for the slice-29 route creation form.
//!
//! Coverage:
//!   * GET renders the form with default values
//!   * `?method=&path=&group=` query string prefills the form
//!   * POST with a valid TS source → 303 redirect to the new route's
//!     detail page; the route lands in the registry
//!   * Reserved-prefix path returns an inline validation error
//!   * Bad TS source surfaces the in-host swc transpile error
//!   * Missing CSRF token is rejected by the middleware (403)

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

#[tokio::test]
async fn route_new_form_renders_with_defaults() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/routes/new"))
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
    assert!(body.contains("function handle"));
    // (new implicit group) option present even when no groups exist.
    assert!(body.contains("(new implicit group)"));
}

#[tokio::test]
async fn route_new_form_honours_query_string_prefill() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/ui/routes/new?method=PUT&path=/v1/charges/refund"))
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
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    // `function handle(...)` script form — what the engine actually
    // accepts (see ADR-0020 + DEFAULT_TS_HANDLER_SOURCE).
    let form_body = format!(
        "_csrf={csrf}&method=POST&path=/v1/charges&group=&language=typescript&source=function+handle(req%2C+route%2C+group)+%7B+return+%7Bstatus%3A+200%2C+headers%3A+%5B%5D%2C+body%3A+new+Uint8Array%28%29%7D%3B+%7D"
    );
    let resp = client
        .post(url(&h, "/ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        303,
        "303 redirect on success: {}",
        resp.text().await.unwrap_or_default()
    );
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        loc.starts_with("/ui/routes/"),
        "redirected to detail: {loc}"
    );
}

#[tokio::test]
async fn route_new_submit_allows_formerly_reserved_path() {
    // ADR-0033: no reserved paths. A route at `/health` (a control-plane path
    // on the apex) is mockable on its group's subdomain, so the form accepts it
    // and 303s to the new route's detail page.
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&method=GET&path=/health&group=&language=typescript&source=function+handle()+%7B+return+%7B+status%3A+200%2C+headers%3A+%5B%5D%2C+body%3A+new+Uint8Array()+%7D%3B+%7D"
    );
    let resp = client
        .post(url(&h, "/ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "create succeeds and redirects");
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        loc.starts_with("/ui/routes/"),
        "redirected to the new route detail: {loc}"
    );
}

#[tokio::test]
async fn route_new_submit_with_bad_source_reports_compile_failed() {
    // ADR-0020 slice B: bad TS source is rejected in-host by swc.
    // No Node sidecar — the form just re-renders with the parser's
    // error in the "Compile failed" diagnostic strip.
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&method=POST&path=/v1/charges&group=&language=typescript&source=function+handle(+%7B"
    );
    let resp = client
        .post(url(&h, "/ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Compile failed"), "diagnostic shown: {body}");
}

#[tokio::test]
async fn route_new_submit_without_csrf_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    // Note: omitted _csrf field entirely.
    let resp = client
        .post(url(&h, "/ui/routes/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("method=POST&path=/v1/x&group=&language=typescript&source=x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

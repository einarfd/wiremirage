//! Tier-2 smoke tests for the group-creation form (ADR-0030 phase 3d).
//!
//! Coverage:
//!   * GET renders the form (name / TTL / sliding-TTL fields)
//!   * POST with an explicit name → 303 to the group detail page; the
//!     group exists (verified by following the redirect)
//!   * POST with a blank name → 303 to an auto-assigned DNS-safe name
//!   * Invalid DNS-label name → inline 400 with the value preserved
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
    auth.create_user("admin", true).expect("admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(LocalAuth::parse("admin:devpassword:admin").expect("auth"))
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
async fn group_new_form_renders() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let body = client
        .get(url(&h, "/__ui/groups/new"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Create group"));
    assert!(body.contains("name=\"name\""));
    assert!(body.contains("name=\"ttl_seconds\""));
    assert!(body.contains("name=\"sliding_ttl\""));
}

#[tokio::test]
async fn group_new_submit_creates_and_redirects() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/groups/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "_csrf={csrf}&name=my-tenant&ttl_seconds=&sliding_ttl=on"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "303 on success");
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/__ui/groups/my-tenant"
    );

    // Following the redirect proves the group really exists.
    let detail = client
        .get(url(&h, "/__ui/groups/my-tenant"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(detail.status().as_u16(), 200);
    assert!(detail.text().await.unwrap().contains("my-tenant"));
}

#[tokio::test]
async fn group_new_submit_auto_names_when_blank() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/groups/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}&name=&ttl_seconds=&sliding_ttl=on"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303);
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(
        loc.starts_with("/__ui/groups/"),
        "redirected to a group: {loc}"
    );
    assert_ne!(
        loc, "/__ui/groups/new",
        "got an auto-assigned name, not 'new'"
    );
}

#[tokio::test]
async fn group_new_submit_rejects_invalid_name() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/groups/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        // Uppercase + underscore: not a valid DNS label.
        .body(format!(
            "_csrf={csrf}&name=Bad_Name&ttl_seconds=&sliding_ttl=on"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Invalid group name"), "error shown: {body}");
    // Submitted value preserved on re-render.
    assert!(body.contains("Bad_Name"), "value preserved: {body}");
}

#[tokio::test]
async fn group_new_submit_without_csrf_is_forbidden() {
    let h = start().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .post(url(&h, "/__ui/groups/new"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("name=x&ttl_seconds=&sliding_ttl=on") // _csrf omitted
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

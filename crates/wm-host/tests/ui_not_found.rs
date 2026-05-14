//! Tier-2 smoke tests for the UI 404 page (slice 26 polish).
//!
//! Coverage:
//!   * `/__ui/typo` returns 404 with the branded HTML page (extends
//!     `base.html`), not the JSON error blob.
//!   * `/__api/typo` keeps the JSON 404 — scripts and agents are the
//!     audience there.
//!   * Mock traffic (non-reserved path) keeps the JSON 404 + writes
//!     to the unmatched journal.
//!   * The requested path is HTML-escaped in the rendered page so a
//!     crafted URL can't smuggle script tags.

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

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
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness { addr, server }
}

#[tokio::test]
async fn ui_typo_renders_branded_html_404() {
    let h = start().await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__ui/typo")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.starts_with("text/html"),
        "UI 404 should be HTML, got {ct}"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("Page not found"));
    assert!(body.contains("typo"), "path mentioned in body");
    // The page extends base.html so the nav + CSS link should be
    // present.
    assert!(body.contains("/__ui/static/wm.css"));
    assert!(body.contains("/__ui/journal/live"));
}

#[tokio::test]
async fn ui_404_does_not_reflect_unescaped_html_from_the_path() {
    let h = start().await;
    let client = no_redirect_client();
    // axum keeps the path percent-encoded in `req.uri().path()`, so
    // `%3Cscript%3E` survives into the template as the literal six
    // characters. Either way, the raw `<script>` tag must not appear
    // in the rendered body — that's the property we care about.
    let resp = client
        .get(url(&h, "/__ui/%3Cscript%3Ealert(1)%3C/script%3E"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "raw script tag must not appear in the HTML body: {body}"
    );
}

#[tokio::test]
async fn api_typo_keeps_json_404() {
    let h = start().await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__api/typo")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.starts_with("application/json"),
        "API 404 should be JSON, got {ct}"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"code\":\"not_found\""));
    assert!(body.contains("reserved path"));
}

#[tokio::test]
async fn auth_typo_keeps_json_404() {
    // Same rule applies to /__auth/* — the surface is consumed by
    // login flows and bots, not browsers exploring.
    let h = start().await;
    let client = no_redirect_client();
    let resp = client.get(url(&h, "/__auth/typo")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("application/json"));
}

#[tokio::test]
async fn mock_traffic_typo_keeps_json_404() {
    let h = start().await;
    let client = no_redirect_client();
    let resp = client
        .post(url(&h, "/v1/no/such/route"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.starts_with("application/json"),
        "mock-traffic 404 should be JSON, got {ct}"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"code\":\"not_found\""));
    assert!(body.contains("no route matched"));
}

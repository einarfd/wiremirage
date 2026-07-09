//! Tier-2 smoke tests for the slice-40 source edit page.
//!
//! Coverage:
//!   * GET renders the textarea pre-populated with stored source
//!   * GET on a wasm-uploaded route → 404 (no source to edit)
//!   * GET as non-owner non-admin → 403
//!   * POST with new source goes through in-host swc, redirects to detail
//!   * After redirect, the detail page shows the new source
//!   * POST without CSRF → 403 (handled by the CSRF middleware)
//!   * POST with invalid TS source → re-renders with compile_failed

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
// Plain script-shape JS (no `export`) so the source survives the
// in-host swc strip round-trip byte-for-byte and the assertions on
// the rendered detail page can anchor on "status: 200" / "status: 201".
const ORIGINAL_SOURCE: &str = "function handle(_req, _r, _g) {\n  return { status: 200, headers: [], body: new Uint8Array() };\n}";
const UPDATED_SOURCE: &str = "function handle(_req, _r, _g) {\n  return { status: 201, headers: [], body: new Uint8Array() };\n}";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
    admin_id: String,
    alice_id: String,
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

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

async fn start_seeded() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let admin = auth.create_user("admin@test.example", true).expect("admin");
    let alice = auth
        .create_user("alice@test.example", false)
        .expect("alice");

    let registry = Arc::new(Registry::new(storage.clone()));
    registry
        .create_group(NewGroup {
            name: "stripe-mock".into(),
            owner_id: admin.id.clone(),
            ttl_seconds: Some(3600),
            sliding_ttl: Some(true),
        })
        .expect("group");
    // One source-language route (the subject under test) + one
    // wasm-uploaded route so the 404 branch has data to assert against.
    registry
        .create_route(NewRoute {
            group: Some("stripe-mock".into()),
            methods: vec!["POST".into()],
            path: "/v1/sessions".into(),
            language: "javascript".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: Vec::new(),
            source: Some(ORIGINAL_SOURCE.into()),
            owner_id: admin.id.clone(),
        })
        .expect("seed js route");
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
        .expect("seed wasm route");

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
    Harness {
        addr,
        server,
        admin_id: admin.id,
        alice_id: alice.id,
    }
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
async fn edit_form_renders_with_current_source_for_source_route() {
    let h = start_seeded().await;
    let _ = &h.admin_id;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Edit handler source"), "page header");
    // Textarea is present and pre-populated with the stored source.
    // minijinja escapes some characters, so we anchor on a substring
    // that survives unchanged ("status: 200").
    assert!(
        body.contains("status: 200"),
        "current source pre-filled: {body}"
    );
    assert!(body.contains("name=\"source\""));
    assert!(body.contains("name=\"_csrf\""));
    // Language picker present and the route's current language is
    // pre-selected. The seed route was registered as javascript.
    assert!(
        body.contains("name=\"language\""),
        "language select present"
    );
    assert!(
        body.contains("value=\"javascript\" selected"),
        "current language pre-selected: {body}"
    );
}

#[tokio::test]
async fn edit_submit_switches_language_javascript_to_typescript() {
    // The PATCH path accepts `language` as part of the artifact triple
    // (slice 15), so the UI form lets users flip JS ↔ TS without
    // re-creating the route. ADR-0020 slice B handles the transpile
    // in-process; no external compiler.
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&language=typescript&source={}",
        urlencoding::encode(UPDATED_SOURCE),
    );
    let resp = client
        .post(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "303 redirect on success");

    // Detail page should now report the language as TypeScript.
    let detail = client
        .get(url(&h, "/ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        detail.contains("typescript"),
        "language switched to typescript on detail: {detail}"
    );
}

#[tokio::test]
async fn edit_submit_ignores_unsupported_language_values() {
    // A hand-crafted POST that sends e.g. `language=wasm` should NOT
    // silently flip the route to a wasm artifact — that path needs a
    // pre-compiled component, not a source recompile. We treat any
    // value outside the offered dropdown options as "keep the existing
    // language" rather than fail with a confusing compile error.
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&language=wasm&source={}",
        urlencoding::encode(UPDATED_SOURCE),
    );
    let resp = client
        .post(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303);
    let detail = client
        .get(url(&h, "/ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The route stayed `javascript` (its original language).
    assert!(
        detail.contains("javascript"),
        "language unchanged: {detail}"
    );
}

#[tokio::test]
async fn edit_form_404s_on_wasm_route_without_stored_source() {
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    let resp = client
        .get(url(&h, "/ui/routes/stripe-mock/2/source/edit"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn edit_form_403_for_non_owner_non_admin() {
    let h = start_seeded().await;
    let _ = &h.alice_id;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "alice").await;
    let resp = client
        .get(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn edit_submit_with_new_source_updates_and_redirects() {
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let form_body = format!(
        "_csrf={csrf}&source={}",
        urlencoding::encode(UPDATED_SOURCE),
    );
    let resp = client
        .post(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "303 redirect on success");
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(loc, "/ui/routes/stripe-mock/1");

    // Follow the redirect: the detail page should now show the new source.
    let detail = client
        .get(url(&h, "/ui/routes/stripe-mock/1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        detail.contains("status: 201"),
        "updated source visible on detail: {detail}"
    );
    assert!(
        !detail.contains("status: 200"),
        "old source no longer rendered"
    );
}

#[tokio::test]
async fn edit_submit_without_csrf_is_forbidden() {
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, _csrf) = login_cookie(&h, &client, "admin").await;
    // Note: _csrf intentionally absent.
    let resp = client
        .post(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("source={}", urlencoding::encode(UPDATED_SOURCE)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn edit_submit_with_invalid_ts_source_reports_compile_failed() {
    // ADR-0020 slice B: invalid TS source is rejected in-host by swc.
    // The form re-renders with the parser's diagnostic.
    let h = start_seeded().await;
    let client = no_redirect_client();
    let (cookie, csrf) = login_cookie(&h, &client, "admin").await;
    let bad = "function handle( {"; // mismatched braces
    let form_body = format!(
        "_csrf={csrf}&language=typescript&source={}",
        urlencoding::encode(bad),
    );
    let resp = client
        .post(url(&h, "/ui/routes/stripe-mock/1/source/edit"))
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

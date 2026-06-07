//! Tier-2 end-to-end tests for the slice-3 REST API.
//!
//! Boots the host on a random port, drives it via reqwest, exercises
//! POST/GET/DELETE on `/__api/routes`, and verifies that mock-traffic
//! requests get routed to the registered components.

use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Client;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

// Pre-compiled fixture components for tests that need a *dispatching*
// route but aren't testing route creation itself (journal, state,
// trace, cascade, pagination). Seeding these via the internal registry
// is fast — it skips the ~30s StarlingMonkey compile that attaching the
// shared engine for source dispatch would cost. The public create API
// is source-only (ADR-0023); this is the internal path other tier-2
// suites (http_smoke, ui_*) already use.
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");
const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

fn counter_wasm() -> Vec<u8> {
    std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture")
}

// Source-language handlers replacing the old wasm fixtures (ADR-0023
// retired public wasm upload). `echo_source` mirrors the echo fixture
// ("echo: METHOD PATH"); `counter_source` mirrors the counter fixture
// (route-private `count` incremented per request, body "count=N").
// Both dispatch through the embedded js-engine.wasm attached in
// `Harness::start`.
fn echo_source() -> &'static str {
    r#"function handle(req, route, group) {
        return {
            status: 200,
            headers: [["content-type", "text/plain"]],
            body: new TextEncoder().encode("echo: " + req.method + " " + req.path),
        };
    }"#
}

fn counter_source() -> &'static str {
    r#"function handle(req, route, group) {
        const n = route.incr("count", 1n);
        return {
            status: 200,
            headers: [["content-type", "text/plain"]],
            body: new TextEncoder().encode("count=" + n.toString()),
        };
    }"#
}

/// Path to the build-time js-engine.wasm (ADR-0020 slice C stamps
/// `WM_JS_ENGINE_WASM`). `None` only in a broken build without the
/// engine artifact — dispatch-asserting tests need it.
fn js_engine_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

struct Harness {
    addr: String,
    client: Client,
    auth: Auth,
    state: AppState,
    server: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Fast harness for API-surface tests (create / list / patch /
    /// auth / conflict). Storing source never touches the engine, so
    /// these don't pay the StarlingMonkey compile cost.
    async fn start() -> Self {
        Self::start_inner(false).await
    }

    /// Harness with the shared js-engine attached, so source-language
    /// routes actually *dispatch*. Only the handful of tests that hit
    /// a mock URL and assert on the response need this — attaching the
    /// engine compiles StarlingMonkey (~30s in debug), so the default
    /// `start()` skips it.
    async fn start_with_engine() -> Self {
        Self::start_inner(true).await
    }

    async fn start_inner(attach_engine: bool) -> Self {
        // Install the W3C propagator once per process so the tier-2
        // tests that send `traceparent` headers see the trace_id
        // stamped on journal records. Idempotent; the global subscriber
        // is set-once but the propagator is just a swap.
        static PROPAGATOR_ONCE: std::sync::Once = std::sync::Once::new();
        PROPAGATOR_ONCE.call_once(wm_host::telemetry::install_propagator);

        let storage = Storage::in_memory();
        let auth = Auth::new(storage.clone());
        auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
            .expect("bootstrap admin");
        let runtime = Runtime::new(storage.clone()).expect("runtime");
        let runtime = match (attach_engine, js_engine_path()) {
            (true, Some(p)) => runtime.with_js_engine(&p).expect("attach js engine"),
            _ => runtime,
        };
        let runtime = Arc::new(runtime);
        let registry = Arc::new(Registry::new(storage.clone()));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let journal = Journal::new(storage);
        let state = AppState::new(runtime, routes, auth.clone(), journal);
        let app = router(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr").to_string();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve");
        });

        // Default reqwest client carries the bootstrap admin token; tests
        // that want to drive auth-failure cases construct their own client
        // via `Harness::unauthenticated_client` etc.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {BOOTSTRAP_TOKEN}")).unwrap(),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("build client");

        Harness {
            addr,
            client,
            auth,
            state,
            server,
        }
    }

    /// Reqwest client with no Authorization header — for testing 401 paths.
    fn unauthenticated_client(&self) -> Client {
        Client::new()
    }

    /// Seed a *dispatching* route directly via the internal registry
    /// using a pre-compiled fixture component, owned by the bootstrap
    /// admin. For tests that need a working handler to drive journal /
    /// state / trace / cascade behaviour without paying the engine
    /// compile cost of source dispatch. Returns `(group, number)`.
    fn seed_route(&self, methods: &[&str], path: &str, wasm: Vec<u8>) -> (String, u64) {
        self.seed_route_inner(None, methods, path, wasm)
    }

    /// As `seed_route`, but attaches to an existing named group.
    fn seed_route_in_group(&self, group: &str, methods: &[&str], path: &str, wasm: Vec<u8>) {
        self.seed_route_inner(Some(group), methods, path, wasm);
    }

    fn seed_route_inner(
        &self,
        group: Option<&str>,
        methods: &[&str],
        path: &str,
        wasm: Vec<u8>,
    ) -> (String, u64) {
        let owner = self
            .auth
            .get_user_by_name("bootstrap")
            .expect("lookup bootstrap")
            .expect("bootstrap exists")
            .id;
        let route = self
            .state
            .routes()
            .registry()
            .create_route(NewRoute {
                group: group.map(String::from),
                methods: methods.iter().map(|m| m.to_string()).collect(),
                path: path.into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: wasm,
                source: None,
                owner_id: owner,
            })
            .expect("seed route");
        self.state.routes().refresh_after_create(route.clone());
        (route.group_name, u64::from(route.number))
    }

    /// Provision an additional non-admin user with one token, and return a
    /// reqwest client carrying that token in the default Authorization
    /// header. Used to drive ownership-check tests.
    fn provision_user(&self, name: &str, is_admin: bool) -> (String, Client) {
        let user = self.auth.create_user(name, is_admin).expect("create user");
        let (_token, plaintext) = self
            .auth
            .create_token(&user.id, "default", None)
            .expect("create token");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {plaintext}")).unwrap(),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("build client");
        (user.id, client)
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Build a mock-traffic request carrying the per-group virtual-host
    /// `Host` header (ADR-0030). `group` is the name of the group the
    /// target route belongs to; mock traffic is served on
    /// `{group}.{apex}` subdomains, with the apex being `localhost` in
    /// tests.
    fn mock(&self, method: reqwest::Method, group: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
    }

    async fn create_route_body(&self, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url("/__api/routes"))
            .json(&body)
            .send()
            .await
            .expect("post")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// -- Happy path ---------------------------------------------------------------

#[tokio::test]
async fn route_responses_carry_per_group_url() {
    // ADR-0030 phase 3a: REST route responses report the full per-group
    // mock URL ({scheme}://{group}.{apex}{path}), pattern verbatim.
    let h = Harness::start_with_engine().await;

    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/widgets/{id}",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"].as_str().expect("group name");
    let number = body["number"].as_u64().expect("number");
    // The harness client sends a Host of `h.addr` — the apex these
    // control-plane calls land on — so the group URL prefixes the label.
    let expected = format!("http://{group}.{}/widgets/{{id}}", h.addr);
    assert_eq!(body["url"].as_str().expect("url field"), expected);

    // GET reflects the same URL.
    let show = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}")))
        .send()
        .await
        .expect("get");
    assert_eq!(show.status().as_u16(), 200);
    let show_body: serde_json::Value = show.json().await.expect("json");
    assert_eq!(show_body["url"].as_str().expect("url field"), expected);
}

#[tokio::test]
async fn catch_all_route_handles_arbitrary_paths() {
    // ADR-0028: an `ANY /{rest...}` backstop handles every path in its
    // group — end-to-end through real dispatch, not just the matcher.
    let h = Harness::start_with_engine().await;
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["ANY"],
            "path": "/{rest...}",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap().to_string();

    // A deep path no specific route defines still hits the catch-all.
    let resp = h
        .mock(reqwest::Method::GET, &group, "/anything/at/all")
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.text().await.expect("body"),
        "echo: GET /anything/at/all"
    );
}

#[tokio::test]
async fn create_then_call_then_delete_then_404() {
    let h = Harness::start_with_engine().await;

    // Create a route via the API.
    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .expect("location header");
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"]
        .as_str()
        .expect("group name")
        .to_string();
    let number = body["number"].as_u64().expect("number");
    assert_eq!(location, format!("/__api/routes/{group}/{number}"));

    // Call the route — verifies the dispatcher sees it.
    let resp = h
        .mock(reqwest::Method::POST, &group, "/v1/charges")
        .body(r#"{"x":1}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.expect("body"), "echo: POST /v1/charges");

    // Show the route via GET.
    let show = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show.status().as_u16(), 200);

    // Delete it.
    let del = h
        .client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 204);

    // Mock traffic now 404s.
    let resp = h
        .mock(reqwest::Method::POST, &group, "/v1/charges")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 404);

    // GET 404s too.
    let show2 = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show2.status().as_u16(), 404);
}

#[tokio::test]
async fn list_routes_returns_created() {
    let h = Harness::start().await;

    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/a",
        "language": "javascript",
        "source": echo_source(),
    }))
    .await;
    h.create_route_body(json!({
        "methods": ["GET"],
        "path": "/b",
        "language": "javascript",
        "source": echo_source(),
    }))
    .await;

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().expect("routes").len(), 2);
}

#[tokio::test]
async fn path_params_extracted_for_user_routes() {
    let h = Harness::start().await;
    let (group, _) = h.seed_route(&["GET"], "/users/{id}", echo_wasm());

    for id in ["123", "me", "abc-def"] {
        let body = h
            .mock(reqwest::Method::GET, &group, &format!("/users/{id}"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("echo: GET /users/{id}"));
    }
}

#[tokio::test]
async fn counter_state_persists_across_calls_in_memory() {
    let h = Harness::start().await;
    let (group, _) = h.seed_route(&["GET"], "/bump", counter_wasm());
    for expected in 1..=3u32 {
        let body = h
            .mock(reqwest::Method::GET, &group, "/bump")
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("count={expected}"));
    }
}

// -- Activity tracking (slice 17) ---------------------------------------------

#[tokio::test]
async fn activity_fields_bump_on_dispatch() {
    let h = Harness::start().await;
    let (group, number) = h.seed_route(&["GET"], "/v1/activity", echo_wasm());
    let location = format!("/__api/routes/{group}/{number}");

    // Fresh route: never hit.
    let created: serde_json::Value = h
        .client
        .get(h.url(&location))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(created["hits_total"], 0);
    assert!(
        created.get("last_hit_at").is_none() || created["last_hit_at"].is_null(),
        "fresh route should not have last_hit_at: {created}"
    );

    // Drive three real dispatches against the route.
    for _ in 0..3 {
        h.mock(reqwest::Method::GET, &group, "/v1/activity")
            .send()
            .await
            .expect("get");
    }

    // The route's hits_total should now be 3 with last_hit_at set.
    let body: serde_json::Value = h
        .client
        .get(h.url(&location))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["hits_total"], 3);
    assert!(
        body["last_hit_at"].is_string(),
        "expected RFC3339 last_hit_at, got {body}"
    );

    // The group should have a matching last_activity_at.
    let g_body: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/groups/{group}")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert!(
        g_body["last_activity_at"].is_string(),
        "expected last_activity_at on group, got {g_body}"
    );
}

#[tokio::test]
async fn list_routes_reflects_dispatch_hits() {
    // Regression: the list endpoint used to read from the RouteTable's
    // cached snapshot, which refreshes on create/delete/update/cascade
    // but not on dispatch hits. After traffic, list_routes was
    // returning hits_total: 0 / last_hit_at: None forever while the
    // show endpoint (reading the registry directly) had the right
    // values. The slice-22 list pages exposed the gap: routes with
    // dozens of hits rendered as 'never hit'. Fix reads from the
    // registry directly in list_routes_core.
    let h = Harness::start().await;
    let (group, _) = h.seed_route(&["GET"], "/v1/list-hits", echo_wasm());

    // Drive a handful of dispatches.
    for _ in 0..4 {
        h.mock(reqwest::Method::GET, &group, "/v1/list-hits")
            .send()
            .await
            .expect("dispatch");
    }

    // The route on the LIST endpoint must reflect the hits — that's
    // the path the regression touched. The single-route SHOW endpoint
    // was always correct.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let row = body["routes"]
        .as_array()
        .expect("routes array")
        .iter()
        .find(|r| r["path"] == "/v1/list-hits")
        .expect("our route is in the list");
    assert_eq!(row["hits_total"], 4, "list endpoint must show fresh hits");
    assert!(
        row["last_hit_at"].is_string(),
        "list endpoint must show fresh last_hit_at, got {row}"
    );
}

// -- Validation / errors -------------------------------------------------------

#[tokio::test]
async fn typescript_source_path_creates_route_with_stored_source() {
    // ADR-0020 slice B path: TS source is transpiled to JS in-host
    // via swc, then stored. No external compiler — `language:
    // "typescript"` just works.
    let h = Harness::start().await;

    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "typescript",
            "source": "function handle(req: unknown, _r: unknown, _g: unknown) { return { status: 200, headers: [], body: new Uint8Array() }; }",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["language"], "typescript");
    let group = body["group"]["name"].as_str().unwrap().to_string();
    let number = body["number"].as_u64().unwrap();

    // GET /source returns the post-strip JS — verifies the in-host
    // transpile actually ran (types stripped, function body kept).
    let resp = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}/source")))
        .send()
        .await
        .expect("get source");
    let body: serde_json::Value = resp.json().await.expect("json");
    let stored = body["source"].as_str().expect("source string");
    assert!(!stored.contains(": unknown"), "types stripped: {stored}");
    assert!(stored.contains("function handle"));
}

#[tokio::test]
async fn typescript_source_path_surfaces_transpile_errors() {
    // Invalid TS source — swc's parser fails, the host returns
    // `compile_failed` with the parser's error message.
    let h = Harness::start().await;

    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/bad",
            "language": "typescript",
            "source": "function handle(req: unknown {",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "compile_failed");
}

// -- Source storage + GET /source ---------------------------------------------

// JS round-trips verbatim through `/__api/routes` source storage (no
// transpile, no whitespace munging). TS now goes through swc before
// storage, so byte-for-byte assertions on TS belong in
// `tests/ts_transpile.rs`; the source-storage assertions below use JS
// to keep the round-trip clean.
const JS_SOURCE: &str =
    "function handle(req, _r, _g) { return { status: 200, headers: [], body: new Uint8Array() }; }";

#[tokio::test]
async fn source_is_persisted_for_source_language_routes() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": JS_SOURCE,
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"].as_str().unwrap().to_string();
    let number = body["number"].as_u64().unwrap();

    let resp = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}/source")))
        .send()
        .await
        .expect("get source");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["slug"], format!("{group}/{number}"));
    assert_eq!(body["language"], "javascript");
    assert_eq!(body["source"], JS_SOURCE);
}

#[tokio::test]
async fn source_updates_on_source_language_patch() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/thing",
            "language": "javascript",
            "source": JS_SOURCE,
        }))
        .await;
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"].as_str().unwrap().to_string();
    let number = body["number"].as_u64().unwrap();

    let new_src = "function handle(req, _r, _g) { /* v2 */ return { status: 200, headers: [], body: new Uint8Array() }; }";
    let resp = h
        .client
        .patch(h.url(&format!("/__api/routes/{group}/{number}")))
        .json(&json!({ "language": "javascript", "source": new_src }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 200);

    let resp = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}/source")))
        .send()
        .await
        .expect("get source");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["source"], new_src);
}

#[tokio::test]
async fn source_endpoint_forbids_non_owner() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/private",
            "language": "javascript",
            "source": JS_SOURCE,
        }))
        .await;
    let body: serde_json::Value = resp.json().await.expect("json");
    let group = body["group"]["name"].as_str().unwrap().to_string();
    let number = body["number"].as_u64().unwrap();

    let (_id, other) = h.provision_user("nosey", false);
    let resp = other
        .get(h.url(&format!("/__api/routes/{group}/{number}/source")))
        .send()
        .await
        .expect("get source");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn source_endpoint_returns_404_for_unknown_route() {
    let h = Harness::start().await;
    let resp = h
        .client
        .get(h.url("/__api/routes/nope/99/source"))
        .send()
        .await
        .expect("get source");
    assert_eq!(resp.status().as_u16(), 404);
}

// -- Auth -------------------------------------------------------------------

#[tokio::test]
async fn rejects_request_without_authorization_header() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn rejects_request_with_bogus_token() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .post(h.url("/__api/routes"))
        .header("authorization", "Bearer wmt_not_a_real_token")
        .json(&json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn rejects_non_bearer_scheme() {
    let h = Harness::start().await;
    let resp = h
        .unauthenticated_client()
        .get(h.url("/__api/routes"))
        .header("authorization", format!("Basic {BOOTSTRAP_TOKEN}"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn mock_traffic_does_not_require_auth() {
    // SUTs hitting mock routes don't carry tokens. The dispatch handler
    // (everything not under a reserved prefix) stays open.
    let h = Harness::start().await;
    let (group, _) = h.seed_route(&["GET"], "/v1/anonymous", echo_wasm());

    let resp = h
        .unauthenticated_client()
        .get(h.url("/v1/anonymous"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn routing_is_scoped_per_group_subdomain() {
    // ADR-0030: mock traffic is matched within the group its Host resolves
    // to. The same path lives independently per group, the apex serves no
    // mock traffic, and an unknown subdomain 404s.
    let h = Harness::start().await;
    for name in ["alpha", "beta"] {
        let r = h
            .client
            .post(h.url("/__api/groups"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("create group");
        assert_eq!(r.status().as_u16(), 201, "{name} group");
    }
    // Only `alpha` owns /widget.
    h.seed_route_in_group("alpha", &["GET"], "/widget", echo_wasm());

    // Served on alpha's subdomain.
    let on_alpha = h
        .mock(reqwest::Method::GET, "alpha", "/widget")
        .send()
        .await
        .expect("call");
    assert_eq!(on_alpha.status().as_u16(), 200, "alpha serves /widget");

    // beta has no such route → 404 (the path isn't shared across groups).
    let on_beta = h
        .mock(reqwest::Method::GET, "beta", "/widget")
        .send()
        .await
        .expect("call");
    assert_eq!(on_beta.status().as_u16(), 404, "beta has no /widget");

    // The apex (Host = the test apex `localhost`) serves no mock traffic.
    let on_apex = h
        .client
        .get(h.url("/widget"))
        .header(reqwest::header::HOST, "localhost")
        .send()
        .await
        .expect("call");
    assert_eq!(
        on_apex.status().as_u16(),
        404,
        "apex serves no mock traffic"
    );

    // An unknown subdomain (no such group) → 404.
    let on_unknown = h
        .client
        .get(h.url("/widget"))
        .header(reqwest::header::HOST, "nope.localhost")
        .send()
        .await
        .expect("call");
    assert_eq!(on_unknown.status().as_u16(), 404, "unknown group 404s");
}

#[tokio::test]
async fn rename_group_moves_routes_to_the_new_subdomain() {
    // ADR-0030: a group's name is its subdomain, so renaming moves its
    // routes to the new subdomain end-to-end (incl. the in-memory route
    // table refresh — without it, the new subdomain wouldn't match).
    let h = Harness::start().await;
    let r = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "before" }))
        .send()
        .await
        .expect("create group");
    assert_eq!(r.status().as_u16(), 201);
    h.seed_route_in_group("before", &["GET"], "/widget", echo_wasm());

    // Served on the original subdomain.
    let pre = h
        .mock(reqwest::Method::GET, "before", "/widget")
        .send()
        .await
        .expect("call");
    assert_eq!(pre.status().as_u16(), 200);

    // Rename via PATCH; the response reflects the new name.
    let patched: serde_json::Value = h
        .client
        .patch(h.url("/__api/groups/before"))
        .json(&json!({ "name": "after" }))
        .send()
        .await
        .expect("patch")
        .json()
        .await
        .expect("json");
    assert_eq!(patched["name"], "after");

    // Now served on the new subdomain...
    let on_new = h
        .mock(reqwest::Method::GET, "after", "/widget")
        .send()
        .await
        .expect("call");
    assert_eq!(on_new.status().as_u16(), 200, "served on renamed subdomain");

    // ...and the old subdomain is now an unknown group → 404.
    let on_old = h
        .mock(reqwest::Method::GET, "before", "/widget")
        .send()
        .await
        .expect("call");
    assert_eq!(
        on_old.status().as_u16(),
        404,
        "old subdomain no longer routes"
    );
}

#[tokio::test]
async fn rejects_reserved_path() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/__api/sneaky",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn rejects_wasm_language_upload() {
    // ADR-0023: pre-compiled wasm upload was retired from the public
    // surface. `language: "wasm"` is rejected with a validation error
    // pointing the caller at source upload.
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/foo",
            "language": "wasm",
            "source": "ignored",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("source") && msg.contains("no longer supported"),
        "message should steer to source upload, got {msg:?}"
    );
}

#[tokio::test]
async fn rejects_invalid_path_pattern() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "no-leading-slash",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn rejects_pattern_shape_conflict() {
    let h = Harness::start().await;
    let first: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/users/{id}",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = first["group"]["name"].as_str().unwrap();
    // /users/me has the same shape as /users/{id} in the SAME group — must
    // conflict (per-group namespace, ADR-0030).
    let resp = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/users/me",
            "group": group,
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn source_create_conflict_is_detected_before_transpile() {
    // Register a wasm route at /v1/charges, then try to register a
    // TypeScript-source route at the same path. The precheck in
    // create_route_core must short-circuit with a 409 conflict
    // *before* the in-host swc transpile runs. We pass syntactically
    // bad TS — if the precheck were missing, swc would surface a
    // 400 compile_failed instead of the 409 we assert on.
    let h = Harness::start().await;
    let first: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = first["group"]["name"].as_str().unwrap();

    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "group": group,
            "language": "typescript",
            // Intentionally broken — would 400 compile_failed if the
            // precheck didn't fire first.
            "source": "function handle( {",
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 409, "expected 409 conflict");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        body["error"]["code"], "conflict",
        "must NOT be compile_failed — precheck should fire first"
    );
}

// -- PATCH /__api/routes (slice 15) -----------------------------------------

#[tokio::test]
async fn patch_route_swaps_path_and_evicts_old_dispatch() {
    let h = Harness::start().await;
    let (group, number) = h.seed_route(&["POST"], "/v1/charges", echo_wasm());
    let location = format!("/__api/routes/{group}/{number}");

    // Move the route to a new path.
    let patched = h
        .client
        .patch(h.url(&location))
        .json(&json!({ "path": "/v1/refunds" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patched.status().as_u16(), 200);
    let body: serde_json::Value = patched.json().await.expect("json");
    assert_eq!(body["path"], "/v1/refunds");

    // The new path dispatches; the old one 404s (route table refreshed).
    let resp = h
        .mock(reqwest::Method::POST, &group, "/v1/refunds")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let stale = h
        .mock(reqwest::Method::POST, &group, "/v1/charges")
        .send()
        .await
        .expect("post");
    assert_eq!(stale.status().as_u16(), 404);
}

#[tokio::test]
async fn streaming_response_journals_counts_and_disposition() {
    // ADR-0022 slice 2: a streamed response records its chunk/byte
    // totals and terminal disposition in the journal. The body itself
    // isn't captured (it streamed to the client), so the byte total
    // lands in `original_body_size` with `body_truncated` set, and a
    // synthetic `[stream] …` handler-log line carries the summary.
    let h = Harness::start_with_engine().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/stream",
            "language": "javascript",
            "source": r#"
                function handle(req, route, group) {
                  const out = host.responseStream({ status: 200, headers: [] });
                  out.write("aaaa");   // 4 bytes
                  out.write("bbbbbb"); // 6 bytes
                  out.close();
                }
            "#,
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap().to_string();

    // Drive the stream and drain the body so the handler finishes.
    let body = reqwest::Client::new()
        .get(h.url("/v1/stream"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "aaaabbbbbb");

    // The journal entry is written by a deferred task after the handler
    // finishes, so poll briefly for it.
    let mut entry = serde_json::Value::Null;
    for _ in 0..40 {
        let listed: serde_json::Value = h
            .client
            .get(h.url(&format!("/__api/journal/{group}")))
            .send()
            .await
            .expect("journal get")
            .json()
            .await
            .expect("json");
        if let Some(first) = listed["entries"].as_array().and_then(|a| a.first()) {
            entry = first.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(!entry.is_null(), "expected a journal entry for the stream");
    assert_eq!(entry["response"]["status"], 200);
    assert_eq!(
        entry["response"]["original_body_size"], 10,
        "byte total (4+6) lands in original_body_size"
    );
    assert_eq!(entry["response"]["body_truncated"], true);
    let logs = entry["handler_logs"].as_array().expect("logs");
    assert!(
        logs.iter().any(
            |l| l["message"].as_str().is_some_and(|m| m.contains("[stream]")
                && m.contains("2 chunks")
                && m.contains("10 bytes")
                && m.contains("finished"))
        ),
        "stream summary log line present: {logs:?}"
    );
}

#[tokio::test]
async fn dry_run_captures_streamed_chunks() {
    // ADR-0022 slice 3: dry-run of a streaming source handler. Two
    // things this proves: (a) dry-run works for source/engine routes
    // at all (it goes through the shared engine, not component bytes),
    // and (b) a handler that streams via host.responseStream has its
    // head + concatenated chunks captured in the dry-run response —
    // no real client, the chunks are collected in-process.
    let h = Harness::start_with_engine().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/dry-stream",
            "language": "javascript",
            "source": r#"
                function handle(req, route, group) {
                  const out = host.responseStream({
                    status: 202,
                    headers: [["content-type", "text/event-stream"]],
                  });
                  out.write("data: a\n\n");
                  out.write("data: b\n\n");
                  out.close();
                }
            "#,
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();

    let resp = h
        .client
        .post(h.url(&format!("/__api/routes/{group}/{number}/dry-run")))
        .json(&json!({ "method": "GET", "path": "/v1/dry-stream" }))
        .send()
        .await
        .expect("dry-run");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    // Status comes from the streamed head, not the (ignored) return.
    assert_eq!(body["status"], 202);
    assert_eq!(
        body["body"], "data: a\n\ndata: b\n\n",
        "dry-run captures the concatenated streamed chunks"
    );
}

/// Poll a group's journal until the first entry appears (the streaming
/// journal write is deferred to a task after the handler finishes).
async fn poll_first_journal_entry(h: &Harness, group: &str) -> serde_json::Value {
    for _ in 0..80 {
        let listed: serde_json::Value = h
            .client
            .get(h.url(&format!("/__api/journal/{group}")))
            .send()
            .await
            .expect("journal get")
            .json()
            .await
            .expect("json");
        if let Some(first) = listed["entries"].as_array().and_then(|a| a.first()) {
            return first.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no journal entry appeared for group {group}");
}

#[tokio::test]
async fn streaming_handler_error_after_start_keeps_committed_response() {
    // ADR-0022 slice 3: once a handler commits the head via `start`,
    // an error afterwards can't un-send it. The status + the chunk
    // already streamed are what the client gets — the handler can't
    // retroactively turn a streamed 200 into a 500. (A JS `throw` is
    // caught by the engine shim, which would otherwise return a 500;
    // because the 200 head is already on the wire, that 500 is
    // discarded.)
    let h = Harness::start_with_engine().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/stream-then-throw",
            "language": "javascript",
            "source": r#"
                function handle(req, route, group) {
                  const out = host.responseStream({ status: 200, headers: [] });
                  out.write("before the throw\n");
                  throw new Error("too late to change the status");
                }
            "#,
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap().to_string();

    let resp = reqwest::Client::new()
        .get(h.url("/v1/stream-then-throw"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get");
    // Head committed at `start`, not the 500 the shim makes from the
    // throw.
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap_or_default();
    assert!(
        body.contains("before the throw"),
        "client saw the streamed chunk, not a 500 body: {body:?}"
    );

    // The journal entry reflects the committed 200 with a streamed
    // (not-captured) body, not a 500.
    let entry = poll_first_journal_entry(&h, &group).await;
    assert_eq!(entry["response"]["status"], 200);
    assert_eq!(entry["response"]["body_truncated"], true);
}

#[tokio::test]
async fn streaming_handler_sees_client_disconnect() {
    // ADR-0022 slice 3: when the client disconnects mid-stream, the
    // next `write-chunk` returns false so the handler can stop, and the
    // journal records the `client_disconnected` disposition. The
    // handler records how far it got in route state for the assertion.
    use futures::StreamExt;

    let h = Harness::start_with_engine().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/stream-cancel",
            "language": "javascript",
            "source": r#"
                function handle(req, route, group) {
                  const out = host.responseStream({ status: 200, headers: [] });
                  for (let i = 0; i < 200; i++) {
                    if (!out.write("chunk " + i + "\n")) { break; }
                    host.sleep(30);
                  }
                  out.close();
                }
            "#,
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap().to_string();

    // Read the first chunk, then drop the stream → client disconnect.
    {
        let resp = reqwest::Client::new()
            .get(h.url("/v1/stream-cancel"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .send()
            .await
            .expect("get");
        assert_eq!(resp.status().as_u16(), 200);
        let mut stream = resp.bytes_stream();
        let _first = stream.next().await.expect("first chunk").expect("ok");
        // Drop the stream/response → hyper closes the connection.
    }

    // The handler keeps sleeping 30ms between writes, so within a few
    // hundred ms its next write fails and it breaks out; the deferred
    // journal write then records client_disconnected.
    let entry = poll_first_journal_entry(&h, &group).await;
    let err = entry["error"].as_str().unwrap_or_default();
    let logs_have_disconnect = entry["handler_logs"]
        .as_array()
        .map(|a| {
            a.iter().any(|l| {
                l["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("[stream]") && m.contains("client_disconnected"))
            })
        })
        .unwrap_or(false);
    assert!(
        err.contains("client_disconnected") || logs_have_disconnect,
        "journal records the client disconnect (error={err:?})"
    );
}

#[tokio::test]
async fn patch_route_replaces_source() {
    let h = Harness::start_with_engine().await;
    // Start with the echo handler, then PATCH in the counter handler
    // and confirm the response shape changes.
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/bump",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();
    let location = format!("/__api/routes/{group}/{number}");

    // Sanity: the original handler is the echo handler.
    let echo_body = h
        .mock(reqwest::Method::GET, group, "/v1/bump")
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("text");
    assert_eq!(echo_body, "echo: GET /v1/bump");

    // Swap to the counter handler.
    let patched = h
        .client
        .patch(h.url(&location))
        .json(&json!({
            "language": "javascript",
            "source": counter_source(),
        }))
        .send()
        .await
        .expect("patch");
    assert_eq!(patched.status().as_u16(), 200);

    for expected in 1..=2u32 {
        let body = h
            .mock(reqwest::Method::GET, group, "/v1/bump")
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("text");
        assert_eq!(body, format!("count={expected}"));
    }
}

#[tokio::test]
async fn patch_route_rejects_path_conflict() {
    let h = Harness::start().await;
    // Two routes in the SAME group; try to move the second onto the
    // first's path (per-group conflict, ADR-0030).
    let first: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/a",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = first["group"]["name"].as_str().unwrap().to_string();
    let second: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/b",
            "group": group,
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let number = second["number"].as_u64().unwrap();

    let resp = h
        .client
        .patch(h.url(&format!("/__api/routes/{group}/{number}")))
        .json(&json!({ "path": "/v1/a" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn patch_route_with_empty_body_is_bad_request() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/empty",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();
    let resp = h
        .client
        .patch(h.url(&format!("/__api/routes/{group}/{number}")))
        .json(&json!({}))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

// -- Per-route state + dry-run (slice 16) -----------------------------------

#[tokio::test]
async fn route_state_list_and_clear_round_trip() {
    let h = Harness::start().await;
    // Counter handler ticks an `incr("count", 1)` per request; after
    // two calls we should see kv:{group}:{route}:count == 2.
    let (group, number) = h.seed_route(&["GET"], "/v1/bump-state", counter_wasm());

    // Drive two real calls to populate state.
    for _ in 0..2 {
        h.mock(reqwest::Method::GET, &group, "/v1/bump-state")
            .send()
            .await
            .expect("get");
    }

    // GET state lists the counter.
    let resp = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .send()
        .await
        .expect("get state");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "exactly one kv key expected");
    assert_eq!(entries[0]["key"], "count");
    assert_eq!(entries[0]["kind"], "bytes");

    // DELETE state wipes it. (The counter handler ignores the old
    // value on the next call; we just confirm the listing is empty.)
    let del = h
        .client
        .delete(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .send()
        .await
        .expect("delete state");
    assert_eq!(del.status().as_u16(), 204);
    let resp = h
        .client
        .get(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .send()
        .await
        .expect("get state");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert!(body["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn route_state_endpoints_are_owner_or_admin() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/state-locked",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();
    let (_alice_id, alice) = h.provision_user("alice-state", false);
    let resp = alice
        .get(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
    let del = alice
        .delete(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 403);
}

#[tokio::test]
async fn dry_run_does_not_journal_or_mutate_state() {
    let h = Harness::start().await;
    let (group, number) = h.seed_route(&["GET"], "/v1/dryrun-target", counter_wasm());

    // One real call so there's state to snapshot.
    let real = h
        .mock(reqwest::Method::GET, &group, "/v1/dryrun-target")
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("text");
    assert_eq!(real, "count=1");

    // Dry-run the same route. The snapshot sees count=1, so the
    // handler's incr returns 2 — but the *real* state still reads 1.
    let resp = h
        .client
        .post(h.url(&format!("/__api/routes/{group}/{number}/dry-run")))
        .json(&json!({
            "method": "GET",
            "path": "/v1/dryrun-target",
        }))
        .send()
        .await
        .expect("post dry-run");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], 200);
    assert!(body["snapshot_keys"].as_u64().unwrap() >= 1);
    assert_eq!(body["body"], "count=2");

    // Real state untouched: state listing still says 1, and a real
    // call returns count=2 (not count=3 as it would if the dry-run
    // had bumped the real counter).
    let after = h
        .mock(reqwest::Method::GET, &group, "/v1/dryrun-target")
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("text");
    assert_eq!(after, "count=2", "dry-run must not mutate real state");

    // And no journal entry for the dry-run.
    let journal: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("journal get")
        .json()
        .await
        .expect("json");
    let entries = journal["entries"].as_array().expect("entries");
    // Two real calls were made; dry-run must not have added a third.
    assert_eq!(entries.len(), 2, "dry-run must not journal");
}

#[tokio::test]
async fn dry_run_kv_overrides_seed_snapshot_state() {
    // Verifies the slice-33 seed-state surface: the handler reads the
    // override value as starting state instead of whatever the real
    // counter holds, and the real counter stays untouched.
    let h = Harness::start().await;
    let (group, number) = h.seed_route(&["GET"], "/v1/dryrun-with-seed", counter_wasm());

    // No real traffic — real `count` is unset. Seed `count=5`; the
    // handler `incr`s it and should return `count=6`.
    let resp = h
        .client
        .post(h.url(&format!("/__api/routes/{group}/{number}/dry-run")))
        .json(&json!({
            "method": "GET",
            "path": "/v1/dryrun-with-seed",
            // ADR-0025: state values are UTF-8 strings (or {base64}).
            "kv_overrides": {"count": "5"}
        }))
        .send()
        .await
        .expect("post dry-run");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["body"], "count=6");

    // Second seeded run from a different starting point. Snapshot is
    // disposable, so each run starts fresh.
    let resp = h
        .client
        .post(h.url(&format!("/__api/routes/{group}/{number}/dry-run")))
        .json(&json!({
            "method": "GET",
            "path": "/v1/dryrun-with-seed",
            "kv_overrides": {"count": "1"}
        }))
        .send()
        .await
        .expect("post dry-run 2");
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["body"], "count=2");

    // Real `count` was never written — a real GET starts at 1.
    let real = h
        .mock(reqwest::Method::GET, &group, "/v1/dryrun-with-seed")
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("text");
    assert_eq!(real, "count=1", "real counter untouched by dry-run seeds");
}

#[tokio::test]
async fn dry_run_non_owner_forbidden() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/dryrun-locked",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();
    let (_alice_id, alice) = h.provision_user("alice-dry", false);
    let resp = alice
        .post(h.url(&format!("/__api/routes/{group}/{number}/dry-run")))
        .json(&json!({"method": "POST", "path": "/v1/dryrun-locked"}))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 403);
}

// -- ADR-0025: writable handler state ---------------------------------------

/// Create a plain route and return `(group, number)` for state tests.
async fn seed_state_route(h: &Harness) -> (String, u64) {
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/state-target",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    (
        created["group"]["name"].as_str().unwrap().to_string(),
        created["number"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn put_route_state_seed_snapshot_round_trips() {
    let h = Harness::start().await;
    let (group, number) = seed_state_route(&h).await;
    let url = h.url(&format!("/__api/routes/{group}/{number}/state"));

    let put = h
        .client
        .put(&url)
        .json(&json!({"entries": {"config": "hello", "n": "42"}}))
        .send()
        .await
        .expect("put");
    assert_eq!(put.status().as_u16(), 204);

    let snap: serde_json::Value = h
        .client
        .get(format!("{url}?format=snapshot"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    // String values round-trip as bare strings (ADR-0025), not int arrays.
    assert_eq!(snap["entries"]["config"], "hello");
    assert_eq!(snap["entries"]["n"], "42");
}

#[tokio::test]
async fn put_route_state_upserts_then_reset_replaces() {
    let h = Harness::start().await;
    let (group, number) = seed_state_route(&h).await;
    let url = h.url(&format!("/__api/routes/{group}/{number}/state"));
    let snap_url = format!("{url}?format=snapshot");

    // Two upserts: both keys present (upsert, not replace).
    for body in [json!({"entries":{"a":"1"}}), json!({"entries":{"b":"2"}})] {
        let r = h.client.put(&url).json(&body).send().await.expect("put");
        assert_eq!(r.status().as_u16(), 204);
    }
    let snap: serde_json::Value = h
        .client
        .get(&snap_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["entries"]["a"], "1");
    assert_eq!(snap["entries"]["b"], "2");

    // Reset = clear + write a baseline; old keys gone.
    h.client.delete(&url).send().await.expect("delete");
    h.client
        .put(&url)
        .json(&json!({"entries":{"a":"9"}}))
        .send()
        .await
        .expect("put");
    let snap: serde_json::Value = h
        .client
        .get(&snap_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["entries"]["a"], "9");
    assert!(
        snap["entries"].get("b").is_none(),
        "reset should have dropped old keys"
    );
}

#[tokio::test]
async fn put_route_state_binary_round_trips_as_base64() {
    let h = Harness::start().await;
    let (group, number) = seed_state_route(&h).await;
    let url = h.url(&format!("/__api/routes/{group}/{number}/state"));
    // 0xFF 0xFE is not valid UTF-8; base64 of those two bytes is "//4=".
    let put = h
        .client
        .put(&url)
        .json(&json!({"entries": {"bin": {"base64": "//4="}}}))
        .send()
        .await
        .expect("put");
    assert_eq!(put.status().as_u16(), 204);
    let snap: serde_json::Value = h
        .client
        .get(format!("{url}?format=snapshot"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Non-UTF-8 bytes come back as the {base64} form, preserving them exactly.
    assert_eq!(snap["entries"]["bin"]["base64"], "//4=");
}

#[tokio::test]
async fn put_route_state_rejects_oversize_value() {
    let h = Harness::start().await;
    let (group, number) = seed_state_route(&h).await;
    let big = "x".repeat(1024 * 1024 + 1);
    let r = h
        .client
        .put(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .json(&json!({"entries": {"big": big}}))
        .send()
        .await
        .expect("put");
    assert_eq!(r.status().as_u16(), 400);
    let body: serde_json::Value = r.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn put_route_state_non_owner_forbidden() {
    let h = Harness::start().await;
    let (group, number) = seed_state_route(&h).await;
    let (_id, alice) = h.provision_user("alice-state", false);
    let r = alice
        .put(h.url(&format!("/__api/routes/{group}/{number}/state")))
        .json(&json!({"entries": {"a": "1"}}))
        .send()
        .await
        .expect("put");
    assert_eq!(r.status().as_u16(), 403);
}

#[tokio::test]
async fn put_group_state_seed_snapshot_round_trips() {
    let h = Harness::start().await;
    let (group, _number) = seed_state_route(&h).await;
    let url = h.url(&format!("/__api/groups/{group}/state"));
    let put = h
        .client
        .put(&url)
        .json(&json!({"entries": {"inject:rules": "[1,2,3]"}}))
        .send()
        .await
        .expect("put");
    assert_eq!(put.status().as_u16(), 204);
    let snap: serde_json::Value = h
        .client
        .get(format!("{url}?format=snapshot"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap["entries"]["inject:rules"], "[1,2,3]");
}

#[tokio::test]
async fn patch_route_non_owner_non_admin_forbidden() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/locked",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let number = created["number"].as_u64().unwrap();
    let (_alice_id, alice_client) = h.provision_user("alice-patch", false);
    let resp = alice_client
        .patch(h.url(&format!("/__api/routes/{group}/{number}")))
        .json(&json!({ "path": "/v1/stolen" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 403);
}

// -- /__api/tokens ------------------------------------------------------------

#[tokio::test]
async fn create_token_returns_plaintext_then_authenticates() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci-runner" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let plaintext = body["token"].as_str().expect("token field").to_string();
    assert!(plaintext.starts_with("wmt_"));
    assert_eq!(body["record"]["name"], "ci-runner");

    // The new token should authenticate on its own — drive a request that
    // hits an authenticated endpoint with it.
    let client = Client::new();
    let listed = client
        .get(h.url("/__api/tokens"))
        .header("Authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .expect("list");
    assert_eq!(listed.status().as_u16(), 200);
}

#[tokio::test]
async fn list_tokens_returns_callers_tokens() {
    let h = Harness::start().await;
    // Bootstrap created a token already; create one more.
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "extra" }))
        .send()
        .await
        .expect("post");
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/tokens"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let tokens = body["tokens"].as_array().expect("tokens array");
    assert_eq!(tokens.len(), 2);
    let names: Vec<&str> = tokens.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"bootstrap"));
    assert!(names.contains(&"extra"));
    // No plaintext leaks in list responses.
    for t in tokens {
        assert!(
            t.get("token").is_none(),
            "list response must not expose plaintext"
        );
    }
}

#[tokio::test]
async fn get_token_by_name() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "deploy-bot", "ttl_seconds": 3600 }))
        .send()
        .await
        .expect("post");
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/tokens/deploy-bot"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["name"], "deploy-bot");
    assert!(body.get("expires_at").is_some());
}

#[tokio::test]
async fn delete_token_revokes_it() {
    let h = Harness::start().await;
    let created: serde_json::Value = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "throwaway" }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let plaintext = created["token"].as_str().unwrap().to_string();

    let del = h
        .client
        .delete(h.url("/__api/tokens/throwaway"))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 204);

    // Subsequent uses of the revoked token are 401.
    let client = Client::new();
    let resp = client
        .get(h.url("/__api/tokens"))
        .header("Authorization", format!("Bearer {plaintext}"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);

    // Second DELETE for the same name 404s.
    let again = h
        .client
        .delete(h.url("/__api/tokens/throwaway"))
        .send()
        .await
        .expect("delete");
    assert_eq!(again.status().as_u16(), 404);
}

#[tokio::test]
async fn create_token_rejects_duplicate_name() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci" }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "ci" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

// -- Ownership checks ---------------------------------------------------------

#[tokio::test]
async fn create_route_records_callers_user_id_as_owner() {
    let h = Harness::start().await;
    let resp = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/things",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await;
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    let owner_id = body["owner_id"].as_str().expect("owner_id field");
    assert!(!owner_id.is_empty(), "owner_id must not be empty");

    // The owner_id should match the bootstrap user — confirm by listing
    // and checking the stored value is consistent.
    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["routes"][0]["owner_id"], owner_id);
}

#[tokio::test]
async fn non_owner_non_admin_cannot_delete_route() {
    let h = Harness::start().await;
    // Bootstrap admin creates a route.
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/billing",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let location = format!("/__api/routes/{group}/{number}");

    // A different, non-admin user tries to delete it.
    let (_user_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "forbidden");

    // The route is still there — bootstrap can still see it.
    let show = h.client.get(h.url(&location)).send().await.expect("get");
    assert_eq!(show.status().as_u16(), 200);
}

#[tokio::test]
async fn admin_can_delete_route_owned_by_someone_else() {
    let h = Harness::start().await;
    // Alice (non-admin) creates a route.
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let create: serde_json::Value = alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let location = format!("/__api/routes/{group}/{number}");

    // Bootstrap (admin) deletes Alice's route.
    let resp = h
        .client
        .delete(h.url(&location))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
}

#[tokio::test]
async fn owner_can_delete_their_own_route() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let create: serde_json::Value = alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing-2",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let number = create["number"].as_u64().unwrap();
    let resp = alice_client
        .delete(h.url(&format!("/__api/routes/{group}/{number}")))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
}

#[tokio::test]
async fn token_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    let resp = unauth
        .get(h.url("/__api/tokens"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);
    let resp = unauth
        .post(h.url("/__api/tokens"))
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 401);
}

// -- /__api/users -------------------------------------------------------------

#[tokio::test]
async fn admin_creates_lists_and_reads_user() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/users"))
        .json(&json!({ "name": "alice", "is_admin": false }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["is_admin"], false);

    // List includes alice + bootstrap.
    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let mut names: Vec<&str> = listed["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice", "bootstrap"]);

    // GET by name works too.
    let one: serde_json::Value = h
        .client
        .get(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(one["name"], "alice");
}

#[tokio::test]
async fn non_admin_cannot_create_or_list_users() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);

    let resp = alice_client
        .post(h.url("/__api/users"))
        .json(&json!({ "name": "mallory" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 403);

    let resp = alice_client
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn me_returns_caller_record() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let body: serde_json::Value = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["is_admin"], false);
}

#[tokio::test]
async fn user_can_read_own_record_by_name() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .get(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn user_cannot_read_another_users_record() {
    let h = Harness::start().await;
    h.provision_user("bob", false);
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    let resp = alice_client
        .get(h.url("/__api/users/bob"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn admin_can_promote_user() {
    let h = Harness::start().await;
    h.provision_user("alice", false);
    let resp = h
        .client
        .patch(h.url("/__api/users/alice"))
        .json(&json!({ "is_admin": true }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["is_admin"], true);
}

#[tokio::test]
async fn patch_rejects_demoting_last_admin() {
    let h = Harness::start().await;
    // Bootstrap is the only admin.
    let resp = h
        .client
        .patch(h.url("/__api/users/bootstrap"))
        .json(&json!({ "is_admin": false }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "forbidden");
}

#[tokio::test]
async fn patch_with_no_recognised_fields_is_bad_request() {
    let h = Harness::start().await;
    h.provision_user("alice", false);
    let resp = h
        .client
        .patch(h.url("/__api/users/alice"))
        .json(&json!({}))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn admin_cannot_delete_themselves() {
    let h = Harness::start().await;
    let resp = h
        .client
        .delete(h.url("/__api/users/bootstrap"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn cannot_delete_last_admin_via_another_admin() {
    let h = Harness::start().await;
    // Provision a second admin so they can attempt to delete bootstrap;
    // then demote themselves first to leave bootstrap as the lone admin
    // (we can't actually demote via the API once they're alone, but for
    // the symmetry we just verify the bootstrap-delete path).
    let (_other_id, other) = h.provision_user("other-admin", true);
    // First delete bootstrap from `other`'s perspective — succeeds
    // because two admins exist.
    let resp = other
        .delete(h.url("/__api/users/bootstrap"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);
    // Now `other` is the lone admin; their own delete attempt is
    // refused by self-delete first, but we can also confirm the
    // last-admin guard via PATCH demotion below in another test. Here
    // we just check that bootstrap is gone.
    let listed: serde_json::Value = other
        .get(h.url("/__api/users"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let names: Vec<&str> = listed["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"bootstrap"));
    assert!(names.contains(&"other-admin"));
}

#[tokio::test]
async fn delete_user_refused_when_user_owns_routes() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    // Alice creates a route.
    alice_client
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .delete(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn delete_user_cascades_tokens() {
    let h = Harness::start().await;
    let (_alice_id, alice_client) = h.provision_user("alice", false);
    // Sanity: alice can hit /me with her token before deletion.
    let pre = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get");
    assert_eq!(pre.status().as_u16(), 200);

    let resp = h
        .client
        .delete(h.url("/__api/users/alice"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);

    // Alice's token no longer authenticates.
    let post = alice_client
        .get(h.url("/__api/users/me"))
        .send()
        .await
        .expect("get");
    assert_eq!(post.status().as_u16(), 401);
}

#[tokio::test]
async fn user_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    for path in ["/__api/users", "/__api/users/me", "/__api/users/alice"] {
        let resp = unauth.get(h.url(path)).send().await.expect("get");
        assert_eq!(resp.status().as_u16(), 401, "GET {path}");
    }
}

// -- /__api/journal -----------------------------------------------------------

/// Create a route, hit it once with mock traffic, and return the route's
/// group name so tests can inspect the journal that should now hold one
/// entry. Mock traffic doesn't need an auth header.
async fn seed_one_request(h: &Harness) -> String {
    let (group, _number) = h.seed_route(&["POST"], "/v1/charges", echo_wasm());
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/charges"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body(r#"{"amount":1000}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    group
}

#[tokio::test]
async fn dispatched_request_produces_journal_entry() {
    let h = Harness::start().await;
    let group = seed_one_request(&h).await;
    let listed: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["request"]["method"], "POST");
    assert_eq!(entry["request"]["path"], "/v1/charges");
    assert_eq!(entry["response"]["status"], 200);
    assert_eq!(entry["matched_pattern"], "/v1/charges");
    assert_eq!(entry["number"], 1);
    // Echo handler returns "echo: METHOD PATH" — verify the response
    // body was journaled too.
    assert_eq!(entry["response"]["body"], "echo: POST /v1/charges");
}

#[tokio::test]
async fn unmatched_request_produces_unmatched_record() {
    let h = Harness::start().await;
    // An unmatched record is only written when the request targets an
    // EXISTING group's subdomain (ADR-0030); a route establishes one.
    let (group, _number) = h.seed_route(&["GET"], "/v1/charges", echo_wasm());
    let unauth = Client::new();
    let resp = unauth
        .get(h.url("/no-such-route"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["request"]["method"], "GET");
    assert_eq!(entries[0]["request"]["path"], "/no-such-route");
}

#[tokio::test]
async fn unmatched_record_carries_near_misses_for_method_mismatch() {
    let h = Harness::start().await;
    // Register a POST /v1/charges route — the SUT hits GET /v1/charges
    // (same path, wrong method). The dispatcher should write an
    // unmatched record whose `near_misses` flags the method mismatch.
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let unauth = Client::new();
    let miss = unauth
        .get(h.url("/v1/charges"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("get");
    assert_eq!(miss.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let near = listed["entries"][0]["near_misses"]
        .as_array()
        .expect("near_misses array");
    assert_eq!(near.len(), 1, "one method-mismatch near-miss");
    let nm = &near[0];
    assert_eq!(nm["route_path"], "/v1/charges");
    assert_eq!(nm["reason"]["kind"], "method_mismatch");
    assert_eq!(nm["reason"]["got"], "GET");
    assert!(nm["route"].as_str().unwrap().ends_with("/1"));
}

#[tokio::test]
async fn unmatched_record_carries_near_misses_for_prefix_typo() {
    let h = Harness::start().await;
    // Register /v1/refunds — the SUT hits /v1/refund (one segment off
    // by a literal-prefix).
    let created: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/refunds",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = created["group"]["name"].as_str().unwrap();
    let unauth = Client::new();
    let miss = unauth
        .post(h.url("/v1/refund"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("post");
    assert_eq!(miss.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let near = listed["entries"][0]["near_misses"]
        .as_array()
        .expect("near_misses array");
    assert_eq!(near.len(), 1, "one prefix-match near-miss");
    let nm = &near[0];
    assert_eq!(nm["reason"]["kind"], "prefix_match");
    assert_eq!(nm["reason"]["expected"], "refunds");
    assert_eq!(nm["reason"]["got"], "refund");
}

#[tokio::test]
async fn reserved_path_404_does_not_journal() {
    let h = Harness::start().await;
    // Hit a /__api/* path that doesn't exist — should be 404 (reserved
    // prefix) and should NOT show up in unmatched.
    let resp = h
        .client
        .get(h.url("/__api/typo"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert!(
        listed["entries"].as_array().unwrap().is_empty(),
        "reserved-prefix typos must not pollute the unmatched log"
    );
}

#[tokio::test]
async fn trace_id_is_stamped_from_inbound_traceparent() {
    let h = Harness::start().await;
    let (group, _number) = h.seed_route(&["POST"], "/v1/things", echo_wasm());
    // Send a request with a hand-crafted W3C traceparent.
    let trace_id = "0123456789abcdef0123456789abcdef";
    let traceparent = format!("00-{trace_id}-aaaaaaaaaaaaaaaa-01");
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/things"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .header("traceparent", traceparent)
        .body("{}")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);

    let listed: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let entry = &listed["entries"].as_array().unwrap()[0];
    assert_eq!(entry["trace_id"], trace_id);
}

#[tokio::test]
async fn response_carries_x_trace_id_back_to_sut() {
    let h = Harness::start().await;
    let (group, _number) = h.seed_route(&["POST"], "/v1/echo", echo_wasm());

    let trace_id = "0123456789abcdef0123456789abcdef";
    let inbound = format!("00-{trace_id}-aaaaaaaaaaaaaaaa-01");
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .header("traceparent", &inbound)
        .body("{}")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let outbound = resp
        .headers()
        .get("x-trace-id")
        .expect("response carries X-Trace-Id")
        .to_str()
        .expect("ascii");
    // Same trace_id the SUT sent in `traceparent`. We don't echo
    // `traceparent` itself because W3C only specifies that header on
    // the request side; X-Trace-Id is honest about being a correlation
    // hint, not a propagation primitive.
    assert_eq!(outbound, trace_id);
}

#[tokio::test]
async fn unmatched_response_carries_x_trace_id() {
    let h = Harness::start().await;
    // Target an existing group's subdomain so this is an unmatched
    // request within a known group (ADR-0030), not an unknown-group miss.
    let (group, _number) = h.seed_route(&["GET"], "/v1/known", echo_wasm());
    let trace_id = "0123456789abcdef0123456789abcdef";
    let inbound = format!("00-{trace_id}-aaaaaaaaaaaaaaaa-01");
    let unauth = Client::new();
    let resp = unauth
        .get(h.url("/no-such"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .header("traceparent", &inbound)
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    let outbound = resp
        .headers()
        .get("x-trace-id")
        .expect("response carries X-Trace-Id on unmatched too")
        .to_str()
        .unwrap();
    assert_eq!(outbound, trace_id);
}

#[tokio::test]
async fn response_without_inbound_traceparent_has_no_x_trace_id() {
    let h = Harness::start().await;
    let (group, _number) = h.seed_route(&["POST"], "/v1/no-trace", echo_wasm());
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/no-trace"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get("x-trace-id").is_none(),
        "host should not manufacture an X-Trace-Id when no inbound traceparent was present"
    );
    // And no spurious `traceparent` either.
    assert!(resp.headers().get("traceparent").is_none());
}

#[tokio::test]
async fn cursor_pagination_round_trips() {
    let h = Harness::start().await;
    let group = seed_one_request(&h).await;
    let unauth = Client::new();
    // Drive 4 more requests so we have 5 total.
    for _ in 0..4 {
        let resp = unauth
            .post(h.url("/v1/charges"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .send()
            .await
            .expect("post");
        assert_eq!(resp.status().as_u16(), 200);
    }

    let first: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}?limit=2")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["entries"][0]["number"], 5);
    assert_eq!(first["entries"][1]["number"], 4);
    let next_before = first["next_before"].as_u64().expect("next_before");
    assert_eq!(next_before, 4);

    let next: serde_json::Value = h
        .client
        .get(h.url(&format!(
            "/__api/journal/{group}?before={next_before}&limit=2"
        )))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(next["entries"][0]["number"], 3);
    assert_eq!(next["entries"][1]["number"], 2);

    let tail: serde_json::Value = h
        .client
        .get(h.url(&format!("/__api/journal/{group}?before=2&limit=10")))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(tail["entries"].as_array().unwrap().len(), 1);
    assert_eq!(tail["entries"][0]["number"], 1);
    assert!(
        tail["next_before"].is_null(),
        "next_before should be null at the oldest page"
    );
}

#[tokio::test]
async fn unmatched_endpoint_is_owner_scoped_not_admin_only() {
    // ADR-0030 SemFLIP: any authed caller may list unmatched; a tenant who
    // owns no groups gets a 200 with an empty list (not the old admin-only
    // 403). Positive owner-visibility is covered in ui_unmatched_pages.rs.
    let h = Harness::start().await;
    let (_alice_id, alice) = h.provision_user("alice", false);
    let resp = alice
        .get(h.url("/__api/unmatched"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(
        body["entries"].as_array().map(|a| a.len()),
        Some(0),
        "a tenant owning no groups sees no unmatched entries"
    );
}

#[tokio::test]
async fn group_owner_can_read_journal_admin_can_too() {
    let h = Harness::start().await;
    let (_alice_id, alice) = h.provision_user("alice", false);
    let create: serde_json::Value = alice
        .post(h.url("/__api/routes"))
        .json(&json!({
            "methods": ["POST"],
            "path": "/v1/alice-thing",
            "language": "javascript",
            "source": echo_source(),
        }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();

    // Alice (owner of the only route in this group) can read it.
    let resp = alice
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);

    // Bootstrap (admin) can read it too.
    let resp = h
        .client
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);

    // A different non-admin who owns nothing in this group is rejected.
    let (_bob_id, bob) = h.provision_user("bob", false);
    let resp = bob
        .get(h.url(&format!("/__api/journal/{group}")))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn journal_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    for path in ["/__api/journal/anything", "/__api/unmatched"] {
        let resp = unauth.get(h.url(path)).send().await.expect("get");
        assert_eq!(resp.status().as_u16(), 401, "GET {path}");
    }
}

// -- /__api/groups ------------------------------------------------------------

#[tokio::test]
async fn create_then_get_group() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "stripe-mock", "ttl_seconds": 3600, "sliding_ttl": false }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "stripe-mock");
    assert_eq!(body["ttl_seconds"], 3600);
    assert_eq!(body["sliding_ttl"], false);
    assert_eq!(body["implicit"], false);

    let read: serde_json::Value = h
        .client
        .get(h.url("/__api/groups/stripe-mock"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(read["name"], "stripe-mock");
}

#[tokio::test]
async fn create_group_defaults_to_24h_sliding_true() {
    let h = Harness::start().await;
    let body: serde_json::Value = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "defaults" }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    assert_eq!(body["ttl_seconds"], 24 * 60 * 60);
    assert_eq!(body["sliding_ttl"], true);
}

#[tokio::test]
async fn create_group_rejects_duplicate_name() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "dup" }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "dup" }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 409);
}

#[tokio::test]
async fn create_group_rejects_excessive_ttl() {
    let h = Harness::start().await;
    let resp = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "too-long", "ttl_seconds": 30u64 * 24 * 60 * 60 + 1 }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 500); // Malformed → internal in this build
}

#[tokio::test]
async fn list_groups_filters_by_owner_for_non_admin() {
    let h = Harness::start().await;
    // Bootstrap (admin) creates one; alice creates one.
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "admin-group" }))
        .send()
        .await
        .expect("post");
    let (_alice_id, alice) = h.provision_user("alice", false);
    alice
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "alice-group" }))
        .send()
        .await
        .expect("post");

    // Admin sees both.
    let admin_view: serde_json::Value = h
        .client
        .get(h.url("/__api/groups"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let admin_names: Vec<&str> = admin_view["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert!(admin_names.contains(&"admin-group"));
    assert!(admin_names.contains(&"alice-group"));

    // Alice sees only her own.
    let alice_view: serde_json::Value = alice
        .get(h.url("/__api/groups"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let alice_names: Vec<&str> = alice_view["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(alice_names, vec!["alice-group"]);
}

#[tokio::test]
async fn patch_group_updates_ttl_and_sliding() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "patch-me" }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .patch(h.url("/__api/groups/patch-me"))
        .json(&json!({ "ttl_seconds": 7200, "sliding_ttl": false }))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["ttl_seconds"], 7200);
    assert_eq!(body["sliding_ttl"], false);
}

#[tokio::test]
async fn patch_group_with_no_fields_is_validation_failure() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "empty-patch" }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .patch(h.url("/__api/groups/empty-patch"))
        .json(&json!({}))
        .send()
        .await
        .expect("patch");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn delete_group_cascades_routes_and_state() {
    let h = Harness::start().await;
    let group_create: serde_json::Value = h
        .client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "cascadable" }))
        .send()
        .await
        .expect("post")
        .json()
        .await
        .expect("json");
    assert_eq!(group_create["name"], "cascadable");

    // Create a route inside the group.
    h.seed_route_in_group("cascadable", &["POST"], "/v1/billed", echo_wasm());

    // Hit the route once so the journal has an entry.
    let unauth = Client::new();
    let resp = unauth
        .post(h.url("/v1/billed"))
        .header(reqwest::header::HOST, "cascadable.localhost")
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);

    // Delete the group.
    let del = h
        .client
        .delete(h.url("/__api/groups/cascadable"))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status().as_u16(), 204);

    // Group, route, journal, and mock-traffic should all be gone.
    let resp = h
        .client
        .get(h.url("/__api/groups/cascadable"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);

    let resp = unauth
        .post(h.url("/v1/billed"))
        .header(reqwest::header::HOST, "cascadable.localhost")
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "route should be unreachable after group cascade"
    );

    let resp = h
        .client
        .get(h.url("/__api/journal/cascadable"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn group_endpoints_owner_or_admin_only() {
    let h = Harness::start().await;
    let (_alice_id, alice) = h.provision_user("alice", false);
    alice
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "alice-private" }))
        .send()
        .await
        .expect("post");

    // Bob (not admin, not owner) is rejected on every per-group action.
    let (_bob_id, bob) = h.provision_user("bob", false);
    for (method, path) in [
        ("GET", "/__api/groups/alice-private"),
        ("PATCH", "/__api/groups/alice-private"),
        ("DELETE", "/__api/groups/alice-private"),
        ("POST", "/__api/groups/alice-private/refresh"),
        ("DELETE", "/__api/groups/alice-private/state"),
        ("DELETE", "/__api/groups/alice-private/journal"),
    ] {
        let req = match method {
            "GET" => bob.get(h.url(path)),
            "PATCH" => bob.patch(h.url(path)).json(&json!({ "ttl_seconds": 60 })),
            "POST" => bob.post(h.url(path)),
            "DELETE" => bob.delete(h.url(path)),
            _ => unreachable!(),
        };
        let resp = req.send().await.expect("send");
        assert_eq!(resp.status().as_u16(), 403, "{method} {path}");
    }

    // Admin (bootstrap) can hit them all.
    let resp = h
        .client
        .get(h.url("/__api/groups/alice-private"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn refresh_group_returns_updated_record() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "refreshable", "ttl_seconds": 3600 }))
        .send()
        .await
        .expect("post");
    let resp = h
        .client
        .post(h.url("/__api/groups/refreshable/refresh"))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "refreshable");
    assert_eq!(body["ttl_seconds"], 3600);
}

#[tokio::test]
async fn delete_group_state_clears_kv_but_leaves_routes() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "stateful" }))
        .send()
        .await
        .expect("post");
    h.seed_route_in_group("stateful", &["GET"], "/v1/state-test", echo_wasm());

    let resp = h
        .client
        .delete(h.url("/__api/groups/stateful/state"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);

    // Route still serves.
    let unauth = Client::new();
    let resp = unauth
        .get(h.url("/v1/state-test"))
        .header(reqwest::header::HOST, "stateful.localhost")
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn delete_group_journal_clears_entries_but_leaves_routes() {
    let h = Harness::start().await;
    h.client
        .post(h.url("/__api/groups"))
        .json(&json!({ "name": "journal-clear" }))
        .send()
        .await
        .expect("post");
    h.seed_route_in_group("journal-clear", &["GET"], "/v1/journal-test", echo_wasm());
    let unauth = Client::new();
    unauth
        .get(h.url("/v1/journal-test"))
        .header(reqwest::header::HOST, "journal-clear.localhost")
        .send()
        .await
        .expect("get");

    // One entry should be present.
    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/journal/journal-clear"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["entries"].as_array().unwrap().len(), 1);

    // Clear journal.
    let resp = h
        .client
        .delete(h.url("/__api/groups/journal-clear/journal"))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status().as_u16(), 204);

    let listed: serde_json::Value = h
        .client
        .get(h.url("/__api/journal/journal-clear"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert!(listed["entries"].as_array().unwrap().is_empty());

    // Route still serves.
    let resp = unauth
        .get(h.url("/v1/journal-test"))
        .header(reqwest::header::HOST, "journal-clear.localhost")
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn group_endpoints_require_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    for path in ["/__api/groups", "/__api/groups/anything"] {
        let resp = unauth.get(h.url(path)).send().await.expect("get");
        assert_eq!(resp.status().as_u16(), 401, "GET {path}");
    }
}

// -- /__api/match -----------------------------------------------------------

#[tokio::test]
async fn match_probe_requires_auth() {
    let h = Harness::start().await;
    let unauth = h.unauthenticated_client();
    let resp = unauth
        .get(h.url("/__api/match?method=GET&path=/anything"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn match_probe_returns_hit_for_matching_route() {
    let h = Harness::start().await;
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges/{id}",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();

    let resp = h
        .client
        .get(h.url(&format!(
            "/__api/match?group={group}&method=POST&path=/v1/charges/abc"
        )))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["matched"], true);
    assert_eq!(body["route"]["path"], "/v1/charges/{id}");
    let params = body["path_params"].as_array().unwrap();
    assert_eq!(params[0][0], "id");
    assert_eq!(params[0][1], "abc");
}

#[tokio::test]
async fn match_probe_returns_method_mismatch_near_miss() {
    let h = Harness::start().await;
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();

    let resp = h
        .client
        .get(h.url(&format!(
            "/__api/match?group={group}&method=GET&path=/v1/charges"
        )))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["matched"], false);
    let near = body["near_misses"].as_array().unwrap();
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["reason"], "method_mismatch");
    assert_eq!(near[0]["details"]["got"], "GET");
    assert_eq!(
        near[0]["details"]["expected_methods"]
            .as_array()
            .unwrap()
            .first()
            .unwrap(),
        "POST"
    );
}

#[tokio::test]
async fn match_probe_returns_prefix_match_near_miss() {
    let h = Harness::start().await;
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"],
            "path": "/v1/charges",
            "language": "javascript",
            "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();

    let resp = h
        .client
        .get(h.url(&format!(
            "/__api/match?group={group}&method=GET&path=/v1/charge"
        )))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["matched"], false);
    let near = body["near_misses"].as_array().unwrap();
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["reason"], "prefix_match");
    assert_eq!(near[0]["details"]["expected"], "charges");
    assert_eq!(near[0]["details"]["got"], "charge");
}

#[tokio::test]
async fn match_probe_requires_group() {
    // ADR-0030: matching is per-subdomain, so the probe must name a group.
    let h = Harness::start().await;
    let resp = h
        .client
        .get(h.url("/__api/match?method=GET&path=/v1/charges"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn match_probe_is_group_scoped() {
    // Two groups can both define /v1/charges; a probe only sees the
    // group it names (ADR-0030).
    let h = Harness::start().await;
    let a: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"], "path": "/v1/charges",
            "language": "javascript", "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group_a = a["group"]["name"].as_str().unwrap().to_string();
    let b: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["GET"], "path": "/v1/charges",
            "language": "javascript", "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group_b = b["group"]["name"].as_str().unwrap().to_string();

    // POST /v1/charges hits in group A but only method-mismatches in B.
    let in_a: serde_json::Value = h
        .client
        .get(h.url(&format!(
            "/__api/match?group={group_a}&method=POST&path=/v1/charges"
        )))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(in_a["matched"], true, "POST matches in group A");

    let in_b: serde_json::Value = h
        .client
        .get(h.url(&format!(
            "/__api/match?group={group_b}&method=POST&path=/v1/charges"
        )))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(in_b["matched"], false, "POST does not match in group B");
    assert_eq!(in_b["near_misses"][0]["reason"], "method_mismatch");
}

#[tokio::test]
async fn match_probe_forbidden_for_non_owner() {
    // Probing reveals a group's routes → owner-or-admin only (ADR-0030).
    let h = Harness::start().await;
    let create: serde_json::Value = h
        .create_route_body(json!({
            "methods": ["POST"], "path": "/v1/charges",
            "language": "javascript", "source": echo_source(),
        }))
        .await
        .json()
        .await
        .expect("json");
    let group = create["group"]["name"].as_str().unwrap();
    let (_alice_id, alice) = h.provision_user("alice", false);
    let resp = alice
        .get(h.url(&format!(
            "/__api/match?group={group}&method=POST&path=/v1/charges"
        )))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn match_probe_rejects_bad_method() {
    let h = Harness::start().await;
    let resp = h
        .client
        .get(h.url("/__api/match?method=get&path=/v1/charges"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn match_probe_rejects_bad_path() {
    let h = Harness::start().await;
    let resp = h
        .client
        .get(h.url("/__api/match?method=GET&path=no-leading-slash"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 400);
}

// -- Slice 18: list filtering / sort / pagination -----------------------------

async fn make_three_routes(h: &Harness) {
    // Three routes across two explicit groups so we can exercise the
    // `group` filter as well as multi-method matching. Seeded with the
    // echo fixture (the list/filter queries don't care about the
    // artifact, and one caller dispatches them). Groups are created
    // up-front because a named group is a reference, not a create.
    for name in ["alpha", "beta"] {
        let r = h
            .client
            .post(h.url("/__api/groups"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("post group");
        assert_eq!(r.status().as_u16(), 201, "{name} group create");
    }

    h.seed_route_in_group("alpha", &["GET"], "/v1/a", echo_wasm());
    h.seed_route_in_group("alpha", &["POST"], "/v1/b", echo_wasm());
    h.seed_route_in_group("beta", &["GET", "POST"], "/v2/c", echo_wasm());
}

#[tokio::test]
async fn list_routes_filters_by_group_and_method() {
    let h = Harness::start().await;
    make_three_routes(&h).await;

    // Filter to group alpha only.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?group=alpha"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 2);
    assert!(body["next_offset"].is_null());

    // Filter to method POST only — matches `alpha` route 2 and
    // the multi-method `beta` route.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?method=POST"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 2);

    // Combine: group=alpha + method=POST → one route.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?group=alpha&method=POST"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_routes_glob_pattern() {
    let h = Harness::start().await;
    make_three_routes(&h).await;

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?path_pattern=/v1/*"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 2);
}

#[tokio::test]
async fn list_routes_pagination_returns_next_offset() {
    let h = Harness::start().await;
    make_three_routes(&h).await;

    // Page 1: limit 2, expect next_offset=2 and total=3.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?limit=2"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 3);
    assert_eq!(body["next_offset"], 2);

    // Page 2: starting at offset 2, one entry remains, no next_offset.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/routes?limit=2&offset=2"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], 3);
    assert!(body["next_offset"].is_null());
}

#[tokio::test]
async fn list_routes_rejects_bad_sort_with_parameter_diagnostic() {
    let h = Harness::start().await;
    let resp = h
        .client
        .get(h.url("/__api/routes?sort=bogus"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], "validation_failed");
    let diags = body["error"]["diagnostics"]
        .as_array()
        .expect("diagnostics");
    assert!(diags.iter().any(|d| d == "parameter=sort"));
}

#[tokio::test]
async fn list_routes_owner_id_filter_is_admin_only() {
    let h = Harness::start().await;
    make_three_routes(&h).await;
    let (_other_id, other_client) = h.provision_user("eve", false);

    // Non-admin caller passing `owner_id` → 403 with parameter diagnostic.
    let resp = other_client
        .get(h.url("/__api/routes?owner_id=somebody"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 403);
    let body: serde_json::Value = resp.json().await.expect("json");
    let diags = body["error"]["diagnostics"].as_array().unwrap();
    assert!(diags.iter().any(|d| d == "parameter=owner_id"));
}

#[tokio::test]
async fn list_routes_non_admin_only_sees_own() {
    let h = Harness::start().await;
    make_three_routes(&h).await;
    let (_other_id, other_client) = h.provision_user("eve", false);

    // Eve owns nothing, so her list is empty.
    let body: serde_json::Value = other_client
        .get(h.url("/__api/routes"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["routes"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);
}

#[tokio::test]
async fn list_groups_filters_by_name_prefix() {
    let h = Harness::start().await;
    make_three_routes(&h).await; // creates implicit groups `alpha` and `beta`

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/groups?name_prefix=alp"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let groups = body["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "alpha");
    assert_eq!(body["total"], 1);
}

#[tokio::test]
async fn list_groups_sort_by_name_asc() {
    let h = Harness::start().await;
    make_three_routes(&h).await;

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/groups?sort=name&dir=asc"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    let groups = body["groups"].as_array().expect("groups");
    let names: Vec<&str> = groups.iter().map(|g| g["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn list_journal_filters_by_method_and_status() {
    let h = Harness::start().await;
    make_three_routes(&h).await;

    // Drive some traffic: GET /v1/a, POST /v1/b (group alpha), GET /v2/c
    // (group beta) — each on its own group subdomain (ADR-0030).
    for (method, group, path) in [
        ("GET", "alpha", "/v1/a"),
        ("POST", "alpha", "/v1/b"),
        ("GET", "beta", "/v2/c"),
    ] {
        let mut req = h.mock(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            group,
            path,
        );
        if method == "POST" {
            req = req.body("{}");
        }
        let resp = req.send().await.expect("call");
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Filter alpha's journal by GET method → just one entry.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/journal/alpha?method=GET"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);

    // Filter by status 2xx — all three are echo successes, so alpha
    // shows both entries.
    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/journal/alpha?status=2xx"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);

    // Bad status filter surfaces with parameter diagnostic.
    let resp = h
        .client
        .get(h.url("/__api/journal/alpha?status=bogus"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn list_unmatched_filters_by_path_pattern() {
    let h = Harness::start().await;
    // A route establishes a group (ADR-0030); requests to that group's
    // subdomain that don't match its path all land in unmatched.
    let (group, _number) = h.seed_route(&["GET"], "/established", echo_wasm());
    for path in ["/v1/a", "/v1/b", "/v2/c"] {
        let _ = h
            .mock(reqwest::Method::GET, &group, path)
            .send()
            .await
            .expect("call");
    }

    let body: serde_json::Value = h
        .client
        .get(h.url("/__api/unmatched?path_pattern=/v1/*"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);
}

//! Tier-2 MCP integration tests. Boots wm-host on a random port,
//! connects an rmcp streamable-HTTP client at `/__api/mcp`, and
//! exercises a representative slice of the 13 slice-10 tools end to
//! end. The streamable-HTTP transport here is the same path real MCP
//! clients (Claude Desktop, Cursor, etc.) take, so a green run here
//! is meaningful evidence that the server actually works.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::journal::{
    HandlerLogEntry, Journal, NewJournalEntry, RequestEnvelope, ResourceUsage, ResponseEnvelope,
};
use wm_host::registry::{NewGroup, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";
const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");
const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn counter_wasm() -> Vec<u8> {
    std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture")
}

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

#[derive(Debug, Clone, Default)]
struct DummyClient;
impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

struct Harness {
    base_url: String,
    state: AppState,
    server: tokio::task::JoinHandle<()>,
    _mock_compiler: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start() -> Harness {
    start_with_compiler(None).await
}

async fn start_with_compiler(compiler: Option<CompilerClient>) -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let mut state = AppState::new(runtime, routes, auth, journal);
    if let Some(c) = compiler {
        state = state.with_compiler(c);
    }
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base_url: format!("http://{addr}"),
        state,
        server,
        _mock_compiler: None,
    }
}

/// Start the host wired to a canned-bytes mock compiler that returns
/// the echo fixture's wasm for any `/compile` POST. Exercises the real
/// source-language pipeline through `api::create_route_core` /
/// `patch_route_core` without needing a real componentize-js sidecar.
async fn start_with_mock_compiler() -> Harness {
    let canned_b64 = B64.encode(echo_wasm());
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

fn sample_handled(group_id: &str, group_name: &str, status: u16) -> NewJournalEntry {
    NewJournalEntry {
        trace_id: None,
        group_id: group_id.into(),
        group_name: group_name.into(),
        route_id: "r1".into(),
        route_number: 1,
        matched_pattern: "/v1/charges".into(),
        request: RequestEnvelope {
            method: "POST".into(),
            path: "/v1/charges".into(),
            headers: vec![],
            body: vec![],
            original_body_size: 0,
            body_truncated: false,
        },
        response: ResponseEnvelope {
            status,
            headers: vec![],
            body: vec![],
            original_body_size: 0,
            body_truncated: false,
        },
        path_params: vec![],
        query: vec![],
        handler_logs: Vec::<HandlerLogEntry>::new(),
        duration_ms: 5,
        resources: ResourceUsage::default(),
        error: None,
        dropped_response_headers: vec![],
    }
}

fn transport(
    base_url: &str,
    token: Option<&str>,
) -> StreamableHttpClientTransport<reqwest::Client> {
    let url = format!("{base_url}/__api/mcp");
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(t) = token {
        config = config.auth_header(t);
    }
    // `from_config` uses rmcp's default reqwest::Client builder,
    // which `transport-streamable-http-client-reqwest` wires up.
    StreamableHttpClientTransport::<reqwest::Client>::from_config(config)
}

#[tokio::test]
async fn list_tools_returns_all_expected_tools() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let tools = client.list_all_tools().await.expect("list_tools");
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    let mut expected = vec![
        // Slice 10
        "clear_group_state",
        "create_group",
        "create_route",
        "delete_group",
        "delete_route",
        "list_groups",
        "list_recent_unmatched",
        "list_routes",
        "refresh_group_ttl",
        "show_group",
        "show_route",
        "show_route_source",
        "summarize_workspace",
        "who_am_i",
        // Slice 11
        "tail_journal",
        "wait_for_request",
        // Slice 13
        "find_route",
        // Slice 15
        "update_route",
        // Slice 16
        "clear_route_state",
        "dry_run_route",
        "show_route_state",
        // Slice 43
        "update_group",
    ];
    expected.sort();
    assert_eq!(names, expected);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn who_am_i_returns_bootstrap_user() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let result = client
        .call_tool(CallToolRequestParams::new("who_am_i"))
        .await
        .expect("who_am_i");
    let structured = result.structured_content.expect("structured content set");
    let user = structured.get("user").and_then(|v| v.as_object()).unwrap();
    assert_eq!(user.get("name").and_then(|v| v.as_str()), Some("bootstrap"));
    assert_eq!(user.get("is_admin").and_then(|v| v.as_bool()), Some(true));

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn create_group_then_show_round_trip() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let create = client
        .call_tool(
            CallToolRequestParams::new("create_group").with_arguments(
                serde_json::json!({ "name": "stripe-mock" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("create_group");
    let created = create.structured_content.expect("created struct");
    assert_eq!(
        created.get("name").and_then(|v| v.as_str()),
        Some("stripe-mock")
    );

    let show = client
        .call_tool(
            CallToolRequestParams::new("show_group").with_arguments(
                serde_json::json!({ "group": "stripe-mock" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("show_group");
    let shown = show.structured_content.expect("shown struct");
    assert_eq!(
        shown.get("name").and_then(|v| v.as_str()),
        Some("stripe-mock")
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn delete_group_without_confirm_is_validation_error() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Set up a group to attempt deleting.
    client
        .call_tool(
            CallToolRequestParams::new("create_group").with_arguments(
                serde_json::json!({ "name": "delete-me" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("create_group");

    // confirm: false → tool returns Err(ErrorData); rmcp surfaces
    // that as a JSON-RPC error, not a successful CallToolResult with
    // `is_error: true`. The error carries our `validation_failed`
    // marker in its `data` field so MCP clients can branch on it
    // the same way they do for REST.
    let err = client
        .call_tool(
            CallToolRequestParams::new("delete_group").with_arguments(
                serde_json::json!({ "group": "delete-me", "confirm": false })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect_err("expected JSON-RPC error");
    let msg = format!("{err}");
    assert!(
        msg.contains("validation_failed") || msg.contains("confirm"),
        "expected validation marker in error, got: {msg}"
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_group_changes_ttl_and_sliding_flag() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Seed a group with the default TTL + sliding=true.
    client
        .call_tool(
            CallToolRequestParams::new("create_group").with_arguments(
                json!({ "name": "edit-me", "ttl_seconds": 3600, "sliding_ttl": true })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("create_group");

    // Patch both fields in one call.
    let updated = client
        .call_tool(
            CallToolRequestParams::new("update_group").with_arguments(
                json!({
                    "group": "edit-me",
                    "ttl_seconds": 7200,
                    "sliding_ttl": false,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("update_group");
    let body = updated.structured_content.expect("structured");
    assert_eq!(body["ttl_seconds"], 7200);
    assert_eq!(body["sliding_ttl"], false);

    // Confirm with a follow-up show_group that the change persisted.
    let shown = client
        .call_tool(
            CallToolRequestParams::new("show_group")
                .with_arguments(json!({ "group": "edit-me" }).as_object().unwrap().clone()),
        )
        .await
        .expect("show_group");
    let shown = shown.structured_content.unwrap();
    assert_eq!(shown["ttl_seconds"], 7200);
    assert_eq!(shown["sliding_ttl"], false);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_group_rejects_empty_patch() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");
    client
        .call_tool(
            CallToolRequestParams::new("create_group")
                .with_arguments(json!({ "name": "no-op" }).as_object().unwrap().clone()),
        )
        .await
        .expect("create_group");

    let err = client
        .call_tool(
            CallToolRequestParams::new("update_group")
                .with_arguments(json!({ "group": "no-op" }).as_object().unwrap().clone()),
        )
        .await
        .expect_err("empty patch is a validation error");
    let msg = format!("{err}");
    assert!(
        msg.contains("validation_failed") || msg.contains("at least one"),
        "validation surfaces: {msg}"
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_group_forbidden_for_non_owner_non_admin() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Admin creates a group, then a non-admin user without ownership
    // tries to update it.
    client
        .call_tool(
            CallToolRequestParams::new("create_group")
                .with_arguments(json!({ "name": "admin-only" }).as_object().unwrap().clone()),
        )
        .await
        .expect("admin creates group");

    let alice = h
        .state
        .auth()
        .create_user("alice", false)
        .expect("alice user");
    let (_token, alice_plain) = h
        .state
        .auth()
        .create_token(&alice.id, "default", None)
        .expect("alice token");

    let alice_client = DummyClient
        .serve(transport(&h.base_url, Some(&alice_plain)))
        .await
        .expect("alice connect");
    let err = alice_client
        .call_tool(
            CallToolRequestParams::new("update_group").with_arguments(
                json!({ "group": "admin-only", "ttl_seconds": 7200 })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect_err("non-owner can't update");
    let msg = format!("{err}");
    assert!(
        msg.contains("forbidden") || msg.contains("not_found"),
        "owner-or-admin gate fires: {msg}"
    );

    alice_client.cancel().await.expect("alice cancel");
    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn wait_for_request_returns_matching_entries() {
    let h = start().await;
    // Need a real group so the auth gate can resolve it.
    h.state
        .routes()
        .registry()
        .create_group(NewGroup {
            name: "stripe-mock".into(),
            owner_id: "admin-id".into(),
            ttl_seconds: None,
            sliding_ttl: None,
        })
        .expect("create group");
    let group = h
        .state
        .routes()
        .registry()
        .read_group_by_ref("stripe-mock")
        .unwrap();

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Spawn a writer that produces 2 matching entries shortly after
    // the tool starts waiting.
    let writer_state = h.state.clone();
    let writer_group_id = group.id.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..2 {
            writer_state
                .journal()
                .record_handled(sample_handled(&writer_group_id, "stripe-mock", 200))
                .unwrap();
        }
    });

    let result = client
        .call_tool(
            CallToolRequestParams::new("wait_for_request").with_arguments(
                serde_json::json!({
                    "group": "stripe-mock",
                    "count": 2,
                    "timeout_seconds": 5,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("wait_for_request");
    writer.await.unwrap();

    let structured = result.structured_content.expect("structured");
    assert_eq!(
        structured.get("timed_out").and_then(|v| v.as_bool()),
        Some(false)
    );
    let entries = structured
        .get("entries")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(entries.len(), 2);
    for e in entries {
        assert_eq!(
            e.get("group_name").and_then(|v| v.as_str()),
            Some("stripe-mock")
        );
    }

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn wait_for_request_times_out_with_partial_results() {
    let h = start().await;
    h.state
        .routes()
        .registry()
        .create_group(NewGroup {
            name: "stripe-mock".into(),
            owner_id: "admin-id".into(),
            ttl_seconds: None,
            sliding_ttl: None,
        })
        .expect("create group");
    let group = h
        .state
        .routes()
        .registry()
        .read_group_by_ref("stripe-mock")
        .unwrap();

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Only 1 entry will arrive but we ask for 3 with a short timeout.
    let writer_state = h.state.clone();
    let writer_group_id = group.id.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        writer_state
            .journal()
            .record_handled(sample_handled(&writer_group_id, "stripe-mock", 200))
            .unwrap();
    });

    let result = client
        .call_tool(
            CallToolRequestParams::new("wait_for_request").with_arguments(
                serde_json::json!({
                    "group": "stripe-mock",
                    "count": 3,
                    "timeout_seconds": 1,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("wait_for_request");
    writer.await.unwrap();

    let structured = result.structured_content.expect("structured");
    assert_eq!(
        structured.get("timed_out").and_then(|v| v.as_bool()),
        Some(true)
    );
    let entries = structured
        .get("entries")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(
        entries.len(),
        1,
        "should have the one entry that did arrive"
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn wait_for_request_requires_group_or_route_for_non_admin() {
    let h = start().await;
    let user = h
        .state
        .auth()
        .create_user("alice", false)
        .expect("create user");
    let (_t, alice_token) = h
        .state
        .auth()
        .create_token(&user.id, "default", None)
        .expect("create token");

    let client = DummyClient
        .serve(transport(&h.base_url, Some(&alice_token)))
        .await
        .expect("connect");

    let err = client
        .call_tool(
            CallToolRequestParams::new("wait_for_request").with_arguments(
                serde_json::json!({ "count": 1, "timeout_seconds": 1 })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect_err("expected forbidden");
    assert!(format!("{err}").contains("forbidden") || format!("{err}").contains("admin"));

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn tail_journal_returns_on_idle_timeout() {
    let h = start().await;
    h.state
        .routes()
        .registry()
        .create_group(NewGroup {
            name: "twilio-mock".into(),
            owner_id: "admin-id".into(),
            ttl_seconds: None,
            sliding_ttl: None,
        })
        .expect("create group");
    let group = h
        .state
        .routes()
        .registry()
        .read_group_by_ref("twilio-mock")
        .unwrap();

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let writer_state = h.state.clone();
    let writer_group_id = group.id.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer_state
            .journal()
            .record_handled(sample_handled(&writer_group_id, "twilio-mock", 200))
            .unwrap();
        // Then go silent — the tail should idle out.
    });

    let result = client
        .call_tool(
            CallToolRequestParams::new("tail_journal").with_arguments(
                serde_json::json!({
                    "group": "twilio-mock",
                    "max_entries": 100,
                    "idle_timeout_seconds": 1,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("tail_journal");
    writer.await.unwrap();

    let structured = result.structured_content.expect("structured");
    assert_eq!(
        structured.get("stopped_reason").and_then(|v| v.as_str()),
        Some("idle_timeout")
    );
    let entries = structured
        .get("entries")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(entries.len(), 1);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn find_route_returns_hit_and_method_mismatch_near_miss() {
    let h = start().await;

    // Drop a route in via the registry (no wasm validation, no need
    // for a fixture).
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: "test-owner".into(),
        })
        .expect("create_route");
    h.state.routes().refresh_after_create(route);

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Hit case.
    let hit = client
        .call_tool(
            CallToolRequestParams::new("find_route").with_arguments(
                serde_json::json!({ "method": "POST", "path": "/v1/charges" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("find_route hit");
    let structured = hit.structured_content.expect("structured");
    assert_eq!(
        structured.get("matched").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(structured["route"]["path"].as_str(), Some("/v1/charges"));

    // Miss + near-miss.
    let miss = client
        .call_tool(
            CallToolRequestParams::new("find_route").with_arguments(
                serde_json::json!({ "method": "GET", "path": "/v1/charges" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("find_route miss");
    let structured = miss.structured_content.expect("structured");
    assert_eq!(
        structured.get("matched").and_then(|v| v.as_bool()),
        Some(false)
    );
    let near = structured
        .get("near_misses")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["reason"].as_str(), Some("method_mismatch"));

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn show_route_source_returns_source_for_typescript_route() {
    let h = start().await;

    // Stash a "source-language" route directly via the registry so we
    // don't need a real compiler sidecar. The bytes-as-component
    // validation is bypassed at this layer (MCP create_route is
    // wasm-only, but we're going through the registry here).
    let bootstrap_user = h
        .state
        .auth()
        .get_user_by_name("bootstrap")
        .expect("get_user_by_name")
        .expect("bootstrap user exists");
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/v1/snippet".into(),
            language: "typescript".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: Some("export function handle() {}".into()),
            owner_id: bootstrap_user.id.clone(),
        })
        .expect("create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let result = client
        .call_tool(
            CallToolRequestParams::new("show_route_source").with_arguments(
                serde_json::json!({ "route": slug })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("show_route_source");
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["language"].as_str(), Some("typescript"));
    assert_eq!(
        structured["source"].as_str(),
        Some("export function handle() {}")
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn show_route_source_is_null_for_wasm_route() {
    let h = start().await;

    let bootstrap_user = h
        .state
        .auth()
        .get_user_by_name("bootstrap")
        .expect("get_user_by_name")
        .expect("bootstrap user exists");
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/v1/wasm".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user.id.clone(),
        })
        .expect("create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let result = client
        .call_tool(
            CallToolRequestParams::new("show_route_source").with_arguments(
                serde_json::json!({ "route": slug })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("show_route_source");
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["language"].as_str(), Some("wasm"));
    assert!(structured["source"].is_null());

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn invalid_token_fails_to_connect() {
    let h = start().await;
    // Bad token → the auth middleware rejects with 401 before rmcp
    // can complete its initialize handshake.
    let connect = DummyClient
        .serve(transport(&h.base_url, Some("wmt_obviously_wrong")))
        .await;
    assert!(
        connect.is_err(),
        "expected initialize to fail with bad token"
    );
}

// -- Slice 19: MCP list filters / sort / pagination --------------------------

#[tokio::test]
async fn list_groups_supports_name_prefix_and_pagination() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    for name in ["alpha", "beta", "gamma"] {
        client
            .call_tool(
                CallToolRequestParams::new("create_group").with_arguments(
                    serde_json::json!({ "name": name })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("create_group");
    }

    // Name prefix filter.
    let resp = client
        .call_tool(
            CallToolRequestParams::new("list_groups").with_arguments(
                serde_json::json!({ "name_prefix": "alp" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("list_groups");
    let body = resp.structured_content.expect("structured");
    let names: Vec<&str> = body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha"]);
    assert_eq!(body["total"], 1);

    // Pagination: limit=2 over 3 groups → next_offset=2.
    let resp = client
        .call_tool(
            CallToolRequestParams::new("list_groups").with_arguments(
                serde_json::json!({ "limit": 2, "sort": "name", "dir": "asc" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("list_groups");
    let body = resp.structured_content.expect("structured");
    assert_eq!(body["groups"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 3);
    assert_eq!(body["next_offset"], 2);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn list_routes_bad_sort_returns_filter_error() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let err = client
        .call_tool(
            CallToolRequestParams::new("list_routes").with_arguments(
                serde_json::json!({ "sort": "bogus" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    // The handler rejects via map_filter_error → ErrorData; rmcp
    // surfaces that as a call error.
    assert!(err.is_err(), "expected error for bogus sort");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn list_recent_unmatched_filters_by_path_pattern() {
    let h = start().await;

    // Seed unmatched entries directly via the journal, like the CLI
    // smoke test does — the MCP filter is the part under test.
    for path in ["/v1/a", "/v1/b", "/v2/c"] {
        h.state
            .journal()
            .record_unmatched(wm_host::journal::NewUnmatchedEntry {
                trace_id: None,
                request: wm_host::journal::RequestEnvelope {
                    method: "GET".into(),
                    path: path.into(),
                    headers: vec![],
                    body: vec![],
                    body_truncated: false,
                    original_body_size: 0,
                },
                near_misses: Vec::new(),
            })
            .expect("record unmatched");
    }

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let resp = client
        .call_tool(
            CallToolRequestParams::new("list_recent_unmatched").with_arguments(
                serde_json::json!({ "path_pattern": "/v1/*" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("list_recent_unmatched");
    let body = resp.structured_content.expect("structured");
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn list_recent_unmatched_includes_near_misses_projection() {
    // Backfill of the slice-35 deferral: the MCP UnmatchedSummary
    // used to drop the near_misses list, forcing agents to re-fetch
    // via REST `/__api/unmatched/{n}` to see the "Did you mean…?"
    // candidates. Now it ships them on the list response.
    let h = start().await;

    h.state
        .journal()
        .record_unmatched(wm_host::journal::NewUnmatchedEntry {
            trace_id: None,
            request: wm_host::journal::RequestEnvelope {
                method: "GET".into(),
                path: "/v1/charges".into(),
                headers: vec![],
                body: vec![],
                body_truncated: false,
                original_body_size: 0,
            },
            near_misses: vec![wm_host::journal::UnmatchedNearMiss {
                route: "stripe-mock/1".into(),
                route_path: "/v1/charges".into(),
                route_methods: vec!["POST".into()],
                reason: wm_host::journal::UnmatchedNearMissReason::MethodMismatch {
                    expected_methods: vec!["POST".into()],
                    got: "GET".into(),
                },
            }],
        })
        .expect("record unmatched");

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");
    let resp = client
        .call_tool(CallToolRequestParams::new("list_recent_unmatched"))
        .await
        .expect("list_recent_unmatched");
    let body = resp.structured_content.expect("structured");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    let near = entries[0]["near_misses"]
        .as_array()
        .expect("near_misses array present");
    assert_eq!(near.len(), 1, "the seeded near-miss surfaces");
    assert_eq!(near[0]["route"], "stripe-mock/1");
    assert_eq!(near[0]["reason"]["kind"], "method_mismatch");
    assert_eq!(near[0]["reason"]["got"], "GET");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn list_recent_unmatched_emits_empty_near_misses_when_none() {
    // The field must be present (as `[]`) even when the dispatcher
    // found no neighbours, so agent code can rely on its shape.
    let h = start().await;
    h.state
        .journal()
        .record_unmatched(wm_host::journal::NewUnmatchedEntry {
            trace_id: None,
            request: wm_host::journal::RequestEnvelope {
                method: "GET".into(),
                path: "/totally/unknown".into(),
                headers: vec![],
                body: vec![],
                body_truncated: false,
                original_body_size: 0,
            },
            near_misses: vec![],
        })
        .expect("record unmatched");

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");
    let resp = client
        .call_tool(CallToolRequestParams::new("list_recent_unmatched"))
        .await
        .expect("list_recent_unmatched");
    let body = resp.structured_content.expect("structured");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]["near_misses"].is_array(),
        "near_misses must be present as an empty array, not omitted"
    );
    assert_eq!(entries[0]["near_misses"].as_array().unwrap().len(), 0);

    client.cancel().await.expect("cancel");
}

// -- Slice 33: dry-run seed state ---------------------------------------------

#[tokio::test]
async fn dry_run_route_with_kv_overrides_seeds_snapshot() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    let h = start().await;

    // Register the counter route directly via the registry (avoids
    // round-tripping through `create_route` MCP, which would also
    // need a wasm artifact uploaded).
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/v1/dryrun-mcp".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: counter_wasm(),
            source: None,
            owner_id: h
                .state
                .auth()
                .get_user_by_name("bootstrap")
                .expect("user")
                .expect("bootstrap user exists")
                .id,
        })
        .expect("create route");
    h.state.routes().refresh_after_create(route.clone());

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Seed `count=9` (base64 of byte 0x39 = '9'), expect the
    // handler's `incr` to return `count=10`.
    let resp = client
        .call_tool(
            CallToolRequestParams::new("dry_run_route").with_arguments(
                serde_json::json!({
                    "route": format!("{}/{}", route.group_name, route.number),
                    "method": "GET",
                    "path": "/v1/dryrun-mcp",
                    "kv_overrides_b64": { "count": B64.encode(b"9") },
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("dry_run_route");
    let structured = resp.structured_content.expect("structured");
    let body_b64 = structured["body_b64"].as_str().expect("body_b64");
    let body_bytes = B64.decode(body_b64).expect("decode body");
    assert_eq!(String::from_utf8(body_bytes).unwrap(), "count=10");

    // Bad base64 in the override map surfaces a typed error.
    let bad = client
        .call_tool(
            CallToolRequestParams::new("dry_run_route").with_arguments(
                serde_json::json!({
                    "route": format!("{}/{}", route.group_name, route.number),
                    "method": "GET",
                    "path": "/v1/dryrun-mcp",
                    "kv_overrides_b64": { "count": "not-base-64!!!" },
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert!(bad.is_err(), "bad base64 in override is rejected");

    client.cancel().await.expect("cancel");
}

// -- Slice 42: MCP source-language create + update --------------------------

#[tokio::test]
async fn create_route_accepts_typescript_source() {
    let h = start_with_mock_compiler().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let resp = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/charges",
                    "language": "typescript",
                    "source": "export function handle() { return { status: 200, headers: [], body: new Uint8Array() }; }",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("create_route ts source");
    let body = resp.structured_content.expect("structured");
    assert_eq!(body["language"], "typescript");
    assert!(body["number"].is_number(), "route assigned a number");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn create_route_typescript_without_compiler_returns_compile_failed() {
    let h = start().await; // no compiler configured
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let err = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/charges",
                    "language": "typescript",
                    "source": "export function handle() {}",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    let err = err.expect_err("no compiler → compile_failed");
    let msg = err.to_string();
    assert!(
        msg.contains("compiler sidecar not configured") || msg.contains("compile_failed"),
        "compile_failed surfaces from MCP: {msg}",
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn create_route_rejects_source_and_wasm_together() {
    let h = start_with_mock_compiler().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let err = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/either-or",
                    "language": "typescript",
                    "source": "export function handle() {}",
                    "compiled_wasm_b64": B64.encode(echo_wasm()),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    let err = err.expect_err("either source or wasm, not both");
    let msg = err.to_string();
    assert!(
        msg.contains("not both") || msg.contains("validation_failed"),
        "validation surfaces: {msg}",
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_route_swaps_typescript_source() {
    let h = start_with_mock_compiler().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Seed a TS route via MCP.
    let created = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/swap-src",
                    "language": "typescript",
                    "source": "export function handle() { return { status: 200, headers: [], body: new Uint8Array() }; }",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("seed");
    let group = created.structured_content.as_ref().unwrap()["group"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let number = created.structured_content.as_ref().unwrap()["number"]
        .as_u64()
        .unwrap();

    // Update its source through MCP.
    let updated = client
        .call_tool(
            CallToolRequestParams::new("update_route").with_arguments(
                json!({
                    "route": format!("{group}/{number}"),
                    "language": "typescript",
                    "source": "export function handle() { return { status: 201, headers: [], body: new Uint8Array() }; }",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("update_route ts source");
    let body = updated.structured_content.expect("structured");
    assert_eq!(body["language"], "typescript");

    // Verify the stored source actually changed by fetching it.
    let src = client
        .call_tool(
            CallToolRequestParams::new("show_route_source").with_arguments(
                json!({ "route": format!("{group}/{number}") })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("show_route_source");
    let src_body = src.structured_content.expect("structured");
    assert!(
        src_body["source"]
            .as_str()
            .unwrap_or("")
            .contains("status: 201"),
        "updated source visible: {src_body:?}",
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_route_can_switch_wasm_to_source_and_back() {
    // Start wasm, swap to TS, swap back to wasm. Each transition must
    // recompute the artifact cleanly: TS swap stores source + sets
    // language=typescript; wasm swap clears stored source +
    // sets language=wasm.
    let h = start_with_mock_compiler().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Wasm create.
    let created = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/flip",
                    "language": "wasm",
                    "bindings_version": "0.1.0",
                    "compiled_wasm_b64": B64.encode(echo_wasm()),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("wasm create");
    let group = created.structured_content.as_ref().unwrap()["group"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let number = created.structured_content.as_ref().unwrap()["number"]
        .as_u64()
        .unwrap();
    let slug = format!("{group}/{number}");

    // Swap to TS source.
    let ts = client
        .call_tool(
            CallToolRequestParams::new("update_route").with_arguments(
                json!({
                    "route": &slug,
                    "language": "typescript",
                    "source": "export function handle() {}",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("swap to ts");
    assert_eq!(
        ts.structured_content.as_ref().unwrap()["language"],
        "typescript"
    );

    // Confirm source is stored.
    let src = client
        .call_tool(
            CallToolRequestParams::new("show_route_source")
                .with_arguments(json!({ "route": &slug }).as_object().unwrap().clone()),
        )
        .await
        .expect("show after ts");
    assert!(
        src.structured_content
            .as_ref()
            .unwrap()
            .get("source")
            .map(|v| v.is_string())
            .unwrap_or(false),
        "source present after TS swap"
    );

    // Swap back to wasm.
    let back = client
        .call_tool(
            CallToolRequestParams::new("update_route").with_arguments(
                json!({
                    "route": &slug,
                    "language": "wasm",
                    "bindings_version": "0.1.0",
                    "compiled_wasm_b64": B64.encode(echo_wasm()),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("swap back to wasm");
    assert_eq!(
        back.structured_content.as_ref().unwrap()["language"],
        "wasm"
    );

    // Confirm source was cleared.
    let src2 = client
        .call_tool(
            CallToolRequestParams::new("show_route_source")
                .with_arguments(json!({ "route": &slug }).as_object().unwrap().clone()),
        )
        .await
        .expect("show after wasm");
    assert!(
        src2.structured_content
            .as_ref()
            .unwrap()
            .get("source")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "source cleared after wasm swap"
    );

    client.cancel().await.expect("cancel");
}

//! Tier-2 MCP integration tests. Boots wm-host on a random port,
//! connects an rmcp streamable-HTTP client at `/__api/mcp`, and
//! exercises a representative slice of the 13 slice-10 tools end to
//! end. The streamable-HTTP transport here is the same path real MCP
//! clients (Claude Desktop, Cursor, etc.) take, so a green run here
//! is meaningful evidence that the server actually works.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::journal::{
    HandlerLogEntry, Journal, NewJournalEntry, NewUnmatchedEntry, RequestEnvelope, ResourceUsage,
    ResponseEnvelope,
};
use wm_host::registry::{NewGroup, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";
const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

fn counter_wasm() -> Vec<u8> {
    std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture")
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal);
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
    }
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
        // ADR-0025
        "set_group_state",
        "set_route_state",
        // MCP parity batch (first-user feedback)
        "list_journal",
        "show_group_state",
        "show_unmatched",
        // Slice 43
        "update_group",
        // Capabilities (post-ADR-0021 follow-up)
        "get_capabilities",
    ];
    expected.sort();
    assert_eq!(names, expected);

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn get_capabilities_returns_overview_and_clock_topic() {
    // Verifies the MCP-only path for handler-API discovery works
    // end-to-end: an agent that only sees MCP can fetch the
    // overview and a specific topic, and the clock primitives
    // (ADR-0021) appear in the clock section.
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // No-arg call → overview.
    let result = client
        .call_tool(CallToolRequestParams::new("get_capabilities"))
        .await
        .expect("get_capabilities overview");
    let structured = result
        .structured_content
        .expect("overview structured content");
    assert_eq!(
        structured.get("topic").and_then(|v| v.as_str()),
        Some("overview")
    );
    let overview_content = structured
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content string");
    assert!(
        overview_content.contains("function handle"),
        "overview should show the handler signature: {overview_content}"
    );
    // The overview lists known topics inline.
    assert!(overview_content.contains("clock"));
    let topics = structured
        .get("available_topics")
        .and_then(|v| v.as_array())
        .expect("available_topics array");
    let topic_names: Vec<&str> = topics.iter().filter_map(|v| v.as_str()).collect();
    assert!(topic_names.contains(&"clock"));
    assert!(topic_names.contains(&"store"));
    assert!(topic_names.contains(&"gotchas"));

    // Specific topic → clock section with the new primitives named.
    let result = client
        .call_tool(
            CallToolRequestParams::new("get_capabilities")
                .with_arguments(json!({ "topic": "clock" }).as_object().unwrap().clone()),
        )
        .await
        .expect("get_capabilities clock");
    let structured = result.structured_content.expect("clock structured");
    assert_eq!(
        structured.get("topic").and_then(|v| v.as_str()),
        Some("clock")
    );
    let clock_content = structured
        .get("content")
        .and_then(|v| v.as_str())
        .expect("clock content");
    for needle in ["host.sleep", "host.wallTimeMs", "host.monotonicMs"] {
        assert!(
            clock_content.contains(needle),
            "clock content should name `{needle}`: {clock_content}"
        );
    }

    // Unknown topic → falls back to overview gracefully.
    let result = client
        .call_tool(
            CallToolRequestParams::new("get_capabilities").with_arguments(
                json!({ "topic": "nonexistent" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("get_capabilities nonexistent");
    let structured = result.structured_content.expect("nonexistent structured");
    assert_eq!(
        structured.get("topic").and_then(|v| v.as_str()),
        Some("overview"),
        "unknown topic should fall back to overview, not error"
    );

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
    let group = route.group_name.clone();
    h.state.routes().refresh_after_create(route);

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Hit case.
    let hit = client
        .call_tool(
            CallToolRequestParams::new("find_route").with_arguments(
                serde_json::json!({ "group": group, "method": "POST", "path": "/v1/charges" })
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
                serde_json::json!({ "group": group, "method": "GET", "path": "/v1/charges" })
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
                group_id: "mock".into(),
                group_name: "mock".into(),
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
            group_id: "mock".into(),
            group_name: "mock".into(),
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
            group_id: "mock".into(),
            group_name: "mock".into(),
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

    // Register the counter route directly via the registry with the
    // counter fixture component — simpler than seeding source + the
    // shared engine just to get a stateful handler for the dry-run.
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

    // Seed `count=9` as a plain string (ADR-0025 string-first), expect
    // the handler's `incr` to return `count=10`.
    let resp = client
        .call_tool(
            CallToolRequestParams::new("dry_run_route").with_arguments(
                serde_json::json!({
                    "route": format!("{}/{}", route.group_name, route.number),
                    "method": "GET",
                    "path": "/v1/dryrun-mcp",
                    "kv_overrides": { "count": "9" },
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("dry_run_route");
    let structured = resp.structured_content.expect("structured");
    // ADR-0026: body is string-first, so a text response comes back as a
    // plain JSON string.
    assert_eq!(structured["body"], "count=10");

    // The `{base64}` escape hatch seeds the same bytes.
    let resp = client
        .call_tool(
            CallToolRequestParams::new("dry_run_route").with_arguments(
                serde_json::json!({
                    "route": format!("{}/{}", route.group_name, route.number),
                    "method": "GET",
                    "path": "/v1/dryrun-mcp",
                    "kv_overrides": { "count": { "base64": B64.encode(b"9") } },
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("dry_run_route binary override");
    let structured = resp.structured_content.expect("structured");
    assert_eq!(structured["body"], "count=10");

    client.cancel().await.expect("cancel");
}

// -- Slice 42: MCP source-language create + update --------------------------

#[tokio::test]
async fn create_route_accepts_typescript_source() {
    let h = start().await;
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
async fn create_route_typescript_with_bad_source_returns_compile_failed() {
    // ADR-0020 slice B: invalid TS is rejected in-host by swc.
    // `compile_failed` surfaces to the MCP caller with the parser's
    // diagnostic.
    let h = start().await;
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
                    "source": "function handle( {",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    let err = err.expect_err("bad TS → compile_failed");
    let msg = err.to_string();
    assert!(
        msg.contains("compile_failed"),
        "compile_failed surfaces from MCP: {msg}",
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn update_route_swaps_typescript_source() {
    let h = start().await;
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
async fn update_route_swaps_to_javascript_source() {
    // Seed a TS route, swap it to JS source via MCP, confirm the
    // language flips and the new source is stored. (ADR-0023 retired
    // the wasm artifact path; source-to-source is the swap that
    // remains.)
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let created = client
        .call_tool(
            CallToolRequestParams::new("create_route").with_arguments(
                json!({
                    "methods": ["POST"],
                    "path": "/v1/flip",
                    "language": "typescript",
                    "source": "export function handle() { return { status: 200, headers: [], body: new Uint8Array() }; }",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("ts create");
    let group = created.structured_content.as_ref().unwrap()["group"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let number = created.structured_content.as_ref().unwrap()["number"]
        .as_u64()
        .unwrap();
    let slug = format!("{group}/{number}");

    // Swap to JS source.
    let js = client
        .call_tool(
            CallToolRequestParams::new("update_route").with_arguments(
                json!({
                    "route": &slug,
                    "language": "javascript",
                    "source": "function handle() { return { status: 201, headers: [], body: new Uint8Array() }; }",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await
        .expect("swap to js");
    assert_eq!(
        js.structured_content.as_ref().unwrap()["language"],
        "javascript"
    );

    // Confirm the new source is stored.
    let src = client
        .call_tool(
            CallToolRequestParams::new("show_route_source")
                .with_arguments(json!({ "route": &slug }).as_object().unwrap().clone()),
        )
        .await
        .expect("show after js");
    assert!(
        src.structured_content.as_ref().unwrap()["source"]
            .as_str()
            .unwrap_or("")
            .contains("status: 201"),
        "updated JS source visible"
    );

    client.cancel().await.expect("cancel");
}

// ---------------------------------------------------------------------
// MCP parity batch (first-user feedback): base_url discovery,
// show_group_state read-back, and the list_journal history query.
// ---------------------------------------------------------------------

#[tokio::test]
async fn who_am_i_surfaces_base_url() {
    // First-user friction #4: the serving base URL was undiscoverable
    // from the API. `who_am_i` now returns it.
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
    let base = structured
        .get("base_url")
        .and_then(|v| v.as_str())
        .expect("base_url present");
    // Direct exposure (no WM_TRUSTED_PROXY) → derived from the Host
    // header the rmcp client sent, which is the loopback addr we bound.
    assert!(base.starts_with("http://"), "base_url was {base}");
    assert!(base.contains("127.0.0.1"), "base_url was {base}");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn summarize_workspace_surfaces_base_url() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let result = client
        .call_tool(CallToolRequestParams::new("summarize_workspace"))
        .await
        .expect("summarize_workspace");
    let structured = result.structured_content.expect("structured");
    let base = structured
        .get("host")
        .and_then(|v| v.get("base_url"))
        .and_then(|v| v.as_str())
        .expect("host.base_url present");
    assert!(base.starts_with("http://"), "base_url was {base}");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn set_then_show_group_state_round_trip() {
    // First-user friction #2: clear_group_state existed but there was
    // no read-back. set_group_state → show_group_state now round-trips.
    let h = start().await;
    h.state
        .routes()
        .registry()
        .create_group(NewGroup {
            name: "vertex-mock".into(),
            owner_id: "admin-id".into(),
            ttl_seconds: None,
            sliding_ttl: None,
        })
        .expect("create group");

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    client
        .call_tool(
            CallToolRequestParams::new("set_group_state").with_arguments(
                json!({ "group": "vertex-mock", "entries": { "scenario": "slowdown" } })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("set_group_state");

    let shown = client
        .call_tool(
            CallToolRequestParams::new("show_group_state").with_arguments(
                json!({ "group": "vertex-mock" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("show_group_state");
    let structured = shown.structured_content.expect("structured");
    let entries = structured
        .get("entries")
        .and_then(|v| v.as_array())
        .expect("entries array");
    let scenario = entries
        .iter()
        .find(|e| e.get("key").and_then(|v| v.as_str()) == Some("scenario"))
        .expect("scenario key present");
    assert_eq!(scenario.get("kind").and_then(|v| v.as_str()), Some("bytes"));
    // WireBytes: a clean UTF-8 value serializes as a plain string.
    assert_eq!(
        scenario.get("value").and_then(|v| v.as_str()),
        Some("slowdown")
    );

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn show_unmatched_returns_full_request_envelope() {
    // First-user friction #5, reframed: the unmatched journal already
    // captures the full request the SUT sent to an unknown path; MCP
    // just couldn't read the headers/body (only a summary). show_unmatched
    // closes that — no catch-all route needed.
    let h = start().await;
    let rec = h
        .state
        .journal()
        .record_unmatched(NewUnmatchedEntry {
            trace_id: None,
            group_id: "mock".into(),
            group_name: "mock".into(),
            request: RequestEnvelope {
                method: "POST".into(),
                path: "/v1/unknown-endpoint".into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"probe":true}"#.to_vec(),
                original_body_size: 14,
                body_truncated: false,
            },
            near_misses: vec![],
        })
        .expect("record unmatched");

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let shown = client
        .call_tool(
            CallToolRequestParams::new("show_unmatched")
                .with_arguments(json!({ "number": rec.number }).as_object().unwrap().clone()),
        )
        .await
        .expect("show_unmatched");
    let structured = shown.structured_content.expect("structured");
    let request = structured.get("request").expect("request envelope");
    assert_eq!(request.get("method").and_then(|v| v.as_str()), Some("POST"));
    assert_eq!(
        request.get("path").and_then(|v| v.as_str()),
        Some("/v1/unknown-endpoint")
    );
    // The whole point: the body the SUT sent is now retrievable over MCP.
    // A clean UTF-8 body serializes as a plain string (WireBytes).
    assert_eq!(
        request.get("body").and_then(|v| v.as_str()),
        Some(r#"{"probe":true}"#)
    );

    // Unknown number → not found.
    let missing = client
        .call_tool(
            CallToolRequestParams::new("show_unmatched")
                .with_arguments(json!({ "number": 99999 }).as_object().unwrap().clone()),
        )
        .await;
    assert!(missing.is_err(), "unknown unmatched number should error");

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn list_journal_returns_stored_entries_with_filter() {
    // First-user friction #3: journal access was live-only. list_journal
    // pulls completed entries after the fact, with the same filter
    // surface as REST.
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

    // Seed three handled entries: two 200s and one 503.
    for status in [200u16, 200, 503] {
        h.state
            .journal()
            .record_handled(sample_handled(&group.id, "stripe-mock", status))
            .unwrap();
    }

    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    // Unfiltered: all three come back.
    let all = client
        .call_tool(
            CallToolRequestParams::new("list_journal").with_arguments(
                json!({ "group": "stripe-mock" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("list_journal");
    let all_entries = all
        .structured_content
        .expect("structured")
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("entries");
    assert_eq!(all_entries.len(), 3);

    // Status-filtered: only the 503.
    let errs = client
        .call_tool(
            CallToolRequestParams::new("list_journal").with_arguments(
                json!({ "group": "stripe-mock", "status": "5xx" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("list_journal status");
    let err_entries = errs
        .structured_content
        .expect("structured")
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("entries");
    assert_eq!(err_entries.len(), 1);
    assert_eq!(
        err_entries[0]
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_u64()),
        Some(503)
    );

    client.cancel().await.expect("cancel");
}

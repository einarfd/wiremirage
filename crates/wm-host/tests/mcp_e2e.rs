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
use wm_host::auth::Auth;
use wm_host::journal::{
    HandlerLogEntry, Journal, NewJournalEntry, RequestEnvelope, ResourceUsage, ResponseEnvelope,
};
use wm_host::registry::{NewGroup, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

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

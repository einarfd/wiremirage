//! Tier-2 MCP integration tests. Boots wm-host on a random port,
//! connects an rmcp streamable-HTTP client at `/__api/mcp`, and
//! exercises a representative slice of the 13 slice-10 tools end to
//! end. The streamable-HTTP transport here is the same path real MCP
//! clients (Claude Desktop, Cursor, etc.) take, so a green run here
//! is meaningful evidence that the server actually works.

use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
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
    let app = router(AppState::new(runtime, routes, auth, journal));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base_url: format!("http://{addr}"),
        server,
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
async fn list_tools_returns_thirteen_slice_ten_tools() {
    let h = start().await;
    let client = DummyClient
        .serve(transport(&h.base_url, Some(BOOTSTRAP_TOKEN)))
        .await
        .expect("connect");

    let tools = client.list_all_tools().await.expect("list_tools");
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    let mut expected = vec![
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

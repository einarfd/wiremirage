//! Tier-2 tests for the stateless MCP transport (ADR-0037 item 3).
//!
//! Under more than one replica a client's follow-up request can land on
//! any replica, so the transport must not depend on server-side session
//! state. These drive `/api/mcp` with a plain HTTP client rather than
//! the rmcp client, because what needs asserting is the wire shape the
//! rmcp client would otherwise hide:
//!
//!   * `initialize` issues no `Mcp-Session-Id`
//!   * subsequent calls succeed carrying no session id — the
//!     multi-replica case, where consecutive requests may be served by
//!     different processes
//!   * `GET` and `DELETE` return 405, which the Streamable HTTP spec
//!     explicitly permits for a server offering no server-to-client
//!     stream and no client-terminated sessions
//!   * simple tools answer with plain JSON, not `text/event-stream`
//!   * the two long-blocking streaming tools emit a request-scoped
//!     progress heartbeat when — and only when — the caller supplies a
//!     `progressToken`. Request-scoped notifications ride the response
//!     stream of the request that asked for them, so they need no
//!     session and hold under this transport.
//!
//! The spec makes all three server-optional: a server "**MAY** assign a
//! session ID", and the client's obligation to echo one is conditional
//! on having been given one.

use std::sync::Arc;

use reqwest::Client;
use serde_json::{Value, json};
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";
const PROTOCOL_VERSION: &str = "2025-06-18";

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
    auth.bootstrap_admin("admin@test.example", BOOTSTRAP_TOKEN)
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base_url: format!("http://{addr}"),
        server,
    }
}

/// POST one JSON-RPC message, carrying no session id — deliberately, so
/// every call in this file exercises the path a fresh replica sees.
async fn rpc(h: &Harness, client: &Client, body: Value) -> reqwest::Response {
    client
        .post(format!("{}/api/mcp", h.base_url))
        .bearer_auth(BOOTSTRAP_TOKEN)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .json(&body)
        .send()
        .await
        .expect("send")
}

fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "stateless-test", "version": "0"}
        }
    })
}

#[tokio::test]
async fn initialize_issues_no_session_id() {
    let h = start().await;
    let client = Client::new();
    let resp = rpc(&h, &client, initialize_body()).await;

    assert_eq!(resp.status().as_u16(), 200, "initialize succeeds");
    assert!(
        resp.headers().get("mcp-session-id").is_none(),
        "no session id issued, so none can be expected back: {:?}",
        resp.headers()
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "plain JSON for a simple request-response call, got {ct:?}"
    );
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["result"]["protocolVersion"], PROTOCOL_VERSION);
}

#[tokio::test]
async fn independent_requests_without_a_session_id_all_succeed() {
    // The multi-replica case: each of these could have been served by a
    // different process, so none of them carries session state forward.
    let h = start().await;
    let client = Client::new();

    let resp = rpc(&h, &client, initialize_body()).await;
    assert_eq!(resp.status().as_u16(), 200, "initialize");

    let resp = rpc(
        &h,
        &client,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "tools/list with no session id");
    let body: Value = resp.json().await.expect("json");
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .len();
    assert!(tools > 0, "tools are listed statelessly");

    let resp = rpc(
        &h,
        &client,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "who_am_i", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "tools/call with no session id");
    let body: Value = resp.json().await.expect("json");
    assert!(
        body["result"]["isError"].as_bool() != Some(true),
        "call succeeded: {body}"
    );
}

#[tokio::test]
async fn get_and_delete_are_405() {
    let h = start().await;
    let client = Client::new();

    // A server offering no server-to-client stream at this endpoint
    // "**MUST** either return Content-Type: text/event-stream ... or
    // else return HTTP 405 Method Not Allowed".
    let resp = client
        .get(format!("{}/api/mcp", h.base_url))
        .bearer_auth(BOOTSTRAP_TOKEN)
        .header("accept", "text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 405, "GET offers no SSE stream");

    // "The server **MAY** respond to this request with HTTP 405 Method
    // Not Allowed, indicating that the server does not allow clients to
    // terminate sessions."
    let resp = client
        .delete(format!("{}/api/mcp", h.base_url))
        .bearer_auth(BOOTSTRAP_TOKEN)
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        405,
        "DELETE — there is no session to terminate"
    );
}

#[tokio::test]
async fn a_stale_session_id_is_ignored_rather_than_rejected() {
    // A legacy client that cached a session id from a previous
    // deployment must not be locked out; with no session store there is
    // nothing to look the id up in, so it is simply ignored.
    let h = start().await;
    let client = Client::new();

    let resp = client
        .post(format!("{}/api/mcp", h.base_url))
        .bearer_auth(BOOTSTRAP_TOKEN)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .header("mcp-session-id", "session-from-a-previous-life")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "an unknown session id is not an error when sessions don't exist"
    );
}

/// No `progressToken`, no progress: the response stays on the
/// `json_response` fast path, byte-for-byte as before the heartbeat
/// existed. This is the half that guarantees the change costs nothing
/// to a client that did not ask for it.
#[tokio::test]
async fn a_blocking_tool_without_a_progress_token_stays_plain_json() {
    let h = start().await;
    let client = Client::new();

    // Host-wide wait, which the bootstrapped admin is allowed, so the
    // test needs no group fixture.
    let resp = rpc(
        &h,
        &client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "wait_for_request",
                "arguments": {"timeout_seconds": 1}
            }
        }),
    )
    .await;

    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "no progress was asked for, so nothing precedes the result and \
         the response stays plain JSON, got {ct:?}"
    );
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["result"]["structuredContent"]["timed_out"], true);
}

/// With a `progressToken` the tool announces itself *before* blocking.
/// That first notification is the response's first byte, which is the
/// whole point: a client's first-byte timer and any proxy read timeout
/// see an alive connection instead of a silent one.
#[tokio::test]
async fn a_blocking_tool_beats_before_it_blocks() {
    let h = start().await;
    let client = Client::new();

    let started = std::time::Instant::now();
    let resp = rpc(
        &h,
        &client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "wait_for_request",
                "arguments": {"timeout_seconds": 2},
                "_meta": {"progressToken": "tok-1"}
            }
        }),
    )
    .await;

    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "progress can only be delivered on a stream, so rmcp switches \
         this one response over, got {ct:?}"
    );

    let body = resp.text().await.expect("body");
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).expect("frame is json"))
        .collect();

    assert!(
        frames.len() >= 2,
        "expected a heartbeat and then the result, got {frames:?}"
    );
    assert_eq!(
        frames[0]["method"], "notifications/progress",
        "the heartbeat comes first: {frames:?}"
    );
    assert_eq!(frames[0]["params"]["progressToken"], "tok-1");
    assert!(
        frames[0]["params"]["message"].is_string(),
        "the beat carries a human-readable status: {frames:?}"
    );

    let result = frames.last().expect("result frame");
    assert_eq!(result["id"], 1, "the tool result terminates the stream");
    assert_eq!(result["result"]["structuredContent"]["timed_out"], true);

    // The result itself must still take the full wait — beating is a
    // keep-alive, not an early return.
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "the tool still waited its full timeout"
    );
}

/// A heartbeat carries status, never journal data. The entries stay in
/// the single terminal result, so an agent's view of the tool is
/// unchanged — this is a transport-level fix, not a new surface.
#[tokio::test]
async fn a_heartbeat_carries_no_journal_entries() {
    let h = start().await;
    let client = Client::new();

    let resp = rpc(
        &h,
        &client,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "tail_journal",
                "arguments": {"idle_timeout_seconds": 1},
                "_meta": {"progressToken": "tok-2"}
            }
        }),
    )
    .await;

    let body = resp.text().await.expect("body");
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).expect("frame is json"))
        .collect();

    for frame in frames
        .iter()
        .filter(|f| f["method"] == "notifications/progress")
    {
        assert!(
            frame["params"].get("entries").is_none(),
            "progress is status only: {frame}"
        );
    }
    let result = frames.last().expect("result frame");
    assert!(
        result["result"]["structuredContent"]["entries"].is_array(),
        "the entries still arrive in the terminal result: {result}"
    );
}

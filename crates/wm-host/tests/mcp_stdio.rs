//! Tier-3 stdio sanity test for the MCP service.
//!
//! Wires a `WmMcpServer` over an in-memory duplex pipe pair and runs
//! the protocol-level handshake + `tools/list` against it from a
//! client serve loop. This exercises the parts of rmcp that are
//! transport-agnostic — service construction, initialize/list_tools —
//! ensuring we haven't accidentally encoded an HTTP-only assumption
//! into `WmMcpServer`.
//!
//! Tool *invocation* over stdio is intentionally not covered here.
//! Our tools pull `AuthContext` out of `http::request::Parts` that
//! the streamable-HTTP transport injects into the rmcp request
//! context. Stdio doesn't have HTTP parts, so a stdio session would
//! need its own auth bridge (a separate per-process credential
//! flow). Slice 10's deployment model treats stdio as a future
//! testing convenience, not a production transport, so we leave
//! that bridge for a later slice.

use std::sync::Arc;

use rmcp::model::ClientInfo;
use rmcp::{ClientHandler, ServiceExt};
use wm_host::AppState;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::mcp::WmMcpServer;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{Runtime, Storage};

#[derive(Debug, Clone, Default)]
struct DummyClient;
impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

fn empty_state() -> AppState {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    AppState::new(runtime, routes, auth, journal)
}

#[tokio::test]
async fn list_tools_works_over_stdio_duplex() {
    let (client_write, server_read) = tokio::io::duplex(4096);
    let (server_write, client_read) = tokio::io::duplex(4096);

    let server_handle = tokio::spawn(async move {
        let server = WmMcpServer::new(Arc::new(empty_state()));
        let running = server
            .serve((server_read, server_write))
            .await
            .expect("server.serve");
        // Wait for the client to disconnect; the test handles
        // shutdown by dropping its end of the duplex.
        let _ = running.waiting().await;
    });

    let client = DummyClient
        .serve((client_read, client_write))
        .await
        .expect("client.serve");

    let tools = client
        .list_all_tools()
        .await
        .expect("list_tools over stdio");
    assert_eq!(
        tools.len(),
        20,
        "stdio transport should expose the same 20 tools as HTTP \
         (13 slice-10 tools + 2 slice-11 streaming tools + 1 slice-13 find_route \
         + 1 slice-15 update_route + 3 slice-16 route-state/dry-run tools)"
    );

    client.cancel().await.expect("cancel");
    server_handle.await.expect("join server");
}

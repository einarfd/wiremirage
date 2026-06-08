//! MCP server (slice 10).
//!
//! Lives inside `wm-host` rather than a separate crate because the
//! tools need direct access to `AppState` (auth, registry, journal,
//! storage), and a separate crate would create a circular workspace
//! dep. ADR-0015 left this open as an implementation question; we
//! settle it as "module of the host" here.
//!
//! Mounted onto the host's axum router at `/__api/mcp` via
//! [`router`]. Same bearer-token auth as the rest of `/__api/*` —
//! the auth middleware authenticates, then injects an `Arc<AppState>`
//! plus the resolved `AuthContext` into the request extensions where
//! per-tool handlers can pull them out.

use std::sync::Arc;

use axum::Router;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;

use crate::AppState;

mod auth;
mod context;
mod error;
mod server;
mod tools;

pub use server::WmMcpServer;

/// Build the axum sub-router that exposes the MCP service at
/// `/__api/mcp` with bearer-token auth applied.
///
/// The returned router is intended to be `.merge`d into the host's
/// main router. Each request is authenticated by the same
/// `/__api/*` middleware before reaching the rmcp tower service.
pub fn router(state: AppState) -> Router {
    let config = mcp_server_config(state.mcp_allowed_hosts());
    let state = Arc::new(state);
    let factory_state = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(WmMcpServer::new(factory_state.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    // Apply our auth layer before rmcp gets the request.
    Router::new()
        .nest_service("/__api/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            state,
            auth::require_bearer,
        ))
}

/// Build the rmcp transport config, allowlisting the trusted-proxy
/// hostname(s) (`WM_TRUSTED_PROXY`, threaded through `AppState`; ADR-0027).
///
/// rmcp's streamable-HTTP server defaults its `allowed_hosts` to
/// `["localhost", "127.0.0.1", "::1"]` as DNS-rebinding protection.
/// Behind a reverse proxy on a real domain (the typical production
/// shape) the inbound `Host` header is the public hostname (e.g.
/// `wm.example.com`), which would otherwise be rejected with
/// `"disallowed Host header (possible DNS rebinding attempt)"` —
/// before our own auth middleware sees the request, surfacing to the
/// client as an opaque auth failure. `extra_hosts` (the
/// `WM_TRUSTED_PROXY` value) are added on top of the localhost defaults,
/// which remain so the dev path keeps working.
fn mcp_server_config(extra_hosts: &[String]) -> StreamableHttpServerConfig {
    let cfg = StreamableHttpServerConfig::default();
    if extra_hosts.is_empty() {
        return cfg;
    }
    let mut hosts = cfg.allowed_hosts.clone();
    hosts.extend(extra_hosts.iter().cloned());
    cfg.with_allowed_hosts(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;
    use crate::auth::Auth;
    use crate::journal::Journal;
    use crate::registry::Registry;
    use crate::route_table::RouteTable;
    use crate::runtime::Runtime;

    fn empty_state() -> AppState {
        let storage = Storage::in_memory();
        let auth = Auth::new(storage.clone());
        let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
        let registry = Arc::new(Registry::new(storage.clone()));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let journal = Journal::new(storage);
        AppState::new(runtime, routes, auth, journal)
    }

    /// All shipped tools must be registered with the expected
    /// names. This catches "the macro misfired" / "we renamed a tool
    /// and forgot somewhere" regressions; the names are the
    /// public-facing contract for MCP clients and the design doc.
    /// Slice 10 shipped 13 tools; slice 11 added 2 streaming tools;
    /// slice 13 added `find_route`; slice 15 added `update_route`;
    /// slice 16 added `show_route_state`, `clear_route_state`, and
    /// `dry_run_route`.
    #[test]
    fn server_exposes_all_expected_tools() {
        let server = WmMcpServer::new(Arc::new(empty_state()));
        let tools = server.tool_router.list_all();
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
            // ADR-0025
            "set_group_state",
            "set_route_state",
            // MCP parity batch (first-user feedback)
            "list_journal",
            "show_group_state",
            "show_unmatched",
            // Slice 36
            "show_route_source",
            // Slice 43
            "update_group",
            // Post-ADR-0021: MCP-side handler-API discoverability
            "get_capabilities",
            // Cross-surface parity pass
            "clear_journal",
            // Spec import/export parity
            "import_group",
            "export_group",
        ];
        expected.sort();
        assert_eq!(names, expected, "tool list drifted from the design");
    }

    /// Each tool's input schema must declare `type: object` so MCP
    /// clients can render parameter forms. rmcp's `#[tool]` macro
    /// produces this automatically from the `Parameters<T>` extractor;
    /// a regression here usually means we accidentally took the wrong
    /// extractor shape.
    #[test]
    fn every_tool_has_object_input_schema() {
        let server = WmMcpServer::new(Arc::new(empty_state()));
        for tool in server.tool_router.list_all() {
            assert_eq!(
                tool.input_schema.get("type"),
                Some(&serde_json::Value::String("object".to_string())),
                "tool {} has non-object input schema",
                tool.name,
            );
        }
    }

    /// Tools that take meaningful inputs must surface their fields in
    /// the input schema. We check `create_group` (most parameters)
    /// and `delete_group` (the `confirm` guard) as canaries — if the
    /// macro stopped emitting properties, both would fail.
    #[test]
    fn tools_with_inputs_advertise_their_fields() {
        let server = WmMcpServer::new(Arc::new(empty_state()));
        let tools = server.tool_router.list_all();
        let create = tools
            .iter()
            .find(|t| t.name == "create_group")
            .expect("create_group");
        let props = create
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("create_group input has object properties");
        for f in ["name", "ttl_seconds", "sliding_ttl"] {
            assert!(
                props.contains_key(f),
                "create_group missing field `{f}`: {props:?}",
            );
        }

        let delete = tools
            .iter()
            .find(|t| t.name == "delete_group")
            .expect("delete_group");
        let dprops = delete
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("delete_group input has object properties");
        for f in ["group", "confirm"] {
            assert!(
                dprops.contains_key(f),
                "delete_group missing field `{f}`: {dprops:?}",
            );
        }
    }

    /// ADR-0027: `WM_TRUSTED_PROXY` hosts are *added* to the rmcp
    /// Host-header allowlist, not a replacement — the localhost defaults
    /// stay so the dev path keeps working.
    #[test]
    fn mcp_config_allowlists_trusted_hosts_on_top_of_defaults() {
        let defaults = mcp_server_config(&[]);
        let with_host = mcp_server_config(&["wm.example.com".to_string()]);
        assert!(
            with_host
                .allowed_hosts
                .iter()
                .any(|h| h == "wm.example.com"),
            "configured host should be allowlisted"
        );
        assert_eq!(
            with_host.allowed_hosts.len(),
            defaults.allowed_hosts.len() + 1,
            "added on top of the localhost defaults, not replacing them"
        );
    }
}

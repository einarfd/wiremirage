//! MCP tools, grouped by domain. Each module attaches its tools to
//! `WmMcpServer::tool_router` via a `#[tool_router]` impl block.
//!
//! Slice 10 lands the 13 unblocked tools — those whose REST endpoints
//! already exist in the host. The 4 host-blocked tools
//! (`find_route`, `update_route`, `dry_run_route`, per-route state)
//! and the 2 streaming tools (`wait_for_request`, `tail_journal`,
//! both depending on `GET /__api/journal/tail` SSE) land in
//! follow-up slices.

pub mod discovery;
pub mod groups;
pub mod identity;
pub mod routes;
pub mod state;

//! Shared helpers for tools: pulling AuthContext out of the HTTP
//! request parts that rmcp's streamable-HTTP transport injects.
//!
//! The owner-or-admin gates themselves live in `crate::api`
//! (`ensure_group_owner_or_admin`, `ensure_route_owner_or_admin`,
//! `is_group_owner_or_admin`, `is_route_owner_or_admin`) so REST, MCP
//! and UI share one implementation; tools call them and translate the
//! `ApiError` via `super::error::map_api_error`.

use rmcp::ErrorData;

use crate::auth::AuthContext;

use super::error::internal;

/// Pull the AuthContext that the bearer-token middleware inserted
/// into the original axum request's extensions. The streamable-HTTP
/// transport copies `http::request::Parts` (including
/// `parts.extensions`) into rmcp's per-request context, where tools
/// extract it via `Extension<http::request::Parts>`.
pub fn auth_from(parts: &http::request::Parts) -> Result<AuthContext, ErrorData> {
    parts
        .extensions
        .get::<AuthContext>()
        .cloned()
        .ok_or_else(|| internal("auth context missing — middleware not applied?"))
}

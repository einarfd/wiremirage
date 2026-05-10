//! Shared helpers for tools: pulling AuthContext out of the HTTP
//! request parts that rmcp's streamable-HTTP transport injects, and
//! the standard owner-or-admin guard for per-group operations.

use rmcp::ErrorData;

use crate::AppState;
use crate::auth::AuthContext;
use crate::registry::Group;

use super::error::{forbidden, internal, map_registry_error, not_found};

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

/// Resolve a group reference (name or ULID) to its full record,
/// returning `forbidden` for non-admin callers that don't own it.
/// Mirrors `api::ensure_group_owner_or_admin`.
pub fn ensure_group_owner_or_admin(
    state: &AppState,
    auth: &AuthContext,
    group_ref: &str,
) -> Result<Group, ErrorData> {
    let group = state
        .routes()
        .registry()
        .read_group_by_ref(group_ref)
        .map_err(|_| not_found("group not found"))?;
    if !auth.is_admin && group.owner_id != auth.user_id {
        return Err(forbidden("must be the group's owner or an admin"));
    }
    Ok(group)
}

/// Resolve a route slug `{group}/{n}` to its full record, returning
/// `forbidden` for non-admin callers that don't own it.
pub fn ensure_route_owner_or_admin(
    state: &AppState,
    auth: &AuthContext,
    group_ref: &str,
    number: u32,
) -> Result<crate::registry::Route, ErrorData> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(group_ref, number)
        .map_err(map_registry_error)?;
    if !auth.is_admin && route.owner_id != auth.user_id {
        return Err(forbidden("must be the route's owner or an admin"));
    }
    Ok(route)
}

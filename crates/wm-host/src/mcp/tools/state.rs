//! State + dry-run tools — `clear_group_state`, `show_route_state`,
//! `clear_route_state`, `dry_run_route`. The per-route variants
//! (slice 16) sit alongside `clear_group_state` because the agent
//! mental model is "inspect / poke a route's state"; they share the
//! same auth/owner pattern.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dry_run::{DryRunRequest, dry_run};
use crate::mcp::context::{auth_from, ensure_group_owner_or_admin, ensure_route_owner_or_admin};
use crate::mcp::error::{map_registry_error, validation};
use crate::mcp::server::WmMcpServer;
use crate::state::{StateValue, decode_entries};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearGroupStateArgs {
    /// Group name or ULID.
    pub group: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearGroupStateResult {
    pub cleared: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RouteStateArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowRouteStateResult {
    pub entries: Vec<RouteStateEntry>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RouteStateEntry {
    pub key: String,
    /// Storage-level kind: `"bytes"`, `"list"`, `"hash"`, `"set"`, or
    /// `"other"` for co-resident exotic types on Valkey.
    pub kind: String,
    /// Base64-encoded value bytes, present only when `kind == "bytes"`.
    /// Collection-typed values report cardinality via `length`.
    pub value_b64: Option<String>,
    pub length: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetRouteStateArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
    /// Entries to upsert into the route's private `kv:` store. Values
    /// are UTF-8 strings, or `{ "base64": "<...>" }` for binary. Listed
    /// keys are written; others left untouched.
    pub entries: std::collections::HashMap<String, StateValue>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetGroupStateArgs {
    /// Group name or ULID.
    pub group: String,
    /// Entries to upsert into the group's shared `gkv:` store (what
    /// handlers read via `group-store`). Same value form as
    /// `set_route_state`.
    pub entries: std::collections::HashMap<String, StateValue>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetStateResult {
    /// Number of keys written.
    pub written: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearRouteStateArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
    /// Required guard. Must be `true`.
    pub confirm: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearRouteStateResult {
    /// Number of kv keys removed.
    pub cleared: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DryRunRouteArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
    /// HTTP method to pass to the handler.
    pub method: String,
    /// Request path. Must start with `/`. Defaults to the route's own
    /// path when omitted.
    pub path: Option<String>,
    /// Request headers as `[[key, value], ...]`. Defaults to none.
    pub headers: Option<Vec<(String, String)>>,
    /// Request body as base64-encoded bytes. Defaults to empty.
    pub body_b64: Option<String>,
    /// Override the path-params list the handler sees. Defaults to
    /// none.
    pub path_params: Option<Vec<(String, String)>>,
    /// Seed entries written into the route's private `kv:` namespace
    /// *after* the real-state deep-copy and *before* the handler
    /// runs — lets you exercise state-dependent branches without
    /// driving real traffic first. Values are UTF-8 strings, or
    /// `{ "base64": "<...>" }` for binary (`{ "counter": "4" }` seeds
    /// counter=`"4"`). Real state is never touched.
    pub kv_overrides: Option<std::collections::HashMap<String, StateValue>>,
    /// Same as `kv_overrides`, scoped to the group's shared `gkv:`
    /// namespace.
    pub gkv_overrides: Option<std::collections::HashMap<String, StateValue>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DryRunRouteResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Base64-encoded response body.
    pub body_b64: String,
    pub handler_logs: Vec<DryRunLogEntry>,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub snapshot_keys: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DryRunLogEntry {
    pub level: String,
    pub message: String,
    pub timestamp: String,
}

fn parse_route_slug(slug: &str) -> Result<(String, u32), ErrorData> {
    let (group, num) = slug
        .rsplit_once('/')
        .ok_or_else(|| validation(format!("expected `{{group}}/{{n}}` slug, got: {slug}")))?;
    let n = num
        .parse::<u32>()
        .map_err(|_| validation(format!("not a valid route number: {num}")))?;
    Ok((group.to_string(), n))
}

#[tool_router(router = state_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "clear_group_state",
        description = "Clear all per-route stores in a group plus the group's shared store. Routes themselves stay alive. Use between test phases when the whole group's state should reset."
    )]
    pub async fn clear_group_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClearGroupStateArgs>,
    ) -> Result<Json<ClearGroupStateResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        self.state
            .routes()
            .registry()
            .clear_group_state(&group.id)
            .map_err(map_registry_error)?;
        Ok(Json(ClearGroupStateResult { cleared: true }))
    }

    #[tool(
        name = "show_route_state",
        description = "List the kv entries stored under a single route. Bytes-typed values inline their bytes (base64); list / hash / set values report cardinality only. Owner-or-admin."
    )]
    pub async fn show_route_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RouteStateArgs>,
    ) -> Result<Json<ShowRouteStateResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_route_slug(&args.route)?;
        let _route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        let entries = self
            .state
            .routes()
            .registry()
            .list_route_state(&group_ref, number)
            .map_err(map_registry_error)?;
        let mcp_entries = entries
            .into_iter()
            .map(|e| RouteStateEntry {
                key: e.key,
                kind: e.kind,
                value_b64: e.value.as_ref().map(|v| B64.encode(v)),
                length: e.length,
            })
            .collect();
        Ok(Json(ShowRouteStateResult {
            entries: mcp_entries,
        }))
    }

    #[tool(
        name = "set_route_state",
        description = "Upsert kv entries into a route's private store (e.g. seed config or a baseline before a test). Values are UTF-8 strings, or { \"base64\": \"...\" } for binary. Listed keys are written; others untouched. Owner-or-admin."
    )]
    pub async fn set_route_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetRouteStateArgs>,
    ) -> Result<Json<SetStateResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_route_slug(&args.route)?;
        let _route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        let entries = decode_entries(args.entries).map_err(validation)?;
        let written = entries.len() as u64;
        self.state
            .routes()
            .registry()
            .set_route_state(&group_ref, number, entries)
            .map_err(map_registry_error)?;
        Ok(Json(SetStateResult { written }))
    }

    #[tool(
        name = "set_group_state",
        description = "Upsert kv entries into a group's shared store (what handlers read via group-store) — e.g. seed a reusable mock's config. Same value form as set_route_state. Owner-or-admin."
    )]
    pub async fn set_group_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetGroupStateArgs>,
    ) -> Result<Json<SetStateResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        let entries = decode_entries(args.entries).map_err(validation)?;
        let written = entries.len() as u64;
        self.state
            .routes()
            .registry()
            .set_group_state(&group.id, entries)
            .map_err(map_registry_error)?;
        Ok(Json(SetStateResult { written }))
    }

    #[tool(
        name = "clear_route_state",
        description = "Wipe a single route's private kv namespace. The route record stays alive; only its state is cleared. `confirm` must be `true`. Owner-or-admin."
    )]
    pub async fn clear_route_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClearRouteStateArgs>,
    ) -> Result<Json<ClearRouteStateResult>, ErrorData> {
        if !args.confirm {
            return Err(validation(
                "clear_route_state requires `confirm: true` — set it explicitly to proceed",
            ));
        }
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_route_slug(&args.route)?;
        let _route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        let cleared = self
            .state
            .routes()
            .registry()
            .clear_route_state(&group_ref, number)
            .map_err(map_registry_error)?;
        Ok(Json(ClearRouteStateResult { cleared }))
    }

    #[tool(
        name = "dry_run_route",
        description = "Run the route's handler against a synthetic request. State reads see a point-in-time snapshot; writes land in the snapshot and are discarded after the call. No journal entry is recorded. Owner-or-admin."
    )]
    pub async fn dry_run_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<DryRunRouteArgs>,
    ) -> Result<Json<DryRunRouteResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_route_slug(&args.route)?;
        let route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;

        let path = args.path.unwrap_or_else(|| route.path.clone());
        if !path.starts_with('/') {
            return Err(validation("dry-run path must start with /"));
        }
        let body = match args.body_b64.as_deref() {
            Some(s) => B64
                .decode(s)
                .map_err(|e| validation(format!("body_b64 is not valid base64: {e}")))?,
            None => Vec::new(),
        };
        let req = DryRunRequest {
            method: args.method,
            path,
            headers: args.headers.unwrap_or_default(),
            body,
            path_params: args.path_params,
            query: Vec::new(),
            kv_overrides: args.kv_overrides.unwrap_or_default(),
            gkv_overrides: args.gkv_overrides.unwrap_or_default(),
        };
        let result = dry_run(
            self.state.runtime().clone(),
            self.state.routes().clone(),
            route,
            req,
        )
        .await
        .map_err(|e| validation(format!("dry-run: {e}")))?;
        Ok(Json(DryRunRouteResult {
            status: result.status,
            headers: result.headers,
            body_b64: B64.encode(&result.body),
            handler_logs: result
                .handler_logs
                .into_iter()
                .map(|l| DryRunLogEntry {
                    level: l.level,
                    message: l.message,
                    timestamp: l.timestamp.to_rfc3339(),
                })
                .collect(),
            duration_ms: result.duration_ms,
            error: result.error,
            snapshot_keys: result.snapshot_keys,
        }))
    }
}

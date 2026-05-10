//! Discovery / orientation tools — `summarize_workspace`,
//! `list_recent_unmatched`.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::journal::UnmatchedCursor;
use crate::mcp::context::auth_from;
use crate::mcp::error::{forbidden, map_journal_error, map_registry_error};
use crate::mcp::server::WmMcpServer;

const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SummarizeWorkspaceResult {
    pub host: HostInfo,
    pub user: UserInfo,
    pub groups: Vec<GroupSummary>,
    pub recent_unmatched_count_5m: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HostInfo {
    pub version: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UserInfo {
    pub name: String,
    pub is_admin: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GroupSummary {
    pub name: String,
    pub id: String,
    pub owner_id: String,
    pub route_count: u32,
    pub ttl_seconds: u64,
    pub is_mine: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRecentUnmatchedArgs {
    /// Max entries to return. Capped at 100 host-side. Defaults to 20.
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRecentUnmatchedResult {
    pub entries: Vec<UnmatchedSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmatchedSummary {
    pub number: u64,
    pub method: String,
    pub path: String,
    pub created_at: String,
    pub trace_id: Option<String>,
}

#[tool_router(router = discovery_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "summarize_workspace",
        description = "Get a high-level summary of what's in this WireMirage instance: groups you can see, host version, and your user identity. Use this when starting work against an unfamiliar instance to orient yourself."
    )]
    pub async fn summarize_workspace(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<Json<SummarizeWorkspaceResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let groups = if auth.is_admin {
            self.state
                .routes()
                .registry()
                .list_groups()
                .map_err(map_registry_error)?
        } else {
            self.state
                .routes()
                .registry()
                .list_groups_by_owner(&auth.user_id)
                .map_err(map_registry_error)?
        };
        // Per-group route count: fetched via `list_routes_by_group`
        // would be one Valkey hit per group; for a summary tool we
        // can do a single `list_routes` and bucket on the client
        // side. Keeps the cost bounded.
        let all_routes = self
            .state
            .routes()
            .registry()
            .list_routes()
            .map_err(map_registry_error)?;
        let summaries = groups
            .into_iter()
            .map(|g| {
                let route_count = all_routes.iter().filter(|r| r.group_id == g.id).count() as u32;
                GroupSummary {
                    is_mine: g.owner_id == auth.user_id,
                    owner_id: g.owner_id,
                    id: g.id,
                    name: g.name,
                    ttl_seconds: g.ttl_seconds,
                    route_count,
                }
            })
            .collect();
        Ok(Json(SummarizeWorkspaceResult {
            host: HostInfo {
                version: HOST_VERSION.into(),
            },
            user: UserInfo {
                name: auth.user_name,
                is_admin: auth.is_admin,
            },
            groups: summaries,
            // The journal records unmatched but doesn't index by time
            // window in this slice. Reporting 0 keeps the field
            // schema-stable; a follow-up wires the real count once
            // the journal exposes a since-cursor.
            recent_unmatched_count_5m: 0,
        }))
    }

    #[tool(
        name = "list_recent_unmatched",
        description = "List recent unmatched-request entries — requests that arrived at the host but didn't match any route. Use this when the SUT seems to be hitting WireMirage but the mock isn't firing. Admin-only."
    )]
    pub async fn list_recent_unmatched(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListRecentUnmatchedArgs>,
    ) -> Result<Json<ListRecentUnmatchedResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        if !auth.is_admin {
            return Err(forbidden("admin-only"));
        }
        let cursor = UnmatchedCursor {
            before: None,
            limit: args.limit.unwrap_or(20) as usize,
        };
        let records = self
            .state
            .journal()
            .list_unmatched(cursor)
            .map_err(map_journal_error)?;
        let entries = records
            .into_iter()
            .map(|r| UnmatchedSummary {
                number: r.number,
                method: r.request.method,
                path: r.request.path,
                created_at: r.created_at.to_rfc3339(),
                trace_id: r.trace_id,
            })
            .collect();
        Ok(Json(ListRecentUnmatchedResult { entries }))
    }
}

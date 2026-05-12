//! Discovery / orientation tools — `summarize_workspace`,
//! `list_recent_unmatched`, `find_route`.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::api_filters::{glob_match, parse_since, validate_method};
use crate::journal::UnmatchedCursor;
use crate::mcp::context::auth_from;
use crate::mcp::error::{
    forbidden, map_filter_error, map_journal_error, map_registry_error, validation,
};
use crate::mcp::server::WmMcpServer;
use crate::mcp::tools::routes::RouteRecord;
use crate::registry::render_slug;
use crate::route_table::{MatchProbe, NearMissReason};

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
    /// Cursor for the next page: return entries with `number < before`.
    /// Omit to start at the newest.
    pub before: Option<u64>,
    /// HTTP method filter (uppercase, e.g. `GET`).
    pub method: Option<String>,
    /// `*`-glob over the request path.
    pub path_pattern: Option<String>,
    /// Lower bound on `created_at`. Duration suffix (`5m`, `1h`, `2d`,
    /// `30s`) or RFC 3339 timestamp.
    pub since: Option<String>,
    /// Upper bound on `created_at`.
    pub until: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRecentUnmatchedResult {
    pub entries: Vec<UnmatchedSummary>,
    /// Cursor for the next page; absent when the returned page
    /// reached the oldest entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmatchedSummary {
    pub number: u64,
    pub method: String,
    pub path: String,
    pub created_at: String,
    pub trace_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FindRouteArgs {
    /// HTTP method (e.g. `GET`, `POST`, `ANY`).
    pub method: String,
    /// Request path (must start with `/`).
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FindRouteResult {
    /// `true` when a route was found that handles `(method, path)`.
    pub matched: bool,
    /// The matched route record. Set iff `matched == true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteRecord>,
    /// Path parameters extracted from the request path. Set iff
    /// `matched == true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_params: Option<Vec<(String, String)>>,
    /// Routes that almost matched. Empty when `matched == true`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub near_misses: Vec<FindRouteNearMiss>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FindRouteNearMiss {
    /// `{group}/{n}` slug.
    pub route: String,
    pub route_id: String,
    pub route_path: String,
    pub reason: FindRouteNearMissReason,
    /// Free-form details — shape depends on `reason`. See the host's
    /// REST `/__api/match` endpoint for the matching JSON contract.
    pub details: JsonValue,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindRouteNearMissReason {
    MethodMismatch,
    PrefixMatch,
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
        description = "List recent unmatched-request entries — requests that arrived at the host but didn't match any route. Use this when the SUT seems to be hitting WireMirage but the mock isn't firing. Cursor-paginated (`before` / `limit`). Optional filters: `method`, `path_pattern` glob, `since`/`until` against `created_at`. Admin-only."
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
        let method = args
            .method
            .as_deref()
            .map(validate_method)
            .transpose()
            .map_err(map_filter_error)?;
        let now = chrono::Utc::now();
        let since = args
            .since
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(map_filter_error)?;
        let until = args
            .until
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(map_filter_error)?;
        let path_pattern = args.path_pattern.clone();

        let cursor = UnmatchedCursor {
            before: args.before,
            limit: args.limit.unwrap_or(20) as usize,
        };
        let records = self
            .state
            .journal()
            .list_unmatched(cursor)
            .map_err(map_journal_error)?;
        let next_before = records.last().filter(|e| e.number > 1).map(|e| e.number);

        let any_filter =
            method.is_some() || path_pattern.is_some() || since.is_some() || until.is_some();
        let entries: Vec<UnmatchedSummary> = records
            .into_iter()
            .filter(|r| {
                if !any_filter {
                    return true;
                }
                if let Some(m) = method.as_deref()
                    && !m.eq_ignore_ascii_case(&r.request.method)
                {
                    return false;
                }
                if let Some(p) = path_pattern.as_deref()
                    && !glob_match(p, &r.request.path)
                {
                    return false;
                }
                if let Some(s) = since
                    && r.created_at < s
                {
                    return false;
                }
                if let Some(u) = until
                    && r.created_at > u
                {
                    return false;
                }
                true
            })
            .map(|r| UnmatchedSummary {
                number: r.number,
                method: r.request.method,
                path: r.request.path,
                created_at: r.created_at.to_rfc3339(),
                trace_id: r.trace_id,
            })
            .collect();
        Ok(Json(ListRecentUnmatchedResult {
            entries,
            next_before,
        }))
    }

    #[tool(
        name = "find_route",
        description = "Probe what would match a hypothetical request: a method + path pair, like an inbound HTTP request. Returns the matching route if there is one, or a list of near-misses (method-mismatch or literal-prefix typos) explaining what almost matched. Reach for this when debugging \"my mock isn't firing\" — it tells you whether any route exists for the request, and if not, the closest candidates."
    )]
    pub async fn find_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FindRouteArgs>,
    ) -> Result<Json<FindRouteResult>, ErrorData> {
        // Auth: any authenticated user. The match probe is a
        // read-only diagnostic; the route record itself is the only
        // thing returned.
        let _auth = auth_from(&parts)?;

        if args.method.is_empty()
            || !args
                .method
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '-' || c == '_')
        {
            return Err(validation(
                "method must be uppercase ASCII (e.g. POST, GET, ANY)",
            ));
        }
        if !args.path.starts_with('/') {
            return Err(validation("path must start with /"));
        }

        match self.state.routes().probe(&args.method, &args.path) {
            MatchProbe::Hit(m) => Ok(Json(FindRouteResult {
                matched: true,
                route: Some(RouteRecord::from(&m.route)),
                path_params: Some(m.path_params),
                near_misses: Vec::new(),
            })),
            MatchProbe::Miss(near) => Ok(Json(FindRouteResult {
                matched: false,
                route: None,
                path_params: None,
                near_misses: near
                    .into_iter()
                    .map(|nm| {
                        let slug = render_slug(&nm.route.group_name, nm.route.number);
                        let path = nm.route.path.clone();
                        let route_id = nm.route.id.clone();
                        let (reason, details) = match nm.reason {
                            NearMissReason::MethodMismatch {
                                expected_methods,
                                got,
                            } => (
                                FindRouteNearMissReason::MethodMismatch,
                                serde_json::json!({
                                    "expected_methods": expected_methods,
                                    "got": got,
                                }),
                            ),
                            NearMissReason::PrefixMatch {
                                segment_index,
                                expected,
                                got,
                            } => (
                                FindRouteNearMissReason::PrefixMatch,
                                serde_json::json!({
                                    "segment_index": segment_index,
                                    "expected": expected,
                                    "got": got,
                                }),
                            ),
                        };
                        FindRouteNearMiss {
                            route: slug,
                            route_id,
                            route_path: path,
                            reason,
                            details,
                        }
                    })
                    .collect(),
            })),
        }
    }
}

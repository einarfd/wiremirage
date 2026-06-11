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
use crate::journal::{
    CallbackRecord, JournalRecord, ListCursor, UnmatchedCursor, UnmatchedNearMiss, UnmatchedRecord,
};
use crate::journal_filter::{JournalFilter, RouteSlug, StatusFilter};
use crate::mcp::context::auth_from;
use crate::mcp::error::{
    forbidden, map_filter_error, map_journal_error, map_registry_error, not_found, validation,
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
    /// Public base URL this host is reached at (e.g.
    /// `https://wm.example.com`), derived from the request and honoring
    /// `X-Forwarded-*` behind a trusted proxy. Mock routes
    /// answer directly under it (`{base_url}{route.path}`).
    pub base_url: String,
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
    /// The group's mock-traffic base URL: `{scheme}://{name}.{apex}`
    /// (virtual-host routing). Send the SUT here; the apex
    /// `base_url` on the parent result is control-plane (UI/API/MCP) only.
    pub base_url: String,
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
    /// The group (subdomain) the request was addressed to.
    pub group: String,
    pub method: String,
    pub path: String,
    pub created_at: String,
    pub trace_id: Option<String>,
    /// Routes that nearly matched, populated by the dispatcher at
    /// unmatched-write time. Empty when no neighbour was
    /// close enough — `[]`, not omitted, so agents can rely on the
    /// field being present.
    #[serde(default)]
    pub near_misses: Vec<UnmatchedNearMiss>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowUnmatchedArgs {
    /// The unmatched entry's `number` (from `list_recent_unmatched`).
    pub number: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FindRouteArgs {
    /// Group (name or ULID) to probe within. Required: matching is
    /// per-subdomain, so a probe names its tenant.
    pub group: String,
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
    /// REST `/api/match` endpoint for the matching JSON contract.
    pub details: JsonValue,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindRouteNearMissReason {
    MethodMismatch,
    PrefixMatch,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListJournalArgs {
    /// Group name or ULID. Required — the journal is per-group, and the
    /// gate is owner-or-admin of *this* group.
    pub group: String,
    /// Restrict to one route by `{group}/{n}` slug.
    pub route: Option<String>,
    /// HTTP method filter (uppercase, e.g. `POST`).
    pub method: Option<String>,
    /// Exact match against the route's matched_pattern (e.g.
    /// `/v1/charges/{id}`).
    pub path_pattern: Option<String>,
    /// Status filter: `2xx` / `3xx` / `4xx` / `5xx` or a specific
    /// code like `503`.
    pub status: Option<String>,
    /// Lower bound on `created_at`. Duration suffix (`5m`, `1h`, `2d`,
    /// `30s`) or RFC 3339 timestamp.
    pub since: Option<String>,
    /// Upper bound on `created_at`.
    pub until: Option<String>,
    /// Cursor for the next page: return entries with `number < before`.
    /// Omit to start at the newest.
    pub before: Option<u32>,
    /// Max entries to return. Defaults to 50, capped at 200.
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListJournalResult {
    pub entries: Vec<JournalRecord>,
    /// Cursor for the next page; absent when the returned page reached
    /// the oldest entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCallbacksArgs {
    /// Group name or ULID. Required — callbacks are per-group, gated
    /// owner-or-admin of *this* group.
    pub group: String,
    /// Cursor for the next page: return entries with `number < before`.
    /// Omit to start at the newest.
    pub before: Option<u32>,
    /// Max entries to return. Defaults to 50, capped at 200.
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCallbacksResult {
    pub entries: Vec<CallbackRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u32>,
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
        let trust = self.state.trust_forwarded_headers();
        let summaries = groups
            .into_iter()
            .map(|g| {
                let route_count = all_routes.iter().filter(|r| r.group_id == g.id).count() as u32;
                let base_url = crate::auth_api::group_base_url(&g.name, &parts.headers, trust);
                GroupSummary {
                    is_mine: g.owner_id == auth.user_id,
                    owner_id: g.owner_id,
                    id: g.id,
                    name: g.name,
                    base_url,
                    ttl_seconds: g.ttl_seconds,
                    route_count,
                }
            })
            .collect();
        let base_url =
            crate::auth_api::public_base_url(&parts.headers, self.state.trust_forwarded_headers());
        Ok(Json(SummarizeWorkspaceResult {
            host: HostInfo {
                version: HOST_VERSION.into(),
                base_url,
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
        description = "List recent unmatched-request entries — requests that arrived at a group's subdomain but didn't match any route in it. Use this when the SUT seems to be hitting WireMirage but the mock isn't firing. Cursor-paginated (`before` / `limit`). Optional filters: `method`, `path_pattern` glob, `since`/`until` against `created_at`. Owner-or-admin: a tenant sees their own groups' unmatched; admin sees all."
    )]
    pub async fn list_recent_unmatched(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListRecentUnmatchedArgs>,
    ) -> Result<Json<ListRecentUnmatchedResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        // ADR-0030 SemFLIP: admin sees every group's unmatched; a tenant
        // sees only records attributed to a group they own.
        let visible: Option<std::collections::HashSet<String>> = if auth.is_admin {
            None
        } else {
            Some(
                self.state
                    .routes()
                    .registry()
                    .list_groups_by_owner(&auth.user_id)
                    .map_err(map_registry_error)?
                    .into_iter()
                    .map(|g| g.id)
                    .collect(),
            )
        };
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
            .list_unmatched(cursor, visible.as_ref())
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
                group: r.group_name,
                method: r.request.method,
                path: r.request.path,
                created_at: r.created_at.to_rfc3339(),
                trace_id: r.trace_id,
                near_misses: r.near_misses,
            })
            .collect();
        Ok(Json(ListRecentUnmatchedResult {
            entries,
            next_before,
        }))
    }

    #[tool(
        name = "show_unmatched",
        description = "Fetch one unmatched-request entry in full by its `number` — the complete captured request the SUT sent to a path no route matched: method, path, all headers, and the body (UTF-8 string, or { \"base64\": \"...\" } for binary). `list_recent_unmatched` returns only a summary (group + method + path + near-misses); reach for this to see exactly what an SDK posted so you can build the matching mock. The unmatched journal is the discovery surface: a request hits an undefined path, lands here with its full envelope, and you register the real route from what you see. Owner-or-admin of the group the request was addressed to."
    )]
    pub async fn show_unmatched(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ShowUnmatchedArgs>,
    ) -> Result<Json<UnmatchedRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let record = self
            .state
            .journal()
            .get_unmatched(args.number)
            .map_err(map_journal_error)?;
        let owns = auth.is_admin
            || matches!(
                self.state.routes().registry().read_group_by_ref(&record.group_id),
                Ok(g) if g.owner_id == auth.user_id
            );
        if !owns {
            return Err(forbidden(
                "must be an admin or own this group to read its unmatched requests",
            ));
        }
        Ok(Json(record))
    }

    #[tool(
        name = "list_journal",
        description = "List a group's recent handled requests — the matched-traffic counterpart to list_recent_unmatched, and the way to pull a completed request's journal entry (status, timing, the [stream] summary) after the fact rather than waiting live with wait_for_request / tail_journal. Cursor-paginated (`before` / `limit`, newest first). Optional filters: `route` slug, `method`, `path_pattern`, `status`, `since` / `until`. Owner-or-admin of the group."
    )]
    pub async fn list_journal(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListJournalArgs>,
    ) -> Result<Json<ListJournalResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        // Resolve the group ref (name or ULID); 404 so callers can't
        // probe for groups they can't see.
        let group = self
            .state
            .routes()
            .registry()
            .read_group_by_ref(&args.group)
            .map_err(|_| not_found("group not found"))?;
        // Owner-or-admin gate, matching the REST `/api/journal/{group}`
        // endpoint: admin, or owns at least one route in the group.
        if !auth.is_admin {
            let owned = self
                .state
                .routes()
                .registry()
                .list_routes_by_owner(&auth.user_id)
                .map_err(map_registry_error)?;
            if !owned.iter().any(|r| r.group_id == group.id) {
                return Err(forbidden(
                    "must be admin or own a route in this group to read its journal",
                ));
            }
        }

        let now = chrono::Utc::now();
        let route = args
            .route
            .as_deref()
            .map(RouteSlug::parse)
            .transpose()
            .map_err(|e| validation(format!("invalid `route`: {e}")))?;
        let status = args
            .status
            .as_deref()
            .map(StatusFilter::parse)
            .transpose()
            .map_err(|e| validation(format!("invalid `status`: {e}")))?;
        let method = args
            .method
            .as_deref()
            .map(validate_method)
            .transpose()
            .map_err(map_filter_error)?;
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
        // Group filter tracks the resolved group name, not a free param.
        let filter = JournalFilter {
            group: Some(group.name.clone()),
            route,
            method,
            path_pattern: args.path_pattern.clone(),
            status,
            since,
            until,
        };
        let any_filter = filter.route.is_some()
            || filter.method.is_some()
            || filter.path_pattern.is_some()
            || filter.status.is_some()
            || filter.since.is_some()
            || filter.until.is_some();

        let cursor = ListCursor {
            before: args.before,
            limit: args.limit.unwrap_or(50).clamp(1, 200) as usize,
        };
        let entries = self
            .state
            .journal()
            .list_for_group(&group.id, cursor)
            .map_err(map_journal_error)?;
        // Cursor walks the unfiltered stream so the caller can keep
        // paging even when filters reject a whole page (mirrors REST).
        let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
        let entries = if any_filter {
            entries
                .into_iter()
                .filter(|r| filter.matches_handled(r))
                .collect()
        } else {
            entries
        };
        Ok(Json(ListJournalResult {
            entries,
            next_before,
        }))
    }

    #[tool(
        name = "list_callbacks",
        description = "List a group's outbound-callback delivery outcomes, newest first. When a handler calls `host.scheduleCallback`, the host fires the webhook AFTER the response is sent, so the result can't ride the original journal entry — it lands here instead. Each record carries the request the host sent (url / method / headers / body), the requested `delay_ms`, and the `outcome`: `delivered` (with the SUT's `status`), `egress_denied` (blocked by policy, with the resolved IPs), or `failed` (DNS / connect / timeout). Cursor-paginated (`before` / `limit`). Owner-or-admin of the group."
    )]
    pub async fn list_callbacks(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListCallbacksArgs>,
    ) -> Result<Json<ListCallbacksResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = self
            .state
            .routes()
            .registry()
            .read_group_by_ref(&args.group)
            .map_err(|_| not_found("group not found"))?;
        // Owner-or-admin gate, matching the REST callbacks endpoint and the
        // request journal: admin, or owns a route in the group.
        if !auth.is_admin {
            let owned = self
                .state
                .routes()
                .registry()
                .list_routes_by_owner(&auth.user_id)
                .map_err(map_registry_error)?;
            if !owned.iter().any(|r| r.group_id == group.id) {
                return Err(forbidden(
                    "must be admin or own a route in this group to read its callbacks",
                ));
            }
        }
        let cursor = ListCursor {
            before: args.before,
            limit: args.limit.unwrap_or(50).clamp(1, 200) as usize,
        };
        let entries = self
            .state
            .journal()
            .list_callbacks_for_group(&group.id, cursor)
            .map_err(map_journal_error)?;
        let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
        Ok(Json(ListCallbacksResult {
            entries,
            next_before,
        }))
    }

    #[tool(
        name = "find_route",
        description = "Probe what would match a hypothetical request **within a group**: a method + path pair, like an inbound HTTP request to that group's subdomain. Returns the matching route if there is one, or a list of near-misses explaining what almost matched. Near-miss detection is intentionally shallow: it catches a method mismatch (path pattern matches but methods don't) and a single-segment literal-prefix typo — it does NOT catch deeper edits like a transposed or misspelled segment, so an EMPTY near-miss list does not prove no similar route exists. Reach for this when debugging \"my mock isn't firing\" — it tells you whether any route exists in the group for the request, and if not, the closest candidates. Owner-or-admin of the group (matching is per-subdomain)."
    )]
    pub async fn find_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FindRouteArgs>,
    ) -> Result<Json<FindRouteResult>, ErrorData> {
        // Probing reveals a group's routes, so it's owner-or-admin of the
        // group being probed (ADR-0030) — not any authenticated user.
        let auth = auth_from(&parts)?;
        let group =
            crate::mcp::context::ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;

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

        match self
            .state
            .routes()
            .probe_in_group(&group.name, &args.method, &args.path)
        {
            MatchProbe::Hit(m) => Ok(Json(FindRouteResult {
                matched: true,
                route: Some(RouteRecord::build(
                    &m.route,
                    &parts.headers,
                    self.state.trust_forwarded_headers(),
                )),
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

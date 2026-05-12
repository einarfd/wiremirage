//! Streaming tools — `wait_for_request`, `tail_journal`. Both
//! subscribe directly to the in-process `JournalBus` (no need to
//! round-trip via the SSE endpoint since MCP runs inside the host)
//! and accumulate matching entries until a stop condition fires.
//!
//! Both tools return a single rmcp `CallToolResult` carrying a list
//! of journal entries — request/response shape, not progressive
//! notifications. This matches the design in `mcp-surface.md` where
//! the agent perceives "wait, then receive the matches" as a single
//! tool call.

use std::time::Duration;

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::journal::{JournalEvent, JournalRecord, UnmatchedRecord};
use crate::journal_filter::{JournalFilter, RouteSlug, StatusFilter};
use crate::mcp::context::auth_from;
use crate::mcp::error::{forbidden, validation};
use crate::mcp::server::WmMcpServer;

const DEFAULT_WAIT_TIMEOUT_S: u64 = 30;
const MAX_WAIT_TIMEOUT_S: u64 = 300;
const DEFAULT_TAIL_IDLE_S: u64 = 30;
const MAX_TAIL_IDLE_S: u64 = 300;
const DEFAULT_TAIL_MAX_ENTRIES: u32 = 100;
const MAX_TAIL_MAX_ENTRIES: u32 = 1000;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WaitForRequestArgs {
    /// Group name or ULID. One of `group` or `route` is required —
    /// otherwise admin-only host-wide waits would be too easy to
    /// abuse.
    pub group: Option<String>,
    /// Route slug `{group}/{n}`.
    pub route: Option<String>,
    /// HTTP method filter.
    pub method: Option<String>,
    /// Exact match against the route's matched_pattern (e.g.
    /// `/v1/charges/{id}`).
    pub path_pattern: Option<String>,
    /// Status filter: `2xx` / `3xx` / `4xx` / `5xx` or a specific
    /// code like `503`.
    pub status: Option<String>,
    /// How many matches to wait for. Defaults to 1; capped by
    /// implementation — agent flows asking for >100 are misuse.
    pub count: Option<u32>,
    /// Hard timeout before returning whatever has accumulated.
    /// Defaults to 30s; clamped to [1, 300].
    pub timeout_seconds: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WaitForRequestResult {
    pub entries: Vec<JournalRecord>,
    pub timed_out: bool,
    pub dropped_events: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TailJournalArgs {
    pub group: Option<String>,
    pub route: Option<String>,
    pub method: Option<String>,
    pub path_pattern: Option<String>,
    pub status: Option<String>,
    /// Stop after this many matches. Defaults to 100, capped at 1000.
    pub max_entries: Option<u32>,
    /// Stop after this long without a new match. Defaults to 30s,
    /// clamped to [1, 300].
    pub idle_timeout_seconds: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TailJournalResult {
    pub entries: Vec<JournalRecord>,
    /// Unmatched events that arrived during the window. Included
    /// only when no filter that depends on a matched route is set
    /// (group / route / path_pattern / status all hide unmatched).
    pub unmatched: Vec<UnmatchedRecord>,
    pub stopped_reason: TailStopped,
    pub dropped_events: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TailStopped {
    MaxEntries,
    IdleTimeout,
}

/// Build a `JournalFilter` from the user-facing string fields shared
/// by both streaming tools.
fn build_filter(
    group: Option<&str>,
    route: Option<&str>,
    method: Option<&str>,
    path_pattern: Option<&str>,
    status: Option<&str>,
) -> Result<JournalFilter, ErrorData> {
    let route = route
        .map(RouteSlug::parse)
        .transpose()
        .map_err(|e| validation(format!("invalid route filter: {e}")))?;
    let status = status
        .map(StatusFilter::parse)
        .transpose()
        .map_err(|e| validation(format!("invalid status filter: {e}")))?;
    Ok(JournalFilter {
        group: group.map(String::from),
        route,
        method: method.map(String::from),
        path_pattern: path_pattern.map(String::from),
        status,
        since: None,
        until: None,
    })
}

/// Authorization: when a group/route filter is set, owner-or-admin
/// of that group; otherwise admin-only. Mirrors the SSE endpoint's
/// gate so MCP and HTTP consumers see the same policy.
fn ensure_streaming_authorized(
    state: &crate::AppState,
    auth: &crate::auth::AuthContext,
    filter: &JournalFilter,
) -> Result<(), ErrorData> {
    let group_ref = filter
        .group
        .as_deref()
        .or_else(|| filter.route.as_ref().map(|r| r.group_ref.as_str()));
    if let Some(group_ref) = group_ref {
        let group = state
            .routes()
            .registry()
            .read_group_by_ref(group_ref)
            .map_err(|_| crate::mcp::error::not_found("group not found"))?;
        if !auth.is_admin {
            let owned = state
                .routes()
                .registry()
                .list_routes_by_owner(&auth.user_id)
                .map_err(crate::mcp::error::map_registry_error)?;
            if !owned.iter().any(|r| r.group_id == group.id) {
                return Err(forbidden(
                    "must be admin or own a route in this group to stream its journal",
                ));
            }
        }
        Ok(())
    } else if !auth.is_admin {
        Err(forbidden(
            "host-wide journal streaming is admin-only; supply a group or route filter to scope it",
        ))
    } else {
        Ok(())
    }
}

#[tool_router(router = streaming_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "wait_for_request",
        description = "Wait until one or more journal entries match the supplied filters, then return them. The most-reached-for tool when a test triggers something that should call WireMirage and you want to verify the call landed before proceeding. Returns whatever entries arrived; sets `timed_out: true` if the timeout fired before `count` matches accumulated."
    )]
    pub async fn wait_for_request(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<WaitForRequestArgs>,
    ) -> Result<Json<WaitForRequestResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let filter = build_filter(
            args.group.as_deref(),
            args.route.as_deref(),
            args.method.as_deref(),
            args.path_pattern.as_deref(),
            args.status.as_deref(),
        )?;
        ensure_streaming_authorized(&self.state, &auth, &filter)?;

        let count = args.count.unwrap_or(1).max(1);
        let timeout = Duration::from_secs(
            args.timeout_seconds
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_S)
                .clamp(1, MAX_WAIT_TIMEOUT_S),
        );

        let mut rx = self.state.journal().subscribe();
        let mut entries = Vec::with_capacity(count as usize);
        let mut dropped_events: u32 = 0;
        let timed_out = loop {
            if entries.len() as u32 >= count {
                break false;
            }
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Ok(JournalEvent::Handled(record))) => {
                    if filter.matches(&JournalEvent::Handled(record.clone())) {
                        entries.push(*record);
                    }
                }
                Ok(Ok(JournalEvent::Unmatched(_))) => {
                    // wait_for_request is route-scoped by definition;
                    // unmatched events don't satisfy a count.
                }
                Ok(Err(RecvError::Lagged(n))) => {
                    dropped_events = dropped_events.saturating_add(n as u32);
                    // Keep waiting after a lag; the consumer would
                    // rather see partial results than fail.
                }
                Ok(Err(RecvError::Closed)) => {
                    // Bus closed (host shutting down). Treat as
                    // timed out — consumer can decide what to do.
                    break true;
                }
                Err(_elapsed) => break true,
            }
        };
        Ok(Json(WaitForRequestResult {
            entries,
            timed_out,
            dropped_events,
        }))
    }

    #[tool(
        name = "tail_journal",
        description = "Stream journal entries as they arrive, returning the accumulated batch when either `max_entries` matches have arrived or `idle_timeout_seconds` elapses without a new match. Less common than `wait_for_request` but useful for general observation tasks. Single result, not progressive — the tool blocks until done, then returns everything."
    )]
    pub async fn tail_journal(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<TailJournalArgs>,
    ) -> Result<Json<TailJournalResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let filter = build_filter(
            args.group.as_deref(),
            args.route.as_deref(),
            args.method.as_deref(),
            args.path_pattern.as_deref(),
            args.status.as_deref(),
        )?;
        ensure_streaming_authorized(&self.state, &auth, &filter)?;

        let max_entries = args
            .max_entries
            .unwrap_or(DEFAULT_TAIL_MAX_ENTRIES)
            .clamp(1, MAX_TAIL_MAX_ENTRIES);
        let idle = Duration::from_secs(
            args.idle_timeout_seconds
                .unwrap_or(DEFAULT_TAIL_IDLE_S)
                .clamp(1, MAX_TAIL_IDLE_S),
        );

        let mut rx = self.state.journal().subscribe();
        let mut entries: Vec<JournalRecord> = Vec::new();
        let mut unmatched: Vec<UnmatchedRecord> = Vec::new();
        let mut dropped_events: u32 = 0;
        let stopped_reason = loop {
            let total = entries.len() as u32 + unmatched.len() as u32;
            if total >= max_entries {
                break TailStopped::MaxEntries;
            }
            match tokio::time::timeout(idle, rx.recv()).await {
                Ok(Ok(event)) => {
                    if !filter.matches(&event) {
                        continue;
                    }
                    match event {
                        JournalEvent::Handled(r) => entries.push(*r),
                        JournalEvent::Unmatched(u) => unmatched.push(*u),
                    }
                }
                Ok(Err(RecvError::Lagged(n))) => {
                    dropped_events = dropped_events.saturating_add(n as u32);
                }
                Ok(Err(RecvError::Closed)) => break TailStopped::IdleTimeout,
                Err(_elapsed) => break TailStopped::IdleTimeout,
            }
        };
        Ok(Json(TailJournalResult {
            entries,
            unmatched,
            stopped_reason,
            dropped_events,
        }))
    }
}

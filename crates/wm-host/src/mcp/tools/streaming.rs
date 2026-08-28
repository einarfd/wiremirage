//! Streaming tools — `wait_for_request`, `tail_journal`. Both
//! subscribe directly to the in-process `JournalBus` (no need to
//! round-trip via the SSE endpoint since MCP runs inside the host)
//! and accumulate matching entries until a stop condition fires.
//!
//! Both tools return a single rmcp `CallToolResult` carrying a list
//! of journal entries — request/response shape, not progressive
//! notifications. This matches the design in `mcp-surface.md` where
//! the agent perceives "wait, then receive the matches" as a single
//! tool call. The entries still arrive in one piece at the end; the
//! only thing that travels early is the [`Heartbeat`] below, which
//! carries no journal data.

use std::time::{Duration, Instant};

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::RoleServer;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::{Peer, RequestContext};
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
/// How often a blocked tool emits a progress heartbeat. Comfortably
/// inside the idle windows MCP clients apply to a silent HTTP call
/// (five minutes for Claude Code) without being chatty.
const HEARTBEAT_INTERVAL_S: u64 = 15;

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

/// Emits `notifications/progress` while one of the two streaming tools
/// is blocked, so the wait is visibly alive rather than indistinguishable
/// from a hung server.
///
/// Both tools can block for up to 300s (`MAX_WAIT_TIMEOUT_S` /
/// `MAX_TAIL_IDLE_S`) while sending nothing at all. Anything between the
/// client and the host that applies a read timeout — the MCP client's own
/// first-byte and idle timers, a reverse proxy — cannot tell that from a
/// server that has died, and cuts the call well short of the timeout the
/// caller actually asked for. The first beat is sent before blocking
/// precisely because it becomes the response's first byte.
///
/// This does not weaken the stateless transport of ADR-0037. These are
/// *request-scoped* notifications: they travel on the response stream of
/// the very request being served, not on a server-initiated channel
/// between requests, so nothing needs a session and any replica can serve
/// the call. rmcp switches that one response to `text/event-stream`;
/// every other tool keeps the `json_response` fast path.
///
/// Silent unless the caller supplied a `progressToken` — the spec allows
/// progress only for a request that asked for it, and without one the
/// response stays plain JSON exactly as before.
struct Heartbeat {
    /// `None` when the caller sent no progress token.
    target: Option<(Peer<RoleServer>, ProgressToken)>,
    /// Monotonically increasing, as the `progress` field requires.
    beats: f64,
    started: Instant,
}

impl Heartbeat {
    fn new(ctx: &RequestContext<RoleServer>) -> Self {
        Self {
            target: ctx
                .meta
                .get_progress_token()
                .map(|token| (ctx.peer.clone(), token)),
            beats: 0.0,
            started: Instant::now(),
        }
    }

    /// Whether the caller asked for progress. Gates the heartbeat arm of
    /// the select loops, so a caller that did not ask pays nothing —
    /// not even a timer.
    fn wanted(&self) -> bool {
        self.target.is_some()
    }

    fn elapsed_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Best-effort. A send failure means the client is gone, which the
    /// tool's own timeout already handles; failing the call because a
    /// status update could not be delivered would be worse than the
    /// silence this exists to prevent.
    async fn beat(&mut self, message: String) {
        let Some((peer, token)) = self.target.clone() else {
            return;
        };
        self.beats += 1.0;
        let mut param = ProgressNotificationParam::new(token, self.beats);
        param.message = Some(message);
        let _ = peer.notify_progress(param).await;
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
        ctx: RequestContext<RoleServer>,
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

        // Armed before the first beat so no event dispatched while we
        // announce ourselves can slip past the subscription.
        let mut heartbeat = Heartbeat::new(&ctx);
        let beating = heartbeat.wanted();
        heartbeat
            .beat(format!("waiting for {count} matching request(s)"))
            .await;

        let timed_out = loop {
            if entries.len() as u32 >= count {
                break false;
            }
            let recv = tokio::time::timeout(timeout, rx.recv());
            tokio::pin!(recv);
            // Only the heartbeat sleep restarts on a beat; `recv` is
            // pinned across the inner loop so the caller's timeout keeps
            // running underneath it rather than being reset by our own
            // status updates.
            let received = loop {
                tokio::select! {
                    r = &mut recv => break r,
                    _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_S)), if beating => {
                        let seen = entries.len();
                        let elapsed = heartbeat.elapsed_s();
                        heartbeat
                            .beat(format!(
                                "waiting for {count} matching request(s); {seen} so far, {elapsed}s elapsed"
                            ))
                            .await;
                    }
                }
            };
            match received {
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
        ctx: RequestContext<RoleServer>,
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

        let mut heartbeat = Heartbeat::new(&ctx);
        let beating = heartbeat.wanted();
        heartbeat
            .beat(format!("tailing, up to {max_entries} entries"))
            .await;

        let stopped_reason = loop {
            let total = entries.len() as u32 + unmatched.len() as u32;
            if total >= max_entries {
                break TailStopped::MaxEntries;
            }
            let recv = tokio::time::timeout(idle, rx.recv());
            tokio::pin!(recv);
            // As in `wait_for_request`: beating must not extend the
            // caller's idle window, so the receive future outlives the
            // heartbeat sleeps rather than being rebuilt around them.
            let received = loop {
                tokio::select! {
                    r = &mut recv => break r,
                    _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_S)), if beating => {
                        let elapsed = heartbeat.elapsed_s();
                        heartbeat
                            .beat(format!(
                                "tailing; {total} entries so far, {elapsed}s elapsed"
                            ))
                            .await;
                    }
                }
            };
            match received {
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

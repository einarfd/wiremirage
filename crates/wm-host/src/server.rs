use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use http::header;
use opentelemetry::global;
use serde_json::json;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::Runtime;
use crate::bindings::wiremirage::handler::http::{
    Header, Request as WitRequest, Response as WitResponse,
};
use crate::host_state::StreamHead;
use crate::journal::{
    HANDLED_BODY_LIMIT, HandlerLogEntry, NewJournalEntry, NewUnmatchedEntry, RequestEnvelope,
    ResourceUsage, ResponseEnvelope, UNMATCHED_BODY_LIMIT, UnmatchedNearMiss,
    UnmatchedNearMissReason, truncate_body,
};
use crate::log::LogRecord;
use crate::route_table::{MatchedRoute, NearMiss, NearMissReason, RouteTable};
use crate::telemetry::HeaderExtractor;
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

/// Bounded chunk-channel depth for streaming responses (ADR-0022). A
/// small buffer smooths bursts; past it `blocking_send` parks the
/// handler thread, backpressuring a slow client.
const STREAM_CHANNEL_CAP: usize = 16;

/// Result of running a handler on the blocking thread: the call result
/// (or trap), captured logs, and resource usage. The wall-clock field
/// is filled in by the async caller.
type Outcome = (
    Result<WitResponse, wasmtime::Error>,
    Vec<LogRecord>,
    ResourceUsage,
);

/// Path prefixes the host owns; user routes can never claim them. Requests
/// that don't match any actual host endpoint under these prefixes return
/// 404 directly rather than falling through to the route table — that way
/// a typo in `/__api/...` doesn't accidentally execute a user handler.
const RESERVED_PREFIXES: &[&str] = &["/__api/", "/__ui/", "/__auth/", "/__admin/"];
const RESERVED_EXACT: &[&str] = &["/__health", "/__ready", "/__api", "/__ui", "/__auth"];

#[derive(Clone)]
pub struct AppState {
    runtime: Arc<Runtime>,
    routes: Arc<RouteTable>,
    auth: crate::auth::Auth,
    journal: crate::journal::Journal,
    /// Local-auth credential map (slice 20). Empty when
    /// `WM_LOCAL_AUTH` isn't configured; login attempts just always
    /// fail in that case.
    local_auth: Arc<crate::local_auth::LocalAuth>,
    /// Session store (slice 20). `None` when `SESSION_SECRET` isn't
    /// configured — login endpoints respond 503 in that case so the
    /// operator gets a clear signal rather than silent "wrong
    /// password" rejections.
    sessions: Option<crate::session::SessionStore>,
    /// In-process per-IP throttle for the password login endpoint.
    /// Lives behind an `Arc` so cloning `AppState` shares the counters.
    login_throttle: Arc<crate::login_throttle::LoginThrottle>,
    /// minijinja environment + helpers for the web UI (slice 21).
    /// Built once at startup; cheap to clone (inner Arc).
    ui_templates: crate::ui::UiTemplates,
    /// Shutdown signal cloned into long-lived handlers (the SSE tail
    /// is the only one today). When the main loop receives Ctrl-C /
    /// SIGTERM it flips the watch to `true`; handlers race against
    /// `changed()` to end the response cleanly so graceful-shutdown
    /// isn't held open by an idle EventSource on a browser tab.
    /// `None` in tests that don't bother wiring it up.
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    /// When true, append `Secure` to session + CSRF cookies. Set via
    /// `WM_SECURE_COOKIES=1` in deployments behind an HTTPS edge
    /// (Caddy, an ALB, etc.). Default off so dev workflows over
    /// plain HTTP keep working — browsers drop `Secure` cookies on
    /// non-TLS connections, which would break login.
    secure_cookies: bool,
    /// When true, honor `X-Forwarded-For` for the login throttle's
    /// per-IP key. Default off — the placeholder IP is used in that
    /// case, which collapses everyone into one throttle bucket but
    /// makes IP spoofing impossible. Set via
    /// `WM_TRUST_FORWARDED_HEADERS=1` only when the host is fronted
    /// by a reverse proxy that populates the header reliably.
    trust_forwarded_headers: bool,
    /// GitHub OAuth config, populated when both `WM_GITHUB_CLIENT_ID`
    /// and `WM_GITHUB_CLIENT_SECRET` are set. `None` means the login
    /// page hides the "Continue with GitHub" button and the
    /// `/__auth/start/github` + `/__auth/callback` routes respond
    /// 503. Kept behind `Arc` so cloning `AppState` shares it.
    github_oauth: Option<Arc<crate::github_oauth::GitHubConfig>>,
}

impl AppState {
    pub fn new(
        runtime: Arc<Runtime>,
        routes: Arc<RouteTable>,
        auth: crate::auth::Auth,
        journal: crate::journal::Journal,
    ) -> Self {
        Self {
            runtime,
            routes,
            auth,
            journal,
            local_auth: Arc::new(crate::local_auth::LocalAuth::empty()),
            sessions: None,
            login_throttle: Arc::new(crate::login_throttle::LoginThrottle::new()),
            ui_templates: crate::ui::UiTemplates::new(),
            shutdown: None,
            secure_cookies: false,
            trust_forwarded_headers: false,
            github_oauth: None,
        }
    }

    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    pub fn with_trust_forwarded_headers(mut self, trust: bool) -> Self {
        self.trust_forwarded_headers = trust;
        self
    }

    pub fn secure_cookies(&self) -> bool {
        self.secure_cookies
    }

    pub fn trust_forwarded_headers(&self) -> bool {
        self.trust_forwarded_headers
    }

    pub fn with_shutdown(mut self, rx: tokio::sync::watch::Receiver<bool>) -> Self {
        self.shutdown = Some(rx);
        self
    }

    /// Borrow the shutdown receiver if one was attached. SSE / other
    /// long-lived handlers use `clone()` on the result so they get an
    /// independent waiter that doesn't race against any other handler.
    pub fn shutdown(&self) -> Option<&tokio::sync::watch::Receiver<bool>> {
        self.shutdown.as_ref()
    }

    pub fn with_local_auth(mut self, local_auth: crate::local_auth::LocalAuth) -> Self {
        self.local_auth = Arc::new(local_auth);
        self
    }

    pub fn with_sessions(mut self, sessions: crate::session::SessionStore) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub fn with_github_oauth(mut self, config: crate::github_oauth::GitHubConfig) -> Self {
        self.github_oauth = Some(Arc::new(config));
        self
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub fn routes(&self) -> &Arc<RouteTable> {
        &self.routes
    }

    pub fn auth(&self) -> &crate::auth::Auth {
        &self.auth
    }

    pub fn journal(&self) -> &crate::journal::Journal {
        &self.journal
    }

    pub fn local_auth(&self) -> &crate::local_auth::LocalAuth {
        &self.local_auth
    }

    pub fn sessions(&self) -> Option<&crate::session::SessionStore> {
        self.sessions.as_ref()
    }

    pub fn github_oauth(&self) -> Option<&crate::github_oauth::GitHubConfig> {
        self.github_oauth.as_deref()
    }

    pub fn login_throttle(&self) -> &crate::login_throttle::LoginThrottle {
        &self.login_throttle
    }

    pub fn ui_templates(&self) -> &crate::ui::UiTemplates {
        &self.ui_templates
    }
}

/// Build the axum router. The REST API mounts at `/__api/*`; mock-traffic
/// dispatch is the fallback. The fallback rejects requests under reserved
/// prefixes (e.g., `/__api/typo`) with 404 before consulting user routes.
/// MCP (`/__api/mcp`) merges in as a separate sub-router with its own
/// auth layer.
pub fn router(state: AppState) -> Router {
    let mcp = crate::mcp::router(state.clone());
    let ui = crate::ui::router(state.clone());
    crate::api::router()
        .merge(crate::auth_api::router(state.clone()))
        .merge(crate::mcp_oauth::router(state.clone()))
        .route("/__health", get(health))
        .route("/__ready", get(ready))
        .fallback(any(dispatch))
        .with_state(state)
        .merge(mcp)
        .merge(ui)
}

const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Liveness probe. Public, unauthenticated. Always 200 as long as the
/// process can answer at all — orchestrators use this to decide whether to
/// restart the container.
async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": HOST_VERSION,
    }))
}

/// Readiness probe. Public, unauthenticated. Reports `valkey` status
/// for the configured backend (in-memory is trivially "ok"). Returns
/// 503 if any dependency is configured but unreachable.
async fn ready(State(state): State<AppState>) -> Response {
    let valkey = match state.runtime().storage().ping() {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("unreachable: {e}"),
    };
    let healthy = valkey == "ok";
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({
        "status": if healthy { "ready" } else { "not_ready" },
        "valkey": valkey,
        "version": HOST_VERSION,
    });
    (status, Json(body)).into_response()
}

pub fn is_reserved_path(path: &str) -> bool {
    RESERVED_EXACT.contains(&path) || RESERVED_PREFIXES.iter().any(|&p| path.starts_with(p))
}

/// True iff the request carries a `wm_session` cookie that resolves to
/// a live session. Used by the bare-`/` redirect to pick between
/// `/__ui/` (signed in) and `/__auth/login` (not). Reuses the same
/// cookie name and verification as `ui::auth_redirect::require_session`
/// — slightly duplicated cookie parsing, but the alternative (extract
/// a shared helper) is a tiny refactor for a tiny win.
fn has_valid_session(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(sessions) = state.sessions() else {
        return false;
    };
    let Some(cookie) = pick_session_cookie(headers) else {
        return false;
    };
    sessions.touch(&cookie).is_ok()
}

fn pick_session_cookie(headers: &HeaderMap) -> Option<String> {
    let name = crate::session::COOKIE_NAME;
    for value in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = value.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    match dispatch_inner(state, req).await {
        Ok(resp) => resp,
        Err(e) => {
            // dispatch_inner handles handler-trap errors itself (so they
            // can be journaled). Reaching this arm means an
            // infrastructural failure — request body read, spawn_blocking
            // join, etc. — that there's nothing useful to journal about.
            tracing::error!(error = %e, "dispatch infrastructure failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))
        }
    }
}

// Span fields kept low-cardinality on purpose: `http.method` and
// `route.matched_pattern` are bounded; the raw URL path with path-param
// values is deliberately omitted so OTel attribute cardinality stays
// finite. `route.id` is recorded after a match so a span can be located
// by route ULID, but it's only on matched-route spans.
#[tracing::instrument(
    name = "dispatch",
    skip_all,
    fields(
        http.method = %req.method(),
        route.matched_pattern = tracing::field::Empty,
        route.id = tracing::field::Empty,
        outcome = tracing::field::Empty,
    ),
)]
async fn dispatch_inner(state: AppState, req: Request) -> anyhow::Result<Response> {
    let span = tracing::Span::current();
    let started = std::time::Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let header_map = req.headers().clone();

    // Adopt the upstream W3C trace context, if present, as our span's
    // parent. No-op when nothing is configured (the propagator returns
    // an empty Context) or when the headers carry no traceparent. We
    // keep `trace_id` separately so we can stamp it on the journal
    // record AND set `X-Trace-Id` on the response — both work even when
    // no OTel exporter is active.
    let parent_cx =
        global::get_text_map_propagator(|prop| prop.extract(&HeaderExtractor(&header_map)));
    let trace_id = extract_trace_id(&parent_cx);
    let _ = span.set_parent(parent_cx);

    let body_bytes = match read_body(req.into_body(), MAX_DISPATCH_BODY_BYTES).await {
        BodyReadOutcome::Ok(b) => b,
        BodyReadOutcome::TooLarge => {
            // Refuse before touching the handler. Mock dispatch is
            // unauthenticated, so an unbounded body would be a trivial
            // OOM vector. We deliberately don't journal here — a
            // junk-flood shouldn't pollute the unmatched/handled
            // journals — but the trace ID lands on the response so a
            // legitimate consumer with a too-big body can correlate
            // their failure in OTel/stderr logs.
            span.record("outcome", "body_too_large");
            let mut resp = error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("request body exceeds {MAX_DISPATCH_BODY_BYTES} byte limit"),
            );
            inject_response_trace_id(&trace_id, resp.headers_mut());
            return Ok(resp);
        }
    };

    let path = uri.path();

    // Reserved paths never reach user routes. Any sub-router (slice 3+
    // mounts /__api/* here) takes precedence; if nothing else matched, a
    // request under a reserved prefix is a typo, not mock traffic — and
    // intentionally NOT journaled (typos shouldn't pollute the
    // unmatched log; if operators want them, they're in stderr/OTel).
    //
    // For `/__ui/*` typos specifically, render a branded HTML 404
    // page so a human pointing a browser at a wrong URL lands on the
    // app shell instead of a JSON error blob. Everything else under a
    // reserved prefix (`/__api/typo`, `/__auth/typo`) stays JSON —
    // those surfaces are consumed by scripts and agents that want a
    // parseable error.
    if is_reserved_path(path) {
        span.record("outcome", "reserved_path_404");
        let mut resp = if path.starts_with("/__ui/") {
            crate::ui::render_not_found(&state, path)
        } else {
            not_found_response("reserved path")
        };
        inject_response_trace_id(&trace_id, resp.headers_mut());
        return Ok(resp);
    }

    let matched = match state.routes.find_match(method.as_str(), path) {
        Some(m) => m,
        None => {
            // Per route-model.md, bare `GET /` with no user route
            // claiming it bounces to the UI (if the caller has a
            // session) or the login page (if not). Skipped if a user
            // route explicitly registered `GET /` — `find_match`
            // would have returned `Some` and shadowed this branch.
            //
            // The redirect deliberately doesn't write to the
            // unmatched journal: a human pointing a browser at the
            // bare hostname isn't a "missing mock" signal.
            if method == http::Method::GET && path == "/" {
                let target = if has_valid_session(&state, &header_map) {
                    "/__ui/"
                } else {
                    "/__auth/login"
                };
                span.record("outcome", "root_redirect");
                let mut resp = axum::response::Redirect::to(target).into_response();
                inject_response_trace_id(&trace_id, resp.headers_mut());
                return Ok(resp);
            }

            span.record("outcome", "unmatched_404");
            // Compute nearby routes the SUT might have meant. Slice 35
            // — the heavy lifting is already in `compute_near_misses`,
            // which is the same probe slice 13's `find_route` runs.
            let near_misses: Vec<UnmatchedNearMiss> = state
                .routes()
                .compute_near_misses(method.as_str(), uri.path())
                .into_iter()
                .map(project_near_miss)
                .collect();
            // Journal the unmatched request. Best-effort: a journal
            // failure is logged but doesn't change what the SUT sees.
            let envelope = build_request_envelope(
                &method,
                &uri,
                &header_map,
                body_bytes,
                UNMATCHED_BODY_LIMIT,
            );
            if let Err(e) = state.journal().record_unmatched(NewUnmatchedEntry {
                trace_id: trace_id.clone(),
                request: envelope,
                near_misses,
            }) {
                tracing::warn!(error = %e, "failed to record unmatched journal entry");
            }
            let mut resp = not_found_response("no route matched");
            inject_response_trace_id(&trace_id, resp.headers_mut());
            return Ok(resp);
        }
    };

    span.record("route.matched_pattern", &matched.matched_pattern);
    span.record("route.id", &matched.route.id);

    // Capture the request envelope before `body_bytes` is moved into
    // the wit_request — we need it for the journal regardless of how
    // the handler call turns out.
    let request_envelope = build_request_envelope(
        &method,
        &uri,
        &header_map,
        body_bytes.clone(),
        HANDLED_BODY_LIMIT,
    );

    let wit_request = build_wit_request(
        &method,
        &uri,
        &header_map,
        body_bytes,
        &matched.matched_pattern,
        &matched.path_params,
    );

    let runtime = state.runtime.clone();
    let group_id = matched.route.group_id.clone();
    let route_id = matched.route.id.clone();
    let route_language = matched.route.language.clone();
    let route_source = matched.route.source.clone();

    // Branch on language: source-language routes (ADR-0020) run
    // through the shared engine; everything else (`wasm` uploads,
    // future AOT languages) still goes through the per-route
    // component cache. TS routes were transpiled to JS at create-
    // time, so they're indistinguishable from JS routes at
    // dispatch — both stored fields hold script-shape JS.
    let use_engine = matches!(route_language.as_str(), "javascript" | "typescript");
    let component = if use_engine {
        None
    } else {
        Some(state.routes.component_for(&matched.route)?)
    };

    // spawn_blocking moves out of the async task's tracing context, so
    // capture the current span and re-enter it inside the closure to
    // keep the wasmtime spans children of the dispatch span. The
    // closure returns BOTH the call result and any captured handler
    // logs — we want the logs in the journal even when the handler
    // traps, so propagating an Err the usual way would lose them.
    let parent_span = tracing::Span::current();
    // Outcome carries the call result, the captured logs, and the
    // resource usage. We compute fuel + memory peak inside the
    // blocking task because the wasmtime `Store` doesn't cross the
    // await boundary cleanly; the wall-clock number is filled in by
    // the async caller from its own `Instant::now()` reference.
    let fuel_budget = runtime.handler_fuel();
    // ADR-0022 streaming: the head oneshot fires if the handler calls
    // `response-stream.start`; the bounded chunk channel carries body
    // bytes from the (spawn_blocking) handler thread to the response
    // body stream, with blocking_send providing backpressure.
    let (head_tx, mut head_rx) = tokio::sync::oneshot::channel::<StreamHead>();
    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(STREAM_CHANNEL_CAP);
    let mut join = tokio::task::spawn_blocking(move || -> Outcome {
        let _enter = parent_span.enter();

        if use_engine {
            let source = match route_source {
                Some(s) => s,
                None => {
                    return (
                        Err(wasmtime::Error::msg(format!(
                            "source-language route {route_id} has no source stored"
                        ))),
                        Vec::new(),
                        ResourceUsage::default(),
                    );
                }
            };
            let instantiate_span =
                tracing::info_span!("wasmtime.engine_instantiate", route.id = %route_id).entered();
            let (engine_world, mut store, handles) =
                match runtime.instantiate_engine(&group_id, &route_id, source) {
                    Ok(t) => t,
                    Err(e) => return (Err(e), Vec::new(), ResourceUsage::default()),
                };
            drop(instantiate_span);
            let _call =
                tracing::info_span!("wasmtime.engine_call_handle", route.id = %route_id).entered();
            // ADR-0022: wire the streaming sink so a handler calling
            // `host.responseStream` (response-stream.start) hands the
            // head + chunks back to the dispatch task while it runs.
            store.data_mut().set_response_stream_sink(head_tx, chunk_tx);
            let engine_req = crate::bindings::handler_request_to_engine(wit_request);
            let result = engine_world
                .call_handle(&mut store, &engine_req, handles.route, handles.group)
                .map(crate::bindings::engine_response_to_handler);
            let logs = store.data_mut().take_logs();
            // Fuel is effectively unbounded on the engine path; we
            // still report consumed for the journal because the
            // wasmtime engine still tracks it. memory_peak_bytes is
            // the more interesting number for these routes.
            let fuel_remaining = store.get_fuel().unwrap_or(0);
            let resources = ResourceUsage {
                fuel_consumed: u64::MAX.saturating_sub(fuel_remaining),
                memory_peak_bytes: store.data().limits.peak_memory_bytes as u64,
                wall_clock_ms: 0,
            };
            (result, logs, resources)
        } else {
            let component = component.expect("non-engine path requires a component");
            let instantiate_span =
                tracing::info_span!("wasmtime.instantiate", route.id = %route_id).entered();
            let (handler, mut store, handles) =
                match runtime.instantiate(&component, &group_id, &route_id) {
                    Ok(t) => t,
                    Err(e) => return (Err(e), Vec::new(), ResourceUsage::default()),
                };
            drop(instantiate_span);
            let _call = tracing::info_span!("wasmtime.call_handle", route.id = %route_id).entered();
            let result =
                handler.call_handle(&mut store, &wit_request, handles.route, handles.group);
            let logs = store.data_mut().take_logs();
            // Capture resource usage before the store drops. `get_fuel`
            // returns the *remaining* fuel; we subtract from the budget
            // to get consumed. `peak_memory_bytes` is updated by the
            // `HandlerLimits` impl every time `memory_growing` is
            // approved.
            let fuel_remaining = store.get_fuel().unwrap_or(fuel_budget);
            let resources = ResourceUsage {
                fuel_consumed: fuel_budget.saturating_sub(fuel_remaining),
                memory_peak_bytes: store.data().limits.peak_memory_bytes as u64,
                wall_clock_ms: 0,
            };
            (result, logs, resources)
        }
    });

    // Wait for whichever happens first: the handler commits a streaming
    // head (`response-stream.start`), or it finishes/traps without
    // streaming. `biased` checks the head first so a handler that
    // streams then returns immediately still takes the streaming path.
    let outcome: Outcome = tokio::select! {
        biased;
        head = &mut head_rx => {
            match head {
                Ok(head) => {
                    // Streaming: return the response head now and pump
                    // chunks from `chunk_rx` to the wire while the
                    // handler keeps running on its blocking thread. The
                    // journal entry is written when the handler finishes
                    // (see `spawn_streaming_journal`).
                    span.record("outcome", "streaming");
                    let status = head.status;
                    let headers = head.headers.clone();
                    let resp = build_streaming_response(head, chunk_rx, &trace_id);
                    spawn_streaming_journal(
                        state.clone(),
                        join,
                        StreamingJournalCtx {
                            trace_id: trace_id.clone(),
                            matched: matched.clone(),
                            request: request_envelope,
                            started,
                            status,
                            headers,
                        },
                    );
                    return Ok(resp);
                }
                // Sender dropped without `start` — handler finished or
                // trapped before streaming. Fall back to its result.
                Err(_) => join.await?,
            }
        }
        res = &mut join => res?,
    };

    let (call_result, handler_logs, mut resources) = outcome;
    let duration_ms = started.elapsed().as_millis() as u64;
    resources.wall_clock_ms = duration_ms;
    let handler_log_entries: Vec<HandlerLogEntry> = handler_logs
        .into_iter()
        .map(|r| HandlerLogEntry {
            level: r.level.as_str().to_string(),
            message: r.message,
            timestamp: r.timestamp,
        })
        .collect();

    let (response_envelope, dropped_response_headers, axum_response, error_msg, outcome_label) =
        match &call_result {
            Ok(wit_response) => {
                let (envelope, dropped) = summarize_response(
                    wit_response.status,
                    &wit_response.headers,
                    &wit_response.body,
                );
                let axum = build_axum_response_owned(wit_response);
                (envelope, dropped, axum, None, "ok")
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let body = msg.clone().into_bytes();
                let envelope = ResponseEnvelope {
                    status: 500,
                    headers: vec![],
                    body: body.clone(),
                    body_truncated: false,
                    original_body_size: body.len(),
                };
                tracing::error!(error = %e, "handler invocation failed");
                (
                    envelope,
                    Vec::new(),
                    error_response(StatusCode::INTERNAL_SERVER_ERROR, &msg),
                    Some(msg),
                    "handler_error",
                )
            }
        };

    span.record("outcome", outcome_label);

    // Best-effort journal write: a failure here is logged but doesn't
    // change what the SUT sees.
    let entry = NewJournalEntry {
        trace_id: trace_id.clone(),
        group_id: matched.route.group_id.clone(),
        group_name: matched.route.group_name.clone(),
        route_id: matched.route.id.clone(),
        route_number: matched.route.number,
        matched_pattern: matched.matched_pattern.clone(),
        request: request_envelope,
        response: response_envelope,
        path_params: matched.path_params.clone(),
        query: vec![],
        handler_logs: handler_log_entries,
        duration_ms,
        resources,
        error: error_msg,
        dropped_response_headers,
    };
    if let Err(e) = state.journal().record_handled(entry) {
        tracing::warn!(error = %e, "failed to record journal entry");
    }

    // Activity tracking: bump per-route counter + stamp last_hit_at /
    // last_activity_at. Best-effort like the journal write — a failure
    // here is logged but doesn't change what the SUT sees. Drives the
    // "most recently active" sort defaults on list endpoints.
    if let Err(e) = state.routes().registry().record_route_hit(
        &matched.route.group_id,
        &matched.route.id,
        chrono::Utc::now(),
    ) {
        tracing::warn!(error = %e, "activity tracking failed");
    }

    // Sliding TTL bump: best-effort. A failure here would mean Valkey
    // is unhappy, in which case the journal write above probably also
    // failed and the operator already knows. Don't punish the SUT.
    if let Err(e) = state
        .routes()
        .registry()
        .refresh_group_if_sliding(&matched.route.group_id)
    {
        tracing::warn!(error = %e, "sliding TTL bump failed");
    }

    let mut axum_response = axum_response;
    inject_response_trace_id(&trace_id, axum_response.headers_mut());
    Ok(axum_response)
}

/// Set `X-Trace-Id` to the inbound trace_id so the SUT can correlate
/// request <-> response by grepping its own logs without an external
/// trace store. No-op when no inbound `traceparent` was present — the
/// host doesn't manufacture one.
///
/// Why a custom header rather than echoing `traceparent` back? W3C
/// Trace Context only specifies `traceparent` on the request side; the
/// response-side `traceresponse` proposal is still draft and not
/// widely supported. Echoing `traceparent` would be a layering misuse
/// (semantically it claims to be a parent of a downstream span). A
/// plain `X-Trace-Id` is honest about being a correlation hint, not a
/// propagation primitive. If `traceresponse` finalizes, swap it in.
/// Slim down a `route_table::NearMiss` (which carries the full
/// `Route` including `compiled_wasm`) into the persistable
/// `UnmatchedNearMiss` shape stored on the journal record.
fn project_near_miss(nm: NearMiss) -> UnmatchedNearMiss {
    let route = format!("{}/{}", nm.route.group_name, nm.route.number);
    UnmatchedNearMiss {
        route,
        route_path: nm.route.path,
        route_methods: nm.route.methods,
        reason: match nm.reason {
            NearMissReason::MethodMismatch {
                expected_methods,
                got,
            } => UnmatchedNearMissReason::MethodMismatch {
                expected_methods,
                got,
            },
            NearMissReason::PrefixMatch {
                segment_index,
                expected,
                got,
            } => UnmatchedNearMissReason::PrefixMatch {
                segment_index,
                expected,
                got,
            },
        },
    }
}

fn inject_response_trace_id(trace_id: &Option<String>, headers: &mut HeaderMap) {
    let Some(tid) = trace_id else {
        return;
    };
    if let Ok(value) = http::HeaderValue::from_str(tid) {
        headers.insert(HeaderName::from_static("x-trace-id"), value);
    }
}

/// Pull the W3C trace_id out of an OTel context, returning the
/// 32-hex-char string when present and `None` when the context carries
/// the invalid sentinel (i.e., no inbound `traceparent`). Used to
/// stamp `trace_id` on journal records so they correlate with traces
/// in the OTel backend when both are configured.
fn extract_trace_id(cx: &opentelemetry::Context) -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    let tid = cx.span().span_context().trace_id();
    if tid == opentelemetry::trace::TraceId::INVALID {
        None
    } else {
        Some(tid.to_string())
    }
}

fn build_request_envelope(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
    body_limit: usize,
) -> RequestEnvelope {
    let header_pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_lowercase(), s.to_string()))
        })
        .collect();
    let (body, original_size, truncated) = truncate_body(body, body_limit);
    RequestEnvelope {
        method: method.as_str().to_uppercase(),
        path: uri.path().to_string(),
        headers: header_pairs,
        body,
        body_truncated: truncated,
        original_body_size: original_size,
    }
}

/// Decide which response headers the host will *send* (the journal
/// records the same set the SUT sees) and which it will *drop* (per
/// `reserved_response_header`). Returns the journal envelope and the
/// list of dropped header names.
fn summarize_response(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> (ResponseEnvelope, Vec<String>) {
    let mut kept = Vec::with_capacity(headers.len());
    let mut dropped = Vec::new();
    for (name, value) in headers {
        if reserved_response_header(name) {
            dropped.push(name.clone());
        } else {
            kept.push((name.clone(), value.clone()));
        }
    }
    let (body_bytes, original_size, truncated) = truncate_body(body.to_vec(), HANDLED_BODY_LIMIT);
    (
        ResponseEnvelope {
            status,
            headers: kept,
            body: body_bytes,
            body_truncated: truncated,
            original_body_size: original_size,
        },
        dropped,
    )
}

/// Maximum size of a mock-dispatch request body. Per storage-model.md's
/// `limits.request_body_size: 10MiB`. Anything larger gets rejected
/// with 413 *before* the handler is touched — protects the host from
/// OOM-by-curl. Path /__api/* uses its own (larger) limit so wasm
/// uploads still fit.
pub(crate) const MAX_DISPATCH_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Outcome of reading a request body. `TooLarge` is the typed
/// signal `dispatch_inner` needs so it can return 413 with a trace
/// header instead of bubbling a generic 500.
pub(crate) enum BodyReadOutcome {
    Ok(Vec<u8>),
    TooLarge,
}

async fn read_body(body: Body, max_bytes: usize) -> BodyReadOutcome {
    // `axum::body::to_bytes` enforces the cap by accumulating up to
    // `max_bytes` and erroring once a chunk would push past it. Any
    // error here (length-limit OR underlying IO failure) we map to
    // TooLarge for the dispatch path's purposes — the trace ID still
    // lands on the response so the operator can find the failed
    // request in logs.
    match axum::body::to_bytes(body, max_bytes).await {
        Ok(b) => BodyReadOutcome::Ok(b.to_vec()),
        Err(_) => BodyReadOutcome::TooLarge,
    }
}

fn build_wit_request(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
    matched_pattern: &str,
    path_params: &[(String, String)],
) -> WitRequest {
    let path = uri.path().to_string();

    let headers_vec: Vec<Header> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_lowercase(), s.to_string()))
        })
        .collect();

    WitRequest {
        method: method.as_str().to_uppercase(),
        path,
        matched_pattern: matched_pattern.to_string(),
        path_params: path_params.to_vec(),
        // Query parsing arrives with the request-body / debug surface
        // work later in the project; the WIT contract permits an empty
        // list when the host doesn't parse it.
        query: vec![],
        headers: headers_vec,
        body,
    }
}

fn build_axum_response_owned(resp: &WitResponse) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = Bytes::copy_from_slice(&resp.body);

    let mut builder = Response::builder().status(status);
    for (name, value) in &resp.headers {
        if reserved_response_header(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder.header(header::CONTENT_LENGTH, body.len());
    builder
        .body(Body::from(body))
        .expect("response build should not fail")
}

fn reserved_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "content-length" | "transfer-encoding" | "connection"
    )
}

/// Build the axum response for a streaming handler (ADR-0022). The head
/// (status + headers) is committed; the body is a stream fed by the
/// handler's `write-chunk` calls over `chunk_rx`. No `Content-Length`
/// is set, so hyper uses chunked transfer-encoding and the chunks reach
/// the client incrementally. Reserved headers are dropped as for
/// buffered responses; the client-disconnect signal travels back the
/// other way (dropping `chunk_rx` makes `write-chunk` return false).
fn build_streaming_response(
    head: StreamHead,
    chunk_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    trace_id: &Option<String>,
) -> Response {
    let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body_stream = ReceiverStream::new(chunk_rx)
        .map(|chunk| Ok::<Bytes, std::convert::Infallible>(Bytes::from(chunk)));

    let mut builder = Response::builder().status(status);
    for (name, value) in &head.headers {
        if reserved_response_header(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let mut resp = match builder.body(Body::from_stream(body_stream)) {
        Ok(r) => r,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("invalid streaming response head: {e}"),
        ),
    };
    inject_response_trace_id(trace_id, resp.headers_mut());
    resp
}

/// Inputs the deferred streaming-journal task needs once the handler
/// finishes. The body isn't captured (it streamed straight to the
/// client); slice 2 adds chunk/byte counts + a head/tail sample.
struct StreamingJournalCtx {
    trace_id: Option<String>,
    matched: MatchedRoute,
    request: RequestEnvelope,
    started: std::time::Instant,
    status: u16,
    headers: Vec<(String, String)>,
}

/// Spawn the journal write for a streamed response. Runs after the
/// handler's blocking task finishes (success or trap) — by then the
/// body has been streamed to the client. Best-effort, like the
/// buffered journal write.
fn spawn_streaming_journal(
    state: AppState,
    join: tokio::task::JoinHandle<Outcome>,
    ctx: StreamingJournalCtx,
) {
    tokio::spawn(async move {
        let (result, logs, mut resources) = match join.await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "streaming handler task join failed");
                return;
            }
        };
        let duration_ms = ctx.started.elapsed().as_millis() as u64;
        resources.wall_clock_ms = duration_ms;
        let handler_log_entries: Vec<HandlerLogEntry> = logs
            .into_iter()
            .map(|r| HandlerLogEntry {
                level: r.level.as_str().to_string(),
                message: r.message,
                timestamp: r.timestamp,
            })
            .collect();
        // A trap after `start` means the stream was cut mid-flight; the
        // client already saw the head + partial body. Record the error.
        let error_msg = result.err().map(|e| format!("{e:#}"));
        let response = ResponseEnvelope {
            status: ctx.status,
            headers: ctx.headers,
            body: Vec::new(),
            body_truncated: false,
            original_body_size: 0,
        };
        let entry = NewJournalEntry {
            trace_id: ctx.trace_id,
            group_id: ctx.matched.route.group_id.clone(),
            group_name: ctx.matched.route.group_name.clone(),
            route_id: ctx.matched.route.id.clone(),
            route_number: ctx.matched.route.number,
            matched_pattern: ctx.matched.matched_pattern.clone(),
            request: ctx.request,
            response,
            path_params: ctx.matched.path_params.clone(),
            query: vec![],
            handler_logs: handler_log_entries,
            duration_ms,
            resources,
            error: error_msg,
            dropped_response_headers: vec![],
        };
        if let Err(e) = state.journal().record_handled(entry) {
            tracing::warn!(error = %e, "failed to record streaming journal entry");
        }
        if let Err(e) = state.routes().registry().record_route_hit(
            &ctx.matched.route.group_id,
            &ctx.matched.route.id,
            chrono::Utc::now(),
        ) {
            tracing::warn!(error = %e, "activity tracking failed (streaming)");
        }
        if let Err(e) = state
            .routes()
            .registry()
            .refresh_group_if_sliding(&ctx.matched.route.group_id)
        {
            tracing::warn!(error = %e, "sliding TTL bump failed (streaming)");
        }
    });
}

fn not_found_response(reason: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"error":{{"code":"not_found","message":"{reason}"}}}}"#
        )))
        .expect("not-found response build should not fail")
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(
            HeaderName::from_static("x-wiremirage-error"),
            msg.lines().next().unwrap_or(msg),
        )
        .body(Body::from(msg.to_string()))
        .expect("error response build should not fail")
}

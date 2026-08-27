use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use http::header;
use opentelemetry::global;
use serde_json::json;
use tracing::Instrument;
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
/// (or trap), captured logs, resource usage, and — for streaming
/// handlers — a summary of what was streamed. The wall-clock field is
/// filled in by the async caller.
type Outcome = (
    Result<WitResponse, wasmtime::Error>,
    Vec<LogRecord>,
    ResourceUsage,
    Option<StreamSummary>,
    // Outbound callbacks the handler scheduled (ADR-0034), drained from
    // HostState before the store drops. Empty on the non-engine path and
    // whenever the handler scheduled none.
    Vec<crate::callout::ScheduledCallback>,
);

/// What a streaming handler produced, captured from `HostState` before
/// the store drops so the deferred journal task can record it
/// (ADR-0022 slice 2).
struct StreamSummary {
    chunks: u64,
    bytes: u64,
    /// Terminal state: `finished` (handler returned cleanly),
    /// `client_disconnected` (a write failed because the peer left),
    /// or `trapped` (handler trapped mid-stream, incl. budget-exceeded).
    disposition: &'static str,
}

impl StreamSummary {
    /// Derive the summary from the per-invocation stats and the call
    /// result. `None` if the handler didn't stream.
    fn from_state(stats: Option<(u64, u64, bool)>, result_ok: bool) -> Option<Self> {
        let (chunks, bytes, client_gone) = stats?;
        let disposition = if client_gone {
            "client_disconnected"
        } else if result_ok {
            "finished"
        } else {
            "trapped"
        };
        Some(StreamSummary {
            chunks,
            bytes,
            disposition,
        })
    }
}

/// Which routing target a request's `Host` header resolves to under
/// virtual-host routing (ADR-0030).
enum HostKind {
    /// The bare apex — control-plane only, serves no mock traffic.
    Apex,
    /// A `{group}.{apex}` subdomain; the captured label is the group name.
    Group(String),
    /// Anything else — a foreign host, a bare IP, or a multi-level name —
    /// with no group to route to.
    Foreign,
}

/// Resolve the request `Host` against the configured apex. The port is
/// stripped and the host lowercased before comparison; `apex` is already
/// lowercased on `AppState`.
fn resolve_host_kind(headers: &HeaderMap, apex: &str) -> HostKind {
    let raw = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let host = raw
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        return HostKind::Foreign;
    }
    if host == apex {
        return HostKind::Apex;
    }
    if let Some(label) = host.strip_suffix(&format!(".{apex}")) {
        // Single-level subdomains only: a label with an embedded dot
        // (`a.b.{apex}`) isn't a group we serve.
        if !label.is_empty() && !label.contains('.') {
            return HostKind::Group(label.to_string());
        }
    }
    HostKind::Foreign
}

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
    /// When true, append `Secure` to session + CSRF cookies. Enabled by
    /// `WM_TRUSTED_PROXY` (ADR-0027) — i.e. when the host is behind an
    /// HTTPS-terminating edge. Default off so dev workflows over plain
    /// HTTP keep working (browsers drop `Secure` cookies on non-TLS
    /// connections, which would break login).
    secure_cookies: bool,
    /// When true, honor `X-Forwarded-For` for the login throttle's
    /// per-IP key and `X-Forwarded-Proto`/`-Host` for OAuth redirect-URI
    /// derivation. Enabled by `WM_TRUSTED_PROXY` (ADR-0027). Default off
    /// — the placeholder IP is used otherwise, which collapses everyone
    /// into one throttle bucket but makes IP spoofing impossible.
    trust_forwarded_headers: bool,
    /// Public hostname(s) the trusted edge serves, added to rmcp's MCP
    /// `Host`-header allowlist on top of the localhost defaults (ADR-0027,
    /// DNS-rebinding protection). Empty = MCP accepts localhost only. Set
    /// from `WM_TRUSTED_PROXY`.
    mcp_allowed_hosts: Vec<String>,
    /// The apex hostname this instance serves on (ADR-0030 virtual-host
    /// routing). Mock traffic is served on group subdomains
    /// `{group}.{apex_host}`; the apex itself is control-plane only.
    /// Derived in `main.rs` from `WM_APEX_HOST`, else the first
    /// `WM_TRUSTED_PROXY` host, else `localhost` (dev default). Stored
    /// lowercased. Compared against the request `Host` header to resolve
    /// which group a mock request targets.
    apex_host: String,
    /// GitHub OAuth config, populated when both `WM_GITHUB_CLIENT_ID`
    /// and `WM_GITHUB_CLIENT_SECRET` are set. `None` means the login
    /// page hides the "Continue with GitHub" button and the
    /// `/auth/start/github` + `/auth/callback` routes respond
    /// 503. Kept behind `Arc` so cloning `AppState` shares it.
    github_oauth: Option<Arc<crate::github_oauth::GitHubConfig>>,
    /// Generic OIDC provider (ADR-0035), populated when `WM_OIDC_ISSUER`
    /// is set and discovery succeeded at startup. `None` means the login
    /// page hides the OIDC button and the `/auth/start/oidc` +
    /// `/auth/callback/oidc` routes respond 503.
    oidc: Option<Arc<crate::oidc::OidcProvider>>,
    /// Outbound-callback egress policy (ADR-0034). Default `disabled()` —
    /// callbacks are off unless the operator sets `WM_EGRESS`. Behind an
    /// `Arc` so cloning `AppState` (per request) is cheap and the firing
    /// background tasks share one policy.
    egress: Arc<crate::egress::EgressPolicy>,
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
            mcp_allowed_hosts: Vec::new(),
            apex_host: "localhost".to_string(),
            github_oauth: None,
            oidc: None,
            egress: Arc::new(crate::egress::EgressPolicy::disabled()),
        }
    }

    /// Set the outbound-callback egress policy (ADR-0034). Wired from
    /// `WM_EGRESS*` in `main.rs`; defaults to fully disabled.
    pub fn with_egress(mut self, egress: crate::egress::EgressPolicy) -> Self {
        self.egress = Arc::new(egress);
        self
    }

    pub fn egress(&self) -> &Arc<crate::egress::EgressPolicy> {
        &self.egress
    }

    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    pub fn with_trust_forwarded_headers(mut self, trust: bool) -> Self {
        self.trust_forwarded_headers = trust;
        self
    }

    /// Public hostname(s) added to the MCP `Host`-header allowlist
    /// (ADR-0027). Set from `WM_TRUSTED_PROXY`.
    pub fn with_mcp_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.mcp_allowed_hosts = hosts;
        self
    }

    pub fn secure_cookies(&self) -> bool {
        self.secure_cookies
    }

    pub fn trust_forwarded_headers(&self) -> bool {
        self.trust_forwarded_headers
    }

    pub fn mcp_allowed_hosts(&self) -> &[String] {
        &self.mcp_allowed_hosts
    }

    /// Set the apex hostname (ADR-0030). Stored lowercased so Host-header
    /// comparison is case-insensitive.
    pub fn with_apex_host(mut self, apex: impl Into<String>) -> Self {
        self.apex_host = apex.into().to_ascii_lowercase();
        self
    }

    pub fn apex_host(&self) -> &str {
        &self.apex_host
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

    pub fn with_oidc(mut self, provider: crate::oidc::OidcProvider) -> Self {
        self.oidc = Some(Arc::new(provider));
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

    pub fn oidc(&self) -> Option<&crate::oidc::OidcProvider> {
        self.oidc.as_deref()
    }

    pub fn login_throttle(&self) -> &crate::login_throttle::LoginThrottle {
        &self.login_throttle
    }

    pub fn ui_templates(&self) -> &crate::ui::UiTemplates {
        &self.ui_templates
    }
}

/// Build the axum router. The REST API mounts at `/api/*`; mock-traffic
/// dispatch is the fallback. The fallback rejects requests under reserved
/// prefixes (e.g., `/api/typo`) with 404 before consulting user routes.
/// MCP (`/api/mcp`) merges in as a separate sub-router with its own
/// auth layer.
pub fn router(state: AppState) -> Router {
    let mcp = crate::mcp::router(state.clone());
    // ADR-0024 slice 2: control-plane HTTP metrics. `route_layer` runs
    // after routing (so `MatchedPath` is populated) and does NOT wrap
    // the fallback — mock dispatch keeps its own `wm.dispatch.*` metrics
    // and is never double-counted here. The same middleware layers onto
    // the UI sub-router, which merges in after `with_state`. The MCP
    // streamable service is intentionally not instrumented at the HTTP
    // boundary (per-tool metrics are a deferred slice).
    let ui =
        crate::ui::router(state.clone()).route_layer(middleware::from_fn(internal_http_metrics));
    crate::api::router()
        .merge(crate::auth_api::router(state.clone()))
        .merge(crate::mcp_oauth::router(state.clone()))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route_layer(middleware::from_fn(internal_http_metrics))
        .fallback(any(dispatch))
        .with_state(state.clone())
        .merge(mcp)
        .merge(ui)
        // ADR-0033: control-plane is served on the apex (and on direct/loopback
        // access); a recognized `{group}.{apex}` subdomain is pure mock space,
        // so divert it to mock dispatch before any control-plane route can
        // match — letting a tenant mock /health, /api/*, /auth/*, etc.
        .layer(middleware::from_fn_with_state(
            state,
            control_plane_apex_gate,
        ))
}

/// ADR-0033 apex-only control-plane routing. The control-plane surfaces
/// (/api, /ui, /auth, /health, /ready, MCP) live at the apex. A request to a
/// recognized `{group}.{apex}` subdomain is mock traffic for that group, so we
/// divert it to mock dispatch before the path-based control-plane routes can
/// match — that is what lets a tenant mock /health, /api/*, /auth/*, etc.
/// Everything else (the apex itself, direct IP / loopback access) falls through
/// to the control-plane router, which is auth-gated as before.
async fn control_plane_apex_gate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    match resolve_host_kind(req.headers(), state.apex_host()) {
        HostKind::Group(_) => dispatch(State(state), req).await,
        _ => next.run(req).await,
    }
}

/// Map a matched axum route template to its control-plane surface label,
/// or `None` for paths we don't record (the health/ready probes — high
/// frequency, low operator value). The label space is a fixed enum so
/// `wm.surface` stays bounded. ADR-0024 slice 2.
fn surface_for_route(route: &str) -> Option<&'static str> {
    if route.starts_with("/api/mcp") {
        Some("mcp")
    } else if route.starts_with("/api") {
        Some("api")
    } else if route.starts_with("/ui") {
        Some("ui")
    } else if route.starts_with("/auth") || route.starts_with("/.well-known") {
        Some("auth")
    } else {
        None
    }
}

/// Middleware for the control-plane (API / UI / auth) surfaces. Records
/// the `http.server.*` metrics AND opens a request span so internal
/// traffic shows up in traces with per-route latency — the analogue of
/// arkiv's framework-level request spans, and what lets a trace backend
/// answer "p95 latency for `/api/groups/{group}`" that the routeless
/// aggregate metrics can't.
///
/// The route *template* comes from `MatchedPath` (never the resolved
/// path, so path-param values can't explode `http.route` cardinality);
/// `wm.surface` is the bounded api/ui/auth enum. Requests whose surface
/// is `None` (the health/ready probes) pass through untouched. Static
/// assets get the metric but NOT a span — they're high-volume and
/// low-diagnostic-value, and per-asset spans would bloat trace storage
/// (same rationale as arkiv excluding its NiceGUI static traffic).
async fn internal_http_metrics(req: Request, next: Next) -> Response {
    let Some(route) = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
    else {
        return next.run(req).await;
    };
    let Some(surface) = surface_for_route(&route) else {
        return next.run(req).await;
    };
    let method = req.method().as_str().to_owned();

    // Body size from Content-Length when present — recording a 0 for
    // chunked/absent bodies would skew the histogram, so skip instead.
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        crate::metrics::record_internal_request_body_bytes(surface, &method, len);
    }

    // Static-asset routes get metrics but no span (volume control).
    let span = if route.starts_with("/ui/static") {
        tracing::Span::none()
    } else {
        tracing::info_span!(
            "http.server.request",
            http.request.method = %method,
            http.route = %route,
            wm.surface = surface,
            http.response.status_code = tracing::field::Empty,
        )
    };

    async move {
        let _in_flight = crate::metrics::internal_in_flight(surface, &method);
        let started = std::time::Instant::now();
        let resp = next.run(req).await;
        let status = resp.status().as_u16();
        tracing::Span::current().record("http.response.status_code", status);
        crate::metrics::record_internal_http(
            surface,
            &method,
            &route,
            status,
            started.elapsed().as_secs_f64(),
        );
        resp
    }
    .instrument(span)
    .await
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

/// True iff the request carries a `wm_session` cookie that resolves to
/// a live session. Used by the bare-`/` redirect to pick between
/// `/ui/` (signed in) and `/auth/login` (not). Reuses the same
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

/// Message for a failed handler call (500 body + journal `error`). A clean JS
/// exception from the engine is already a readable one-liner, so pass it
/// through. A wasm-level *trap* otherwise dumps an opaque backtrace that's
/// useless to the handler author — replace it with a short, actionable line.
/// The raw error is still emitted to the host trace for operators.
fn friendly_handler_error(e: &wasmtime::Error) -> String {
    let raw = format!("{e:#}");
    let lower = raw.to_ascii_lowercase();
    // Only intercept genuine wasm traps (the cases that dump a backtrace);
    // a JS error that merely mentions "unreachable" must pass through.
    let is_wasm_trap = lower.contains("wasm trap")
        || lower.contains("wasm backtrace")
        || (lower.contains("unreachable") && lower.contains("wasm"));
    if !is_wasm_trap {
        return raw;
    }
    if lower.contains("epoch") || lower.contains("interrupt") {
        "handler exceeded its time budget (epoch deadline)".to_string()
    } else if lower.contains("fuel") {
        "handler exceeded its CPU budget (out of fuel)".to_string()
    } else if lower.contains("memory") || lower.contains("allocation") {
        "handler exceeded its memory budget".to_string()
    } else {
        "handler trapped (engine-level fault) — common causes: calling an \
         unsupported web API (e.g. network access) or a bug in the handler; \
         the raw wasm backtrace is in the host logs/trace"
            .to_string()
    }
}

// Span fields kept low-cardinality on purpose: `http.method` and
// `route.matched_pattern` are bounded; the raw URL path with path-param
// values is deliberately omitted so OTel attribute cardinality stays
// finite. `route.id` is recorded after a match so a span can be located
// by route ULID, but it's only on matched-route spans.
//
// ADR-0024 slice 2: the per-route resource attributes (fuel / memory /
// wall on the buffered path, head-latency on the streaming path) ride
// the span rather than a labeled metric — traces are exemplar-shaped,
// so the route-level cardinality that's forbidden on metrics is exactly
// what trace backends are built to slice. "p95 fuel for /v1/charges" is
// a span query; the aggregate `wm.handler.*` histograms stay routeless.
#[tracing::instrument(
    name = "dispatch",
    skip_all,
    fields(
        http.method = %req.method(),
        route.matched_pattern = tracing::field::Empty,
        route.id = tracing::field::Empty,
        outcome = tracing::field::Empty,
        handler.fuel_consumed = tracing::field::Empty,
        handler.memory_peak_bytes = tracing::field::Empty,
        handler.wall_ms = tracing::field::Empty,
        streaming.head_latency_ms = tracing::field::Empty,
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

    // ADR-0033: there are no globally reserved paths. The control-plane
    // surfaces (/api, /ui, /auth, /health, /ready, MCP) are served by the
    // router on the apex / direct-access hosts; a request to a `{group}.{apex}`
    // subdomain is diverted to mock dispatch wholesale by
    // `control_plane_apex_gate` before it can reach the router, so a tenant can
    // mock any path. A request reaching this fallback is therefore either an
    // unmatched apex path (handled in the `Apex` arm below — including the
    // branded `/ui/*` 404) or mock traffic on a subdomain.

    // ADR-0024: claim a mock-dispatch in-flight slot now that we've
    // ruled out internal-prefix traffic. The guard decrements on drop,
    // including on every early-return below; for streaming dispatches
    // we transfer ownership into the streaming journal task so the
    // gauge tracks the request for as long as the stream is pumping.
    let mut in_flight = Some(crate::metrics::dispatch_in_flight(method.as_str()));
    crate::metrics::record_request_body_bytes(method.as_str(), body_bytes.len() as u64);

    // Resolve the request Host to a group (ADR-0030 / ADR-0033 virtual-host
    // routing). Mock traffic is served on `{group}.{apex}` subdomains. A
    // group subdomain is diverted to mock dispatch by `control_plane_apex_gate`
    // before routing, so reaching this fallback for a non-subdomain host (the
    // apex itself, loopback, a bare IP) means an *unmatched control-plane path*
    // — the host serves no mock traffic, so it's never journaled (no group to
    // attribute it to).
    let group_name = match resolve_host_kind(&header_map, state.apex_host()) {
        HostKind::Apex | HostKind::Foreign => {
            // Bare `GET /` bounces a human to the UI (with a session) or login.
            if method == http::Method::GET && path == "/" {
                let target = if has_valid_session(&state, &header_map) {
                    "/ui/"
                } else {
                    "/auth/login"
                };
                span.record("outcome", "root_redirect");
                let mut resp = axum::response::Redirect::to(target).into_response();
                inject_response_trace_id(&trace_id, resp.headers_mut());
                return Ok(resp);
            }
            // A `/ui/*` path that no UI route matched is a human at a bad URL —
            // render the branded HTML 404 page rather than a JSON blob.
            if path.starts_with("/ui/") {
                span.record("outcome", "ui_not_found");
                let mut resp = crate::ui::render_not_found(&state, path);
                inject_response_trace_id(&trace_id, resp.headers_mut());
                return Ok(resp);
            }
            span.record("outcome", "control_plane_404");
            crate::metrics::record_dispatch(
                method.as_str(),
                404,
                "control_plane_404",
                started.elapsed().as_millis() as u64,
            );
            let mut resp = not_found_response("not found");
            inject_response_trace_id(&trace_id, resp.headers_mut());
            return Ok(resp);
        }
        HostKind::Group(label) => label,
    };

    let matched = match state
        .routes
        .find_match_in_group(&group_name, method.as_str(), path)
    {
        Some(m) => m,
        None => {
            // No route in this group matched. Distinguish an unknown
            // subdomain (no such group → 404, nothing to journal) from a
            // real group with no matching route (record an unmatched
            // entry, with near-misses scoped to the group).
            let group = match state.routes().registry().read_group_by_ref(&group_name) {
                Ok(g) => g,
                Err(_) => {
                    span.record("outcome", "unknown_group_404");
                    crate::metrics::record_dispatch(
                        method.as_str(),
                        404,
                        "unknown_group_404",
                        started.elapsed().as_millis() as u64,
                    );
                    let mut resp = not_found_response("no such group");
                    inject_response_trace_id(&trace_id, resp.headers_mut());
                    return Ok(resp);
                }
            };

            // ADR-0037 item 1: the local route table is a cache, and
            // under more than one replica a route created elsewhere may
            // simply not be in it yet — the common agent workflow is
            // "create a route, immediately send traffic to it". Reload
            // this group from storage and try once more before calling
            // it unmatched. Rate-limited per group inside the table, so
            // junk traffic can't amplify into a storage flood.
            match state
                .routes
                .revalidate_and_rematch(&group.id, &group.name, method.as_str(), path)
            {
                Some(m) => {
                    tracing::debug!(
                        group = %group.name,
                        route = %m.route.number,
                        "route resolved by read-through revalidation"
                    );
                    m
                }
                None => {
                    span.record("outcome", "unmatched_404");
                    // Near-misses scoped to this group (ADR-0030) so "did you
                    // mean…?" suggestions stay within the tenant's own routes.
                    let near_misses: Vec<UnmatchedNearMiss> = state
                        .routes()
                        .compute_near_misses_in_group(&group_name, method.as_str(), uri.path())
                        .into_iter()
                        .map(project_near_miss)
                        .collect();
                    // Journal the unmatched request, attributed to the group it was
                    // addressed to (ADR-0030) so the group's owner can see it — not
                    // just admins. Best-effort: a journal failure is logged but
                    // doesn't change what the SUT sees.
                    let envelope = build_request_envelope(
                        &method,
                        &uri,
                        &header_map,
                        body_bytes,
                        UNMATCHED_BODY_LIMIT,
                    );
                    if let Err(e) = state.journal().record_unmatched(NewUnmatchedEntry {
                        trace_id: trace_id.clone(),
                        group_id: group.id.clone(),
                        group_name: group.name.clone(),
                        request: envelope,
                        near_misses,
                    }) {
                        tracing::warn!(error = %e, "failed to record unmatched journal entry");
                    }
                    crate::metrics::record_dispatch(
                        method.as_str(),
                        404,
                        "unmatched_404",
                        started.elapsed().as_millis() as u64,
                    );
                    let mut resp = not_found_response("no route matched");
                    inject_response_trace_id(&trace_id, resp.headers_mut());
                    return Ok(resp);
                }
            }
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

    // Branch on language: source-language routes (ADR-0020) run
    // through the shared engine; everything else (`wasm` uploads,
    // future AOT languages) still goes through the per-route
    // component cache. The stored `source` is the *original* author
    // source; `engine_source_for` returns the JS the engine runs
    // (TS transpiled + cached, JS as-is).
    let use_engine = matches!(route_language.as_str(), "javascript" | "typescript");
    let (component, route_source) = if use_engine {
        (None, Some(state.routes.engine_source_for(&matched.route)?))
    } else {
        (Some(state.routes.component_for(&matched.route)?), None)
    };

    // ADR-0034: may this request's handler schedule outbound callbacks? Only
    // on the source-language (engine) path, and only when the host has egress
    // enabled at all — so the extra per-request group read (for the group's
    // `callout_enabled` flag) is paid only in egress-on deployments, never in
    // the default-off case. The per-IP egress decision still happens at fire
    // time; this is the capability + per-group opt-in gate.
    let callouts_allowed = use_engine
        && state.egress().is_enabled()
        && state
            .routes()
            .registry()
            .read_group_by_ref(&group_id)
            .map(|g| g.callout_enabled)
            .unwrap_or(false);

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
                        None,
                        Vec::new(),
                    );
                }
            };
            let instantiate_span =
                tracing::info_span!("wasmtime.engine_instantiate", route.id = %route_id).entered();
            let (engine_world, mut store, handles) =
                match runtime.instantiate_engine(&group_id, &route_id, source, callouts_allowed) {
                    Ok(t) => t,
                    Err(e) => {
                        return (
                            Err(e),
                            Vec::new(),
                            ResourceUsage::default(),
                            None,
                            Vec::new(),
                        );
                    }
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
            // Capture streaming stats before the store drops (ADR-0022
            // slice 2) so the deferred journal task can record them.
            let stream_summary =
                StreamSummary::from_state(store.data().stream_stats(), result.is_ok());
            let logs = store.data_mut().take_logs();
            // ADR-0034: drain any callbacks the handler scheduled, before
            // the store drops. The dispatch task fires them after the
            // response is sent.
            let callbacks = store.data_mut().take_scheduled_callbacks();
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
            (result, logs, resources, stream_summary, callbacks)
        } else {
            let component = component.expect("non-engine path requires a component");
            let instantiate_span =
                tracing::info_span!("wasmtime.instantiate", route.id = %route_id).entered();
            let (handler, mut store, handles) =
                match runtime.instantiate(&component, &group_id, &route_id) {
                    Ok(t) => t,
                    Err(e) => {
                        return (
                            Err(e),
                            Vec::new(),
                            ResourceUsage::default(),
                            None,
                            Vec::new(),
                        );
                    }
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
            // Per-route components can't stream (the user-facing
            // `world handler` has no response-stream import) and can't
            // schedule callbacks (no callback import), so no summary and
            // no callbacks on this path.
            (result, logs, resources, None, Vec::new())
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
                    // ADR-0024: TTFB and the dispatch-metric record
                    // both stamp NOW (head delivery is the dispatch
                    // boundary for streaming; the chunk pump that
                    // follows lives on the streaming clock).
                    let head_latency_ms = started.elapsed().as_millis() as u64;
                    crate::metrics::record_streaming_head(head_latency_ms);
                    crate::metrics::record_dispatch(
                        method.as_str(),
                        status,
                        "streaming",
                        head_latency_ms,
                    );
                    // Per-route TTFB on the span (ADR-0024 slice 2) — the
                    // one resource number known before the dispatch span
                    // closes on the streaming path.
                    span.record("streaming.head_latency_ms", head_latency_ms);
                    let head_delivered_at = std::time::Instant::now();
                    let resp = build_streaming_response(head, chunk_rx, &trace_id);
                    // Hand the in-flight slot off to the streaming
                    // task — `wm.dispatch.active_requests` should
                    // count this dispatch for as long as the stream
                    // is actually pumping bytes, not just until the
                    // head goes out.
                    let in_flight = in_flight
                        .take()
                        .expect("in-flight slot present on streaming path");
                    spawn_streaming_journal(
                        state.clone(),
                        join,
                        StreamingJournalCtx {
                            trace_id: trace_id.clone(),
                            matched: matched.clone(),
                            request: request_envelope,
                            started,
                            head_delivered_at,
                            status,
                            headers,
                            in_flight,
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

    // The buffered path ignores the stream summary (None here — a
    // handler that streamed took the early return above).
    let (call_result, handler_logs, mut resources, _stream_summary, scheduled_callbacks) = outcome;
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
                let msg = friendly_handler_error(e);
                let body = msg.clone().into_bytes();
                let envelope = ResponseEnvelope {
                    status: 500,
                    headers: vec![],
                    body: body.clone(),
                    body_truncated: false,
                    original_body_size: body.len(),
                };
                tracing::error!(error = %e, "handler invocation failed");
                // ADR-0024: bucket the trap by reason. Slice 1 uses a
                // string match on the formatted error — adequate for
                // operator-side aggregation, and trivially upgradable
                // to `wasmtime::Trap` downcasting later if the
                // pattern-match becomes brittle.
                crate::metrics::record_handler_trap(crate::metrics::classify_trap(e));
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

    // ADR-0024: record the dispatch + handler-resource metrics. The
    // `outcome_label` and `wm.dispatch.outcome` attribute use the same
    // enum as the span field, so an operator filtering by outcome on a
    // metric can pivot to spans with the same value.
    crate::metrics::record_dispatch(
        method.as_str(),
        response_envelope.status,
        outcome_label,
        duration_ms,
    );
    crate::metrics::record_handler_resources(
        outcome_label,
        resources.fuel_consumed,
        resources.memory_peak_bytes,
        resources.wall_clock_ms,
    );
    // ADR-0024 slice 2: per-route resource detail on the span (see the
    // instrument-macro comment). Buffered path only — the streaming path
    // records head-latency at head-emit instead, since fuel/memory/wall
    // aren't known until after the dispatch span has closed.
    span.record("handler.fuel_consumed", resources.fuel_consumed);
    span.record("handler.memory_peak_bytes", resources.memory_peak_bytes);
    span.record("handler.wall_ms", resources.wall_clock_ms);

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

    // ADR-0034: fire any callbacks the handler scheduled, on background
    // tasks, now that the response is built and the request is journaled.
    // Each task applies the egress filter against the resolved IP and
    // records its own outcome in the group's callback journal. No-op when
    // the handler scheduled none (the common case, and always so unless
    // egress is on and the group opted in).
    if !scheduled_callbacks.is_empty() {
        crate::callout::spawn_callbacks(
            state.journal().clone(),
            state.egress().clone(),
            crate::callout::CallbackContext {
                trace_id: trace_id.clone(),
                group_id: matched.route.group_id.clone(),
                group_name: matched.route.group_name.clone(),
                route_id: matched.route.id.clone(),
                route_number: matched.route.number,
            },
            scheduled_callbacks,
        );
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
/// OOM-by-curl. Path /api/* uses its own (larger) limit so wasm
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

    // Parse the URL query string into (name, value) pairs. Names are
    // lowercased to match the WIT contract (`request.query` doc); both halves
    // are form-decoded (`+` → space, then percent-decoding).
    let query: Vec<Header> = uri
        .query()
        .map(|q| {
            q.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (form_decode(k).to_lowercase(), form_decode(v))
                })
                .collect()
        })
        .unwrap_or_default();

    WitRequest {
        method: method.as_str().to_uppercase(),
        path,
        matched_pattern: matched_pattern.to_string(),
        path_params: path_params.to_vec(),
        query,
        headers: headers_vec,
        body,
    }
}

/// Decode one application/x-www-form-urlencoded component: `+` is a space,
/// then percent-decode. Falls back to the raw input on malformed escapes.
fn form_decode(s: &str) -> String {
    let plus_decoded = s.replace('+', " ");
    urlencoding::decode(&plus_decoded)
        .map(|c| c.into_owned())
        .unwrap_or(plus_decoded)
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
    /// Instant at which the response head reached the wire — used to
    /// compute `wm.streaming.duration_ms` (post-TTFB) separately from
    /// `wm.streaming.head_latency_ms`. ADR-0024.
    head_delivered_at: std::time::Instant,
    status: u16,
    headers: Vec<(String, String)>,
    /// In-flight slot owned by the streaming dispatch. Dropped when
    /// the streaming-journal task finishes so `wm.dispatch.active_requests`
    /// tracks the live stream, not just the time to head delivery.
    /// ADR-0024.
    in_flight: crate::metrics::InFlightGuard,
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
        // `in_flight` is owned by this task — moved out of `ctx` so it
        // drops only at task end, even on the early-return error path
        // below. That's intentional: the dispatch is "in flight" for
        // as long as the streaming task is alive.
        let StreamingJournalCtx {
            trace_id,
            matched,
            request,
            started,
            head_delivered_at,
            status,
            headers,
            in_flight,
        } = ctx;
        let _in_flight = in_flight;
        let (result, logs, mut resources, summary, scheduled_callbacks) = match join.await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "streaming handler task join failed");
                return;
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        resources.wall_clock_ms = duration_ms;
        let mut handler_log_entries: Vec<HandlerLogEntry> = logs
            .into_iter()
            .map(|r| HandlerLogEntry {
                level: r.level.as_str().to_string(),
                message: r.message,
                timestamp: r.timestamp,
            })
            .collect();
        // A trap after `start` means the stream was cut mid-flight; the
        // client already saw the head + partial body. Record the error.
        let mut error_msg = result.err().map(|e| format!("{e:#}"));
        // ADR-0022 slice 2: record stream chunk/byte counts + the
        // terminal disposition. The body itself streamed straight to
        // the client and isn't captured here, so `original_body_size`
        // carries the byte total and `body_truncated` flags the
        // not-captured body. A non-`finished` disposition surfaces in
        // the error field; the count summary goes in a synthetic
        // host log line so `wm journal show` reflects it.
        let (chunks, bytes, disposition) = summary
            .map(|s| (s.chunks, s.bytes, s.disposition))
            .unwrap_or((0, 0, "finished"));
        handler_log_entries.push(HandlerLogEntry {
            level: "info".to_string(),
            message: format!("[stream] {chunks} chunks, {bytes} bytes, {disposition}"),
            timestamp: chrono::Utc::now(),
        });
        if error_msg.is_none() && disposition != "finished" {
            error_msg = Some(format!("stream {disposition} after {chunks} chunks"));
        }
        // ADR-0024: record streaming-completion metrics + the same
        // handler-resource histograms the buffered path records.
        // Outcome bucket for resources is `ok` on a clean stream,
        // `handler_error` on any trap.
        let resource_outcome = if error_msg.is_some() {
            "handler_error"
        } else {
            "ok"
        };
        crate::metrics::record_handler_resources(
            resource_outcome,
            resources.fuel_consumed,
            resources.memory_peak_bytes,
            resources.wall_clock_ms,
        );
        let streaming_duration_ms = head_delivered_at.elapsed().as_millis() as u64;
        crate::metrics::record_streaming_completion(
            chunks,
            bytes,
            disposition,
            streaming_duration_ms,
        );
        let response = ResponseEnvelope {
            status,
            headers,
            body: Vec::new(),
            body_truncated: true,
            original_body_size: bytes as usize,
        };
        let entry = NewJournalEntry {
            // Cloned (not moved) so the trace_id is still available for the
            // callback context fired below (ADR-0034).
            trace_id: trace_id.clone(),
            group_id: matched.route.group_id.clone(),
            group_name: matched.route.group_name.clone(),
            route_id: matched.route.id.clone(),
            route_number: matched.route.number,
            matched_pattern: matched.matched_pattern.clone(),
            request,
            response,
            path_params: matched.path_params.clone(),
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
            &matched.route.group_id,
            &matched.route.id,
            chrono::Utc::now(),
        ) {
            tracing::warn!(error = %e, "activity tracking failed (streaming)");
        }
        if let Err(e) = state
            .routes()
            .registry()
            .refresh_group_if_sliding(&matched.route.group_id)
        {
            tracing::warn!(error = %e, "sliding TTL bump failed (streaming)");
        }
        // ADR-0034: fire callbacks a streaming handler scheduled, same as
        // the buffered path — once the stream has completed and the entry
        // is journaled.
        if !scheduled_callbacks.is_empty() {
            crate::callout::spawn_callbacks(
                state.journal().clone(),
                state.egress().clone(),
                crate::callout::CallbackContext {
                    trace_id: trace_id.clone(),
                    group_id: matched.route.group_id.clone(),
                    group_name: matched.route.group_name.clone(),
                    route_id: matched.route.id.clone(),
                    route_number: matched.route.number,
                },
                scheduled_callbacks,
            );
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

#[cfg(test)]
mod tests {
    use super::friendly_handler_error;

    #[test]
    fn clean_js_error_passes_through() {
        // A JS exception from the engine is already a readable one-liner.
        let e = wasmtime::Error::msg("ReferenceError: log is not defined");
        assert_eq!(
            friendly_handler_error(&e),
            "ReferenceError: log is not defined"
        );
    }

    #[test]
    fn js_error_mentioning_unreachable_is_not_masked() {
        // No wasm-trap markers → not a trap → keep the handler's own message.
        let e = wasmtime::Error::msg("Error: upstream unreachable");
        assert_eq!(friendly_handler_error(&e), "Error: upstream unreachable");
    }

    #[test]
    fn unreachable_trap_becomes_generic_line() {
        let e = wasmtime::Error::msg(
            "error while executing at wasm backtrace:\n  0: <unknown>!<wasm function 1234>\n\
             Caused by: wasm trap: wasm `unreachable` instruction executed",
        );
        let msg = friendly_handler_error(&e);
        assert!(msg.contains("handler trapped"), "got: {msg}");
        assert!(!msg.contains("wasm function"), "backtrace stripped: {msg}");
    }

    #[test]
    fn epoch_trap_reports_time_budget() {
        let e = wasmtime::Error::msg("wasm trap: interrupt");
        assert_eq!(
            friendly_handler_error(&e),
            "handler exceeded its time budget (epoch deadline)"
        );
    }
}

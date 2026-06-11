//! REST API at `/api/*`.
//!
//! Routes (`/api/routes`): POST/GET/DELETE per `rest-api.md`. The
//! only public artifact input is source (`language: "javascript" |
//! "typescript"`, `source`); pre-compiled wasm upload was retired in
//! ADR-0023. Requests are handled in-process: JS is stored verbatim and
//! dispatched through the shared `js-engine.wasm` component, TS goes
//! through `ts_transpile::transpile` (pure-Rust swc) first and is stored
//! as JS. No external compiler.
//!
//! Tokens (`/api/tokens`): POST/GET/DELETE for the caller's own
//! tokens, per ADR-0012. Plaintext is returned exactly once, in the
//! create response.
//!
//! Every handler under `/api/*` requires a valid bearer token; the
//! `AuthContext` extractor (impl below) returns 401 on missing /
//! invalid / expired. Mock-traffic dispatch (the fallback in
//! `server::router`) is intentionally open — SUTs don't have tokens.

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use crate::api_filters::{FilterParseError, SortDir, glob_match, parse_since, validate_method};
use crate::auth::{AuthContext, AuthError, Token, User};
use crate::journal::{
    CallbackRecord, JournalError, JournalRecord, ListCursor, UnmatchedCursor, UnmatchedRecord,
};
use crate::journal_filter::{JournalFilter, RouteSlug, StatusFilter};
use crate::registry::{
    Group, NewGroup, NewRoute, PatchRoute, RegistryError, Route, RouteStateEntry, render_slug,
};
use crate::wire::WireBytes;
use crate::{AppState, SUPPORTED_BINDINGS_VERSION};

/// Maximum size of a `/api/*` JSON body. axum's default is 2 MiB;
/// `POST /api/routes` and `PATCH /api/routes/{group}/{n}` carry
/// handler source, which is comfortably under that for any realistic
/// handler. 16 MiB is a generous ceiling that keeps the auth-gated
/// JSON surface uniform without being a target.
pub(crate) const MAX_API_BODY_BYTES: usize = 16 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/routes", post(create_route).get(list_routes))
        .route(
            "/api/routes/{group}/{number}",
            get(get_route).patch(patch_route).delete(delete_route),
        )
        .route(
            "/api/routes/{group}/{number}/state",
            get(get_route_state)
                .put(set_route_state)
                .delete(delete_route_state),
        )
        .route("/api/routes/{group}/{number}/source", get(get_route_source))
        .route("/api/routes/{group}/{number}/dry-run", post(dry_run_route))
        .route("/api/tokens", post(create_token).get(list_tokens))
        .route(
            "/api/tokens/{name}",
            get(get_token).delete(delete_token).patch(patch_token),
        )
        // Users — POST/GET (admin); GET /me (any authed); GET/PATCH/DELETE
        // /{name} (admin or self for the GET; admin-only for PATCH/DELETE).
        // /me must come before /{name} or axum's matcher will treat "me"
        // as a user name.
        .route("/api/users", post(create_user).get(list_users))
        .route("/api/users/me", get(get_me))
        .route(
            "/api/users/{name}",
            get(get_user).patch(patch_user).delete(delete_user),
        )
        // Journal — list/get per group (admin or any group-route owner).
        .route("/api/journal/{group}", get(list_journal))
        .route("/api/journal/{group}/{number}", get(get_journal_entry))
        // Journal tail — SSE stream of live events. Auth: owner-or-admin
        // when ?group= is set, admin-only otherwise (matches the
        // host-wide unmatched read).
        .route("/api/journal/tail", get(tail_journal))
        // Match probe — read-only diagnostic. Any authenticated user.
        .route("/api/match", get(match_probe))
        // Unmatched — admin-only (host-wide and may include probing traffic).
        .route("/api/unmatched", get(list_unmatched))
        .route("/api/unmatched/{number}", get(get_unmatched_entry))
        // Groups — owner-or-admin for cross-cutting actions; the
        // sub-endpoints (refresh, state, journal) live under the
        // group's slug. Note: `DELETE /api/groups/{group}/journal`
        // here supersedes the `clear-journal` shape from the rest-api
        // doc — same semantics, lives under the group's path.
        .route("/api/groups", post(create_group).get(list_groups))
        // Static segment — registered before `/api/groups/{group}`.
        .route("/api/groups/import", post(import_group_handler))
        .route(
            "/api/groups/{group}",
            get(get_group).patch(patch_group).delete(delete_group),
        )
        .route("/api/groups/{group}/export", get(export_group_handler))
        .route("/api/groups/{group}/refresh", post(refresh_group))
        .route(
            "/api/groups/{group}/state",
            get(get_group_state)
                .put(set_group_state)
                .delete(delete_group_state),
        )
        .route(
            "/api/groups/{group}/journal",
            axum::routing::delete(delete_group_journal),
        )
        // Outbound-callback journal (ADR-0034): the delivery outcomes of
        // callbacks this group's handlers scheduled. Same owner-or-admin
        // gate as the request journal.
        .route("/api/groups/{group}/callbacks", get(list_callbacks))
        .route(
            "/api/groups/{group}/callbacks/{number}",
            get(get_callback_entry),
        )
        // Handler-API capabilities. Returns the same markdown the MCP
        // tool and `wm capabilities` CLI command surface — single
        // source of truth in `crate::capabilities`. Bearer-token
        // gated like everything else under /api/*.
        .route("/api/capabilities", get(list_capabilities))
        .route("/api/capabilities/{topic}", get(get_capability))
        // Lift axum's 2 MiB default so wasm uploads on POST/PATCH
        // /api/routes aren't artificially cut off. The lifted limit
        // covers every /api/* endpoint uniformly — overkill for the
        // endpoints that take small JSON, but the limit is a ceiling,
        // not a target, and uniform is simpler than per-route.
        .layer(DefaultBodyLimit::max(MAX_API_BODY_BYTES))
}

// -- Request / response shapes ------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRouteBody {
    pub(crate) group: Option<String>,
    pub(crate) methods: Vec<String>,
    pub(crate) path: String,
    pub(crate) language: String,
    /// Handler source. Stored verbatim for `javascript`; transpiled
    /// in-process for `typescript`. The only public artifact input
    /// (ADR-0023 — pre-compiled wasm upload was retired).
    pub(crate) source: Option<String>,
}

/// Partial-update payload for `PATCH /api/routes/{group}/{n}`. Every
/// field is optional; `language` is required when replacing the
/// artifact (i.e. when `source` is present).
#[derive(Debug, Deserialize)]
pub(crate) struct PatchRouteBody {
    pub(crate) methods: Option<Vec<String>>,
    pub(crate) path: Option<String>,
    pub(crate) language: Option<String>,
    /// Replacement handler source (`javascript` | `typescript`).
    pub(crate) source: Option<String>,
}

#[derive(Debug, Serialize)]
struct RouteResponse {
    id: String,
    number: u32,
    group: GroupRef,
    methods: Vec<String>,
    path: String,
    /// Full public URL the SUT calls: `{scheme}://{group}.{apex}{path}`
    /// (ADR-0030 virtual-host routing). The path component is the route's
    /// pattern verbatim, `{param}` segments and all.
    url: String,
    language: String,
    bindings_version: String,
    created_at: String,
    owner_id: String,
    hits_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_hit_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct GroupRef {
    id: String,
    name: String,
}

impl RouteResponse {
    /// Build a response, computing the per-group `url` from the request
    /// headers (the apex these control-plane surfaces are served on).
    fn build(r: &Route, headers: &HeaderMap, trust_forwarded: bool) -> Self {
        let url = format!(
            "{}{}",
            crate::auth_api::group_base_url(&r.group_name, headers, trust_forwarded),
            r.path
        );
        Self {
            id: r.id.clone(),
            number: r.number,
            group: GroupRef {
                id: r.group_id.clone(),
                name: r.group_name.clone(),
            },
            methods: r.methods.clone(),
            path: r.path.clone(),
            url,
            language: r.language.clone(),
            bindings_version: r.bindings_version.clone(),
            created_at: r.created_at.to_rfc3339(),
            owner_id: r.owner_id.clone(),
            hits_total: r.hits_total,
            last_hit_at: r.last_hit_at.map(|ts| ts.to_rfc3339()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ListRoutesResponse {
    routes: Vec<RouteResponse>,
    /// Total matches after filters, before pagination. Lets the UI
    /// render "1–20 of 137" without re-asking.
    total: u64,
    /// Pass back as `?offset=` to fetch the next page; absent when
    /// the returned page reached the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<u64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub(crate) struct RoutesListQuery {
    /// Group name or ULID. Filters to routes in that group only.
    pub(crate) group: Option<String>,
    /// Restrict to routes owned by `owner_id`. Admin-only — non-admin
    /// callers may not impersonate-list. Non-admin callers always see
    /// their own routes only and may not pass this parameter.
    pub(crate) owner_id: Option<String>,
    /// HTTP method filter (uppercase, e.g. `GET`, or `ANY`).
    pub(crate) method: Option<String>,
    /// `*`-glob over the route's defined `path` (e.g. `/v1/*`).
    pub(crate) path_pattern: Option<String>,
    /// Lower bound on `last_hit_at`. Duration suffix (`5m`, `1h`,
    /// `2d`, `30s`) or RFC 3339 timestamp. Routes that have never
    /// been hit are excluded.
    pub(crate) since: Option<String>,
    /// Upper bound on `last_hit_at`.
    pub(crate) until: Option<String>,
    /// Free-text needle. Substring-matched (case-insensitive) against
    /// the route's path and methods.
    pub(crate) q: Option<String>,
    /// Sort column: `created_at` (default), `last_hit_at`, `hits_total`.
    pub(crate) sort: Option<String>,
    /// Sort direction: `asc` or `desc`. Default `desc`.
    pub(crate) dir: Option<String>,
    pub(crate) offset: Option<u64>,
    pub(crate) limit: Option<u64>,
}

/// Default + max for `?limit=` on the list endpoints. Kept here
/// rather than in `api_filters` because the cap is policy-level,
/// not parser-level.
const DEFAULT_LIST_LIMIT: u64 = 50;
const MAX_LIST_LIMIT: u64 = 200;

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<String>,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    diagnostics: Vec<String>,
}

impl ApiError {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    fn validation(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "validation_failed",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }

    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "no such route".into(),
            diagnostics: Vec::new(),
        }
    }

    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }

    fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }

    fn compile_failed(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "compile_failed",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: msg.into(),
            diagnostics: Vec::new(),
        }
    }
}

impl From<FilterParseError> for ApiError {
    fn from(err: FilterParseError) -> Self {
        let status = if matches!(err, FilterParseError::OwnerNonAdmin) {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::BAD_REQUEST
        };
        let code = if matches!(err, FilterParseError::OwnerNonAdmin) {
            "forbidden"
        } else {
            "validation_failed"
        };
        let parameter = err.parameter();
        Self {
            status,
            code,
            message: err.to_string(),
            // Surface the offending parameter name as a diagnostic so
            // clients (CLI, web UI) can pinpoint the bad field without
            // string-parsing the message.
            diagnostics: vec![format!("parameter={parameter}")],
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.into(),
                message: self.message,
                diagnostics: self.diagnostics,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

// Auth extractor. Tries `Authorization: Bearer wmt_...` first, falls
// back to a `wm_session` cookie when no bearer token is present.
// Returns 401 when both are missing/invalid.
//
// Two paths share one return type: either path produces an
// `AuthContext` with `credential_kind` set so handlers can branch
// when they need to.
impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(header_value) = parts.headers.get(header::AUTHORIZATION) {
            let raw = header_value
                .to_str()
                .map_err(|_| ApiError::unauthorized("Authorization header is not valid ASCII"))?;
            let token = raw
                .strip_prefix("Bearer ")
                .ok_or_else(|| ApiError::unauthorized("expected `Bearer wmt_...` scheme"))?
                .trim();
            return state
                .auth()
                .authenticate(token)
                .map_err(|e| ApiError::internal(format!("auth lookup: {e}")))?
                .ok_or_else(|| ApiError::unauthorized("invalid or expired token"));
        }
        // No bearer token — try the session cookie. Skip cleanly when
        // no session store is configured (operator hasn't set
        // SESSION_SECRET) — that surfaces as the same 401 as no auth.
        if let Some(sessions) = state.sessions()
            && let Some(cookie) = extract_session_cookie(parts)
        {
            let session = sessions
                .touch(&cookie)
                .map_err(|_| ApiError::unauthorized("invalid or expired session"))?;
            let user = state
                .auth()
                .get_user_by_id(&session.user_id)
                .map_err(|_| ApiError::unauthorized("session refers to a deleted user"))?;
            return Ok(AuthContext {
                user_id: user.id,
                user_name: user.name,
                is_admin: user.is_admin,
                credential_kind: crate::auth::CredentialKind::Session,
                credential_id: session.id,
            });
        }
        Err(ApiError::unauthorized(
            "missing Authorization header or session cookie",
        ))
    }
}

/// Pull the `wm_session` cookie value from the `Cookie` header.
/// Returns `None` when the header is missing, isn't valid UTF-8, or
/// doesn't contain a `wm_session=...` pair. Multiple `Cookie`
/// headers (which axum exposes via `get_all`) are walked in order;
/// the first hit wins.
fn extract_session_cookie(parts: &Parts) -> Option<String> {
    for header_value in parts.headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = header_value.to_str() else {
            continue;
        };
        // `Cookie: a=1; b=2; wm_session=token.sig` — pairs are
        // semicolon-separated, each pair `key=value`.
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix(&format!("{}=", crate::session::COOKIE_NAME)) {
                return Some(value.to_string());
            }
        }
    }
    None
}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::NotFound => ApiError::not_found(),
            AuthError::NameTaken(name) => {
                ApiError::conflict(format!("name {name:?} is already in use"))
            }
            AuthError::Storage(e) => ApiError::internal(format!("storage: {e}")),
            AuthError::Malformed(msg) => ApiError::internal(format!("malformed record: {msg}")),
        }
    }
}

impl From<JournalError> for ApiError {
    fn from(err: JournalError) -> Self {
        match err {
            JournalError::NotFound => ApiError::not_found(),
            JournalError::Storage(e) => ApiError::internal(format!("storage: {e}")),
            JournalError::Malformed(msg) => {
                ApiError::internal(format!("malformed journal record: {msg}"))
            }
        }
    }
}

impl From<RegistryError> for ApiError {
    fn from(err: RegistryError) -> Self {
        match err {
            RegistryError::NotFound => ApiError::not_found(),
            RegistryError::Conflict(msg) => ApiError::conflict(msg),
            RegistryError::InvalidPath(e) => ApiError::validation(format!("invalid path: {e}")),
            RegistryError::InvalidMethod(m) => ApiError::validation(format!("invalid method: {m}")),
            RegistryError::InvalidName(n) => {
                ApiError::validation(format!("invalid group name: {n}"))
            }
            RegistryError::Storage(e) => ApiError::internal(format!("storage: {e}")),
            RegistryError::Malformed(msg) => ApiError::internal(format!("malformed record: {msg}")),
        }
    }
}

// -- Handlers -----------------------------------------------------------------

async fn create_route(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Json(body): Json<CreateRouteBody>,
) -> Result<Response, ApiError> {
    let route = create_route_core(&state, &auth, body).await?;
    let location = format!(
        "/api/routes/{}",
        render_slug(&route.group_name, route.number)
    );
    let resp_body = RouteResponse::build(&route, &headers, state.trust_forwarded_headers());
    let mut resp = (StatusCode::CREATED, Json(resp_body)).into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        HeaderValue::try_from(location).expect("ascii location"),
    );
    Ok(resp)
}

/// Shared validate + compile + register pipeline behind both
/// `POST /api/routes` and the UI's `POST /ui/routes/new`. Lives
/// in `api.rs` so the validation rules stay in one place.
pub(crate) async fn create_route_core(
    state: &AppState,
    auth: &AuthContext,
    body: CreateRouteBody,
) -> Result<crate::registry::Route, ApiError> {
    // ADR-0033: no reserved paths — a route lives on its group's subdomain,
    // which is pure mock space, so any path (incl. /health, /api/*) is fair game.

    // Tenancy gate (ADR-0030): a route may only be added to an *existing*
    // group you own (or as admin). The group is the tenancy boundary, so
    // without this any authed user could inject a route into another tenant's
    // group — it would serve on that group's subdomain, stay invisible to the
    // owner (route lists are owner-scoped), and grant the injector read access
    // to the group's whole journal (journal read is "admin or owns a route in
    // the group"). Creating a brand-new named group or an implicit one (group
    // omitted) stays open to everyone.
    //
    // A group you don't own returns the SAME `not_found` as a missing one
    // (per rest-api.md `POST /api/routes`): group names are a global
    // namespace, so a distinct 403 would leak that a name is taken by another
    // tenant. (This is stricter than the route PATCH/DELETE gates, which 403
    // on a slug you already had to know.)
    if let Some(reference) = body.group.as_deref() {
        match state.routes().registry().read_group_by_ref(reference) {
            Ok(group) if group.owner_id == auth.user_id || auth.is_admin => {}
            // Not yours, or doesn't exist → indistinguishable not_found.
            _ => return Err(ApiError::not_found()),
        }
    }

    // Cheap conflict precheck so idempotent retries don't burn an
    // swc transpile per attempt — the same scan runs again inside
    // `registry.create_route()` after we have an artifact, but
    // surfacing it here means a re-seed of an existing slug fails
    // before we do the parser work.
    state.routes().registry().precheck_create_conflict(
        body.group.as_deref(),
        &body.methods,
        &body.path,
    )?;

    let (compiled_wasm, language, bindings_version, source) = match body.language.as_str() {
        "wasm" => {
            // ADR-0023: pre-compiled wasm upload was removed from the
            // public surface. Routes still execute as wasm internally,
            // but the only public artifact input is source.
            return Err(ApiError::validation(
                "pre-compiled wasm upload is no longer supported; send `source` \
                 with `language: \"typescript\"` or `\"javascript\"`",
            ));
        }
        "javascript" => {
            // ADR-0020 shared-engine path: source is stored verbatim
            // on the Route record, no per-route componentize. The
            // dispatcher branches on `language: "javascript"` to
            // route the call through the shared `js-engine.wasm`
            // component at request time.
            let source = body.source.ok_or_else(|| {
                ApiError::validation("source required when language=\"javascript\"")
            })?;
            (
                Vec::new(),
                "javascript".to_string(),
                SUPPORTED_BINDINGS_VERSION.to_string(),
                Some(source),
            )
        }
        "typescript" => {
            // ADR-0020 slice B: pure-Rust swc transpiles TS → JS in
            // the host before storage. The route's `language` is
            // preserved as "typescript" for operator visibility, but
            // dispatch resolves it as engine-language via
            // `dispatches_via_engine`.
            let source = body.source.ok_or_else(|| {
                ApiError::validation("source required when language=\"typescript\"")
            })?;
            let ts_source = source.clone();
            // Transpile now to surface compile errors at create time, but
            // store the *original* TS — the engine-runnable JS is derived and
            // cached at dispatch (ADR-0020; preserves the authored source so
            // show_route_source / export return what the author wrote).
            tokio::task::spawn_blocking(move || crate::ts_transpile::transpile(&ts_source))
                .await
                .map_err(|e| ApiError::compile_failed(format!("transpile task: {e}")))?
                .map_err(ApiError::compile_failed)?;
            (
                Vec::new(),
                "typescript".to_string(),
                SUPPORTED_BINDINGS_VERSION.to_string(),
                Some(source),
            )
        }
        other => {
            return Err(ApiError::validation(format!(
                "unsupported language {other:?}; expected \"wasm\", \"javascript\", or \"typescript\""
            )));
        }
    };

    let route = state.routes().registry().create_route(NewRoute {
        group: body.group,
        methods: body.methods,
        path: body.path,
        language,
        bindings_version,
        compiled_wasm,
        source,
        owner_id: auth.user_id.clone(),
    })?;

    state.routes().refresh_after_create(route.clone());
    Ok(route)
}

async fn list_routes(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Query(q): Query<RoutesListQuery>,
) -> Result<Json<ListRoutesResponse>, ApiError> {
    let paged = list_routes_core(&state, &auth, &q)?;
    let trust = state.trust_forwarded_headers();
    let routes = paged
        .routes
        .iter()
        .map(|r| RouteResponse::build(r, &headers, trust))
        .collect();
    Ok(Json(ListRoutesResponse {
        routes,
        total: paged.total,
        next_offset: paged.next_offset,
    }))
}

/// One paginated page of routes plus the totals the caller needs to
/// render "K of N" + a next-page link. Used by both the REST handler
/// above and the `/ui/routes` page handler.
pub(crate) struct PagedRoutes {
    pub routes: Vec<Route>,
    pub total: u64,
    pub next_offset: Option<u64>,
}

/// Shared filter / sort / paginate path for route listings. Holds the
/// non-admin owner-scoping rule (refuse `owner_id`, always restrict
/// to self) so the UI can't accidentally let a non-admin see other
/// users' routes by handing it a different query string.
pub(crate) fn list_routes_core(
    state: &AppState,
    auth: &AuthContext,
    q: &RoutesListQuery,
) -> Result<PagedRoutes, ApiError> {
    let now = chrono::Utc::now();
    let (offset, limit) = parse_pagination(q.offset, q.limit)?;
    let dir = SortDir::parse(q.dir.as_deref(), SortDir::Desc)?;
    let sort_key = parse_route_sort(q.sort.as_deref())?;
    let method = q.method.as_deref().map(validate_method).transpose()?;
    let since = q
        .since
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;
    let until = q
        .until
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;

    let owner_filter: Option<String> = if auth.is_admin {
        q.owner_id.clone()
    } else {
        if q.owner_id.is_some() {
            return Err(FilterParseError::OwnerNonAdmin.into());
        }
        Some(auth.user_id.clone())
    };

    // Read from the registry (source of truth) rather than the
    // RouteTable's cached snapshot. The snapshot only refreshes on
    // create/delete/update/cascade — `record_route_hit` updates the
    // registry directly, so listing off the snapshot reports stale
    // `hits_total: 0` / `last_hit_at: None` indefinitely. Fast path
    // dispatch still uses the snapshot; listings are not hot.
    let all = state.routes().registry().list_routes()?;
    let mut filtered: Vec<Route> = all
        .into_iter()
        .filter(|r| match owner_filter.as_deref() {
            Some(owner) => r.owner_id == owner,
            None => true,
        })
        .filter(|r| match q.group.as_deref() {
            Some(g) => r.group_name == g || r.group_id == g,
            None => true,
        })
        .filter(|r| match method.as_deref() {
            Some(m) => r.methods.iter().any(|rm| rm == m),
            None => true,
        })
        .filter(|r| match q.path_pattern.as_deref() {
            Some(p) => glob_match(p, &r.path),
            None => true,
        })
        .filter(|r| route_matches_since_until(r, since, until))
        .filter(|r| match q.q.as_deref() {
            Some(needle) => route_matches_q(r, needle),
            None => true,
        })
        .collect();

    sort_routes(&mut filtered, sort_key, dir);

    let total = filtered.len() as u64;
    let start = offset.min(total) as usize;
    let end = (start as u64 + limit).min(total) as usize;
    let next_offset = if (end as u64) < total {
        Some(end as u64)
    } else {
        None
    };
    Ok(PagedRoutes {
        routes: filtered[start..end].to_vec(),
        total,
        next_offset,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteSortKey {
    CreatedAt,
    LastHitAt,
    HitsTotal,
}

pub(crate) fn parse_route_sort(raw: Option<&str>) -> Result<RouteSortKey, FilterParseError> {
    match raw {
        None | Some("created_at") => Ok(RouteSortKey::CreatedAt),
        Some("last_hit_at") => Ok(RouteSortKey::LastHitAt),
        Some("hits_total") => Ok(RouteSortKey::HitsTotal),
        Some(other) => Err(FilterParseError::BadSort(other.to_string())),
    }
}

pub(crate) fn sort_routes(routes: &mut [Route], key: RouteSortKey, dir: SortDir) {
    routes.sort_by(|a, b| {
        let ord = match key {
            RouteSortKey::CreatedAt => a.created_at.cmp(&b.created_at),
            // None sorts last in desc, first in asc — i.e. never-hit
            // routes always appear at the bottom of an activity sort.
            // That matches what a user expects: "show me the busy ones."
            RouteSortKey::LastHitAt => match (a.last_hit_at, b.last_hit_at) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => std::cmp::Ordering::Equal,
            },
            RouteSortKey::HitsTotal => a.hits_total.cmp(&b.hits_total),
        };
        match dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    });
}

pub(crate) fn route_matches_since_until(
    r: &Route,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    let Some(ts) = r.last_hit_at else {
        // Never-hit routes have no activity timestamp to compare. A
        // bounded window inherently excludes them.
        return false;
    };
    if let Some(s) = since
        && ts < s
    {
        return false;
    }
    if let Some(u) = until
        && ts > u
    {
        return false;
    }
    true
}

pub(crate) fn route_matches_q(r: &Route, needle: &str) -> bool {
    let n = needle.to_ascii_lowercase();
    r.path.to_ascii_lowercase().contains(&n)
        || r.methods
            .iter()
            .any(|m| m.to_ascii_lowercase().contains(&n))
}

pub(crate) fn parse_pagination(
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<(u64, u64), FilterParseError> {
    let off = offset.unwrap_or(0);
    let raw_limit = limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if raw_limit == 0 {
        return Err(FilterParseError::BadLimit("0".into()));
    }
    let lim = raw_limit.min(MAX_LIST_LIMIT);
    Ok((off, lim))
}

/// `crate::mcp` reuses the offset slicing.
pub(crate) fn slice_for_page<T: Clone>(
    items: &[T],
    offset: u64,
    limit: u64,
) -> (Vec<T>, u64, Option<u64>) {
    let total = items.len() as u64;
    let start = offset.min(total) as usize;
    let end = (start as u64 + limit).min(total) as usize;
    let next_offset = if (end as u64) < total {
        Some(end as u64)
    } else {
        None
    };
    (items[start..end].to_vec(), total, next_offset)
}

async fn get_route(
    State(state): State<AppState>,
    _auth: AuthContext,
    headers: HeaderMap,
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<RouteResponse>, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    Ok(Json(RouteResponse::build(
        &route,
        &headers,
        state.trust_forwarded_headers(),
    )))
}

async fn patch_route(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Path((group, number)): Path<(String, u32)>,
    Json(body): Json<PatchRouteBody>,
) -> Result<Json<RouteResponse>, ApiError> {
    let updated = patch_route_core(&state, &auth, &group, number, body).await?;
    Ok(Json(RouteResponse::build(
        &updated,
        &headers,
        state.trust_forwarded_headers(),
    )))
}

/// Shared PATCH pipeline used by both the REST handler above and the
/// `/ui/routes/{g}/{n}/source/edit` page handler. Returns the updated
/// `Route` so callers can decide their own response shape (JSON for
/// REST, redirect for UI).
pub(crate) async fn patch_route_core(
    state: &AppState,
    auth: &AuthContext,
    group: &str,
    number: u32,
    body: PatchRouteBody,
) -> Result<crate::registry::Route, ApiError> {
    // Existence + ownership gate. Mirrors the DELETE handler — owner-
    // or-admin per ADR-0014.
    let existing = state.routes().registry().get_route_by_slug(group, number)?;
    if existing.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may update it",
        ));
    }

    let any_field = body.methods.is_some()
        || body.path.is_some()
        || body.language.is_some()
        || body.source.is_some();
    if !any_field {
        return Err(ApiError::validation(
            "PATCH body must include at least one mutable field \
             (`methods`, `path`, `source`)",
        ));
    }
    // Compute the artifact triple (language, bindings_version,
    // compiled_wasm) when the body changes the source. When `source`
    // is absent the existing artifact is preserved; `language` is
    // ignored in that case to keep the metadata in sync with what's
    // actually loaded.
    let artifact_changing = body.source.is_some();
    // `source_patch` mirrors `PatchRoute::source`: outer `Option` is
    // "field present in patch?", inner is the actual value to store.
    // A source-lang swap stores the new source (Some(Some(_))); no
    // artifact change leaves source alone (None).
    let (compiled_wasm, language, bindings_version, source_patch) = if artifact_changing {
        let lang = body.language.as_deref().ok_or_else(|| {
            ApiError::validation("`language` is required when changing the route's source")
        })?;
        match lang {
            "wasm" => {
                // ADR-0023: pre-compiled wasm upload is no longer a
                // public artifact input.
                return Err(ApiError::validation(
                    "pre-compiled wasm upload is no longer supported; send `source` \
                     with `language: \"typescript\"` or `\"javascript\"`",
                ));
            }
            "javascript" => {
                // ADR-0020 shared-engine swap: store source, drop
                // any prior componentized wasm. The dispatcher
                // picks up the language change on the next request.
                let source = body.source.as_deref().ok_or_else(|| {
                    ApiError::validation("source required when language=\"javascript\"")
                })?;
                (
                    Some(Vec::new()),
                    Some("javascript".to_string()),
                    Some(SUPPORTED_BINDINGS_VERSION.to_string()),
                    Some(Some(source.to_string())),
                )
            }
            "typescript" => {
                let source = body.source.as_deref().ok_or_else(|| {
                    ApiError::validation("source required when language=\"typescript\"")
                })?;
                let ts_source = source.to_string();
                // Validate (compile_failed) but store the original TS; the JS
                // is derived + cached at dispatch (ADR-0020).
                tokio::task::spawn_blocking(move || crate::ts_transpile::transpile(&ts_source))
                    .await
                    .map_err(|e| ApiError::compile_failed(format!("transpile task: {e}")))?
                    .map_err(ApiError::compile_failed)?;
                (
                    Some(Vec::new()),
                    Some("typescript".to_string()),
                    Some(SUPPORTED_BINDINGS_VERSION.to_string()),
                    Some(Some(source.to_string())),
                )
            }
            other => {
                return Err(ApiError::validation(format!(
                    "unsupported language {other:?}; expected \"wasm\", \"javascript\", or \"typescript\""
                )));
            }
        }
    } else {
        (None, None, None, None)
    };

    let updated = state.routes().registry().update_route(
        group,
        number,
        PatchRoute {
            methods: body.methods,
            path: body.path,
            language,
            bindings_version,
            compiled_wasm,
            source: source_patch,
        },
    )?;
    state.routes().refresh_after_update(updated.clone());
    Ok(updated)
}

async fn delete_route(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<StatusCode, ApiError> {
    // Look up first so we can (a) check ownership and (b) invalidate the
    // route table after deletion. Per ADR-0014: a route is deletable by
    // its creator, plus by any admin.
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may delete it",
        ));
    }
    state.routes().registry().delete_route(&group, number)?;
    state.routes().refresh_after_delete(&route.id);
    Ok(StatusCode::NO_CONTENT)
}

// -- Per-route state ----------------------------------------------------------

#[derive(Debug, Serialize)]
struct ListRouteStateResponse {
    entries: Vec<RouteStateEntry>,
}

/// Round-trippable state snapshot (ADR-0025): bytes entries only, as the
/// same `string | {base64}` values `PUT .../state` accepts. Collection
/// entries (list/hash/set) are omitted — they can't round-trip through
/// the bytes-only write surface.
#[derive(Debug, Serialize)]
struct StateSnapshotResponse {
    entries: std::collections::HashMap<String, WireBytes>,
}

/// Write payload for `PUT .../state` (ADR-0025).
#[derive(Debug, Deserialize)]
struct SetStateBody {
    entries: std::collections::HashMap<String, WireBytes>,
}

#[derive(Debug, Default, Deserialize)]
struct StateQuery {
    #[serde(default)]
    format: Option<String>,
}

/// Decode `string | {base64}` entries to bytes, enforcing the per-key
/// size cap (shared with the MCP write path via `crate::state`).
fn decode_state_entries(
    entries: std::collections::HashMap<String, WireBytes>,
) -> Result<std::collections::HashMap<String, Vec<u8>>, ApiError> {
    crate::wire::decode_entries(entries).map_err(ApiError::validation)
}

/// Render a state listing as the preview shape (default) or, for
/// `?format=snapshot`, the round-trippable bytes-only snapshot.
fn render_state(entries: Vec<RouteStateEntry>, q: &StateQuery) -> Response {
    if q.format.as_deref() == Some("snapshot") {
        let entries = entries
            .into_iter()
            .filter(|e| e.kind == "bytes")
            .filter_map(|e| e.value.map(|v| (e.key, WireBytes::from_bytes(&v))))
            .collect();
        Json(StateSnapshotResponse { entries }).into_response()
    } else {
        Json(ListRouteStateResponse { entries }).into_response()
    }
}

async fn get_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
    Query(q): Query<StateQuery>,
) -> Result<Response, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may read its state",
        ));
    }
    let entries = state.routes().registry().list_route_state(&group, number)?;
    Ok(render_state(entries, &q))
}

async fn set_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
    Json(body): Json<SetStateBody>,
) -> Result<StatusCode, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may write its state",
        ));
    }
    let entries = decode_state_entries(body.entries)?;
    state
        .routes()
        .registry()
        .set_route_state(&group, number, entries)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<StatusCode, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may clear its state",
        ));
    }
    state
        .routes()
        .registry()
        .clear_route_state(&group, number)?;
    Ok(StatusCode::NO_CONTENT)
}

// -- Source --------------------------------------------------------------------

/// Response shape for `GET /api/routes/{group}/{n}/source`. The
/// `source` field carries the original handler source the caller sent;
/// `None` for pre-compiled `wasm` uploads (no source ever existed) and
/// for records that pre-date slice 36. The slug is included so the
/// agent/UI rendering the result doesn't have to re-thread it from the
/// request URL.
#[derive(Debug, Serialize)]
struct RouteSourceResponse {
    slug: String,
    language: String,
    source: Option<String>,
}

async fn get_route_source(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<RouteSourceResponse>, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may read its source",
        ));
    }
    Ok(Json(RouteSourceResponse {
        slug: render_slug(&route.group_name, route.number),
        language: route.language,
        source: route.source,
    }))
}

// -- Dry-run ------------------------------------------------------------------

async fn dry_run_route(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
    Json(body): Json<crate::dry_run::DryRunRequest>,
) -> Result<Json<crate::dry_run::DryRunResponse>, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if route.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may dry-run it",
        ));
    }
    if !body.path.starts_with('/') {
        return Err(ApiError::validation("dry-run path must start with /"));
    }
    let resp =
        crate::dry_run::dry_run(state.runtime().clone(), state.routes().clone(), route, body)
            .await
            .map_err(|e| ApiError::internal(format!("dry-run: {e}")))?;
    Ok(Json(resp))
}

// -- /api/tokens ------------------------------------------------------------
//
// Slice-5 scope: a caller manages their own tokens. Admin overrides
// (revoking another user's token, listing on behalf of an owner, PATCH
// rename) land in a follow-up.

#[derive(Debug, Deserialize)]
struct CreateTokenBody {
    name: String,
    /// Optional time-to-live in seconds. `None` means the token doesn't
    /// expire (see ADR-0012).
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
struct TokenRecord {
    id: String,
    name: String,
    owner_id: String,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
    scopes: Vec<String>,
}

impl From<&Token> for TokenRecord {
    fn from(t: &Token) -> Self {
        Self {
            id: t.id.clone(),
            name: t.name.clone(),
            owner_id: t.owner_id.clone(),
            created_at: t.created_at.to_rfc3339(),
            expires_at: t.expires_at.map(|ts| ts.to_rfc3339()),
            last_used_at: t.last_used_at.map(|ts| ts.to_rfc3339()),
            scopes: t.scopes.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct CreateTokenResponse {
    /// Plaintext token. Only present in the create response — never
    /// retrievable later. Treat it like a credential.
    token: String,
    record: TokenRecord,
}

#[derive(Debug, Serialize)]
struct ListTokensResponse {
    tokens: Vec<TokenRecord>,
}

async fn create_token(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<CreateTokenBody>,
) -> Result<Response, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::validation("token name must not be empty"));
    }
    let (token, plaintext) =
        state
            .auth()
            .create_token(&auth.user_id, &body.name, body.ttl_seconds)?;
    let resp = CreateTokenResponse {
        token: plaintext,
        record: TokenRecord::from(&token),
    };
    Ok((StatusCode::CREATED, Json(resp)).into_response())
}

async fn list_tokens(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<ListTokensResponse>, ApiError> {
    let tokens = state.auth().list_tokens_for(&auth.user_id)?;
    let records = tokens.iter().map(TokenRecord::from).collect();
    Ok(Json(ListTokensResponse { tokens: records }))
}

async fn get_token(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<Json<TokenRecord>, ApiError> {
    let token = state
        .auth()
        .get_token_by_name(&auth.user_id, &name)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(TokenRecord::from(&token)))
}

async fn delete_token(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let revoked = state.auth().revoke_token_by_name(&auth.user_id, &name)?;
    if !revoked {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct PatchTokenBody {
    /// New token name (required, non-empty).
    name: String,
}

async fn patch_token(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(old_name): Path<String>,
    Json(body): Json<PatchTokenBody>,
) -> Result<Json<TokenRecord>, ApiError> {
    let new_name = body.name.trim();
    if new_name.is_empty() {
        return Err(ApiError::validation("token name must not be empty"));
    }
    let token = state
        .auth()
        .rename_token(&auth.user_id, &old_name, new_name)?;
    Ok(Json(TokenRecord::from(&token)))
}

// -- /api/users -------------------------------------------------------------
//
// Admin-only for cross-user actions. `GET /api/users/me` and
// `GET /api/users/{name}` (when `name` is the caller's own name) are
// open to any authed user. PATCH is admin-only and only toggles
// `is_admin` for now — rename is too close to the deferred user-merge
// operation, see the auth design notes.
//
// Three guardrails on DELETE / PATCH:
//   1. An admin cannot delete themselves (avoids accidental lockout).
//   2. The system cannot drop below one admin (refuse last-admin
//      DELETE or PATCH-to-non-admin).
//   3. A user that owns routes cannot be deleted; admins clean up the
//      routes first.

#[derive(Debug, Deserialize)]
struct CreateUserBody {
    name: String,
    #[serde(default)]
    is_admin: bool,
}

#[derive(Debug, Deserialize)]
struct PatchUserBody {
    is_admin: Option<bool>,
}

#[derive(Debug, Serialize)]
struct UserRecord {
    id: String,
    name: String,
    is_admin: bool,
    created_at: String,
}

impl From<&User> for UserRecord {
    fn from(u: &User) -> Self {
        Self {
            id: u.id.clone(),
            name: u.name.clone(),
            is_admin: u.is_admin,
            created_at: u.created_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ListUsersResponse {
    users: Vec<UserRecord>,
}

fn require_admin(auth: &AuthContext) -> Result<(), ApiError> {
    if auth.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden("admin role required"))
    }
}

async fn create_user(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<CreateUserBody>,
) -> Result<Response, ApiError> {
    require_admin(&auth)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::validation("user name must not be empty"));
    }
    let user = state.auth().create_user(&body.name, body.is_admin)?;
    Ok((StatusCode::CREATED, Json(UserRecord::from(&user))).into_response())
}

async fn list_users(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<ListUsersResponse>, ApiError> {
    require_admin(&auth)?;
    let users = state.auth().list_users()?;
    let records = users.iter().map(UserRecord::from).collect();
    Ok(Json(ListUsersResponse { users: records }))
}

async fn get_me(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Json<UserRecord>, ApiError> {
    let user = state.auth().get_user_by_id(&auth.user_id)?;
    Ok(Json(UserRecord::from(&user)))
}

async fn get_user(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<Json<UserRecord>, ApiError> {
    let user = state
        .auth()
        .get_user_by_name(&name)?
        .ok_or_else(ApiError::not_found)?;
    if !auth.is_admin && user.id != auth.user_id {
        return Err(ApiError::forbidden(
            "only an admin or the user themselves may view this record",
        ));
    }
    Ok(Json(UserRecord::from(&user)))
}

async fn patch_user(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
    Json(body): Json<PatchUserBody>,
) -> Result<Json<UserRecord>, ApiError> {
    require_admin(&auth)?;
    let user = state
        .auth()
        .get_user_by_name(&name)?
        .ok_or_else(ApiError::not_found)?;
    let Some(target_admin) = body.is_admin else {
        // Nothing to patch right now — only `is_admin` is supported. A
        // body with no recognised fields is a 400 so callers don't
        // believe a no-op succeeded.
        return Err(ApiError::validation(
            "PATCH body must include `is_admin` (the only mutable field today)",
        ));
    };
    if !target_admin && user.is_admin && state.auth().count_admins()? <= 1 {
        return Err(ApiError::forbidden(
            "cannot demote the last admin; promote another user first",
        ));
    }
    let updated = state.auth().set_user_admin(&user.id, target_admin)?;
    Ok(Json(UserRecord::from(&updated)))
}

async fn delete_user(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&auth)?;
    let user = state
        .auth()
        .get_user_by_name(&name)?
        .ok_or_else(ApiError::not_found)?;
    if user.id == auth.user_id {
        return Err(ApiError::forbidden(
            "an admin cannot delete themselves; ask another admin",
        ));
    }
    if user.is_admin && state.auth().count_admins()? <= 1 {
        return Err(ApiError::forbidden(
            "cannot delete the last admin; promote another user first",
        ));
    }
    let owned = state.routes().registry().list_routes_by_owner(&user.id)?;
    if !owned.is_empty() {
        return Err(ApiError::conflict(format!(
            "user owns {} route(s); delete them before deleting the user",
            owned.len()
        )));
    }
    state.auth().delete_user(&user.id)?;
    Ok(StatusCode::NO_CONTENT)
}

// -- /api/journal -----------------------------------------------------------
//
// Per-request audit trail for mock traffic. The journal is the
// agent-debugging surface ("did the SUT call my mock, and what
// happened?") and is kept distinct from OTel observability (which is
// the SRE/ops surface). See ADR-0017 for the split.
//
// Authorization: a caller may read a group's journal if they're admin
// OR if they own at least one route in that group. The unmatched-log
// is admin-only because it's host-wide and may include
// probing/scanning traffic from the network.

#[derive(Debug, Deserialize, Default)]
struct JournalListQuery {
    /// Return entries strictly older than this journal number. `None`
    /// (i.e., not supplied) starts from the newest.
    before: Option<u32>,
    /// Cap on entries returned. The journal clamps to its own max
    /// (currently 100). `None` defaults to that cap.
    limit: Option<usize>,
    /// Route slug `{group}/{n}` — restrict to a single route within
    /// the path-scoped group.
    route: Option<String>,
    method: Option<String>,
    /// `*`-glob over the entry's `matched_pattern`.
    path_pattern: Option<String>,
    /// `2xx` / `3xx` / `4xx` / `5xx` or a specific code like `503`.
    status: Option<String>,
    /// Lower bound on `created_at`. Duration suffix or RFC 3339.
    since: Option<String>,
    until: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UnmatchedListQuery {
    before: Option<u64>,
    limit: Option<usize>,
    method: Option<String>,
    /// `*`-glob over the requested path (no matched route exists).
    path_pattern: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListJournalResponse {
    entries: Vec<JournalRecord>,
    /// Pass back as `?before=` to fetch the next page; absent when the
    /// returned page reached the oldest entry.
    next_before: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ListUnmatchedResponse {
    entries: Vec<UnmatchedRecord>,
    next_before: Option<u64>,
}

/// `true` if the caller is admin or owns at least one route in
/// `group_id`. Non-admin members of a group can read its journal —
/// admin-only would lock out the very users who created the routes.
fn caller_can_read_journal(
    state: &AppState,
    auth: &AuthContext,
    group_id: &str,
) -> Result<bool, ApiError> {
    if auth.is_admin {
        return Ok(true);
    }
    let owned = state
        .routes()
        .registry()
        .list_routes_by_owner(&auth.user_id)?;
    Ok(owned.iter().any(|r| r.group_id == group_id))
}

/// Groups whose unmatched records the caller may see (ADR-0030 SemFLIP).
/// `None` = admin (the cross-group view); `Some(set)` = the group IDs the
/// caller owns. Unmatched visibility is keyed on **group ownership** (not
/// route ownership like the matched journal), so a group with zero routes
/// is still visible to its owner — exactly the case where all of a group's
/// traffic is unmatched.
fn unmatched_visible_groups(
    state: &AppState,
    auth: &AuthContext,
) -> Result<Option<std::collections::HashSet<String>>, ApiError> {
    if auth.is_admin {
        return Ok(None);
    }
    let set = state
        .routes()
        .registry()
        .list_groups_by_owner(&auth.user_id)?
        .into_iter()
        .map(|g| g.id)
        .collect();
    Ok(Some(set))
}

/// Whether the caller may read a single unmatched record attributed to
/// `group_id` — admin, or the group's owner.
fn caller_owns_unmatched(state: &AppState, auth: &AuthContext, group_id: &str) -> bool {
    if auth.is_admin {
        return true;
    }
    matches!(
        state.routes().registry().read_group_by_ref(group_id),
        Ok(g) if g.owner_id == auth.user_id
    )
}

async fn list_journal(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
    Query(q): Query<JournalListQuery>,
) -> Result<Json<ListJournalResponse>, ApiError> {
    // Resolve the group reference (name or ULID) to its ULID; 404 if
    // the group doesn't exist so callers can't probe for groups they
    // don't own without a matching ownership check.
    let group_record = state
        .routes()
        .registry()
        .read_group_by_ref(&group)
        .map_err(|_| ApiError::not_found())?;
    if !caller_can_read_journal(&state, &auth, &group_record.id)? {
        return Err(ApiError::forbidden(
            "must be an admin or own a route in this group to read its journal",
        ));
    }
    let filter = build_journal_filter_from_query(
        Some(&group_record.name),
        q.route.as_deref(),
        q.method.as_deref(),
        q.path_pattern.as_deref(),
        q.status.as_deref(),
        q.since.as_deref(),
        q.until.as_deref(),
    )?;
    let any_filter = filter.route.is_some()
        || filter.method.is_some()
        || filter.path_pattern.is_some()
        || filter.status.is_some()
        || filter.since.is_some()
        || filter.until.is_some();

    let cursor = ListCursor {
        before: q.before,
        limit: q.limit.unwrap_or(100),
    };
    let entries = state.journal().list_for_group(&group_record.id, cursor)?;
    let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
    // Filter the page in memory. Cursor-based pagination is over the
    // unfiltered stream — `next_before` always reflects the oldest
    // raw entry seen so the caller can keep walking even when their
    // filters reject everything on a page.
    let entries = if any_filter {
        entries
            .into_iter()
            .filter(|r| filter.matches_handled(r))
            .collect()
    } else {
        entries
    };
    Ok(Json(ListJournalResponse {
        entries,
        next_before,
    }))
}

/// Build a `JournalFilter` from the REST query parameters. `group`
/// is supplied separately because the journal endpoint is
/// path-scoped — we want the group filter to track the URL, not the
/// `?group=` query parameter.
fn build_journal_filter_from_query(
    group: Option<&str>,
    route: Option<&str>,
    method: Option<&str>,
    path_pattern: Option<&str>,
    status: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<JournalFilter, ApiError> {
    let now = chrono::Utc::now();
    let route = route
        .map(RouteSlug::parse)
        .transpose()
        .map_err(|e| ApiError::validation(format!("invalid `route`: {e}")))?;
    let status = status
        .map(StatusFilter::parse)
        .transpose()
        .map_err(|e| ApiError::validation(format!("invalid `status`: {e}")))?;
    let method = method
        .map(validate_method)
        .transpose()
        .map_err(ApiError::from)?;
    let since = since
        .map(|s| parse_since(s, now))
        .transpose()
        .map_err(ApiError::from)?;
    let until = until
        .map(|s| parse_since(s, now))
        .transpose()
        .map_err(ApiError::from)?;
    Ok(JournalFilter {
        group: group.map(String::from),
        route,
        method,
        path_pattern: path_pattern.map(String::from),
        status,
        since,
        until,
    })
}

async fn get_journal_entry(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<JournalRecord>, ApiError> {
    let group_record = state
        .routes()
        .registry()
        .read_group_by_ref(&group)
        .map_err(|_| ApiError::not_found())?;
    if !caller_can_read_journal(&state, &auth, &group_record.id)? {
        return Err(ApiError::forbidden(
            "must be an admin or own a route in this group to read its journal",
        ));
    }
    let entry = state.journal().get(&group_record.id, number)?;
    Ok(Json(entry))
}

// -- /api/groups/{group}/callbacks (ADR-0034) -------------------------------

#[derive(Debug, Deserialize, Default)]
struct CallbackListQuery {
    /// Return entries strictly older than this callback number.
    before: Option<u32>,
    /// Cap on entries returned (journal clamps to its own max).
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ListCallbacksResponse {
    entries: Vec<CallbackRecord>,
    next_before: Option<u32>,
}

async fn list_callbacks(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
    Query(q): Query<CallbackListQuery>,
) -> Result<Json<ListCallbacksResponse>, ApiError> {
    let group_record = state
        .routes()
        .registry()
        .read_group_by_ref(&group)
        .map_err(|_| ApiError::not_found())?;
    // Same gate as the request journal: admin or owns a route in the group.
    if !caller_can_read_journal(&state, &auth, &group_record.id)? {
        return Err(ApiError::forbidden(
            "must be an admin or own a route in this group to read its callbacks",
        ));
    }
    let cursor = ListCursor {
        before: q.before,
        limit: q.limit.unwrap_or(100),
    };
    let entries = state
        .journal()
        .list_callbacks_for_group(&group_record.id, cursor)?;
    let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
    Ok(Json(ListCallbacksResponse {
        entries,
        next_before,
    }))
}

async fn get_callback_entry(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<CallbackRecord>, ApiError> {
    let group_record = state
        .routes()
        .registry()
        .read_group_by_ref(&group)
        .map_err(|_| ApiError::not_found())?;
    if !caller_can_read_journal(&state, &auth, &group_record.id)? {
        return Err(ApiError::forbidden(
            "must be an admin or own a route in this group to read its callbacks",
        ));
    }
    let entry = state.journal().get_callback(&group_record.id, number)?;
    Ok(Json(entry))
}

async fn list_unmatched(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UnmatchedListQuery>,
) -> Result<Json<ListUnmatchedResponse>, ApiError> {
    // ADR-0030 SemFLIP: any authed caller may list; admin sees every
    // group's unmatched, a tenant sees only their own groups'.
    let visible = unmatched_visible_groups(&state, &auth)?;
    let filter = build_journal_filter_from_query(
        None,
        None,
        q.method.as_deref(),
        q.path_pattern.as_deref(),
        None,
        q.since.as_deref(),
        q.until.as_deref(),
    )?;
    let any_filter = filter.method.is_some()
        || filter.path_pattern.is_some()
        || filter.since.is_some()
        || filter.until.is_some();

    let cursor = UnmatchedCursor {
        before: q.before,
        limit: q.limit.unwrap_or(100),
    };
    let entries = state.journal().list_unmatched(cursor, visible.as_ref())?;
    let next_before = entries.last().filter(|e| e.number > 1).map(|e| e.number);
    let entries = if any_filter {
        entries
            .into_iter()
            .filter(|r| filter.matches_unmatched(r))
            .collect()
    } else {
        entries
    };
    Ok(Json(ListUnmatchedResponse {
        entries,
        next_before,
    }))
}

async fn get_unmatched_entry(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(number): Path<u64>,
) -> Result<Json<UnmatchedRecord>, ApiError> {
    let entry = state.journal().get_unmatched(number)?;
    if !caller_owns_unmatched(&state, &auth, &entry.group_id) {
        return Err(ApiError::forbidden(
            "must be an admin or own this group to read its unmatched requests",
        ));
    }
    Ok(Json(entry))
}

// -- /api/groups ------------------------------------------------------------
//
// Lifecycle endpoints. POST is open to any authed user (they own
// what they create); list filters to owned-by-self for non-admin
// callers. PATCH / DELETE / refresh / state-clear / journal-clear all
// require admin or owner. DELETE cascades silently — group owners are
// expected to manage the lifecycle of everything inside.

#[derive(Debug, Deserialize)]
struct CreateGroupBody {
    /// Optional. Omit (or send empty) to be assigned a friendly DNS-safe
    /// name (ADR-0030); an explicit name must be a valid DNS label.
    #[serde(default)]
    name: Option<String>,
    /// Optional configured TTL. Omit to take the default; values are
    /// validated against `MAX_GROUP_TTL_SECONDS`.
    ttl_seconds: Option<u64>,
    /// Optional sliding-TTL flag. Omit to take the default (`true`).
    sliding_ttl: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PatchGroupBody {
    /// Rename the group to this name. Must be a valid DNS label (it's the
    /// group's subdomain; ADR-0030). Changes the group's served base URL.
    #[serde(default)]
    name: Option<String>,
    ttl_seconds: Option<u64>,
    sliding_ttl: Option<bool>,
    /// Toggle outbound-callback opt-in for the group (ADR-0034). Omitted
    /// from the wire leaves it unchanged.
    callout_enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct GroupResponse {
    id: String,
    name: String,
    implicit: bool,
    owner_id: String,
    ttl_seconds: u64,
    sliding_ttl: bool,
    callout_enabled: bool,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_activity_at: Option<String>,
}

impl From<&Group> for GroupResponse {
    fn from(g: &Group) -> Self {
        Self {
            id: g.id.clone(),
            name: g.name.clone(),
            implicit: g.implicit,
            owner_id: g.owner_id.clone(),
            ttl_seconds: g.ttl_seconds,
            sliding_ttl: g.sliding_ttl,
            callout_enabled: g.callout_enabled,
            created_at: g.created_at.to_rfc3339(),
            last_activity_at: g.last_activity_at.map(|ts| ts.to_rfc3339()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ListGroupsResponse {
    groups: Vec<GroupResponse>,
    total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<u64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub(crate) struct GroupsListQuery {
    /// Restrict to groups owned by `owner_id`. Admin-only; non-admin
    /// callers always see only their own groups.
    pub(crate) owner_id: Option<String>,
    /// Prefix match on group name (exact-case).
    pub(crate) name_prefix: Option<String>,
    /// Free-text needle. Substring-matched (case-insensitive) against
    /// the group's name.
    pub(crate) q: Option<String>,
    /// Lower bound on `last_activity_at`.
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
    /// `true` → only implicit groups; `false` → only explicit. Omit
    /// for both.
    pub(crate) implicit: Option<bool>,
    /// Sort column: `created_at` (default), `name`, `last_activity_at`.
    pub(crate) sort: Option<String>,
    pub(crate) dir: Option<String>,
    pub(crate) offset: Option<u64>,
    pub(crate) limit: Option<u64>,
}

/// Fetch a group by reference (name or ULID), and require the caller
/// to be admin or the group's owner. Used by every per-group
/// lifecycle endpoint.
fn ensure_group_owner_or_admin(
    state: &AppState,
    auth: &AuthContext,
    group_ref: &str,
) -> Result<Group, ApiError> {
    let group = state
        .routes()
        .registry()
        .read_group_by_ref(group_ref)
        .map_err(|_| ApiError::not_found())?;
    if !auth.is_admin && group.owner_id != auth.user_id {
        return Err(ApiError::forbidden("must be the group's owner or an admin"));
    }
    Ok(group)
}

async fn create_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<CreateGroupBody>,
) -> Result<Response, ApiError> {
    let group = state.routes().registry().create_group(NewGroup {
        // Empty name → registry auto-assigns a friendly DNS-safe one.
        name: body.name.unwrap_or_default(),
        owner_id: auth.user_id.clone(),
        ttl_seconds: body.ttl_seconds,
        sliding_ttl: body.sliding_ttl,
    })?;
    Ok((StatusCode::CREATED, Json(GroupResponse::from(&group))).into_response())
}

async fn list_groups(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<GroupsListQuery>,
) -> Result<Json<ListGroupsResponse>, ApiError> {
    let paged = list_groups_core(&state, &auth, &q)?;
    let groups = paged.groups.iter().map(GroupResponse::from).collect();
    Ok(Json(ListGroupsResponse {
        groups,
        total: paged.total,
        next_offset: paged.next_offset,
    }))
}

/// One paginated page of groups plus the totals callers need for
/// pagination chrome. Used by `list_groups` (REST) and the
/// `/ui/groups` page handler.
pub(crate) struct PagedGroups {
    pub groups: Vec<Group>,
    pub total: u64,
    pub next_offset: Option<u64>,
}

/// Shared filter / sort / paginate path for group listings — see
/// `list_routes_core` for the equivalent route version.
pub(crate) fn list_groups_core(
    state: &AppState,
    auth: &AuthContext,
    q: &GroupsListQuery,
) -> Result<PagedGroups, ApiError> {
    let now = chrono::Utc::now();
    let (offset, limit) = parse_pagination(q.offset, q.limit)?;
    let dir = SortDir::parse(q.dir.as_deref(), SortDir::Desc)?;
    let sort_key = parse_group_sort(q.sort.as_deref())?;
    let since = q
        .since
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;
    let until = q
        .until
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;

    let owner_filter: Option<String> = if auth.is_admin {
        q.owner_id.clone()
    } else {
        if q.owner_id.is_some() {
            return Err(FilterParseError::OwnerNonAdmin.into());
        }
        Some(auth.user_id.clone())
    };

    let groups = match owner_filter.as_deref() {
        Some(owner) => state.routes().registry().list_groups_by_owner(owner)?,
        None => state.routes().registry().list_groups()?,
    };

    let mut filtered: Vec<Group> = groups
        .into_iter()
        .filter(|g| match q.name_prefix.as_deref() {
            Some(p) => g.name.starts_with(p),
            None => true,
        })
        .filter(|g| match q.q.as_deref() {
            Some(needle) => g
                .name
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase()),
            None => true,
        })
        .filter(|g| group_matches_since_until(g, since, until))
        .filter(|g| match q.implicit {
            Some(want) => g.implicit == want,
            None => true,
        })
        .collect();

    sort_groups(&mut filtered, sort_key, dir);

    let total = filtered.len() as u64;
    let start = offset.min(total) as usize;
    let end = (start as u64 + limit).min(total) as usize;
    let next_offset = if (end as u64) < total {
        Some(end as u64)
    } else {
        None
    };
    Ok(PagedGroups {
        groups: filtered[start..end].to_vec(),
        total,
        next_offset,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupSortKey {
    CreatedAt,
    Name,
    LastActivityAt,
}

pub(crate) fn parse_group_sort(raw: Option<&str>) -> Result<GroupSortKey, FilterParseError> {
    match raw {
        None | Some("created_at") => Ok(GroupSortKey::CreatedAt),
        Some("name") => Ok(GroupSortKey::Name),
        Some("last_activity_at") => Ok(GroupSortKey::LastActivityAt),
        Some(other) => Err(FilterParseError::BadSort(other.to_string())),
    }
}

pub(crate) fn sort_groups(groups: &mut [Group], key: GroupSortKey, dir: SortDir) {
    groups.sort_by(|a, b| {
        let ord = match key {
            GroupSortKey::CreatedAt => a.created_at.cmp(&b.created_at),
            GroupSortKey::Name => a.name.cmp(&b.name),
            GroupSortKey::LastActivityAt => match (a.last_activity_at, b.last_activity_at) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => std::cmp::Ordering::Equal,
            },
        };
        match dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    });
}

pub(crate) fn group_matches_since_until(
    g: &Group,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if since.is_none() && until.is_none() {
        return true;
    }
    let Some(ts) = g.last_activity_at else {
        return false;
    };
    if let Some(s) = since
        && ts < s
    {
        return false;
    }
    if let Some(u) = until
        && ts > u
    {
        return false;
    }
    true
}

async fn get_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<Json<GroupResponse>, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    Ok(Json(GroupResponse::from(&group)))
}

async fn patch_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    Json(body): Json<PatchGroupBody>,
) -> Result<Json<GroupResponse>, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    if body.name.is_none()
        && body.ttl_seconds.is_none()
        && body.sliding_ttl.is_none()
        && body.callout_enabled.is_none()
    {
        return Err(ApiError::validation(
            "PATCH body must include at least `name`, `ttl_seconds`, `sliding_ttl`, or `callout_enabled`",
        ));
    }
    // Rename first (it rewrites the by-name index + route slugs and changes
    // the served subdomain), then apply any ttl/sliding change.
    if let Some(new_name) = body.name.as_deref() {
        let renamed = state
            .routes()
            .registry()
            .rename_group(&group.id, new_name)?;
        state
            .routes()
            .refresh_after_group_rename(&group.id, &renamed.name);
    }
    let updated = if body.ttl_seconds.is_some()
        || body.sliding_ttl.is_some()
        || body.callout_enabled.is_some()
    {
        state.routes().registry().patch_group(
            &group.id,
            body.ttl_seconds,
            body.sliding_ttl,
            body.callout_enabled,
        )?
    } else {
        // Rename-only: re-read so the response reflects the new name.
        state.routes().registry().read_group_by_ref(&group.id)?
    };
    Ok(Json(GroupResponse::from(&updated)))
}

async fn delete_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<StatusCode, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    let deleted_routes = state.routes().registry().cascade_delete_group(&group.id)?;
    // Invalidate the local route table for each cascaded route so the
    // in-memory cache doesn't keep serving them. Multi-host
    // deployments still need keyspace notifications to invalidate
    // peers — see storage-model.md "Cache coherence and route
    // readiness" / Implementation status.
    state.routes().refresh_after_group_cascade(&group.id);
    tracing::info!(
        group_id = %group.id,
        group_name = %group.name,
        routes_deleted = deleted_routes,
        "group cascade-deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_group(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<Json<GroupResponse>, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    let refreshed = state.routes().registry().refresh_group(&group.id)?;
    Ok(Json(GroupResponse::from(&refreshed)))
}

async fn get_group_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    Query(q): Query<StateQuery>,
) -> Result<Response, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    let entries = state.routes().registry().list_group_state(&group.id)?;
    Ok(render_state(entries, &q))
}

async fn set_group_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    Json(body): Json<SetStateBody>,
) -> Result<StatusCode, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    let entries = decode_state_entries(body.entries)?;
    state
        .routes()
        .registry()
        .set_group_state(&group.id, entries)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_group_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<StatusCode, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    state.routes().registry().clear_group_state(&group.id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_group_journal(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<StatusCode, ApiError> {
    let group = ensure_group_owner_or_admin(&state, &auth, &group_ref)?;
    state.routes().registry().clear_group_journal(&group.id)?;
    Ok(StatusCode::NO_CONTENT)
}

// -- /api/groups/import + /api/groups/{group}/export (spec) --------------
//
// Server-side import/export of a routes-only group spec (wm_core::spec).
// Shared by REST (here), the MCP import_group/export_group tools, and the UI.
// The CLI also uses these endpoints (resolving source_file → inline source
// client-side first). State bundles (ADR-0031) stay out of scope.

/// Create a group + its routes from a spec, as the calling user. Rolls the
/// whole group back if any route fails, so a bad spec leaves nothing behind.
pub(crate) async fn import_group_core(
    state: &AppState,
    auth: &AuthContext,
    spec: wm_core::spec::GroupSpec,
) -> Result<wm_core::spec::ImportSummary, ApiError> {
    let normalized =
        wm_core::spec::normalize(&spec).map_err(|e| ApiError::validation(format!("{e:#}")))?;
    let group = state.routes().registry().create_group(NewGroup {
        name: normalized.name.clone(),
        owner_id: auth.user_id.clone(),
        ttl_seconds: normalized.ttl_seconds,
        sliding_ttl: normalized.sliding,
    })?;
    // callout_enabled isn't a create-surface field (toggle-via-patch only;
    // ADR-0034 slice 2), so apply it as a follow-up patch when the spec asked
    // for it. Skipped when absent so the common case writes nothing extra.
    if let Some(flag) = normalized.callout {
        state
            .routes()
            .registry()
            .patch_group(&group.id, None, None, Some(flag))?;
    }
    for (idx, r) in normalized.routes.iter().enumerate() {
        let body = CreateRouteBody {
            group: Some(group.name.clone()),
            methods: r.methods.clone(),
            path: r.path.clone(),
            language: r.language.clone(),
            source: Some(r.source.clone()),
        };
        if let Err(e) = create_route_core(state, auth, body).await {
            // Roll the partial group back so the caller can re-import after
            // fixing the spec (mirrors the CLI's old client-side rollback).
            let _ = state.routes().registry().cascade_delete_group(&group.id);
            state.routes().refresh_after_group_cascade(&group.id);
            return Err(ApiError::validation(format!(
                "route #{idx} ({path}) failed: {msg}; group {name:?} rolled back",
                path = r.path,
                msg = e.message(),
                name = group.name,
            )));
        }
    }
    Ok(wm_core::spec::ImportSummary {
        group: group.name,
        routes_created: normalized.routes.len(),
    })
}

/// Assemble a spec from a group + its routes (each route's stored source).
/// Owner-or-admin. Errors on a wasm-only route (no source to put in a spec).
pub(crate) fn export_group_core(
    state: &AppState,
    auth: &AuthContext,
    group_ref: &str,
) -> Result<wm_core::spec::GroupSpec, ApiError> {
    let group = ensure_group_owner_or_admin(state, auth, group_ref)?;
    let mut routes: Vec<Route> = state
        .routes()
        .registry()
        .list_routes()?
        .into_iter()
        .filter(|r| r.group_id == group.id)
        .collect();
    routes.sort_by_key(|r| r.number);

    let mut route_specs = Vec::with_capacity(routes.len());
    for r in &routes {
        let Some(source) = r.source.clone() else {
            return Err(ApiError::validation(format!(
                "route {}/{} was uploaded as pre-compiled {} (no source); spec export needs source",
                group.name, r.number, r.language
            )));
        };
        route_specs.push(wm_core::spec::RouteSpec {
            // Canonical export: always the plural `methods` form (never the
            // singular `method`), so one document doesn't mix both shapes.
            method: None,
            methods: r.methods.clone(),
            path: r.path.clone(),
            language: Some(r.language.clone()),
            source: Some(source),
            source_file: None,
        });
    }

    Ok(wm_core::spec::GroupSpec {
        name: group.name.clone(),
        ttl: Some(wm_core::spec::format_duration(group.ttl_seconds)),
        sliding: Some(group.sliding_ttl),
        // Emit only when on — keeps the common-case spec free of `callout:
        // false` noise; an absent field imports as off (the default).
        callout: group.callout_enabled.then_some(true),
        routes: route_specs,
    })
}

async fn import_group_handler(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(spec): Json<wm_core::spec::GroupSpec>,
) -> Result<Response, ApiError> {
    let summary = import_group_core(&state, &auth, spec).await?;
    Ok((StatusCode::CREATED, Json(summary)).into_response())
}

async fn export_group_handler(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Result<Json<wm_core::spec::GroupSpec>, ApiError> {
    Ok(Json(export_group_core(&state, &auth, &group_ref)?))
}

// -- /api/journal/tail (SSE) -----------------------------------------------

#[derive(Debug, Deserialize)]
struct TailQuery {
    group: Option<String>,
    route: Option<String>,
    method: Option<String>,
    path_pattern: Option<String>,
    status: Option<String>,
}

async fn tail_journal(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<TailQuery>,
) -> Result<
    axum::response::Sse<
        impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    use crate::journal::JournalEvent;
    use crate::journal_filter::{JournalFilter, RouteSlug, StatusFilter};
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let route = q
        .route
        .as_deref()
        .map(RouteSlug::parse)
        .transpose()
        .map_err(|e| ApiError::validation(format!("invalid route filter: {e}")))?;
    let status = q
        .status
        .as_deref()
        .map(StatusFilter::parse)
        .transpose()
        .map_err(|e| ApiError::validation(format!("invalid status filter: {e}")))?;
    let filter = JournalFilter {
        group: q.group.clone(),
        route,
        method: q.method.clone(),
        path_pattern: q.path_pattern.clone(),
        status,
        since: None,
        until: None,
    };

    // Auth gate. When a group is supplied, owner-or-admin on that
    // group; otherwise admin-only (host-wide tail is sensitive, same
    // shape as `/api/unmatched`).
    if let Some(group_ref) = filter.group.as_deref() {
        let group_record = state
            .routes()
            .registry()
            .read_group_by_ref(group_ref)
            .map_err(|_| ApiError::not_found())?;
        if !caller_can_read_journal(&state, &auth, &group_record.id)? {
            return Err(ApiError::forbidden(
                "must be an admin or own a route in this group to tail its journal",
            ));
        }
    } else if !auth.is_admin {
        return Err(ApiError::forbidden(
            "host-wide journal tail is admin-only; supply ?group= to scope it",
        ));
    }

    let rx = state.journal().subscribe();

    // Emit an immediate `:ready` comment so the browser's
    // `EventSource.onopen` fires as soon as it sees the response body —
    // otherwise it waits for either the first journal event or axum's
    // `KeepAlive::default()` (15 seconds). That made the UI's
    // "connecting…" label hang for up to 15 s on every page load when
    // no traffic was flowing.
    let ready = futures::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().comment("ready"))
    });

    let live = BroadcastStream::new(rx).filter_map(move |result| {
        let filter = filter.clone();
        async move {
            match result {
                Ok(event) if filter.matches(&event) => {
                    let sse = match &event {
                        JournalEvent::Handled(r) => Event::default()
                            .event("handled")
                            .json_data(r)
                            .expect("encode handled record"),
                        JournalEvent::Unmatched(u) => Event::default()
                            .event("unmatched")
                            .json_data(u)
                            .expect("encode unmatched record"),
                    };
                    Some(Ok(sse))
                }
                Ok(_) => None, // filtered out
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(Event::default()
                        .event("warning")
                        .data(format!("dropped {n} events (consumer lagged)"))))
                }
            }
        }
    });

    // End the stream when the host's shutdown signal fires so axum's
    // graceful-shutdown can drain — otherwise an idle browser tab on
    // /ui/journal/live pins this response open forever and the
    // process won't exit on Ctrl-C. In test paths where no shutdown
    // receiver is wired, `pending()` keeps the original behaviour.
    let stop = match state.shutdown() {
        Some(rx) => {
            let mut rx = rx.clone();
            futures::future::FutureExt::boxed(async move {
                let _ = rx.changed().await;
            })
        }
        None => futures::future::FutureExt::boxed(std::future::pending::<()>()),
    };
    let stream = ready.chain(live).take_until(stop);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// -- /api/match -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MatchQuery {
    /// Group (name or ULID) to probe within. Required under ADR-0030:
    /// matching is per-subdomain, so a probe must name its tenant.
    group: Option<String>,
    method: String,
    path: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MatchResponse {
    Hit {
        matched: bool, // always true here; serialized for the wire shape
        // Boxed to keep the enum's variants close in size (clippy's
        // large_enum_variant lint).
        route: Box<RouteResponse>,
        path_params: Vec<(String, String)>,
    },
    Miss {
        matched: bool, // always false here
        near_misses: Vec<NearMissResponse>,
    },
}

#[derive(Debug, Serialize)]
struct NearMissResponse {
    route: String, // {group}/{n} slug
    route_id: String,
    route_path: String,
    reason: NearMissReasonResponse,
    details: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum NearMissReasonResponse {
    MethodMismatch,
    PrefixMatch,
}

fn validate_match_query(q: &MatchQuery) -> Result<(), ApiError> {
    if q.method.is_empty()
        || !q
            .method
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '-' || c == '_')
    {
        return Err(ApiError::validation(
            "method must be uppercase ASCII (e.g. POST, GET, ANY)",
        ));
    }
    if !q.path.starts_with('/') {
        return Err(ApiError::validation("path must start with /"));
    }
    if q.group.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::validation(
            "group is required (matching is per-subdomain; ADR-0030)",
        ));
    }
    Ok(())
}

async fn match_probe(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Query(q): Query<MatchQuery>,
) -> Result<Json<MatchResponse>, ApiError> {
    use crate::route_table::{MatchProbe, NearMissReason};

    validate_match_query(&q)?;

    // Resolve + gate on the group: probing reveals routes, so it's
    // owner-or-admin of the group being probed (ADR-0030).
    let group_ref = q.group.as_deref().unwrap_or_default();
    let group = state
        .routes()
        .registry()
        .read_group_by_ref(group_ref)
        .map_err(|_| ApiError::not_found())?;
    if !auth.is_admin && group.owner_id != auth.user_id {
        return Err(ApiError::forbidden("must be the group's owner or an admin"));
    }

    match state
        .routes()
        .probe_in_group(&group.name, &q.method, &q.path)
    {
        MatchProbe::Hit(m) => Ok(Json(MatchResponse::Hit {
            matched: true,
            route: Box::new(RouteResponse::build(
                &m.route,
                &headers,
                state.trust_forwarded_headers(),
            )),
            path_params: m.path_params,
        })),
        MatchProbe::Miss(near) => Ok(Json(MatchResponse::Miss {
            matched: false,
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
                            NearMissReasonResponse::MethodMismatch,
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
                            NearMissReasonResponse::PrefixMatch,
                            serde_json::json!({
                                "segment_index": segment_index,
                                "expected": expected,
                                "got": got,
                            }),
                        ),
                    };
                    NearMissResponse {
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

// -- Capabilities -------------------------------------------------------------

#[derive(serde::Serialize)]
struct CapabilityResponse {
    topic: String,
    content: String,
    available_topics: Vec<String>,
}

/// `GET /api/capabilities` → overview + topic list. Returns the
/// same shape as `/api/capabilities/{topic}` with topic="overview".
async fn list_capabilities(_: AuthContext) -> Json<CapabilityResponse> {
    let (topic, content) = crate::capabilities::lookup(None);
    Json(CapabilityResponse {
        topic: topic.to_string(),
        content: content.to_string(),
        available_topics: crate::capabilities::topic_names()
            .into_iter()
            .map(String::from)
            .collect(),
    })
}

/// `GET /api/capabilities/{topic}` → the named topic. Unknown
/// topics fall back to the overview rather than 404 — matches the
/// MCP tool's behaviour (typos shouldn't punish an exploring agent).
async fn get_capability(
    _: AuthContext,
    axum::extract::Path(topic): axum::extract::Path<String>,
) -> Json<CapabilityResponse> {
    let (topic, content) = crate::capabilities::lookup(Some(&topic));
    Json(CapabilityResponse {
        topic: topic.to_string(),
        content: content.to_string(),
        available_topics: crate::capabilities::topic_names()
            .into_iter()
            .map(String::from)
            .collect(),
    })
}

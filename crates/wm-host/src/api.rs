//! REST API at `/__api/*`.
//!
//! Routes (`/__api/routes`): POST/GET/DELETE per `rest-api.md`. Body has
//! two shapes: pre-compiled (`language: "wasm"`, `compiled_wasm` base64)
//! and source-based (`language: "typescript"|...`, `source`). Source-based
//! requests forward to the compiler sidecar via `CompilerClient`; if no
//! sidecar is configured, those requests fail with `compile_failed`.
//!
//! Tokens (`/__api/tokens`): POST/GET/DELETE for the caller's own
//! tokens, per ADR-0012. Plaintext is returned exactly once, in the
//! create response.
//!
//! Every handler under `/__api/*` requires a valid bearer token; the
//! `AuthContext` extractor (impl below) returns 401 on missing /
//! invalid / expired. Mock-traffic dispatch (the fallback in
//! `server::router`) is intentionally open — SUTs don't have tokens.

use axum::Json;
use axum::Router;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::api_filters::{FilterParseError, SortDir, glob_match, parse_since, validate_method};
use crate::auth::{AuthContext, AuthError, Token, User};
use crate::journal::{JournalError, JournalRecord, ListCursor, UnmatchedCursor, UnmatchedRecord};
use crate::journal_filter::{JournalFilter, RouteSlug, StatusFilter};
use crate::registry::{
    Group, NewGroup, NewRoute, PatchRoute, RegistryError, Route, RouteStateEntry, render_slug,
};
use crate::server::is_reserved_path;
use crate::{AppState, SUPPORTED_BINDINGS_VERSION};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/__api/routes", post(create_route).get(list_routes))
        .route(
            "/__api/routes/{group}/{number}",
            get(get_route).patch(patch_route).delete(delete_route),
        )
        .route(
            "/__api/routes/{group}/{number}/state",
            get(get_route_state).delete(delete_route_state),
        )
        .route(
            "/__api/routes/{group}/{number}/dry-run",
            post(dry_run_route),
        )
        .route("/__api/tokens", post(create_token).get(list_tokens))
        .route("/__api/tokens/{name}", get(get_token).delete(delete_token))
        // Users — POST/GET (admin); GET /me (any authed); GET/PATCH/DELETE
        // /{name} (admin or self for the GET; admin-only for PATCH/DELETE).
        // /me must come before /{name} or axum's matcher will treat "me"
        // as a user name.
        .route("/__api/users", post(create_user).get(list_users))
        .route("/__api/users/me", get(get_me))
        .route(
            "/__api/users/{name}",
            get(get_user).patch(patch_user).delete(delete_user),
        )
        // Journal — list/get per group (admin or any group-route owner).
        .route("/__api/journal/{group}", get(list_journal))
        .route("/__api/journal/{group}/{number}", get(get_journal_entry))
        // Journal tail — SSE stream of live events. Auth: owner-or-admin
        // when ?group= is set, admin-only otherwise (matches the
        // host-wide unmatched read).
        .route("/__api/journal/tail", get(tail_journal))
        // Match probe — read-only diagnostic. Any authenticated user.
        .route("/__api/match", get(match_probe))
        // Unmatched — admin-only (host-wide and may include probing traffic).
        .route("/__api/unmatched", get(list_unmatched))
        .route("/__api/unmatched/{number}", get(get_unmatched_entry))
        // Groups — owner-or-admin for cross-cutting actions; the
        // sub-endpoints (refresh, state, journal) live under the
        // group's slug. Note: `DELETE /__api/groups/{group}/journal`
        // here supersedes the `clear-journal` shape from the rest-api
        // doc — same semantics, lives under the group's path.
        .route("/__api/groups", post(create_group).get(list_groups))
        .route(
            "/__api/groups/{group}",
            get(get_group).patch(patch_group).delete(delete_group),
        )
        .route("/__api/groups/{group}/refresh", post(refresh_group))
        .route(
            "/__api/groups/{group}/state",
            axum::routing::delete(delete_group_state),
        )
        .route(
            "/__api/groups/{group}/journal",
            axum::routing::delete(delete_group_journal),
        )
}

// -- Request / response shapes ------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRouteBody {
    pub(crate) group: Option<String>,
    pub(crate) methods: Vec<String>,
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) bindings_version: Option<String>,
    /// Base64-encoded `.component.wasm` bytes. Required when
    /// `language == "wasm"`.
    pub(crate) compiled_wasm: Option<String>,
    /// Source code for the source-based path. Forwarded to the compiler
    /// sidecar; returns `compile_failed` if no sidecar is configured.
    pub(crate) source: Option<String>,
}

/// Partial-update payload for `PATCH /__api/routes/{group}/{n}`. Every
/// field is optional; the handler validates that the (language,
/// source-or-compiled_wasm, bindings_version) triple is consistent
/// when the artifact is being replaced.
#[derive(Debug, Deserialize)]
struct PatchRouteBody {
    methods: Option<Vec<String>>,
    path: Option<String>,
    language: Option<String>,
    bindings_version: Option<String>,
    /// Base64-encoded `.component.wasm` bytes. Pairs with
    /// `language: "wasm"`.
    compiled_wasm: Option<String>,
    /// Source code; forwarded to the compiler sidecar. Pairs with a
    /// source language (e.g. `typescript`).
    source: Option<String>,
}

#[derive(Debug, Serialize)]
struct RouteResponse {
    id: String,
    number: u32,
    group: GroupRef,
    methods: Vec<String>,
    path: String,
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

impl From<&Route> for RouteResponse {
    fn from(r: &Route) -> Self {
        Self {
            id: r.id.clone(),
            number: r.number,
            group: GroupRef {
                id: r.group_id.clone(),
                name: r.group_name.clone(),
            },
            methods: r.methods.clone(),
            path: r.path.clone(),
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

    fn compile_failed_with_diagnostics(msg: impl Into<String>, diagnostics: Vec<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "compile_failed",
            message: msg.into(),
            diagnostics,
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
            RegistryError::Storage(e) => ApiError::internal(format!("storage: {e}")),
            RegistryError::Malformed(msg) => ApiError::internal(format!("malformed record: {msg}")),
        }
    }
}

// -- Handlers -----------------------------------------------------------------

async fn create_route(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<CreateRouteBody>,
) -> Result<Response, ApiError> {
    let route = create_route_core(&state, &auth, body).await?;
    let location = format!(
        "/__api/routes/{}",
        render_slug(&route.group_name, route.number)
    );
    let mut resp = (StatusCode::CREATED, Json(RouteResponse::from(&route))).into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        HeaderValue::try_from(location).expect("ascii location"),
    );
    Ok(resp)
}

/// Shared validate + compile + register pipeline behind both
/// `POST /__api/routes` and the UI's `POST /__ui/routes/new`. Lives
/// in `api.rs` so the validation rules stay in one place.
pub(crate) async fn create_route_core(
    state: &AppState,
    auth: &AuthContext,
    body: CreateRouteBody,
) -> Result<crate::registry::Route, ApiError> {
    if is_reserved_path(&body.path) {
        return Err(ApiError::validation(format!(
            "path {:?} starts with a reserved prefix and cannot be claimed",
            body.path
        )));
    }

    if body.source.is_some() && body.compiled_wasm.is_some() {
        return Err(ApiError::validation(
            "send either `source` or `compiled_wasm`, not both",
        ));
    }

    let (compiled_wasm, language, bindings_version) = match body.language.as_str() {
        "wasm" => {
            let encoded = body
                .compiled_wasm
                .ok_or_else(|| ApiError::validation("compiled_wasm required when language=wasm"))?;
            let bytes = B64
                .decode(encoded.as_bytes())
                .map_err(|e| ApiError::validation(format!("compiled_wasm base64 decode: {e}")))?;
            let bv = body.bindings_version.ok_or_else(|| {
                ApiError::validation("bindings_version required when language=wasm")
            })?;
            if bv != SUPPORTED_BINDINGS_VERSION {
                return Err(ApiError::validation(format!(
                    "bindings_version {bv:?} is not supported (expected {SUPPORTED_BINDINGS_VERSION:?})"
                )));
            }
            // Validate the bytes parse as a wasm Component up front, so a
            // malformed upload fails at create time instead of at first
            // request.
            wasmtime::component::Component::from_binary(state.runtime().engine(), &bytes)
                .map_err(|e| ApiError::compile_failed(format!("component validation: {e}")))?;
            (bytes, "wasm".to_string(), bv)
        }
        other => {
            // Source-based path: hand off to the compiler sidecar.
            let source = body.source.ok_or_else(|| {
                ApiError::validation(format!("source required when language={other:?}"))
            })?;
            let compiler = state.compiler().ok_or_else(|| {
                ApiError::compile_failed(
                    "compiler sidecar not configured; set WM_COMPILER_URL or send a pre-compiled component",
                )
            })?;
            let artifact = compiler
                .compile(other, &source)
                .await
                .map_err(|e| match e {
                    crate::compiler::CompilerError::CompileFailed {
                        message,
                        diagnostics,
                    } => ApiError::compile_failed_with_diagnostics(message, diagnostics),
                    other_err => ApiError::compile_failed(format!("{other_err}")),
                })?;
            // Validate the bytes parse here too — defends against a
            // misbehaving sidecar shipping garbage.
            wasmtime::component::Component::from_binary(
                state.runtime().engine(),
                &artifact.component,
            )
            .map_err(|e| ApiError::compile_failed(format!("component validation: {e}")))?;
            (
                artifact.component,
                other.to_string(),
                artifact.bindings_version,
            )
        }
    };

    let route = state.routes().registry().create_route(NewRoute {
        group: body.group,
        methods: body.methods,
        path: body.path,
        language,
        bindings_version,
        compiled_wasm,
        owner_id: auth.user_id.clone(),
    })?;

    state.routes().refresh_after_create(route.clone());
    Ok(route)
}

async fn list_routes(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<RoutesListQuery>,
) -> Result<Json<ListRoutesResponse>, ApiError> {
    let paged = list_routes_core(&state, &auth, &q)?;
    let routes = paged.routes.iter().map(RouteResponse::from).collect();
    Ok(Json(ListRoutesResponse {
        routes,
        total: paged.total,
        next_offset: paged.next_offset,
    }))
}

/// One paginated page of routes plus the totals the caller needs to
/// render "K of N" + a next-page link. Used by both the REST handler
/// above and the `/__ui/routes` page handler.
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
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<RouteResponse>, ApiError> {
    let route = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    Ok(Json(RouteResponse::from(&route)))
}

async fn patch_route(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
    Json(body): Json<PatchRouteBody>,
) -> Result<Json<RouteResponse>, ApiError> {
    // Existence + ownership gate. Mirrors the DELETE handler — owner-
    // or-admin per ADR-0014.
    let existing = state
        .routes()
        .registry()
        .get_route_by_slug(&group, number)?;
    if existing.owner_id != auth.user_id && !auth.is_admin {
        return Err(ApiError::forbidden(
            "only the route's owner or an admin may update it",
        ));
    }

    let any_field = body.methods.is_some()
        || body.path.is_some()
        || body.language.is_some()
        || body.bindings_version.is_some()
        || body.compiled_wasm.is_some()
        || body.source.is_some();
    if !any_field {
        return Err(ApiError::validation(
            "PATCH body must include at least one mutable field \
             (`methods`, `path`, `source`, `compiled_wasm`)",
        ));
    }
    if body.source.is_some() && body.compiled_wasm.is_some() {
        return Err(ApiError::validation(
            "send either `source` or `compiled_wasm`, not both",
        ));
    }
    if let Some(ref new_path) = body.path
        && is_reserved_path(new_path)
    {
        return Err(ApiError::validation(format!(
            "path {new_path:?} starts with a reserved prefix and cannot be claimed",
        )));
    }

    // Compute the artifact triple (language, bindings_version,
    // compiled_wasm) when the body changes the wasm bytes. When neither
    // `source` nor `compiled_wasm` is present, the existing artifact is
    // preserved; `language` and `bindings_version` are ignored in that
    // case to keep the metadata in sync with what's actually loaded.
    let artifact_changing = body.compiled_wasm.is_some() || body.source.is_some();
    let (compiled_wasm, language, bindings_version) = if artifact_changing {
        let lang = body.language.as_deref().ok_or_else(|| {
            ApiError::validation(
                "`language` is required when changing the route's source/wasm artifact",
            )
        })?;
        match lang {
            "wasm" => {
                let encoded = body.compiled_wasm.as_deref().ok_or_else(|| {
                    ApiError::validation("compiled_wasm required when language=wasm")
                })?;
                let bytes = B64.decode(encoded.as_bytes()).map_err(|e| {
                    ApiError::validation(format!("compiled_wasm base64 decode: {e}"))
                })?;
                let bv = body.bindings_version.clone().ok_or_else(|| {
                    ApiError::validation("bindings_version required when language=wasm")
                })?;
                if bv != SUPPORTED_BINDINGS_VERSION {
                    return Err(ApiError::validation(format!(
                        "bindings_version {bv:?} is not supported (expected {SUPPORTED_BINDINGS_VERSION:?})"
                    )));
                }
                wasmtime::component::Component::from_binary(state.runtime().engine(), &bytes)
                    .map_err(|e| ApiError::compile_failed(format!("component validation: {e}")))?;
                (Some(bytes), Some("wasm".to_string()), Some(bv))
            }
            other => {
                let source = body.source.as_deref().ok_or_else(|| {
                    ApiError::validation(format!("source required when language={other:?}"))
                })?;
                let compiler = state.compiler().ok_or_else(|| {
                    ApiError::compile_failed(
                        "compiler sidecar not configured; set WM_COMPILER_URL or send a pre-compiled component",
                    )
                })?;
                let artifact = compiler.compile(other, source).await.map_err(|e| match e {
                    crate::compiler::CompilerError::CompileFailed {
                        message,
                        diagnostics,
                    } => ApiError::compile_failed_with_diagnostics(message, diagnostics),
                    other_err => ApiError::compile_failed(format!("{other_err}")),
                })?;
                wasmtime::component::Component::from_binary(
                    state.runtime().engine(),
                    &artifact.component,
                )
                .map_err(|e| ApiError::compile_failed(format!("component validation: {e}")))?;
                (
                    Some(artifact.component),
                    Some(other.to_string()),
                    Some(artifact.bindings_version),
                )
            }
        }
    } else {
        (None, None, None)
    };

    let updated = state.routes().registry().update_route(
        &group,
        number,
        PatchRoute {
            methods: body.methods,
            path: body.path,
            language,
            bindings_version,
            compiled_wasm,
        },
    )?;
    state.routes().refresh_after_update(updated.clone());
    Ok(Json(RouteResponse::from(&updated)))
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

async fn get_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Result<Json<ListRouteStateResponse>, ApiError> {
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
    Ok(Json(ListRouteStateResponse { entries }))
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

// -- /__api/tokens ------------------------------------------------------------
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

// -- /__api/users -------------------------------------------------------------
//
// Admin-only for cross-user actions. `GET /__api/users/me` and
// `GET /__api/users/{name}` (when `name` is the caller's own name) are
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

// -- /__api/journal -----------------------------------------------------------
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

async fn list_unmatched(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UnmatchedListQuery>,
) -> Result<Json<ListUnmatchedResponse>, ApiError> {
    require_admin(&auth)?;
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
    let entries = state.journal().list_unmatched(cursor)?;
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
    require_admin(&auth)?;
    let entry = state.journal().get_unmatched(number)?;
    Ok(Json(entry))
}

// -- /__api/groups ------------------------------------------------------------
//
// Lifecycle endpoints. POST is open to any authed user (they own
// what they create); list filters to owned-by-self for non-admin
// callers. PATCH / DELETE / refresh / state-clear / journal-clear all
// require admin or owner. DELETE cascades silently — group owners are
// expected to manage the lifecycle of everything inside.

#[derive(Debug, Deserialize)]
struct CreateGroupBody {
    name: String,
    /// Optional configured TTL. Omit to take the default; values are
    /// validated against `MAX_GROUP_TTL_SECONDS`.
    ttl_seconds: Option<u64>,
    /// Optional sliding-TTL flag. Omit to take the default (`true`).
    sliding_ttl: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PatchGroupBody {
    ttl_seconds: Option<u64>,
    sliding_ttl: Option<bool>,
}

#[derive(Debug, Serialize)]
struct GroupResponse {
    id: String,
    name: String,
    implicit: bool,
    owner_id: String,
    ttl_seconds: u64,
    sliding_ttl: bool,
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
        name: body.name,
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
/// `/__ui/groups` page handler.
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
    if body.ttl_seconds.is_none() && body.sliding_ttl.is_none() {
        return Err(ApiError::validation(
            "PATCH body must include at least `ttl_seconds` or `sliding_ttl` — \
             rename and owner-transfer aren't supported in this slice",
        ));
    }
    let updated =
        state
            .routes()
            .registry()
            .patch_group(&group.id, body.ttl_seconds, body.sliding_ttl)?;
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

// -- /__api/journal/tail (SSE) -----------------------------------------------

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
    // shape as `/__api/unmatched`).
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
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
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
    // /__ui/journal/live pins this response open forever and the
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
    let stream = stream.take_until(stop);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// -- /__api/match -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MatchQuery {
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
    Ok(())
}

async fn match_probe(
    State(state): State<AppState>,
    _auth: AuthContext, // any authenticated user; the route record itself is the only thing returned
    Query(q): Query<MatchQuery>,
) -> Result<Json<MatchResponse>, ApiError> {
    use crate::route_table::{MatchProbe, NearMissReason};

    validate_match_query(&q)?;

    match state.routes().probe(&q.method, &q.path) {
        MatchProbe::Hit(m) => Ok(Json(MatchResponse::Hit {
            matched: true,
            route: Box::new(RouteResponse::from(&m.route)),
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
